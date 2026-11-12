// SPDX-License-Identifier: Apache-2.0
//! Wire requests for independently selectable tracing resources.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Resource {
    Idle,
    Census,
    Metadata,
    Epochs,
}

impl Resource {
    pub fn schema(self) -> &'static str {
        match self {
            Self::Idle => "aries.firecracker.page-idle.v1",
            Self::Census => "aries.firecracker.memory-census.v1",
            Self::Metadata => "aries.firecracker.metadata-capture.v1",
            Self::Epochs => "aries.firecracker.trace-epochs.v1",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    Dirty,
    Idle,
    Census,
    Metadata,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Consistency {
    DirtyValidated,
    QuiescenceOnly,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Arm {
        epoch: u64,
        phase: Option<String>,
        metrics: Option<Vec<Metric>>,
    },
    Collect {
        vm_generation: String,
        epoch: u64,
        next_epoch: Option<u64>,
        phase: Option<String>,
    },
    Rotate {
        vm_generation: String,
        epoch: u64,
        next_epoch: Option<u64>,
        phase: Option<String>,
    },
    Capture {
        vm_generation: Option<String>,
        epoch: Option<u64>,
        request_id: Option<String>,
    },
    Begin {
        vm_generation: Option<String>,
        capture_id: String,
        epoch: Option<u64>,
        mode: String,
        consistency: Consistency,
        max_bytes: Option<u64>,
        max_scan_bytes: Option<u64>,
        capture_census: Option<bool>,
    },
    Info {
        capture_id: String,
        vm_generation: String,
    },
    ReadPages {
        capture_id: String,
        vm_generation: String,
        gpas: Vec<u64>,
    },
    Scan {
        capture_id: String,
        vm_generation: String,
        needle_b64: String,
        start_gpa: u64,
        end_gpa: u64,
        cursor: u64,
        max_scan_bytes: u64,
        resident_only: bool,
    },
    Seal {
        capture_id: String,
        vm_generation: String,
    },
    Abort {
        capture_id: String,
        vm_generation: String,
        reason: String,
    },
    Export {
        record_id: String,
        kind: String,
        offset: u64,
        limit: usize,
    },
    Ack {
        record_id: String,
        vm_generation: String,
    },
}
