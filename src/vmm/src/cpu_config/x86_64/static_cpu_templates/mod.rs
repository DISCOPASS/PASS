// Copyright 2022 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use derive_more::Display;
use serde::{Deserialize, Serialize};
use versionize::{VersionMap, Versionize, VersionizeError, VersionizeResult};
use versionize_derive::Versionize;

/// Module with C3 CPU template for x86_64
pub mod c3;
/// Module with T2 CPU template for x86_64
pub mod t2;
/// Module with T2A CPU template for x86_64
pub mod t2a;
/// Module with T2CL CPU template for x86_64
pub mod t2cl;
/// Module with T2S CPU template for x86_64
pub mod t2s;

/// Template types available for configuring the x86 CPU features that map
/// to EC2 instances.
#[derive(
    Debug, Default, Display, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Versionize,
)]
pub enum StaticCpuTemplate {
    /// C3 Template.
    #[display(fmt = "C3")]
    C3,
    /// T2 Template.
    #[display(fmt = "T2")]
    T2,
    /// T2S Template.
    #[display(fmt = "T2S")]
    T2S,
    /// No CPU template is used.
    #[default]
    #[display(fmt = "None")]
    None,
    /// T2CL Template.
    #[display(fmt = "T2CL")]
    T2CL,
    /// T2A Template.
    #[display(fmt = "T2A")]
    T2A,
}

impl StaticCpuTemplate {
    /// Check if no template specified
    pub fn is_none(&self) -> bool {
        self == &StaticCpuTemplate::None
    }
}
