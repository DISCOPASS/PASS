
use std::io::Write;
use std::path::Path;
use std::process::exit;
use serde_json; // requires serde and serde_json dependencies
use serde_json::{json, Value};

use std::io::ErrorKind;
use std::fs;
use std::fs::File;

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::io::AsRawFd;
use libc::{mmap, munmap, open, PROT_READ, PROT_WRITE, MAP_SHARED};

const SOURCE_FILE_PATH: &str = "/tmp/snapshots/recognition.mem";
const PMEM_REGION_PATH: &str = "/dev/dax1.0";
const FILE_SIZE: usize = 1_073_741_824; // 1GB
// const DEVICE_SIZE: usize = 532_708_065_280; // 512GB
const OFFSET: usize = 307 * 1024 * 1024 * 1024; // 307GB

fn validate2() -> io::Result<()> {
    // Open the source file for reading
    let source_file = OpenOptions::new().read(true).open(SOURCE_FILE_PATH)?;
    let source_fd = source_file.as_raw_fd();

    // Open the DAX device for reading
    let path_cstring = CString::new(PMEM_REGION_PATH).unwrap();
    let dax_fd = unsafe { open(path_cstring.as_ptr(), libc::O_RDONLY) };

    if dax_fd < 0 {
        return Err(io::Error::last_os_error());
    }

    // Memory map the source file
    let source_addr = unsafe {
        mmap(
            std::ptr::null_mut(),
            FILE_SIZE,
            PROT_READ,
            MAP_SHARED,
            source_fd,
            0,
        )
    };
    if source_addr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }

    // Memory map the DAX device
    let dax_addr = unsafe {
        mmap(
            std::ptr::null_mut(),
            FILE_SIZE,
            PROT_READ,
            MAP_SHARED,
            dax_fd,
            5 * 0x40000000
            // OFFSET as i64,
        )
    };
    if dax_addr == libc::MAP_FAILED {
        unsafe { munmap(source_addr, FILE_SIZE) };
        return Err(io::Error::last_os_error());
    }

    // // Compare the contents
    // let result = unsafe {
    //     std::ptr::eq(
    //         std::slice::from_raw_parts(source_addr as *const u8, FILE_SIZE),
    //         std::slice::from_raw_parts(dax_addr as *const u8, FILE_SIZE)
    //     )
    // };

    // // Report the result
    // if result {
    //     println!("The contents are identical.");
    // } else {
    //     println!("The contents are different.");
    // }

    let source_slice = unsafe {
        std::slice::from_raw_parts(source_addr as *const u8, FILE_SIZE)
    };
    let dax_slice = unsafe {
        std::slice::from_raw_parts(dax_addr as *const u8, FILE_SIZE)
    };

    let mut differences_found = false;
    for (index, (&byte_source, &byte_dax)) in source_slice.iter().zip(dax_slice.iter()).enumerate() {
        if byte_source != byte_dax {
            println!("Difference found at offset {}: Source byte = {}, DAX byte = {}", index, byte_source, byte_dax);
            differences_found = true;
        }
    }

    if !differences_found {
        println!("No differences found.");
    }
    // Clean up
    unsafe {
        munmap(source_addr, FILE_SIZE);
        munmap(dax_addr, FILE_SIZE);
        libc::close(source_fd);
        libc::close(dax_fd);
    }
    Ok(())
}

fn std_snap_copy(source_file_path: &str, lenf: usize, pmem_region_path: &str, offset: usize) -> io::Result<()> {
    // let metadata = fs::metadata(source_file_path)?;
    // if metadata.is_file() {
    //     let size = metadata.len();
    //     if size == 1024 * 1024 * 1024 {  
    //         println!("File exists and is 1GB in size");
    //     } else {
    //         println!("File exists but size is not 1GB");
    //     }
    // } else {
    // println!("File does not exist");
    // }
    // println!("source_file_path: {}, lenf: {}, pmem_region_path: {}, offset: {}", source_file_path, lenf, pmem_region_path, offset);
    
    // Open the source file
    let source_file = OpenOptions::new().read(true).open(source_file_path)?;
    let source_fd = source_file.as_raw_fd();

    // Open the DAX device
    let path_cstring = CString::new(pmem_region_path).unwrap();
    let dax_fd = unsafe { open(path_cstring.as_ptr(), libc::O_RDWR) };

    if dax_fd < 0 {
        return Err(io::Error::last_os_error());
    }

    // Map the source file
    let source_addr = unsafe {
        mmap(
            std::ptr::null_mut(),
            lenf,
            PROT_READ,
            MAP_SHARED,
            source_fd,
            0,
        )
    };
    if source_addr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }

    // Map the DAX device
    let dax_addr = unsafe {
        mmap(
            std::ptr::null_mut(),
            lenf,
            PROT_READ | PROT_WRITE,
            MAP_SHARED,
            dax_fd,
            offset as i64,
        )
    };
    if dax_addr == libc::MAP_FAILED {
        unsafe { munmap(source_addr, lenf) };
        return Err(io::Error::last_os_error());
    }

    // Copy the data
    unsafe {
        std::ptr::copy_nonoverlapping(source_addr as *const u8, dax_addr as *mut u8, lenf);
        libc::msync(dax_addr, lenf, libc::MS_SYNC);
    }
    // Unmap the memory regions and close file descriptors
    unsafe {
        munmap(source_addr, lenf);
        munmap(dax_addr, lenf);
        libc::close(source_fd);
        libc::close(dax_fd);
    }

    println!("Copy completed successfully.");
    Ok(())
}

fn file_exists(path: &str) -> bool {
    match fs::metadata(path) {
      Ok(_) => true,
      Err(e) if e.kind() == ErrorKind::NotFound => false,
      Err(_) => false  
    }
  }

// config NVDIMM_PFN
//         bool "PFN: Map persistent (device) memory"
//         default LIBNVDIMM
//         depends on ZONE_DEVICE
//         select ND_CLAIM
//         help
//           Map persistent memory, i.e. advertise it to the memory
//           management sub-system.  By default persistent memory does
//           not support direct I/O, RDMA, or any other usage that
//           requires a 'struct page' to mediate an I/O request.  This
//           driver allocates and initializes the infrastructure needed
//           to support those use cases.

//           Select Y if unsure

// config NVDIMM_DAX
//         bool "NVDIMM DAX: Raw access to persistent memory"
//         default LIBNVDIMM
//         depends on NVDIMM_PFN
//         help
//           Support raw device dax access to a persistent memory
//           namespace.  For environments that want to hard partition
//           persistent memory, this capability provides a mechanism to
//           sub-divide a namespace into character devices that can only be
//           accessed via DAX (mmap(2)).

//           Select Y if unsure

// $ ll  /tmp/snapshots
// total 9437328
// drwxrwxr-x 2 root root        400  ./
// drwxrwxrwt 4 root root        120  ../
// -rw-r--r-- 1 root root 1073741824  chameleon.mem
// -rw-r--r-- 1 root root      13559  chameleon.snap
// -rw-r--r-- 1 root root 1073741824  compression.mem
// -rw-r--r-- 1 root root      13559  compression.snap
// -rw-r--r-- 1 root root 1073741824  ffmpeg.mem
// -rw-r--r-- 1 root root      13559  ffmpeg.snap
// -rw-r--r-- 1 root root 1073741824  image.mem
// -rw-r--r-- 1 root root      13559  image.snap
// -rw-r--r-- 1 root root 1073741824  json.mem
// -rw-r--r-- 1 root root      13559  json.snap
// -rw-r--r-- 1 root root 1073741824  matmul.mem
// -rw-r--r-- 1 root root      13559  matmul.snap
// -rw-r--r-- 1 root root 1073741824  pagerank.mem
// -rw-r--r-- 1 root root      13559  pagerank.snap
// -rw-r--r-- 1 root root 1073741824  pyaes.mem
// -rw-r--r-- 1 root root      13559  pyaes.snap
// -rw-r--r-- 1 root root 1073741824  recognition.mem
// -rw-r--r-- 1 root root      13559  recognition.snap
fn snapshot_batch(){
    let dir_path = Path::new("/tmp/snapshots");
    let mut mem_files = Vec::new();
    let mut snap_files = Vec::new();
    let mut succ_mem = Vec::new();

    if let Ok(entries) = fs::read_dir(dir_path) {
        for entry in entries {
            if let Ok(entry) = entry {
                let file_name = entry.file_name();
                let file_name_str = file_name.to_string_lossy();

                if file_name_str.ends_with(".mem") {
                    mem_files.push(file_name_str.to_string());
                } else if file_name_str.ends_with(".snap") {
                    snap_files.push(file_name_str.to_string());
                }
            }
        }
    } else {
        eprintln!("Error reading directory: {:?}", dir_path);
        std::process::exit(1);
    }

    for mem_file in &mem_files {
        let base_name = mem_file.trim_end_matches(".mem");
        let snap_file = format!("{}.snap", base_name);

        if snap_files.contains(&snap_file) {
            succ_mem.push(base_name.to_string());
        } else {
            println!("Warning: No matching .snap file found for {}", mem_file);
        }
    }

    if mem_files.is_empty() {
        println!("No .mem files found in the specified directory.");
    }

    println!("{:?}", succ_mem);
    //chameleon  compression  ffmpeg  image  json  matmul  pagerank  pyaes  recognition

    let mut func_snap_pos: HashMap<&str, usize> = HashMap::new();
    // let funcs =["image", "rnn", "ffmpeg", "recognition",
    //         "pagerank", "mobilenet", "compression", "sentiment", 
    //         "json", "pyaes", "chameleon", "matmul"];
    // let func_snap_pos = HashMap::from([
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
    //     ]);
    for (addr_index, func) in succ_mem.iter().enumerate() {
        
        let file_p=format!("/tmp/snapshots/{}.mem", func);
        if file_exists(file_p.as_str()) == false
        {
            println!("{} not found", file_p);
            continue;
        }
        
        std_snap_copy(file_p.as_str(), FILE_SIZE, PMEM_REGION_PATH, addr_index * (1<<30));
        func_snap_pos.insert(func,addr_index);
        println!("func={} --> return (start_address={} * 0x40000000,len={},visable={})", func,addr_index,1<<30,true);
    }
    
    // Store the HashMap
    let json_data = json!(func_snap_pos);
    let mut file = File::create("/dev/shm/snapshot_index.json").unwrap();
    file.write_all(json_data.to_string().as_bytes()).unwrap();

}
fn main() -> io::Result<()> {
    // snapshot many funcion's VM memory into PMem
    // snapshot_batch(); 
    validate2()?;

    // // snapshot one funcion's VM memory into PMem
    // let args: Vec<String> = std::env::args().collect();
    // let src_path = &args[1];

    // if src_path.ends_with(".mem"){
    //     if file_exists(src_path.as_str()) == false
    //     {
    //         println!("{} not found", src_path);
    //         exit(0);
    //     }
    //     let func_name = src_path.split(".mem").next().unwrap().rsplit('/').next().unwrap();
    //                         println!("{}", func_name);
    //     std_snap_copy(src_path.as_str(), FILE_SIZE, PMEM_REGION_PATH, 0);
    //     // Store the HashMap 
    //     let json_data = json!(HashMap::from([(func_name,0)]));
    //     let mut file = File::create("/dev/shm/snapshot_index.json").unwrap();
    //     file.write_all(json_data.to_string().as_bytes()).unwrap();
    // }
    // else{
    //     println!("{} not snapshot memory", src_path);
    //     exit(0);
    // }
        
    // // Reload the JSON file
    // let file = File::open("/dev/shm/snapshot_index.json").unwrap();
    // let loaded_data: Value = serde_json::from_reader(file).unwrap();

    // // Check the contents
    // for (key, value) in loaded_data.as_object().unwrap().iter() {
    //     let value = value.as_i64().unwrap();
    //     println!("{}: {}", key, value);
    //     assert_eq!(*func_snap_pos.get(key.as_str()).unwrap(), value as usize);
    // }

    Ok(())
}

