// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
use std::fmt::{self, Display, Formatter};

use serde::{ser, Serialize};

/// Enumerates microVM runtime states.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum VmState {
    /// Vm not started (yet)
    #[default]
    NotStarted,
    /// Vm is Paused
    Paused,
    /// Vm is running
    Running,
}

impl Display for VmState {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match *self {
            VmState::NotStarted => write!(f, "Not started"),
            VmState::Paused => write!(f, "Paused"),
            VmState::Running => write!(f, "Running"),
        }
    }
}

impl ser::Serialize for VmState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: ser::Serializer,
    {
        self.to_string().serialize(serializer)
    }
}

/// Serializable struct that contains general information about the microVM.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct InstanceInfo {
    /// The ID of the microVM.
    pub id: String,
    /// Whether the microVM is not started/running/paused.
    pub state: VmState,
    /// The version of the VMM that runs the microVM.
    pub vmm_version: String,
    /// The name of the application that runs the microVM.
    pub app_name: String,
}
