// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use vmm::logger::{IncMetric, METRICS};
use vmm::rpc_interface::VmmAction;
use vmm::vmm_config::khala::KhalaConfig;

use super::super::parsed_request::{ParsedRequest, RequestError, checked_id};
use super::{Body, StatusCode};

pub(crate) fn parse_put_khala(
    body: &Body,
    id_from_path: Option<&str>,
) -> Result<ParsedRequest, RequestError> {
    METRICS.put_api_requests.khala_count.inc();
    let id = if let Some(id) = id_from_path {
        checked_id(id)?
    } else {
        METRICS.put_api_requests.khala_fails.inc();
        return Err(RequestError::EmptyID);
    };

    let device_cfg = serde_json::from_slice::<KhalaConfig>(body.raw()).inspect_err(|_| {
        METRICS.put_api_requests.khala_fails.inc();
    })?;

    if id != device_cfg.id {
        METRICS.put_api_requests.khala_fails.inc();
        Err(RequestError::Generic(
            StatusCode::BadRequest,
            "The id from the path does not match the id from the body!".to_string(),
        ))
    } else {
        Ok(ParsedRequest::new_sync(VmmAction::InsertKhalaDevice(
            device_cfg,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_server::parsed_request::tests::vmm_action_from_request;

    #[test]
    fn test_parse_put_khala_request() {
        parse_put_khala(&Body::new("invalid_payload"), None).unwrap_err();
        parse_put_khala(&Body::new("invalid_payload"), Some("id")).unwrap_err();

        let body = r#"{
            "id": "khala0",
            "shmem_path": "/dev/shm/khala_region",
            "size_mib": 16
        }"#;

        let req = parse_put_khala(&Body::new(body), Some("khala0")).unwrap();
        assert!(matches!(
            vmm_action_from_request(req),
            VmmAction::InsertKhalaDevice(_)
        ));

        // ID mismatch
        parse_put_khala(&Body::new(body), Some("wrong_id")).unwrap_err();
    }
}
