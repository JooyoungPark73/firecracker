import socket
import time
import os
import struct
import hashlib
import csv
from datetime import datetime

HOST_IP = "10.0.1.2"
GUEST_IP = "10.10.1.2"
PORT = 9000

def send_data(sock, data):
    """Send data with 4-byte length prefix."""
    sock.sendall(struct.pack('!I', len(data)))
    sock.sendall(data)

def recv_data(sock):
    """Receive data with 4-byte length prefix."""
    raw_len = sock.recv(4)
    if not raw_len or len(raw_len) < 4:
        return None
    msg_len = struct.unpack('!I', raw_len)[0]
    
    data = b''
    while len(data) < msg_len:
        chunk = sock.recv(min(msg_len - len(data), 65536))
        if not chunk:
            return None
        data += chunk
    return data

def run_benchmark(payload_sizes=None, iterations=10, output_csv='benchmark_results.csv'):
    """
    Run E2E latency benchmark for TCP communication.
    
    Args:
        payload_sizes: List of payload sizes to test (in bytes)
        iterations: Number of iterations per payload size
        output_csv: Output CSV file path
    """
    if payload_sizes is None:
        payload_sizes = [10, 10*1024, 1024*1024, 10*1024*1024]
    
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
            
            # Create new TCP connection for each payload size
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            sock.connect((GUEST_IP, PORT))
            
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
                
                # E2E timing: send data over TCP + wait for guest acknowledgement
                start_time = time.time_ns()
                send_data(sock, host_data)
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
                
                # Receive data over TCP (guest is timing until we send acknowledgement)
                msg = recv_data(sock)
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
                g_status = "✓" if g_to_h_verified else "✗"
                h_status = "✓" if h_to_g_verified else "✗"
                print(f"  Iter {i:2d}: G->H={g_to_h_latency:8,}μs {g_status}  |  "
                      f"H->G={h_to_g_latency:8,}μs {h_status}")
                
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
                    g_to_h_avg = sum(r['g_to_h_latency'] for r in subset) / len(subset)
                    h_to_g_avg = sum(r['h_to_g_latency'] for r in subset) / len(subset)
                    print(f"  Summary: G->H avg={g_to_h_avg:,.1f}μs  |  H->G avg={h_to_g_avg:,.1f}μs")
        
    finally:
        csv_file.close()
    
    print("\n" + "="*80)
    print(f"Benchmark complete. Results saved to {output_csv}")
    return results

if __name__ == "__main__":
    run_benchmark(
        # 16B to 16MB in power of 2 steps
        payload_sizes=[2**x for x in range(4, 25)],
        iterations=10,
        output_csv='benchmark_results.csv'
    )
