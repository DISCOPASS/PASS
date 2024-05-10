// Copyright 2022 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
use std::ffi::CString;
use std::collections::HashMap;
use std::fs::File;
use serde_json::{json, Value};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::{mem, ptr};
use std::slice;
use serde::Deserialize;
use userfaultfd::Uffd;
use utils::get_page_size;
use utils::sock_ctrl_msg::ScmSocket;
// ------------rust-pmem------------
// extern crate pmem;
// extern crate rand;
// extern crate memmap;
// use std::fs::File;
// use std::io::{self, Write, BufRead, BufReader};
// use std::time::Instant;
// use pmem::persist;
// use pmem::pmap::PersistentMap;
// use rand::{Rng, thread_rng};

// This is the same with the one used in src/vmm.
/// This describes the mapping between Firecracker base virtual address and offset in the
/// buffer or file backend for a guest memory region. It is used to tell an external
/// process/thread where to populate the guest memory data for this range.
///
/// E.g. Guest memory contents for a region of `size` bytes can be found in the backend
/// at `offset` bytes from the beginning, and should be copied/populated into `base_host_address`.
#[derive(Clone, Debug, Deserialize)]
pub struct GuestRegionUffdMapping {
    /// Base host virtual address where the guest memory contents for this region
    /// should be copied/populated.
    pub base_host_virt_addr: u64,
    /// Region size.
    pub size: usize,
    /// Offset in the backend file/buffer where the region contents are.
    pub offset: u64,
}

struct MemRegion {
    mapping: GuestRegionUffdMapping,
    page_states: HashMap<u64, MemPageState>,
}

pub struct UffdPfHandler {
    mem_regions: Vec<MemRegion>,
    backing_buffer: *const u8,
    pub uffd: Uffd,
    // Not currently used but included to demonstrate how a page fault handler can
    // fetch Firecracker's PID in order to make it aware of any crashes/exits.
    _firecracker_pid: u32,
}

#[derive(Clone)]
pub enum MemPageState {
    Uninitialized,
    FromFile,
    Removed,
    Anonymous,
}

impl UffdPfHandler {
    pub fn from_unix_stream(stream: UnixStream, data: *const u8, size: usize) -> Self {
        let mut message_buf = vec![0u8; 1024];
        let (bytes_read, file) = stream
            .recv_with_fd(&mut message_buf[..])
            .expect("Cannot recv_with_fd");
        message_buf.resize(bytes_read, 0);

        let body = String::from_utf8(message_buf).unwrap();
        let file = file.expect("Uffd not passed through UDS!");

        let mappings = serde_json::from_str::<Vec<GuestRegionUffdMapping>>(&body)
            .expect("Cannot deserialize memory mappings.");
        let memsize: usize = mappings.iter().map(|r| r.size).sum();

        // Make sure memory size matches backing data size.
        assert_eq!(memsize, size);

        let uffd = unsafe { Uffd::from_raw_fd(file.into_raw_fd()) };

        let creds: libc::ucred = get_peer_process_credentials(stream);

        let mem_regions = create_mem_regions(&mappings);

        Self {
            mem_regions,
            backing_buffer: data,
            uffd,
            _firecracker_pid: creds.pid as u32,
        }
    }

    pub fn update_mem_state_mappings(&mut self, start: u64, end: u64, state: &MemPageState) {
        for region in self.mem_regions.iter_mut() {
            for (key, value) in region.page_states.iter_mut() {
                if key >= &start && key < &end {
                    *value = state.clone();
                }
            }
        }
    }

    fn populate_from_file(&self, region: &MemRegion) -> (u64, u64) {
        let src = self.backing_buffer as u64 + region.mapping.offset;
        let start_addr = region.mapping.base_host_virt_addr;
        let len = region.mapping.size;
        // Populate whole region from backing mem-file.
        // This offers an example of how memory can be loaded in RAM,
        // however this can be adjusted to accommodate use case needs.
        let ret = unsafe {
            self.uffd
                .copy(src as *const _, start_addr as *mut _, len, true)
                .expect("Uffd copy failed")
        };

        // Make sure the UFFD copied some bytes.
        assert!(ret > 0);

        return (start_addr, start_addr + len as u64);
    }

    fn zero_out(&mut self, addr: u64) -> (u64, u64) {
        let page_size = get_page_size().unwrap();

        let ret = unsafe {
            self.uffd
                .zeropage(addr as *mut _, page_size, true)
                .expect("Uffd zeropage failed")
        };
        // Make sure the UFFD zeroed out some bytes.
        assert!(ret > 0);

        return (addr, addr + page_size as u64);
    }

    pub fn serve_pf(&mut self, addr: *mut u8) {
        let page_size = get_page_size().unwrap();

        // Find the start of the page that the current faulting address belongs to.
        let dst = (addr as usize & !(page_size as usize - 1)) as *mut libc::c_void;
        let fault_page_addr = dst as u64;

        // Get the state of the current faulting page.
        for region in self.mem_regions.iter() {
            match region.page_states.get(&fault_page_addr) {
                // Our simple PF handler has a simple strategy:
                // There exist 4 states in which a memory page can be in:
                // 1. Uninitialized - page was never touched
                // 2. FromFile - the page is populated with content from snapshotted memory file
                // 3. Removed - MADV_DONTNEED was called due to balloon inflation
                // 4. Anonymous - page was zeroed out -> this implies that more than one page fault
                //    event was received. This can be a consequence of guest reclaiming back its
                //    memory from the host (through balloon device)
                Some(MemPageState::Uninitialized) | Some(MemPageState::FromFile) => {
                    let (start, end) = self.populate_from_file(region);
                    self.update_mem_state_mappings(start, end, &MemPageState::FromFile);
                    return;
                }
                Some(MemPageState::Removed) | Some(MemPageState::Anonymous) => {
                    let (start, end) = self.zero_out(fault_page_addr);
                    self.update_mem_state_mappings(start, end, &MemPageState::Anonymous);
                    return;
                }
                None => {
                    ();
                }
            }
        }

        panic!(
            "Could not find addr: {:?} within guest region mappings.",
            addr
        );
    }
}

fn get_peer_process_credentials(stream: UnixStream) -> libc::ucred {
    let mut creds: libc::ucred = libc::ucred {
        pid: 0,
        gid: 0,
        uid: 0,
    };
    let mut creds_size = mem::size_of::<libc::ucred>() as u32;

    let ret = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut creds as *mut _ as *mut _,
            &mut creds_size as *mut libc::socklen_t,
        )
    };
    if ret != 0 {
        panic!("Failed to get peer process credentials");
    }

    creds
}

fn create_mem_regions(mappings: &Vec<GuestRegionUffdMapping>) -> Vec<MemRegion> {
    let page_size = get_page_size().unwrap();
    let mut mem_regions: Vec<MemRegion> = Vec::with_capacity(mappings.len());

    for r in mappings.iter() {
        let mapping = r.clone();
        let mut addr = r.base_host_virt_addr;
        let end_addr = r.base_host_virt_addr + r.size as u64;
        let mut page_states = HashMap::new();

        while addr < end_addr {
            page_states.insert(addr, MemPageState::Uninitialized);
            addr += page_size as u64;
        }
        mem_regions.push(MemRegion {
            mapping,
            page_states,
        });
    }

    mem_regions
}

// static PMEM_FILE_PATH: &str = "/mnt/pmem0/pmem_test";

// fn generate_large_dataset() -> Vec<i32> {
//     let mut rng = thread_rng();
//     (0..1_000_000).map(|_| rng.gen_range(1..=100)).collect()
// }

// fn pmem_write()-> io::Result<()>{
//     let data: Vec<i32> = generate_large_dataset();
//     let pm_map = PersistentMap::open_or_create(PMEM_FILE_PATH, 8_000_000, false, 0o666).unwrap();
//     let offset = 0;
    
//     // Writing and reading data in PM
//     for (i, &item) in data.iter().enumerate() {
//         let offset_i = offset + i * std::mem::size_of::<i32>();
//         unsafe {
//             let mut_ref = pm_map.write(offset_i as isize, item);
//             // println!("Data in PM: {:?}", *mut_ref);
//             let okm=*mut_ref;
//             persist(&*mut_ref);  // Ensuring data persistence
//         }
//     }
//     Ok(())
// }


// fn pmem_read() -> io::Result<()> {
//     let pm_map = PersistentMap::open_or_create(PMEM_FILE_PATH, 8_000_000, false, 0o666).unwrap();
//     let offset = 0;
    
//     for i in 0..1_000_000 {
//         let offset_i = offset + i * std::mem::size_of::<i32>();
//         unsafe {
//             let val_ref = pm_map.read(offset_i as isize);
//             let _value: i32 = *val_ref;
//             // println!("Data in PM via DAX: {:?}", *val_ref);
//         }
//     }

//     Ok(())
// }

pub fn create_pf_handler() -> UffdPfHandler {
    let uffd_sock_path = std::env::args().nth(1).expect("No socket path given");
    let mem4fun = std::env::args().nth(2).expect("No snapshot memory given"); //e.g., "recognition"
    // // check the state difference between the origianal snapshot memory file and the byte-adressable snapshot memory on PMem
    // let func_snap_pos:HashMap<&str, i64> = HashMap::from([
    //     ("image", 0),
    //     ("rnn", 1),
    //     ("ffmpeg", 2),
    //     ("recognition", 3),
    //     ("pagerank", 4),
    //     ("mobilenet", 5),
    //     ("compression", 6),
    //     ("sentiment", 7),
    //     ("json", 8),
    //     ("pyaes", 9),
    //     ("chameleon", 10),
    //     ("matmul", 11)
    //   ]);
    
    //   let funcs =["image", "rnn", "ffmpeg", "recognition",
    //             "pagerank", "mobilenet", "compression", "sentiment", 
    //             "json", "pyaes", "chameleon", "matmul"];
    
    //   let mut kv_tuples: Vec<(&str, usize)> = Vec::new();
    //   for (i, func) in funcs.iter().enumerate() {
    //     // println!("hash[{}]={}, i: {}", func,func_snap_pos[func], i);
    //     assert!(func_snap_pos[func] == i as i64);
    //   }
      
    // let check_str = mem_file_path.as_str();
    // let parts = check_str.split("/").collect::<Vec<_>>();
    // let func = parts[parts.len()-2];
    // let value = func_snap_pos.get(&func).cloned().unwrap_or(-1);
    // if value >= 0 {
    //     println!("func {} at {}", func, value<<30);
    // }
    // else{
    //     println!("func {} not found", func);
    //     std::process::exit(0); 
    // }

    let file = File::open("/dev/shm/snapshot_index.json").unwrap();
    let func_snap_pos: Value = serde_json::from_reader(file).unwrap();
    // let search_key="matmul";
    // if !func_snap_pos.contains_key(search_key) {
    //     println!("Can not locate & refer to the snapshot memory of {}.", search_key);
    // } 

    // Check the hash table for snapshot memory index
    for (key, value) in func_snap_pos.as_object().unwrap().iter() {
        let value = value.as_i64().unwrap();
        println!("{}: {}", key, value);
        // assert_eq!(*func_snap_pos.get(key.as_str()).unwrap(), value as usize);
    }

    // first address of VM snapshot memory for "recognition" workload on PMem
    // println!("test {}",mem4fun);
    // let add_addr=func_snap_pos["recognition"].as_i64().unwrap()<<30;
    let add_addr=func_snap_pos[mem4fun].as_i64().unwrap()<<30;
    // let file = File::open(mem_file_path).expect("Cannot open memfile");
    // let size = file.metadata().unwrap().len() as usize;
    // // mmap a memory area used to bring in the faulting regions.
    // let ret = unsafe {
    //     libc::mmap(
    //         ptr::null_mut(),
    //         size,
    //         libc::PROT_READ,
    //         libc::MAP_PRIVATE,
    //         file.as_raw_fd(),
    //         0,
    //     )
    // };
    // if ret == libc::MAP_FAILED {
    //     panic!("mmap failed");
    // }
    // let memfile_buffer = ret as *const u8;
    let path_cstring = CString::new("/dev/dax1.0").unwrap();
    let dax_fd = unsafe { libc::open(path_cstring.as_ptr(), libc::O_RDWR) };

    if dax_fd < 0 {
        panic!("memory file open failed");
    }
    
    let mut size = 1_073_741_824;
    let dax_addr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            1_073_741_824,
            libc::PROT_READ|libc::PROT_WRITE,
            libc::MAP_SHARED, //libc::MAP_SHARED|libc::MAP_POPULATE,
            dax_fd,
            add_addr //idx * 1024 * 1024 * 1024,
        )
    };
    if dax_addr == libc::MAP_FAILED {
        panic!("mmap failed");
    }
    println!("dax_addr: {:p}", dax_addr);
    // ==== page frames to pages =====
    // let add_addr=value<<30;
    // let memfile_buffer = unsafe {dax_addr.add(4096) } as *const u8;
    // size = 1_073_741_824+(value<<30) as usize;
    /// emulate the page table forked from the PM manager
    let PAGE_SIZE = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    let total_pages = size / PAGE_SIZE;
    let mut cnt=0;
    // deliberately trigger page faults by accessing all virtual pages. 
    // establish the actual MMU page mappings 
    for page_id in 0..total_pages {
        let page_offset = page_id * PAGE_SIZE;
        let page_addr = unsafe { dax_addr.add(page_offset) } as *mut u8;

        let page_data = page_id.to_ne_bytes(); // Convert page ID to byte array
        let read_data = unsafe {
            slice::from_raw_parts(page_addr, page_data.len())
        };
        // println!("page_id: {}, page_addr: {:p}, page_data: {:?}, read_data: {:?}", page_id, page_addr, page_data, read_data);
        // if read_data != page_data {
        //     panic!("Data mismatch at page ID {}", page_id);
        // }
    }

    println!("loaded data");

    let memfile_buffer = dax_addr  as *const u8;
    println!("mem_buffer: {:p}", memfile_buffer);
    
    let len_size = 1_073_741_824;
    // Get Uffd from UDS. We'll use the uffd to handle PFs for Firecracker.
    let listener = UnixListener::bind(&uffd_sock_path).expect("Cannot bind to socket path");

    let (stream, _) = listener.accept().expect("Cannot listen on UDS socket");

    UffdPfHandler::from_unix_stream(stream, memfile_buffer, len_size)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::undocumented_unsafe_blocks)]

    use super::*;
    use std::alloc::Layout;

    use std::fs::File;
    use std::io::Cursor;
    use std::mem;
    use std::mem::size_of_val;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread::spawn;

    // extern crate log;
    extern crate pmem;
    // extern crate libc;
    // use std::os::unix::io::AsRawFd;
    // use std::fs::OpenOptions;
    // use libc::{PROT_READ, PROT_WRITE, MAP_SHARED};
    // use pmem::is_pmem;
    // use pmem::persistentmap::PersistentMap;


    #[test]
    fn observe_pm() {
        use std::ptr::null_mut;
        use std::slice;
        use std::ffi::CString;
        const PMEM_REGION_PATH: &str = "/dev/dax1.0";
        const TEST_STRING: &str = r"Hello Optane!";

        // Open the DAX device
        // let fd = unsafe { libc::open(PMEM_REGION_PATH.as_ptr() as *const i8, libc::O_RDWR) };
        
        let path_cstring = CString::new(PMEM_REGION_PATH).unwrap();
        let fd = unsafe { libc::open(path_cstring.as_ptr(), libc::O_RDWR) };

        if fd < 0 {
            panic!("Failed to open DAX device");
        }

        // Map the DAX device into memory using zero-copy method
        // let addr = pmem_bindings::pmem_map_file(...);
        const DEVICE_SIZE: usize = 532_708_065_280;
        let addr = unsafe {
            libc::mmap(
                null_mut(),
                DEVICE_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED_VALIDATE | libc::MAP_SYNC,
                fd,
                0,
            )
        };

        if addr == libc::MAP_FAILED {
            let e = std::io::Error::last_os_error();
            panic!("Failed to mmap DAX device: {}", e);
        }

        // Write to PMem using byte-addressable feature
        unsafe {
            let pmem_slice = slice::from_raw_parts_mut(addr as *mut u8, TEST_STRING.len());
            let pmem_vec: Vec<u8> = pmem_slice[..TEST_STRING.len()].to_vec();
            pmem::persist(&pmem_vec);
            println!("pmem_vec: {:?}", pmem_vec);
            pmem_slice.copy_from_slice(TEST_STRING.as_bytes());
        }

        // Read from PMem
        let read_string = unsafe {
            let pmem_slice = slice::from_raw_parts(addr as *mut u8, TEST_STRING.len());
            String::from_utf8_lossy(pmem_slice).to_string()
        };

        assert_eq!(read_string, TEST_STRING);
        // println!("Successfully wrote and read from PMem: {}", read_string);

        // Cleanup
        unsafe {
            libc::munmap(addr, DEVICE_SIZE);
            libc::close(fd);
        }
    }
}
