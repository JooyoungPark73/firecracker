#!/bin/bash
# Setup script for Nexus shared memory device

set -e

SHMEM_PATH="/dev/shm/nexus_region"
SHMEM_SIZE_MB=16

echo "Setting up Nexus shared memory..."

# Remove existing shared memory file
sudo rm -f "${SHMEM_PATH}"

# Create shared memory file using shm_open equivalent
# We'll use Python to create it properly with shm_open
python3 << 'EOF'
import mmap
import os
from pathlib import Path

SHMEM_PATH = "/dev/shm/nexus_region"
SHMEM_SIZE = 16 * 1024 * 1024  # 16 MB

# Remove if exists
try:
    os.unlink(SHMEM_PATH)
except FileNotFoundError:
    pass

# Create and initialize the shared memory file
with open(SHMEM_PATH, 'wb') as f:
    f.write(b'\x00' * SHMEM_SIZE)

# Set permissions
os.chmod(SHMEM_PATH, 0o666)

print(f"Created shared memory file at {SHMEM_PATH} ({SHMEM_SIZE} bytes)")
EOF

# Verify the file was created
if [ -f "${SHMEM_PATH}" ]; then
    SIZE=$(stat -c%s "${SHMEM_PATH}")
    echo "✓ Shared memory file created: ${SHMEM_PATH}"
    echo "  Size: ${SIZE} bytes ($(( SIZE / 1024 / 1024 )) MB)"
    echo "  Permissions: $(stat -c%a ${SHMEM_PATH})"
else
    echo "✗ Failed to create shared memory file"
    exit 1
fi

echo "Nexus shared memory setup complete!"
