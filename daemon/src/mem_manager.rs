// #[macro_use]
// extern crate lazy_static;
use lazy_static::lazy_static;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
// use std::sync::Mutex;

const DAX_MM_SIZE: u64 = 140 * 1024 * 1024 * 1024; // 140 GB
const ALIGN: u64 = 2 * 1024 * 1024; // 2 MB
const META_BLOCK_SIZE: usize = (2 * 4096 * 4096) as usize;
const MM_META_NR: usize = 1000;

// // Represents a block of memory in the pool
// struct MemoryBlock {
//     address: *mut u8,
//     size: usize,
//     is_allocated: bool,
// }

// // The Memory Pool Manager
// struct MemoryPoolManager {
//     pool: Vec<MemoryBlock>,
//     allocations: HashMap<usize, usize>, // Maps application ID to block index
// }

// impl MemoryPoolManager {
//     // Initialize the memory pool with PMem
//     fn new(pmem_addr: *mut u8, pmem_size: usize, block_size: usize) -> Self {
//         let mut blocks = Vec::with_capacity(pmem_size / block_size);
//         let mut current_addr = pmem_addr;
//         for _ in 0..(pmem_size / block_size) {
//             blocks.push(MemoryBlock {
//                 address: current_addr,
//                 size: block_size,
//                 is_allocated: false,
//             });
//             // Move the current address pointer forward by block_size bytes
//             current_addr = unsafe { current_addr.add(block_size) };
//         }
//         MemoryPoolManager {
//             pool: blocks,
//             allocations: HashMap::new(),
//         }
//     }

//     // Allocate a memory block to an application
//     fn allocate(&mut self, app_id: usize) -> Option<*mut u8> {
//         for (index, block) in self.pool.iter_mut().enumerate() {
//             if !block.is_allocated {
//                 block.is_allocated = true;
//                 self.allocations.insert(app_id, index);
//                 return Some(block.address);
//             }
//         }
//         None
//     }

//     // Deallocate a memory block
//     fn deallocate(&mut self, app_id: usize) {
//         if let Some(&block_index) = self.allocations.get(&app_id) {
//             self.pool[block_index].is_allocated = false;
//             self.allocations.remove(&app_id);
//         }
//     }

// }

// // Global shared instance of the Memory Pool Manager
// lazy_static! {
//     static ref MEMORY_POOL_MANAGER: Arc<Mutex<MemoryPoolManager>> = 
//     Arc::new(Mutex::new(MemoryPoolManager::new(
//         /* PMem initialization parameters */)));
// }

// // Global shared instance of the Memory Pool Manager
// lazy_static! {
//     static ref MEMORY_POOL_MANAGER: Arc<Mutex<MemoryPoolManager>> = Arc::new(Mutex::new(MemoryPoolManager::new(
//         // Replace these parameters with actual PMem initialization parameters
//         pmem_addr as *mut u8,
//         pmem_size as usize,
//         block_size as usize,
//     )));
// }

// // Global shared instance of the Memory Pool Manager
// lazy_static! {
//     static ref MEMORY_POOL_MANAGER: Arc<Mutex<MemoryPoolManager>> = Arc::new(Mutex::new(MemoryPoolManager::new(
//         /* PMem initialization parameters */
//         std::ptr::null_mut(),
//         0,
//         4096,
//     )));
// }



#[derive(Copy, Clone)]
struct MmMeta {
    file_name: [u8; 64],
    offset: u64,
    size: u64,
    magic: u32,
}

impl MmMeta {
    const fn new() -> Self {
        Self {
            file_name: [0; 64],
            offset: 0,
            size: 0,
            magic: 0x66666666,
        }
    }
}

struct MetaBlock {
    magic: u32,
    mm_meta_nr: u32,
    mm_meta: [MmMeta; MM_META_NR],
}

impl MetaBlock {
    const fn new() -> Self {
        Self {
            magic: 0xdeadbeef,
            mm_meta_nr: 0,
            mm_meta: [MmMeta::new(); MM_META_NR],
        }
    }
}

struct PMMmapRegisterCenter {
    filename: String,
    fd: libc::c_int,
    shmfile_mmap_addr_start: u64,
    shmfile_mmap_data_addr_start: u64,
    shmfile_mmap_addr: AtomicU64,
    meta_block: Mutex<&'static mut MetaBlock>,
}

impl PMMmapRegisterCenter {
    fn new(numa_id: i32) -> Self {
        let filename = format!("/dev/dax{}.0", numa_id);
        let fd = unsafe {
            libc::open(
                filename.as_ptr() as *const libc::c_char,
                libc::O_RDWR,
            )
        };

        if fd < 0 {
            panic!("Failed to open file {}: {}", filename, std::io::Error::last_os_error());
        }

        let data = unsafe {
            libc::mmap(
                ptr::null_mut(),
                DAX_MM_SIZE as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                fd,
                0,
            )
        };

        if data == libc::MAP_FAILED {
            panic!("mmap failed");
        }

        let meta_block = unsafe { &mut *(data as *mut MetaBlock) };

        if meta_block.magic != MetaBlock::new().magic {
            // First use of this device, initialize meta block
            *meta_block = MetaBlock::new();
        }

        let shmfile_mmap_addr_start = data as u64;
        let shmfile_mmap_data_addr_start = shmfile_mmap_addr_start + META_BLOCK_SIZE as u64;
        let shmfile_mmap_addr = AtomicU64::new(shmfile_mmap_data_addr_start);

        Self {
            filename,
            fd,
            shmfile_mmap_addr_start,
            shmfile_mmap_data_addr_start,
            shmfile_mmap_addr,
            meta_block: Mutex::new(meta_block),
        }
    }

    fn register(&self, name: &str, size: u64) -> *mut u8 {
        let mut meta_block = self.meta_block.lock().unwrap();
        let mut offset = 0;

        for mm_meta in &mut meta_block.mm_meta {
            if mm_meta.file_name == name.as_bytes() {
                if mm_meta.size != size {
                    // Reinitialize meta block
                    **meta_block = MetaBlock::new();
                    break;
                } else {
                    offset = mm_meta.offset;
                    break;
                }
            }
        }

        if offset == 0 {
            let aligned_size = (size + ALIGN - 1) / ALIGN * ALIGN;
            offset = self
                .shmfile_mmap_addr
                .fetch_add(aligned_size, Ordering::Relaxed);

            let mm_meta_nr = meta_block.mm_meta_nr as usize;
            let mm_meta = &mut meta_block.mm_meta[mm_meta_nr];
            mm_meta.file_name[..name.len()].copy_from_slice(name.as_bytes());
            mm_meta.offset = offset - self.shmfile_mmap_data_addr_start;
            mm_meta.size = size;
            meta_block.mm_meta_nr += 1;
        }

        (self.shmfile_mmap_data_addr_start + offset) as *mut u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_retrieve_region() {
        let pm_center = PMMmapRegisterCenter::new(1); // Assuming NUMA ID 1

        // Register a new region
        let name = "test_region";
        let size = 1024 * 1024; // 1 MB
        let ptr = pm_center.register(name, size);

        // Write some data to the region
        let data = vec![1u8; size as usize];
        unsafe { ptr.copy_from_nonoverlapping(data.as_ptr(), size as usize) };

        // Re-register the same region
        let ptr2 = pm_center.register(name, size);
        assert_eq!(ptr as u64, ptr2 as u64, "Pointers should match for the same region");

        // Verify the written data
        let mut read_data = vec![0u8; size as usize];
        unsafe { read_data.as_mut_ptr().copy_from_nonoverlapping(ptr, size as usize) };
        assert_eq!(read_data, data, "Data should match the written data");

        // Register a region with a different size
        let name2 = "test_region_2";
        let size2 = 2 * 1024 * 1024; // 2 MB
        let ptr3 = pm_center.register(name2, size2);
        assert_ne!(ptr as u64, ptr3 as u64, "Pointers should not match for different sizes");
    }

    #[test]
    fn register_and_reinitialize() {
        let pm_center = PMMmapRegisterCenter::new(1); // Assuming NUMA ID 1

        // Register a region
        let name = "test_region";
        let size = 1024 * 1024; // 1 MB
        let ptr = pm_center.register(name, size);

        // Re-register the same region with a different size
        let new_size = 2 * 1024 * 1024; // 2 MB
        let ptr2 = pm_center.register(name, new_size);
        assert_ne!(ptr as u64, ptr2 as u64, "Pointers should not match after reinitializing");
    }
}