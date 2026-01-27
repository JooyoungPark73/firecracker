// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Nexus PCI Device - Main Implementation

use std::fmt::{self, Debug};
use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Barrier};

use kvm_bindings::kvm_userspace_memory_region;
use log::{debug, error};
use pci::{PciBarPrefetchable, PciBarRegionType, PciClassCode, PciSubclass};
use serde::{Deserialize, Serialize};

use crate::pci::configuration::PciConfiguration;
use crate::pci::{BarReprogrammingParams, DeviceRelocationError, PciDevice};
use crate::utils::mib_to_bytes;
use crate::vstate::bus::BusDevice;
use crate::vstate::memory::MemoryError;
use crate::Vm;

/// PCI Vendor ID for Nexus device (0x1234 = Generic/QEMU)
pub const NEXUS_VENDOR_ID: u16 = 0x1234;
/// PCI Device ID for Nexus device (0x1110 = Nexus Shared Memory)
pub const NEXUS_DEVICE_ID: u16 = 0x1110;
/// PCI Revision ID
pub const NEXUS_REVISION_ID: u8 = 0x01;
/// PCI Subsystem Vendor ID
pub const NEXUS_SUBSYSTEM_VENDOR_ID: u16 = 0x1234;
/// PCI Subsystem ID
pub const NEXUS_SUBSYSTEM_ID: u16 = 0x1110;

/// BAR 0: Shared memory region
pub const SHMEM_BAR_INDEX: usize = 0;

/// Control register offsets (reserved for future use)
pub const DEVICE_ID_REG: u64 = 0x00;
pub const STATUS_REG: u64 = 0x04;
pub const SHMEM_SIZE_LO_REG: u64 = 0x08;
pub const SHMEM_SIZE_HI_REG: u64 = 0x0C;

/// Device status flags
pub const STATUS_READY: u32 = 0x0001;
pub const STATUS_SHMEM_MAPPED: u32 = 0x0002;

/// PCI class for Nexus device (Memory Controller)
#[derive(Clone, Copy, Debug)]
pub struct NexusPciClass;

impl PciSubclass for NexusPciClass {
    fn get_register_value(&self) -> u8 {
        0x00 // RAM memory subclass
    }
}

/// Errors for Nexus device operations
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum NexusError {
    /// Failed to open shared memory file: {0}
    OpenShmemFile(std::io::Error),
    /// Failed to get file metadata: {0}
    FileMetadata(std::io::Error),
    /// Shared memory file size mismatch: expected {expected}, got {actual}
    SizeMismatch { expected: u64, actual: u64 },
    /// Failed to create memory region: {0}
    CreateMemoryRegion(#[from] MemoryError),
    /// Failed to register memory region with KVM: {0}
    RegisterMemoryRegion(#[from] crate::vstate::vm::VmError),
    /// Invalid configuration: {0}
    InvalidConfig(String),
}

/// Configuration for a Nexus shared memory device
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NexusConfig {
    /// Device identifier
    pub id: String,
    /// Path to the shared memory file on the host (e.g., /dev/shm/nexus_region)
    pub path_on_host: String,
}

/// Nexus PCI Device implementation
pub struct NexusPciDevice {
    /// Device identifier
    pub(crate) id: String,
    /// Device configuration
    pub(crate) config: NexusConfig,
    /// PCI configuration space
    pub(crate) configuration: PciConfiguration,
    /// PCI BDF (Bus/Device/Function)
    pub(crate) pci_device_bdf: u32,
    /// Control BAR address (unused in current implementation)
    pub(crate) control_bar_addr: u64,
    /// Shared memory BAR address
    pub(crate) shmem_bar_addr: u64,
    /// Guest physical address for shared memory
    pub(crate) shmem_guest_addr: u64,
    /// Actual size of shared memory in bytes
    pub(crate) shmem_size_bytes: u64,
    /// Backing file handle (kept alive to maintain fd)
    pub(crate) file: File,
    /// File length in bytes
    pub(crate) file_len: u64,
    /// mmap pointer (userspace_addr for KVM)
    pub(crate) mmap_ptr: u64,
}

impl Debug for NexusPciDevice {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("NexusPciDevice")
            .field("id", &self.id)
            .field("config", &self.config)
            .field("pci_device_bdf", &self.pci_device_bdf)
            .field("shmem_bar_addr", &format_args!("{:#x}", self.shmem_bar_addr))
            .field("shmem_guest_addr", &format_args!("{:#x}", self.shmem_guest_addr))
            .field("shmem_size_bytes", &self.shmem_size_bytes)
            .field("file_len", &self.file_len)
            .field("mmap_ptr", &format_args!("{:#x}", self.mmap_ptr))
            .finish()
    }
}

impl NexusPciDevice {
    /// Create a new Nexus PCI device
    pub fn new(
        id: String,
        config: NexusConfig,
        pci_device_bdf: u32,
    ) -> Result<Self, NexusError> {
        // Validate configuration
        if config.path_on_host.is_empty() {
            return Err(NexusError::InvalidConfig(
                "Path cannot be empty".to_string(),
            ));
        }

        // Open and mmap the backing file
        let (file, file_len, mmap_ptr) = Self::mmap_backing_file(&config.path_on_host)?;
        let shmem_size_bytes = file_len;

        // Create PCI configuration with interrupt pin
        let mut configuration = PciConfiguration::new_type0(
            NEXUS_VENDOR_ID,
            NEXUS_DEVICE_ID,
            NEXUS_REVISION_ID,
            PciClassCode::MemoryController,
            &NexusPciClass,
            NEXUS_SUBSYSTEM_VENDOR_ID,
            NEXUS_SUBSYSTEM_ID,
            None, // No MSI-X
        );

        // Set interrupt pin INT#A (required for uio_pci_generic)
        let int_pin = 0x01u32;
        let reg15_value = configuration.read_reg(15);
        configuration.set_register(15, (reg15_value & 0xFFFF_00FF) | (int_pin << 8));

        debug!(
            "Creating Nexus device '{}' with shared memory at {}, INT#A assigned",
            id, config.path_on_host
        );

        Ok(NexusPciDevice {
            id,
            config,
            configuration,
            pci_device_bdf,
            control_bar_addr: 0,
            shmem_bar_addr: 0,
            shmem_guest_addr: 0,
            shmem_size_bytes: shmem_size_bytes as u64,
            file,
            file_len,
            mmap_ptr,
        })
    }

    /// Allocate and configure PCI BARs
    pub fn allocate_bars(
        &mut self,
        mmio64_allocator: &mut vm_allocator::AddressAllocator,
    ) -> Result<(), NexusError> {
        use vm_allocator::AllocPolicy;

        let shmem_alignment = self.shmem_size_bytes.max(0x1000);
        let shmem_bar_addr = mmio64_allocator
            .allocate(
                self.shmem_size_bytes,
                shmem_alignment,
                AllocPolicy::FirstMatch,
            )
            .map_err(|e| {
                NexusError::InvalidConfig(format!("Failed to allocate shared memory BAR: {}", e))
            })?
            .start();

        self.shmem_bar_addr = shmem_bar_addr;
        self.shmem_guest_addr = shmem_bar_addr;

        self.configuration
            .add_pci_bar(SHMEM_BAR_INDEX, shmem_bar_addr, self.shmem_size_bytes);

        debug!(
            "Allocated Nexus shared memory BAR 0 at {:#x}, size {:#x}",
            shmem_bar_addr, self.shmem_size_bytes
        );

        Ok(())
    }

    /// Map backing file into memory (adapted from pmem)
    fn mmap_backing_file(path: &str) -> Result<(File, u64, u64), NexusError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(NexusError::OpenShmemFile)?;

        let file_len = file
            .metadata()
            .map_err(NexusError::FileMetadata)?
            .len();

        if file_len == 0 {
            return Err(NexusError::InvalidConfig(
                "Backing file size is 0".to_string(),
            ));
        }

        let prot = libc::PROT_READ | libc::PROT_WRITE;
        let flags = libc::MAP_SHARED | libc::MAP_NORESERVE;

        // SAFETY: Calling mmap with valid arguments
        let mmap_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                file_len as usize,
                prot,
                flags,
                file.as_raw_fd(),
                0,
            )
        };

        if mmap_ptr == libc::MAP_FAILED {
            return Err(NexusError::InvalidConfig(format!(
                "Failed to mmap shared memory file: {}",
                std::io::Error::last_os_error()
            )));
        }

        debug!(
            "mmapped Nexus backing file '{}' ({} bytes) at userspace_addr {:#x}",
            path, file_len, mmap_ptr as u64
        );

        Ok((file, file_len, mmap_ptr as u64))
    }

    /// Map the shared memory file into guest address space
    pub fn map_shared_memory(&mut self, vm: &Vm) -> Result<(), NexusError> {
        // File is already mmapped from new(), just register with KVM

        // Get KVM slot
        let slot = vm.next_kvm_slot(1)
            .ok_or_else(|| NexusError::InvalidConfig("No KVM slot available".to_string()))?;

        // Register with KVM
        let kvm_region = kvm_userspace_memory_region {
            slot,
            guest_phys_addr: self.shmem_guest_addr,
            memory_size: self.shmem_size_bytes,
            userspace_addr: self.mmap_ptr,
            flags: 0,
        };

        vm.set_user_memory_region(kvm_region)
            .map_err(|e| NexusError::InvalidConfig(format!("Failed to register KVM memory region: {}", e)))?;

        debug!(
            "Registered Nexus shared memory with KVM: guest_phys_addr={:#x}, size={:#x}, slot={}, userspace_addr={:#x}",
            self.shmem_guest_addr, self.shmem_size_bytes, slot, self.mmap_ptr
        );

        Ok(())
    }

    /// Get device ID
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Get device configuration
    pub fn config(&self) -> &NexusConfig {
        &self.config
    }

    /// Get shared memory BAR address
    pub fn shmem_bar_addr(&self) -> u64 {
        self.shmem_bar_addr
    }
}

impl PciDevice for NexusPciDevice {
    fn write_config_register(
        &mut self,
        reg_idx: usize,
        offset: u64,
        data: &[u8],
    ) -> Option<Arc<Barrier>> {
        self.configuration.write_config_register(reg_idx, offset, data);
        None
    }

    fn read_config_register(&mut self, reg_idx: usize) -> u32 {
        self.configuration.read_reg(reg_idx)
    }

    fn detect_bar_reprogramming(
        &mut self,
        reg_idx: usize,
        data: &[u8],
    ) -> Option<BarReprogrammingParams> {
        self.configuration.detect_bar_reprogramming(reg_idx, data)
    }

    fn move_bar(&mut self, old_base: u64, new_base: u64) -> Result<(), DeviceRelocationError> {
        if self.shmem_bar_addr == old_base {
            debug!("Nexus: Moving BAR from {:#x} to {:#x}", old_base, new_base);
            self.shmem_bar_addr = new_base;
            self.shmem_guest_addr = new_base;
            error!("Nexus: BAR relocation after mapping is not supported");
        }
        Ok(())
    }

    fn read_bar(&mut self, base: u64, offset: u64, data: &mut [u8]) {
        if base == self.shmem_bar_addr {
            // Handled by KVM EPT - shouldn't be called
            debug!("Nexus: Unexpected read_bar at offset {:#x}", offset);
        }
    }

    fn write_bar(&mut self, base: u64, _offset: u64, _data: &[u8]) -> Option<Arc<Barrier>> {
        if base == self.shmem_bar_addr {
            // Silently ignore - UIO probing
        }
        None
    }
}

impl BusDevice for NexusPciDevice {
    fn read(&mut self, base: u64, offset: u64, data: &mut [u8]) {
        self.read_bar(base, offset, data);
    }

    fn write(&mut self, base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        self.write_bar(base, offset, data)
    }
}
