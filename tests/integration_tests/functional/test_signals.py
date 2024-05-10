# Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Tests scenarios for Firecracker signal handling."""

import json
import os
import resource as res
from signal import SIGBUS, SIGHUP, SIGILL, SIGPIPE, SIGSEGV, SIGSYS, SIGXCPU, SIGXFSZ
from time import sleep

import pytest

from framework import utils

signum_str = {
    SIGBUS: "sigbus",
    SIGSEGV: "sigsegv",
    SIGXFSZ: "sigxfsz",
    SIGXCPU: "sigxcpu",
    SIGPIPE: "sigpipe",
    SIGHUP: "sighup",
    SIGILL: "sigill",
    SIGSYS: "sigsys",
}


@pytest.mark.parametrize(
    "signum", [SIGBUS, SIGSEGV, SIGXFSZ, SIGXCPU, SIGPIPE, SIGHUP, SIGILL, SIGSYS]
)
def test_generic_signal_handler(test_microvm_with_api, signum):
    """
    Test signal handling for all handled signals.

    @type: functional
    """
    microvm = test_microvm_with_api
    microvm.spawn()

    # We don't need to monitor the memory for this test.
    microvm.memory_monitor = None

    microvm.basic_config()

    # Configure metrics based on a file.
    metrics_path = os.path.join(microvm.path, "metrics_fifo")
    utils.run_cmd("touch {}".format(metrics_path))
    response = microvm.metrics.put(
        metrics_path=microvm.create_jailed_resource(metrics_path)
    )
    assert microvm.api_session.is_status_no_content(response.status_code)

    microvm.start()
    firecracker_pid = int(microvm.jailer_clone_pid)
    sleep(0.5)

    metrics_jail_path = os.path.join(microvm.chroot(), metrics_path)
    metrics_fd = open(metrics_jail_path, encoding="utf-8")

    line_metrics = metrics_fd.readlines()
    assert len(line_metrics) == 1

    os.kill(firecracker_pid, signum)
    # Firecracker gracefully handles SIGPIPE (doesn't terminate).
    if signum == int(SIGPIPE):
        msg = "Received signal 13"
        # Flush metrics to file, so we can see the SIGPIPE at bottom assert.
        # This is going to fail if process has exited.
        response = microvm.actions.put(action_type="FlushMetrics")
        assert microvm.api_session.is_status_no_content(response.status_code)
    else:
        microvm.expect_kill_by_signal = True
        # Ensure that the process was terminated.
        utils.wait_process_termination(firecracker_pid)
        msg = "Shutting down VM after intercepting signal {}".format(signum)

    microvm.check_log_message(msg)

    if signum != SIGSYS:
        metric_line = json.loads(metrics_fd.readlines()[0])
        assert metric_line["signals"][signum_str[signum]] == 1


def test_sigxfsz_handler(test_microvm_with_api):
    """
    Test intercepting and handling SIGXFSZ.

    @type: functional
    """
    microvm = test_microvm_with_api
    microvm.spawn()

    # We don't need to monitor the memory for this test.
    microvm.memory_monitor = None

    # We need to use the Sync file engine type. If we use io_uring we will not
    # get a SIGXFSZ. We'll instead get an errno 27 File too large as the
    # completed entry status code.
    microvm.basic_config(rootfs_io_engine="Sync")

    # Configure metrics based on a file.
    metrics_path = os.path.join(microvm.path, "metrics_fifo")
    utils.run_cmd("touch {}".format(metrics_path))
    response = microvm.metrics.put(
        metrics_path=microvm.create_jailed_resource(metrics_path)
    )
    assert microvm.api_session.is_status_no_content(response.status_code)

    microvm.start()

    metrics_jail_path = os.path.join(microvm.jailer.chroot_path(), metrics_path)
    metrics_fd = open(metrics_jail_path, encoding="utf-8")
    line_metrics = metrics_fd.readlines()
    assert len(line_metrics) == 1

    firecracker_pid = int(microvm.jailer_clone_pid)
    size = os.path.getsize(metrics_jail_path)
    # The SIGXFSZ is triggered because the size of rootfs is bigger than
    # the size of metrics file times 3. Since the metrics file is flushed
    # twice we have to make sure that the limit is bigger than that
    # in order to make sure the SIGXFSZ metric is logged
    res.prlimit(firecracker_pid, res.RLIMIT_FSIZE, (size * 3, res.RLIM_INFINITY))

    while True:
        try:
            utils.run_cmd("ps -p {}".format(firecracker_pid))
            sleep(1)
        except ChildProcessError:
            break

    microvm.expect_kill_by_signal = True
    msg = "Shutting down VM after intercepting signal 25, code 0"
    microvm.check_log_message(msg)
    metric_line = json.loads(metrics_fd.readlines()[0])
    assert metric_line["signals"]["sigxfsz"] == 1


def test_handled_signals(test_microvm_with_api, network_config):
    """
    Test that handled signals don't kill the microVM.

    @type: functional
    """
    microvm = test_microvm_with_api
    microvm.spawn()

    # We don't need to monitor the memory for this test.
    microvm.memory_monitor = None

    microvm.basic_config(vcpu_count=2)

    # Configure a network interface.
    _tap, _, _ = microvm.ssh_network_config(network_config, "1")

    microvm.start()
    firecracker_pid = int(microvm.jailer_clone_pid)

    # Open a SSH connection to validate the microVM stays alive.
    # Just validate a simple command: `nproc`
    cmd = "nproc"
    _, stdout, stderr = microvm.ssh.execute_command(cmd)
    assert stderr.read() == ""
    assert int(stdout.read()) == 2

    # We have a handler installed for this signal.
    # The 35 is the SIGRTMIN for musl libc.
    # We hardcode this value since the SIGRTMIN python reports
    # is 34, which is likely the one for glibc.
    os.kill(firecracker_pid, 35)

    # Validate the microVM is still up and running.
    _, stdout, stderr = microvm.ssh.execute_command(cmd)
    assert stderr.read() == ""
    assert int(stdout.read()) == 2
