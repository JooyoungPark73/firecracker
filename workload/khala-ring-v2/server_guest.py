#!/usr/bin/env python3
"""
Guest-side server for khala-ring-v2 benchmark
Zero-VSOCK in critical path, uses metadata ring for coordination
"""

import os
import sys
import time
import socket
import struct
import hashlib

# Add current directory to path to import khala
sys.path.insert(0, os.path.dirname(__file__))
from khala import KhalaDevice

AF_VSOCK = 40
VMADDR_CID_ANY = 0xFFFFFFFF
PORT = 9000

def recv_exact(sock, n):
    data = b''
    while len(data) < n:
        chunk = sock.recv(n - len(data))
        if not chunk:
            raise ConnectionError(f"Socket closed. Expected {n} bytes, got {len(data)}")
        data += chunk
    return data

def warmup_device(dev):
    """Force OS to allocate pages."""
    _ = dev.mm[:]
    os.pread(dev.fd, 4096, dev.DATA_OFFSET)

def handle_host_to_guest(conn, dev, payload_size):
    """
    Handle H->G transfer.
    VSOCK only for MD5 and timing data (NOT in critical path).
    """
    # Receive MD5 via VSOCK (outside timing)
    expected_md5_bytes = recv_exact(conn, 32)
    expected_md5 = expected_md5_bytes.decode('ascii')
    
    # *** CRITICAL PATH START ***
    # No VSOCK calls in this section!
    start_time = time.time_ns()
    
    msg = dev.pull(payload_size, timeout=10.0)
    
    end_time = time.time_ns()
    # *** CRITICAL PATH END ***
    # (Metadata flag FLAG_CONSUMED is set in pull(), host can see it)
    
    elapsed_us = (end_time - start_time) // 1000
    
    # Send timing back to host via VSOCK (outside timing)
    conn.sendall(struct.pack('!Q', elapsed_us))
    
    # Verify outside timing
    verified = False
    if msg:
        received_md5 = hashlib.md5(msg).hexdigest()
        verified = (received_md5 == expected_md5)
    
    # Send verification via VSOCK (outside timing)
    conn.sendall(b'1' if verified else b'0')
    
    return verified

def handle_guest_to_host(conn, dev, payload_size):
    """
    Handle G->H transfer.
    VSOCK only for MD5 and timing data (NOT in critical path).
    """
    # Generate data and MD5 outside timing region
    test_data = os.urandom(payload_size)
    expected_md5 = hashlib.md5(test_data).hexdigest()
    
    # Send MD5 via VSOCK (outside timing)
    conn.sendall(expected_md5.encode('ascii'))
    
    # *** CRITICAL PATH START ***
    # No VSOCK calls in this section!
    start_time = time.time_ns()
    
    if not dev.push(test_data, timeout=10.0):
        print(f"Push failed G->H")
        return False
    
    # Wait for host to consume via metadata flag (busy-wait in pull())
    # We need to wait for FLAG_CONSUMED to complete our timing
    write_idx = (dev.ctrl.msg_write_idx - 1) % dev.MAX_INFLIGHT
    meta_entry = dev.meta_ring.entries[write_idx]
    
    # Busy-wait for host to mark as consumed
    timeout_start = time.monotonic()
    while meta_entry.flags != dev.FLAG_CONSUMED:
        if time.monotonic() - timeout_start > 10.0:
            return False
        pass  # Busy wait
    
    end_time = time.time_ns()
    # *** CRITICAL PATH END ***
    
    elapsed_us = (end_time - start_time) // 1000
    
    # Send timing back to host via VSOCK (outside timing)
    conn.sendall(struct.pack('!Q', elapsed_us))
    
    # Receive verification result via VSOCK (outside timing)
    verification = recv_exact(conn, 1)
    
    return verification == b'1'

def run_server():
    if not os.path.exists(KhalaDevice.DEVICE_PATH):
        print(f"Error: {KhalaDevice.DEVICE_PATH} not found")
        return 1
    
    print(f"Starting Khala v2 Server on Port {PORT} (Zero-VSOCK Critical Path)")
    
    listen_sock = socket.socket(AF_VSOCK, socket.SOCK_STREAM)
    listen_sock.bind((VMADDR_CID_ANY, PORT))
    listen_sock.listen(1)
    
    while True:
        conn, addr = listen_sock.accept()
        print(f"New connection from CID {addr[0]}")
        
        try:
            # 1. Receive Config
            config_data = recv_exact(conn, 12)
            payload_size = struct.unpack('!Q', config_data[:8])[0]
            iterations = struct.unpack('!I', config_data[8:12])[0]
            
            if payload_size == 0:
                break
            
            print(f"Config: {payload_size:,} bytes x {iterations}")
            
            # 2. Open Device
            dev = KhalaDevice()
            
            # 3. Warmup
            warmup_device(dev)
            
            # 4. Run iterations (host controls session reset)
            for i in range(iterations):
                # Lock-step: Host always sends first
                if not handle_host_to_guest(conn, dev, payload_size):
                    print(f"Iter {i} H->G failed")
                    break
                    
                if not handle_guest_to_host(conn, dev, payload_size):
                    print(f"Iter {i} G->H failed")
                    break
            
            dev.close()
            print("Test complete")
            
        except Exception as e:
            print(f"Error: {e}")
            import traceback
            traceback.print_exc()
        finally:
            conn.close()
            
    listen_sock.close()

if __name__ == "__main__":
    run_server()
