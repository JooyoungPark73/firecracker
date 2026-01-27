// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use vmm::logger::{IncMetric, METRICS};
use vmm::rpc_interface::VmmAction;
use vmm::vmm_config::nexus::NexusConfig;

use super::super::parsed_request::{ParsedRequest, RequestError, checked_id};
use super::{Body, StatusCode};

pub(crate) fn parse_put_nexus(
    body: &Body,
    id_from_path: Option<&str>,
) -> Result<ParsedRequest, RequestError> {
    METRICS.put_api_requests.nexus_count.inc();
    let id = if let Some(id) = id_from_path {
        checked_id(id)?
    } else {
        METRICS.put_api_requests.nexus_fails.inc();
        return Err(RequestError::EmptyID);
    };

    let device_cfg = serde_json::from_slice::<NexusConfig>(body.raw()).inspect_err(|_| {
        METRICS.put_api_requests.nexus_fails.inc();
    })?;

    if id != device_cfg.id {
        METRICS.put_api_requests.nexus_fails.inc();
        Err(RequestError::Generic(
            StatusCode::BadRequest,
            "The id from the path does not match the id from the body!".to_string(),
        ))
    } else {
        Ok(ParsedRequest::new_sync(VmmAction::InsertNexusDevice(
            device_cfg,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_server::parsed_request::tests::vmm_action_from_request;

    #[test]
    fn test_parse_put_nexus_request() {
        parse_put_nexus(&Body::new("invalid_payload"), None).unwrap_err();
        parse_put_nexus(&Body::new("invalid_payload"), Some("id")).unwrap_err();

        let body = r#"{
            "id": "nexus0",
            "path_on_host": "/dev/shm/nexus_region"
        }"#;

        let req = parse_put_nexus(&Body::new(body), Some("nexus0")).unwrap();
        assert!(matches!(
            vmm_action_from_request(req),
            VmmAction::InsertNexusDevice(_)
        ));

        // ID mismatch
        parse_put_nexus(&Body::new(body), Some("wrong_id")).unwrap_err();
    }
}
