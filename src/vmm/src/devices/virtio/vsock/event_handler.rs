// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

/// The vsock object implements the runtime logic of our vsock device:
/// 1. Respond to TX queue events by wrapping virtio buffers into `VsockPacket`s, then sending
/// those    packets to the `VsockBackend`;
/// 2. Forward backend FD event notifications to the `VsockBackend`;
/// 3. Fetch incoming packets from the `VsockBackend` and place them into the virtio RX queue;
/// 4. Whenever we have processed some virtio buffers (either TX or RX), let the driver know by
///    raising our assigned IRQ.
///
/// In a nutshell, the logic looks like this:
/// - on TX queue event:
///   - fetch all packets from the TX queue and send them to the backend; then
///   - if the backend has queued up any incoming packets, fetch them into any available RX
///     buffers.
/// - on RX queue event:
///   - fetch any incoming packets, queued up by the backend, into newly available RX buffers.
/// - on backend event:
///   - forward the event to the backend; then
///   - again, attempt to fetch any incoming packets queued by the backend into virtio RX
///     buffers.
use std::os::unix::io::AsRawFd;

use event_manager::{EventOps, Events, MutEventSubscriber};
use logger::{debug, error, warn, IncMetric, METRICS};
use utils::epoll::EventSet;

use super::device::{Vsock, EVQ_INDEX, RXQ_INDEX, TXQ_INDEX};
use super::VsockBackend;
use crate::devices::virtio::VirtioDevice;

impl<B> Vsock<B>
where
    B: VsockBackend + 'static,
{
    pub fn handle_rxq_event(&mut self, evset: EventSet) -> bool {
        debug!("vsock: RX queue event");

        if evset != EventSet::IN {
            warn!("vsock: rxq unexpected event {:?}", evset);
            METRICS.vsock.rx_queue_event_fails.inc();
            return false;
        }

        let mut raise_irq = false;
        if let Err(err) = self.queue_events[RXQ_INDEX].read() {
            error!("Failed to get vsock rx queue event: {:?}", err);
            METRICS.vsock.rx_queue_event_fails.inc();
        } else if self.backend.has_pending_rx() {
            raise_irq |= self.process_rx();
            METRICS.vsock.rx_queue_event_count.inc();
        }
        raise_irq
    }

    pub fn handle_txq_event(&mut self, evset: EventSet) -> bool {
        debug!("vsock: TX queue event");

        if evset != EventSet::IN {
            warn!("vsock: txq unexpected event {:?}", evset);
            METRICS.vsock.tx_queue_event_fails.inc();
            return false;
        }

        let mut raise_irq = false;
        if let Err(err) = self.queue_events[TXQ_INDEX].read() {
            error!("Failed to get vsock tx queue event: {:?}", err);
            METRICS.vsock.tx_queue_event_fails.inc();
        } else {
            raise_irq |= self.process_tx();
            METRICS.vsock.tx_queue_event_count.inc();
            // The backend may have queued up responses to the packets we sent during
            // TX queue processing. If that happened, we need to fetch those responses
            // and place them into RX buffers.
            if self.backend.has_pending_rx() {
                raise_irq |= self.process_rx();
            }
        }
        raise_irq
    }

    pub fn handle_evq_event(&mut self, evset: EventSet) -> bool {
        debug!("vsock: event queue event");

        if evset != EventSet::IN {
            warn!("vsock: evq unexpected event {:?}", evset);
            METRICS.vsock.ev_queue_event_fails.inc();
            return false;
        }

        if let Err(err) = self.queue_events[EVQ_INDEX].read() {
            error!("Failed to consume vsock evq event: {:?}", err);
            METRICS.vsock.ev_queue_event_fails.inc();
        }
        false
    }

    pub fn notify_backend(&mut self, evset: EventSet) -> bool {
        debug!("vsock: backend event");

        self.backend.notify(evset);
        // After the backend has been kicked, it might've freed up some resources, so we
        // can attempt to send it more data to process.
        // In particular, if `self.backend.send_pkt()` halted the TX queue processing (by
        // reurning an error) at some point in the past, now is the time to try walking the
        // TX queue again.
        let mut raise_irq = self.process_tx();
        if self.backend.has_pending_rx() {
            raise_irq |= self.process_rx();
        }
        raise_irq
    }

    fn register_runtime_events(&self, ops: &mut EventOps) {
        if let Err(err) = ops.add(Events::new(&self.queue_events[RXQ_INDEX], EventSet::IN)) {
            error!("Failed to register rx queue event: {}", err);
        }
        if let Err(err) = ops.add(Events::new(&self.queue_events[TXQ_INDEX], EventSet::IN)) {
            error!("Failed to register tx queue event: {}", err);
        }
        if let Err(err) = ops.add(Events::new(&self.queue_events[EVQ_INDEX], EventSet::IN)) {
            error!("Failed to register ev queue event: {}", err);
        }
        if let Err(err) = ops.add(Events::new(&self.backend, self.backend.get_polled_evset())) {
            error!("Failed to register vsock backend event: {}", err);
        }
    }

    fn register_activate_event(&self, ops: &mut EventOps) {
        if let Err(err) = ops.add(Events::new(&self.activate_evt, EventSet::IN)) {
            error!("Failed to register activate event: {}", err);
        }
    }

    fn handle_activate_event(&self, ops: &mut EventOps) {
        debug!("vsock: activate event");
        if let Err(err) = self.activate_evt.read() {
            error!("Failed to consume net activate event: {:?}", err);
        }
        self.register_runtime_events(ops);
        if let Err(err) = ops.remove(Events::new(&self.activate_evt, EventSet::IN)) {
            error!("Failed to un-register activate event: {}", err);
        }
    }
}

impl<B> MutEventSubscriber for Vsock<B>
where
    B: VsockBackend + 'static,
{
    fn process(&mut self, event: Events, ops: &mut EventOps) {
        let source = event.fd();
        let evset = event.event_set();
        let rxq = self.queue_events[RXQ_INDEX].as_raw_fd();
        let txq = self.queue_events[TXQ_INDEX].as_raw_fd();
        let evq = self.queue_events[EVQ_INDEX].as_raw_fd();
        let backend = self.backend.as_raw_fd();
        let activate_evt = self.activate_evt.as_raw_fd();

        if self.is_activated() {
            let mut raise_irq = false;
            match source {
                _ if source == rxq => raise_irq = self.handle_rxq_event(evset),
                _ if source == txq => raise_irq = self.handle_txq_event(evset),
                _ if source == evq => raise_irq = self.handle_evq_event(evset),
                _ if source == backend => {
                    raise_irq = self.notify_backend(evset);
                }
                _ if source == activate_evt => {
                    self.handle_activate_event(ops);
                }
                _ => warn!("Unexpected vsock event received: {:?}", source),
            }
            if raise_irq {
                self.signal_used_queue().unwrap_or_default();
            }
        } else {
            warn!(
                "Vsock: The device is not yet activated. Spurious event received: {:?}",
                source
            );
        }
    }

    fn init(&mut self, ops: &mut EventOps) {
        // This function can be called during different points in the device lifetime:
        //  - shortly after device creation,
        //  - on device activation (is-activated already true at this point),
        //  - on device restore from snapshot.
        if self.is_activated() {
            self.register_runtime_events(ops);
        } else {
            self.register_activate_event(ops);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use event_manager::{EventManager, SubscriberOps};
    use utils::vm_memory::Bytes;

    use super::super::*;
    use super::*;
    use crate::devices::virtio::vsock::packet::VSOCK_PKT_HDR_SIZE;
    use crate::devices::virtio::vsock::test_utils::{EventHandlerContext, TestContext};

    #[test]
    fn test_txq_event() {
        // Test case:
        // - the driver has something to send (there's data in the TX queue); and
        // - the backend has no pending RX data.
        {
            let test_ctx = TestContext::new();
            let mut ctx = test_ctx.create_event_handler_context();
            ctx.mock_activate(test_ctx.mem.clone());

            ctx.device.backend.set_pending_rx(false);
            ctx.signal_txq_event();

            // The available TX descriptor should have been used.
            assert_eq!(ctx.guest_txvq.used.idx.get(), 1);
            // The available RX descriptor should be untouched.
            assert_eq!(ctx.guest_rxvq.used.idx.get(), 0);
        }

        // Test case:
        // - the driver has something to send (there's data in the TX queue); and
        // - the backend also has some pending RX data.
        {
            let test_ctx = TestContext::new();
            let mut ctx = test_ctx.create_event_handler_context();
            ctx.mock_activate(test_ctx.mem.clone());

            ctx.device.backend.set_pending_rx(true);
            ctx.signal_txq_event();

            // Both available RX and TX descriptors should have been used.
            assert_eq!(ctx.guest_txvq.used.idx.get(), 1);
            assert_eq!(ctx.guest_rxvq.used.idx.get(), 1);
        }

        // Test case:
        // - the driver has something to send (there's data in the TX queue); and
        // - the backend errors out and cannot process the TX queue.
        {
            let test_ctx = TestContext::new();
            let mut ctx = test_ctx.create_event_handler_context();
            ctx.mock_activate(test_ctx.mem.clone());

            ctx.device.backend.set_pending_rx(false);
            ctx.device.backend.set_tx_err(Some(VsockError::NoData));
            ctx.signal_txq_event();

            // Both RX and TX queues should be untouched.
            assert_eq!(ctx.guest_txvq.used.idx.get(), 0);
            assert_eq!(ctx.guest_rxvq.used.idx.get(), 0);
        }

        // Test case:
        // - the driver supplied a malformed TX buffer.
        {
            let test_ctx = TestContext::new();
            let mut ctx = test_ctx.create_event_handler_context();
            ctx.mock_activate(test_ctx.mem.clone());

            // Invalidate the packet header descriptor, by setting its length to 0.
            ctx.guest_txvq.dtable[0].len.set(0);
            ctx.signal_txq_event();

            // The available descriptor should have been consumed, but no packet should have
            // reached the backend.
            assert_eq!(ctx.guest_txvq.used.idx.get(), 1);
            assert_eq!(ctx.device.backend.tx_ok_cnt, 0);
        }

        // Test case: spurious TXQ_EVENT.
        {
            let test_ctx = TestContext::new();
            let mut ctx = test_ctx.create_event_handler_context();
            ctx.mock_activate(test_ctx.mem.clone());

            assert!(!ctx.device.handle_txq_event(EventSet::IN));
        }
    }

    #[test]
    fn test_rxq_event() {
        // Test case:
        // - there is pending RX data in the backend; and
        // - the driver makes RX buffers available; and
        // - the backend successfully places its RX data into the queue.
        {
            let test_ctx = TestContext::new();
            let mut ctx = test_ctx.create_event_handler_context();
            ctx.mock_activate(test_ctx.mem.clone());

            ctx.device.backend.set_pending_rx(true);
            ctx.device.backend.set_rx_err(Some(VsockError::NoData));
            ctx.signal_rxq_event();

            // The available RX buffer should've been left untouched.
            assert_eq!(ctx.guest_rxvq.used.idx.get(), 0);
        }

        // Test case:
        // - there is pending RX data in the backend; and
        // - the driver makes RX buffers available; and
        // - the backend errors out, when attempting to receive data.
        {
            let test_ctx = TestContext::new();
            let mut ctx = test_ctx.create_event_handler_context();
            ctx.mock_activate(test_ctx.mem.clone());

            ctx.device.backend.set_pending_rx(true);
            ctx.signal_rxq_event();

            // The available RX buffer should have been used.
            assert_eq!(ctx.guest_rxvq.used.idx.get(), 1);
        }

        // Test case: the driver provided a malformed RX descriptor chain.
        {
            let test_ctx = TestContext::new();
            let mut ctx = test_ctx.create_event_handler_context();
            ctx.mock_activate(test_ctx.mem.clone());

            // Invalidate the packet header descriptor, by setting its length to 0.
            ctx.guest_rxvq.dtable[0].len.set(0);

            // The chain should've been processed, without employing the backend.
            assert!(ctx.device.process_rx());
            assert_eq!(ctx.guest_rxvq.used.idx.get(), 1);
            assert_eq!(ctx.device.backend.rx_ok_cnt, 0);
        }

        // Test case: spurious RXQ_EVENT.
        {
            let test_ctx = TestContext::new();
            let mut ctx = test_ctx.create_event_handler_context();
            ctx.mock_activate(test_ctx.mem.clone());
            ctx.device.backend.set_pending_rx(false);
            assert!(!ctx.device.handle_rxq_event(EventSet::IN));
        }
    }

    #[test]
    fn test_evq_event() {
        // Test case: spurious EVQ_EVENT.
        {
            let test_ctx = TestContext::new();
            let mut ctx = test_ctx.create_event_handler_context();
            ctx.device.backend.set_pending_rx(false);
            assert!(!ctx.device.handle_evq_event(EventSet::IN));
        }
    }

    #[test]
    fn test_backend_event() {
        // Test case:
        // - a backend event is received; and
        // - the backend has pending RX data.
        {
            let test_ctx = TestContext::new();
            let mut ctx = test_ctx.create_event_handler_context();
            ctx.mock_activate(test_ctx.mem.clone());

            ctx.device.backend.set_pending_rx(true);
            ctx.device.notify_backend(EventSet::IN);

            // The backend should've received this event.
            assert_eq!(ctx.device.backend.evset, Some(EventSet::IN));
            // TX queue processing should've been triggered.
            assert_eq!(ctx.guest_txvq.used.idx.get(), 1);
            // RX queue processing should've been triggered.
            assert_eq!(ctx.guest_rxvq.used.idx.get(), 1);
        }

        // Test case:
        // - a backend event is received; and
        // - the backend doesn't have any pending RX data.
        {
            let test_ctx = TestContext::new();
            let mut ctx = test_ctx.create_event_handler_context();
            ctx.mock_activate(test_ctx.mem.clone());

            ctx.device.backend.set_pending_rx(false);
            ctx.device.notify_backend(EventSet::IN);

            // The backend should've received this event.
            assert_eq!(ctx.device.backend.evset, Some(EventSet::IN));
            // TX queue processing should've been triggered.
            assert_eq!(ctx.guest_txvq.used.idx.get(), 1);
            // The RX queue should've been left untouched.
            assert_eq!(ctx.guest_rxvq.used.idx.get(), 0);
        }
    }

    // Creates an epoll handler context and attempts to assemble a VsockPkt from the descriptor
    // chains available on the rx and tx virtqueues, but first it will set the addr and len
    // of the descriptor specified by desc_idx to the provided values. We are only using this
    // function for testing error cases, so the asserts always expect is_err() to be true. When
    // desc_idx = 0 we are altering the header (first descriptor in the chain), and when
    // desc_idx = 1 we are altering the packet buffer.
    fn vsock_bof_helper(test_ctx: &mut TestContext, desc_idx: usize, addr: u64, len: u32) {
        use utils::vm_memory::GuestAddress;

        assert!(desc_idx <= 1);

        {
            let mut ctx = test_ctx.create_event_handler_context();
            ctx.guest_rxvq.dtable[desc_idx].addr.set(addr);
            ctx.guest_rxvq.dtable[desc_idx].len.set(len);
            // If the descriptor chain is already declared invalid, there's no reason to assemble
            // a packet.
            if let Some(rx_desc) = ctx.device.queues[RXQ_INDEX].pop(&test_ctx.mem) {
                assert!(VsockPacket::from_rx_virtq_head(&rx_desc).is_err());
            }
        }

        {
            let mut ctx = test_ctx.create_event_handler_context();

            // When modifiyng the buffer descriptor, make sure the len field is altered in the
            // vsock packet header descriptor as well.
            if desc_idx == 1 {
                // The vsock packet len field has offset 24 in the header.
                let hdr_len_addr = GuestAddress(ctx.guest_txvq.dtable[0].addr.get() + 24);
                test_ctx
                    .mem
                    .write_obj(len.to_le_bytes(), hdr_len_addr)
                    .unwrap();
            }

            ctx.guest_txvq.dtable[desc_idx].addr.set(addr);
            ctx.guest_txvq.dtable[desc_idx].len.set(len);

            if let Some(tx_desc) = ctx.device.queues[TXQ_INDEX].pop(&test_ctx.mem) {
                assert!(VsockPacket::from_tx_virtq_head(&tx_desc).is_err());
            }
        }
    }

    #[test]
    fn test_vsock_bof() {
        use utils::vm_memory::GuestAddress;

        const GAP_SIZE: usize = 768 << 20;
        const FIRST_AFTER_GAP: usize = 1 << 32;
        const GAP_START_ADDR: usize = FIRST_AFTER_GAP - GAP_SIZE;
        const MIB: usize = 1 << 20;

        let mut test_ctx = TestContext::new();
        test_ctx.mem = utils::vm_memory::test_utils::create_anon_guest_memory(
            &[
                (GuestAddress(0), 8 * MIB),
                (GuestAddress((GAP_START_ADDR - MIB) as u64), MIB),
                (GuestAddress(FIRST_AFTER_GAP as u64), MIB),
            ],
            false,
        )
        .unwrap();

        // The default configured descriptor chains are valid.
        {
            let mut ctx = test_ctx.create_event_handler_context();
            let rx_desc = ctx.device.queues[RXQ_INDEX].pop(&test_ctx.mem).unwrap();
            assert!(VsockPacket::from_rx_virtq_head(&rx_desc).is_ok());
        }

        {
            let mut ctx = test_ctx.create_event_handler_context();
            let tx_desc = ctx.device.queues[TXQ_INDEX].pop(&test_ctx.mem).unwrap();
            assert!(VsockPacket::from_tx_virtq_head(&tx_desc).is_ok());
        }

        // Let's check what happens when the header descriptor is right before the gap.
        vsock_bof_helper(
            &mut test_ctx,
            0,
            GAP_START_ADDR as u64 - 1,
            VSOCK_PKT_HDR_SIZE as u32,
        );

        // Let's check what happens when the buffer descriptor crosses into the gap, but does
        // not go past its right edge.
        vsock_bof_helper(
            &mut test_ctx,
            1,
            GAP_START_ADDR as u64 - 4,
            GAP_SIZE as u32 + 4,
        );

        // Let's modify the buffer descriptor addr and len such that it crosses over the MMIO gap,
        // and check we cannot assemble the VsockPkts.
        vsock_bof_helper(
            &mut test_ctx,
            1,
            GAP_START_ADDR as u64 - 4,
            GAP_SIZE as u32 + 100,
        );
    }

    #[test]
    fn test_event_handler() {
        let mut event_manager = EventManager::new().unwrap();
        let test_ctx = TestContext::new();
        let EventHandlerContext {
            device,
            guest_rxvq,
            guest_txvq,
            ..
        } = test_ctx.create_event_handler_context();

        let vsock = Arc::new(Mutex::new(device));
        let _id = event_manager.add_subscriber(vsock.clone());

        // Push a queue event
        // - the driver has something to send (there's data in the TX queue); and
        // - the backend also has some pending RX data.
        {
            let mut device = vsock.lock().unwrap();
            device.backend.set_pending_rx(true);
            device.queue_events[TXQ_INDEX].write(1).unwrap();
        }

        // EventManager should report no events since vsock has only registered
        // its activation event so far (even though there is also a queue event pending).
        let ev_count = event_manager.run_with_timeout(50).unwrap();
        assert_eq!(ev_count, 0);

        // Manually force a queue event and check it's ignored pre-activation.
        {
            let device = vsock.lock().unwrap();

            // Artificially push event.
            device.queue_events[TXQ_INDEX].write(1).unwrap();
            let ev_count = event_manager.run_with_timeout(50).unwrap();
            assert_eq!(ev_count, 0);

            // Both available RX and TX descriptors should be untouched.
            assert_eq!(guest_rxvq.used.idx.get(), 0);
            assert_eq!(guest_txvq.used.idx.get(), 0);
        }

        // Now activate the device.
        vsock
            .lock()
            .unwrap()
            .activate(test_ctx.mem.clone())
            .unwrap();
        // Process the activate event.
        let ev_count = event_manager.run_with_timeout(50).unwrap();
        assert_eq!(ev_count, 1);

        // Handle the previously pushed queue event through EventManager.
        {
            let ev_count = event_manager
                .run_with_timeout(100)
                .expect("Metrics event timeout or error.");
            assert_eq!(ev_count, 1);
            // Both available RX and TX descriptors should have been used.
            assert_eq!(guest_rxvq.used.idx.get(), 1);
            assert_eq!(guest_txvq.used.idx.get(), 1);
        }
    }
}
