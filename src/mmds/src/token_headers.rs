// Copyright 2021 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::result::Result;

use micro_http::{HttpHeaderError, RequestError};

/// Header rejected by MMDS.
pub const REJECTED_HEADER: &str = "X-Forwarded-For";

/// Wrapper over the list of token headers associated with a Request.
#[derive(Debug, PartialEq, Eq)]
pub struct TokenHeaders {
    /// The `X-metadata-token` header might be used by HTTP clients to specify a token in order
    /// to authenticate to the session. This is used for guest requests to MMDS only.
    x_metadata_token: Option<String>,
    /// The `X-metadata-token-ttl-seconds` header might be used by HTTP clients to specify
    /// the expiry time of a token. This is used for PUT requests issued by the guest to MMDS only.
    x_metadata_token_ttl_seconds: Option<u32>,
}

impl Default for TokenHeaders {
    /// Token headers are not present in the request by default.
    fn default() -> Self {
        Self {
            x_metadata_token: None,
            x_metadata_token_ttl_seconds: None,
        }
    }
}

impl TokenHeaders {
    /// `X-metadata-token` header.
    const X_METADATA_TOKEN: &'static str = "X-metadata-token";
    /// `X-metadata-token-ttl-seconds` header.
    const X_METADATA_TOKEN_TTL_SECONDS: &'static str = "X-metadata-token-ttl-seconds";

    /// Return `TokenHeaders` from headers map.
    pub fn try_from(map: &HashMap<String, String>) -> Result<TokenHeaders, RequestError> {
        let mut headers = Self::default();
        let lowercased_headers: HashMap<String, String> = map
            .iter()
            .map(|(k, v)| (k.to_lowercase(), v.clone()))
            .collect();

        if let Some(token) = lowercased_headers.get(&TokenHeaders::X_METADATA_TOKEN.to_lowercase())
        {
            headers.x_metadata_token = Some(token.to_string());
        }

        if let Some(value) =
            lowercased_headers.get(&TokenHeaders::X_METADATA_TOKEN_TTL_SECONDS.to_lowercase())
        {
            match value.parse::<u32>() {
                Ok(seconds) => {
                    headers.x_metadata_token_ttl_seconds = Some(seconds);
                }
                Err(_) => {
                    return Err(RequestError::HeaderError(HttpHeaderError::InvalidValue(
                        TokenHeaders::X_METADATA_TOKEN_TTL_SECONDS.to_string(),
                        value.to_string(),
                    )));
                }
            }
        }

        Ok(headers)
    }

    /// Returns the `XMetadataToken` token.
    pub fn x_metadata_token(&self) -> Option<&String> {
        self.x_metadata_token.as_ref()
    }

    /// Returns the `XMetadataTokenTtlSeconds` token.
    pub fn x_metadata_token_ttl_seconds(&self) -> Option<u32> {
        self.x_metadata_token_ttl_seconds
    }

    /// Sets the `XMetadataToken` token.
    pub fn set_x_metadata_token(&mut self, token: String) {
        self.x_metadata_token = Some(token)
    }

    /// Sets the `XMetadataTokenTtlSeconds` token.
    pub fn set_x_metadata_token_ttl_seconds(&mut self, ttl: u32) {
        self.x_metadata_token_ttl_seconds = Some(ttl);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default() {
        let headers = TokenHeaders::default();
        assert_eq!(headers.x_metadata_token(), None);
        assert_eq!(headers.x_metadata_token_ttl_seconds(), None);
    }

    #[test]
    fn test_try_from_headers() {
        // Empty token header map.
        let map: HashMap<String, String> = HashMap::default();
        let headers = TokenHeaders::try_from(&map).unwrap();
        assert_eq!(headers, TokenHeaders::default());

        // Unrecognised headers.
        let mut map: HashMap<String, String> = HashMap::default();
        map.insert("Some-Header".to_string(), "10".to_string());
        map.insert("Another-Header".to_string(), "value".to_string());
        let headers = TokenHeaders::try_from(&map).unwrap();
        assert_eq!(headers, TokenHeaders::default());

        // Valid headers.
        let mut map: HashMap<String, String> = HashMap::default();
        map.insert("Some-Header".to_string(), "10".to_string());
        map.insert(
            TokenHeaders::X_METADATA_TOKEN_TTL_SECONDS.to_string(),
            "60".to_string(),
        );
        map.insert(
            TokenHeaders::X_METADATA_TOKEN.to_string(),
            "foo".to_string(),
        );
        let headers = TokenHeaders::try_from(&map).unwrap();
        assert_eq!(headers.x_metadata_token_ttl_seconds().unwrap(), 60);
        assert_eq!(*headers.x_metadata_token().unwrap(), "foo".to_string());

        let mut map: HashMap<String, String> = HashMap::default();
        map.insert(TokenHeaders::X_METADATA_TOKEN.to_string(), "".to_string());
        let headers = TokenHeaders::try_from(&map).unwrap();
        assert_eq!(*headers.x_metadata_token().unwrap(), "".to_string());

        // Lowercased headers
        let mut map: HashMap<String, String> = HashMap::default();
        map.insert(
            TokenHeaders::X_METADATA_TOKEN_TTL_SECONDS
                .to_string()
                .to_lowercase(),
            "60".to_string(),
        );
        let headers = TokenHeaders::try_from(&map).unwrap();
        assert_eq!(headers.x_metadata_token_ttl_seconds().unwrap(), 60);

        // Invalid value.
        let mut map: HashMap<String, String> = HashMap::default();
        map.insert(
            TokenHeaders::X_METADATA_TOKEN_TTL_SECONDS.to_string(),
            "-60".to_string(),
        );
        assert_eq!(
            TokenHeaders::try_from(&map).unwrap_err(),
            RequestError::HeaderError(HttpHeaderError::InvalidValue(
                TokenHeaders::X_METADATA_TOKEN_TTL_SECONDS.to_string(),
                "-60".to_string()
            ))
        );
    }
}
