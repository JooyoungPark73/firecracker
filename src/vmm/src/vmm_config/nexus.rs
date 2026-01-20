// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Configuration and builder for Nexus shared memory devices

use std::sync::{Arc, Mutex};

pub use crate::devices::nexus::{NexusConfig, NexusError, NexusPciDevice};

/// Errors associated with Nexus device configuration
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum NexusConfigError {
    /// Device creation failed: {0}
    CreateDevice(#[from] NexusError),
    /// Device with ID '{0}' already exists
    DeviceAlreadyExists(String),
    /// Invalid device configuration: {0}
    InvalidConfig(String),
}

/// Builder for Nexus shared memory devices
#[derive(Debug, Default)]
pub struct NexusBuilder {
    /// Collection of Nexus device configurations
    pub configs: Vec<NexusConfig>,
    /// Collection of Nexus devices (populated during VM build)
    pub devices: Vec<Arc<Mutex<NexusPciDevice>>>,
}

impl NexusBuilder {
    /// Create a new NexusBuilder
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a Nexus device from configuration
    ///
    /// This stores the configuration for later device creation during VM initialization.
    pub fn build(&mut self, config: NexusConfig) -> Result<(), NexusConfigError> {
        // Check if device with this ID already exists
        if self.configs.iter().any(|c| c.id == config.id) {
            return Err(NexusConfigError::DeviceAlreadyExists(config.id.clone()));
        }

        // Validate configuration
        if config.id.is_empty() {
            return Err(NexusConfigError::InvalidConfig(
                "Device ID cannot be empty".to_string(),
            ));
        }

        if config.shmem_path.is_empty() {
            return Err(NexusConfigError::InvalidConfig(
                "Shared memory path cannot be empty".to_string(),
            ));
        }

        if config.size_mib == 0 {
            return Err(NexusConfigError::InvalidConfig(
                "Size must be greater than 0 MiB".to_string(),
            ));
        }

        // Store the configuration
        self.configs.push(config);

        Ok(())
    }

    /// Add an existing Nexus device to the builder
    ///
    /// This is used during snapshot restoration to add devices
    /// in the same order as they were in the original VM.
    pub fn add_device(&mut self, device: Arc<Mutex<NexusPciDevice>>) {
        self.devices.push(device);
    }

    /// Get the list of device configurations
    pub fn configs(&self) -> Vec<NexusConfig> {
        self.configs.clone()
    }

    /// Check if any devices are configured
    pub fn is_empty(&self) -> bool {
        self.configs.is_empty()
    }

    /// Get the number of configured devices
    pub fn len(&self) -> usize {
        self.configs.len()
    }

    /// Get a device configuration by ID
    pub fn get_config(&self, id: &str) -> Option<&NexusConfig> {
        self.configs.iter().find(|c| c.id == id)
    }

    /// Get a device by ID
    pub fn get_device(&self, id: &str) -> Option<&Arc<Mutex<NexusPciDevice>>> {
        self.devices.iter().find(|d| d.lock().unwrap().id() == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nexus_builder_new() {
        let builder = NexusBuilder::new();
        assert!(builder.is_empty());
        assert_eq!(builder.len(), 0);
    }

    #[test]
    fn test_nexus_builder_build() {
        let mut builder = NexusBuilder::new();

        let config = NexusConfig {
            id: "nexus0".to_string(),
            shmem_path: "/dev/shm/nexus_test".to_string(),
            size_mib: 1,
        };

        let result = builder.build(config.clone());
        assert!(result.is_ok());
        assert_eq!(builder.len(), 1);

        let configs = builder.configs();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0], config);
    }

    #[test]
    fn test_nexus_builder_duplicate_id() {
        let mut builder = NexusBuilder::new();

        let config = NexusConfig {
            id: "nexus0".to_string(),
            shmem_path: "/dev/shm/nexus_test".to_string(),
            size_mib: 1,
        };

        // First device should succeed
        assert!(builder.build(config.clone()).is_ok());

        // Second device with same ID should fail
        let result = builder.build(config);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            NexusConfigError::DeviceAlreadyExists(_)
        ));
    }

    #[test]
    fn test_nexus_builder_invalid_config() {
        let mut builder = NexusBuilder::new();

        // Empty ID
        let config = NexusConfig {
            id: "".to_string(),
            shmem_path: "/dev/shm/nexus_test".to_string(),
            size_mib: 1,
        };
        assert!(builder.build(config).is_err());

        // Empty path
        let config = NexusConfig {
            id: "nexus0".to_string(),
            shmem_path: "".to_string(),
            size_mib: 1,
        };
        assert!(builder.build(config).is_err());

        // Zero size
        let config = NexusConfig {
            id: "nexus0".to_string(),
            shmem_path: "/dev/shm/nexus_test".to_string(),
            size_mib: 0,
        };
        assert!(builder.build(config).is_err());
    }

    #[test]
    fn test_nexus_builder_get_device() {
        let mut builder = NexusBuilder::new();

        let config = NexusConfig {
            id: "nexus0".to_string(),
            shmem_path: "/dev/shm/nexus_test".to_string(),
            size_mib: 1,
        };

        builder.build(config).unwrap();

        let config_result = builder.get_config("nexus0");
        assert!(config_result.is_some());

        let device = builder.get_device("nonexistent");
        assert!(device.is_none());
    }
}
