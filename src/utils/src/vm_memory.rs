// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.
use std::fs::File;
use std::io::Write;
use std::io::Error as IoError;
use std::os::unix::io::AsRawFd;
use libc::MAP_POPULATE;
// use logger::{error, info, warn};
use nix::sys::mman::{madvise,  MmapAdvise, ProtFlags};
use vm_memory::bitmap::AtomicBitmap;
pub use vm_memory::bitmap::Bitmap;
use vm_memory::mmap::{check_file_offset, NewBitmap};
pub use vm_memory::mmap::{MmapRegionBuilder, MmapRegionError};
pub use vm_memory::mmap;
pub use vm_memory::{
    address, Address, ByteValued, Bytes, Error, FileOffset, GuestAddress, GuestMemory,
    GuestMemoryError, GuestMemoryRegion, GuestUsize, MemoryRegionAddress, MmapRegion,
    VolatileMemory, VolatileMemoryError,
};
// pub use vm_memory::mmap::print_guest_memory;
pub type GuestMemoryMmap = vm_memory::GuestMemoryMmap<Option<AtomicBitmap>>;
pub type GuestRegionMmap = vm_memory::GuestRegionMmap<Option<AtomicBitmap>>;
pub type GuestMmapRegion = vm_memory::MmapRegion<Option<AtomicBitmap>>;

const GUARD_PAGE_COUNT: usize = 1;

/// Build a `MmapRegion` surrounded by guard pages.
///
/// Initially, we map a `PROT_NONE` guard region of size:
/// `size` + (GUARD_PAGE_COUNT * 2 * page_size).
/// The guard region is mapped with `PROT_NONE`, so that any access to this region will cause
/// a SIGSEGV.
///
/// The actual accessible region is going to be nested in the larger guard region.
/// This is done by mapping over the guard region, starting at an address of
/// `guard_region_addr + (GUARD_PAGE_COUNT * page_size)`.
/// This results in a border of `GUARD_PAGE_COUNT` pages on either side of the region, which
/// acts as a safety net for accessing out-of-bounds addresses that are not allocated for the
/// guest's memory.
fn build_guarded_region(
    maybe_file_offset: Option<FileOffset>,
    size: usize,
    prot: i32,
    flags: i32,
    track_dirty_pages: bool,
) -> Result<GuestMmapRegion, MmapRegionError> {
    let page_size = crate::get_page_size().expect("Cannot retrieve page size.");
    // Create the guarded range size (received size + X pages),
    // where X is defined as a constant GUARD_PAGE_COUNT.
    let guarded_size = size + GUARD_PAGE_COUNT * 2 * page_size;

    // Map the guarded range to PROT_NONE
    // SAFETY: Safe because the parameters are valid.
    let guard_addr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            guarded_size,
            libc::PROT_NONE,
            libc::MAP_ANONYMOUS | libc::MAP_PRIVATE | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };

    if guard_addr == libc::MAP_FAILED {
        return Err(MmapRegionError::Mmap(IoError::last_os_error()));
    }

    let (fd, offset) = match maybe_file_offset {
        Some(ref file_offset) => {
            check_file_offset(file_offset, size)?;
            (file_offset.file().as_raw_fd(), file_offset.start())
        }
        None => (-1, 0),
    };

    let region_start_addr = guard_addr as usize + page_size * GUARD_PAGE_COUNT;

    // Inside the protected range, starting with guard_addr + PAGE_SIZE,
    // map the requested range with received protection and flags
    // SAFETY: Safe because the parameters are valid.
    let region_addr = unsafe {
        libc::mmap(
            region_start_addr as *mut libc::c_void,
            size,
            prot,
            flags | libc::MAP_FIXED,
            fd,
            offset as libc::off_t,
        )
    };

    if region_addr == libc::MAP_FAILED {
        return Err(MmapRegionError::Mmap(IoError::last_os_error()));
    }
    // unsafe {
    //     madvise(region_addr, size, MmapAdvise::MADV_SEQUENTIAL)
    //     .map_err(|e| {
    //         MmapRegionError::MappingOverlap
    //     })?;
    // }
    let bitmap = match track_dirty_pages {
        true => Some(AtomicBitmap::with_len(size)),
        false => None,
    };

    // SAFETY: Safe because the parameters are valid.
    unsafe {
        MmapRegionBuilder::new_with_bitmap(size, bitmap)
            .with_raw_mmap_pointer(region_addr.cast::<u8>())
            .with_mmap_prot(prot)
            .with_mmap_flags(flags)
            .build()
    }
}

/// Helper for creating the guest memory.
pub fn create_guest_memory(
    regions: &[(Option<FileOffset>, GuestAddress, usize)],
    track_dirty_pages: bool,
) -> std::result::Result<GuestMemoryMmap, Error> {
    let prot = libc::PROT_READ | libc::PROT_WRITE;
    let mut mmap_regions = Vec::with_capacity(regions.len());

    for region in regions {
        let flags = match region.0 {
            None => libc::MAP_NORESERVE | libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            // mmap memory files with pre-population
            Some(_) => libc::MAP_NORESERVE  | libc::MAP_PRIVATE | libc::MAP_POPULATE,
        };

        let mmap_region =
            build_guarded_region(region.0.clone(), region.2, prot, flags, track_dirty_pages)
                .map_err(Error::MmapRegion)?;

        mmap_regions.push(GuestRegionMmap::new(mmap_region, region.1)?);
    }
    // let len = (size + PAGE_SIZE - 1) / PAGE_SIZE;
    // let mut vec = vec![0; len];
    // unsafe {
    //     mincore(mapping as *const _, size, vec.as_mut_ptr());
    // }
    // warn!("PASS_debug create_guest_memory {:?} regions at vm_memory.rs", mmap_regions.len());
    // let mut file = File::create("/mnt/pmem0/foo.txt").unwrap();
    // write!(file, "PASS_debug create_guest_memory {:?} regions at vm_memory.rs", mmap_regions.len()).unwrap();
    GuestMemoryMmap::from_regions(mmap_regions)
}

pub fn mark_dirty_mem(mem: &GuestMemoryMmap, addr: GuestAddress, len: usize) {
    let _ = mem.try_access(len, addr, |_total, count, caddr, region| {
        if let Some(bitmap) = region.bitmap() {
            bitmap.mark_dirty(caddr.0 as usize, count);
        }
        Ok(count)
    });
}

pub mod test_utils {
    use super::*;

    /// Test helper used to initialize the guest memory without adding guard pages.
    /// This is needed because the default `create_guest_memory`
    /// uses MmapRegionBuilder::build_raw() for setting up the memory with guard pages, which would
    /// error if the size is not a multiple of the page size.
    /// There are unit tests which need a custom memory size, not a multiple of the page size.
    pub fn create_guest_memory_unguarded(
        regions: &[(GuestAddress, usize)],
        track_dirty_pages: bool,
    ) -> std::result::Result<GuestMemoryMmap, Error> {
        let prot = libc::PROT_READ | libc::PROT_WRITE;
        let flags = libc::MAP_NORESERVE | libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
        let mut mmap_regions = Vec::with_capacity(regions.len());

        for region in regions {
            mmap_regions.push(GuestRegionMmap::new(
                MmapRegionBuilder::new_with_bitmap(
                    region.1,
                    match track_dirty_pages {
                        true => Some(AtomicBitmap::with_len(region.1)),
                        false => None,
                    },
                )
                .with_mmap_prot(prot)
                .with_mmap_flags(flags)
                .build()
                .map_err(Error::MmapRegion)?,
                region.0,
            )?);
        }
        GuestMemoryMmap::from_regions(mmap_regions)
    }

    /// Test helper used to initialize the guest memory, without the option of file-backed mmap.
    /// It is just a little syntactic sugar that helps deduplicate test code.
    pub fn create_anon_guest_memory(
        regions: &[(GuestAddress, usize)],
        track_dirty_pages: bool,
    ) -> std::result::Result<GuestMemoryMmap, Error> {
        create_guest_memory(
            &regions.iter().map(|r| (None, r.0, r.1)).collect::<Vec<_>>(),
            track_dirty_pages,
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::undocumented_unsafe_blocks)]
    use super::*;
    use crate::get_page_size;
    use crate::tempfile::TempFile;

    enum AddrOp {
        Read,
        Write,
    }

    impl AddrOp {
        fn apply_on_addr(&self, addr: *mut u8) {
            match self {
                AddrOp::Read => {
                    // We have to do something perform a read_volatile, otherwise
                    // the Release version will optimize it out, making the test fail.
                    unsafe { std::ptr::read_volatile(addr) };
                }
                AddrOp::Write => unsafe {
                    std::ptr::write(addr, 0xFF);
                },
            }
        }
    }

    fn fork_and_run(function: &dyn Fn(), expect_sigsegv: bool) {
        let pid = unsafe { libc::fork() };
        match pid {
            0 => {
                function();
            }
            child_pid => {
                let mut child_status: i32 = -1;
                let pid_done = unsafe { libc::waitpid(child_pid, &mut child_status, 0) };
                assert_eq!(pid_done, child_pid);

                if expect_sigsegv {
                    // Asserts that the child process terminated because
                    // it received a signal that was not handled.
                    assert!(libc::WIFSIGNALED(child_status));
                    // Signal code should be a SIGSEGV
                    assert_eq!(libc::WTERMSIG(child_status), libc::SIGSEGV);
                } else {
                    assert!(libc::WIFEXITED(child_status));
                    // Signal code should be a SIGSEGV
                    assert_eq!(libc::WEXITSTATUS(child_status), 0);
                }
            }
        };
    }

    fn validate_guard_region(region: &GuestMmapRegion) {
        let page_size = get_page_size().unwrap();

        // Check that the created range allows us to write inside it
        let addr = region.as_ptr();

        unsafe {
            std::ptr::write(addr, 0xFF);
            assert_eq!(std::ptr::read(addr), 0xFF);
        }

        // Try a read/write operation against the left guard border of the range
        let left_border = (addr as usize - page_size) as *mut u8;
        fork_and_run(&|| AddrOp::Read.apply_on_addr(left_border), true);
        fork_and_run(&|| AddrOp::Write.apply_on_addr(left_border), true);

        // Try a read/write operation against the right guard border of the range
        let right_border = (addr as usize + region.size()) as *mut u8;
        fork_and_run(&|| AddrOp::Read.apply_on_addr(right_border), true);
        fork_and_run(&|| AddrOp::Write.apply_on_addr(right_border), true);
    }

    fn loop_guard_region_to_sigsegv(region: &GuestMmapRegion) {
        let page_size = get_page_size().unwrap();
        let right_page_guard = region.as_ptr() as usize + region.size();

        fork_and_run(
            &|| {
                let mut addr = region.as_ptr() as usize;
                loop {
                    if addr >= right_page_guard {
                        break;
                    }
                    AddrOp::Write.apply_on_addr(addr as *mut u8);

                    addr += page_size;
                }
            },
            false,
        );

        fork_and_run(
            &|| {
                AddrOp::Write.apply_on_addr(right_page_guard as *mut u8);
            },
            true,
        );
    }

    #[test]
    fn test_build_guarded_region() {
        // Create anonymous guarded region.
        {
            let page_size = get_page_size().unwrap();
            let size = page_size * 10;
            let prot = libc::PROT_READ | libc::PROT_WRITE;
            let flags = libc::MAP_ANONYMOUS | libc::MAP_NORESERVE | libc::MAP_PRIVATE;

            let region = build_guarded_region(None, size, prot, flags, false).unwrap();

            // Verify that the region was built correctly
            assert_eq!(region.size(), size);
            assert!(region.file_offset().is_none());
            assert_eq!(region.prot(), prot);
            assert_eq!(region.flags(), flags);

            validate_guard_region(&region);
        }

        // Create guarded region from file.
        {
            let file = TempFile::new().unwrap().into_file();
            let page_size = get_page_size().unwrap();

            let prot = libc::PROT_READ | libc::PROT_WRITE;
            let flags = libc::MAP_NORESERVE | libc::MAP_PRIVATE;
            let offset = 0;
            let size = 10 * page_size;
            assert_eq!(unsafe { libc::ftruncate(file.as_raw_fd(), 4096 * 10) }, 0);

            let region = build_guarded_region(
                Some(FileOffset::new(file, offset)),
                size,
                prot,
                flags,
                false,
            )
            .unwrap();

            // Verify that the region was built correctly
            assert_eq!(region.size(), size);
            // assert_eq!(region.file_offset().unwrap().start(), offset as u64);
            assert_eq!(region.prot(), prot);
            assert_eq!(region.flags(), flags);

            validate_guard_region(&region);
        }
    }

    #[test]
    fn test_create_guest_memory() {
        // Test that all regions are guarded.
        {
            let region_size = 0x10000;
            let regions = vec![
                (None, GuestAddress(0x0), region_size),
                (None, GuestAddress(0x10000), region_size),
                (None, GuestAddress(0x20000), region_size),
                (None, GuestAddress(0x30000), region_size),
            ];

            let guest_memory = create_guest_memory(&regions, false).unwrap();
            guest_memory.iter().for_each(|region| {
                validate_guard_region(region);
                loop_guard_region_to_sigsegv(region);
            });
        }

        // Check dirty page tracking is off.
        {
            let region_size = 0x10000;
            let regions = vec![
                (None, GuestAddress(0x0), region_size),
                (None, GuestAddress(0x10000), region_size),
                (None, GuestAddress(0x20000), region_size),
                (None, GuestAddress(0x30000), region_size),
            ];

            let guest_memory = create_guest_memory(&regions, false).unwrap();
            guest_memory.iter().for_each(|region| {
                assert!(region.bitmap().is_none());
            });
        }

        // Check dirty page tracking is on.
        {
            let region_size = 0x10000;
            let regions = vec![
                (None, GuestAddress(0x0), region_size),
                (None, GuestAddress(0x10000), region_size),
                (None, GuestAddress(0x20000), region_size),
                (None, GuestAddress(0x30000), region_size),
            ];

            let guest_memory = create_guest_memory(&regions, true).unwrap();
            guest_memory.iter().for_each(|region| {
                assert!(region.bitmap().is_some());
            });
        }
    }

    #[test]
    fn test_mark_dirty_mem() {
        let page_size = crate::get_page_size().unwrap();
        let region_size = page_size * 3;

        let regions = vec![
            (None, GuestAddress(0), region_size), // pages 0-2
            (None, GuestAddress(region_size as u64), region_size), // pages 3-5
            (None, GuestAddress(region_size as u64 * 2), region_size), // pages 6-8
        ];
        let guest_memory = create_guest_memory(&regions, true).unwrap();

        let dirty_map = [
            // page 0: not dirty
            (0, page_size, false),
            // pages 1-2: dirty range in one region
            (page_size, page_size * 2, true),
            // page 3: not dirty
            (page_size * 3, page_size, false),
            // pages 4-7: dirty range across 2 regions,
            (page_size * 4, page_size * 4, true),
            // page 8: not dirty
            (page_size * 8, page_size, false),
        ];

        // Mark dirty memory
        for (addr, len, dirty) in &dirty_map {
            if *dirty {
                mark_dirty_mem(&guest_memory, GuestAddress(*addr as u64), *len);
            }
        }

        // Check that the dirty memory was set correctly
        for (addr, len, dirty) in &dirty_map {
            guest_memory
                .try_access(
                    *len,
                    GuestAddress(*addr as u64),
                    |_total, count, caddr, region| {
                        let offset = caddr.0 as usize;
                        let bitmap = region.bitmap().as_ref().unwrap();
                        for i in offset..offset + count {
                            assert_eq!(bitmap.dirty_at(i), *dirty);
                        }
                        Ok(count)
                    },
                )
                .unwrap();
        }
    }
}
