// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use logger::{IncMetric, METRICS};
use vmm::vmm_config::net::{NetworkInterfaceConfig, NetworkInterfaceUpdateConfig};

use super::super::VmmAction;
use crate::parsed_request::{checked_id, Error, ParsedRequest};
use crate::request::{Body, StatusCode};

pub(crate) fn parse_put_net(
    body: &Body,
    id_from_path: Option<&&str>,
) -> Result<ParsedRequest, Error> {
    METRICS.put_api_requests.network_count.inc();
    let id = if let Some(id) = id_from_path {
        checked_id(id)?
    } else {
        METRICS.put_api_requests.network_fails.inc();
        return Err(Error::EmptyID);
    };

    let netif = serde_json::from_slice::<NetworkInterfaceConfig>(body.raw()).map_err(|err| {
        METRICS.put_api_requests.network_fails.inc();
        err
    })?;
    if id != netif.iface_id.as_str() {
        METRICS.put_api_requests.network_fails.inc();
        return Err(Error::Generic(
            StatusCode::BadRequest,
            format!(
                "The id from the path [{}] does not match the id from the body [{}]!",
                id,
                netif.iface_id.as_str()
            ),
        ));
    }
    Ok(ParsedRequest::new_sync(VmmAction::InsertNetworkDevice(
        netif,
    )))
}

pub(crate) fn parse_patch_net(
    body: &Body,
    id_from_path: Option<&&str>,
) -> Result<ParsedRequest, Error> {
    METRICS.patch_api_requests.network_count.inc();
    let id = if let Some(id) = id_from_path {
        checked_id(id)?
    } else {
        METRICS.patch_api_requests.network_count.inc();
        return Err(Error::EmptyID);
    };

    let netif =
        serde_json::from_slice::<NetworkInterfaceUpdateConfig>(body.raw()).map_err(|err| {
            METRICS.patch_api_requests.network_fails.inc();
            err
        })?;
    if id != netif.iface_id {
        METRICS.patch_api_requests.network_count.inc();
        return Err(Error::Generic(
            StatusCode::BadRequest,
            format!(
                "The id from the path [{}] does not match the id from the body [{}]!",
                id,
                netif.iface_id.as_str()
            ),
        ));
    }
    Ok(ParsedRequest::new_sync(VmmAction::UpdateNetworkInterface(
        netif,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsed_request::tests::vmm_action_from_request;

    #[test]
    fn test_parse_put_net_request() {
        let body = r#"{
                "iface_id": "foo",
                "host_dev_name": "bar",
                "guest_mac": "12:34:56:78:9A:BC"
              }"#;
        // 1. Exercise infamous "The id from the path does not match id from the body!".
        assert!(parse_put_net(&Body::new(body), Some(&"bar")).is_err());
        // 2. The `id_from_path` cannot be None.
        assert!(parse_put_net(&Body::new(body), None).is_err());

        // 3. Success case.
        let netif_clone = serde_json::from_str::<NetworkInterfaceConfig>(body).unwrap();
        match vmm_action_from_request(parse_put_net(&Body::new(body), Some(&"foo")).unwrap()) {
            VmmAction::InsertNetworkDevice(netif) => assert_eq!(netif, netif_clone),
            _ => panic!("Test failed."),
        }

        // 4. Serde error for invalid field (bytes instead of bandwidth).
        let body = r#"
        {
            "iface_id": "foo",
            "rx_rate_limiter": {
                "bytes": {
                    "size": 62500,
                    "refill_time": 1000
                }
            },
            "tx_rate_limiter": {
                "bytes": {
                    "size": 62500,
                    "refill_time": 1000
                }
            }
        }"#;

        assert!(parse_put_net(&Body::new(body), Some(&"foo")).is_err());
    }

    #[test]
    fn test_parse_patch_net_request() {
        let body = r#"{
                "iface_id": "foo",
                "rx_rate_limiter": {
                },
                "tx_rate_limiter": {
                }
        }"#;
        // 1. Exercise infamous "The id from the path does not match id from the body!".
        assert!(parse_patch_net(&Body::new(body), Some(&"bar")).is_err());
        // 2. The `id_from_path` cannot be None.
        assert!(parse_patch_net(&Body::new(body), None).is_err());

        // 3. Success case.
        let netif_clone = serde_json::from_str::<NetworkInterfaceUpdateConfig>(body).unwrap();
        match vmm_action_from_request(parse_patch_net(&Body::new(body), Some(&"foo")).unwrap()) {
            VmmAction::UpdateNetworkInterface(netif) => assert_eq!(netif, netif_clone),
            _ => panic!("Test failed."),
        }

        // 4. Serde error for invalid field (bytes instead of bandwidth).
        let body = r#"
        {
            "iface_id": "foo",
            "rx_rate_limiter": {
                "bytes": {
                    "size": 62500,
                    "refill_time": 1000
                }
            },
            "tx_rate_limiter": {
                "bytes": {
                    "size": 62500,
                    "refill_time": 1000
                }
            }
        }"#;
        assert!(parse_patch_net(&Body::new(body), Some(&"foo")).is_err());
    }
}
