// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

//! Contains support for parsing and constructing MAC addresses
//! More information about MAC addresses can be found [here]
//!
//! [here]: https://en.wikipedia.org/wiki/MAC_address

use std::fmt;
use std::result::Result;

use serde::de::{Deserialize, Deserializer, Error};
use serde::ser::{Serialize, Serializer};
use versionize::{VersionMap, Versionize, VersionizeResult};
use versionize_derive::Versionize;

/// The number of tuples (the ones separated by ":") contained in a MAC address.
pub const MAC_ADDR_LEN: usize = 6;

/// Represents a MAC address
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Versionize)]
/// Representation of a MAC address.
pub struct MacAddr {
    bytes: [u8; MAC_ADDR_LEN],
}

impl fmt::Display for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let b = &self.bytes;
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5]
        )
    }
}

impl From<[u8; 6]> for MacAddr {
    fn from(bytes: [u8; 6]) -> Self {
        Self { bytes }
    }
}

impl From<MacAddr> for [u8; 6] {
    fn from(mac: MacAddr) -> Self {
        mac.bytes
    }
}

impl MacAddr {
    /// Try to turn a `&str` into a `MacAddr` object. The method will return the `str` that failed
    /// to be parsed.
    /// # Arguments
    ///
    /// * `s` - reference that can be converted to &str.
    /// # Example
    ///
    /// ```
    /// use self::utils::net::mac::MacAddr;
    /// MacAddr::parse_str("12:34:56:78:9a:BC").unwrap();
    /// ```
    pub fn parse_str<S>(s: &S) -> Result<MacAddr, &str>
    where
        S: AsRef<str> + ?Sized,
    {
        let v: Vec<&str> = s.as_ref().split(':').collect();
        let mut bytes = [0u8; MAC_ADDR_LEN];

        if v.len() != MAC_ADDR_LEN {
            return Err(s.as_ref());
        }

        for i in 0..MAC_ADDR_LEN {
            if v[i].len() != 2 {
                return Err(s.as_ref());
            }
            bytes[i] = u8::from_str_radix(v[i], 16).map_err(|_| s.as_ref())?;
        }

        Ok(MacAddr { bytes })
    }

    /// Create a `MacAddr` from a slice.
    /// Does not check whether `src.len()` == `MAC_ADDR_LEN`.
    /// # Arguments
    ///
    /// * `src` - slice from which to copy MAC address content.
    /// # Example
    ///
    /// ```
    /// use self::utils::net::mac::MacAddr;
    /// let mac = MacAddr::from_bytes_unchecked(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
    /// println!("{}", mac.to_string());
    /// ```
    #[inline]
    pub fn from_bytes_unchecked(src: &[u8]) -> MacAddr {
        // TODO: using something like std::mem::uninitialized could avoid the extra initialization,
        // if this ever becomes a performance bottleneck.
        let mut bytes = [0u8; MAC_ADDR_LEN];
        bytes[..].copy_from_slice(src);

        MacAddr { bytes }
    }

    /// Return the underlying content of this `MacAddr` in bytes.
    /// # Example
    ///
    /// ```
    /// use self::utils::net::mac::MacAddr;
    /// let mac = MacAddr::from([0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
    /// assert_eq!([0x01, 0x02, 0x03, 0x04, 0x05, 0x06], mac.get_bytes());
    /// ```
    #[inline]
    pub fn get_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl Serialize for MacAddr {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        Serialize::serialize(&self.to_string(), serializer)
    }
}

impl<'de> Deserialize<'de> for MacAddr {
    fn deserialize<D>(deserializer: D) -> Result<MacAddr, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = <std::string::String as Deserialize>::deserialize(deserializer)?;
        MacAddr::parse_str(&s).map_err(|_| D::Error::custom("The provided MAC address is invalid."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mac_addr() {
        // too long
        assert!(MacAddr::parse_str("aa:aa:aa:aa:aa:aa:aa").is_err());

        // invalid hex
        assert!(MacAddr::parse_str("aa:aa:aa:aa:aa:ax").is_err());

        // single digit mac address component should be invalid
        assert!(MacAddr::parse_str("aa:aa:aa:aa:aa:b").is_err());

        // components with more than two digits should also be invalid
        assert!(MacAddr::parse_str("aa:aa:aa:aa:aa:bbb").is_err());

        let mac = MacAddr::parse_str("12:34:56:78:9a:BC").unwrap();

        println!("parsed MAC address: {}", mac);

        let bytes = mac.get_bytes();
        assert_eq!(bytes, [0x12u8, 0x34, 0x56, 0x78, 0x9a, 0xbc]);
    }

    #[test]
    fn test_mac_addr_serialization_and_deserialization() {
        let mac: MacAddr =
            serde_json::from_str("\"12:34:56:78:9a:bc\"").expect("MacAddr deserialization failed.");

        let bytes = mac.get_bytes();
        assert_eq!(bytes, [0x12u8, 0x34, 0x56, 0x78, 0x9a, 0xbc]);

        let s = serde_json::to_string(&mac).expect("MacAddr serialization failed.");
        assert_eq!(s, "\"12:34:56:78:9a:bc\"");
    }
}
