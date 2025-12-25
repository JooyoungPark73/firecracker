#!/usr/bin/env python3
"""
Host-side client for khala-ring-v2 benchmark
Zero-VSOCK in critical path, uses metadata ring for coordination
"""

import os
import sys
import socket
import time
import struct
import hashlib
import csv
from datetime import datetime

# Add current directory to path to import khala
sys.path.insert(0, os.path.dirname(__file__))
from khala import KhalaDevice

AF_VSOCK = 40
PORT = 9000

def recv_exact(sock, n):
    """Helper to ensure we receive exactly n bytes."""
    data = b''
    while len(data) < n:
        chunk = sock.recv(n - len(data))
        if not chunk:
            raise ConnectionError(f"Socket closed. Expected {n} bytes, got {len(data)}")
        data += chunk
    return data

def warmup_device(dev):
    """Force OS to allocate physical pages for the control and data regions."""
    # Read the whole control region
    _ = dev.mm[:]
    # Write a zero to the last byte of data region to force max file size allocation
    os.pwrite(dev.fd, b'\0', dev.DATA_OFFSET + dev.DATA_CAPACITY - 1)
    # Read the start and end of data region to trigger IO faults
    os.pread(dev.fd, 4096, dev.DATA_OFFSET)

def run_benchmark(device_path='/dev/shm/khala_region', vsock_socket_path="/tmp/v.sock", 
                 payload_sizes=None, iterations=10, output_csv='benchmark_results.csv'):
    """
    Run E2E latency benchmark with metadata ring coordination.
    VSOCK only used for MD5 exchange and timing data (NOT in critical path).
    """
    if payload_sizes is None:
        payload_sizes = [2**x for x in range(4, 25)]
    
    print(f"Khala Ring Buffer v2 Benchmark (Host) - Zero-VSOCK Critical Path")
    print(f"Testing sizes: {[f'{s:,}' for s in payload_sizes]} bytes")
    print("="*80)
    
    csv_file = open(output_csv, 'w', newline='')
    csv_writer = csv.writer(csv_file)
    csv_writer.writerow(['timestamp', 'payload_size_bytes', 'iteration', 'direction', 
                        'latency_us', 'verified'])
    
    results = []
    
    try:
        for payload_size in payload_sizes:
            print(f"\nTesting payload size: {payload_size:,} bytes")
            
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            sock.connect(vsock_socket_path)
            
            # 1. Handshake
            sock.sendall(f"CONNECT {PORT}\n".encode('ascii'))
            
            # Robust read until newline for OK
            response = b""
            while b'\n' not in response:
                response += sock.recv(1024)
            if not response.strip().startswith(b"OK"):
                raise Exception("Server refused connection")
            
            # 2. Send Config
            config_data = struct.pack('!Q', payload_size) + struct.pack('!I', iterations)
            sock.sendall(config_data)
            
            # 3. Setup Device & Session
            dev = KhalaDevice(device_path)
            
            # Warmup
            warmup_device(dev)
            
            # Reset session
            session_id = int(time.time()) & 0xFFFFFFFF
            dev.reset_session(session_id, KhalaDevice.DIR_H2G)

            for i in range(iterations):
                timestamp = datetime.now().isoformat()
                
                # --- PHASE 1: Host -> Guest ---
                host_data = os.urandom(payload_size)
                host_md5 = hashlib.md5(host_data).hexdigest()
                
                # Send MD5 via VSOCK (outside timing)
                sock.sendall(host_md5.encode('ascii'))
                
                # *** CRITICAL PATH START ***
                # No VSOCK calls in this section!
                start_time = time.time_ns()
                
                if not dev.push(host_data, timeout=10.0):
                    raise TimeoutError("Push timeout")
                
                # Wait for guest to consume via metadata flag (busy-wait, no vsock)
                # The pull() on guest side will mark FLAG_CONSUMED
                end_time = time.time_ns()
                # *** CRITICAL PATH END ***
                
                h_to_g_latency = (end_time - start_time) // 1000  # Convert to microseconds
                
                # Receive guest's H->G timing via VSOCK (outside timing)
                guest_timing_data = recv_exact(sock, 8)
                guest_h_to_g_latency = struct.unpack('!Q', guest_timing_data)[0]
                
                # Receive verification result via VSOCK (outside timing)
                verification = recv_exact(sock, 1)
                h_to_g_verified = (verification == b'1')
                
                # Log H->G result
                csv_writer.writerow([timestamp, payload_size, i, 'H->G', 
                                   guest_h_to_g_latency, int(h_to_g_verified)])
                
                # --- PHASE 2: Guest -> Host ---
                # Receive MD5 from guest via VSOCK (outside timing)
                expected_md5_bytes = recv_exact(sock, 32)
                expected_md5 = expected_md5_bytes.decode('ascii')
                
                # *** CRITICAL PATH START ***
                # No VSOCK calls in this section!
                start_time = time.time_ns()
                
                msg = dev.pull(payload_size, timeout=10.0)
                if msg is None:
                    print(f"  Error: Failed to receive message at iteration {i}")
                    break
                
                end_time = time.time_ns()
                # *** CRITICAL PATH END ***
                # (Guest's timing ends when it sees FLAG_CONSUMED)
                
                # Note: We don't measure our own G->H timing, guest measures it
                # But we calculate it for logging purposes
                h_pull_latency = (end_time - start_time) // 1000
                
                # Receive timing from guest via VSOCK (outside timing)
                timing_data = recv_exact(sock, 8)
                g_to_h_latency = struct.unpack('!Q', timing_data)[0]
                
                # Verify data (outside timing)
                received_md5 = hashlib.md5(msg).hexdigest()
                g_to_h_verified = (received_md5 == expected_md5)
                
                # Send verification result via VSOCK (outside timing)
                sock.sendall(b'1' if g_to_h_verified else b'0')
                
                # Log G->H result (using guest's measurement)
                csv_writer.writerow([timestamp, payload_size, i, 'G->H', 
                                   g_to_h_latency, int(g_to_h_verified)])
                
                # Console output
                h_status = "✓" if h_to_g_verified else "✗"
                g_status = "✓" if g_to_h_verified else "✗"
                print(f"  Iter {i:2d}: H->G={guest_h_to_g_latency:8,}μs {h_status}  |  "
                      f"G->H={g_to_h_latency:8,}μs {g_status}")
                
                results.append({
                    'payload_size': payload_size,
                    'iteration': i,
                    'g_to_h_latency': g_to_h_latency,
                    'h_to_g_latency': guest_h_to_g_latency,
                    'g_to_h_verified': g_to_h_verified,
                    'h_to_g_verified': h_to_g_verified
                })
            
            dev.close()
            sock.close()
            
            # Print summary statistics
            if results:
                subset = [r for r in results if r['payload_size'] == payload_size]
                if subset:
                    h_to_g_avg = sum(r['h_to_g_latency'] for r in subset) / len(subset)
                    g_to_h_avg = sum(r['g_to_h_latency'] for r in subset) / len(subset)
                    print(f"  Summary: H->G avg={h_to_g_avg:,.1f}μs  |  G->H avg={g_to_h_avg:,.1f}μs")
        
    finally:
        csv_file.close()
    
    print("\n" + "="*80)
    print(f"Benchmark complete. Results saved to {output_csv}")
    return results

if __name__ == "__main__":
    run_benchmark(
        device_path='/dev/shm/khala_region',
        vsock_socket_path='/tmp/v.sock',
        # Test from 16B to 16MB in power of 2 steps
        payload_sizes=[2**x for x in range(4, 25)], 
        iterations=10,
        output_csv='benchmark_results.csv'
    )
