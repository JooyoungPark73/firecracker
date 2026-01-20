import socket
import time
import mmap
import os
import struct
import hashlib
import csv
from datetime import datetime

AF_VSOCK = 40
PORT = 9000
MMAP_SIZE = 16 * 1024 * 1024  # Fixed 16MB

def read_file_message(sock, fd):
    """Read message from file after receiving length via socket."""
    raw_len = sock.recv(4)
    if not raw_len or len(raw_len) < 4:
        return None
    msg_len = struct.unpack('!I', raw_len)[0]
    # Read from file at offset 0
    data = os.pread(fd, msg_len, 0)
    if len(data) < msg_len:
        return None
    return data

def write_file_message(sock, fd, data):
    """Write message to file and send length via socket."""
    # Write to file at offset 0
    os.pwrite(fd, data, 0)
    sock.sendall(struct.pack('!I', len(data)))


def run_benchmark(mmap_path='/dev/shm/nexus_region', vsock_socket_path="/tmp/v.sock", 
                 payload_sizes=None, iterations=10, output_csv='benchmark_results.csv'):
    """
    Run E2E latency benchmark for mmap-based communication.
    
    Args:
        mmap_path: Path to shared memory device
        vsock_socket_path: Path to vsock Unix socket
        payload_sizes: List of payload sizes to test (in bytes)
        iterations: Number of iterations per payload size
        output_csv: Output CSV file path
    """
    if payload_sizes is None:
        payload_sizes = [10, 10*1024, 1024*1024, 10*1024*1024]
    
    # Open shared memory file once
    f = os.open(mmap_path, os.O_RDWR)
    
    print(f"Testing payload sizes: {[f'{s:,}' for s in payload_sizes]} bytes")
    print(f"Iterations per size: {iterations}")
    print(f"Output file: {output_csv}")
    print("="*80)
    
    # Prepare CSV file
    csv_file = open(output_csv, 'w', newline='')
    csv_writer = csv.writer(csv_file)
    csv_writer.writerow(['timestamp', 'payload_size_bytes', 'iteration', 'direction', 
                        'latency_us', 'verified'])
    
    results = []
    
    try:
        for payload_size in payload_sizes:
            print(f"\nTesting payload size: {payload_size:,} bytes")
            
            # Create new connection for each payload size
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            sock.connect(vsock_socket_path)
            
            # Send CONNECT command
            connect_cmd = f"CONNECT {PORT}\n"
            sock.sendall(connect_cmd.encode('ascii'))
            
            # Read acknowledgement
            response = b""
            while b'\n' not in response:
                chunk = sock.recv(1024)
                if not chunk:
                    raise Exception("Connection closed before receiving OK")
                response += chunk
            
            response_str = response.decode('ascii').strip()
            if not response_str.startswith("OK"):
                raise Exception(f"Expected OK response, got: {response_str}")
            
            # Send test configuration to server
            config_data = struct.pack('!Q', payload_size) + struct.pack('!I', iterations)
            sock.sendall(config_data)
            
            for i in range(iterations):
                timestamp = datetime.now().isoformat()
                
                # --- Host -> Guest ---
                # Generate data and MD5 outside timing region
                host_data = os.urandom(payload_size)
                host_md5 = hashlib.md5(host_data).hexdigest()
                
                # Send MD5 to guest
                sock.sendall(host_md5.encode('ascii'))
                
                # E2E timing: write to mmap + wait for guest acknowledgement
                start_time = time.time_ns()
                write_file_message(sock, f, host_data)
                ack = sock.recv(1)  # Wait for guest read acknowledgement
                end_time = time.time_ns()
                
                h_to_g_latency = (end_time - start_time) // 1000  # Convert to microseconds
                
                # Receive guest's H->G timing
                guest_timing_data = sock.recv(8)
                if len(guest_timing_data) != 8:
                    print(f"  Error: Failed to receive guest timing at iteration {i}")
                    break
                guest_h_to_g_latency = struct.unpack('!Q', guest_timing_data)[0]
                
                # Receive verification result (outside timing)
                verification = sock.recv(1)
                h_to_g_verified = (verification == b'1')
                
                # Log H->G result (using host measurement)
                csv_writer.writerow([timestamp, payload_size, i, 'H->G', 
                                   h_to_g_latency, int(h_to_g_verified)])
                
                # --- Guest -> Host ---
                # Receive MD5 from guest
                expected_md5_bytes = sock.recv(32)
                if len(expected_md5_bytes) != 32:
                    print(f"  Error: Failed to receive MD5 at iteration {i}")
                    break
                expected_md5 = expected_md5_bytes.decode('ascii')
                
                # Read from mmap (guest is timing until we send acknowledgement)
                msg = read_file_message(sock, f)
                if msg is None:
                    print(f"  Error: Failed to receive message at iteration {i}")
                    sock.sendall(b'0')  # Send ack even on error
                    break
                
                # Send acknowledgement to complete guest's E2E timing
                sock.sendall(b'1')
                
                # Receive timing from guest
                timing_data = sock.recv(8)
                if len(timing_data) != 8:
                    print(f"  Error: Failed to receive timing at iteration {i}")
                    break
                g_to_h_latency = struct.unpack('!Q', timing_data)[0]
                
                # Verify data (outside timing)
                received_md5 = hashlib.md5(msg).hexdigest()
                g_to_h_verified = (received_md5 == expected_md5)
                
                # Send verification result
                sock.sendall(b'1' if g_to_h_verified else b'0')
                
                # Log G->H result
                csv_writer.writerow([timestamp, payload_size, i, 'G->H', 
                                   g_to_h_latency, int(g_to_h_verified)])
                
                # Console output
                h_status = "✓" if h_to_g_verified else "✗"
                g_status = "✓" if g_to_h_verified else "✗"
                print(f"  Iter {i:2d}: H->G={h_to_g_latency:8,}μs {h_status}  |  "
                      f"G->H={g_to_h_latency:8,}μs {g_status}")
                
                results.append({
                    'payload_size': payload_size,
                    'iteration': i,
                    'g_to_h_latency': g_to_h_latency,
                    'h_to_g_latency': h_to_g_latency,
                    'g_to_h_verified': g_to_h_verified,
                    'h_to_g_verified': h_to_g_verified
                })
            
            # Close connection for this payload size
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
        os.close(f)
    
    print("\n" + "="*80)
    print(f"Benchmark complete. Results saved to {output_csv}")
    return results

if __name__ == "__main__":
    run_benchmark(
        mmap_path='/dev/shm/nexus_region',
        vsock_socket_path='/tmp/v.sock',
        # 16B to 16MB in power of 2 steps
        payload_sizes=[2**x for x in range(4, 25)],
        iterations=10,
        output_csv='benchmark_results.csv'
    )

    