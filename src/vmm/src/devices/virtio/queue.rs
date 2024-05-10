// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use std::cmp::min;
use std::num::Wrapping;
use std::sync::atomic::{fence, Ordering};

use logger::error;
use utils::vm_memory::{
    Address, ByteValued, Bytes, GuestAddress, GuestMemory, GuestMemoryError, GuestMemoryMmap,
};

pub(super) const VIRTQ_DESC_F_NEXT: u16 = 0x1;
pub(super) const VIRTQ_DESC_F_WRITE: u16 = 0x2;

// GuestMemoryMmap::read_obj_from_addr() will be used to fetch the descriptor,
// which has an explicit constraint that the entire descriptor doesn't
// cross the page boundary. Otherwise the descriptor may be splitted into
// two mmap regions which causes failure of GuestMemoryMmap::read_obj_from_addr().
//
// The Virtio Spec 1.0 defines the alignment of VirtIO descriptor is 16 bytes,
// which fulfills the explicit constraint of GuestMemoryMmap::read_obj_from_addr().

#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    /// Descriptor index out of bounds.
    #[error("Descriptor index out of bounds: {0}.")]
    DescIndexOutOfBounds(u16),
    /// Attempted an invalid write into the used ring.
    #[error("Failed to write value into the virtio queue used ring: {0}")]
    UsedRing(#[from] GuestMemoryError),
}

/// A virtio descriptor constraints with C representative.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Descriptor {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

// SAFETY: `Descriptor` is a POD and contains no padding.
unsafe impl ByteValued for Descriptor {}

/// A virtio descriptor chain.
pub struct DescriptorChain<'a> {
    desc_table: GuestAddress,
    queue_size: u16,
    ttl: u16, // used to prevent infinite chain cycles

    /// Reference to guest memory
    pub mem: &'a GuestMemoryMmap,

    /// Index into the descriptor table
    pub index: u16,

    /// Guest physical address of device specific data
    pub addr: GuestAddress,

    /// Length of device specific data
    pub len: u32,

    /// Includes next, write, and indirect bits
    pub flags: u16,

    /// Index into the descriptor table of the next descriptor if flags has
    /// the next bit set
    pub next: u16,
}

impl<'a> DescriptorChain<'a> {
    fn checked_new(
        mem: &GuestMemoryMmap,
        desc_table: GuestAddress,
        queue_size: u16,
        index: u16,
    ) -> Option<DescriptorChain> {
        if index >= queue_size {
            return None;
        }

        let desc_head = mem.checked_offset(desc_table, (index as usize) * 16)?;
        mem.checked_offset(desc_head, 16)?;

        // These reads can't fail unless Guest memory is hopelessly broken.
        let desc = match mem.read_obj::<Descriptor>(desc_head) {
            Ok(ret) => ret,
            Err(err) => {
                // TODO log address
                error!("Failed to read virtio descriptor from memory: {}", err);
                return None;
            }
        };
        let chain = DescriptorChain {
            mem,
            desc_table,
            queue_size,
            ttl: queue_size,
            index,
            addr: GuestAddress(desc.addr),
            len: desc.len,
            flags: desc.flags,
            next: desc.next,
        };

        if chain.is_valid() {
            Some(chain)
        } else {
            None
        }
    }

    fn is_valid(&self) -> bool {
        !self.has_next() || self.next < self.queue_size
    }

    /// Gets if this descriptor chain has another descriptor chain linked after it.
    pub fn has_next(&self) -> bool {
        self.flags & VIRTQ_DESC_F_NEXT != 0 && self.ttl > 1
    }

    /// If the driver designated this as a write only descriptor.
    ///
    /// If this is false, this descriptor is read only.
    /// Write only means the the emulated device can write and the driver can read.
    pub fn is_write_only(&self) -> bool {
        self.flags & VIRTQ_DESC_F_WRITE != 0
    }

    /// Gets the next descriptor in this descriptor chain, if there is one.
    ///
    /// Note that this is distinct from the next descriptor chain returned by `AvailIter`, which is
    /// the head of the next _available_ descriptor chain.
    pub fn next_descriptor(&self) -> Option<DescriptorChain<'a>> {
        if self.has_next() {
            DescriptorChain::checked_new(self.mem, self.desc_table, self.queue_size, self.next).map(
                |mut c| {
                    c.ttl = self.ttl - 1;
                    c
                },
            )
        } else {
            None
        }
    }
}

pub struct DescriptorIterator<'a>(Option<DescriptorChain<'a>>);

impl<'a> IntoIterator for DescriptorChain<'a> {
    type Item = DescriptorChain<'a>;
    type IntoIter = DescriptorIterator<'a>;

    fn into_iter(self) -> Self::IntoIter {
        DescriptorIterator(Some(self))
    }
}

impl<'a> Iterator for DescriptorIterator<'a> {
    type Item = DescriptorChain<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.take().map(|desc| {
            self.0 = desc.next_descriptor();
            desc
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// A virtio queue's parameters.
pub struct Queue {
    /// The maximal size in elements offered by the device
    pub(crate) max_size: u16,

    /// The queue size in elements the driver selected
    pub size: u16,

    /// Indicates if the queue is finished with configuration
    pub ready: bool,

    /// Guest physical address of the descriptor table
    pub desc_table: GuestAddress,

    /// Guest physical address of the available ring
    pub avail_ring: GuestAddress,

    /// Guest physical address of the used ring
    pub used_ring: GuestAddress,

    pub(crate) next_avail: Wrapping<u16>,
    pub(crate) next_used: Wrapping<u16>,

    /// VIRTIO_F_RING_EVENT_IDX negotiated (notification suppression enabled)
    pub(crate) uses_notif_suppression: bool,
    /// The number of added used buffers since last guest kick
    pub(crate) num_added: Wrapping<u16>,
}

#[allow(clippy::len_without_is_empty)]
impl Queue {
    /// Constructs an empty virtio queue with the given `max_size`.
    pub fn new(max_size: u16) -> Queue {
        Queue {
            max_size,
            size: 0,
            ready: false,
            desc_table: GuestAddress(0),
            avail_ring: GuestAddress(0),
            used_ring: GuestAddress(0),
            next_avail: Wrapping(0),
            next_used: Wrapping(0),
            uses_notif_suppression: false,
            num_added: Wrapping(0),
        }
    }

    pub fn get_max_size(&self) -> u16 {
        self.max_size
    }

    /// Return the actual size of the queue, as the driver may not set up a
    /// queue as big as the device allows.
    pub fn actual_size(&self) -> u16 {
        min(self.size, self.max_size)
    }

    pub fn is_valid(&self, mem: &GuestMemoryMmap) -> bool {
        let queue_size = u64::from(self.actual_size());
        let desc_table = self.desc_table;
        let desc_table_size = 16 * queue_size;
        let avail_ring = self.avail_ring;
        let avail_ring_size = 6 + 2 * queue_size;
        let used_ring = self.used_ring;
        let used_ring_size = 6 + 8 * queue_size;
        if !self.ready {
            error!("attempt to use virtio queue that is not marked ready");
            false
        } else if self.size > self.max_size || self.size == 0 || (self.size & (self.size - 1)) != 0
        {
            error!("virtio queue with invalid size: {}", self.size);
            false
        } else if desc_table
            .checked_add(desc_table_size)
            .map_or(true, |v| !mem.address_in_range(v))
        {
            error!(
                "virtio queue descriptor table goes out of bounds: start:0x{:08x} size:0x{:08x}",
                desc_table.raw_value(),
                desc_table_size
            );
            false
        } else if avail_ring
            .checked_add(avail_ring_size)
            .map_or(true, |v| !mem.address_in_range(v))
        {
            error!(
                "virtio queue available ring goes out of bounds: start:0x{:08x} size:0x{:08x}",
                avail_ring.raw_value(),
                avail_ring_size
            );
            false
        } else if used_ring
            .checked_add(used_ring_size)
            .map_or(true, |v| !mem.address_in_range(v))
        {
            error!(
                "virtio queue used ring goes out of bounds: start:0x{:08x} size:0x{:08x}",
                used_ring.raw_value(),
                used_ring_size
            );
            false
        } else if desc_table.raw_value() & 0xf != 0 {
            error!("virtio queue descriptor table breaks alignment constraints");
            false
        } else if avail_ring.raw_value() & 0x1 != 0 {
            error!("virtio queue available ring breaks alignment constraints");
            false
        } else if used_ring.raw_value() & 0x3 != 0 {
            error!("virtio queue used ring breaks alignment constraints");
            false
        } else if self.len(mem) > self.max_size {
            error!(
                "virtio queue number of available descriptors {} is greater than queue max size {}",
                self.len(mem),
                self.max_size
            );
            false
        } else {
            true
        }
    }

    /// Returns the number of yet-to-be-popped descriptor chains in the avail ring.
    fn len(&self, mem: &GuestMemoryMmap) -> u16 {
        (self.avail_idx(mem) - self.next_avail).0
    }

    /// Checks if the driver has made any descriptor chains available in the avail ring.
    pub fn is_empty(&self, mem: &GuestMemoryMmap) -> bool {
        self.len(mem) == 0
    }

    /// Pop the first available descriptor chain from the avail ring.
    pub fn pop<'b>(&mut self, mem: &'b GuestMemoryMmap) -> Option<DescriptorChain<'b>> {
        let len = self.len(mem);
        // The number of descriptor chain heads to process should always
        // be smaller or equal to the queue size, as the driver should
        // never ask the VMM to process a available ring entry more than
        // once. Checking and reporting such incorrect driver behavior
        // can prevent potential hanging and Denial-of-Service from
        // happening on the VMM side.
        if len > self.actual_size() {
            // We are choosing to interrupt execution since this could be a potential malicious
            // driver scenario. This way we also eliminate the risk of repeatedly
            // logging and potentially clogging the microVM through the log system.
            panic!("The number of available virtio descriptors is greater than queue size!");
        }

        if len == 0 {
            return None;
        }

        self.do_pop_unchecked(mem)
    }

    /// Try to pop the first available descriptor chain from the avail ring.
    /// If no descriptor is available, enable notifications.
    pub fn pop_or_enable_notification<'b>(
        &mut self,
        mem: &'b GuestMemoryMmap,
    ) -> Option<DescriptorChain<'b>> {
        if !self.uses_notif_suppression {
            return self.pop(mem);
        }

        if self.try_enable_notification(mem) {
            return None;
        }

        self.do_pop_unchecked(mem)
    }

    /// Pop the first available descriptor chain from the avail ring.
    ///
    /// # Important
    /// This is an internal method that ASSUMES THAT THERE ARE AVAILABLE DESCRIPTORS. Otherwise it
    /// will retrieve a descriptor that contains garbage data (obsolete/empty).
    fn do_pop_unchecked<'b>(&mut self, mem: &'b GuestMemoryMmap) -> Option<DescriptorChain<'b>> {
        // This fence ensures all subsequent reads see the updated driver writes.
        fence(Ordering::Acquire);

        // We'll need to find the first available descriptor, that we haven't yet popped.
        // In a naive notation, that would be:
        // `descriptor_table[avail_ring[next_avail]]`.
        //
        // First, we compute the byte-offset (into `self.avail_ring`) of the index of the next
        // available descriptor. `self.avail_ring` stores the address of a `struct
        // virtq_avail`, as defined by the VirtIO spec:
        //
        // ```C
        // struct virtq_avail {
        //   le16 flags;
        //   le16 idx;
        //   le16 ring[QUEUE_SIZE];
        //   le16 used_event
        // }
        // ```
        //
        // We use `self.next_avail` to store the position, in `ring`, of the next available
        // descriptor index, with a twist: we always only increment `self.next_avail`, so the
        // actual position will be `self.next_avail % self.actual_size()`.
        // We are now looking for the offset of `ring[self.next_avail % self.actual_size()]`.
        // `ring` starts after `flags` and `idx` (4 bytes into `struct virtq_avail`), and holds
        // 2-byte items, so the offset will be:
        let index_offset = 4 + 2 * (self.next_avail.0 % self.actual_size());

        // `self.is_valid()` already performed all the bound checks on the descriptor table
        // and virtq rings, so it's safe to unwrap guest memory reads and to use unchecked
        // offsets.
        let desc_index: u16 = mem
            .read_obj(self.avail_ring.unchecked_add(u64::from(index_offset)))
            .unwrap();

        DescriptorChain::checked_new(mem, self.desc_table, self.actual_size(), desc_index).map(
            |dc| {
                self.next_avail += Wrapping(1);
                dc
            },
        )
    }

    /// Undo the effects of the last `self.pop()` call.
    /// The caller can use this, if it was unable to consume the last popped descriptor chain.
    pub fn undo_pop(&mut self) {
        self.next_avail -= Wrapping(1);
    }

    /// Puts an available descriptor head into the used ring for use by the guest.
    pub fn add_used(
        &mut self,
        mem: &GuestMemoryMmap,
        desc_index: u16,
        len: u32,
    ) -> Result<(), QueueError> {
        if desc_index >= self.actual_size() {
            error!(
                "attempted to add out of bounds descriptor to used ring: {}",
                desc_index
            );
            return Err(QueueError::DescIndexOutOfBounds(desc_index));
        }

        let used_ring = self.used_ring;
        let next_used = u64::from(self.next_used.0 % self.actual_size());
        let used_elem = used_ring.unchecked_add(4 + next_used * 8);

        mem.write_obj(u32::from(desc_index), used_elem)?;

        let len_addr = used_elem.unchecked_add(4);
        mem.write_obj(len, len_addr)?;

        self.num_added += Wrapping(1);
        self.next_used += Wrapping(1);

        // This fence ensures all descriptor writes are visible before the index update is.
        fence(Ordering::Release);

        let next_used_addr = used_ring.unchecked_add(2);
        mem.write_obj(self.next_used.0, next_used_addr)
            .map_err(QueueError::UsedRing)
    }

    /// Fetch the available ring index (`virtq_avail->idx`) from guest memory.
    /// This is written by the driver, to indicate the next slot that will be filled in the avail
    /// ring.
    fn avail_idx(&self, mem: &GuestMemoryMmap) -> Wrapping<u16> {
        // Bound checks for queue inner data have already been performed, at device activation time,
        // via `self.is_valid()`, so it's safe to unwrap and use unchecked offsets here.
        // Note: the `MmioTransport` code ensures that queue addresses cannot be changed by the
        // guest       after device activation, so we can be certain that no change has
        // occurred since the last `self.is_valid()` check.
        let addr = self.avail_ring.unchecked_add(2);
        Wrapping(mem.read_obj::<u16>(addr).unwrap())
    }

    /// Get the value of the used event field of the avail ring.
    #[inline(always)]
    pub fn used_event(&self, mem: &GuestMemoryMmap) -> Wrapping<u16> {
        // We need to find the `used_event` field from the avail ring.
        let used_event_addr = self
            .avail_ring
            .unchecked_add(u64::from(4 + 2 * self.actual_size()));

        Wrapping(mem.read_obj::<u16>(used_event_addr).unwrap())
    }

    /// Helper method that writes `val` to the `avail_event` field of the used ring.
    fn set_avail_event(&mut self, val: u16, mem: &GuestMemoryMmap) {
        let avail_event_addr = self
            .used_ring
            .unchecked_add(u64::from(4 + 8 * self.actual_size()));

        mem.write_obj(val, avail_event_addr).unwrap();
    }

    /// Try to enable notification events from the guest driver. Returns true if notifications were
    /// successfully enabled. Otherwise it means that one or more descriptors can still be consumed
    /// from the available ring and we can't guarantee that there will be a notification. In this
    /// case the caller might want to consume the mentioned descriptors and call this method again.
    pub fn try_enable_notification(&mut self, mem: &GuestMemoryMmap) -> bool {
        // If the device doesn't use notification suppression, we'll continue to get notifications
        // no matter what.
        if !self.uses_notif_suppression {
            return true;
        }

        let len = self.len(mem);
        if len != 0 {
            // The number of descriptor chain heads to process should always
            // be smaller or equal to the queue size.
            if len > self.actual_size() {
                // We are choosing to interrupt execution since this could be a potential malicious
                // driver scenario. This way we also eliminate the risk of
                // repeatedly logging and potentially clogging the microVM through
                // the log system.
                panic!("The number of available virtio descriptors is greater than queue size!");
            }
            return false;
        }

        // Set the next expected avail_idx as avail_event.
        self.set_avail_event(self.next_avail.0, mem);

        // Make sure all subsequent reads are performed after `set_avail_event`.
        fence(Ordering::SeqCst);

        // If the actual avail_idx is different than next_avail one or more descriptors can still
        // be consumed from the available ring.
        self.next_avail.0 == self.avail_idx(mem).0
    }

    /// Enable notification suppression.
    pub fn enable_notif_suppression(&mut self) {
        self.uses_notif_suppression = true;
    }

    /// Check if we need to kick the guest.
    ///
    /// Please note this method has side effects: once it returns `true`, it considers the
    /// driver will actually be notified, and won't return `true` again until the driver
    /// updates `used_event` and/or the notification conditions hold once more.
    ///
    /// This is similar to the `vring_need_event()` method implemented by the Linux kernel.
    pub fn prepare_kick(&mut self, mem: &GuestMemoryMmap) -> bool {
        // If the device doesn't use notification suppression, always return true
        if !self.uses_notif_suppression {
            return true;
        }

        // We need to expose used array entries before checking the used_event.
        fence(Ordering::SeqCst);

        let new = self.next_used;
        let old = self.next_used - self.num_added;
        let used_event = self.used_event(mem);

        self.num_added = Wrapping(0);

        new - used_event - Wrapping(1) < new - old
    }
}

#[cfg(test)]
mod tests {

    use utils::vm_memory::test_utils::create_anon_guest_memory;
    use utils::vm_memory::{GuestAddress, GuestMemoryMmap};

    pub use super::*;
    use crate::devices::virtio::test_utils::{default_mem, single_region_mem, VirtQueue};
    use crate::devices::virtio::QueueError::{DescIndexOutOfBounds, UsedRing};

    impl Queue {
        fn avail_event(&self, mem: &GuestMemoryMmap) -> u16 {
            let avail_event_addr = self
                .used_ring
                .unchecked_add(u64::from(4 + 8 * self.actual_size()));

            mem.read_obj::<u16>(avail_event_addr).unwrap()
        }
    }

    #[test]
    fn test_checked_new_descriptor_chain() {
        let m = &create_anon_guest_memory(
            &[(GuestAddress(0), 0x10000), (GuestAddress(0x20000), 0x2000)],
            false,
        )
        .unwrap();
        let vq = VirtQueue::new(GuestAddress(0), m, 16);

        assert!(vq.end().0 < 0x1000);

        // index >= queue_size
        assert!(DescriptorChain::checked_new(m, vq.dtable_start(), 16, 16).is_none());

        // desc_table address is way off
        assert!(DescriptorChain::checked_new(m, GuestAddress(0x00ff_ffff_ffff), 16, 0).is_none());

        // Let's create an invalid chain.
        {
            // The first desc has a normal len, and the next_descriptor flag is set.
            vq.dtable[0].addr.set(0x1000);
            vq.dtable[0].len.set(0x1000);
            vq.dtable[0].flags.set(VIRTQ_DESC_F_NEXT);
            // .. but the the index of the next descriptor is too large
            vq.dtable[0].next.set(16);

            assert!(DescriptorChain::checked_new(m, vq.dtable_start(), 16, 0).is_none());
        }

        // Finally, let's test an ok chain.
        {
            vq.dtable[0].next.set(1);
            vq.dtable[1].set(0x2000, 0x1000, 0, 0);

            let c = DescriptorChain::checked_new(m, vq.dtable_start(), 16, 0).unwrap();

            assert_eq!(c.mem as *const GuestMemoryMmap, m as *const GuestMemoryMmap);
            assert_eq!(c.desc_table, vq.dtable_start());
            assert_eq!(c.queue_size, 16);
            assert_eq!(c.ttl, c.queue_size);
            assert_eq!(c.index, 0);
            assert_eq!(c.addr, GuestAddress(0x1000));
            assert_eq!(c.len, 0x1000);
            assert_eq!(c.flags, VIRTQ_DESC_F_NEXT);
            assert_eq!(c.next, 1);

            assert!(c.next_descriptor().unwrap().next_descriptor().is_none());
        }
    }

    #[test]
    fn test_queue_validation() {
        let m = &default_mem();
        let vq = VirtQueue::new(GuestAddress(0), m, 16);

        let mut q = vq.create_queue();

        // q is currently valid
        assert!(q.is_valid(m));

        // shouldn't be valid when not marked as ready
        q.ready = false;
        assert!(!q.is_valid(m));
        q.ready = true;

        // or when size > max_size
        q.size = q.max_size << 1;
        assert!(!q.is_valid(m));
        q.size = q.max_size;

        // or when size is 0
        q.size = 0;
        assert!(!q.is_valid(m));
        q.size = q.max_size;

        // or when size is not a power of 2
        q.size = 11;
        assert!(!q.is_valid(m));
        q.size = q.max_size;

        // or when avail_idx - next_avail > max_size
        q.next_avail = Wrapping(5);
        assert!(!q.is_valid(m));
        // avail_ring + 2 is the address of avail_idx in guest mem
        m.write_obj::<u16>(64_u16, q.avail_ring.unchecked_add(2))
            .unwrap();
        assert!(!q.is_valid(m));
        m.write_obj::<u16>(5_u16, q.avail_ring.unchecked_add(2))
            .unwrap();
        q.max_size = 2;
        assert!(!q.is_valid(m));

        // reset dirtied values
        q.max_size = 16;
        q.next_avail = Wrapping(0);
        m.write_obj::<u16>(0, q.avail_ring.unchecked_add(2))
            .unwrap();

        // or if the various addresses are off

        q.desc_table = GuestAddress(0xffff_ffff);
        assert!(!q.is_valid(m));
        q.desc_table = GuestAddress(0x1001);
        assert!(!q.is_valid(m));
        q.desc_table = vq.dtable_start();

        q.avail_ring = GuestAddress(0xffff_ffff);
        assert!(!q.is_valid(m));
        q.avail_ring = GuestAddress(0x1001);
        assert!(!q.is_valid(m));
        q.avail_ring = vq.avail_start();

        q.used_ring = GuestAddress(0xffff_ffff);
        assert!(!q.is_valid(m));
        q.used_ring = GuestAddress(0x1001);
        assert!(!q.is_valid(m));
        q.used_ring = vq.used_start();
    }

    #[test]
    fn test_queue_processing() {
        let m = &default_mem();
        let vq = VirtQueue::new(GuestAddress(0), m, 16);
        let mut q = vq.create_queue();

        q.ready = true;

        // Let's create two simple descriptor chains.

        for j in 0..5 {
            vq.dtable[j].set(
                0x1000 * (j + 1) as u64,
                0x1000,
                VIRTQ_DESC_F_NEXT,
                (j + 1) as u16,
            );
        }

        // the chains are (0, 1) and (2, 3, 4)
        vq.dtable[1].flags.set(0);
        vq.dtable[4].flags.set(0);
        vq.avail.ring[0].set(0);
        vq.avail.ring[1].set(2);
        vq.avail.idx.set(2);

        // We've just set up two chains.
        assert_eq!(q.len(m), 2);

        // The first chain should hold exactly two descriptors.
        let d = q.pop(m).unwrap().next_descriptor().unwrap();
        assert!(!d.has_next());
        assert!(d.next_descriptor().is_none());

        // We popped one chain, so there should be only one left.
        assert_eq!(q.len(m), 1);

        // The next chain holds three descriptors.
        let d = q
            .pop(m)
            .unwrap()
            .next_descriptor()
            .unwrap()
            .next_descriptor()
            .unwrap();
        assert!(!d.has_next());
        assert!(d.next_descriptor().is_none());

        // We've popped both chains, so the queue should be empty.
        assert!(q.is_empty(m));
        assert!(q.pop(m).is_none());

        // Undoing the last pop should let us walk the last chain again.
        q.undo_pop();
        assert_eq!(q.len(m), 1);

        // Walk the last chain again (three descriptors).
        let d = q
            .pop(m)
            .unwrap()
            .next_descriptor()
            .unwrap()
            .next_descriptor()
            .unwrap();
        assert!(!d.has_next());
        assert!(d.next_descriptor().is_none());

        // Undoing the last pop should let us walk the last chain again.
        q.undo_pop();
        assert_eq!(q.len(m), 1);

        // Walk the last chain again (three descriptors) using pop_or_enable_notification().
        let d = q
            .pop_or_enable_notification(m)
            .unwrap()
            .next_descriptor()
            .unwrap()
            .next_descriptor()
            .unwrap();
        assert!(!d.has_next());
        assert!(d.next_descriptor().is_none());

        // There are no more descriptors, but notification suppression is disabled.
        // Calling pop_or_enable_notification() should simply return None.
        assert_eq!(q.avail_event(m), 0);
        assert!(q.pop_or_enable_notification(m).is_none());
        assert_eq!(q.avail_event(m), 0);

        // There are no more descriptors and notification suppression is enabled. Calling
        // pop_or_enable_notification() should enable notifications.
        q.enable_notif_suppression();
        assert!(q.pop_or_enable_notification(m).is_none());
        assert_eq!(q.avail_event(m), 2);
    }

    #[test]
    #[should_panic(
        expected = "The number of available virtio descriptors is greater than queue size!"
    )]
    fn test_invalid_avail_idx_no_notification() {
        // This test ensures constructing a descriptor chain succeeds
        // with valid available ring indexes while it produces an error with invalid
        // indexes.
        // No notification suppression enabled.
        let m = &single_region_mem(0x6000);

        // We set up a queue of size 4.
        let vq = VirtQueue::new(GuestAddress(0), m, 4);
        let mut q = vq.create_queue();

        for j in 0..4 {
            vq.dtable[j].set(
                0x1000 * (j + 1) as u64,
                0x1000,
                VIRTQ_DESC_F_NEXT,
                (j + 1) as u16,
            );
        }

        // Create 2 descriptor chains.
        // the chains are (0, 1) and (2, 3)
        vq.dtable[1].flags.set(0);
        vq.dtable[3].flags.set(0);
        vq.avail.ring[0].set(0);
        vq.avail.ring[1].set(2);
        vq.avail.idx.set(2);

        // We've just set up two chains.
        assert_eq!(q.len(m), 2);

        // We process the first descriptor.
        let d = q.pop(m).unwrap().next_descriptor();
        assert!(matches!(d, Some(x) if !x.has_next()));
        // We confuse the device and set the available index as being 6.
        vq.avail.idx.set(6);

        // We've actually just popped a descriptor so 6 - 1 = 5.
        assert_eq!(q.len(m), 5);

        // However, since the apparent length set by the driver is more than the queue size,
        // we would be running the risk of going through some descriptors more than once.
        // As such, we expect to panic.
        q.pop(m);
    }

    #[test]
    #[should_panic(
        expected = "The number of available virtio descriptors is greater than queue size!"
    )]
    fn test_invalid_avail_idx_with_notification() {
        // This test ensures constructing a descriptor chain succeeds
        // with valid available ring indexes while it produces an error with invalid
        // indexes.
        // Notification suppression is enabled.
        let m = &single_region_mem(0x6000);

        // We set up a queue of size 4.
        let vq = VirtQueue::new(GuestAddress(0), m, 4);
        let mut q = vq.create_queue();

        q.uses_notif_suppression = true;

        // Create 1 descriptor chain of 4.
        for j in 0..4 {
            vq.dtable[j].set(
                0x1000 * (j + 1) as u64,
                0x1000,
                VIRTQ_DESC_F_NEXT,
                (j + 1) as u16,
            );
        }
        // We need to clear the VIRTQ_DESC_F_NEXT for the last descriptor.
        vq.dtable[3].flags.set(0);
        vq.avail.ring[0].set(0);

        // driver sets available index to suspicious value.
        vq.avail.idx.set(6);

        q.pop_or_enable_notification(m);
    }

    #[test]
    fn test_add_used() {
        let m = &default_mem();
        let vq = VirtQueue::new(GuestAddress(0), m, 16);

        let mut q = vq.create_queue();
        assert_eq!(vq.used.idx.get(), 0);

        // Valid queue addresses configuration
        {
            // index too large
            match q.add_used(m, 16, 0x1000) {
                Err(DescIndexOutOfBounds(16)) => (),
                _ => unreachable!(),
            }

            // should be ok
            q.add_used(m, 1, 0x1000).unwrap();
            assert_eq!(vq.used.idx.get(), 1);
            let x = vq.used.ring[0].get();
            assert_eq!(x.id, 1);
            assert_eq!(x.len, 0x1000);
        }

        // Invalid queue addresses configuration
        {
            q.used_ring = GuestAddress(0xffff_ffff);
            // writing descriptor index to this ring address should fail
            match q.add_used(m, 1, 0x1000) {
                Err(UsedRing(GuestMemoryError::InvalidGuestAddress(GuestAddress(
                    0x0001_0000_000B,
                )))) => {}
                _ => unreachable!(),
            }

            q.used_ring = GuestAddress(0xfff0);
            // writing len to this ring address should fail
            match q.add_used(m, 1, 0x1000) {
                Err(UsedRing(GuestMemoryError::InvalidGuestAddress(GuestAddress(0x1_0000)))) => {}
                _ => unreachable!(),
            };
        }
    }

    #[test]
    fn test_used_event() {
        let m = &default_mem();
        let vq = VirtQueue::new(GuestAddress(0), m, 16);

        let q = vq.create_queue();
        assert_eq!(q.used_event(m), Wrapping(0));

        vq.avail.event.set(10);
        assert_eq!(q.used_event(m), Wrapping(10));

        vq.avail.event.set(u16::MAX);
        assert_eq!(q.used_event(m), Wrapping(u16::MAX));
    }

    #[test]
    fn test_set_avail_event() {
        let m = &default_mem();
        let vq = VirtQueue::new(GuestAddress(0), m, 16);

        let mut q = vq.create_queue();
        assert_eq!(vq.used.event.get(), 0);

        q.set_avail_event(10, m);
        assert_eq!(vq.used.event.get(), 10);

        q.set_avail_event(u16::MAX, m);
        assert_eq!(vq.used.event.get(), u16::MAX);
    }

    #[test]
    fn test_needs_kick() {
        let m = &default_mem();
        let vq = VirtQueue::new(GuestAddress(0), m, 16);
        let mut q = vq.create_queue();

        {
            // If the device doesn't have notification suppression support,
            // `needs_notification()` should always return true.
            q.uses_notif_suppression = false;
            for used_idx in 0..10 {
                for used_event in 0..10 {
                    for num_added in 0..10 {
                        q.next_used = Wrapping(used_idx);
                        vq.avail.event.set(used_event);
                        q.num_added = Wrapping(num_added);
                        assert!(q.prepare_kick(m));
                    }
                }
            }
        }

        q.enable_notif_suppression();
        {
            // old used idx < used_event < next_used
            q.next_used = Wrapping(10);
            vq.avail.event.set(6);
            q.num_added = Wrapping(5);
            assert!(q.prepare_kick(m));
        }

        {
            // old used idx = used_event < next_used
            q.next_used = Wrapping(10);
            vq.avail.event.set(6);
            q.num_added = Wrapping(4);
            assert!(q.prepare_kick(m));
        }

        {
            // used_event < old used idx < next_used
            q.next_used = Wrapping(10);
            vq.avail.event.set(6);
            q.num_added = Wrapping(3);
            assert!(!q.prepare_kick(m));
        }
    }

    #[test]
    fn test_try_enable_notification() {
        let m = &default_mem();
        let vq = VirtQueue::new(GuestAddress(0), m, 16);
        let mut q = vq.create_queue();

        q.ready = true;

        // We create a simple descriptor chain
        vq.dtable[0].set(0x1000_u64, 0x1000, 0, 0);
        vq.avail.ring[0].set(0);
        vq.avail.idx.set(1);

        assert_eq!(q.len(m), 1);

        // Notification suppression is disabled. try_enable_notification shouldn't do anything.
        assert!(q.try_enable_notification(m));
        assert_eq!(q.avail_event(m), 0);

        // Enable notification suppression and check again. There is 1 available descriptor chain.
        // Again nothing should happen.
        q.enable_notif_suppression();
        assert!(!q.try_enable_notification(m));
        assert_eq!(q.avail_event(m), 0);

        // Consume the descriptor. avail_event should be modified
        assert!(q.pop(m).is_some());
        assert!(q.try_enable_notification(m));
        assert_eq!(q.avail_event(m), 1);
    }

    #[test]
    fn test_queue_error_display() {
        let err = UsedRing(GuestMemoryError::InvalidGuestAddress(GuestAddress(0)));
        let _ = format!("{}{:?}", err, err);

        let err = DescIndexOutOfBounds(1);
        let _ = format!("{}{:?}", err, err);
    }
}
