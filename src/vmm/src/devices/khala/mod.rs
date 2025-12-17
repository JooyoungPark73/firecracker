// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Khala Shared Memory PCI Device
//!
//! This module implements a custom PCI device that exposes a shared memory region
//! from the host to the guest VM for zero-copy communication.
//!
//! # Architecture
//! - BAR 0: Shared memory region mapped from host file
//! - Uses KVM memory regions for zero-copy access via EPT
//! - Compatible with uio_pci_generic driver in guest
//!
//! # Snapshot Support
//! - Configuration is saved in snapshots
//! - Shared memory file must exist at restore time
//! - Memory content is NOT saved (external state)

pub mod device;
pub mod metrics;
pub mod persist;

pub use device::{KhalaConfig, KhalaError, KhalaPciDevice};
pub use metrics::KhalaMetrics;
pub use persist::{KhalaPciDeviceState, KhalaPersistError};
