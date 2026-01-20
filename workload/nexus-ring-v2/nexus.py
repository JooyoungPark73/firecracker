#!/usr/bin/env python3
"""
Nexus Shared Memory SDK v2 - Zero-VSOCK Critical Path
Hybrid Access Model: mmap for control+metadata (4KB), pread/pwrite for data (4KB-2MB)
"""

import os
import mmap
import ctypes
import time
from typing import Optional


# Global Control Structure (128 bytes at offset 0)
class NexusControl(ctypes.Structure):
    _pack_ = 8
    _fields_ = [
        ("session_id", ctypes.c_uint32),
        ("direction", ctypes.c_uint32),      # 0=Idle, 1=G2H, 2=H2G
        ("data_write_head", ctypes.c_uint64),  # Monotonic byte counter for data ring
        ("data_read_tail", ctypes.c_uint64),   # Monotonic byte counter for data ring
        ("msg_write_idx", ctypes.c_uint32),    # Current write index (0-30)
        ("msg_read_idx", ctypes.c_uint32),     # Current read index (0-30)
        ("status", ctypes.c_uint32),           # 0=OK, 1=Error
        ("padding", ctypes.c_uint32),
        # Pad to 128 bytes for cache alignment
        ("_reserved", ctypes.c_uint8 * 96),
    ]


# Message Metadata Entry (32 bytes each)
class MessageMeta(ctypes.Structure):
    _pack_ = 8
    _fields_ = [
        ("length", ctypes.c_uint64),        # Payload size
        ("data_offset", ctypes.c_uint64),   # Position in data ring (byte offset)
        ("flags", ctypes.c_uint32),         # 0=empty, 1=ready, 2=consumed
        ("seq_num", ctypes.c_uint32),       # Sequence number for ordering
        ("_padding", ctypes.c_uint64),      # Pad to 32 bytes
    ]


# Metadata Ring Array (31 entries × 32 bytes = 992 bytes at offset 128)
class MetadataRing(ctypes.Structure):
    _pack_ = 8
    _fields_ = [
        ("entries", MessageMeta * 31),
    ]


class NexusDevice:
    """
    Nexus Device with Metadata Ring Coordination.
    - Control + Metadata (0-4KB): Memory-mapped for atomic coordination
    - Data Region (4KB-2MB): File I/O for payload transfer
    """
    
    DEVICE_PATH = "/dev/nexus0"
    TOTAL_SIZE = 2 * 1024 * 1024  # 2MB
    CONTROL_SIZE = 4096            # 4KB (128 control + 992 metadata + padding)
    DATA_OFFSET = 4096
    DATA_CAPACITY = TOTAL_SIZE - CONTROL_SIZE  # 2,093,056 bytes
    
    MAX_CHUNK_SIZE = 64 * 1024     # 64KB per message
    MAX_INFLIGHT = 31              # Maximum in-flight messages
    
    # Direction constants
    DIR_IDLE = 0
    DIR_G2H = 1
    DIR_H2G = 2
    
    # Status constants
    STATUS_OK = 0
    STATUS_ERROR = 1
    
    # Message flags
    FLAG_EMPTY = 0
    FLAG_READY = 1
    FLAG_CONSUMED = 2
    
    def __init__(self, device_path: str = DEVICE_PATH):
        """Initialize Nexus device with metadata ring."""
        self.fd = os.open(device_path, os.O_RDWR | os.O_SYNC)
        
        # Map ONLY the control region (first 4KB)
        self.mm = mmap.mmap(
            self.fd, 
            self.CONTROL_SIZE,
            mmap.MAP_SHARED,
            mmap.PROT_READ | mmap.PROT_WRITE,
            offset=0
        )
        
        # Overlay control structure at offset 0
        self.ctrl = NexusControl.from_buffer(self.mm, 0)
        
        # Overlay metadata ring at offset 128
        self.meta_ring = MetadataRing.from_buffer(self.mm, 128)
        
    def close(self):
        """Clean up resources."""
        # Delete ctypes structure references before closing mmap
        if hasattr(self, 'ctrl'):
            del self.ctrl
        if hasattr(self, 'meta_ring'):
            del self.meta_ring
        if hasattr(self, 'mm'):
            self.mm.close()
        if hasattr(self, 'fd'):
            os.close(self.fd)
    
    def reset_session(self, session_id: int, direction: int):
        """Initialize a new session and clear metadata ring."""
        self.ctrl.session_id = session_id
        self.ctrl.direction = direction
        self.ctrl.data_write_head = 0
        self.ctrl.data_read_tail = 0
        self.ctrl.msg_write_idx = 0
        self.ctrl.msg_read_idx = 0
        self.ctrl.status = self.STATUS_OK
        
        # Clear all metadata entries
        for i in range(self.MAX_INFLIGHT):
            self.meta_ring.entries[i].flags = self.FLAG_EMPTY
            self.meta_ring.entries[i].length = 0
            self.meta_ring.entries[i].data_offset = 0
            self.meta_ring.entries[i].seq_num = 0
    
    def _write_chunk(self, data: bytes, head: int) -> int:
        """
        Write data to ring buffer using split writes if needed.
        Returns number of bytes written.
        """
        chunk_size = len(data)
        ring_index = head % self.DATA_CAPACITY
        space_at_end = self.DATA_CAPACITY - ring_index
        
        if chunk_size <= space_at_end:
            # Single write
            os.pwrite(self.fd, data, self.DATA_OFFSET + ring_index)
        else:
            # Split write: wrap around
            first_part = data[:space_at_end]
            second_part = data[space_at_end:]
            os.pwrite(self.fd, first_part, self.DATA_OFFSET + ring_index)
            os.pwrite(self.fd, second_part, self.DATA_OFFSET)
        
        return chunk_size
    
    def _read_chunk(self, size: int, tail: int) -> bytes:
        """
        Read data from ring buffer using split reads if needed.
        Returns bytes read.
        """
        ring_index = tail % self.DATA_CAPACITY
        space_at_end = self.DATA_CAPACITY - ring_index
        
        if size <= space_at_end:
            # Single read
            return os.pread(self.fd, size, self.DATA_OFFSET + ring_index)
        else:
            # Split read: wrap around
            first_part = os.pread(self.fd, space_at_end, self.DATA_OFFSET + ring_index)
            second_part = os.pread(self.fd, size - space_at_end, self.DATA_OFFSET)
            return first_part + second_part
    
    def write_available(self) -> int:
        """Calculate available write space (single-producer)."""
        head = self.ctrl.data_write_head
        tail = self.ctrl.data_read_tail
        used = head - tail
        return self.DATA_CAPACITY - used
    
    def read_available(self) -> int:
        """Calculate available read bytes (single-consumer)."""
        head = self.ctrl.data_write_head
        tail = self.ctrl.data_read_tail
        return head - tail
    
    def push(self, data: bytes, timeout: float = 5.0) -> bool:
        """
        Push data to ring buffer (producer side).
        Automatically chunks large payloads to support data larger than ring capacity.
        Uses metadata ring for coordination instead of vsock.
        """
        start = time.monotonic()
        total_len = len(data)
        written = 0
        
        # Get current metadata slot
        write_idx = self.ctrl.msg_write_idx
        meta_entry = self.meta_ring.entries[write_idx]
        
        # Wait for consumer to consume previous message in this slot
        while meta_entry.flags != self.FLAG_EMPTY and meta_entry.flags != self.FLAG_CONSUMED:
            if time.monotonic() - start > timeout:
                return False
            # Pure busy-wait, no sleep
        
        # Record starting position for metadata
        start_offset = self.ctrl.data_write_head
        
        # Write metadata FIRST so consumer can start reading immediately
        # This is critical for messages larger than ring capacity
        meta_entry.length = total_len
        meta_entry.data_offset = start_offset
        meta_entry.seq_num = write_idx
        meta_entry.flags = self.FLAG_READY  # Signal consumer to start reading NOW
        
        # Write data in chunks
        while written < total_len:
            # Wait for space in data ring
            while True:
                available = self.write_available()
                if available > 0:
                    break
                
                if time.monotonic() - start > timeout:
                    return False
                # Pure busy-wait, no sleep
            
            # Determine chunk size (limited by available space and MAX_CHUNK_SIZE)
            remaining = total_len - written
            to_write = min(remaining, available, self.MAX_CHUNK_SIZE)
            
            # Write chunk to ring buffer
            head = self.ctrl.data_write_head
            chunk = data[written : written + to_write]
            self._write_chunk(chunk, head)
            
            # Update control (allows consumer to start reading while we continue writing)
            self.ctrl.data_write_head = head + to_write
            written += to_write
        
        # Metadata already set at the beginning (FLAG_READY)
        # Consumer may already be reading at this point
        
        # Advance write index
        self.ctrl.msg_write_idx = (write_idx + 1) % self.MAX_INFLIGHT
        
        return True
    
    def pull(self, size: int, timeout: float = 5.0) -> Optional[bytes]:
        """
        Pull data from ring buffer (consumer side).
        Automatically chunks large reads to support data larger than ring capacity.
        Uses metadata ring for coordination instead of vsock.
        """
        start = time.monotonic()
        
        # Get current metadata slot
        read_idx = self.ctrl.msg_read_idx
        meta_entry = self.meta_ring.entries[read_idx]
        
        # Busy-wait for producer to mark message as ready
        while meta_entry.flags != self.FLAG_READY:
            if time.monotonic() - start > timeout:
                return None
            # Busy wait (no sleep for lowest latency)
            pass
        
        # Verify expected size matches
        if meta_entry.length != size:
            self.ctrl.status = self.STATUS_ERROR
            return None
        
        # Read data in chunks
        received = 0
        message_buffer = bytearray(size)
        tail = meta_entry.data_offset
        
        while received < size:
            # Wait for data to be available
            while True:
                head = self.ctrl.data_write_head
                available = head - (tail + received)
                if available > 0:
                    break
                
                if time.monotonic() - start > timeout:
                    return None
                # Pure busy-wait, no sleep
            
            # Determine chunk size
            remaining = size - received
            to_read = min(remaining, available, self.MAX_CHUNK_SIZE)
            
            # Read chunk from ring buffer
            chunk = self._read_chunk(to_read, tail + received)
            message_buffer[received : received + to_read] = chunk
            received += to_read
            
            # Update control incrementally (frees up space for producer)
            self.ctrl.data_read_tail = tail + received
        
        # All data received, mark as consumed
        meta_entry.flags = self.FLAG_CONSUMED
        
        # Advance read index
        self.ctrl.msg_read_idx = (read_idx + 1) % self.MAX_INFLIGHT
        
        return bytes(message_buffer)
    
    def get_control_state(self) -> dict:
        """Get current control state for debugging."""
        return {
            'session_id': self.ctrl.session_id,
            'direction': self.ctrl.direction,
            'data_write_head': self.ctrl.data_write_head,
            'data_read_tail': self.ctrl.data_read_tail,
            'msg_write_idx': self.ctrl.msg_write_idx,
            'msg_read_idx': self.ctrl.msg_read_idx,
            'status': self.ctrl.status,
        }


if __name__ == "__main__":
    # Simple self-test
    print("Nexus Python SDK v2 (Zero-VSOCK)")
    print(f"Control size: {NexusDevice.CONTROL_SIZE} bytes")
    print(f"Data capacity: {NexusDevice.DATA_CAPACITY:,} bytes")
    print(f"Max chunk: {NexusDevice.MAX_CHUNK_SIZE:,} bytes")
    print(f"Max in-flight: {NexusDevice.MAX_INFLIGHT} messages")
    print(f"Total size: {NexusDevice.TOTAL_SIZE:,} bytes")
