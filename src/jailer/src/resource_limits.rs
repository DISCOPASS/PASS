// Copyright 2021 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::fmt::{Display, Formatter};

use utils::syscall::SyscallReturnCode;

use super::{Error, Result};

// Default limit for the maximum number of file descriptors open at a time.
const NO_FILE: u64 = 2048;
// File size resource argument name.
pub(crate) const FSIZE_ARG: &str = "fsize";
// Number of files resource argument name.
pub(crate) const NO_FILE_ARG: &str = "no-file";

#[derive(Clone, Copy)]
pub enum Resource {
    // Size of created files.
    RlimitFsize,
    // Number of open file descriptors.
    RlimitNoFile,
}

impl From<Resource> for u32 {
    fn from(resource: Resource) -> u32 {
        match resource {
            #[allow(clippy::unnecessary_cast)]
            // Definition of libc::RLIMIT_FSIZE depends on the target_env:
            //      * when equals to "musl" -> libc::RLIMIT_FSIZE is a c_int (which is an i32)
            //      * when equals to "gnu" -> libc::RLIMIT_FSIZE is __rlimit_resource_t which is a
            //        c_uint (which is an u32)
            Resource::RlimitFsize => libc::RLIMIT_FSIZE as u32,
            #[allow(clippy::unnecessary_cast)]
            // Definition of libc::RLIMIT_NOFILE depends on the target_env:
            //      * when equals to "musl" -> libc::RLIMIT_NOFILE is a c_int (which is an i32)
            //      * when equals to "gnu" -> libc::RLIMIT_NOFILE is __rlimit_resource_t which is a
            //        c_uint (which is an u32)
            Resource::RlimitNoFile => libc::RLIMIT_NOFILE as u32,
        }
    }
}

impl Display for Resource {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Resource::RlimitFsize => write!(f, "size of file"),
            Resource::RlimitNoFile => write!(f, "number of file descriptors"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ResourceLimits {
    file_size: Option<u64>,
    no_file: u64,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        ResourceLimits {
            file_size: None,
            no_file: NO_FILE,
        }
    }
}

impl ResourceLimits {
    pub fn install(self) -> Result<()> {
        if let Some(file_size) = self.file_size {
            // Set file size limit.
            ResourceLimits::set_limit(Resource::RlimitFsize, file_size)?;
        }
        // Set limit on number of file descriptors.
        ResourceLimits::set_limit(Resource::RlimitNoFile, self.no_file)?;

        Ok(())
    }

    fn set_limit(resource: Resource, target: libc::rlim_t) -> Result<()> {
        let rlim: libc::rlimit = libc::rlimit {
            rlim_cur: target,
            rlim_max: target,
        };

        // SAFETY: Safe because `resource` is a known-valid constant, and `&rlim`
        // is non-dangling.
        SyscallReturnCode(unsafe { libc::setrlimit(u32::from(resource) as _, &rlim) })
            .into_empty_result()
            .map_err(|_| Error::Setrlimit(resource.to_string()))
    }

    pub fn set_file_size(&mut self, file_size: u64) {
        self.file_size = Some(file_size);
    }

    pub fn set_no_file(&mut self, no_file: u64) {
        self.no_file = no_file;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::undocumented_unsafe_blocks)]
    use super::*;

    #[test]
    #[allow(clippy::unnecessary_cast)]
    fn test_from_resource() {
        assert_eq!(u32::from(Resource::RlimitFsize), libc::RLIMIT_FSIZE as u32);
        assert_eq!(
            u32::from(Resource::RlimitNoFile),
            libc::RLIMIT_NOFILE as u32
        );
    }

    #[test]
    fn test_display_resource() {
        assert_eq!(
            Resource::RlimitFsize.to_string(),
            "size of file".to_string()
        );
        assert_eq!(
            Resource::RlimitNoFile.to_string(),
            "number of file descriptors".to_string()
        );
    }

    #[test]
    fn test_default_resource_limits() {
        let mut rlimits = ResourceLimits::default();
        assert!(rlimits.file_size.is_none());
        assert_eq!(rlimits.no_file, NO_FILE);

        rlimits.set_file_size(1);
        assert_eq!(rlimits.file_size.unwrap(), 1);
        rlimits.set_no_file(1);
        assert_eq!(rlimits.no_file, 1);
    }

    #[test]
    fn test_set_resource_limits() {
        let resource = Resource::RlimitNoFile;
        let new_limit = NO_FILE - 1;
        // Get current file size limit.
        let mut rlim: libc::rlimit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        unsafe { libc::getrlimit(u32::from(resource) as _, &mut rlim) };
        assert_ne!(rlim.rlim_cur, new_limit);
        assert_ne!(rlim.rlim_max, new_limit);

        // Set new file size limit.
        ResourceLimits::set_limit(resource, new_limit).unwrap();

        // Verify new limit.
        let mut rlim: libc::rlimit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        unsafe { libc::getrlimit(u32::from(resource) as _, &mut rlim) };
        assert_eq!(rlim.rlim_cur, new_limit);
        assert_eq!(rlim.rlim_max, new_limit);
    }
}
