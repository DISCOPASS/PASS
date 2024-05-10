# Getting Started with Firecracker

## Prerequisites

You can check if your system meets the requirements by running
`firecracker/tools/devtool checkenv`.

An opinionated way to run Firecracker is to launch an
[EC2](https://aws.amazon.com/ec2/) `i3.metal` instance with Ubuntu 18.04.

Nested virtualization is not supported on non-metal EC2 instances. This is why
`.metal` instances are used.

### Architecture & OS

Firecracker supports **x86_64** and **aarch64** Linux, see
[specific supported kernels](kernel-policy.md).

### KVM

Firecracker requires [the KVM Linux kernel module](https://www.linux-kvm.org/).

The presence of the KVM module can be checked with:

```bash
lsmod | grep kvm
```

An example output where it is enabled:

```bash
kvm_intel             348160  0
kvm                   970752  1 kvm_intel
irqbypass              16384  1 kvm
```

Some Linux distributions use the `kvm` group to manage access to `/dev/kvm`,
while others rely on access control lists. If you have the ACL package for your
distro installed, you can grant Read+Write access with:

```bash
sudo setfacl -m u:${USER}:rw /dev/kvm
```

Otherwise, if access is managed via the `kvm` group:

```bash
[ $(stat -c "%G" /dev/kvm) = kvm ] && sudo usermod -aG kvm ${USER} \
&& echo "Access granted."
```

If none of the above works, you will need to either install the file
system ACL package for your distro and use the `setfacl` command as above,
or run Firecracker as `root` (via `sudo`).

You can check if you have access to `/dev/kvm` with:

```bash
[ -r /dev/kvm ] && [ -w /dev/kvm ] && echo "OK" || echo "FAIL"
```

## Running Firecracker

In production, Firecracker is designed to be run inside
an execution jail, set up by the [`jailer`](../src/jailer/) binary. This is how
our [integration test suite](#running-the-integration-test-suite) does it. This
guide will not use the [`jailer`](../src/jailer/).

```bash
ARCH="$(uname -m)"

# Download a linux kernel binary
wget https://s3.amazonaws.com/spec.ccfc.min/img/quickstart_guide/${ARCH}/kernels/vmlinux.bin

# Download a rootfs
wget https://s3.amazonaws.com/spec.ccfc.min/ci-artifacts/disks/${ARCH}/ubuntu-18.04.ext4

# Download the ssh key for the rootfs
wget https://s3.amazonaws.com/spec.ccfc.min/ci-artifacts/disks/${ARCH}/ubuntu-18.04.id_rsa

# Set user read permission on the ssh key
chmod 400 ./ubuntu-18.04.id_rsa

# Clone the firecracker repository
git clone https://github.com/firecracker-microvm/firecracker

# Start docker
sudo systemctl start docker

# Build firecracker
#
# It is possible to build for gnu, by passing the arguments '-l gnu'.
#
# This will produce the firecracker and jailer binaries under
# `./firecracker/build/cargo_target/${toolchain}/debug`.
#
sudo ./firecracker/tools/devtool build

API_SOCKET="./firecracker.socket"

# Remove API unix socket
rm -f $API_SOCKET

# Run firecracker
./firecracker/build/cargo_target/${ARCH}-unknown-linux-musl/debug/firecracker \
    --api-sock "${API_SOCKET}"
```

In a new terminal (do not close the 1st one):

```bash
TAP_DEV="tap0"
TAP_IP="172.16.0.1"
MASK_SHORT="/30"

# Setup network interface
sudo ip link del "$TAP_DEV" 2> /dev/null || true
sudo ip tuntap add dev "$TAP_DEV" mode tap
sudo ip addr add "${TAP_IP}${MASK_SHORT}" dev "$TAP_DEV"
sudo ip link set dev "$TAP_DEV" up

# Enable ip forwarding
sudo sh -c "echo 1 > /proc/sys/net/ipv4/ip_forward"

# Set up microVM internet access
sudo iptables -t nat -D POSTROUTING -o eth0 -j MASQUERADE || true
sudo iptables -D FORWARD -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT \
    || true
sudo iptables -D FORWARD -i tap0 -o eth0 -j ACCEPT || true
sudo iptables -t nat -A POSTROUTING -o eth0 -j MASQUERADE
sudo iptables -I FORWARD 1 -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
sudo iptables -I FORWARD 1 -i tap0 -o eth0 -j ACCEPT

API_SOCKET="./firecracker.socket"
LOGFILE="./firecracker.log"

# Create log file
touch $LOGFILE

# Set log file
curl -X PUT --unix-socket "${API_SOCKET}" \
    --data "{
        \"log_path\": \"${LOGFILE}\",
        \"level\": \"Debug\",
        \"show_level\": true,
        \"show_log_origin\": true
    }" \
    "http://localhost/logger"

KERNEL="./vmlinux.bin"
KERNEL_BOOT_ARGS="console=ttyS0 reboot=k panic=1 pci=off"

ARCH=$(uname -m)

if [ ${ARCH} = "aarch64" ]; then
    KERNEL_BOOT_ARGS="keep_bootcon ${KERNEL_BOOT_ARGS}"
fi

# Set boot source
curl -X PUT --unix-socket "${API_SOCKET}" \
    --data "{
        \"kernel_image_path\": \"${KERNEL}\",
        \"boot_args\": \"${KERNEL_BOOT_ARGS}\"
    }" \
    "http://localhost/boot-source"

ROOTFS="./ubuntu-18.04.ext4"

# Set rootfs
curl -X PUT --unix-socket "${API_SOCKET}" \
    --data "{
        \"drive_id\": \"rootfs\",
        \"path_on_host\": \"${ROOTFS}\",
        \"is_root_device\": true,
        \"is_read_only\": false
    }" \
    "http://localhost/drives/rootfs"

# The IP address of a guest is derived from its MAC address with
# `fcnet-setup.sh`, this has been pre-configured in the guest rootfs. It is
# important that `TAP_IP` and `FC_MAC` match this.
FC_MAC="06:00:AC:10:00:02"

# Set network interface
curl -X PUT --unix-socket "${API_SOCKET}" \
    --data "{
        \"iface_id\": \"net1\",
        \"guest_mac\": \"$FC_MAC\",
        \"host_dev_name\": \"$TAP_DEV\"
    }" \
    "http://localhost/network-interfaces/net1"

# API requests are handled asynchronously, it is important the configuration is
# set, before `InstanceStart`.
sleep 0.015s

# Start microVM
curl -X PUT --unix-socket "${API_SOCKET}" \
    --data "{
        \"action_type\": \"InstanceStart\"
    }" \
    "http://localhost/actions"

# API requests are handled asynchronously, it is important the microVM has been
# started before we attempt to SSH into it.
sleep 0.015s

# SSH into the microVM
sudo ssh -i ./ubuntu-18.04.id_rsa 172.16.0.2

# Use `root` for both the login and password.
# Run `reboot` to exit.
```

Issuing a `reboot` command inside the guest will gracefully shutdown Firecracker.
This is due to the fact that Firecracker doesn't implement guest power management.

### Configuring the microVM without sending API requests

You can boot a guest without using the API socket by passing the parameter
`--config-file` to the Firecracker process.
E.g.:

```wrap
./firecracker --api-sock /tmp/firecracker.socket --config-file <path_to_the_configuration_file>
```

`path_to_the_configuration_file` is the path to a JSON file with the
configuration for all of the microVM's resources. The JSON **must** contain the
configuration for the guest kernel and rootfs, all of the other resources are
optional. This configuration method will also start the microVM, as such you
need to specify all desired pre-boot configurable resources in the JSON. The
names of the resources can be seen in
[`firecracker.yaml`](../src/api_server/swagger/firecracker.yaml) and the
names of their fields are the same that are used in the API requests.

An example of configuration file is provided:
[`tests/framework/vm_config.json`](../tests/framework/vm_config.json).

After the microVM is started you can still use the socket to send API requests
for post-boot operations.

### Building Firecracker

SSH can be used to work with libraries from private git repos by passing
the `--ssh-keys` flag to specify the paths to your public and private SSH keys
on the host. Both are required for git authentication when fetching the
repositories.

```bash
tools/devtool build --ssh-keys ~/.ssh/id_rsa.pub ~/.ssh/id_rsa
```

Only a single set of credentials is supported. `devtool` cannot fetch multiple
private repos which rely on different credentials.

`tools/devtool build` builds in `debug` to build release binaries pass
`--release` e.g. `tools/devtool build --release`

Documentation on `devtool` can be seen with `tools/devtool --help`.

## Running the Integration Test Suite

Integration tests can be run with `tools/devtool test`.

The test suite is designed to ensure our [SLA parameters](../SPECIFICATION.md)
as measured on EC2 .metal instances, as such performance tests may fail when not
run on these machines. Specifically, don't be alarmed if you see
`tests/integration_tests/performance/test_process_startup_time.py` failing when
not run on an EC2 .metal instance. You can skip performance tests with:

```bash
./tools/devtool test -- --ignore integration_tests/performance
```

## Errors while using `curl` to access the API

Points to check to confirm the API socket is running and accessible:

- Check that the user running the Firecracker process and the user using `curl`
  have equivalent privileges. For example, if you run Firecracker with **sudo**
  that you run `curl` with **sudo** as well.
- [SELinux](https://man7.org/linux/man-pages/man8/selinux.8.html) can regulate
  access to sockets on RHEL based distributions. How user's permissions are
  configured is environmentally specific, but for the purposes of
  troubleshooting you can check if it is enabled in `/etc/selinux/config`.
- With the Firecracker process running using `--api-sock /tmp/firecracker.socket`,
  confirm that the socket is open:
  - `ss -a | grep '/tmp/firecracker.socket'`
  - If you have socat available, try `socat - UNIX-CONNECT:/tmp/firecracker.socket`
    This will throw an explicit error if the socket is inaccessible, or it will pause
    and wait for input to continue.
