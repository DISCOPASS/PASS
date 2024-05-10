# Firecracker Snapshotting

## Table of Contents

- [What is microVM snapshotting?](#about-microvm-snapshotting)
- [Snapshotting in Firecracker](#snapshotting-in-firecracker)
  - [Supported platforms](#supported-platforms)
  - [Overview](#overview)
  - [Snapshot files management](#snapshot-files-management)
  - [Performance](#performance)
  - [Developer preview status](#developer-preview-status)
  - [Limitations](#limitations)
- [Firecracker Snapshotting characteristics](#firecracker-snapshotting-characteristics)
- [Snapshot versioning](#snapshot-versioning)
- [Snapshot API](#snapshot-api)
  - [Pausing the microVM](#pausing-the-microvm)
  - [Creating snapshots](#creating-snapshots)
    - [Creating full snapshots](#creating-full-snapshots)
    - [Creating diff snapshots](#creating-diff-snapshots)
  - [Resuming the microVM](#resuming-the-microvm)
  - [Loading snapshots](#loading-snapshots)
- [Provisioning host disk space for snapshots](#provisioning-host-disk-space-for-snapshots)
- [Ensure continued network connectivity for clones](#ensure-continued-network-connectivity-for-clones)
- [Snapshot security and uniqueness](#snapshot-security-and-uniqueness)
  - [Secure and insecure usage examples](#usage-examples)
  - [Reusing snapshotted states securely](#reusing-snapshotted-states-securely)
- [Vsock device limitation](#vsock-device-limitation)

## About microVM snapshotting

MicroVM snapshotting is a mechanism through which a running microVM and its
resources can be serialized and saved to an external medium in the form of a
`snapshot`. This snapshot can be later used to restore a microVM with its
guest workload at that particular point in time.

## Snapshotting in Firecracker

### Supported platforms

The Firecracker snapshot feature is in [developer preview](../RELEASE_POLICY.md)
on all CPU micro-architectures listed in [README](../../README.md#supported-platforms).
See [this section](#developer-preview-status) for more info.

### Overview

A Firecracker microVM snapshot can be used for loading it later in a different
Firecracker process, and the original guest workload is being simply resumed.

The original guest which the snapshot is created from, should see no side
effects from this process (other than the latency introduced by the snapshot
creation process).

Both network and vsock packet loss can be expected on guests that are resumed
from snapshots in another Firecracker process.
It is also not guaranteed that the state of the network connections survives
the process.

In order to make restoring possible, Firecracker snapshots save the full state
of the following resources:

- the guest memory,
- the emulated HW state (both KVM and Firecracker emulated HW).

The state of the components listed above is generated independently, which brings
flexibility to our snapshotting support. This means that taking a snapshot results
in multiple files that are composing the full microVM snapshot:

- the guest memory file,
- the microVM state file,
- zero or more disk files (depending on how many the guest had; these are
  **managed by the users**).

The design allows sharing of memory pages and read only disks between multiple
microVMs. When loading a snapshot, instead of loading at resume time the full
contents from file to memory, Firecracker creates a
[MAP_PRIVATE mapping](http://man7.org/linux/man-pages/man2/mmap.2.html) of the
memory file, resulting in runtime on-demand loading of memory pages. Any subsequent
memory writes go to a copy-on-write anonymous memory mapping.
This has the advantage of very fast snapshot loading times, but comes with the cost
of having to keep the guest memory file around for the entire lifetime of the
resumed microVM.

### Snapshot files management

The Firecracker snapshot design offers a very simple interface to interact with
snapshots but provides no functionality to package or manage them on the host.

The [threat containment model](../design.md#threat-containment) states
that the host, host/API communication and snapshot files are trusted by Firecracker.

To ensure a secure integration with the snapshot functionality, users need to secure
snapshot files by implementing authentication and encryption schemes while
managing their lifecycle or moving them across the trust boundary, like for
example when provisioning them from a repository to a host over the network.

Firecracker is optimized for fast load/resume, and it's designed to do some
very basic sanity checks only on the vm state file. It only verifies integrity
using a 64-bit CRC value embedded in the vm state file, but this is only
a partial measure to protect against accidental corruption, as the disk
files and memory file need to be secured as well. It is important to note that
CRC computation is validated before trying to load the snapshot. Should it
encounter failure, an error will be shown to the user and the Firecracker
process will be terminated.

### Performance

The Firecracker snapshot create/resume performance depends on the memory size,
vCPU count and emulated devices count.
The Firecracker CI runs snapshot tests on:

- AWS **m5d.metal** and **m6i.metal** instances for Intel
- AWS **m6g.metal** and **c7g.metal** for ARM
- AWS **m6a.metal** for AMD

We are running nightly performance tests for all the enumerated platforms on
all supported kernel versions.
The baselines can be found in their [respective config file](../../tests/integration_tests/performance/configs/).

### Developer preview status

The snapshot functionality is still in developer preview due to the following:

- Poor entropy and replayable randomness when resuming multiple microvms from
  the same snapshot. We do not recommend to use snapshotting in production if
  there is no mechanism to guarantee proper secrecy and uniqueness between
  guests.
  Please see [Snapshot security and uniqueness](#snapshot-security-and-uniqueness).

### Limitations

- High snapshot latency on 5.4+ host kernels due to cgroups V1. We
  strongly recommend to deploy snapshots on cgroups V2 enabled hosts for the
  implied kernel versions - [related issue](https://github.com/firecracker-microvm/firecracker/issues/2129).
- Guest network connectivity is not guaranteed to be preserved after resume.
  For recommendations related to guest network connectivity for clones please
  see [Network connectivity for clones](network-for-clones.md).
- Vsock device does not have full snapshotting support.
  Please see [Vsock device limitation](#vsock-device-limitation).
- Snapshotting on arm64 works for both GICv2 and GICv3 enabled guests.
  However, restoring between different GIC version is not possible.

## Firecracker Snapshotting characteristics

- Fresh Firecracker microVMs are booted using `anonymous` memory, while microVMs
  resumed from snapshot load memory on-demand from the snapshot and copy-on-write
  to anonymous memory.
- Resuming from a snapshot is optimized for speed, while taking a snapshot involves
  some extra CPU cycles for synchronously writing dirty memory pages to the memory
  snapshot file. Taking a snapshot of a fresh microVM, on which dirty pages tracking
  is not enabled, results in the full contents of guest memory being written to the
  snapshot.
- The _memory file_ and _microVM state file_ are generated by Firecracker on snapshot
  creation. The disk contents are _not_ explicitly flushed to their backing files.
- The API calls exposing the snapshotting functionality have clear **Prerequisites**
  that describe the requirements on when/how they should be used.
- The Firecracker microVM's MMDS config is included in the snapshot. However, the
  data store is not persisted across snapshots.
- Configuration information for metrics and logs are not saved to the snapshot.
  These need to be reconfigured on the restored microVM.

## Snapshot versioning

The Firecracker snapshotting implementation offers support for snapshot versioning
(`cross-version snapshots`) in the following contexts:

- Saving snapshots at older versions

  This refers to being able to create a snapshot with any version in the
  `[N, N + o]` interval, while running Firecracker version `N+o`.

  The possibility to save snapshots at older versions might not be offered by
  all Firecracker releases. Depending on the features that it introduces, a new
  Firecracker release `v` might drop the possibility to save snapshots at any
  versions older than `v`.

  For example Firecracker v1.0 and v1.1 adds support for some additional virtio
  features (e.g. notification suppression). These features lead the guest
  drivers to behave in a very specific way and as a consequence the Firecracker
  devices have to respond accordingly. As a result, the snapshots that are
  created while these features are in use will not be backwards compatible with
  previous versions of Firecracker since the devices that come with these older
  versions do not behave in a way that’s compatible with the snapshotted guest
  drivers.

  The list of versions that break snapshot backwards compatibility: `1.0`, `1.1`
- Loading snapshots from older versions (being able to load a snapshot created
  by any Firecracker version in the `[N, N + o]` interval, in a Firecracker
  version `N+o`).

The design supports an unlimited number of versions, the value of `o` (maximum number
of older versions that we can restore from / save a snapshot to, from the current
version) will be defined later.

## Snapshot API

Firecracker exposes the following APIs for manipulating snapshots: `Pause`, `Resume`
and `CreateSnapshot` can be called only after booting the microVM, while `LoadSnapshot`
is allowed only before boot.

### Pausing the microVM

To create a snapshot, first you have to pause the running microVM and its vCPUs with
the following API command:

```bash
curl --unix-socket /tmp/firecracker.socket -i \
    -X PATCH 'http://localhost/vm' \
    -H 'Accept: application/json' \
    -H 'Content-Type: application/json' \
    -d '{
            "state": "Paused"
    }'
```

**Prerequisites**: The microVM is booted.
                   Successive calls of this request keep the microVM in the `Paused`
                   state.
**Effects**:

- _on success_: microVM is guaranteed to be `Paused`.
- _on failure_: no side-effects.

### Creating snapshots

Now that the microVM is paused, you can create a snapshot, which can be either
a `full`one or a `diff` one. Full snapshots always create a complete,
resume-able snapshot of the current microVM state and memory. Diff snapshots
save the current microVM state and the memory dirtied since the last snapshot
(full or diff). Diff snapshots are not resume-able, but can be merged into a
full snapshot. In this context, we will refer to the base as the first memory
file created by a `/snapshot/create` API call and the layer as a memory file
created by a subsequent `/snapshot/create` API call. The order in which the
snapshots were created matters and they should be merged in the same order
in which they were created. To merge a `diff` snapshot memory file on
top of a base, users should copy its content over the base. This can be done
using the `rebase-snap` tool provided with the firecracker release:

```bash
rebase-snap --base-file path/to/base --diff-file path/to/layer
```

After executing the command above, the base would be a resumable snapshot memory
file describing the state of the memory at the moment of creation of the layer.
More layers which were created later can be merged on top of this base.

This process needs to be repeated for each layer until the one describing the
desired memory state is merged on top of the base, which is constantly updated
with information from previously merged layers. Please note that users should
not merge state files which resulted from `/snapshot/create` API calls and
they should use the state file created in the same call as the memory file
which was merged last on top of the base.

#### Creating full snapshots

For creating a full snapshot, you can use the following API command:

```bash
curl --unix-socket /tmp/firecracker.socket -i \
    -X PUT 'http://localhost/snapshot/create' \
    -H  'Accept: application/json' \
    -H  'Content-Type: application/json' \
    -d '{
            "snapshot_type": "Full",
            "snapshot_path": "./snapshot_file",
            "mem_file_path": "./mem_file",
            "version": "1.0.0"
    }'
```

Details about the required and optional fields can be found in the
[swagger definition](../../src/api_server/swagger/firecracker.yaml).

*Note*: If the files indicated by `snapshot_path` and `mem_file_path` don't
exist at the specified paths, then they will be created right before generating
the snapshot. If they exist, the files will be truncated and overwritten.

**Prerequisites**: The microVM is `Paused`.

**Effects**:

- _on success_:
  - The file indicated by `snapshot_path` (e.g. `/path/to/snapshot_file`)
    contains the devices' model state and emulation state. The one indicated
    by `mem_file_path`(e.g. `/path/to/mem_file`) contains a full copy of the
    guest memory.
  - The generated snapshot files are immediately available to be used (current process
    releases ownership). At this point, the block devices backing files should be
    backed up externally by the user.
    Please note that block device contents are only guaranteed to be committed/flushed
    to the host FS, but not necessarily to the underlying persistent storage
    (could still live in host FS cache).
  - If diff snapshots were enabled, the snapshot creation resets then the
    dirtied page bitmap and marks all pages clean (from a diff snapshot point
    of view).
  - If a `version` is specified, the new snapshot is saved at that version,
    otherwise it will be saved at the latest snapshot version of the running
    Firecracker. The version is only used for the microVM state file as it
    contains internal state structures for device emulation, vCPUs and others
    that can change their format from a Firecracker version to another.
    Versioning is not required for the block and memory files.

- _on failure_: no side-effects.

**Notes**:

- The separate block device file components of the snapshot have to be handled
  by the user.
- If specified, `version` must match the firecracker version that introduced a
  snapshot version, which may differ from the running Firecracker version. For
  example, if you are running on `1.1.2` and want to target version `1.0.4`, you
  should specify `1.0.0`. Not specifying `version` uses the latest snapshot
  version available to that version.

#### Creating diff snapshots

For creating a diff snapshot, you should use the same API command, but with
`snapshot_type` field set to `Diff`.

*Note*: If not specified, `snapshot_type` is by default `Full`.

```bash
curl --unix-socket /tmp/firecracker.socket -i \
    -X PUT 'http://localhost/snapshot/create' \
    -H  'Accept: application/json' \
    -H  'Content-Type: application/json' \
    -d '{
            "snapshot_type": "Diff",
            "snapshot_path": "./snapshot_file",
            "mem_file_path": "./mem_file",
            "version": "1.0.0"
    }'
```

**Prerequisites**: The microVM is `Paused`.

*Note*: On a fresh microVM, `track_dirty_pages` field should be set to `true`,
when configuring the `/machine-config` resource, while on a snapshot loaded
microVM, `enable_diff_snapshots` from `PUT /snapshot/load`request body,
should be set.

**Effects**:

- _on success_:
  - The file indicated by `snapshot_path` contains the devices' model state and
    emulation state, same as when creating a full snapshot. The one indicated by
    `mem_file_path` contains this time a **diff copy** of the guest memory; the
    diff consists of the memory pages which have been dirtied since the last
    snapshot creation or since the creation of the microVM, whichever of these
    events was the most recent.
  - All the other effects mentioned in the **Effects** paragraph from
    **Creating full snapshots** section apply here.
- _on failure_: no side-effects.

*Note*: This is an example of an API command that enables dirty page tracking:

```bash
curl --unix-socket /tmp/firecracker.socket -i  \
    -X PUT 'http://localhost/machine-config' \
    -H 'Accept: application/json'            \
    -H 'Content-Type: application/json'      \
    -d '{
            "vcpu_count": 2,
            "mem_size_mib": 1024,
            "smt": false,
            "track_dirty_pages": true
    }'
```

Enabling this support enables KVM dirty page tracking, so it comes at a cost
(which consists of CPU cycles spent by KVM accounting for dirtied pages); it
should only be used when needed.

Creating a snapshot will **not** influence state, will **not** stop or end the microVM,
it can be used as before, so the microVM can be resumed if you still want to
use it.
At this point, in case you plan to continue using the current microVM, you
should make sure to also copy the disk backing files.

### Resuming the microVM

You can resume the microVM by sending the following API command:

```bash
curl --unix-socket /tmp/firecracker.socket -i \
    -X PATCH 'http://localhost/vm' \
    -H 'Accept: application/json' \
    -H 'Content-Type: application/json' \
    -d '{
            "state": "Resumed"
    }'
```

**Prerequisites**: The microVM is `Paused`.
                   Successive calls of this request are ignored (microVM remains
                   in the running state).
**Effects**:

- _on success_: microVM is guaranteed to be `Resumed`.
- _on failure_: no side-effects.

### Loading snapshots

If you want to load a snapshot, you can do that only **before** the microVM is configured
(the only resources that can be configured prior are the Logger and the Metrics systems)
by sending the following API command:

```bash
curl --unix-socket /tmp/firecracker.socket -i \
    -X PUT 'http://localhost/snapshot/load' \
    -H  'Accept: application/json' \
    -H  'Content-Type: application/json' \
    -d '{
            "snapshot_path": "./snapshot_file",
            "mem_backend": {
                "backend_path": "./mem_file",
                "backend_type": "File"
            },
            "enable_diff_snapshots": true,
            "resume_vm": false
    }'
```

The `backend_type` field represents the memory backend type used for loading the
snapshot. Accepted values are:

- `File` - rely on the kernel to handle page faults when loading the contents of
  the guest memory file into memory.
- `Uffd` - use a dedicated user space process to handle page faults that occur
  for the guest memory range. Please refer to [this](handling-page-faults-on-snapshot-resume.md)
  for more details on handling page faults in the user space.

The meaning of `backend_path` depends on the `backend_type` chosen:

- if using `File`, then `backend_path` should contain the path to the snapshot's
  memory file to be loaded.
- when using `Uffd`, `backend_path` refers to the path of the unix domain socket
  used for communication between Firecracker and the user space process that handles
  page faults.

When relying on the OS to handle page faults, the command below is also accepted.
Note that `mem_file_path` field is currently under the deprecation policy.
`mem_file_path` and `mem_backend` are mutually exclusive, therefore specifying them
both at the same time will return an error.

```bash
curl --unix-socket /tmp/firecracker.socket -i \
    -X PUT 'http://localhost/snapshot/load' \
    -H  'Accept: application/json' \
    -H  'Content-Type: application/json' \
    -d '{
            "snapshot_path": "./snapshot_file",
            "mem_file_path": "./mem_file",
            "enable_diff_snapshots": true,
            "resume_vm": false
    }'
```

Details about the required and optional fields can be found in the
[swagger definition](../../src/api_server/swagger/firecracker.yaml).

**Prerequisites**: A full memory snapshot and a microVM state file **must** be
provided. The disk backing files, network interfaces backing TAPs and/or vsock
backing socket that were used for the original microVM's configuration
should be set up and accessible to the new Firecracker process (in
which the microVM is resumed). These host-resources need to be
accessible at the same relative paths to the new Firecracker process
as they were to the original one.

**Effects:**

- _on success_:
  - The complete microVM state is loaded from snapshot into the current Firecracker
    process.
  - It then resets the dirtied page bitmap and marks all pages clean (from a
    diff snapshot point of view).
  - The loaded microVM is now in the `Paused` state, so it needs to be resumed
    for it to run.
  - The memory file (pointed by `backend_path` when using `File` backend type,
    or pointed by `mem_file_path`) **must** be considered immutable from Firecracker
    and host point of view. It backs the guest OS memory for read access through
    the page cache. External modification to this file corrupts the guest memory
    and leads to undefined behavior.
  - The file indicated by `snapshot_path`, that is used to load from, is
    released and no longer used by this process.
  - If `enable_diff_snapshots` is set, then diff snapshots can be taken
    afterwards.
  - If `resume_vm` is set, the vm is automatically resumed if load is
    successful.
- _on failure_: A specific error is reported and then the current Firecracker process
                is ended (as it might be in an invalid state).

*Notes*:
Please, keep in mind that only by setting to true `enable_diff_snapshots`, when
loading a snapshot, or `track_dirty_pages`, when configuring the machine on a
fresh microVM, you can then create a `diff` snapshot. Also, `track_dirty_pages`
is not saved when creating a snapshot, so you need to explicitly set
`enable_diff_snapshots` when sending `LoadSnapshot`command if you want to be
able to do diff snapshots from a loaded microVM.
Another thing that you should be aware of is the following: if a fresh microVM
can create diff snapshots, then if you create a **full** snapshot, the memory
file contains the whole guest memory, while if you create a **diff** one, that
file is sparse and only contains the guest dirtied pages.
With these in mind, some possible snapshotting scenarios are the following:

- `Boot from a fresh microVM` -> `Pause` -> `Create snapshot` -> `Resume` ->
  `Pause` -> `Create snapshot` -> ... ;
- `Boot from a fresh microVM` -> `Pause` -> `Create snapshot` -> `Resume` ->
  `Pause` -> `Resume` -> ... -> `Pause` -> `Create snapshot` -> ... ;
- `Load snapshot` -> `Resume` -> `Pause` -> `Create snapshot` -> `Resume` ->
  `Pause` -> `Create snapshot` -> ... ;
- `Load snapshot` -> `Resume` -> `Pause` -> `Create snapshot` -> `Resume` ->
  `Pause` -> `Resume` -> ... -> `Pause` -> `Create snapshot` -> ... ;
  where `Create snapshot` can refer to either a full or a diff snapshot for
  all the aforementioned flows.

It is also worth knowing, a microVM that is restored from snapshot will be
resumed with the guest OS wall-clock continuing from the moment of the
snapshot creation. For this reason, the wall-clock should be updated to the
current time, on the guest-side. More details on how you could do this can
be found at a [related FAQ](../../FAQ.md#my-guest-wall-clock-is-drifting-how-can-i-fix-it).

## Provisioning host disk space for snapshots

Depending on VM memory size, snapshots can consume a lot of disk space. Firecracker
integrators **must** ensure that the provisioned disk space is sufficient for normal
operation of their service as well as during failure scenarios. If the service exposes
the snapshot triggers to customers, integrators **must** enforce proper disk
quotas to avoid any DoS threats that would cause the service to fail or
function abnormally.

## Ensure continued network connectivity for clones

For recommendations related to continued network connectivity for multiple
clones created from a single Firecracker microVM snapshot please see [this doc](network-for-clones.md).

## Snapshot security and uniqueness

When snapshots are used in a such a manner that a given guest's state is resumed
from more than once, guest information assumed to be unique may in fact not be;
this information can include identifiers, random numbers and random number
seeds, the guest OS entropy pool, as well as cryptographic tokens. Without a
strong mechanism that enables users to guarantee that unique things stay unique
across snapshot restores, we consider resuming execution from the same state
more than once insecure.

For more information please see [this doc](random-for-clones.md)

### Usage examples

#### Example 1: secure usage (currently in dev preview)

```console
Boot microVM A -> ... -> Create snapshot S -> Terminate
                                           -> Load S in microVM B -> Resume -> ...
```

Here, microVM A terminates after creating the snapshot without ever resuming
work, and a single microVM B resumes execution from snapshot S. In this case,
unique identifiers, random numbers, and cryptographic tokens that are meant to
be used once are indeed only used once. In this example, we consider microVM B
secure.

#### Example 2: potentially insecure usage

```console
Boot microVM A -> ... -> Create snapshot S -> Resume -> ...
                                           -> Load S in microVM B -> Resume -> ...
```

Here, both microVM A and B do work starting from the state stored in snapshot S.
Unique identifiers, random numbers, and cryptographic tokens that are meant to
be used once may be used twice. It doesn't matter if microVM A is terminated
before microVM B resumes execution from snapshot S or not. In this example, we
consider both microVMs insecure as soon as microVM A resumes execution.

#### Example 3: potentially insecure usage

```console
Boot microVM A -> ... -> Create snapshot S -> ...
                                           -> Load S in microVM B -> Resume -> ...
                                           -> Load S in microVM C -> Resume -> ...
                                           [...]
```

Here, both microVM B and C do work starting from the state stored in snapshot S.
Unique identifiers, random numbers, and cryptographic tokens that are meant to
be used once may be used twice. It doesn't matter at which points in time
microVMs B and C resume execution, or if microVM A terminates or not after the
snapshot is created. In this example, we consider microVMs B and C insecure, and
we also consider microVM A insecure if it resumes execution.

### Reusing snapshotted states securely

We are currently working to add a functionality that will notify guest operating
systems of the snapshot event in order to enable secure reuse of snapshotted
microVM states, guest operating systems, language runtimes, and cryptographic
libraries. In some cases, user applications will need to handle the snapshot
create/restore events in such a way that the uniqueness and randomness
properties are preserved and guaranteed before resuming the workload.

We've started a discussion on how the Linux operating system might securely
handle being snapshotted [here](https://lkml.org/lkml/2020/10/16/629).

## Vsock device limitation

Vsock must be inactive during snapshot. Vsock device can break if snapshotted
while having active connections. Firecracker snapshots do not capture any
inflight network or vsock (through the linux unix domain socket backend)
traffic that has left or not yet entered Firecracker.

The above, coupled with the fact that Vsock control protocol is not resilient
to vsock packet loss, leads to Vsock device breakage when doing a snapshot while
there are active Vsock connections.

As a solution to the above issue, active Vsock connections prior to
snapshotting the VM are forcibly closed by sending a specific event called
`VIRTIO_VSOCK_EVENT_TRANSPORT_RESET`. The event is sent on `SnapshotCreate`.
On `SnapshotResume`, when the VM becomes active again,
the vsock driver closes all existing connections.
Listen sockets still remain active. Users wanting to build vsock applications that
use the snapshot capability have to take this into consideration. More details
about this event can be found in the official Virtio document
[here](https://docs.oasis-open.org/virtio/virtio/v1.1/virtio-v1.1.pdf),
section 5.10.6.6 Device Events.

Firecracker handles sending the `reset` event to the vsock driver,
thus the customers are no longer responsible for closing
active connections.
