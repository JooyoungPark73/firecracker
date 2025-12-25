#!/usr/bin/env python3
import os
import mmap
import ctypes
import socket
import time

# --- 1. Robust Framing Helper ---
def recv_until_newline(sock: socket.socket) -> bytes:
    """Read from socket until a newline is found."""
    buf = b""
    while True:
        chunk = sock.recv(4096)
        if not chunk:
            raise ConnectionError("Socket closed")
        buf += chunk
        if b"\n" in buf:
            message, leftover = buf.split(b"\n", 1)
            return message

# --- 2. Explicit Struct Alignment ---
class KhalaControl(ctypes.Structure):
    _pack_ = 8
    _fields_ = [
        ("direction", ctypes.c_uint32),
        ("status", ctypes.c_uint32),
        ("write_head", ctypes.c_uint64),
        ("read_tail", ctypes.c_uint64),
        ("session_id", ctypes.c_uint32),
        ("padding", ctypes.c_uint32),
    ]

class KhalaDevice:
    DEVICE_PATH = "/dev/khala0"
    TOTAL_SIZE = 2 * 1024 * 1024
    CONTROL_SIZE = 4096
    DATA_OFFSET = 4096
    CAPACITY = TOTAL_SIZE - CONTROL_SIZE
    
    # Tuning: Split large transfers into smaller atomic updates
    # 256KB - 512KB is usually the sweet spot for L2 Cache pipelining
    MAX_CHUNK_SIZE = 64 * 1024 
    
    DIR_IDLE = 0
    DIR_G2H = 1
    DIR_H2G = 2
    STATUS_OK = 0
    STATUS_ERROR = 1

    def __init__(self, device_path: str = DEVICE_PATH):
        self.fd = os.open(device_path, os.O_RDWR)
        
        self.mm = mmap.mmap(
            self.fd, 
            self.CONTROL_SIZE, 
            mmap.MAP_SHARED, 
            mmap.PROT_READ | mmap.PROT_WRITE
        )
        self.ctrl = KhalaControl.from_buffer(self.mm)

    def close(self):
        if hasattr(self, 'ctrl'):
            del self.ctrl
        if hasattr(self, 'mm'):
            self.mm.close()
            del self.mm
        if hasattr(self, 'fd'):
            os.close(self.fd)
            del self.fd

    def reset_session(self, session_id: int, direction: int):
        self.ctrl.session_id = session_id
        self.ctrl.direction = direction
        self.ctrl.write_head = 0
        self.ctrl.read_tail = 0
        self.ctrl.status = self.STATUS_OK

    def push(self, data: bytes, timeout: float = 5.0) -> bool:
        total_len = len(data)
        written = 0
        start = time.time()
        
        while written < total_len:
            # 1. Wait for space
            while True:
                head = self.ctrl.write_head
                tail = self.ctrl.read_tail
                available = self.CAPACITY - (head - tail)
                
                if available > 0:
                    break
                
                if time.time() - start > timeout:
                    return False
            
            # 2. Determine chunk size
            # CAP the transfer size to MAX_CHUNK_SIZE to force frequent updates
            remaining = total_len - written
            to_write = min(remaining, available, self.MAX_CHUNK_SIZE)
            
            # 3. Write Data
            ring_idx = head % self.CAPACITY
            space_at_end = self.CAPACITY - ring_idx
            
            chunk = data[written : written + to_write]
            
            if to_write <= space_at_end:
                os.pwrite(self.fd, chunk, self.DATA_OFFSET + ring_idx)
            else:
                os.pwrite(self.fd, chunk[:space_at_end], self.DATA_OFFSET + ring_idx)
                os.pwrite(self.fd, chunk[space_at_end:], self.DATA_OFFSET)

            # 4. Atomic Update
            # This is the key: We update 'head' frequently so the receiver 
            # can start reading immediately, even if we have more data to send.
            self.ctrl.write_head = head + to_write
            written += to_write
            
        return True

    def pull(self, size: int, timeout: float = 5.0) -> bytes:
        received = 0
        chunks = []
        message_buffer = bytearray(size)
        start = time.time()
        
        while received < size:
            # 1. Wait for data
            while True:
                head = self.ctrl.write_head
                tail = self.ctrl.read_tail
                available = head - tail
                
                if available > 0:
                    break
                
                if time.time() - start > timeout:
                    return b"" 

            # 2. Determine chunk size
            # Also cap read size to keep the loop tight and responsive
            remaining = size - received
            to_read = min(remaining, available, self.MAX_CHUNK_SIZE)
            
            # 3. Read Data
            ring_idx = tail % self.CAPACITY
            space_at_end = self.CAPACITY - ring_idx
            
            if to_read <= space_at_end:
                chunk = os.pread(self.fd, to_read, self.DATA_OFFSET + ring_idx)
            else:
                p1 = os.pread(self.fd, space_at_end, self.DATA_OFFSET + ring_idx)
                p2 = os.pread(self.fd, to_read - space_at_end, self.DATA_OFFSET)
                chunk = p1 + p2
            
            message_buffer[received:received+to_read] = chunk

            # 4. Atomic Update
            # Free up space for the sender immediately
            self.ctrl.read_tail = tail + to_read
            received += to_read
            
        return bytes(message_buffer)
