// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Nexus Device Metrics (Placeholder)

use std::sync::Arc;

use crate::logger::{METRICS, StoreMetric};

/// Nexus device metrics
#[derive(Debug)]
pub struct NexusMetrics {
    /// Device identifier
    pub id: String,
}

impl NexusMetrics {
    /// Create new metrics for a Nexus device
    pub fn new(id: String) -> Arc<Self> {
        Arc::new(NexusMetrics { id })
    }
}
