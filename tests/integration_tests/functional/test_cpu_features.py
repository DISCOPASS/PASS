# Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Tests for the CPU topology emulation feature."""

# pylint: disable=too-many-lines

import os
import platform
import re
import shutil
import sys
import time
from difflib import unified_diff
from pathlib import Path

import pandas as pd
import pytest

import framework.utils_cpuid as cpuid_utils
from framework import utils
from framework.artifacts import NetIfaceConfig
from framework.defs import SUPPORTED_HOST_KERNELS
from framework.utils_cpu_templates import SUPPORTED_CPU_TEMPLATES

PLATFORM = platform.machine()
UNSUPPORTED_HOST_KERNEL = (
    utils.get_kernel_version(level=1) not in SUPPORTED_HOST_KERNELS
)


def clean_and_mkdir(dir_path):
    """
    Create a clean directory
    """
    shutil.rmtree(dir_path, ignore_errors=True)
    os.makedirs(dir_path)


def _check_cpuid_x86(test_microvm, expected_cpu_count, expected_htt):
    expected_cpu_features = {
        "cpu count": "{} ({})".format(hex(expected_cpu_count), expected_cpu_count),
        "CLFLUSH line size": "0x8 (8)",
        "hypervisor guest status": "true",
        "hyper-threading / multi-core supported": expected_htt,
    }

    cpuid_utils.check_guest_cpuid_output(
        test_microvm, "cpuid -1", None, "=", expected_cpu_features
    )


def _check_extended_cache_features(vm):
    l3_params = cpuid_utils.get_guest_cpuid(vm, "0x80000006")[(0x80000006, 0, "edx")]

    # fmt: off
    line_size     = (l3_params >>  0) & 0xFF
    lines_per_tag = (l3_params >>  8) & 0xF
    assoc         = (l3_params >> 12) & 0xF
    cache_size    = (l3_params >> 18) & 0x3FFF
    # fmt: on

    assert line_size > 0
    assert lines_per_tag == 0x1  # This is hardcoded in the AMD spec
    assert assoc == 0x9  # This is hardcoded in the AMD spec
    assert cache_size > 0


def _check_cpu_features_arm(test_microvm):
    if cpuid_utils.get_instance_type() == "m6g.metal":
        expected_cpu_features = {
            "Flags": "fp asimd evtstrm aes pmull sha1 sha2 crc32 atomics fphp "
            "asimdhp cpuid asimdrdm lrcpc dcpop asimddp ssbs",
        }
    else:
        expected_cpu_features = {
            "Flags": "fp asimd evtstrm aes pmull sha1 sha2 crc32 atomics fphp "
            "asimdhp cpuid asimdrdm jscvt fcma lrcpc dcpop sha3 sm3 sm4 asimddp "
            "sha512 asimdfhm dit uscat ilrcpc flagm ssbs",
        }

    cpuid_utils.check_guest_cpuid_output(
        test_microvm, "lscpu", None, ":", expected_cpu_features
    )


def get_cpu_template_dir(cpu_template):
    """
    Utility function to return a valid string which will be used as
    name of the directory where snapshot artifacts are stored during
    snapshot test and loaded from during restore test.

    """
    return cpu_template if cpu_template else "none"


def skip_test_based_on_artifacts(snapshot_artifacts_dir):
    """
    It is possible that some X template is not supported on
    the instance where the snapshots were created and,
    snapshot is loaded on an instance where X is supported. This
    results in error since restore doesn't find the file to load.
    e.g. let's suppose snapshot is created on Skylake and restored
    on Cascade Lake. So, the created artifacts could just be:
    snapshot_artifacts/wrmsr/vmlinux-4.14/T2S
    but the restore test would fail because the files in
    snapshot_artifacts/wrmsr/vmlinux-4.14/T2CL won't be available.
    To avoid this we make an assumption that if template directory
    does not exist then snapshot was not created for that template
    and we skip the test.
    """
    if not Path.exists(snapshot_artifacts_dir):
        reason = f"\n Since {snapshot_artifacts_dir} does not exist \
                we skip the test assuming that snapshot was not"
        pytest.skip(re.sub(" +", " ", reason))


@pytest.mark.skipif(PLATFORM != "x86_64", reason="CPUID is only supported on x86_64.")
@pytest.mark.parametrize(
    "num_vcpus",
    [1, 2, 16],
)
@pytest.mark.parametrize(
    "htt",
    [True, False],
)
def test_cpuid(test_microvm_with_api, network_config, num_vcpus, htt):
    """
    Check the CPUID for a microvm with the specified config.

    @type: functional
    """
    vm = test_microvm_with_api
    vm.spawn()
    vm.basic_config(vcpu_count=num_vcpus, smt=htt)
    _tap, _, _ = vm.ssh_network_config(network_config, "1")
    vm.start()
    _check_cpuid_x86(vm, num_vcpus, "true" if num_vcpus > 1 else "false")


@pytest.mark.skipif(PLATFORM != "x86_64", reason="CPUID is only supported on x86_64.")
@pytest.mark.skipif(
    cpuid_utils.get_cpu_vendor() != cpuid_utils.CpuVendor.AMD,
    reason="L3 cache info is only present in 0x80000006 for AMD",
)
def test_extended_cache_features(test_microvm_with_api, network_config):
    """
    Check extended cache features (leaf 0x80000006).
    """
    vm = test_microvm_with_api
    vm.spawn()
    vm.basic_config()
    _tap, _, _ = vm.ssh_network_config(network_config, "1")
    vm.start()
    _check_extended_cache_features(vm)


@pytest.mark.skipif(
    PLATFORM != "x86_64", reason="The CPU brand string is masked only on x86_64."
)
def test_brand_string(test_microvm_with_api, network_config):
    """
    Ensure good formatting for the guest brand string.

    * For Intel CPUs, the guest brand string should be:
        Intel(R) Xeon(R) Processor @ {host frequency}
    where {host frequency} is the frequency reported by the host CPUID
    (e.g. 4.01GHz)
    * For AMD CPUs, the guest brand string should be:
        AMD EPYC
    * For other CPUs, the guest brand string should be:
        ""

    @type: functional
    """
    test_microvm = test_microvm_with_api
    test_microvm.spawn()

    test_microvm.basic_config(vcpu_count=1)
    _tap, _, _ = test_microvm.ssh_network_config(network_config, "1")
    test_microvm.start()

    guest_cmd = "cat /proc/cpuinfo | grep 'model name' | head -1"
    _, stdout, stderr = test_microvm.ssh.execute_command(guest_cmd)
    assert stderr.read() == ""
    stdout = stdout.read()

    cpu_vendor = cpuid_utils.get_cpu_vendor()
    if cpu_vendor == cpuid_utils.CpuVendor.AMD:
        # Assert the model name matches "AMD EPYC"
        mo = re.search("model name.*: AMD EPYC", stdout)
        assert mo
    elif cpu_vendor == cpuid_utils.CpuVendor.INTEL:
        # Get host frequency
        cif = open("/proc/cpuinfo", "r", encoding="utf-8")
        cpu_info = cif.read()
        mo = re.search("model name.*:.* ([0-9]*.[0-9]*[G|M|T]Hz)", cpu_info)
        assert mo
        host_frequency = mo.group(1)

        # Assert the model name matches "Intel(R) Xeon(R) Processor @ "
        mo = re.search(
            "model name.*: Intel\\(R\\) Xeon\\(R\\) Processor @ ([0-9]*.[0-9]*[T|G|M]Hz)",
            stdout,
        )
        assert mo
        # Get the frequency
        guest_frequency = mo.group(1)

        # Assert the guest frequency matches the host frequency
        assert host_frequency == guest_frequency
    else:
        assert False


# Some MSR values should not be checked since they can change at guest runtime
# and between different boots.
# Current exceptions:
# * FS and GS change on task switch and arch_prctl.
# * TSC is different for each guest.
# * MSR_{C, L}STAR used for SYSCALL/SYSRET; can be different between guests.
# * MSR_IA32_SYSENTER_E{SP, IP} used for SYSENTER/SYSEXIT; same as above.
# * MSR_KVM_{WALL, SYSTEM}_CLOCK addresses for struct pvclock_* can be different.
# * MSR_IA32_TSX_CTRL is not available to read/write via KVM (known limitation).
#
# More detailed information about MSRs can be found in the Intel® 64 and IA-32
# Architectures Software Developer’s Manual - Volume 4: Model-Specific Registers
# Check `arch_gen/src/x86/msr_idex.rs` and `msr-index.h` in upstream Linux
# for symbolic definitions.
# fmt: off
MSR_EXCEPTION_LIST = [
    "0x10",        # MSR_IA32_TSC
    "0x11",        # MSR_KVM_WALL_CLOCK
    "0x12",        # MSR_KVM_SYSTEM_TIME
    "0x122",       # MSR_IA32_TSX_CTRL
    "0x175",       # MSR_IA32_SYSENTER_ESP
    "0x176",       # MSR_IA32_SYSENTER_EIP
    "0x6e0",       # MSR_IA32_TSC_DEADLINE
    "0xc0000082",  # MSR_LSTAR
    "0xc0000083",  # MSR_CSTAR
    "0xc0000100",  # MSR_FS_BASE
    "0xc0000101",  # MSR_GS_BASE
    # MSRs below are required only on T2A, however,
    # we are adding them to the common exception list to keep things simple
    "0x834"     ,  # LVT Performance Monitor Interrupt Register
    "0xc0010007",  # MSR_K7_PERFCTR3
    "0xc001020b",  # Performance Event Counter MSR_F15H_PERF_CTR5
    "0xc0011029",  # MSR_F10H_DECFG also referred to as MSR_AMD64_DE_CFG
    "0x830"     ,  # IA32_X2APIC_ICR is interrupt command register and,
                   # bit 0-7 represent interrupt vector that varies.
    "0x83f"     ,  # IA32_X2APIC_SELF_IPI
                   # A self IPI is semantically identical to an
                   # inter-processor interrupt sent via the ICR,
                   # with a Destination Shorthand of Self,
                   # Trigger Mode equal to Edge,
                   # and a Delivery Mode equal to Fixed.
                   # bit 0-7 represent interrupt vector that varies.
]
# fmt: on


MSR_SUPPORTED_TEMPLATES = ["T2A", "T2CL", "T2S"]


@pytest.fixture(
    name="msr_cpu_template",
    params=set(SUPPORTED_CPU_TEMPLATES).intersection(MSR_SUPPORTED_TEMPLATES),
)
def msr_cpu_template_fxt(request):
    """CPU template fixture for MSR read/write supported CPU templates"""
    return request.param


@pytest.mark.skipif(
    UNSUPPORTED_HOST_KERNEL,
    reason=f"Supported kernels are {SUPPORTED_HOST_KERNELS}",
)
@pytest.mark.timeout(900)
@pytest.mark.nonci
def test_cpu_rdmsr(
    microvm_factory, msr_cpu_template, guest_kernel, rootfs_msrtools, network_config
):
    """
    Test MSRs that are available to the guest.

    This test boots a uVM and tries to read a set of MSRs from the guest.
    The guest MSR list is compared against a list of MSRs that are expected
    when running on a particular combination of host kernel, guest kernel and
    CPU template.

    The list is dependent on:
    * host kernel version, since firecracker relies on MSR emulation provided
      by KVM
    * guest kernel version, since some MSRs are writable from guest uVMs and
      different guest kernels might set different values
    * CPU template, since enabled CPUIDs are different between CPU templates
      and some MSRs are not available if CPUID features are disabled

    This comparison helps validate that defaults have not changed due to
    emulation implementation changes by host kernel patches and CPU templates.

    TODO: This validates T2S, T2CL and T2A templates. Since T2 and C3 did not
    set the ARCH_CAPABILITIES MSR, the value of that MSR is different between
    different host CPU types (see Github PR #3066). So we can either:
    * add an exceptions for different template types when checking values
    * deprecate T2 and C3 since they are somewhat broken

    Testing matrix:
    - All supported guest kernels and rootfs
    - Microvm: 1vCPU with 1024 MB RAM

    @type: functional
    """

    vcpus, guest_mem_mib = 1, 1024
    vm = microvm_factory.build(guest_kernel, rootfs_msrtools, monitor_memory=False)
    vm.spawn()
    vm.ssh_network_config(network_config, "1")
    vm.basic_config(
        vcpu_count=vcpus, mem_size_mib=guest_mem_mib, cpu_template=msr_cpu_template
    )
    vm.start()
    vm.ssh.scp_file("../resources/tests/msr/msr_reader.sh", "/bin/msr_reader.sh")
    _, stdout, stderr = vm.ssh.run("/bin/msr_reader.sh")
    assert stderr.read() == ""

    # Load results read from the microvm
    microvm_df = pd.read_csv(stdout)

    # Load baseline
    # Baselines are taken by running `msr_reader.sh` on:
    # * host running kernel 4.14 and guest 4.14 with the `bionic-msrtools` rootfs
    # * host running kernel 4.14 and guest 5.10 with the `bionic-msrtools` rootfs
    # * host running kernel 5.10 and guest 4.14 with the `bionic-msrtools` rootfs
    # * host running kernel 5.10 and guest 5.10 with the `bionic-msrtools` rootfs
    host_kv = utils.get_kernel_version(level=1)
    guest_kv = re.search("vmlinux-(.*).bin", guest_kernel.name()).group(1)
    baseline_file_name = (
        f"msr_list_{msr_cpu_template}_{host_kv}host_{guest_kv}guest.csv"
    )
    baseline_file_path = f"../resources/tests/msr/{baseline_file_name}"
    baseline_df = pd.read_csv(baseline_file_path)

    # We first want to see if the same set of MSRs are exposed in the microvm.
    # Drop the VALUE columns and compare the 2 dataframes.
    impl_diff = pd.concat(
        [microvm_df.drop(columns="VALUE"), baseline_df.drop(columns="VALUE")],
        keys=["microvm", "baseline"],
    ).drop_duplicates(keep=False)
    assert impl_diff.empty, f"\n {impl_diff}"

    # Now drop the STATUS column to compare values for each MSR
    microvm_val_df = microvm_df.drop(columns="STATUS")
    baseline_val_df = baseline_df.drop(columns="STATUS")

    # pylint: disable=C0121
    microvm_val_df = microvm_val_df[
        microvm_val_df["MSR_ADDR"].isin(MSR_EXCEPTION_LIST) == False
    ]
    baseline_val_df = baseline_val_df[
        baseline_val_df["MSR_ADDR"].isin(MSR_EXCEPTION_LIST) == False
    ]

    # Compare values
    val_diff = pd.concat(
        [microvm_val_df, baseline_val_df], keys=["microvm", "baseline"]
    ).drop_duplicates(keep=False)
    assert val_diff.empty, f"\n {val_diff}"


# These names need to be consistent across the two parts of the snapshot-restore test
# that spans two instances (one that takes a snapshot and one that restores from it)
# fmt: off
SNAPSHOT_RESTORE_SHARED_NAMES = {
    "snapshot_artifacts_root_dir_wrmsr": "snapshot_artifacts/wrmsr",
    "snapshot_artifacts_root_dir_cpuid": "snapshot_artifacts/cpuid",
    "msr_reader_host_fname":             "../resources/tests/msr/msr_reader.sh",
    "msr_reader_guest_fname":            "/bin/msr_reader.sh",
    "msrs_before_fname":                 "msrs_before.txt",
    "msrs_after_fname":                  "msrs_after.txt",
    "cpuid_before_fname":                "cpuid_before.txt",
    "cpuid_after_fname":                 "cpuid_after.txt",
    "snapshot_fname":                    "vmstate",
    "mem_fname":                         "mem",
    "rootfs_fname":                      "bionic-msrtools.ext4",
    # Testing matrix:
    # * Rootfs: Ubuntu 18.04 with msr-tools package installed
    # * Microvm: 1vCPU with 1024 MB RAM
    "disk_keyword":                      "bionic-msrtools",
    "microvm_keyword":                   "1vcpu_1024mb",
}
# fmt: on


def dump_msr_state_to_file(dump_fname, ssh_conn, shared_names):
    """
    Read MSR state via SSH and dump it into a file.
    """
    ssh_conn.scp_file(
        shared_names["msr_reader_host_fname"], shared_names["msr_reader_guest_fname"]
    )
    _, stdout, stderr = ssh_conn.execute_command(shared_names["msr_reader_guest_fname"])
    assert stderr.read() == ""

    with open(dump_fname, "w", encoding="UTF-8") as file:
        file.write(stdout.read())


@pytest.mark.skipif(
    UNSUPPORTED_HOST_KERNEL,
    reason=f"Supported kernels are {SUPPORTED_HOST_KERNELS}",
)
@pytest.mark.timeout(900)
@pytest.mark.nonci
def test_cpu_wrmsr_snapshot(
    microvm_factory, guest_kernel, rootfs_msrtools, msr_cpu_template
):
    """
    This is the first part of the test verifying
    that MSRs retain their values after restoring from a snapshot.

    This function makes MSR value modifications according to the
    ../resources/tests/msr/wrmsr_list.txt file.

    Before taking a snapshot, MSR values are dumped into a text file.
    After restoring from the snapshot on another instance, the MSRs are
    dumped again and their values are compared to previous.
    Some MSRs are not inherently supposed to retain their values, so they
    form an MSR exception list.

    This part of the test is responsible for taking a snapshot and publishing
    its files along with the `before` MSR dump.

    @type: functional
    """
    shared_names = SNAPSHOT_RESTORE_SHARED_NAMES

    vcpus, guest_mem_mib = 1, 1024
    vm = microvm_factory.build(guest_kernel, rootfs_msrtools, monitor_memory=False)
    vm.spawn()
    vm.add_net_iface(NetIfaceConfig())
    vm.basic_config(
        vcpu_count=vcpus,
        mem_size_mib=guest_mem_mib,
        cpu_template=msr_cpu_template,
        track_dirty_pages=True,
    )
    vm.start()

    # Make MSR modifications
    msr_writer_host_fname = "../resources/tests/msr/msr_writer.sh"
    msr_writer_guest_fname = "/bin/msr_writer.sh"
    vm.ssh.scp_file(msr_writer_host_fname, msr_writer_guest_fname)

    wrmsr_input_host_fname = "../resources/tests/msr/wrmsr_list.txt"
    wrmsr_input_guest_fname = "/tmp/wrmsr_input.txt"
    vm.ssh.scp_file(wrmsr_input_host_fname, wrmsr_input_guest_fname)

    _, _, stderr = vm.ssh.execute_command(
        f"{msr_writer_guest_fname} {wrmsr_input_guest_fname}"
    )
    assert stderr.read() == ""

    # Dump MSR state to a file that will be published to S3 for the 2nd part of the test
    snapshot_artifacts_dir = (
        Path(shared_names["snapshot_artifacts_root_dir_wrmsr"])
        / guest_kernel.base_name()
        / (msr_cpu_template if msr_cpu_template else "none")
    )
    clean_and_mkdir(snapshot_artifacts_dir)

    msrs_before_fname = snapshot_artifacts_dir / shared_names["msrs_before_fname"]

    dump_msr_state_to_file(msrs_before_fname, vm.ssh, shared_names)
    # On T2A, the restore test fails with error "cannot allocate memory" so,
    # adding delay below as a workaround to unblock the tests for now.
    # TODO: Debug the issue and remove this delay. Create below issue to track this:
    # https://github.com/firecracker-microvm/firecracker/issues/3453
    time.sleep(0.25)

    # Take a snapshot
    vm.pause_to_snapshot(
        mem_file_path=shared_names["mem_fname"],
        snapshot_path=shared_names["snapshot_fname"],
        diff=True,
    )

    # Copy snapshot files to be published to S3 for the 2nd part of the test
    chroot_dir = Path(vm.chroot())
    shutil.copyfile(
        chroot_dir / shared_names["mem_fname"],
        snapshot_artifacts_dir / shared_names["mem_fname"],
    )
    shutil.copyfile(
        chroot_dir / shared_names["snapshot_fname"],
        snapshot_artifacts_dir / shared_names["snapshot_fname"],
    )
    shutil.copyfile(
        chroot_dir / shared_names["rootfs_fname"],
        snapshot_artifacts_dir / shared_names["rootfs_fname"],
    )


def diff_msrs(before, after, column_to_drop):
    """
    Calculates and formats a diff between two MSR tables.
    """
    # Drop irrelevant column
    before_stripped = before.drop(column_to_drop, axis=1)
    after_stripped = after.drop(column_to_drop, axis=1)

    # Check that values in remaining columns are the same
    all_equal = (before_stripped == after_stripped).all(axis=None)

    # Arrange the diff as a side by side comparison of statuses
    not_equal = (before_stripped != after_stripped).any(axis=1)
    before_stripped.columns = ["MSR_ADDR", "Before"]
    after_stripped.columns = ["MSR_ADDR", "After"]
    diff = pd.merge(
        before_stripped[not_equal],
        after_stripped[not_equal],
        on=["MSR_ADDR", "MSR_ADDR"],
    ).to_string()

    # Return the diff or an empty string
    return diff if not all_equal else ""


def check_msr_values_are_equal(before_msrs_fname, after_msrs_fname):
    """
    Checks that MSR statuses and values in the files are equal.
    """
    before = pd.read_csv(before_msrs_fname)
    after = pd.read_csv(after_msrs_fname)

    flt_before = before[~before["MSR_ADDR"].isin(MSR_EXCEPTION_LIST)]
    flt_after = after[~after["MSR_ADDR"].isin(MSR_EXCEPTION_LIST)]

    # Consider only values of MSRs which are present both before and after
    flt = (flt_before["STATUS"] == "implemented") & (
        flt_after["STATUS"] == "implemented"
    )
    impl_before = flt_before.loc[flt]
    impl_after = flt_after.loc[flt]

    status_diff = diff_msrs(before, after, column_to_drop="VALUE")
    value_diff = diff_msrs(impl_before, impl_after, column_to_drop="STATUS")
    assert not status_diff
    assert not value_diff


@pytest.mark.skipif(
    UNSUPPORTED_HOST_KERNEL,
    reason=f"Supported kernels are {SUPPORTED_HOST_KERNELS}",
)
@pytest.mark.timeout(900)
@pytest.mark.nonci
def test_cpu_wrmsr_restore(
    microvm_factory, msr_cpu_template, guest_kernel, rootfs_msrtools
):
    """
    This is the second part of the test verifying
    that MSRs retain their values after restoring from a snapshot.

    Before taking a snapshot, MSR values are dumped into a text file.
    After restoring from the snapshot on another instance, the MSRs are
    dumped again and their values are compared to previous.
    Some MSRs are not inherently supposed to retain their values, so they
    form an MSR exception list.

    This part of the test is responsible for restoring from a snapshot and
    comparing two sets of MSR values.

    @type: functional
    """

    shared_names = SNAPSHOT_RESTORE_SHARED_NAMES
    cpu_template_dir = msr_cpu_template if msr_cpu_template else "none"
    snapshot_artifacts_dir = (
        Path(shared_names["snapshot_artifacts_root_dir_wrmsr"])
        / guest_kernel.base_name()
        / cpu_template_dir
    )

    skip_test_based_on_artifacts(snapshot_artifacts_dir)

    vm = microvm_factory.build()
    vm.spawn()
    # recreate eth0
    iface = NetIfaceConfig()
    vm.create_tap_and_ssh_config(
        host_ip=iface.host_ip,
        guest_ip=iface.guest_ip,
        netmask_len=iface.netmask,
        tapname=iface.tap_name,
    )
    # would be better to also capture the SSH key in the snapshot
    ssh_key = rootfs_msrtools.ssh_key().local_path()
    vm.ssh_config["ssh_key_path"] = ssh_key

    mem = snapshot_artifacts_dir / shared_names["mem_fname"]
    vmstate = snapshot_artifacts_dir / shared_names["snapshot_fname"]
    rootfs = snapshot_artifacts_dir / shared_names["rootfs_fname"]
    # Restore from the snapshot
    vm.restore_from_snapshot(
        snapshot_mem=mem,
        snapshot_vmstate=vmstate,
        snapshot_disks=[rootfs],
        snapshot_is_diff=True,
    )

    # Dump MSR state to a file for further comparison
    msrs_after_fname = snapshot_artifacts_dir / shared_names["msrs_after_fname"]
    dump_msr_state_to_file(msrs_after_fname, vm.ssh, shared_names)

    # Compare the two lists of MSR values and assert they are equal
    check_msr_values_are_equal(
        Path(snapshot_artifacts_dir) / shared_names["msrs_before_fname"],
        Path(snapshot_artifacts_dir) / shared_names["msrs_after_fname"],
    )


def dump_cpuid_to_file(dump_fname, ssh_conn):
    """
    Read CPUID via SSH and dump it into a file.
    """
    _, stdout, stderr = ssh_conn.execute_command("cpuid --one-cpu")
    assert stderr.read() == ""

    with open(dump_fname, "w", encoding="UTF-8") as file:
        file.write(stdout.read())


@pytest.mark.skipif(
    UNSUPPORTED_HOST_KERNEL,
    reason=f"Supported kernels are {SUPPORTED_HOST_KERNELS}",
)
@pytest.mark.timeout(900)
@pytest.mark.nonci
def test_cpu_cpuid_snapshot(
    microvm_factory, guest_kernel, rootfs_msrtools, msr_cpu_template
):
    """
    This is the first part of the test verifying
    that CPUID remains the same after restoring from a snapshot.

    Before taking a snapshot, CPUID is dumped into a text file.
    After restoring from the snapshot on another instance, the CPUID is
    dumped again and its content is compared to previous.

    This part of the test is responsible for taking a snapshot and publishing
    its files along with the `before` CPUID dump.

    @type: functional
    """
    shared_names = SNAPSHOT_RESTORE_SHARED_NAMES

    vm = microvm_factory.build(
        kernel=guest_kernel,
        rootfs=rootfs_msrtools,
    )
    vm.spawn()
    vm.add_net_iface(NetIfaceConfig())
    vm.basic_config(
        vcpu_count=1,
        mem_size_mib=1024,
        cpu_template=msr_cpu_template,
        track_dirty_pages=True,
    )
    vm.start()

    # Dump CPUID to a file that will be published to S3 for the 2nd part of the test
    cpu_template_dir = get_cpu_template_dir(msr_cpu_template)
    snapshot_artifacts_dir = (
        Path(shared_names["snapshot_artifacts_root_dir_cpuid"])
        / guest_kernel.base_name()
        / cpu_template_dir
    )
    clean_and_mkdir(snapshot_artifacts_dir)

    cpuid_before_fname = (
        Path(snapshot_artifacts_dir) / shared_names["cpuid_before_fname"]
    )

    dump_cpuid_to_file(cpuid_before_fname, vm.ssh)

    # Take a snapshot
    vm.pause_to_snapshot(
        mem_file_path=shared_names["mem_fname"],
        snapshot_path=shared_names["snapshot_fname"],
        diff=True,
    )

    # Copy snapshot files to be published to S3 for the 2nd part of the test
    chroot_dir = Path(vm.chroot())
    shutil.copyfile(
        chroot_dir / shared_names["mem_fname"],
        snapshot_artifacts_dir / shared_names["mem_fname"],
    )
    shutil.copyfile(
        chroot_dir / shared_names["snapshot_fname"],
        snapshot_artifacts_dir / shared_names["snapshot_fname"],
    )
    shutil.copyfile(
        chroot_dir / shared_names["rootfs_fname"],
        snapshot_artifacts_dir / Path(rootfs_msrtools.local_path()).name,
    )


def check_cpuid_is_equal(before_cpuid_fname, after_cpuid_fname, guest_kernel_name):
    """
    Checks that CPUID dumps in the files are equal.
    """
    with open(before_cpuid_fname, "r", encoding="UTF-8") as file:
        before = file.readlines()
    with open(after_cpuid_fname, "r", encoding="UTF-8") as file:
        after = file.readlines()

    diff = sys.stdout.writelines(unified_diff(before, after))

    assert not diff, f"\n{guest_kernel_name}:\n\n{diff}"


@pytest.mark.skipif(
    UNSUPPORTED_HOST_KERNEL,
    reason=f"Supported kernels are {SUPPORTED_HOST_KERNELS}",
)
@pytest.mark.timeout(900)
@pytest.mark.nonci
def test_cpu_cpuid_restore(
    microvm_factory, msr_cpu_template, guest_kernel, rootfs_msrtools
):
    """
    This is the second part of the test verifying
    that CPUID remains the same after restoring from a snapshot.

    Before taking a snapshot, CPUID is dumped into a text file.
    After restoring from the snapshot on another instance, the CPUID is
    dumped again and compared to previous.

    This part of the test is responsible for restoring from a snapshot and
    comparing two CPUIDs.

    @type: functional
    """

    shared_names = SNAPSHOT_RESTORE_SHARED_NAMES
    cpu_template_dir = get_cpu_template_dir(msr_cpu_template)
    snapshot_artifacts_dir = (
        Path(shared_names["snapshot_artifacts_root_dir_cpuid"])
        / guest_kernel.base_name()
        / cpu_template_dir
    )

    skip_test_based_on_artifacts(snapshot_artifacts_dir)

    vm = microvm_factory.build()
    vm.spawn()
    # recreate eth0
    iface = NetIfaceConfig()
    vm.create_tap_and_ssh_config(
        host_ip=iface.host_ip,
        guest_ip=iface.guest_ip,
        netmask_len=iface.netmask,
        tapname=iface.tap_name,
    )
    ssh_arti = rootfs_msrtools.ssh_key()
    vm.ssh_config["ssh_key_path"] = ssh_arti.local_path()

    # Restore from the snapshot
    mem = snapshot_artifacts_dir / shared_names["mem_fname"]
    vmstate = snapshot_artifacts_dir / shared_names["snapshot_fname"]
    rootfs = snapshot_artifacts_dir / shared_names["rootfs_fname"]
    vm.restore_from_snapshot(
        snapshot_mem=mem,
        snapshot_vmstate=vmstate,
        snapshot_disks=[rootfs],
        snapshot_is_diff=True,
    )

    # Dump CPUID to a file for further comparison
    cpuid_after_fname = Path(snapshot_artifacts_dir) / shared_names["cpuid_after_fname"]
    dump_cpuid_to_file(cpuid_after_fname, vm.ssh)

    # Compare the two lists of MSR values and assert they are equal
    check_cpuid_is_equal(
        Path(snapshot_artifacts_dir) / shared_names["cpuid_before_fname"],
        Path(snapshot_artifacts_dir) / shared_names["cpuid_after_fname"],
        guest_kernel.base_name(),  # this is to annotate the assertion output
    )


@pytest.mark.skipif(
    PLATFORM != "x86_64", reason="CPU features are masked only on x86_64."
)
@pytest.mark.parametrize("cpu_template", ["T2", "T2S", "C3"])
def test_cpu_template(test_microvm_with_api, network_config, cpu_template):
    """
    Test masked and enabled cpu features against the expected template.

    This test checks that all expected masked features are not present in the
    guest and that expected enabled features are present for each of the
    supported CPU templates.

    @type: functional
    """
    test_microvm = test_microvm_with_api
    test_microvm.spawn()
    # Set template as specified in the `cpu_template` parameter.
    test_microvm.basic_config(
        vcpu_count=1,
        mem_size_mib=256,
        cpu_template=cpu_template,
    )
    _tap, _, _ = test_microvm.ssh_network_config(network_config, "1")

    response = test_microvm.actions.put(action_type="InstanceStart")
    if cpuid_utils.get_cpu_vendor() != cpuid_utils.CpuVendor.INTEL:
        # We shouldn't be able to apply Intel templates on AMD hosts
        assert test_microvm.api_session.is_status_bad_request(response.status_code)
        return

    assert test_microvm.api_session.is_status_no_content(response.status_code)
    check_masked_features(test_microvm, cpu_template)
    check_enabled_features(test_microvm, cpu_template)


def check_masked_features(test_microvm, cpu_template):
    """Verify the masked features of the given template."""
    # fmt: off
    if cpu_template == "C3":
        must_be_unset = [
            (0x1, 0x0, "ecx",
                (1 << 2) |  # DTES64
                (1 << 3) |  # MONITOR
                (1 << 4) |  # DS_CPL_SHIFT
                (1 << 5) |  # VMX
                (1 << 8) |  # TM2
                (1 << 10) | # CNXT_ID
                (1 << 11) | # SDBG
                (1 << 12) | # FMA
                (1 << 14) | # XTPR_UPDATE
                (1 << 15) | # PDCM
                (1 << 22)   # MOVBE
            ),
            (0x1, 0x0, "edx",
                (1 << 18) | # PSN
                (1 << 20) | # DS
                (1 << 22) | # ACPI
                (1 << 27) | # SS
                (1 << 29) | # TM
                (1 << 31)   # PBE
            ),
            (0x7, 0x0, "ebx",
                (1 << 2) |  # SGX
                (1 << 3) |  # BMI1
                (1 << 4) |  # HLE
                (1 << 5) |  # AVX2
                (1 << 8) |  # BMI2
                (1 << 10) | # INVPCID
                (1 << 11) | # RTM
                (1 << 12) | # RDT_M
                (1 << 14) | # MPX
                (1 << 15) | # RDT_A
                (1 << 16) | # AVX512F
                (1 << 17) | # AVX512DQ
                (1 << 18) | # RDSEED
                (1 << 19) | # ADX
                (1 << 21) | # AVX512IFMA
                (1 << 23) | # CLFLUSHOPT
                (1 << 24) | # CLWB
                (1 << 25) | # PT
                (1 << 26) | # AVX512PF
                (1 << 27) | # AVX512ER
                (1 << 28) | # AVX512CD
                (1 << 29) | # SHA
                (1 << 30) | # AVX512BW
                (1 << 31)   # AVX512VL
            ),
            (0x7, 0x0, "ecx",
                (1 << 1) |  # AVX512_VBMI
                (1 << 2) |  # UMIP
                (1 << 3) |  # PKU
                (1 << 4) |  # OSPKE
                (1 << 11) | # AVX512_VNNI
                (1 << 14) | # AVX512_VPOPCNTDQ
                (1 << 16) | # LA57
                (1 << 22) | # RDPID
                (1 << 30)   # SGX_LC
            ),
            (0x7, 0x0, "edx",
                (1 << 2) |  # AVX512_4VNNIW
                (1 << 3)    # AVX512_4FMAPS
            ),
            (0xd, 0x0, "eax",
                (1 << 3) |  # MPX_STATE bit 0
                (1 << 4) |  # MPX_STATE bit 1
                (1 << 5) |  # AVX512_STATE bit 0
                (1 << 6) |  # AVX512_STATE bit 1
                (1 << 7) |  # AVX512_STATE bit 2
                (1 << 9)    # PKRU
            ),
            (0xd, 0x1, "eax",
                (1 << 1) |  # XSAVEC_SHIFT
                (1 << 2) |  # XGETBV_SHIFT
                (1 << 3)    # XSAVES_SHIFT
            ),
            (0x80000001, 0x0, "ecx",
                (1 << 5) |  # LZCNT
                (1 << 8)    # PREFETCH
            ),
            (0x80000001, 0x0, "edx",
                (1 << 26)   # PDPE1GB
            ),
        ]
    elif cpu_template in ("T2", "T2S"):
        must_be_unset = [
            (0x1, 0x0, "ecx",
                (1 << 2) |  # DTES64
                (1 << 3) |  # MONITOR
                (1 << 4) |  # DS_CPL_SHIFT
                (1 << 5) |  # VMX
                (1 << 6) |  # SMX
                (1 << 7) |  # EIST
                (1 << 8) |  # TM2
                (1 << 10) | # CNXT_ID
                (1 << 11) | # SDBG
                (1 << 14) | # XTPR_UPDATE
                (1 << 15) | # PDCM
                (1 << 18)   # DCA
            ),
            (0x1, 0x0, "edx",
                (1 << 18) | # PSN
                (1 << 20) | # DS
                (1 << 22) | # ACPI
                (1 << 27) | # SS
                (1 << 29) | # TM
                (1 << 30) | # IA64
                (1 << 31)   # PBE
            ),
            (0x7, 0x0, "ebx",
                (1 << 2) |  # SGX
                (1 << 4) |  # HLE
                (1 << 11) | # RTM
                (1 << 12) | # RDT_M
                (1 << 14) | # MPX
                (1 << 15) | # RDT_A
                (1 << 16) | # AVX512F
                (1 << 17) | # AVX512DQ
                (1 << 18) | # RDSEED
                (1 << 19) | # ADX
                (1 << 21) | # AVX512IFMA
                (1 << 22) | # PCOMMIT
                (1 << 23) | # CLFLUSHOPT
                (1 << 24) | # CLWB
                (1 << 25) | # PT
                (1 << 26) | # AVX512PF
                (1 << 27) | # AVX512ER
                (1 << 28) | # AVX512CD
                (1 << 29) | # SHA
                (1 << 30) | # AVX512BW
                (1 << 31)   # AVX512VL
            ),
            (0x7, 0x0, "ecx",
                (1 << 1) |  # AVX512_VBMI
                (1 << 2) |  # UMIP
                (1 << 3) |  # PKU
                (1 << 4) |  # OSPKE
                (1 << 6) |  # AVX512_VBMI2
                (1 << 8) |  # GFNI
                (1 << 9) |  # VAES
                (1 << 10) | # VPCLMULQDQ
                (1 << 11) | # AVX512_VNNI
                (1 << 12) | # AVX512_BITALG
                (1 << 14) | # AVX512_VPOPCNTDQ
                (1 << 16) | # LA57
                (1 << 22) | # RDPID
                (1 << 30)   # SGX_LC
            ),
            (0x7, 0x0, "edx",
                (1 << 2) |  # AVX512_4VNNIW
                (1 << 3) |  # AVX512_4FMAPS
                (1 << 4) |  # FSRM
                (1 << 8)    # AVX512_VP2INTERSECT
            ),
            (0xd, 0x0, "eax",
                (1 << 3) |  # MPX_STATE bit 0
                (1 << 4) |  # MPX_STATE bit 1
                (1 << 5) |  # AVX512_STATE bit 0
                (1 << 6) |  # AVX512_STATE bit 1
                (1 << 7) |  # AVX512_STATE bit 2
                (1 << 9)    # PKRU
            ),
            (0xd, 0x1, "eax",
                (1 << 1) |  # XSAVEC_SHIFT
                (1 << 2) |  # XGETBV_SHIFT
                (1 << 3)    # XSAVES_SHIFT
            ),
            (0x80000001, 0x0, "ecx",
                (1 << 8) |  # PREFETCH
                (1 << 29)   # MWAIT_EXTENDED
            ),
            (0x80000001, 0x0, "edx",
                (1 << 26)   # PDPE1GB
            ),
            (0x80000008, 0x0, "ebx",
                (1 << 9)    # WBNOINVD
            )
        ]
    # fmt: on

    cpuid_utils.check_cpuid_feat_flags(
        test_microvm,
        [],
        must_be_unset,
    )


def check_enabled_features(test_microvm, cpu_template):
    """Test for checking that all expected features are enabled in guest."""
    enabled_list = {  # feature_info_1_edx
        "x87 FPU on chip": "true",
        "CMPXCHG8B inst": "true",
        "virtual-8086 mode enhancement": "true",
        "SSE extensions": "true",
        "SSE2 extensions": "true",
        "debugging extensions": "true",
        "page size extensions": "true",
        "time stamp counter": "true",
        "RDMSR and WRMSR support": "true",
        "physical address extensions": "true",
        "machine check exception": "true",
        "APIC on chip": "true",
        "MMX Technology": "true",
        "SYSENTER and SYSEXIT": "true",
        "memory type range registers": "true",
        "PTE global bit": "true",
        "FXSAVE/FXRSTOR": "true",
        "machine check architecture": "true",
        "conditional move/compare instruction": "true",
        "page attribute table": "true",
        "page size extension": "true",
        "CLFLUSH instruction": "true",
        # feature_info_1_ecx
        "PNI/SSE3: Prescott New Instructions": "true",
        "PCLMULDQ instruction": "true",
        "SSSE3 extensions": "true",
        "AES instruction": "true",
        "CMPXCHG16B instruction": "true",
        "process context identifiers": "true",
        "SSE4.1 extensions": "true",
        "SSE4.2 extensions": "true",
        "extended xAPIC support": "true",
        "POPCNT instruction": "true",
        "time stamp counter deadline": "true",
        "XSAVE/XSTOR states": "true",
        "OS-enabled XSAVE/XSTOR": "true",
        "AVX: advanced vector extensions": "true",
        "F16C half-precision convert instruction": "true",
        "RDRAND instruction": "true",
        "hypervisor guest status": "true",
        # thermal_and_power_mgmt
        "ARAT always running APIC timer": "true",
        # extended_features
        "FSGSBASE instructions": "true",
        "IA32_TSC_ADJUST MSR supported": "true",
        "SMEP supervisor mode exec protection": "true",
        "enhanced REP MOVSB/STOSB": "true",
        "SMAP: supervisor mode access prevention": "true",
        # xsave_0xd_0
        "XCR0 supported: x87 state": "true",
        "XCR0 supported: SSE state": "true",
        "XCR0 supported: AVX state": "true",
        # xsave_0xd_1
        "XSAVEOPT instruction": "true",
        # extended_080000001_edx
        "SYSCALL and SYSRET instructions": "true",
        "64-bit extensions technology available": "true",
        "execution disable": "true",
        "RDTSCP": "true",
        # intel_080000001_ecx
        "LAHF/SAHF supported in 64-bit mode": "true",
        # adv_pwr_mgmt
        "TscInvariant": "true",
    }

    cpuid_utils.check_guest_cpuid_output(
        test_microvm, "cpuid -1", None, "=", enabled_list
    )
    if cpu_template == "T2":
        t2_enabled_features = {
            "FMA": "true",
            "BMI": "true",
            "BMI2": "true",
            "AVX2": "true",
            "MOVBE": "true",
            "INVPCID": "true",
        }
        cpuid_utils.check_guest_cpuid_output(
            test_microvm, "cpuid -1", None, "=", t2_enabled_features
        )
