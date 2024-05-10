// Copyright 2021 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

#![deny(missing_docs)]

//! High-level interface over Linux io_uring.
//!
//! Aims to provide an easy-to-use interface, while making some Firecracker-specific simplifying
//! assumptions. The crate does not currently aim at supporting all io_uring features and use
//! cases. For example, it only works with pre-registered fds and read/write/fsync requests.
//!
//! Requires at least kernel version 5.10.51.
//! For more information on io_uring, refer to the man pages.
//! [This pdf](https://kernel.dk/io_uring.pdf) is also very useful, though outdated at times.

#[allow(clippy::undocumented_unsafe_blocks)]
mod bindings;
pub mod operation;
mod probe;
mod queue;
pub mod restriction;

use std::collections::HashSet;
use std::fs::File;
use std::io::Error as IOError;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

use bindings::io_uring_params;
use operation::{Cqe, OpCode, Operation};
use probe::{ProbeWrapper, PROBE_LEN};
use queue::completion::CompletionQueue;
pub use queue::completion::Error as CQueueError;
pub use queue::submission::Error as SQueueError;
use queue::submission::SubmissionQueue;
use restriction::Restriction;
use utils::syscall::SyscallReturnCode;

// IO_uring operations that we require to be supported by the host kernel.
const REQUIRED_OPS: [OpCode; 2] = [OpCode::Read, OpCode::Write];
// Taken from linux/fs/io_uring.c
const IORING_MAX_FIXED_FILES: usize = 1 << 15;

type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
/// IoUring Error.
pub enum Error {
    /// Error originating in the completion queue.
    CQueue(CQueueError),
    /// Could not enable the ring.
    Enable(IOError),
    /// A FamStructWrapper operation has failed.
    Fam(utils::fam::Error),
    /// The number of ops in the ring is >= CQ::count
    FullCQueue,
    /// Fd was not registered.
    InvalidFixedFd(i32),
    /// There are no registered fds.
    NoRegisteredFds,
    /// Error probing the io_uring subsystem.
    Probe(IOError),
    /// Could not register eventfd.
    RegisterEventfd(IOError),
    /// Could not register file.
    RegisterFile(IOError),
    /// Attempted to register too many files.
    RegisterFileLimitExceeded,
    /// Could not register restrictions.
    RegisterRestrictions(IOError),
    /// Error calling io_uring_setup.
    Setup(IOError),
    /// Error originating in the submission queue.
    SQueue(SQueueError),
    /// Required feature is not supported on the host kernel.
    UnsupportedFeature(&'static str),
    /// Required operation is not supported on the host kernel.
    UnsupportedOperation(&'static str),
}

impl Error {
    /// Return true if this error is caused by a full submission or completion queue.
    pub fn is_throttling_err(&self) -> bool {
        matches!(
            self,
            Error::FullCQueue | Error::SQueue(SQueueError::FullQueue)
        )
    }
}

/// Main object representing an io_uring instance.
pub struct IoUring {
    registered_fds_count: u32,
    squeue: SubmissionQueue,
    cqueue: CompletionQueue,
    // Make sure the fd is declared after the queues, so that it isn't dropped before them.
    // If we drop the queues after the File, the associated kernel mem will never be freed.
    // The correct cleanup order is munmap(rings) -> close(fd).
    // We don't need to manually drop the fields in order,since Rust has a well defined drop order.
    fd: File,

    // The total number of ops. These includes the ops on the submission queue, the in-flight ops
    // and the ops that are in the CQ, but haven't been popped yet.
    num_ops: u32,
}

impl IoUring {
    /// Create a new instance.
    ///
    /// # Arguments
    ///
    /// * `num_entries` - Requested number of entries in the ring. Will be rounded up to the
    /// nearest power of two.
    /// * `files` - Files to be registered for IO.
    /// * `restrictions` - Vector of [`Restriction`](restriction/enum.Restriction.html)s
    /// * `eventfd` - Optional eventfd for receiving completion notifications.
    pub fn new(
        num_entries: u32,
        files: Vec<&File>,
        restrictions: Vec<Restriction>,
        eventfd: Option<RawFd>,
    ) -> Result<Self> {
        let mut params = io_uring_params {
            // Create the ring as disabled, so that we may register restrictions.
            flags: bindings::IORING_SETUP_R_DISABLED,

            ..Default::default()
        };

        // SAFETY: Safe because values are valid and we check the return value.
        let fd = SyscallReturnCode(unsafe {
            libc::syscall(
                libc::SYS_io_uring_setup,
                num_entries,
                &mut params as *mut io_uring_params,
            ) as libc::c_int
        })
        .into_result()
        .map_err(Error::Setup)?;

        // SAFETY: Safe because the fd is valid and because this struct owns the fd.
        let file = unsafe { File::from_raw_fd(fd) };

        Self::check_features(params)?;

        let squeue = SubmissionQueue::new(fd, &params).map_err(Error::SQueue)?;
        let cqueue = CompletionQueue::new(fd, &params).map_err(Error::CQueue)?;

        let mut instance = Self {
            squeue,
            cqueue,
            fd: file,
            registered_fds_count: 0,
            num_ops: 0,
        };

        instance.check_operations()?;

        if let Some(eventfd) = eventfd {
            instance.register_eventfd(eventfd)?;
        }

        instance.register_restrictions(restrictions)?;

        instance.register_files(files)?;

        instance.enable()?;

        Ok(instance)
    }

    /// Push an [`Operation`](operation/struct.Operation.html) onto the submission queue.
    ///
    /// # Safety
    /// Unsafe because we pass a raw user_data pointer to the kernel.
    /// It's up to the caller to make sure that this value is ever freed (not leaked).
    pub unsafe fn push<T>(&mut self, op: Operation<T>) -> std::result::Result<(), (Error, T)> {
        // validate that we actually did register fds
        let fd = op.fd() as i32;
        match self.registered_fds_count {
            0 => Err((Error::NoRegisteredFds, op.user_data())),
            len if fd < 0 || (len as i32 - 1) < fd => {
                Err((Error::InvalidFixedFd(fd), op.user_data()))
            }
            _ => {
                if self.num_ops >= self.cqueue.count() {
                    return Err((Error::FullCQueue, op.user_data()));
                }
                self.squeue
                    .push(op.into_sqe())
                    .map(|res| {
                        // This is safe since self.num_ops < IORING_MAX_CQ_ENTRIES (65536)
                        self.num_ops += 1;
                        res
                    })
                    .map_err(|err_tuple: (SQueueError, T)| -> (Error, T) {
                        (Error::SQueue(err_tuple.0), err_tuple.1)
                    })
            }
        }
    }

    /// Pop a completed entry off the completion queue. Returns `Ok(None)` if there are no entries.
    /// The type `T` must be the same as the `user_data` type used for `push`-ing the operation.
    ///
    /// # Safety
    /// Unsafe because we reconstruct the `user_data` from a raw pointer passed by the kernel.
    /// It's up to the caller to make sure that `T` is the correct type of the `user_data`, that
    /// the raw pointer is valid and that we have full ownership of that address.
    pub unsafe fn pop<T>(&mut self) -> Result<Option<Cqe<T>>> {
        self.cqueue
            .pop()
            .map(|maybe_cqe| {
                maybe_cqe.map(|cqe| {
                    // This is safe since the pop-ed CQEs have been previously pushed. However
                    // we use a saturating_sub for extra safety.
                    self.num_ops = self.num_ops.saturating_sub(1);
                    cqe
                })
            })
            .map_err(Error::CQueue)
    }

    fn do_submit(&mut self, min_complete: u32) -> Result<u32> {
        self.squeue.submit(min_complete).map_err(Error::SQueue)
    }

    /// Submit all operations but don't wait for any completions.
    pub fn submit(&mut self) -> Result<u32> {
        self.do_submit(0)
    }

    /// Submit all operations and wait for their completion.
    pub fn submit_and_wait_all(&mut self) -> Result<u32> {
        self.do_submit(self.num_ops)
    }

    /// Return the number of operations currently on the submission queue.
    pub fn pending_sqes(&self) -> Result<u32> {
        self.squeue.pending().map_err(Error::SQueue)
    }

    /// A total of the number of ops in the submission and completion queues, as well as the
    /// in-flight ops.
    pub fn num_ops(&self) -> u32 {
        self.num_ops
    }

    fn enable(&mut self) -> Result<()> {
        // SAFETY: Safe because values are valid and we check the return value.
        SyscallReturnCode(unsafe {
            libc::syscall(
                libc::SYS_io_uring_register,
                self.fd.as_raw_fd(),
                bindings::IORING_REGISTER_ENABLE_RINGS,
                std::ptr::null::<libc::c_void>(),
                0,
            )
        } as libc::c_int)
        .into_empty_result()
        .map_err(Error::Enable)
    }

    fn register_files(&mut self, files: Vec<&File>) -> Result<()> {
        if files.is_empty() {
            // No-op.
            return Ok(());
        }

        if (self.registered_fds_count as usize).saturating_add(files.len()) > IORING_MAX_FIXED_FILES
        {
            return Err(Error::RegisterFileLimitExceeded);
        }

        // SAFETY: Safe because values are valid and we check the return value.
        SyscallReturnCode(unsafe {
            libc::syscall(
                libc::SYS_io_uring_register,
                self.fd.as_raw_fd(),
                bindings::IORING_REGISTER_FILES,
                files
                    .iter()
                    .map(|f| f.as_raw_fd())
                    .collect::<Vec<_>>()
                    .as_mut_slice()
                    .as_mut_ptr() as *const _,
                files.len(),
            ) as libc::c_int
        })
        .into_empty_result()
        .map_err(Error::RegisterFile)?;

        // Safe to truncate since files.len() < IORING_MAX_FIXED_FILES
        self.registered_fds_count += files.len() as u32;
        Ok(())
    }

    fn register_eventfd(&self, fd: RawFd) -> Result<()> {
        // SAFETY: Safe because values are valid and we check the return value.
        SyscallReturnCode(unsafe {
            libc::syscall(
                libc::SYS_io_uring_register,
                self.fd.as_raw_fd(),
                bindings::IORING_REGISTER_EVENTFD,
                (&fd) as *const _,
                1,
            ) as libc::c_int
        })
        .into_empty_result()
        .map_err(Error::RegisterEventfd)
    }

    fn register_restrictions(&self, restrictions: Vec<Restriction>) -> Result<()> {
        if restrictions.is_empty() {
            // No-op.
            return Ok(());
        }
        // SAFETY: Safe because values are valid and we check the return value.
        SyscallReturnCode(unsafe {
            libc::syscall(
                libc::SYS_io_uring_register,
                self.fd.as_raw_fd(),
                bindings::IORING_REGISTER_RESTRICTIONS,
                restrictions
                    .iter()
                    .map(bindings::io_uring_restriction::from)
                    .collect::<Vec<_>>()
                    .as_mut_slice()
                    .as_mut_ptr(),
                restrictions.len(),
            )
        } as libc::c_int)
        .into_empty_result()
        .map_err(Error::RegisterRestrictions)
    }

    fn check_features(params: io_uring_params) -> Result<()> {
        // We require that the host kernel will never drop completed entries due to an (unlikely)
        // overflow in the completion queue.
        // This feature is supported for kernels greater than 5.7.
        // An alternative fix would be to keep an internal counter that tracks the number of
        // submitted entries that haven't been completed and makes sure it doesn't exceed
        // (2 * num_entries).
        if (params.features & bindings::IORING_FEAT_NODROP) == 0 {
            return Err(Error::UnsupportedFeature("IORING_FEAT_NODROP"));
        }

        Ok(())
    }

    fn check_operations(&self) -> Result<()> {
        let mut probes = ProbeWrapper::new(PROBE_LEN).map_err(Error::Fam)?;

        // SAFETY: Safe because values are valid and we check the return value.
        SyscallReturnCode(unsafe {
            libc::syscall(
                libc::SYS_io_uring_register,
                self.fd.as_raw_fd(),
                bindings::IORING_REGISTER_PROBE,
                probes.as_mut_fam_struct_ptr(),
                PROBE_LEN,
            )
        } as libc::c_int)
        .into_empty_result()
        .map_err(Error::Probe)?;

        let supported_opcodes: HashSet<u8> = probes
            .as_slice()
            .iter()
            .filter(|op| ((u32::from(op.flags)) & bindings::IO_URING_OP_SUPPORTED) != 0)
            .map(|op| op.op)
            .collect();

        for opcode in REQUIRED_OPS.iter() {
            if !supported_opcodes.contains(&(*opcode as u8)) {
                return Err(Error::UnsupportedOperation((*opcode).into()));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::undocumented_unsafe_blocks)]
    use std::os::unix::fs::FileExt;

    use proptest::prelude::*;
    use proptest::strategy::Strategy;
    use proptest::test_runner::{Config, TestRunner};
    use utils::kernel_version::{min_kernel_version_for_io_uring, KernelVersion};
    use utils::skip_if_io_uring_unsupported;
    use utils::syscall::SyscallReturnCode;
    use utils::tempfile::TempFile;
    use utils::vm_memory::{Bytes, MmapRegion, VolatileMemory};

    /// -------------------------------------
    /// BEGIN PROPERTY BASED TESTING
    use super::*;

    fn drain_cqueue(ring: &mut IoUring) {
        while let Some(entry) = unsafe { ring.pop::<u32>().unwrap() } {
            assert!(entry.result().is_ok());

            // Assert that there were no partial writes.
            let count = entry.result().unwrap();
            let user_data = entry.user_data();
            assert_eq!(count, user_data);
        }
    }

    fn setup_mem_region(len: usize) -> MmapRegion {
        const PROT: i32 = libc::PROT_READ | libc::PROT_WRITE;
        const FLAGS: i32 = libc::MAP_ANONYMOUS | libc::MAP_PRIVATE;

        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, PROT, FLAGS, -1, 0) };

        if (ptr as isize) < 0 {
            panic!("Mmap failed with {}", std::io::Error::last_os_error());
        }

        unsafe {
            // Use the raw version because we want to unmap memory ourselves.
            MmapRegion::build_raw(ptr.cast::<u8>(), len, PROT, FLAGS).unwrap()
        }
    }

    fn free_mem_region(region: MmapRegion) {
        unsafe { libc::munmap(region.as_ptr().cast::<libc::c_void>(), region.len()) };
    }

    fn read_entire_mem_region(region: &MmapRegion) -> Vec<u8> {
        let mut result = vec![0u8; region.len()];
        let count = region.as_volatile_slice().read(&mut result[..], 0).unwrap();
        assert_eq!(count, region.len());
        result
    }

    fn arbitrary_rw_operation(file_len: u32) -> impl Strategy<Value = Operation<u32>> {
        (
            // OpCode: 0 -> Write, 1 -> Read.
            0..2,
            // Length of the operation.
            0u32..file_len,
        )
            .prop_flat_map(move |(op, len)| {
                (
                    // op
                    Just(op),
                    // len
                    Just(len),
                    // offset
                    (0u32..(file_len - len)),
                    // mem region offset
                    (0u32..(file_len - len)),
                )
            })
            .prop_map(move |(op, len, off, mem_off)| {
                // We actually use an offset instead of an address, because we later need to modify
                // the memory region on which the operation is performed, based on the opcode.
                let mut operation = match op {
                    0 => Operation::write(0, mem_off as usize, len, off.into(), len),
                    _ => Operation::read(0, mem_off as usize, len, off.into(), len),
                };

                // Make sure the operations are executed in-order, so that they are equivalent to
                // their sync counterparts.
                operation.set_linked();
                operation
            })
    }

    #[test]
    fn proptest_read_write_correctness() {
        skip_if_io_uring_unsupported!();
        // Performs a sequence of random read and write operations on two files, with sync and
        // async IO, respectively.
        // Verifies that the files are identical afterwards and that the read operations returned
        // the same values.

        const FILE_LEN: usize = 1024;
        // The number of arbitrary operations in a testrun.
        const OPS_COUNT: usize = 2000;
        const RING_SIZE: u32 = 128;

        // Allocate and init memory for holding the data that will be written into the file.
        let write_mem_region = setup_mem_region(FILE_LEN);

        let sync_read_mem_region = setup_mem_region(FILE_LEN);

        let async_read_mem_region = setup_mem_region(FILE_LEN);

        // Init the write buffers with 0,1,2,...
        for i in 0..FILE_LEN {
            write_mem_region
                .as_volatile_slice()
                .write_obj((i % (u8::MAX as usize)) as u8, i)
                .unwrap();
        }

        // Create two files and init their contents to zeros.
        let init_contents = [0u8; FILE_LEN];
        let file_async = TempFile::new().unwrap().into_file();
        file_async.write_all_at(&init_contents, 0).unwrap();

        let file_sync = TempFile::new().unwrap().into_file();
        file_sync.write_all_at(&init_contents, 0).unwrap();

        // Create a custom test runner since we had to add some state buildup to the test.
        // (Referring to the the above initializations).
        let mut runner = TestRunner::new(Config {
            #[cfg(target_arch = "x86_64")]
            cases: 1000, // Should run for about a minute.
            // Lower the cases on ARM since they take longer and cause coverage test timeouts.
            #[cfg(target_arch = "aarch64")]
            cases: 500,
            ..Config::default()
        });

        runner
            .run(
                &proptest::collection::vec(arbitrary_rw_operation(FILE_LEN as u32), OPS_COUNT),
                |set| {
                    let mut ring =
                        IoUring::new(RING_SIZE, vec![&file_async], vec![], None).unwrap();

                    for mut operation in set {
                        // Perform the sync op.
                        let count = match operation.opcode {
                            OpCode::Write => u32::try_from(
                                SyscallReturnCode(unsafe {
                                    libc::pwrite(
                                        file_sync.as_raw_fd(),
                                        write_mem_region.as_ptr().add(operation.addr.unwrap())
                                            as *const libc::c_void,
                                        operation.len.unwrap() as usize,
                                        operation.offset.unwrap() as i64,
                                    ) as libc::c_int
                                })
                                .into_result()
                                .unwrap(),
                            )
                            .unwrap(),
                            OpCode::Read => u32::try_from(
                                SyscallReturnCode(unsafe {
                                    libc::pread(
                                        file_sync.as_raw_fd(),
                                        sync_read_mem_region
                                            .as_ptr()
                                            .add(operation.addr.unwrap())
                                            .cast::<libc::c_void>(),
                                        operation.len.unwrap() as usize,
                                        operation.offset.unwrap() as i64,
                                    ) as libc::c_int
                                })
                                .into_result()
                                .unwrap(),
                            )
                            .unwrap(),
                            _ => unreachable!(),
                        };

                        if count < operation.len.unwrap() {
                            panic!("Synchronous partial operation: {:?}", operation);
                        }

                        // Perform the async op.

                        // Modify the operation address based on the opcode.
                        match operation.opcode {
                            OpCode::Write => {
                                operation.addr = Some(unsafe {
                                    write_mem_region.as_ptr().add(operation.addr.unwrap()) as usize
                                })
                            }
                            OpCode::Read => {
                                operation.addr = Some(unsafe {
                                    async_read_mem_region.as_ptr().add(operation.addr.unwrap())
                                        as usize
                                })
                            }
                            _ => unreachable!(),
                        };

                        // If the ring is full, submit and wait.
                        if ring.pending_sqes().unwrap() == RING_SIZE {
                            ring.submit_and_wait_all().unwrap();
                            drain_cqueue(&mut ring);
                        }
                        unsafe {
                            ring.push(operation).unwrap();
                        }
                    }

                    // Submit any left async ops and wait.
                    ring.submit_and_wait_all().unwrap();
                    drain_cqueue(&mut ring);

                    // Get the write result for async IO.
                    let mut async_result = [0u8; FILE_LEN];
                    file_async.read_exact_at(&mut async_result, 0).unwrap();

                    // Get the write result for sync IO.
                    let mut sync_result = [0u8; FILE_LEN];
                    file_sync.read_exact_at(&mut sync_result, 0).unwrap();

                    // Now compare the write results.
                    assert_eq!(sync_result, async_result);

                    // Now compare the read results for sync and async IO.
                    assert_eq!(
                        read_entire_mem_region(&sync_read_mem_region),
                        read_entire_mem_region(&async_read_mem_region)
                    );

                    Ok(())
                },
            )
            .unwrap();

        // Clean up the memory.
        free_mem_region(write_mem_region);
        free_mem_region(sync_read_mem_region);
        free_mem_region(async_read_mem_region);
    }
}
