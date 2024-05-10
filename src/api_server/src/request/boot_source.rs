// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use logger::{IncMetric, METRICS};
use vmm::vmm_config::boot_source::BootSourceConfig;

use super::super::VmmAction;
use crate::parsed_request::{Error, ParsedRequest};
use crate::request::Body;

pub(crate) fn parse_put_boot_source(body: &Body) -> Result<ParsedRequest, Error> {
    METRICS.put_api_requests.boot_source_count.inc();
    Ok(ParsedRequest::new_sync(VmmAction::ConfigureBootSource(
        serde_json::from_slice::<BootSourceConfig>(body.raw()).map_err(|err| {
            METRICS.put_api_requests.boot_source_fails.inc();
            err
        })?,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_boot_request() {
        assert!(parse_put_boot_source(&Body::new("invalid_payload")).is_err());

        let body = r#"{
                "kernel_image_path": "/foo/bar",
                "initrd_path": "/bar/foo",
                "boot_args": "foobar"
              }"#;
        let same_body = BootSourceConfig {
            kernel_image_path: String::from("/foo/bar"),
            initrd_path: Some(String::from("/bar/foo")),
            boot_args: Some(String::from("foobar")),
        };
        let result = parse_put_boot_source(&Body::new(body));
        assert!(result.is_ok());
        let parsed_req = result.unwrap_or_else(|_e| panic!("Failed test."));

        assert!(parsed_req == ParsedRequest::new_sync(VmmAction::ConfigureBootSource(same_body)));
    }
}
