// Copyright 2022 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod device;
mod event_handler;
pub mod persist;

pub use self::device::{Entropy, Error};

pub(crate) const RNG_NUM_QUEUES: usize = 1;
pub(crate) const RNG_QUEUE_SIZE: u16 = 256;

pub(crate) const RNG_QUEUE: usize = 0;
