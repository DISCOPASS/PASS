// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

pub use crate::devices::virtio::balloon::device::BalloonStats;
pub use crate::devices::virtio::BALLOON_DEV_ID;
use crate::devices::virtio::{Balloon, BalloonConfig};

type MutexBalloon = Arc<Mutex<Balloon>>;

/// Errors associated with the operations allowed on the balloon.
#[derive(Debug, derive_more::From)]
pub enum BalloonConfigError {
    /// The user made a request on an inexistent balloon device.
    DeviceNotFound,
    /// Device not activated yet.
    DeviceNotActive,
    /// The user tried to enable/disable the statistics after boot.
    InvalidStatsUpdate,
    /// Amount of pages requested is too large.
    TooManyPagesRequested,
    /// The user polled the statistics of a balloon device that
    /// does not have the statistics enabled.
    StatsNotFound,
    /// Failed to create a balloon device.
    CreateFailure(crate::devices::virtio::balloon::BalloonError),
    /// Failed to update the configuration of the ballon device.
    UpdateFailure(std::io::Error),
}

impl fmt::Display for BalloonConfigError {
    fn fmt(&self, f: &mut fmt::Formatter) -> std::fmt::Result {
        use self::BalloonConfigError::*;
        match self {
            DeviceNotFound => write!(f, "No balloon device found."),
            DeviceNotActive => write!(
                f,
                "Device is inactive, check if balloon driver is enabled in guest kernel."
            ),
            InvalidStatsUpdate => write!(f, "Cannot enable/disable the statistics after boot."),
            TooManyPagesRequested => write!(f, "Amount of pages requested is too large."),
            StatsNotFound => write!(f, "Statistics for the balloon device are not enabled"),
            CreateFailure(err) => write!(f, "Error creating the balloon device: {:?}", err),
            UpdateFailure(err) => write!(
                f,
                "Error updating the balloon device configuration: {:?}",
                err
            ),
        }
    }
}

type Result<T> = std::result::Result<T, BalloonConfigError>;

/// This struct represents the strongly typed equivalent of the json body
/// from balloon related requests.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BalloonDeviceConfig {
    /// Target balloon size in MiB.
    pub amount_mib: u32,
    /// Option to deflate the balloon in case the guest is out of memory.
    pub deflate_on_oom: bool,
    /// Interval in seconds between refreshing statistics.
    #[serde(default)]
    pub stats_polling_interval_s: u16,
}

impl From<BalloonConfig> for BalloonDeviceConfig {
    fn from(state: BalloonConfig) -> Self {
        BalloonDeviceConfig {
            amount_mib: state.amount_mib,
            deflate_on_oom: state.deflate_on_oom,
            stats_polling_interval_s: state.stats_polling_interval_s,
        }
    }
}

/// The data fed into a balloon update request. Currently, only the number
/// of pages and the stats polling interval can be updated.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BalloonUpdateConfig {
    /// Target balloon size in MiB.
    pub amount_mib: u32,
}

/// The data fed into a balloon statistics interval update request.
/// Note that the state of the statistics cannot be changed from ON to OFF
/// or vice versa after boot, only the interval of polling can be changed
/// if the statistics were activated in the device configuration.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BalloonUpdateStatsConfig {
    /// Interval in seconds between refreshing statistics.
    pub stats_polling_interval_s: u16,
}

/// A builder for `Balloon` devices from 'BalloonDeviceConfig'.
#[cfg_attr(not(test), derive(Default))]
pub struct BalloonBuilder {
    inner: Option<MutexBalloon>,
}

impl BalloonBuilder {
    /// Creates an empty Balloon Store.
    pub fn new() -> Self {
        Self { inner: None }
    }

    /// Inserts a Balloon device in the store.
    /// If an entry already exists, it will overwrite it.
    pub fn set(&mut self, cfg: BalloonDeviceConfig) -> Result<()> {
        self.inner = Some(Arc::new(Mutex::new(Balloon::new(
            cfg.amount_mib,
            cfg.deflate_on_oom,
            cfg.stats_polling_interval_s,
            // `restored` flag is false because this code path
            // is never called by snapshot restore functionality.
            false,
        )?)));

        Ok(())
    }

    /// Inserts an existing balloon device.
    pub fn set_device(&mut self, balloon: MutexBalloon) {
        self.inner = Some(balloon);
    }

    /// Provides a reference to the Balloon if present.
    pub fn get(&self) -> Option<&MutexBalloon> {
        self.inner.as_ref()
    }

    /// Returns the same structure that was used to configure the device.
    pub fn get_config(&self) -> Result<BalloonDeviceConfig> {
        self.get()
            .ok_or(BalloonConfigError::DeviceNotFound)
            .map(|balloon_mutex| balloon_mutex.lock().expect("Poisoned lock").config())
            .map(BalloonDeviceConfig::from)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn default_config() -> BalloonDeviceConfig {
        BalloonDeviceConfig {
            amount_mib: 0,
            deflate_on_oom: false,
            stats_polling_interval_s: 0,
        }
    }

    impl Default for BalloonBuilder {
        fn default() -> BalloonBuilder {
            let mut balloon = BalloonBuilder::new();
            assert!(balloon.set(BalloonDeviceConfig::default()).is_ok());
            balloon
        }
    }

    #[test]
    fn test_balloon_create() {
        let default_balloon_config = default_config();
        let balloon_config = BalloonDeviceConfig {
            amount_mib: 0,
            deflate_on_oom: false,
            stats_polling_interval_s: 0,
        };
        assert_eq!(default_balloon_config, balloon_config);
        let mut builder = BalloonBuilder::new();
        assert!(builder.get().is_none());

        builder.set(balloon_config).unwrap();
        assert_eq!(builder.get().unwrap().lock().unwrap().num_pages(), 0);
        assert_eq!(builder.get_config().unwrap(), default_balloon_config);

        let _update_config = BalloonUpdateConfig { amount_mib: 5 };
        let _stats_update_config = BalloonUpdateStatsConfig {
            stats_polling_interval_s: 5,
        };
    }

    #[test]
    fn test_from_balloon_state() {
        let expected_balloon_config = BalloonDeviceConfig {
            amount_mib: 5,
            deflate_on_oom: false,
            stats_polling_interval_s: 3,
        };

        let actual_balloon_config = BalloonDeviceConfig::from(BalloonConfig {
            amount_mib: 5,
            deflate_on_oom: false,
            stats_polling_interval_s: 3,
        });

        assert_eq!(expected_balloon_config, actual_balloon_config);
    }

    #[test]
    fn test_error_messages() {
        use std::io;

        use super::BalloonConfigError::*;
        let err = CreateFailure(crate::devices::virtio::balloon::BalloonError::EventFd(
            io::Error::from_raw_os_error(0),
        ));
        let _ = format!("{}{:?}", err, err);

        let err = UpdateFailure(io::Error::from_raw_os_error(0));
        let _ = format!("{}{:?}", err, err);

        let err = DeviceNotFound;
        let _ = format!("{}{:?}", err, err);

        let err = InvalidStatsUpdate;
        let _ = format!("{}{:?}", err, err);

        let err = TooManyPagesRequested;
        let _ = format!("{}{:?}", err, err);

        let err = StatsNotFound;
        let _ = format!("{}{:?}", err, err);
    }

    #[test]
    fn test_set_device() {
        let mut builder = BalloonBuilder::new();
        let balloon = Balloon::new(0, true, 0, true).unwrap();
        builder.set_device(Arc::new(Mutex::new(balloon)));
        assert!(builder.inner.is_some());
    }
}
