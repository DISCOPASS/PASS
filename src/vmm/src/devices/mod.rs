// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.
use std::io;

mod bus;
pub mod legacy;
pub mod pseudo;
pub mod virtio;

use logger::{error, IncMetric, METRICS};

pub use self::bus::{Bus, BusDevice, Error as BusError};
use crate::devices::virtio::{QueueError, VsockError};

// Function used for reporting error in terms of logging
// but also in terms of METRICS net event fails.
pub(crate) fn report_net_event_fail(err: Error) {
    error!("{:?}", err);
    METRICS.net.event_fails.inc();
}

pub(crate) fn report_balloon_event_fail(err: virtio::balloon::BalloonError) {
    error!("{:?}", err);
    METRICS.balloon.event_fails.inc();
}

#[derive(Debug)]
pub enum Error {
    /// Failed to read from the TAP device.
    FailedReadTap,
    /// Failed to signal irq.
    FailedSignalingIrq(io::Error),
    /// IO error.
    IoError(io::Error),
    /// Device received malformed payload.
    MalformedPayload,
    /// Device received malformed descriptor.
    MalformedDescriptor,
    /// Error during queue processing.
    QueueError(QueueError),
    /// Vsock device error.
    VsockError(VsockError),
}
