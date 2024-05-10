# Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Tests scenario for microvms with max vcpus(32)."""

MAX_VCPUS = 32


def test_max_vcpus(test_microvm_with_api, network_config):
    """
    Test if all configured guest vcpus are online.

    @type: functional
    """
    microvm = test_microvm_with_api
    microvm.spawn()

    # Configure a microVM with 32 vCPUs.
    microvm.basic_config(vcpu_count=MAX_VCPUS)
    _tap, _, _ = microvm.ssh_network_config(network_config, "1")

    microvm.start()

    cmd = "nproc"
    _, stdout, stderr = microvm.ssh.execute_command(cmd)
    assert stderr.read() == ""
    assert int(stdout.read()) == MAX_VCPUS
