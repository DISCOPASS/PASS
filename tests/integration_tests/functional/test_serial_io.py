# Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Tests scenario for the Firecracker serial console."""

import fcntl
import os
import platform
import subprocess
import termios
import time

import host_tools.logging as log_tools
from framework import utils
from framework.artifacts import SnapshotType
from framework.builder import MicrovmBuilder, SnapshotBuilder
from framework.microvm import Serial
from framework.state_machine import TestState

PLATFORM = platform.machine()


class WaitTerminal(TestState):  # pylint: disable=too-few-public-methods
    """Initial state when we wait for the login prompt."""

    def handle_input(self, serial, input_char) -> TestState:
        """Handle input and return next state."""
        if self.match(input_char):
            serial.tx("id")
            return WaitIDResult("uid=0(root) gid=0(root) groups=0(root)")
        return self


class WaitIDResult(TestState):  # pylint: disable=too-few-public-methods
    """Wait for the console to show the result of the 'id' shell command."""

    def handle_input(self, unused_serial, input_char) -> TestState:
        """Handle input and return next state."""
        if self.match(input_char):
            return TestFinished()
        return self


class TestFinished(TestState):  # pylint: disable=too-few-public-methods
    """Test complete and successful."""

    def handle_input(self, unused_serial, _) -> TestState:
        """Return self since the test is about to end."""
        return self


def test_serial_after_snapshot(bin_cloner_path):
    """
    Serial I/O after restoring from a snapshot.

    @type: functional
    """
    vm_builder = MicrovmBuilder(bin_cloner_path)
    vm_instance = vm_builder.build_vm_nano(
        daemonize=False,
    )
    microvm = vm_instance.vm
    root_disk = vm_instance.disks[0]
    ssh_key = vm_instance.ssh_key

    serial = Serial(microvm)
    serial.open()
    microvm.start()

    # Image used for tests on aarch64 has autologon
    if PLATFORM == "x86_64":
        serial.rx(token="login: ")
        serial.tx("root")
        serial.rx("Password: ")
        serial.tx("root")
    # Make sure that at the time we snapshot the vm, the user is logged in.
    serial.rx("#")

    snapshot_builder = SnapshotBuilder(microvm)

    # Create snapshot.
    snapshot = snapshot_builder.create(
        [root_disk.local_path()], ssh_key, SnapshotType.FULL
    )
    # Kill base microVM.
    microvm.kill()

    # Load microVM clone from snapshot.
    test_microvm, _ = vm_builder.build_from_snapshot(
        snapshot, resume=True, daemonize=False
    )
    serial = Serial(test_microvm)
    serial.open()
    # We need to send a newline to signal the serial to flush
    # the login content.
    serial.tx("")
    serial.rx("#")
    serial.tx("pwd")
    res = serial.rx("#")
    assert "/root" in res


def test_serial_console_login(test_microvm_with_api):
    """
    Test serial console login.

    @type: functional
    """
    microvm = test_microvm_with_api
    microvm.jailer.daemonize = False
    microvm.spawn()

    # We don't need to monitor the memory for this test because we are
    # just rebooting and the process dies before pmap gets the RSS.
    microvm.memory_monitor = None

    # Set up the microVM with 1 vCPU and a serial console.
    microvm.basic_config(
        vcpu_count=1, boot_args="console=ttyS0 reboot=k panic=1 pci=off"
    )

    microvm.start()

    serial = Serial(microvm)
    serial.open()
    current_state = WaitTerminal("ubuntu-fc-uvm:")

    while not isinstance(current_state, TestFinished):
        output_char = serial.rx_char()
        current_state = current_state.handle_input(serial, output_char)


def get_total_mem_size(pid):
    """Get total memory usage for a process."""
    cmd = f"pmap {pid} | tail -n 1 | sed 's/^ //' | tr -s ' ' | cut -d' ' -f2"
    rc, stdout, stderr = utils.run_cmd(cmd)
    assert rc == 0
    assert stderr == ""

    return stdout


def send_bytes(tty, bytes_count, timeout=60):
    """Send data to the terminal."""
    start = time.time()
    for _ in range(bytes_count):
        fcntl.ioctl(tty, termios.TIOCSTI, "\n")
        current = time.time()
        if current - start > timeout:
            break


def test_serial_dos(test_microvm_with_api):
    """
    Test serial console behavior under DoS.

    @type: functional
    """
    microvm = test_microvm_with_api
    microvm.jailer.daemonize = False
    microvm.spawn()
    microvm.memory_events_queue = None

    # Set up the microVM with 1 vCPU and a serial console.
    microvm.basic_config(
        vcpu_count=1,
        add_root_device=False,
        boot_args="console=ttyS0 reboot=k panic=1 pci=off",
    )
    microvm.start()

    # Open an fd for firecracker process terminal.
    tty_path = f"/proc/{microvm.jailer_clone_pid}/fd/0"
    tty_fd = os.open(tty_path, os.O_RDWR)

    # Check if the total memory size changed.
    before_size = get_total_mem_size(microvm.jailer_clone_pid)
    send_bytes(tty_fd, 100000000, timeout=1)
    after_size = get_total_mem_size(microvm.jailer_clone_pid)
    assert before_size == after_size, (
        "The memory size of the "
        "Firecracker process "
        "changed from {} to {}.".format(before_size, after_size)
    )


def test_serial_block(test_microvm_with_api, network_config):
    """
    Test that writing to stdout never blocks the vCPU thread.

    @type: functional
    """
    test_microvm = test_microvm_with_api
    test_microvm.jailer.daemonize = False
    test_microvm.spawn()
    # Set up the microVM with 1 vCPU so we make sure the vCPU thread
    # responsible for the SSH connection will also run the serial.
    test_microvm.basic_config(
        vcpu_count=1,
        mem_size_mib=512,
        boot_args="console=ttyS0 reboot=k panic=1 pci=off",
    )

    _tap, _, _ = test_microvm.ssh_network_config(network_config, "1")

    # Configure the metrics.
    metrics_fifo_path = os.path.join(test_microvm.path, "metrics_fifo")
    metrics_fifo = log_tools.Fifo(metrics_fifo_path)
    response = test_microvm.metrics.put(
        metrics_path=test_microvm.create_jailed_resource(metrics_fifo.path)
    )
    assert test_microvm.api_session.is_status_no_content(response.status_code)

    test_microvm.start()

    # Get an initial reading of missed writes to the serial.
    fc_metrics = test_microvm.flush_metrics(metrics_fifo)
    init_count = fc_metrics["uart"]["missed_write_count"]

    screen_pid = test_microvm.screen_pid
    # Stop `screen` process which captures stdout so we stop consuming stdout.
    subprocess.check_call("kill -s STOP {}".format(screen_pid), shell=True)

    # Generate a random text file.
    exit_code, _, _ = test_microvm.ssh.execute_command(
        "base64 /dev/urandom | head -c 100000 > file.txt"
    )

    # Dump output to terminal
    exit_code, _, _ = test_microvm.ssh.execute_command("cat file.txt > /dev/ttyS0")
    assert exit_code == 0

    # Check that the vCPU isn't blocked.
    exit_code, _, _ = test_microvm.ssh.execute_command("cd /")
    assert exit_code == 0

    # Check the metrics to see if the serial missed bytes.
    fc_metrics = test_microvm.flush_metrics(metrics_fifo)
    last_count = fc_metrics["uart"]["missed_write_count"]

    # Should be significantly more than before the `cat` command.
    assert last_count - init_count > 10000
