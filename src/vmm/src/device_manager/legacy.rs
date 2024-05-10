// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.
#![cfg(target_arch = "x86_64")]

use std::sync::{Arc, Mutex};

use kvm_ioctls::VmFd;
use libc::EFD_NONBLOCK;
use logger::METRICS;
use utils::eventfd::EventFd;
use vm_superio::Serial;

use crate::devices::legacy::{EventFdTrigger, SerialDevice, SerialEventsWrapper};

/// Errors corresponding to the `PortIODeviceManager`.
#[derive(Debug, derive_more::From, thiserror::Error)]
pub enum Error {
    /// Cannot add legacy device to Bus.
    #[error("Failed to add legacy device to Bus: {0}")]
    BusError(crate::devices::BusError),
    /// Cannot create EventFd.
    #[error("Failed to create EventFd: {0}")]
    EventFd(std::io::Error),
}

type Result<T> = ::std::result::Result<T, Error>;

fn create_serial(com_event: EventFdTrigger) -> Result<Arc<Mutex<SerialDevice>>> {
    let serial_device = Arc::new(Mutex::new(SerialDevice {
        serial: Serial::with_events(
            com_event.try_clone()?,
            SerialEventsWrapper {
                metrics: METRICS.uart.clone(),
                buffer_ready_event_fd: None,
            },
            Box::new(std::io::sink()),
        ),
        input: None,
    }));

    Ok(serial_device)
}

/// The `PortIODeviceManager` is a wrapper that is used for registering legacy devices
/// on an I/O Bus. It currently manages the uart and i8042 devices.
/// The `LegacyDeviceManger` should be initialized only by using the constructor.
pub struct PortIODeviceManager {
    pub io_bus: crate::devices::Bus,
    pub stdio_serial: Arc<Mutex<SerialDevice>>,
    pub i8042: Arc<Mutex<crate::devices::legacy::I8042Device>>,

    // Communication event on ports 1 & 3.
    pub com_evt_1_3: EventFdTrigger,
    // Communication event on ports 2 & 4.
    pub com_evt_2_4: EventFdTrigger,
    // Keyboard event.
    pub kbd_evt: EventFd,
}

impl PortIODeviceManager {
    /// x86 global system interrupt for communication events on serial ports 1
    /// & 3. See
    /// <https://en.wikipedia.org/wiki/Interrupt_request_(PC_architecture)>.
    const COM_EVT_1_3_GSI: u32 = 4;
    /// x86 global system interrupt for communication events on serial ports 2
    /// & 4. See
    /// <https://en.wikipedia.org/wiki/Interrupt_request_(PC_architecture)>.
    const COM_EVT_2_4_GSI: u32 = 3;
    /// x86 global system interrupt for keyboard port.
    /// See <https://en.wikipedia.org/wiki/Interrupt_request_(PC_architecture)>.
    const KBD_EVT_GSI: u32 = 1;
    /// Legacy serial port device addresses. See
    /// <https://tldp.org/HOWTO/Serial-HOWTO-10.html#ss10.1>.
    const SERIAL_PORT_ADDRESSES: [u64; 4] = [0x3f8, 0x2f8, 0x3e8, 0x2e8];
    /// Size of legacy serial ports.
    const SERIAL_PORT_SIZE: u64 = 0x8;
    /// i8042 keyboard data register address. See
    /// <https://elixir.bootlin.com/linux/latest/source/drivers/input/serio/i8042-io.h#L41>.
    const I8042_KDB_DATA_REGISTER_ADDRESS: u64 = 0x060;
    /// i8042 keyboard data register size.
    const I8042_KDB_DATA_REGISTER_SIZE: u64 = 0x5;

    /// Create a new DeviceManager handling legacy devices (uart, i8042).
    pub fn new(serial: Arc<Mutex<SerialDevice>>, i8042_reset_evfd: EventFd) -> Result<Self> {
        let io_bus = crate::devices::Bus::new();
        let com_evt_1_3 = serial
            .lock()
            .expect("Poisoned lock")
            .serial
            .interrupt_evt()
            .try_clone()?;
        let com_evt_2_4 = EventFdTrigger::new(EventFd::new(EFD_NONBLOCK)?);
        let kbd_evt = EventFd::new(libc::EFD_NONBLOCK)?;

        let i8042 = Arc::new(Mutex::new(crate::devices::legacy::I8042Device::new(
            i8042_reset_evfd,
            kbd_evt.try_clone()?,
        )));

        Ok(PortIODeviceManager {
            io_bus,
            stdio_serial: serial,
            i8042,
            com_evt_1_3,
            com_evt_2_4,
            kbd_evt,
        })
    }

    /// Register supported legacy devices.
    pub fn register_devices(&mut self, vm_fd: &VmFd) -> Result<()> {
        let serial_2_4 = create_serial(self.com_evt_2_4.try_clone()?)?;
        let serial_1_3 = create_serial(self.com_evt_1_3.try_clone()?)?;
        self.io_bus.insert(
            self.stdio_serial.clone(),
            Self::SERIAL_PORT_ADDRESSES[0],
            Self::SERIAL_PORT_SIZE,
        )?;
        self.io_bus.insert(
            serial_2_4.clone(),
            Self::SERIAL_PORT_ADDRESSES[1],
            Self::SERIAL_PORT_SIZE,
        )?;
        self.io_bus.insert(
            serial_1_3.clone(),
            Self::SERIAL_PORT_ADDRESSES[2],
            Self::SERIAL_PORT_SIZE,
        )?;
        self.io_bus.insert(
            serial_2_4,
            Self::SERIAL_PORT_ADDRESSES[3],
            Self::SERIAL_PORT_SIZE,
        )?;
        self.io_bus.insert(
            self.i8042.clone(),
            Self::I8042_KDB_DATA_REGISTER_ADDRESS,
            Self::I8042_KDB_DATA_REGISTER_SIZE,
        )?;

        vm_fd
            .register_irqfd(&self.com_evt_1_3, Self::COM_EVT_1_3_GSI)
            .map_err(|e| Error::EventFd(std::io::Error::from_raw_os_error(e.errno())))?;
        vm_fd
            .register_irqfd(&self.com_evt_2_4, Self::COM_EVT_2_4_GSI)
            .map_err(|e| Error::EventFd(std::io::Error::from_raw_os_error(e.errno())))?;
        vm_fd
            .register_irqfd(&self.kbd_evt, Self::KBD_EVT_GSI)
            .map_err(|e| Error::EventFd(std::io::Error::from_raw_os_error(e.errno())))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use utils::vm_memory::GuestAddress;

    use super::*;

    #[test]
    fn test_register_legacy_devices() {
        let guest_mem = utils::vm_memory::test_utils::create_anon_guest_memory(
            &[(GuestAddress(0x0), 0x1000)],
            false,
        )
        .unwrap();
        let mut vm = crate::builder::setup_kvm_vm(&guest_mem, false).unwrap();
        crate::builder::setup_interrupt_controller(&mut vm).unwrap();
        let mut ldm = PortIODeviceManager::new(
            create_serial(EventFdTrigger::new(EventFd::new(EFD_NONBLOCK).unwrap())).unwrap(),
            EventFd::new(libc::EFD_NONBLOCK).unwrap(),
        )
        .unwrap();
        assert!(ldm.register_devices(vm.fd()).is_ok());
    }
}
