// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Nexus Device Snapshot/Restore Support

use serde::{Deserialize, Serialize};

use super::device::{NexusConfig, NexusError, NexusPciDevice};
use crate::pci::configuration::{PciConfiguration, PciConfigurationState};
use crate::snapshot::Persist;
use crate::Vm;

/// Saved state for Nexus PCI device
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NexusPciDeviceState {
    /// Device identifier
    pub id: String,
    /// Device configuration (file path, size)
    pub config: NexusConfig,
    /// PCI BDF (Bus/Device/Function)
    pub pci_device_bdf: u32,
    /// PCI configuration state
    pub pci_configuration: PciConfigurationState,
    /// Shared memory BAR address
    pub shmem_bar_addr: u64,
    /// Guest physical address for shared memory region
    pub shmem_guest_addr: u64,
    /// Shared memory size in bytes
    pub shmem_size_bytes: u64,
}

/// Constructor arguments for restoring Nexus device
#[derive(Debug)]
pub struct NexusConstructorArgs<'a> {
    /// VM reference for KVM registration
    pub vm: &'a Vm,
}

/// Errors during Nexus snapshot/restore
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum NexusPersistError {
    /// Error creating Nexus device: {0}
    Nexus(#[from] NexusError),
}

impl<'a> Persist<'a> for NexusPciDevice {
    type State = NexusPciDeviceState;
    type ConstructorArgs = NexusConstructorArgs<'a>;
    type Error = NexusPersistError;

    fn save(&self) -> Self::State {
        NexusPciDeviceState {
            id: self.id.clone(),
            config: self.config.clone(),
            pci_device_bdf: self.pci_device_bdf,
            pci_configuration: self.configuration.state(),
            shmem_bar_addr: self.shmem_bar_addr,
            shmem_guest_addr: self.shmem_guest_addr,
            shmem_size_bytes: self.shmem_size_bytes,
        }
    }

    fn restore(
        constructor_args: Self::ConstructorArgs,
        state: &Self::State,
    ) -> Result<Self, Self::Error> {
        // Create new device with saved configuration
        let mut device = NexusPciDevice::new(
            state.id.clone(),
            state.config.clone(),
            state.pci_device_bdf,
        )?;

        // Restore PCI configuration
        device.configuration = PciConfiguration::type0_from_state(
            state.pci_configuration.clone(),
            None, // No MSI-X
        );

        // Restore BAR addresses
        device.shmem_bar_addr = state.shmem_bar_addr;
        device.shmem_guest_addr = state.shmem_guest_addr;

        // Re-map the shared memory file (must exist at same path)
        // This re-opens the file and re-registers with KVM
        device.map_shared_memory(constructor_args.vm)?;

        Ok(device)
    }
}