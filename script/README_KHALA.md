# Khala Shared Memory Device - Testing Guide

This directory contains scripts and test programs for the Khala shared memory device.

## Overview

Khala is a custom **PCI device** that provides zero-copy shared memory communication between the Firecracker host and guest VM.

**Implementation Approach:**
- **PCI Device Emulation**: Khala is implemented as a PCI device in Firecracker (not a kernel module)
- **Driver**: Uses standard Linux `uio_pci_generic` driver (no custom kernel driver needed)
- **Memory Mapping**: Shared memory is mapped via KVM into guest address space through PCI BARs

**Note:** If you see kernel messages like `khala_shmem: missing khala_shmem= cmdline`, this indicates an old kernel driver approach. The current implementation does **NOT** require kernel parameters or custom drivers.

**Key Features:****
- Custom PCI device (Vendor: 0x1234, Device: 0xDEAD)
- Zero-copy memory access via KVM
- Works with standard `uio_pci_generic` Linux driver
- Supports up to 16MB shared memory (configurable)

## Architecture

```
┌─────────────────┐
│  Host Client    │
│  (Python)       │
│                 │
│  Sends requests │
│  via /dev/shm/  │
└────────┬────────┘
         │
         │ Zero-copy via KVM
         │
┌────────▼────────┐
│  Firecracker    │
│  Khala Device   │
│  (PCI 1234:dead)│
└────────┬────────┘
         │
         │ PCI BAR mapping
         │
┌────────▼────────┐
│  Guest Server   │
│  (Python)       │
│                 │
│  Handles reqs   │
│  via uio driver │
└─────────────────┘
```

## Files

### Scripts
- **`setup_khala_shmem.sh`** - Creates and initializes the shared memory file
- **`boot_fc.sh`** - Updated to configure Khala device via API
- **`start_fs_bin.sh`** - Starts Firecracker with PCI support
- **`test_khala.sh`** - Interactive test script

### Test Programs
- **`khala_host_client.py`** - Host-side Python client (sends requests)
- **`khala_guest_server.py`** - Guest-side Python server (handles requests)

## Quick Start

### 1. Build Firecracker with Khala Support

```bash
cd /users/nehalem/firecracker
./tools/devtool build --release
```

### 2. Setup Shared Memory

```bash
bash script/setup_khala_shmem.sh
```

This creates `/dev/shm/khala_region` (16MB)

### 3. Start Firecracker

Terminal 1:
```bash
bash script/start_fs_bin.sh
```

Terminal 2:
```bash
bash script/boot_fc.sh
```

The boot script now includes:
```bash
sudo curl --unix-socket /tmp/firecracker.socket -i \
    -X PUT 'http://localhost/khala/khala0' \
    -H 'Content-Type: application/json' \
    -d '{
         "id": "khala0",
         "shmem_path": "/dev/shm/khala_region",
         "size_mib": 16
    }'
```

### 4. Start Host Client

Terminal 3:
```bash
python3 script/khala_host_client.py
```

Expected output:
```
[Host] Khala Shared Memory Client
[Host] Opening shared memory: /dev/shm/khala_region
[Host] Shared memory mapped successfully (16777216 bytes)
[Host] Waiting for guest server to be ready...
```

### 5. Start Guest Server

In the Firecracker VM console:

First, copy the guest server to the VM (via network, vsock, or build it into the rootfs).

Then run:
```bash
python3 khala_guest_server.py
```

Expected output:
```
[Guest] Khala Shared Memory Server
[Guest] Found Khala device: 0000:00:02.0
[Guest]   Vendor: 0x1234, Device: 0xdead
[Guest]   UIO device: /dev/uio0
[Guest] Shared memory mapped successfully!
[Guest] Server ready - waiting for requests...
```

## Communication Protocol

The shared memory is divided into two regions:

```
Offset 0x000000 (0):    Host → Guest buffer (1MB)
Offset 0x100000 (1MB):  Guest → Host buffer (1MB)
```

Each message has the format:
```
[4 bytes: length (uint32 LE)] [N bytes: message data (UTF-8)]
```

## Testing Without Guest Python

You can test basic PCI device detection in the guest:

```bash
# In guest VM
lspci -v

# Should show:
# 00:02.0 RAM memory: Device 1234:dead (rev 01)
```

Check UIO binding:
```bash
modprobe uio_pci_generic
echo "1234 dead" > /sys/bus/pci/drivers/uio_pci_generic/new_id
ls -la /dev/uio*
```

Read control registers:
```bash
python3 << 'EOF'
import struct

with open("/sys/bus/pci/devices/0000:00:02.0/resource0", "rb") as f:
    data = f.read(16)
    dev_id, status, size_lo, size_hi = struct.unpack("<IIII", data)
    print(f"Device ID Register: {dev_id:#010x}")
    print(f"Status Register: {status:#010x}")  
    print(f"Shared Memory Size: {(size_hi << 32) | size_lo} bytes")
EOF
```

Expected output:
```
Device ID Register: 0x12340xdead
Status Register: 0x00000003
Shared Memory Size: 16777216 bytes
```

## Troubleshooting

### Host Issues

**Problem:** "Shared memory file not found"
```bash
# Solution: Run setup script
bash script/setup_khala_shmem.sh
```

**Problem:** "Permission denied" when accessing shared memory
```bash
# Solution: Check permissions
ls -la /dev/shm/khala_region
# Should be: -rw-rw-rw- (666)

# Fix if needed:
sudo chmod 666 /dev/shm/khala_region
```

### Guest Issues

**Problem:** "Khala device not found"
```bash
# Check if PCI is enabled in Firecracker
# start_fs_bin.sh should have: --enable-pci

# Check PCI devices
lspci -nn
# Should see: 00:02.0 RAM memory [0500]: Device [1234:dead]
```

**Problem:** "No UIO device found"
```bash
# Load driver
modprobe uio_pci_generic

# Manually bind
echo "1234 dead" > /sys/bus/pci/drivers/uio_pci_generic/new_id

# Check
ls /sys/bus/pci/devices/*/uio/uio*
```

**Problem:** "Failed to map shared memory"
```bash
# Check BAR resources
ls -la /sys/bus/pci/devices/0000:00:02.0/resource*

# The resource files should exist and be readable
# You may need root privileges to mmap them
```

## API Configuration

The Khala device can be configured via the Firecracker API:

**PUT /khala/{id}**

Request body:
```json
{
  "id": "khala0",
  "shmem_path": "/dev/shm/khala_region",
  "size_mib": 16
}
```

Fields:
- `id` (required): Unique device identifier
- `shmem_path` (required): Path to shared memory file on host
- `size_mib` (required): Size in MiB (must match file size)

## Performance Notes

- **Zero-copy:** No data copying between host and guest
- **Latency:** Direct memory access, microsecond-range latency
- **Throughput:** Limited only by memory bandwidth

## Future Enhancements (Ver. 2)

- [ ] MSI-X interrupts for event notification
- [ ] HugePage support (2MB/1GB pages)
- [ ] Multiple shared memory regions
- [ ] Hot-plug/hot-unplug support
- [ ] Advanced security/access controls
- [ ] Doorbell registers for signaling

## License

Same as Firecracker (Apache 2.0)
