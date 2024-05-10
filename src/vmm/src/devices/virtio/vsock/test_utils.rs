// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

#![doc(hidden)]

use std::os::unix::io::{AsRawFd, RawFd};

use utils::epoll::EventSet;
use utils::eventfd::EventFd;
use utils::vm_memory::{GuestAddress, GuestMemoryMmap};

use crate::devices::virtio::test_utils::{single_region_mem, VirtQueue as GuestQ};
use crate::devices::virtio::vsock::device::{RXQ_INDEX, TXQ_INDEX};
use crate::devices::virtio::vsock::packet::{VsockPacket, VSOCK_PKT_HDR_SIZE};
use crate::devices::virtio::{
    VirtioDevice, Vsock, VsockBackend, VsockChannel, VsockEpollListener, VsockError,
    VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE,
};

type Result<T> = std::result::Result<T, VsockError>;

pub struct TestBackend {
    pub evfd: EventFd,
    pub rx_err: Option<VsockError>,
    pub tx_err: Option<VsockError>,
    pub pending_rx: bool,
    pub rx_ok_cnt: usize,
    pub tx_ok_cnt: usize,
    pub evset: Option<EventSet>,
}

impl TestBackend {
    pub fn new() -> Self {
        Self {
            evfd: EventFd::new(libc::EFD_NONBLOCK).unwrap(),
            rx_err: None,
            tx_err: None,
            pending_rx: false,
            rx_ok_cnt: 0,
            tx_ok_cnt: 0,
            evset: None,
        }
    }

    pub fn set_rx_err(&mut self, err: Option<VsockError>) {
        self.rx_err = err;
    }
    pub fn set_tx_err(&mut self, err: Option<VsockError>) {
        self.tx_err = err;
    }
    pub fn set_pending_rx(&mut self, prx: bool) {
        self.pending_rx = prx;
    }
}

impl Default for TestBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl VsockChannel for TestBackend {
    fn recv_pkt(&mut self, pkt: &mut VsockPacket, mem: &GuestMemoryMmap) -> Result<()> {
        let cool_buf = [0xDu8, 0xE, 0xA, 0xD, 0xB, 0xE, 0xE, 0xF];
        match self.rx_err.take() {
            None => {
                let buf_size = pkt.buf_size();
                if buf_size > 0 {
                    let buf: Vec<u8> = (0..buf_size)
                        .map(|i| cool_buf[i % cool_buf.len()])
                        .collect();
                    pkt.read_at_offset_from(mem, 0, &mut std::io::Cursor::new(buf), buf_size)
                        .unwrap();
                }
                self.rx_ok_cnt += 1;
                Ok(())
            }
            Some(err) => Err(err),
        }
    }

    fn send_pkt(&mut self, _pkt: &VsockPacket, _mem: &GuestMemoryMmap) -> Result<()> {
        match self.tx_err.take() {
            None => {
                self.tx_ok_cnt += 1;
                Ok(())
            }
            Some(err) => Err(err),
        }
    }

    fn has_pending_rx(&self) -> bool {
        self.pending_rx
    }
}

impl AsRawFd for TestBackend {
    fn as_raw_fd(&self) -> RawFd {
        self.evfd.as_raw_fd()
    }
}

impl VsockEpollListener for TestBackend {
    fn get_polled_evset(&self) -> EventSet {
        EventSet::IN
    }
    fn notify(&mut self, evset: EventSet) {
        self.evset = Some(evset);
    }
}
impl VsockBackend for TestBackend {}

pub struct TestContext {
    pub cid: u64,
    pub mem: GuestMemoryMmap,
    pub mem_size: usize,
    pub device: Vsock<TestBackend>,
}

impl TestContext {
    pub fn new() -> Self {
        const CID: u64 = 52;
        const MEM_SIZE: usize = 1024 * 1024 * 128;
        let mem = single_region_mem(MEM_SIZE);
        Self {
            cid: CID,
            mem,
            mem_size: MEM_SIZE,
            device: Vsock::new(CID, TestBackend::new()).unwrap(),
        }
    }

    pub fn create_event_handler_context(&self) -> EventHandlerContext {
        const QSIZE: u16 = 256;

        let guest_rxvq = GuestQ::new(GuestAddress(0x0010_0000), &self.mem, QSIZE);
        let guest_txvq = GuestQ::new(GuestAddress(0x0020_0000), &self.mem, QSIZE);
        let guest_evvq = GuestQ::new(GuestAddress(0x0030_0000), &self.mem, QSIZE);
        let rxvq = guest_rxvq.create_queue();
        let txvq = guest_txvq.create_queue();
        let evvq = guest_evvq.create_queue();

        // Set up one available descriptor in the RX queue.
        guest_rxvq.dtable[0].set(
            0x0040_0000,
            VSOCK_PKT_HDR_SIZE as u32,
            VIRTQ_DESC_F_WRITE | VIRTQ_DESC_F_NEXT,
            1,
        );
        guest_rxvq.dtable[1].set(0x0040_1000, 4096, VIRTQ_DESC_F_WRITE, 0);

        guest_rxvq.avail.ring[0].set(0);
        guest_rxvq.avail.idx.set(1);

        // Set up one available descriptor in the TX queue.
        guest_txvq.dtable[0].set(0x0050_0000, VSOCK_PKT_HDR_SIZE as u32, VIRTQ_DESC_F_NEXT, 1);
        guest_txvq.dtable[1].set(0x0050_1000, 4096, 0, 0);
        guest_txvq.avail.ring[0].set(0);
        guest_txvq.avail.idx.set(1);

        let queues = vec![rxvq, txvq, evvq];
        EventHandlerContext {
            guest_rxvq,
            guest_txvq,
            guest_evvq,
            device: Vsock::with_queues(self.cid, TestBackend::new(), queues).unwrap(),
        }
    }
}

impl Default for TestContext {
    fn default() -> Self {
        Self::new()
    }
}

pub struct EventHandlerContext<'a> {
    pub device: Vsock<TestBackend>,
    pub guest_rxvq: GuestQ<'a>,
    pub guest_txvq: GuestQ<'a>,
    pub guest_evvq: GuestQ<'a>,
}

impl<'a> EventHandlerContext<'a> {
    pub fn mock_activate(&mut self, mem: GuestMemoryMmap) {
        // Artificially activate the device.
        self.device.activate(mem).unwrap();
    }

    pub fn signal_txq_event(&mut self) {
        self.device.queue_events[TXQ_INDEX].write(1).unwrap();
        self.device.handle_txq_event(EventSet::IN);
    }
    pub fn signal_rxq_event(&mut self) {
        self.device.queue_events[RXQ_INDEX].write(1).unwrap();
        self.device.handle_rxq_event(EventSet::IN);
    }
}

impl<B> Vsock<B>
where
    B: VsockBackend,
{
    pub fn write_element_in_queue(vsock: &Vsock<B>, idx: usize, val: u64) {
        if idx > vsock.queue_events.len() - 1 {
            panic!("Index bigger than the number of queues of this device");
        }
        vsock.queue_events[idx].write(val).unwrap();
    }

    pub fn get_element_from_interest_list(vsock: &Vsock<B>, idx: usize) -> u64 {
        match idx {
            0..=2 => u64::try_from(vsock.queue_events[idx].as_raw_fd()).unwrap(),
            3 => u64::try_from(vsock.backend.as_raw_fd()).unwrap(),
            4 => u64::try_from(vsock.activate_evt.as_raw_fd()).unwrap(),
            _ => panic!("Index bigger than interest list"),
        }
    }
}
