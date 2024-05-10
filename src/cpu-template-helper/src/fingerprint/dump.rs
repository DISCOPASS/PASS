// Copyright 2023 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs::read_to_string;
use std::sync::{Arc, Mutex};

use vmm::Vmm;

use crate::fingerprint::Fingerprint;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Failed to dump CPU configuration.
    #[error("Failed to dump CPU config: {0}")]
    DumpCpuConfig(#[from] crate::template::dump::Error),
    /// Failed to read sysfs file.
    #[error("Failed to read {0}: {1}")]
    ReadSysfsFile(String, std::io::Error),
    /// Failed to get kernel version.
    #[error("Failed to get kernel version: {0}")]
    GetKernelVersion(std::io::Error),
    /// Shell command failed.
    #[error("`{0}` failed: {1}")]
    ShellCommand(String, String),
}

pub fn dump(vmm: Arc<Mutex<Vmm>>) -> Result<Fingerprint, Error> {
    Ok(Fingerprint {
        firecracker_version: crate::utils::CPU_TEMPLATE_HELPER_VERSION.to_string(),
        kernel_version: get_kernel_version()?,
        #[cfg(target_arch = "x86_64")]
        microcode_version: read_sysfs_file("/sys/devices/system/cpu/cpu0/microcode/version")?,
        #[cfg(target_arch = "aarch64")]
        microcode_version: read_sysfs_file(
            "/sys/devices/system/cpu/cpu0/regs/identification/revidr_el1",
        )?,
        bios_version: read_sysfs_file("/sys/devices/virtual/dmi/id/bios_version")?,
        // TODO: Replace this with `read_sysfs_file("/sys/devices/virtual/dmi/id/bios_release")`
        // after the end of kernel 4.14 support.
        // https://github.com/firecracker-microvm/firecracker/issues/3677
        bios_revision: run_shell_command(
            "set -o pipefail && dmidecode -t bios | grep \"BIOS Revision\" | cut -d':' -f2 | tr \
             -d ' \\n'",
        )?,
        guest_cpu_config: crate::template::dump::dump(vmm)?,
    })
}

fn get_kernel_version() -> Result<String, Error> {
    // SAFETY: An all-zeroed value for `libc::utsname` is valid.
    let mut name: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: The passed arg is a valid mutable reference of `libc::utsname`.
    let ret = unsafe { libc::uname(&mut name) };
    if ret < 0 {
        return Err(Error::GetKernelVersion(std::io::Error::last_os_error()));
    }

    // SAFETY: The fields of `libc::utsname` are terminated by a null byte ('\0').
    // https://man7.org/linux/man-pages/man2/uname.2.html
    let c_str = unsafe { std::ffi::CStr::from_ptr(name.release.as_ptr()) };
    // SAFETY: The `release` field is an array of `char` in C, in other words, ASCII.
    let version = c_str.to_str().unwrap();
    Ok(version.to_string())
}

fn read_sysfs_file(path: &str) -> Result<String, Error> {
    let s = read_to_string(path).map_err(|err| Error::ReadSysfsFile(path.to_string(), err))?;
    Ok(s.trim_end_matches('\n').to_string())
}

fn run_shell_command(cmd: &str) -> Result<String, Error> {
    let output = std::process::Command::new("bash")
        .args(["-c", cmd])
        .output()
        .map_err(|err| Error::ShellCommand(cmd.to_string(), err.to_string()))?;

    if !output.status.success() {
        return Err(Error::ShellCommand(
            cmd.to_string(),
            format!(
                "code: {:?}\nstdout: {}\nstderr: {}",
                output.status.code(),
                std::str::from_utf8(&output.stdout).unwrap(),
                std::str::from_utf8(&output.stderr).unwrap(),
            ),
        ));
    }
    Ok(std::str::from_utf8(&output.stdout).unwrap().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_kernel_version() {
        // `get_kernel_version()` should always succeed.
        get_kernel_version().unwrap();
    }

    #[test]
    fn test_read_valid_sysfs_file() {
        // The sysfs file for microcode version should exist and be read.
        let valid_sysfs_path = "/sys/devices/virtual/dmi/id/bios_version";
        read_sysfs_file(valid_sysfs_path).unwrap();
    }

    #[test]
    fn test_read_invalid_sysfs_file() {
        let invalid_sysfs_path = "/sys/invalid/path";
        if read_sysfs_file(invalid_sysfs_path).is_ok() {
            panic!("Should fail with `No such file or directory`");
        }
    }

    #[test]
    fn test_run_valid_shell_command() {
        let valid_cmd = "ls";
        run_shell_command(valid_cmd).unwrap();
    }

    #[test]
    fn test_run_invalid_shell_command() {
        let invalid_cmd = "unknown_command";
        if run_shell_command(invalid_cmd).is_ok() {
            panic!("Should fail with `unknown_command: not found`");
        }
    }
}
