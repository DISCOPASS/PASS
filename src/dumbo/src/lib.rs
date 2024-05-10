// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

#![deny(missing_docs)]

//! Provides helper logic for parsing and writing protocol data units, and minimalist
//! implementations of a TCP listener, a TCP connection, and an HTTP/1.1 server.
pub mod pdu;
pub mod tcp;

use std::ops::Index;

use utils::net::mac::MacAddr;

pub use crate::pdu::arp::{EthIPv4ArpFrame, ETH_IPV4_FRAME_LEN};
pub use crate::pdu::ethernet::{
    EthernetFrame, ETHERTYPE_ARP, ETHERTYPE_IPV4, PAYLOAD_OFFSET as ETHERNET_PAYLOAD_OFFSET,
};
pub use crate::pdu::ipv4::{IPv4Packet, PROTOCOL_TCP, PROTOCOL_UDP};
pub use crate::pdu::udp::{UdpDatagram, UDP_HEADER_SIZE};

/// Represents a generalization of a borrowed `[u8]` slice.
#[allow(clippy::len_without_is_empty)]
pub trait ByteBuffer: Index<usize, Output = u8> {
    /// Returns the length of the buffer.
    fn len(&self) -> usize;

    /// Reads `buf.len()` bytes from `self` into `buf`, starting at `offset`.
    ///
    /// # Panics
    ///
    /// Panics if `offset + buf.len()` > `self.len()`.
    fn read_to_slice(&self, offset: usize, buf: &mut [u8]);
}

impl ByteBuffer for [u8] {
    #[inline]
    fn len(&self) -> usize {
        self.len()
    }

    #[inline]
    fn read_to_slice(&self, offset: usize, buf: &mut [u8]) {
        let buf_len = buf.len();
        buf.copy_from_slice(&self[offset..offset + buf_len]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bb_len<T: ByteBuffer + ?Sized>(buf: &T) -> usize {
        buf.len()
    }

    fn bb_is_empty<T: ByteBuffer + ?Sized>(buf: &T) -> bool {
        buf.len() == 0
    }

    fn bb_read_from_1<T: ByteBuffer + ?Sized>(src: &T, dst: &mut [u8]) {
        src.read_to_slice(1, dst);
    }

    #[test]
    fn test_u8_byte_buffer() {
        let a = [1u8, 2, 3];
        let mut b = [0u8; 2];
        assert_eq!(bb_len(a.as_ref()), a.len());
        assert!(!bb_is_empty(a.as_ref()));
        bb_read_from_1(a.as_ref(), b.as_mut());
        assert_eq!(b, [2, 3]);
    }
}
