import socket
import subprocess
import time
import mmap
import os
import struct

AF_VSOCK = 40
VMADDR_CID_ANY = 0xFFFFFFFF
PORT = 9000

OFFSET_MEM = 0x0

BUF_SIZE = 1024 * 2

def read_vsock_message(conn):
    # Read the 4-byte length prefix
    raw_len = b''
    while len(raw_len) < 4:
        chunk = conn.recv(4 - len(raw_len))
        if not chunk:
            return None
        raw_len += chunk
    msg_len = struct.unpack('!I', raw_len)[0]

    # Read the message itself
    data = b''
    while len(data) < msg_len:
        to_read = min(BUF_SIZE, msg_len - len(data))
        chunk = conn.recv(to_read)
        if not chunk:
            return None
        data += chunk
    return data

def write_vsock_message(conn, data):
    # Send the 4-byte length prefix
    msg_len = len(data)
    conn.sendall(struct.pack('!I', msg_len))
    # Send the message in BUF_SIZE chunks
    sent = 0
    while sent < msg_len:
        end = min(sent + BUF_SIZE, msg_len)
        conn.sendall(data[sent:end])
        sent = end

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
    # mm.seek(0)
    # data = mm.read(msg_len)
    data = mm[:msg_len]

    if len(data) < msg_len:
        return None
    return data

# Write a length-prefixed message to a memory-mapped file
# Overwrites the previous message

def write_mmap_message(sock, mm, data):
    # Write data to mmap
    #mm.seek(0)
    #mm.write(data)
    #mm.flush()

    # direct write to file
    mm[:len(data)] = data
    # Send 4-byte length through socket
    sock.sendall(struct.pack('!I', len(data)))

# Example main_mmap function
# Usage: create a file (e.g. mmapfile.bin) of sufficient size (e.g. 1MB), then run main_mmap('mmapfile.bin')

def main_mmap(mmap_path='/tmp/firecracker-pmem', mmap_size=16*1024*1024):
    
    f = os.open(mmap_path, os.O_RDWR)
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        mm = mmap.mmap(f, mmap_size, offset=OFFSET_MEM, prot=mmap.PROT_READ | mmap.PROT_WRITE, flags=mmap.MAP_SHARED)
        s.bind(('0.0.0.0', 9000))
        s.listen(1)
        print("Server listening on port 9000")
        while True:
            conn, addr = s.accept()
            # receive 4 messages from guest and send 4 responses
            for _ in range(4):
                msg = read_mmap_message(conn, mm)
                if msg:
                    print(f"Received message: {msg.decode()}")
                    msg_resp = b"Response From Host: " + msg
                    start_time = time.time_ns() // 1000  # Convert nanoseconds to microseconds
                    write_mmap_message(conn, mm, msg_resp)
                    end_time = time.time_ns() // 1000  # Convert nanoseconds to microseconds
                    print(f"Time taken host->guest: {end_time - start_time} microseconds")
            conn.close()
        mm.close()
    except KeyboardInterrupt:
        print("\nShutting down server...")
    finally:
        s.close()
        os.close(f)


def read_large_mmap_message(sock, mm):
    full_data = b''
    while True:
        # Receive 4-byte length from socket
        raw_len = b''
        while len(raw_len) < 4:
            chunk = sock.recv(4 - len(raw_len))
            if not chunk:
                return None
            raw_len += chunk
        msg_len = struct.unpack('!I', raw_len)[0]
        if msg_len == 0:
            break # end of transmission
        # Read the message itself
        data = b''
        data = mm[:msg_len]

        if len(data) < msg_len:
            return None
        full_data += data
    return full_data

def read_message(conn):
    raw_len = conn.recv(4)
    if not raw_len:
        return None
    msg_len = struct.unpack('!I', raw_len)[0]
    data = b''
    while len(data) < msg_len:
        packet = conn.recv(msg_len - len(data))
        if not packet:
            return None
        data += packet
    return data

def write_message(conn, data):
    length = len(data)
    conn.sendall(struct.pack('!I', length))
    conn.sendall(data) 

def main_benchmark(mmap_path='/tmp/firecracker-pmem', mmap_size=16*1024*1024, num_iterations=10):
    import hashlib
    import statistics
    
    f = os.open(mmap_path, os.O_RDWR | os.O_SYNC)
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        mm = mmap.mmap(f, mmap_size, offset=OFFSET_MEM, prot=mmap.PROT_READ | mmap.PROT_WRITE, flags=mmap.MAP_SHARED)
        s.bind(('0.0.0.0', 9000))
        s.listen(1)
        print("Benchmark Server listening on port 9000")
        print(f"Running {num_iterations} iterations for each size")
        print("="*60)
        
        conn, addr = s.accept()
        print(f"Connected to {addr}")
        
        # Test sizes: 10B, 10KB, 10MB
        test_sizes = [10, 10*1024, 10*1024*1024]
        
        for size in test_sizes:
            print(f"\nTesting {size} bytes:")
            h_to_g_times = []
            g_to_h_times = []
            
            for i in range(num_iterations):
                # Receive expected MD5 from guest over socket
                expected_md5_bytes = conn.recv(32)
                if len(expected_md5_bytes) != 32:
                    print(f"Error: Failed to receive MD5 at iteration {i}")
                    break
                expected_md5 = expected_md5_bytes.decode('ascii')
                
                # Guest -> Host (receive data via mmap)
                start_time = time.time_ns() // 1000
                msg = read_mmap_message(conn, mm)
                end_time = time.time_ns() // 1000
                g_to_h_time = end_time - start_time
                
                if msg is None:
                    print(f"Error: Failed to receive message at iteration {i}")
                    break
                
                # Verify data integrity by computing MD5 of received data
                received_md5 = hashlib.md5(msg).hexdigest()
                verified = (received_md5 == expected_md5)
                
                if not verified:
                    print(f"Warning: G->H verification failed at iteration {i}")
                    print(f"  Expected: {expected_md5}")
                    print(f"  Received: {received_md5}")
                
                # Send verification result back to guest
                conn.sendall(b'1' if verified else b'0')
                
                # Generate new random data for Host -> Guest
                host_data = os.urandom(size)
                host_md5 = hashlib.md5(host_data).hexdigest()
                
                # Send MD5 to guest over socket
                conn.sendall(host_md5.encode('ascii'))
                
                # Host -> Guest (send data via mmap)
                start_time = time.time_ns() // 1000
                write_mmap_message(conn, mm, host_data)
                end_time = time.time_ns() // 1000
                h_to_g_time = end_time - start_time
                
                # Wait for verification from guest
                guest_verification = conn.recv(1)
                if guest_verification != b'1':
                    print(f"Warning: H->G verification failed at iteration {i}")
                
                # Skip first iteration (page faulting)
                if i > 0:
                    h_to_g_times.append(h_to_g_time)
                    g_to_h_times.append(g_to_h_time)
                    print(f"  Iter {i}: G->H={g_to_h_time}μs (MD5={'OK' if verified else 'FAIL'}), H->G={h_to_g_time}μs")
                else:
                    print(f"  Iter {i} (warmup, skipped): G->H={g_to_h_time}μs, H->G={h_to_g_time}μs")
            
            # Calculate statistics (excluding first iteration)
            if h_to_g_times and g_to_h_times:
                print(f"\n  Statistics for {size} bytes (excluding first iteration):")
                print(f"    Guest->Host: avg={statistics.mean(g_to_h_times):.2f}μs, stdev={statistics.stdev(g_to_h_times) if len(g_to_h_times) > 1 else 0:.2f}μs")
                print(f"    Host->Guest: avg={statistics.mean(h_to_g_times):.2f}μs, stdev={statistics.stdev(h_to_g_times) if len(h_to_g_times) > 1 else 0:.2f}μs")
        
        conn.close()
        mm.close()
    except KeyboardInterrupt:
        print("\nShutting down server...")
    finally:
        s.close()
        os.close(f)

if __name__ == "__main__":
    # main()
    # main_vsock_unix()
    # main_mmap()
    main_benchmark()
    
    
    