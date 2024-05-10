// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::undocumented_unsafe_blocks)]
mod serial_utils {
    use std::io;
    use std::os::unix::io::{AsRawFd, RawFd};

    use vmm::devices::legacy::ReadableFd;

    pub struct MockSerialInput(pub RawFd);

    impl io::Read for MockSerialInput {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let count = unsafe { libc::read(self.0, buf.as_mut_ptr().cast(), buf.len()) };
            if count < 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(count as usize)
        }
    }

    impl AsRawFd for MockSerialInput {
        fn as_raw_fd(&self) -> RawFd {
            self.0
        }
    }

    impl ReadableFd for MockSerialInput {}
}

use std::io;
use std::io::Stdout;
use std::os::raw::{c_int, c_void};
use std::sync::{Arc, Mutex};

use event_manager::{EventManager, SubscriberOps};
use libc::EFD_NONBLOCK;
use logger::METRICS;
use serial_utils::MockSerialInput;
use utils::eventfd::EventFd;
use vm_superio::Serial;
use vmm::devices::legacy::{EventFdTrigger, SerialEventsWrapper, SerialWrapper};
use vmm::devices::BusDevice;

fn create_serial(
    pipe: c_int,
) -> Arc<Mutex<SerialWrapper<EventFdTrigger, SerialEventsWrapper, Box<Stdout>>>> {
    // Serial input is the reading end of the pipe.
    let serial_in = MockSerialInput(pipe);
    let kick_stdin_evt = EventFdTrigger::new(EventFd::new(libc::EFD_NONBLOCK).unwrap());

    Arc::new(Mutex::new(SerialWrapper {
        serial: Serial::with_events(
            EventFdTrigger::new(EventFd::new(EFD_NONBLOCK).unwrap()),
            SerialEventsWrapper {
                metrics: METRICS.uart.clone(),
                buffer_ready_event_fd: Some(kick_stdin_evt.try_clone().unwrap()),
            },
            Box::new(io::stdout()),
        ),
        input: Some(Box::new(serial_in)),
    }))
}

#[test]
fn test_issue_serial_hangup_anon_pipe_while_registered_stdin() {
    let mut fds: [c_int; 2] = [0; 2];
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    assert!(rc == 0);

    // Serial input is the reading end of the pipe.
    let serial = create_serial(fds[0]);

    // Make reading fd non blocking to read just what is inflight.
    let flags = unsafe { libc::fcntl(fds[0], libc::F_GETFL, 0) };
    let mut rc = unsafe { libc::fcntl(fds[0], libc::F_SETFL, flags | libc::O_NONBLOCK) };
    assert!(rc == 0);

    const BYTES_COUNT: usize = 65; // Serial FIFO_SIZE + 1.
    let mut dummy_data = [1u8; BYTES_COUNT];
    rc = unsafe {
        libc::write(
            fds[1],
            dummy_data.as_mut_ptr() as *const c_void,
            dummy_data.len(),
        ) as i32
    };
    assert!(dummy_data.len() == rc as usize);

    // Register the reading end of the pipe to the event manager, to be processed later on.
    let mut event_manager = EventManager::new().unwrap();
    let _id = event_manager.add_subscriber(serial.clone());

    // `EventSet::IN` was received on stdin. The event handling will consume
    // 64 bytes from stdin. The stdin monitoring is still armed.
    let mut ev_count = event_manager.run().unwrap();
    assert_eq!(ev_count, 1);

    let mut data = [0u8; BYTES_COUNT];

    // On the main thread, we will simulate guest "vCPU" thread serial reads.
    let data_bus_offset = 0;
    for i in 0..BYTES_COUNT - 1 {
        serial
            .lock()
            .unwrap()
            .read(data_bus_offset, &mut data[i..=i]);
    }

    assert!(data[..31] == dummy_data[..31]);
    assert!(data[32..64] == dummy_data[32..64]);

    // The avail capacity of the serial FIFO is 64.
    // Read the 65th from the stdin through the kick stdin event triggered by 64th of the serial
    // FIFO read, or by the armed level-triggered stdin monitoring. Either one of the events might
    // be handled first. The handling of the second event will find the stdin without any pending
    // bytes and will result in EWOULDBLOCK. Usually, EWOULDBLOCK will reregister the stdin, but
    // since it was not unregistered before, it will do a noop.
    ev_count = event_manager.run().unwrap();
    assert_eq!(ev_count, 2);

    // The avail capacity of the serial FIFO is 63.
    rc = unsafe {
        libc::write(
            fds[1],
            dummy_data.as_mut_ptr() as *const c_void,
            dummy_data.len(),
        ) as i32
    };
    assert!(dummy_data.len() == rc as usize);

    // Writing to the other end of the pipe triggers handling a stdin event.
    // Now, 63 bytes will be read from stdin, filling up the buffer.
    ev_count = event_manager.run().unwrap();
    assert_eq!(ev_count, 1);

    // Close the writing end (this sends an HANG_UP to the reading end).
    // While the stdin is registered, this event is caught by the event manager.
    rc = unsafe { libc::close(fds[1]) };
    assert!(rc == 0);

    // This cycle of epoll has two important events. First, the received HANGUP and second
    // the fact that the FIFO is full, so even if the stdin reached EOF, there are still
    // pending bytes to be read. We still unregister the stdin and keep reading from it until
    // we get all pending bytes.
    ev_count = event_manager.run().unwrap();
    assert_eq!(ev_count, 1);

    // Free up 64 bytes from the serial FIFO.
    for i in 0..BYTES_COUNT - 1 {
        serial
            .lock()
            .unwrap()
            .read(data_bus_offset, &mut data[i..=i]);
    }

    // Process the kick stdin event generated by the reading of the 64th byte of the serial FIFO.
    // This will consume some more bytes from the stdin while the stdin is unregistered.
    ev_count = event_manager.run().unwrap();
    assert_eq!(ev_count, 1);

    // Two more bytes left. At the 2nd byte, another kick read stdin event is generated,
    // trying to fill again the serial FIFO with more bytes.
    for i in 0..2 {
        serial
            .lock()
            .unwrap()
            .read(data_bus_offset, &mut data[i..=i]);
    }

    // We try to read again, but we detect that stdin received previously EOF.
    // This can be deduced by reading from a non-blocking fd and getting 0 bytes as a result,
    // instead of EWOUDBLOCK. We unregister the stdin and the kick stdin read evt.
    ev_count = event_manager.run().unwrap();
    assert_eq!(ev_count, 1);

    // Nothing can interrupt us.
    ev_count = event_manager.run_with_timeout(1).unwrap();
    assert_eq!(ev_count, 0);
}

#[test]
fn test_issue_hangup() {
    let mut fds: [c_int; 2] = [0; 2];
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    assert!(rc == 0);

    // Serial input is the reading end of the pipe.
    let serial = create_serial(fds[0]);

    // Make reading fd non blocking to read just what is inflight.
    let flags = unsafe { libc::fcntl(fds[0], libc::F_GETFL, 0) };
    let mut rc = unsafe { libc::fcntl(fds[0], libc::F_SETFL, flags | libc::O_NONBLOCK) };
    assert!(rc == 0);

    // Close the writing end (this sends an HANG_UP to the reading end).
    // While the stdin is registered, this event is caught by the event manager.
    rc = unsafe { libc::close(fds[1]) };
    assert!(rc == 0);

    // Register the reading end of the pipe to the event manager, to be processed later on.
    let mut event_manager = EventManager::new().unwrap();
    let _id = event_manager.add_subscriber(serial);

    let mut ev_count = event_manager.run().unwrap();
    assert_eq!(ev_count, 1);

    // Nothing can interrupt us.
    ev_count = event_manager.run_with_timeout(1).unwrap();
    assert_eq!(ev_count, 0);
}

#[test]
fn test_issue_serial_hangup_anon_pipe_while_unregistered_stdin() {
    let mut fds: [c_int; 2] = [0; 2];
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    assert!(rc == 0);

    // Serial input is the reading end of the pipe.
    let serial = create_serial(fds[0]);

    // Make reading fd non blocking to read just what is inflight.
    let flags = unsafe { libc::fcntl(fds[0], libc::F_GETFL, 0) };
    let mut rc = unsafe { libc::fcntl(fds[0], libc::F_SETFL, flags | libc::O_NONBLOCK) };
    assert!(rc == 0);

    const BYTES_COUNT: usize = 65; // Serial FIFO_SIZE + 1.
    let mut dummy_data = [1u8; BYTES_COUNT];
    rc = unsafe {
        libc::write(
            fds[1],
            dummy_data.as_mut_ptr() as *const c_void,
            dummy_data.len(),
        ) as i32
    };
    assert!(dummy_data.len() == rc as usize);

    // Register the reading end of the pipe to the event manager, to be processed later on.
    let mut event_manager = EventManager::new().unwrap();
    let _id = event_manager.add_subscriber(serial.clone());

    // `EventSet::IN` was received on stdin. The event handling will consume
    // 64 bytes from stdin. The stdin monitoring is still armed.
    let mut ev_count = event_manager.run_with_timeout(0).unwrap();
    assert_eq!(ev_count, 1);

    let mut data = [0u8; BYTES_COUNT];

    // On the main thread, we will simulate guest "vCPU" thread serial reads.
    let data_bus_offset = 0;
    for i in 0..BYTES_COUNT - 1 {
        serial
            .lock()
            .unwrap()
            .read(data_bus_offset, &mut data[i..=i]);
    }

    assert!(data[..31] == dummy_data[..31]);
    assert!(data[32..64] == dummy_data[32..64]);

    // The avail capacity of the serial FIFO is 64.
    // Read the 65th from the stdin through the kick stdin event triggered by 64th of the serial
    // FIFO read, or by the armed level-triggered stdin monitoring. Either one of the events might
    // be handled first. The handling of the second event will find the stdin without any pending
    // bytes and will result in EWOULDBLOCK. Usually, EWOULDBLOCK will reregister the stdin, but
    // since it was not unregistered before, it will do a noop.
    ev_count = event_manager.run().unwrap();
    assert_eq!(ev_count, 2);

    // The avail capacity of the serial FIFO is 63.
    rc = unsafe {
        libc::write(
            fds[1],
            dummy_data.as_mut_ptr() as *const c_void,
            dummy_data.len(),
        ) as i32
    };
    assert!(dummy_data.len() == rc as usize);

    // Writing to the other end of the pipe triggers handling an stdin event.
    // Now, 63 bytes will be read from stdin, filling up the buffer.
    ev_count = event_manager.run().unwrap();
    assert_eq!(ev_count, 1);

    // Serial FIFO is full, so silence the stdin. We do not need any other interruptions
    // until the serial FIFO is freed.
    ev_count = event_manager.run().unwrap();
    assert_eq!(ev_count, 1);

    // Close the writing end (this sends an HANG_UP to the reading end).
    // While the stdin is unregistered, this event is not caught by the event manager.
    rc = unsafe { libc::close(fds[1]) };
    assert!(rc == 0);

    // This would be a blocking epoll_wait, since the buffer is full and stdin is unregistered.
    // There is no event that can break the epoll wait loop.
    ev_count = event_manager.run_with_timeout(0).unwrap();
    assert_eq!(ev_count, 0);

    // Free up 64 bytes from the serial FIFO.
    for i in 0..BYTES_COUNT - 1 {
        serial
            .lock()
            .unwrap()
            .read(data_bus_offset, &mut data[i..=i]);
    }

    // Process the kick stdin event generated by the reading of the 64th byte of the serial FIFO.
    // This will consume some more bytes from the stdin. Keep in mind that the HANGUP event was
    // lost and we do not know that the stdin reached EOF.
    ev_count = event_manager.run().unwrap();
    assert_eq!(ev_count, 1);

    // Two more bytes left. At the 2nd byte, another kick read stdin event is generated,
    // trying to fill again the serial FIFO with more bytes. Keep in mind that the HANGUP event was
    // lost and we do not know that the stdin reached EOF.
    for i in 0..2 {
        serial
            .lock()
            .unwrap()
            .read(data_bus_offset, &mut data[i..=i]);
    }

    // We try to read again, but we detect that stdin received previously EOF.
    // This can be deduced by reading from a non-blocking fd and getting 0 bytes as a result,
    // instead of EWOUDBLOCK. We unregister the stdin and the kick stdin read evt.
    ev_count = event_manager.run().unwrap();
    assert_eq!(ev_count, 1);

    // Nothing can interrupt us.
    ev_count = event_manager.run_with_timeout(0).unwrap();
    assert_eq!(ev_count, 0);
}
