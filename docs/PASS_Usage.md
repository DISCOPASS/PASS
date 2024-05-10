
# Snapshot the memory state of a function's microVM to native byte-addressable PMem.

## After saving the memory state using the official firecracker VMM API, automatically transfer the memory data in the snapshot file to native byte-addressable PMem:
```
cargo run --bin snapshot2pm $Snapshot_Memory_PATH
```

# Restore a function's microVM memory state from native byte-addressable PMem directly

## Prepare to establish a full and valid mapping
```
CC=icx CFLAGS="-O3" cargo run --bin snapstart_mem_handler /tmp/sock.socket $FUN_NAME
```

## Restore a microVM's memory state from a snapshot (with pre-built address mapping) using the uffd interface instead of the file interface
```
curl --unix-socket /tmp/firecracker.socket -i \
    -X PUT 'http://<VMM_controler_ip>/snapshot/load' \
    -H  'Accept: application/json' \
    -H  'Content-Type: application/json' \
    -d '{
        "snapshot_path": $FUN_VM_STATE,
        "mem_backend": {
            "backend_type": "Uffd",
            "backend_path": "/tmp/shenben.socket"
        },
        "enable_diff_snapshots": false,
        "resume_vm": true
    }'
```

# Invoke a function within a MicroVM with data parameters 
## The default invocation IP:port for a MicroVM is `172.16.0.2:5000`
```
curl -s -X POST -H "Content-Type: application/json" -d "$data" http://172.16.0.2:5000/invoke
```
