// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
use std::path::PathBuf;
use std::collections::HashMap; 
use std::string::String;
use logger::{IncMetric, METRICS,info};
use serde::de::Error as DeserializeError;
use vmm::vmm_config::snapshot::{
    CreateSnapshotParams, LoadSnapshotConfig, LoadSnapshotParams, MemBackendConfig, MemBackendType,
    Vm, VmState,
};

use super::super::VmmAction;
use crate::parsed_request::{Error, ParsedRequest};
use crate::request::{Body, Method, StatusCode};

/// Deprecation message for the `mem_file_path` field.
const LOAD_DEPRECATION_MESSAGE: &str = "PUT /snapshot/load: mem_file_path field is deprecated.";
/// None of the `mem_backend` or `mem_file_path` fields has been specified.
pub const MISSING_FIELD: &str =
    "missing field: either `mem_backend` or `mem_file_path` is required";
/// Both the `mem_backend` and `mem_file_path` fields have been specified.
/// Only specifying one of them is allowed.
pub const TOO_MANY_FIELDS: &str =
    "too many fields: either `mem_backend` or `mem_file_path` exclusively is required";

pub(crate) fn parse_put_snapshot(
    body: &Body,
    request_type_from_path: Option<&&str>,
) -> Result<ParsedRequest, Error> {
    match request_type_from_path {
        Some(&request_type) => match request_type {
            "create" => Ok(ParsedRequest::new_sync(VmmAction::CreateSnapshot(
                serde_json::from_slice::<CreateSnapshotParams>(body.raw())?,
            ))),
            "load" => parse_put_snapshot_load(body),
            _ => Err(Error::InvalidPathMethod(
                format!("/snapshot/{}", request_type),
                Method::Put,
            )),
        },
        None => Err(Error::Generic(
            StatusCode::BadRequest,
            "Missing snapshot operation type.".to_string(),
        )),
    }
}

pub(crate) fn parse_patch_vm_state(body: &Body) -> Result<ParsedRequest, Error> {
    let vm = serde_json::from_slice::<Vm>(body.raw())?;

    match vm.state {
        VmState::Paused => Ok(ParsedRequest::new_sync(VmmAction::Pause)),
        VmState::Resumed => Ok(ParsedRequest::new_sync(VmmAction::Resume)),
    }
}

fn parse_put_snapshot_load(body: &Body) -> Result<ParsedRequest, Error> {
    let snapshot_config = serde_json::from_slice::<LoadSnapshotConfig>(body.raw())?;

    match (&snapshot_config.mem_backend, &snapshot_config.mem_file_path) {
        // Ensure `mem_file_path` and `mem_backend` fields are not present at the same time.
        (Some(_), Some(_)) => {
            return Err(Error::SerdeJson(serde_json::Error::custom(TOO_MANY_FIELDS)))
        }
        // Ensure that one of `mem_file_path` or `mem_backend` fields is always specified.
        (None, None) => return Err(Error::SerdeJson(serde_json::Error::custom(MISSING_FIELD))),
        _ => {}
    }

    // Check for the presence of deprecated `mem_file_path` field and create
    // deprecation message if found.
    let mut deprecation_message = None;
    if snapshot_config.mem_file_path.is_some() {
        // `mem_file_path` field in request is deprecated.
        METRICS.deprecated_api.deprecated_http_api_calls.inc();
        deprecation_message = Some(LOAD_DEPRECATION_MESSAGE);
    }

    // If `mem_file_path` is specified instead of `mem_backend`, we construct the
    // `MemBackendConfig` object from the path specified, with `File` as backend type.
    let mem_backend = match snapshot_config.mem_backend {
        Some(backend_cfg) => backend_cfg,
        None => {
            MemBackendConfig {
                // This is safe to unwrap() because we ensure above that one of the two:
                // either `mem_file_path` or `mem_backend` field is always specified.
                backend_path: snapshot_config.mem_file_path.unwrap(),
                backend_type: MemBackendType::File,
            }
        }
    };
    info!("PASS_debug decode snapshot params and re-encode them...");
    info!("snapshot_path: {:?}", snapshot_config.snapshot_path);
    info!("mem_backend: {:?}", mem_backend);
    let snapshot_params = LoadSnapshotParams {
        snapshot_path: snapshot_config.snapshot_path,
        mem_backend,
        enable_diff_snapshots: snapshot_config.enable_diff_snapshots,
        enable_user_page_faults: false,
        sock_file_path: PathBuf::from("/tmp/PASS.socket"),
        overlay_file_path: PathBuf::from("/tmp/overlay_file"),
        overlay_regions: HashMap::new(),
        ws_file_path: PathBuf::from("/tmp/ws_file"), // Provide an identifier for ws_file_path
        ws_regions: Vec::new(), // Provide an identifier for ws_regions
        load_ws: false, // Provide an identifier for load_ws
        fadvise: "".to_string(), // Provide an identifier for fadvise
        resume_vm: snapshot_config.resume_vm,
    };

    // Construct the `ParsedRequest` object.
    let mut parsed_req = ParsedRequest::new_sync(VmmAction::LoadSnapshot(snapshot_params));

    // If `mem_file_path` was present, set the deprecation message in `parsing_info`.
    if let Some(msg) = deprecation_message {
        parsed_req.parsing_info().append_deprecation_message(msg);
    }

    Ok(parsed_req)
}

#[cfg(test)]
mod tests {
    use vmm::vmm_config::snapshot::{MemBackendConfig, MemBackendType};

    use super::*;
    use crate::parsed_request::tests::{depr_action_from_req, vmm_action_from_request};

    #[test]
    fn test_parse_put_snapshot() {
        use std::path::PathBuf;

        use vmm::vmm_config::snapshot::SnapshotType;

        let mut body = r#"{
                "snapshot_type": "Diff",
                "snapshot_path": "foo",
                "mem_file_path": "bar",
                "version": "0.23.0"
              }"#;

        let mut expected_cfg = CreateSnapshotParams {
            snapshot_type: SnapshotType::Diff,
            snapshot_path: PathBuf::from("foo"),
            mem_file_path: PathBuf::from("bar"),
            version: Some(String::from("0.23.0")),
        };

        match vmm_action_from_request(
            parse_put_snapshot(&Body::new(body), Some(&"create")).unwrap(),
        ) {
            VmmAction::CreateSnapshot(cfg) => assert_eq!(cfg, expected_cfg),
            _ => panic!("Test failed."),
        }

        body = r#"{
                "snapshot_path": "foo",
                "mem_file_path": "bar"
              }"#;

        expected_cfg = CreateSnapshotParams {
            snapshot_type: SnapshotType::Full,
            snapshot_path: PathBuf::from("foo"),
            mem_file_path: PathBuf::from("bar"),
            version: None,
        };

        match vmm_action_from_request(
            parse_put_snapshot(&Body::new(body), Some(&"create")).unwrap(),
        ) {
            VmmAction::CreateSnapshot(cfg) => assert_eq!(cfg, expected_cfg),
            _ => panic!("Test failed."),
        }

        let invalid_body = r#"{
                "invalid_field": "foo",
                "mem_file_path": "bar"
              }"#;

        assert!(parse_put_snapshot(&Body::new(invalid_body), Some(&"create")).is_err());

        body = r#"{
                "snapshot_path": "foo",
                "mem_backend": {
                    "backend_path": "bar",
                    "backend_type": "File"
                }
              }"#;

        let mut expected_cfg = LoadSnapshotParams {
            snapshot_path: PathBuf::from("foo"),
            mem_backend: MemBackendConfig {
                backend_path: PathBuf::from("bar"),
                backend_type: MemBackendType::File,
            },
            enable_diff_snapshots: false,
            resume_vm: false,
        };

        let mut parsed_request = parse_put_snapshot(&Body::new(body), Some(&"load")).unwrap();
        assert!(parsed_request
            .parsing_info()
            .take_deprecation_message()
            .is_none());

        match vmm_action_from_request(parsed_request) {
            VmmAction::LoadSnapshot(cfg) => assert_eq!(cfg, expected_cfg),
            _ => panic!("Test failed."),
        }

        body = r#"{
                "snapshot_path": "foo",
                "mem_backend": {
                    "backend_path": "bar",
                    "backend_type": "File"
                },
                "enable_diff_snapshots": true
              }"#;

        expected_cfg = LoadSnapshotParams {
            snapshot_path: PathBuf::from("foo"),
            mem_backend: MemBackendConfig {
                backend_path: PathBuf::from("bar"),
                backend_type: MemBackendType::File,
            },
            enable_diff_snapshots: true,
            resume_vm: false,
        };

        let mut parsed_request = parse_put_snapshot(&Body::new(body), Some(&"load")).unwrap();
        assert!(parsed_request
            .parsing_info()
            .take_deprecation_message()
            .is_none());
        match vmm_action_from_request(parsed_request) {
            VmmAction::LoadSnapshot(cfg) => assert_eq!(cfg, expected_cfg),
            _ => panic!("Test failed."),
        }

        body = r#"{
                "snapshot_path": "foo",
                "mem_backend": {
                    "backend_path": "bar",
                    "backend_type": "Uffd"
                },
                "resume_vm": true
              }"#;

        expected_cfg = LoadSnapshotParams {
            snapshot_path: PathBuf::from("foo"),
            mem_backend: MemBackendConfig {
                backend_path: PathBuf::from("bar"),
                backend_type: MemBackendType::Uffd,
            },
            enable_diff_snapshots: false,
            resume_vm: true,
        };

        let mut parsed_request = parse_put_snapshot(&Body::new(body), Some(&"load")).unwrap();
        assert!(parsed_request
            .parsing_info()
            .take_deprecation_message()
            .is_none());
        match vmm_action_from_request(parsed_request) {
            VmmAction::LoadSnapshot(cfg) => assert_eq!(cfg, expected_cfg),
            _ => panic!("Test failed."),
        }

        body = r#"{
                "snapshot_path": "foo",
                "mem_file_path": "bar",
                "resume_vm": true
              }"#;

        expected_cfg = LoadSnapshotParams {
            snapshot_path: PathBuf::from("foo"),
            mem_backend: MemBackendConfig {
                backend_path: PathBuf::from("bar"),
                backend_type: MemBackendType::File,
            },
            enable_diff_snapshots: false,
            resume_vm: true,
        };

        let parsed_request = parse_put_snapshot(&Body::new(body), Some(&"load")).unwrap();
        match depr_action_from_req(parsed_request, Some(LOAD_DEPRECATION_MESSAGE.to_string())) {
            VmmAction::LoadSnapshot(cfg) => assert_eq!(cfg, expected_cfg),
            _ => panic!("Test failed."),
        }

        body = r#"{
                "snapshot_path": "foo",
                "mem_backend": {
                    "backend_path": "bar"
                }
              }"#;

        assert_eq!(
            parse_put_snapshot(&Body::new(body), Some(&"load"))
                .err()
                .unwrap()
                .to_string(),
            "An error occurred when deserializing the json body of a request: missing field \
             `backend_type` at line 5 column 17."
        );

        body = r#"{
                "snapshot_path": "foo",
                "mem_backend": {
                    "backend_type": "File",
                }
              }"#;

        assert_eq!(
            parse_put_snapshot(&Body::new(body), Some(&"load"))
                .err()
                .unwrap()
                .to_string(),
            "An error occurred when deserializing the json body of a request: trailing comma at \
             line 5 column 17."
        );

        body = r#"{
                "snapshot_path": "foo",
                "mem_file_path": "bar",
                "mem_backend": {
                    "backend_path": "bar",
                    "backend_type": "Uffd"
                }
              }"#;

        assert_eq!(
            parse_put_snapshot(&Body::new(body), Some(&"load"))
                .err()
                .unwrap()
                .to_string(),
            Error::SerdeJson(serde_json::Error::custom(TOO_MANY_FIELDS.to_string())).to_string()
        );

        body = r#"{
                "snapshot_path": "foo"
              }"#;

        assert_eq!(
            parse_put_snapshot(&Body::new(body), Some(&"load"))
                .err()
                .unwrap()
                .to_string(),
            Error::SerdeJson(serde_json::Error::custom(MISSING_FIELD.to_string())).to_string()
        );

        body = r#"{
                "mem_backend": {
                    "backend_path": "bar",
                    "backend_type": "Uffd"
                }
              }"#;

        assert_eq!(
            parse_put_snapshot(&Body::new(body), Some(&"load"))
                .err()
                .unwrap()
                .to_string(),
            "An error occurred when deserializing the json body of a request: missing field \
             `snapshot_path` at line 6 column 15."
        );

        assert!(parse_put_snapshot(&Body::new(body), Some(&"invalid")).is_err());
        assert!(parse_put_snapshot(&Body::new(body), None).is_err());
    }

    #[test]
    fn test_parse_patch_vm_state() {
        let mut body = r#"{
                "state": "Paused"
              }"#;

        assert!(parse_patch_vm_state(&Body::new(body))
            .unwrap()
            .eq(&ParsedRequest::new_sync(VmmAction::Pause)));

        body = r#"{
                "state": "Resumed"
              }"#;

        assert!(parse_patch_vm_state(&Body::new(body))
            .unwrap()
            .eq(&ParsedRequest::new_sync(VmmAction::Resume)));

        let invalid_body = r#"{
                "invalid": "Paused"
              }"#;

        assert!(parse_patch_vm_state(&Body::new(invalid_body)).is_err());
    }
}
