# Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Tests that ensure the correctness of the Firecracker API."""

# Disable pylint C0302: Too many lines in module
# pylint: disable=C0302
import os
import platform
import resource
import time

import packaging.version
import pytest

import host_tools.drive as drive_tools
import host_tools.network as net_tools
from framework import utils_cpuid
from framework.artifacts import NetIfaceConfig, SnapshotType
from framework.builder import MicrovmBuilder, SnapshotBuilder
from framework.utils import get_firecracker_version_from_toml, is_io_uring_supported

MEM_LIMIT = 1000000000


def test_api_happy_start(test_microvm_with_api):
    """
    Test that a regular microvm API config and boot sequence works.

    @type: functional
    """
    test_microvm = test_microvm_with_api
    test_microvm.spawn()

    # Set up the microVM with 2 vCPUs, 256 MiB of RAM and
    # a root file system with the rw permission.
    test_microvm.basic_config()

    test_microvm.start()


def test_drive_io_engine(test_microvm_with_api, network_config):
    """
    Test io_engine configuration.

    Test that the io_engine can be configured via the API on kernels that
    support the given type and that FC returns an error otherwise.

    @type: functional
    """
    test_microvm = test_microvm_with_api
    test_microvm.spawn()

    test_microvm.basic_config(add_root_device=False)
    test_microvm.ssh_network_config(network_config, "1")

    supports_io_uring = is_io_uring_supported()

    response = test_microvm.drive.put(
        drive_id="rootfs",
        path_on_host=test_microvm.create_jailed_resource(test_microvm.rootfs_file),
        is_root_device=True,
        is_read_only=False,
        # Set the opposite of the default backend type.
        io_engine="Sync" if supports_io_uring else "Async",
    )

    if not supports_io_uring:
        # The Async engine is not supported for older kernels.
        assert test_microvm.api_session.is_status_bad_request(response.status_code)
        test_microvm.check_log_message(
            "Received Error. Status code: 400 Bad Request. Message: Unable"
            " to create the block device: FileEngine(UnsupportedEngine(Async))"
        )

        # Now configure the default engine type and check that it works.
        response = test_microvm.drive.put_with_default_io_engine(
            drive_id="rootfs",
            path_on_host=test_microvm.create_jailed_resource(test_microvm.rootfs_file),
            is_root_device=True,
            is_read_only=False,
        )

    assert test_microvm.api_session.is_status_no_content(response.status_code)

    test_microvm.start()

    # Execute a simple command to check that the guest booted successfully.
    rc, _, stderr = test_microvm.ssh.execute_command("sync")
    assert rc == 0
    assert stderr.read() == ""

    assert test_microvm.full_cfg.get().json()["drives"][0]["io_engine"] == "Sync"


def test_api_put_update_pre_boot(test_microvm_with_api):
    """
    Test that PUT updates are allowed before the microvm boots.

    Tests updates on drives, boot source and machine config.

    @type: functional
    """
    test_microvm = test_microvm_with_api
    test_microvm.spawn()

    # Set up the microVM with 2 vCPUs, 256 MiB of RAM  and
    # a root file system with the rw permission.
    test_microvm.basic_config()

    fs1 = drive_tools.FilesystemFile(os.path.join(test_microvm.fsfiles, "scratch"))
    response = test_microvm.drive.put(
        drive_id="scratch",
        path_on_host=test_microvm.create_jailed_resource(fs1.path),
        is_root_device=False,
        is_read_only=False,
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Updates to `kernel_image_path` with an invalid path are not allowed.
    response = test_microvm.boot.put(kernel_image_path="foo.bar")
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert (
        "The kernel file cannot be opened: No such file or directory "
        "(os error 2)" in response.text
    )

    # Updates to `kernel_image_path` with a valid path are allowed.
    response = test_microvm.boot.put(
        kernel_image_path=test_microvm.get_jailed_resource(test_microvm.kernel_file)
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Updates to `path_on_host` with an invalid path are not allowed.
    response = test_microvm.drive.put(
        drive_id="rootfs",
        path_on_host="foo.bar",
        is_read_only=True,
        is_root_device=True,
    )
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert "Invalid block device path" in response.text

    # Updates to `is_root_device` that result in two root block devices are not
    # allowed.
    response = test_microvm.drive.put(
        drive_id="scratch",
        path_on_host=test_microvm.get_jailed_resource(fs1.path),
        is_read_only=False,
        is_root_device=True,
    )
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert "A root block device already exists" in response.text

    # Valid updates to `path_on_host` and `is_read_only` are allowed.
    fs2 = drive_tools.FilesystemFile(os.path.join(test_microvm.fsfiles, "otherscratch"))
    response = test_microvm.drive.put(
        drive_id="scratch",
        path_on_host=test_microvm.create_jailed_resource(fs2.path),
        is_read_only=True,
        is_root_device=False,
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Valid updates to all fields in the machine configuration are allowed.
    # The machine configuration has a default value, so all PUTs are updates.
    microvm_config_json = {
        "vcpu_count": 4,
        "smt": platform.machine() == "x86_64",
        "mem_size_mib": 256,
        "track_dirty_pages": True,
    }
    if platform.machine() == "x86_64":
        microvm_config_json["cpu_template"] = "C3"

    if platform.machine() == "aarch64":
        response = test_microvm.machine_cfg.put(
            vcpu_count=microvm_config_json["vcpu_count"],
            smt=microvm_config_json["smt"],
            mem_size_mib=microvm_config_json["mem_size_mib"],
            track_dirty_pages=microvm_config_json["track_dirty_pages"],
        )
    else:
        response = test_microvm.machine_cfg.put(
            vcpu_count=microvm_config_json["vcpu_count"],
            smt=microvm_config_json["smt"],
            mem_size_mib=microvm_config_json["mem_size_mib"],
            cpu_template=microvm_config_json["cpu_template"],
            track_dirty_pages=microvm_config_json["track_dirty_pages"],
        )

    assert test_microvm.api_session.is_status_no_content(response.status_code)

    response = test_microvm.machine_cfg.get()
    assert test_microvm.api_session.is_status_ok(response.status_code)
    response_json = response.json()

    vcpu_count = microvm_config_json["vcpu_count"]
    assert response_json["vcpu_count"] == vcpu_count

    smt = microvm_config_json["smt"]
    assert response_json["smt"] == smt

    mem_size_mib = microvm_config_json["mem_size_mib"]
    assert response_json["mem_size_mib"] == mem_size_mib

    if platform.machine() == "x86_64":
        cpu_template = str(microvm_config_json["cpu_template"])
        assert response_json["cpu_template"] == cpu_template

    track_dirty_pages = microvm_config_json["track_dirty_pages"]
    assert response_json["track_dirty_pages"] == track_dirty_pages


def test_net_api_put_update_pre_boot(test_microvm_with_api):
    """
    Test PUT updates on network configurations before the microvm boots.

    @type: functional
    """
    test_microvm = test_microvm_with_api
    test_microvm.spawn()

    first_if_name = "first_tap"
    tap1 = net_tools.Tap(first_if_name, test_microvm.jailer.netns)
    response = test_microvm.network.put(
        iface_id="1", guest_mac="06:00:00:00:00:01", host_dev_name=tap1.name
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Adding new network interfaces is allowed.
    second_if_name = "second_tap"
    tap2 = net_tools.Tap(second_if_name, test_microvm.jailer.netns)
    response = test_microvm.network.put(
        iface_id="2", guest_mac="07:00:00:00:00:01", host_dev_name=tap2.name
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Updates to a network interface with an unavailable MAC are not allowed.
    guest_mac = "06:00:00:00:00:01"
    response = test_microvm.network.put(
        iface_id="2", host_dev_name=second_if_name, guest_mac=guest_mac
    )
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert f"The MAC address is already in use: {guest_mac}" in response.text

    # Updates to a network interface with an available MAC are allowed.
    response = test_microvm.network.put(
        iface_id="2", host_dev_name=second_if_name, guest_mac="08:00:00:00:00:01"
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Updates to a network interface with an unavailable name are not allowed.
    response = test_microvm.network.put(
        iface_id="1", host_dev_name=second_if_name, guest_mac="06:00:00:00:00:01"
    )
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert "Could not create the network device" in response.text

    # Updates to a network interface with an available name are allowed.
    iface_id = "1"
    tapname = test_microvm.id[:8] + "tap" + iface_id

    tap3 = net_tools.Tap(tapname, test_microvm.jailer.netns)
    response = test_microvm.network.put(
        iface_id=iface_id, host_dev_name=tap3.name, guest_mac="06:00:00:00:00:01"
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)


def test_api_mmds_config(test_microvm_with_api):
    """
    Test /mmds/config PUT scenarios that unit tests can't cover.

    Tests updates on MMDS config before and after attaching a network device.

    @type: negative
    """
    test_microvm = test_microvm_with_api
    test_microvm.spawn()

    # Set up the microVM with 2 vCPUs, 256 MiB of RAM  and
    # a root file system with the rw permission.
    test_microvm.basic_config()

    # Setting MMDS config with empty network interface IDs list is not allowed.
    response = test_microvm.mmds.put_config(json={"network_interfaces": []})
    err_msg = (
        "The list of network interface IDs that allow "
        "forwarding MMDS requests is empty."
    )
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert err_msg in response.text

    # Setting MMDS config when no network device has been attached
    # is not allowed.
    response = test_microvm.mmds.put_config(json={"network_interfaces": ["foo"]})
    err_msg = (
        "The list of network interface IDs provided contains "
        "at least one ID that does not correspond to any "
        "existing network interface."
    )
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert err_msg in response.text

    # Attach network interface.
    tap = net_tools.Tap("tap1", test_microvm.jailer.netns)
    response = test_microvm.network.put(
        iface_id="1", guest_mac="06:00:00:00:00:01", host_dev_name=tap.name
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Setting MMDS config with an ID that does not correspond to an already
    # attached network device is not allowed.
    response = test_microvm.mmds.put_config(json={"network_interfaces": ["1", "foo"]})
    err_msg = (
        "The list of network interface IDs provided contains"
        " at least one ID that does not correspond to any "
        "existing network interface."
    )
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert err_msg in response.text

    # Updates to MMDS version with invalid value are not allowed.
    response = test_microvm.mmds.put_config(
        json={"version": "foo", "network_interfaces": ["1"]}
    )
    err_msg = (
        "An error occurred when deserializing the json body of a "
        "request: unknown variant `foo`, expected `V1` or `V2`"
    )
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert err_msg in response.text

    # Valid MMDS config not specifying version or IPv4 address.
    response = test_microvm.mmds.put_config(json={"network_interfaces": ["1"]})
    assert test_microvm.api_session.is_status_no_content(response.status_code)
    assert test_microvm.full_cfg.get().json()["mmds-config"]["version"] == "V1"

    # Valid MMDS config not specifying version.
    mmds_config = {"ipv4_address": "169.254.169.250", "network_interfaces": ["1"]}
    response = test_microvm.mmds.put_config(json=mmds_config)
    assert test_microvm.api_session.is_status_no_content(response.status_code)
    assert (
        test_microvm.full_cfg.get().json()["mmds-config"]["ipv4_address"]
        == "169.254.169.250"
    )

    # Valid MMDS config.
    mmds_config = {
        "version": "V2",
        "ipv4_address": "169.254.169.250",
        "network_interfaces": ["1"],
    }
    response = test_microvm.mmds.put_config(json=mmds_config)
    assert test_microvm.api_session.is_status_no_content(response.status_code)
    assert test_microvm.full_cfg.get().json()["mmds-config"]["version"] == "V2"


# pylint: disable=too-many-statements
def test_api_machine_config(test_microvm_with_api):
    """
    Test /machine_config PUT/PATCH scenarios that unit tests can't cover.

    @type: functional
    """
    test_microvm = test_microvm_with_api
    test_microvm.spawn()

    # Test invalid vcpu count < 0.
    response = test_microvm.machine_cfg.put(vcpu_count="-2")
    assert test_microvm.api_session.is_status_bad_request(response.status_code)

    # Test invalid type for smt flag.
    response = test_microvm.machine_cfg.put(smt="random_string")
    assert test_microvm.api_session.is_status_bad_request(response.status_code)

    # Test invalid CPU template.
    response = test_microvm.machine_cfg.put(cpu_template="random_string")
    assert test_microvm.api_session.is_status_bad_request(response.status_code)

    response = test_microvm.machine_cfg.patch(track_dirty_pages=True)
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Test missing vcpu_count.
    response = test_microvm.machine_cfg.put(mem_size_mib=128)
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert "missing field `vcpu_count` at line 1 column 21." in response.text

    # Test missing mem_size_mib.
    response = test_microvm.machine_cfg.put(vcpu_count=2)
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert "missing field `mem_size_mib` at line 1 column 17." in response.text

    # Test default smt value.
    response = test_microvm.machine_cfg.put(mem_size_mib=128, vcpu_count=1)
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    response = test_microvm.machine_cfg.get()
    assert test_microvm.api_session.is_status_ok(response.status_code)
    assert response.json()["smt"] is False

    # Test that smt=True errors on ARM.
    response = test_microvm.machine_cfg.patch(smt=True)
    if platform.machine() == "x86_64":
        assert test_microvm.api_session.is_status_no_content(response.status_code)
    else:
        assert test_microvm.api_session.is_status_bad_request(response.status_code)
        assert (
            "Enabling simultaneous multithreading is not supported on aarch64"
            in response.text
        )

    # Test invalid mem_size_mib < 0.
    response = test_microvm.machine_cfg.put(mem_size_mib="-2")
    assert test_microvm.api_session.is_status_bad_request(response.status_code)

    # Test invalid mem_size_mib > usize::MAX.
    bad_size = 1 << 64
    response = test_microvm.machine_cfg.put(mem_size_mib=bad_size)
    fail_msg = (
        "error occurred when deserializing the json body of a " "request: invalid type"
    )
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert fail_msg in response.text

    # Reset the configuration of the microvm
    # This will explicitly set vcpu_num = 2, mem_size_mib = 256
    # track_dirty_pages = false. All other parameters are
    # unspecified so will revert to default values.
    test_microvm.basic_config()

    # Test mem_size_mib of valid type, but too large.
    firecracker_pid = int(test_microvm.jailer_clone_pid)
    resource.prlimit(
        firecracker_pid, resource.RLIMIT_AS, (MEM_LIMIT, resource.RLIM_INFINITY)
    )

    bad_size = (1 << 64) - 1
    response = test_microvm.machine_cfg.patch(mem_size_mib=bad_size)
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    response = test_microvm.actions.put(action_type="InstanceStart")
    fail_msg = (
        "Invalid Memory Configuration: MmapRegion(Mmap(Os { code: "
        "12, kind: OutOfMemory, message: Out of memory }))"
    )
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert fail_msg in response.text

    # Test invalid mem_size_mib = 0.
    response = test_microvm.machine_cfg.patch(mem_size_mib=0)
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert "The memory size (MiB) is invalid." in response.text

    # Test valid mem_size_mib.
    response = test_microvm.machine_cfg.patch(mem_size_mib=256)

    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Set the cpu template
    if platform.machine() == "x86_64":
        response = test_microvm.machine_cfg.patch(cpu_template="C3")
        assert test_microvm.api_session.is_status_no_content(response.status_code)
    else:
        # We test with "None" because this is the only option supported on
        # all aarch64 instances. It still tests setting `cpu_template`,
        # even though the values we set is "None".
        response = test_microvm.machine_cfg.patch(cpu_template="None")
        assert test_microvm.api_session.is_status_no_content(response.status_code)

    response = test_microvm.actions.put(action_type="InstanceStart")
    if utils_cpuid.get_cpu_vendor() == utils_cpuid.CpuVendor.AMD:
        # We shouldn't be able to apply Intel templates on AMD hosts
        fail_msg = "CPU vendor mismatched between actual CPU and CPU template"
        assert test_microvm.api_session.is_status_bad_request(response.status_code)
        assert fail_msg in response.text
    else:
        assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Validate full vm configuration after patching machine config.
    response = test_microvm.full_cfg.get()
    assert test_microvm.api_session.is_status_ok(response.status_code)
    json = response.json()
    assert json["machine-config"]["vcpu_count"] == 2
    assert json["machine-config"]["mem_size_mib"] == 256
    assert json["machine-config"]["smt"] is False


def test_api_cpu_config(test_microvm_with_api, custom_cpu_template):
    """
    Test /cpu-config PUT scenarios.

    @type: functional
    """
    test_microvm = test_microvm_with_api
    test_microvm.spawn()

    response = test_microvm.cpu_cfg.put("{}")
    assert test_microvm.api_session.is_status_bad_request(response.status_code)

    response = test_microvm.cpu_cfg.put(custom_cpu_template["template"])
    assert test_microvm.api_session.is_status_no_content(response.status_code)


def test_api_put_update_post_boot(test_microvm_with_api):
    """
    Test that PUT updates are rejected after the microvm boots.

    @type: negative
    """
    test_microvm = test_microvm_with_api
    test_microvm.spawn()

    # Set up the microVM with 2 vCPUs, 256 MiB of RAM  and
    # a root file system with the rw permission.
    test_microvm.basic_config()

    iface_id = "1"
    tapname = test_microvm.id[:8] + "tap" + iface_id
    tap1 = net_tools.Tap(tapname, test_microvm.jailer.netns)
    response = test_microvm.network.put(
        iface_id=iface_id, host_dev_name=tap1.name, guest_mac="06:00:00:00:00:01"
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    test_microvm.start()

    expected_err = (
        "The requested operation is not supported " "after starting the microVM"
    )

    # Valid updates to `kernel_image_path` are not allowed after boot.
    response = test_microvm.boot.put(
        kernel_image_path=test_microvm.get_jailed_resource(test_microvm.kernel_file)
    )
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert expected_err in response.text

    # Valid updates to the machine configuration are not allowed after boot.
    response = test_microvm.machine_cfg.patch(vcpu_count=4)
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert expected_err in response.text

    response = test_microvm.machine_cfg.put(vcpu_count=4, mem_size_mib=128)
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert expected_err in response.text

    # Network interface update is not allowed after boot.
    response = test_microvm.network.put(
        iface_id="1", host_dev_name=tap1.name, guest_mac="06:00:00:00:00:02"
    )
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert expected_err in response.text

    # Block device update is not allowed after boot.
    response = test_microvm.drive.put(
        drive_id="rootfs",
        path_on_host=test_microvm.jailer.jailed_path(test_microvm.rootfs_file),
        is_read_only=False,
        is_root_device=True,
    )
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert expected_err in response.text

    # MMDS config is not allowed post-boot.
    mmds_config = {
        "version": "V2",
        "ipv4_address": "169.254.169.250",
        "network_interfaces": ["1"],
    }
    response = test_microvm.mmds.put_config(json=mmds_config)
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert expected_err in response.text


def test_rate_limiters_api_config(test_microvm_with_api):
    """
    Test the IO rate limiter API config.

    @type: functional
    """
    test_microvm = test_microvm_with_api
    test_microvm.spawn()

    # Test the DRIVE rate limiting API.

    # Test drive with bw rate-limiting.
    fs1 = drive_tools.FilesystemFile(os.path.join(test_microvm.fsfiles, "bw"))
    response = test_microvm.drive.put(
        drive_id="bw",
        path_on_host=test_microvm.create_jailed_resource(fs1.path),
        is_read_only=False,
        is_root_device=False,
        rate_limiter={"bandwidth": {"size": 1000000, "refill_time": 100}},
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Test drive with ops rate-limiting.
    fs2 = drive_tools.FilesystemFile(os.path.join(test_microvm.fsfiles, "ops"))
    response = test_microvm.drive.put(
        drive_id="ops",
        path_on_host=test_microvm.create_jailed_resource(fs2.path),
        is_read_only=False,
        is_root_device=False,
        rate_limiter={"ops": {"size": 1, "refill_time": 100}},
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Test drive with bw and ops rate-limiting.
    fs3 = drive_tools.FilesystemFile(os.path.join(test_microvm.fsfiles, "bwops"))
    response = test_microvm.drive.put(
        drive_id="bwops",
        path_on_host=test_microvm.create_jailed_resource(fs3.path),
        is_read_only=False,
        is_root_device=False,
        rate_limiter={
            "bandwidth": {"size": 1000000, "refill_time": 100},
            "ops": {"size": 1, "refill_time": 100},
        },
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Test drive with 'empty' rate-limiting (same as not specifying the field)
    fs4 = drive_tools.FilesystemFile(os.path.join(test_microvm.fsfiles, "nada"))
    response = test_microvm.drive.put(
        drive_id="nada",
        path_on_host=test_microvm.create_jailed_resource(fs4.path),
        is_read_only=False,
        is_root_device=False,
        rate_limiter={},
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Test the NET rate limiting API.

    # Test network with tx bw rate-limiting.
    iface_id = "1"
    tapname = test_microvm.id[:8] + "tap" + iface_id
    tap1 = net_tools.Tap(tapname, test_microvm.jailer.netns)

    response = test_microvm.network.put(
        iface_id=iface_id,
        guest_mac="06:00:00:00:00:01",
        host_dev_name=tap1.name,
        tx_rate_limiter={"bandwidth": {"size": 1000000, "refill_time": 100}},
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Test network with rx bw rate-limiting.
    iface_id = "2"
    tapname = test_microvm.id[:8] + "tap" + iface_id
    tap2 = net_tools.Tap(tapname, test_microvm.jailer.netns)
    response = test_microvm.network.put(
        iface_id=iface_id,
        guest_mac="06:00:00:00:00:02",
        host_dev_name=tap2.name,
        rx_rate_limiter={"bandwidth": {"size": 1000000, "refill_time": 100}},
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Test network with tx and rx bw and ops rate-limiting.
    iface_id = "3"
    tapname = test_microvm.id[:8] + "tap" + iface_id
    tap3 = net_tools.Tap(tapname, test_microvm.jailer.netns)
    response = test_microvm.network.put(
        iface_id=iface_id,
        guest_mac="06:00:00:00:00:03",
        host_dev_name=tap3.name,
        rx_rate_limiter={
            "bandwidth": {"size": 1000000, "refill_time": 100},
            "ops": {"size": 1, "refill_time": 100},
        },
        tx_rate_limiter={
            "bandwidth": {"size": 1000000, "refill_time": 100},
            "ops": {"size": 1, "refill_time": 100},
        },
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Test entropy device bw and ops rate-limiting.
    response = test_microvm.entropy.put(
        rate_limiter={
            "bandwidth": {"size": 1000000, "refill_time": 100},
            "ops": {"size": 1, "refill_time": 100},
        },
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)


def test_api_patch_pre_boot(test_microvm_with_api):
    """
    Test that PATCH updates are not allowed before the microvm boots.

    @type: negative
    """
    test_microvm = test_microvm_with_api
    test_microvm.spawn()

    # Sets up the microVM with 2 vCPUs, 256 MiB of RAM, 1 network interface
    # and a root file system with the rw permission.
    test_microvm.basic_config()

    fs1 = drive_tools.FilesystemFile(os.path.join(test_microvm.fsfiles, "scratch"))
    drive_id = "scratch"
    response = test_microvm.drive.put(
        drive_id=drive_id,
        path_on_host=test_microvm.create_jailed_resource(fs1.path),
        is_root_device=False,
        is_read_only=False,
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    iface_id = "1"
    tapname = test_microvm.id[:8] + "tap" + iface_id
    tap1 = net_tools.Tap(tapname, test_microvm.jailer.netns)
    response = test_microvm.network.put(
        iface_id=iface_id, host_dev_name=tap1.name, guest_mac="06:00:00:00:00:01"
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Partial updates to the boot source are not allowed.
    response = test_microvm.boot.patch(kernel_image_path="otherfile")
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert "Invalid request method" in response.text

    # Partial updates to the machine configuration are allowed before boot.
    response = test_microvm.machine_cfg.patch(vcpu_count=4)
    assert test_microvm.api_session.is_status_no_content(response.status_code)
    response_json = test_microvm.machine_cfg.get().json()
    assert response_json["vcpu_count"] == 4

    # Partial updates to the logger configuration are not allowed.
    response = test_microvm.logger.patch(level="Error")
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert "Invalid request method" in response.text

    # Patching drive before boot is not allowed.
    response = test_microvm.drive.patch(drive_id=drive_id, path_on_host="foo.bar")
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert (
        "The requested operation is not supported before starting the "
        "microVM." in response.text
    )

    # Patching net before boot is not allowed.
    response = test_microvm.network.patch(iface_id=iface_id)
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert (
        "The requested operation is not supported before starting the "
        "microVM." in response.text
    )


def test_negative_api_patch_post_boot(test_microvm_with_api):
    """
    Test PATCH updates that are not allowed after the microvm boots.

    @type: negative
    """
    test_microvm = test_microvm_with_api
    test_microvm.spawn()

    # Sets up the microVM with 2 vCPUs, 256 MiB of RAM, 1 network iface and
    # a root file system with the rw permission.
    test_microvm.basic_config()

    fs1 = drive_tools.FilesystemFile(os.path.join(test_microvm.fsfiles, "scratch"))
    response = test_microvm.drive.put(
        drive_id="scratch",
        path_on_host=test_microvm.create_jailed_resource(fs1.path),
        is_root_device=False,
        is_read_only=False,
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    iface_id = "1"
    tapname = test_microvm.id[:8] + "tap" + iface_id
    tap1 = net_tools.Tap(tapname, test_microvm.jailer.netns)
    response = test_microvm.network.put(
        iface_id=iface_id, host_dev_name=tap1.name, guest_mac="06:00:00:00:00:01"
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    test_microvm.start()

    # Partial updates to the boot source are not allowed.
    response = test_microvm.boot.patch(kernel_image_path="otherfile")
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert "Invalid request method" in response.text

    # Partial updates to the machine configuration are not allowed after boot.
    expected_err = (
        "The requested operation is not supported " "after starting the microVM"
    )
    response = test_microvm.machine_cfg.patch(vcpu_count=4)
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert expected_err in response.text

    # Partial updates to the logger configuration are not allowed.
    response = test_microvm.logger.patch(level="Error")
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert "Invalid request method" in response.text


def test_drive_patch(test_microvm_with_api):
    """
    Extensively test drive PATCH scenarios before and after boot.

    @type: functional
    """
    test_microvm = test_microvm_with_api
    test_microvm.spawn()

    # Sets up the microVM with 2 vCPUs, 256 MiB of RAM and
    # a root file system with the rw permission.
    test_microvm.basic_config(rootfs_io_engine="Sync")

    # The drive to be patched.
    fs = drive_tools.FilesystemFile(os.path.join(test_microvm.fsfiles, "scratch"))
    response = test_microvm.drive.put(
        drive_id="scratch",
        path_on_host=test_microvm.create_jailed_resource(fs.path),
        is_root_device=False,
        is_read_only=False,
        io_engine="Async" if is_io_uring_supported() else "Sync",
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Patching drive before boot is not allowed.
    response = test_microvm.drive.patch(drive_id="scratch", path_on_host="foo.bar")
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert (
        "The requested operation is not supported before starting the "
        "microVM." in response.text
    )

    test_microvm.start()

    _drive_patch(test_microvm)


@pytest.mark.skipif(
    platform.machine() != "x86_64", reason="not yet implemented on aarch64"
)
def test_send_ctrl_alt_del(test_microvm_with_api):
    """
    Test shutting down the microVM gracefully on x86, by sending CTRL+ALT+DEL.

    @type: functional
    """
    # This relies on the i8042 device and AT Keyboard support being present in
    # the guest kernel.
    test_microvm = test_microvm_with_api
    test_microvm.spawn()

    test_microvm.basic_config()
    test_microvm.start()

    # Wait around for the guest to boot up and initialize the user space
    time.sleep(2)

    response = test_microvm.actions.put(action_type="SendCtrlAltDel")
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    firecracker_pid = test_microvm.jailer_clone_pid

    # If everyting goes as expected, the guest OS will issue a reboot,
    # causing Firecracker to exit.
    # We'll keep poking Firecracker for at most 30 seconds, waiting for it
    # to die.
    start_time = time.time()
    shutdown_ok = False
    while time.time() - start_time < 30:
        try:
            os.kill(firecracker_pid, 0)
            time.sleep(0.01)
        except OSError:
            shutdown_ok = True
            break

    assert shutdown_ok


def _drive_patch(test_microvm):
    """Exercise drive patch test scenarios."""
    # Patches without mandatory fields are not allowed.
    response = test_microvm.drive.patch(drive_id="scratch")
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert "at least one property to patch: path_on_host, rate_limiter" in response.text

    drive_path = "foo.bar"

    # Cannot patch drive permissions post boot.
    response = test_microvm.drive.patch(
        drive_id="scratch", path_on_host=drive_path, is_read_only=True
    )
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert "unknown field `is_read_only`" in response.text

    # Cannot patch io_engine post boot.
    response = test_microvm.drive.patch(
        drive_id="scratch", path_on_host=drive_path, io_engine="Sync"
    )
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert "unknown field `io_engine`" in response.text

    # Updates to `is_root_device` with a valid value are not allowed.
    response = test_microvm.drive.patch(
        drive_id="scratch", path_on_host=drive_path, is_root_device=False
    )
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert "unknown field `is_root_device`" in response.text

    # Updates to `path_on_host` with an invalid path are not allowed.
    response = test_microvm.drive.patch(drive_id="scratch", path_on_host=drive_path)
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert (
        "Unable to patch the block device: BackingFile(Os { code: 2, "
        f'kind: NotFound, message: \\"No such file or directory\\" }}, \\"{drive_path}\\")'
        in response.text
    )

    fs = drive_tools.FilesystemFile(os.path.join(test_microvm.fsfiles, "scratch_new"))
    # Updates to `path_on_host` with a valid path are allowed.
    response = test_microvm.drive.patch(
        drive_id="scratch", path_on_host=test_microvm.create_jailed_resource(fs.path)
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Updates to valid `path_on_host` and `rate_limiter` are allowed.
    response = test_microvm.drive.patch(
        drive_id="scratch",
        path_on_host=test_microvm.create_jailed_resource(fs.path),
        rate_limiter={
            "bandwidth": {"size": 1000000, "refill_time": 100},
            "ops": {"size": 1, "refill_time": 100},
        },
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Updates to `rate_limiter` only are allowed.
    response = test_microvm.drive.patch(
        drive_id="scratch",
        rate_limiter={
            "bandwidth": {"size": 5000, "refill_time": 100},
            "ops": {"size": 500, "refill_time": 100},
        },
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Updates to `rate_limiter` and invalid path fail.
    response = test_microvm.drive.patch(
        drive_id="scratch",
        path_on_host="foo.bar",
        rate_limiter={
            "bandwidth": {"size": 5000, "refill_time": 100},
            "ops": {"size": 500, "refill_time": 100},
        },
    )
    assert test_microvm.api_session.is_status_bad_request(response.status_code)
    assert "No such file or directory" in response.text

    # Validate full vm configuration after patching drives.
    response = test_microvm.full_cfg.get()
    assert test_microvm.api_session.is_status_ok(response.status_code)
    assert response.json()["drives"] == [
        {
            "drive_id": "rootfs",
            "path_on_host": "/bionic.rootfs.ext4",
            "is_root_device": True,
            "partuuid": None,
            "is_read_only": False,
            "cache_type": "Unsafe",
            "io_engine": "Sync",
            "rate_limiter": None,
        },
        {
            "drive_id": "scratch",
            "path_on_host": "/scratch_new.ext4",
            "is_root_device": False,
            "partuuid": None,
            "is_read_only": False,
            "cache_type": "Unsafe",
            "io_engine": "Async" if is_io_uring_supported() else "Sync",
            "rate_limiter": {
                "bandwidth": {"size": 5000, "one_time_burst": None, "refill_time": 100},
                "ops": {"size": 500, "one_time_burst": None, "refill_time": 100},
            },
        },
    ]


def test_api_version(test_microvm_with_api):
    """
    Test the permanent VM version endpoint.

    @type: functional
    """
    test_microvm = test_microvm_with_api
    test_microvm.spawn()
    test_microvm.basic_config()

    # Getting the VM version should be available pre-boot.
    preboot_response = test_microvm.version.get_from_api()
    assert test_microvm.api_session.is_status_ok(preboot_response.status_code)
    # Check that the response contains the version.
    assert "firecracker_version" in preboot_response.json()

    # Start the microvm.
    test_microvm.start()

    # Getting the VM version should be available post-boot.
    postboot_response = test_microvm.version.get_from_api()
    assert test_microvm.api_session.is_status_ok(postboot_response.status_code)
    # Check that the response contains the version.
    assert "firecracker_version" in postboot_response.json()
    # Validate VM version post-boot is the same as pre-boot.
    assert preboot_response.json() == postboot_response.json()

    cargo_version = get_firecracker_version_from_toml()
    api_version = packaging.version.parse(
        preboot_response.json()["firecracker_version"]
    )

    # Cargo version should match FC API version
    assert cargo_version == api_version


def test_api_vsock(bin_cloner_path):
    """
    Test vsock related API commands.

    @type: functional
    """
    builder = MicrovmBuilder(bin_cloner_path)
    # Test with the current build.
    vm_instance = builder.build_vm_nano()
    _test_vsock(vm_instance.vm)


def _test_vsock(vm):
    # Create a vsock device.
    response = vm.vsock.put(guest_cid=15, uds_path="vsock.sock")
    assert vm.api_session.is_status_no_content(response.status_code)

    # Updating an existing vsock is currently fine.
    response = vm.vsock.put(guest_cid=166, uds_path="vsock.sock")
    assert vm.api_session.is_status_no_content(response.status_code)

    # Check PUT request. Although vsock_id is deprecated, it must still work.
    response = vm.vsock.put(vsock_id="vsock1", guest_cid=15, uds_path="vsock.sock")
    assert vm.api_session.is_status_no_content(response.status_code)
    assert response.headers["deprecation"]

    # Updating an existing vsock is currently fine even with deprecated
    # `vsock_id`.
    response = vm.vsock.put(vsock_id="vsock1", guest_cid=166, uds_path="vsock.sock")
    assert vm.api_session.is_status_no_content(response.status_code)
    assert response.headers["deprecation"]

    # No other vsock action is allowed after booting the VM.
    vm.start()

    # Updating an existing vsock should not be fine at this point.
    response = vm.vsock.put(guest_cid=17, uds_path="vsock.sock")
    assert vm.api_session.is_status_bad_request(response.status_code)


def test_api_entropy(test_microvm_with_api):
    """
    Test entropy related API commands.

    @type: functional
    """
    test_microvm = test_microvm_with_api
    test_microvm.spawn()
    test_microvm.basic_config()

    # Create a new entropy device should be OK.
    response = test_microvm.entropy.put()
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Overwriting an existing should be OK.
    response = test_microvm.entropy.put()
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Start the microvm
    test_microvm.start()

    response = test_microvm.entropy.put()
    assert test_microvm.api_session.is_status_bad_request(response.status_code)


def test_api_balloon(test_microvm_with_api):
    """
    Test balloon related API commands.

    @type: functional
    """
    test_microvm = test_microvm_with_api
    test_microvm.spawn()
    test_microvm.basic_config()

    # Updating an inexistent balloon device should give an error.
    response = test_microvm.balloon.patch(amount_mib=0)
    assert test_microvm.api_session.is_status_bad_request(response.status_code)

    # Adding a memory balloon should be OK.
    response = test_microvm.balloon.put(amount_mib=1, deflate_on_oom=True)
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # As is overwriting one.
    response = test_microvm.balloon.put(
        amount_mib=0, deflate_on_oom=False, stats_polling_interval_s=5
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Getting the device configuration should be available pre-boot.
    response = test_microvm.balloon.get()
    assert test_microvm.api_session.is_status_ok(response.status_code)
    assert response.json()["amount_mib"] == 0
    assert response.json()["deflate_on_oom"] is False
    assert response.json()["stats_polling_interval_s"] == 5

    # Updating an existing balloon device is forbidden before boot.
    response = test_microvm.balloon.patch(amount_mib=2)
    assert test_microvm.api_session.is_status_bad_request(response.status_code)

    # We can't have a balloon device with a target size greater than
    # the available amount of memory.
    response = test_microvm.balloon.put(
        amount_mib=1024, deflate_on_oom=False, stats_polling_interval_s=5
    )
    assert test_microvm.api_session.is_status_bad_request(response.status_code)

    # Start the microvm.
    test_microvm.start()

    # Updating should fail as driver didn't have time to initialize.
    response = test_microvm.balloon.patch(amount_mib=4)
    assert test_microvm.api_session.is_status_bad_request(response.status_code)

    # Overwriting the existing device should give an error now.
    response = test_microvm.balloon.put(
        amount_mib=3, deflate_on_oom=False, stats_polling_interval_s=3
    )
    assert test_microvm.api_session.is_status_bad_request(response.status_code)

    # Give the balloon driver time to initialize.
    # 500 ms is the maximum acceptable boot time.
    time.sleep(0.5)

    # But updating should be OK.
    response = test_microvm.balloon.patch(amount_mib=4)
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Check we can't request more than the total amount of VM memory.
    response = test_microvm.balloon.patch(amount_mib=300)
    assert test_microvm.api_session.is_status_bad_request(response.status_code)

    # Check we can't disable statistics as they were enabled at boot.
    # We can, however, change the interval to a non-zero value.
    response = test_microvm.balloon.patch_stats(stats_polling_interval_s=5)
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Getting the device configuration should be available post-boot.
    response = test_microvm.balloon.get()
    assert test_microvm.api_session.is_status_ok(response.status_code)
    assert response.json()["amount_mib"] == 4
    assert response.json()["deflate_on_oom"] is False
    assert response.json()["stats_polling_interval_s"] == 5

    # Check we can't overflow the `num_pages` field in the config space by
    # requesting too many MB. There are 256 4K pages in a MB. Here, we are
    # requesting u32::MAX / 128.
    response = test_microvm.balloon.patch(amount_mib=33554432)
    assert test_microvm.api_session.is_status_bad_request(response.status_code)


def test_get_full_config_after_restoring_snapshot(bin_cloner_path):
    """
    Test the configuration of a microVM after restoring from a snapshot.

    @type: functional
    """
    microvm_builder = MicrovmBuilder(bin_cloner_path)
    net_iface = NetIfaceConfig()
    vm_instance = microvm_builder.build_vm_nano(
        net_ifaces=[net_iface], io_engine="Sync"
    )
    test_microvm = vm_instance.vm
    root_disk = vm_instance.disks[0]
    ssh_key = vm_instance.ssh_key
    cpu_vendor = utils_cpuid.get_cpu_vendor()

    setup_cfg = {}

    # Basic config also implies a root block device.
    setup_cfg["machine-config"] = {
        "vcpu_count": 2,
        "mem_size_mib": 256,
        "smt": True,
        "track_dirty_pages": False,
    }

    if cpu_vendor == utils_cpuid.CpuVendor.ARM:
        setup_cfg["machine-config"]["smt"] = False

    if cpu_vendor == utils_cpuid.CpuVendor.INTEL:
        setup_cfg["machine-config"]["cpu_template"] = "C3"

    test_microvm.machine_cfg.patch(**setup_cfg["machine-config"])

    setup_cfg["cpu-config"] = None

    setup_cfg["drives"] = [
        {
            "drive_id": "rootfs",
            "path_on_host": f"/{os.path.basename(root_disk.local_path())}",
            "is_root_device": True,
            "partuuid": None,
            "is_read_only": False,
            "cache_type": "Unsafe",
            "rate_limiter": None,
            "io_engine": "Sync",
        }
    ]

    # Add a memory balloon device.
    response = test_microvm.balloon.put(amount_mib=1, deflate_on_oom=True)
    assert test_microvm.api_session.is_status_no_content(response.status_code)
    setup_cfg["balloon"] = {
        "amount_mib": 1,
        "deflate_on_oom": True,
        "stats_polling_interval_s": 0,
    }

    # Add a vsock device.
    response = test_microvm.vsock.put(guest_cid=15, uds_path="vsock.sock")
    assert test_microvm.api_session.is_status_no_content(response.status_code)
    setup_cfg["vsock"] = {"guest_cid": 15, "uds_path": "vsock.sock"}

    setup_cfg["logger"] = None
    setup_cfg["metrics"] = None
    setup_cfg["mmds-config"] = {
        "version": "V1",
        "network_interfaces": [net_iface.dev_name],
    }

    response = test_microvm.mmds.put_config(json=setup_cfg["mmds-config"])
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    # Start the microvm.
    test_microvm.start()

    # Add a tx rate limiter to the net device.
    tx_rl = {
        "bandwidth": {"size": 1000000, "refill_time": 100, "one_time_burst": None},
        "ops": None,
    }

    response = test_microvm.network.patch(
        iface_id=net_iface.dev_name, tx_rate_limiter=tx_rl
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)
    setup_cfg["network-interfaces"] = [
        {
            "guest_mac": net_tools.mac_from_ip(net_iface.guest_ip),
            "iface_id": net_iface.dev_name,
            "host_dev_name": net_iface.tap_name,
            "rx_rate_limiter": None,
            "tx_rate_limiter": tx_rl,
        }
    ]
    # Create a snapshot builder from a microvm.
    snapshot_builder = SnapshotBuilder(test_microvm)

    # Create base snapshot.
    snapshot = snapshot_builder.create(
        [root_disk.local_path()], ssh_key, SnapshotType.FULL
    )

    microvm, _ = microvm_builder.build_from_snapshot(
        snapshot, resume=True, diff_snapshots=False
    )

    expected_cfg = setup_cfg.copy()

    # We expect boot-source to be set with the following values
    expected_cfg["boot-source"] = {
        "kernel_image_path": test_microvm.get_jailed_resource(test_microvm.kernel_file),
        "initrd_path": None,
        "boot_args": "console=ttyS0 reboot=k panic=1",
    }

    # no ipv4 specified during PUT /mmds/config so we expect the default
    expected_cfg["mmds-config"] = {
        "version": "V1",
        "ipv4_address": "169.254.169.254",
        "network_interfaces": [net_iface.dev_name],
    }

    # We should expect a null entropy device
    expected_cfg["entropy"] = None

    # Validate full vm configuration post-restore.
    response = microvm.full_cfg.get()
    assert microvm.api_session.is_status_ok(response.status_code)
    assert response.json() != setup_cfg
    assert response.json() == expected_cfg


def test_get_full_config(test_microvm_with_api):
    """
    Test the reported configuration of a microVM configured with all resources.

    @type: functional
    """
    test_microvm = test_microvm_with_api

    expected_cfg = {}

    test_microvm.spawn()
    # Basic config also implies a root block device.
    test_microvm.basic_config(rootfs_io_engine="Sync")
    expected_cfg["machine-config"] = {
        "vcpu_count": 2,
        "mem_size_mib": 256,
        "smt": False,
        "track_dirty_pages": False,
    }
    expected_cfg["cpu-config"] = None
    expected_cfg["boot-source"] = {
        "kernel_image_path": "/vmlinux.bin",
        "initrd_path": None,
    }
    expected_cfg["drives"] = [
        {
            "drive_id": "rootfs",
            "path_on_host": "/bionic.rootfs.ext4",
            "is_root_device": True,
            "partuuid": None,
            "is_read_only": False,
            "cache_type": "Unsafe",
            "rate_limiter": None,
            "io_engine": "Sync",
        }
    ]

    # Add a memory balloon device.
    response = test_microvm.balloon.put(amount_mib=1, deflate_on_oom=True)
    assert test_microvm.api_session.is_status_no_content(response.status_code)
    expected_cfg["balloon"] = {
        "amount_mib": 1,
        "deflate_on_oom": True,
        "stats_polling_interval_s": 0,
    }

    # Add a vsock device.
    response = test_microvm.vsock.put(guest_cid=15, uds_path="vsock.sock")
    assert test_microvm.api_session.is_status_no_content(response.status_code)
    expected_cfg["vsock"] = {"guest_cid": 15, "uds_path": "vsock.sock"}

    # Add a net device.
    iface_id = "1"
    tapname = test_microvm.id[:8] + "tap" + iface_id
    tap1 = net_tools.Tap(tapname, test_microvm.jailer.netns)
    guest_mac = "06:00:00:00:00:01"
    tx_rl = {
        "bandwidth": {"size": 1000000, "refill_time": 100, "one_time_burst": None},
        "ops": None,
    }
    response = test_microvm.network.put(
        iface_id=iface_id,
        guest_mac=guest_mac,
        host_dev_name=tap1.name,
        tx_rate_limiter=tx_rl,
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)
    expected_cfg["network-interfaces"] = [
        {
            "iface_id": iface_id,
            "host_dev_name": tap1.name,
            "guest_mac": "06:00:00:00:00:01",
            "rx_rate_limiter": None,
            "tx_rate_limiter": tx_rl,
        }
    ]

    # Update MMDS config.
    mmds_config = {
        "version": "V2",
        "ipv4_address": "169.254.169.250",
        "network_interfaces": ["1"],
    }
    response = test_microvm.mmds.put_config(json=mmds_config)
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    expected_cfg["logger"] = None
    expected_cfg["metrics"] = None
    expected_cfg["mmds-config"] = {
        "version": "V2",
        "ipv4_address": "169.254.169.250",
        "network_interfaces": ["1"],
    }

    # We should expect a null entropy device
    expected_cfg["entropy"] = None

    # Getting full vm configuration should be available pre-boot.
    response = test_microvm.full_cfg.get()
    assert test_microvm.api_session.is_status_ok(response.status_code)
    assert response.json() == expected_cfg

    # Start the microvm.
    test_microvm.start()

    # Validate full vm configuration post-boot as well.
    response = test_microvm.full_cfg.get()
    assert test_microvm.api_session.is_status_ok(response.status_code)
    assert response.json() == expected_cfg


def test_map_private_seccomp_regression(test_microvm_with_api):
    """
    Seccomp mmap MAP_PRIVATE regression test.

    When sending large buffer to an api endpoint there will be an attempt to
    call mmap with MAP_PRIVATE|MAP_ANONYMOUS. This would result in vmm being
    killed by the seccomp filter before this PR.

    @type: regression
    """
    test_microvm = test_microvm_with_api
    test_microvm.jailer.extra_args.update(
        {"http-api-max-payload-size": str(1024 * 1024 * 2)}
    )
    test_microvm.spawn()
    test_microvm.api_session.untime()

    response = test_microvm.mmds.get()
    assert test_microvm.api_session.is_status_ok(response.status_code)
    assert response.json() == {}

    data_store = {"latest": {"meta-data": {"ami-id": "b" * (1024 * 1024)}}}

    response = test_microvm.mmds.put(json=data_store)
    assert test_microvm.api_session.is_status_no_content(response.status_code)


# pylint: disable=protected-access
def test_negative_snapshot_load_api(microvm_factory):
    """
    Test snapshot load API.

    @type: negative
    """
    vm = microvm_factory.build()
    vm.spawn()

    # Specifying both `mem_backend` and 'mem_file_path` should fail.
    datax = {
        "snapshot_path": "foo",
        "mem_backend": {"backend_type": "File", "backend_path": "bar"},
        "mem_file_path": "bar",
    }
    response = vm.snapshot._load._api_session.put(
        "{}".format(vm.snapshot._load._snapshot_cfg_url), json=datax
    )
    err_msg = (
        "too many fields: either `mem_backend` or "
        "`mem_file_path` exclusively is required."
    )
    assert err_msg in response.text, response.text

    # API request with `mem_backend` but no `backend_type` should fail.
    datax = {"snapshot_path": "foo", "mem_backend": {"backend_path": "bar"}}
    response = vm.snapshot._load._api_session.put(
        "{}".format(vm.snapshot._load._snapshot_cfg_url), json=datax
    )
    err_msg = "missing field `backend_type`"
    assert err_msg in response.text, response.text

    # API request with `mem_backend` but no `backend_path` should fail.
    datax = {"snapshot_path": "foo", "mem_backend": {"backend_type": "File"}}
    response = vm.snapshot._load._api_session.put(
        "{}".format(vm.snapshot._load._snapshot_cfg_url), json=datax
    )
    err_msg = "missing field `backend_path`"
    assert err_msg in response.text, response.text

    # API request with invalid `backend_type` should fail.
    datax = {
        "snapshot_path": "foo",
        "mem_backend": {"backend_type": "foo", "backend_path": "foo"},
    }
    response = vm.snapshot._load._api_session.put(
        "{}".format(vm.snapshot._load._snapshot_cfg_url), json=datax
    )
    err_msg = "unknown variant `foo`, expected `File` or `Uffd`"
    assert err_msg in response.text, response.text

    # API request without `snapshot_path` should fail.
    datax = {"mem_backend": {"backend_type": "File", "backend_path": "foo"}}
    response = vm.snapshot._load._api_session.put(
        "{}".format(vm.snapshot._load._snapshot_cfg_url), json=datax
    )
    err_msg = "missing field `snapshot_path`"
    assert err_msg in response.text, response.text

    # API request without `mem_backend` or `mem_file_path` should fail.
    datax = {"snapshot_path": "foo"}
    response = vm.snapshot._load._api_session.put(
        "{}".format(vm.snapshot._load._snapshot_cfg_url), json=datax
    )
    err_msg = "missing field: either `mem_backend` or " "`mem_file_path` is required"
    assert err_msg in response.text, response.text

    # Deprecated API should return deprecation response header.
    datax = {"snapshot_path": "foo", "mem_file_path": "bar"}
    response = vm.snapshot._load._api_session.put(
        "{}".format(vm.snapshot._load._snapshot_cfg_url), json=datax
    )
    assert response.headers["deprecation"]
