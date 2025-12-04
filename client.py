import mmap
import os
import socket
import time
import struct

# Read a length-prefixed message from a memory-mapped file
# The file must be pre-created and large enough for the message
# Returns the message bytes, or None if not available

def read_mmap_message(sock, mm):
    # Receive 4-byte length from socket
    raw_len = b''
    while len(raw_len) < 4:
        chunk = sock.recv(4 - len(raw_len))
        if not chunk:
            return None
        raw_len += chunk
    msg_len = struct.unpack('!I', raw_len)[0]
    # Read data from mmap
    data = mm[:msg_len]

    if len(data) < msg_len:
        return None
    return data

# Write a length-prefixed message to a memory-mapped file
# Overwrites the previous message

def write_mmap_message(sock, mm, data):
    # Write data to mmap
    mm[:len(data)] = data
    # Send 4-byte length through socket
    sock.sendall(struct.pack('!I', len(data)))

def main_benchmark(num_iterations=10):
    import hashlib
    import statistics
    
    mmapSize = 16 * 1024 * 1024
    hostIP = "10.0.1.2"

    # Try khala-shmem device first (raw_memory mode with driver)
    if os.path.exists('/dev/khala-shmem'):
        print("Using /dev/khala-shmem (raw shared memory)")
        memPath = "/dev/khala-shmem"
        mmapOffset = 0
    # Fallback to traditional virtio-pmem
    elif os.path.exists('/dev/pmem0'):
        print("Using /dev/pmem0 (virtio-pmem mode)")
        memPath = "/dev/pmem0"
        mmapOffset = 0
    else:
        print("ERROR: No shared memory device found")
        exit(1)

    f = os.open(memPath, os.O_RDWR)
    mm = None
    try: 
        mm = mmap.mmap(f, mmapSize, offset=mmapOffset, prot=mmap.PROT_READ | mmap.PROT_WRITE, flags=mmap.MAP_SHARED)
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.connect((hostIP, 9000))
        
        print(f"Running {num_iterations} iterations for each size")
        print("="*60)
        
        # Test sizes: 10B, 10KB, 10MB
        test_sizes = [10, 10*1024, 10*1024*1024]
        
        for size in test_sizes:
            print(f"\nTesting {size} bytes:")
            g_to_h_times = []
            h_to_g_times = []
            
            for i in range(num_iterations):
                # Generate test data
                test_data = os.urandom(size)
                guest_md5 = hashlib.md5(test_data).hexdigest()
                
                # Send MD5 to host over socket first
                s.sendall(guest_md5.encode('ascii'))
                
                # Guest -> Host (send data via mmap)
                start_time = time.time_ns() // 1000
                write_mmap_message(s, mm, test_data)
                end_time = time.time_ns() // 1000
                g_to_h_time = end_time - start_time
                
                # Receive verification from host
                host_verification = s.recv(1)
                if host_verification != b'1':
                    print(f"Warning: G->H verification failed at iteration {i}")
                
                # Receive expected MD5 from host over socket
                expected_md5_bytes = s.recv(32)
                if len(expected_md5_bytes) != 32:
                    print(f"Error: Failed to receive MD5 at iteration {i}")
                    break
                expected_md5 = expected_md5_bytes.decode('ascii')
                
                # Host -> Guest (receive data via mmap)
                start_time = time.time_ns() // 1000
                msg = read_mmap_message(s, mm)
                end_time = time.time_ns() // 1000
                h_to_g_time = end_time - start_time
                
                # Verify data integrity
                if msg is None:
                    print(f"Error: Failed to receive message at iteration {i}")
                    s.sendall(b'0')
                    break
                
                received_md5 = hashlib.md5(msg).hexdigest()
                verified = (received_md5 == expected_md5)
                
                if verified:
                    s.sendall(b'1')  # Send verification success
                else:
                    s.sendall(b'0')  # Send verification failure
                    print(f"Warning: H->G verification failed at iteration {i}")
                    print(f"  Expected: {expected_md5}")
                    print(f"  Received: {received_md5}")
                
                # Skip first iteration (page faulting)
                if i > 0:
                    g_to_h_times.append(g_to_h_time)
                    h_to_g_times.append(h_to_g_time)
                    print(f"  Iter {i}: G->H={g_to_h_time}μs, H->G={h_to_g_time}μs (MD5={'OK' if verified else 'FAIL'})")
                else:
                    print(f"  Iter {i} (warmup, skipped): G->H={g_to_h_time}μs, H->G={h_to_g_time}μs")
            
            # Calculate statistics (excluding first iteration)
            if g_to_h_times and h_to_g_times:
                print(f"\n  Statistics for {size} bytes (excluding first iteration):")
                print(f"    Guest->Host: avg={statistics.mean(g_to_h_times):.2f}μs, stdev={statistics.stdev(g_to_h_times) if len(g_to_h_times) > 1 else 0:.2f}μs")
                print(f"    Host->Guest: avg={statistics.mean(h_to_g_times):.2f}μs, stdev={statistics.stdev(h_to_g_times) if len(h_to_g_times) > 1 else 0:.2f}μs")
        
        s.close()
    finally:
        if mm:
            mm.close()
        os.close(f)

if __name__ == "__main__":
    main_benchmark()