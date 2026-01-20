#!/usr/bin/env python3
"""
Guest-side server for ring buffer benchmark using Nexus SDK
"""

import os
import sys
import time
import socket
import struct
import hashlib

# Add current directory to path to import nexus
sys.path.insert(0, os.path.dirname(__file__))
from .nexus import NexusDevice

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

def handle_guest_to_host(conn, dev, payload_size):
    """Handle G->H transfer: generate data, measure E2E time, send timing."""
    # Generate data and MD5 outside timing region
    test_data = os.urandom(payload_size)
    expected_md5 = hashlib.md5(test_data).hexdigest()
    
    # Send MD5 first
    conn.sendall(expected_md5.encode('ascii'))
    
    # E2E timing: push to ring buffer + wait for host acknowledgement (not verification)
    start_time = time.time_ns()
    if not dev.push(test_data, timeout=10.0):
        print(f"Push failed G->H")
        return False
    ack = recv_exact(conn, 1)  # Wait for host read acknowledgement
    end_time = time.time_ns()
    
    elapsed_us = (end_time - start_time) // 1000
    
    # Send timing back to host
    conn.sendall(struct.pack('!Q', elapsed_us))
    
    # Now receive verification result (outside timing)
    verification = recv_exact(conn, 1)
    
    return verification == b'1'

def handle_host_to_guest(conn, dev, payload_size):
    """Handle H->G transfer: receive data, measure E2E time, send timing."""
    # Receive MD5 first
    expected_md5_bytes = recv_exact(conn, 32)
    expected_md5 = expected_md5_bytes.decode('ascii')
    
    # E2E timing: pull from ring buffer + send acknowledgement
    start_time = time.time_ns()
    msg = dev.pull(payload_size, timeout=10.0)
    conn.sendall(b'1')  # Send acknowledgement that read is complete
    end_time = time.time_ns()
    
    elapsed_us = (end_time - start_time) // 1000
    
    # Send timing back to host
    conn.sendall(struct.pack('!Q', elapsed_us))
    
    # Verify outside timing and send result
    verified = False
    if msg:
        received_md5 = hashlib.md5(msg).hexdigest()
        verified = (received_md5 == expected_md5)
    conn.sendall(b'1' if verified else b'0')
    
    return verified

def run_server():
    if not os.path.exists(NexusDevice.DEVICE_PATH):
        print(f"Error: {NexusDevice.DEVICE_PATH} not found")
        return 1
    
    print(f"Starting Nexus Server on Port {PORT}")
    
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
            
            if payload_size == 0: break
            
            print(f"Config: {payload_size:,} bytes x {iterations}")
            
            # 2. Open Device
            dev = NexusDevice()
            
            # 4. Run iterations
            for i in range(iterations):
                # Lock-step: Host always sends first in client.py
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