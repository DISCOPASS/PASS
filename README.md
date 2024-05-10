# PASS

PASS is an augmented hypervisor built upon the open-source Firecracker VMM, enabling direct access to Persistent Memory to expedite the SnapStart execution of short-lived functions.

# Environments

```
Software:
OS: Ubuntu 22.04
Kernel: Linux 5.10.130-0510130-generic
KVM-enabled
Rust
Intel Optane PMEM firmware: 02.02.00.1553

Hardware:
Intel Optane PMEM 200 series 
```
*The persistent memory should be mounted as a /dev/daxX.0 .*


# Getting Started
```
CC=gcc CFLAGS="-O3 -march=native -mtune=native -fopenmp" cargo build --release --target=x86_64-unknown-linux-gnu
```
The Firecracker binary will be placed at `./target/x86_64-unknown-linux-gnu/release/firecracker`.

## Dependencies for launching a VM

### kernel image

You can download the [vmlinux-5.10.bin](https://drive.google.com/file/d/1ylhPWuGstSbB9-qmxJzeZwWTJk0cN8lf/view?usp=sharing), or compile the vm kernel mannully from the [linux kernel source code](https://cdn.kernel.org/pub/linux/kernel/v5.x/linux-5.10.130.tar.xz).

### filesystem image
You can download the [official quoted filesystem image](https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md#getting-a-rootfs-and-guest-kernel-image) or generate a filesystem image refer to [FaaSnap](https://github.com/ucsdsysnet/faasnap/blob/6d47f5a808d34d37213c57e42a302b351e904614/README.md#L20)


# Result Reproduction

We adopted [FaaSnap](https://github.com/ucsdsysnet/faasnap.git)'s evaluation script for assessing our PASS framework's performance, as both leverage the Firecracker VMM. To replicate our paper's results in comparison with FaaSnap, input the Firecracker binary path into FaaSnap's JSON template (*test-2inputs.json* / *test-6inputs.json* --> `executables`). 
(You may need to pre-configure/pre-install the necessary environment dependencies for FaaSnap in advance.)

**Compared Approaches**:

- *Lambda SnapStart* : snapshot memory file in SSD
- *Vanilla* : snapshot memory file in PMem filesystem
- *FaaSnap* : snapshot memory file in general - filesystem with prefetching technology
- *DRAM-Cached* : snapshot memory file in DRAM
- *PASS* : snapshot memory in native byte-adressable PMem with PASS technology


## Experiment for SnapStart Execution Time (Figure 9-10)
1. Configure `test-2inputs.json`.

    - `base_path` is where snapshot files location. 
    - `kernels` are the locations of vanilla and sanpage kernels. (Configure them in the same one kernenl since we do not invlove any VM kernel optimiztions.)
    - filesystem `images` is the rootfs location.
    - `executables` is the Firecracker binary for both FaaSnap / PASS.
    - specify `redis_host` and `redis_passwd` accordingly.
    - `home_dir` is the current runtime directory.
    - `test_dir` is where snapshot files location. 
    - Specify `host` and `trace_api`.
 

1. Run tests:
    - `sudo ./test.py test-2inputs.json`
    - After the tests finish, go to `http://<ip>:9411`, and use traceIDs to find trace results.

## Experiment for High-Concurrency SnapStart(Figure 11-12)
1. Configure `test-2inputs.json`.
    - Same as the above, except for `parallelism` and `par_snapshots`.
    - Set both `parallelism` and `par_snapshots` to the target parallelism.

1. Run tests:
    `sudo ./test.py test-2inputs.json`
    - After the tests finish, go to `http://<ip>:9411`, and use traceIDs to find trace results.

If your goal is to simply run a microVM with PASS support, without the need for performance evaluation/comparison, you can consult the [sample case](./docs/PASS_Usage.md) for guidance.