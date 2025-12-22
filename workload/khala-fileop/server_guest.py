import os
import time
import socket
import struct
import hashlib

KHALA_DEVICE = "/dev/khala0"

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

def handle_guest_to_host(conn, fd, payload_size):
    """Handle G->H transfer: generate data, measure E2E time, send timing."""
    # Generate data and MD5 outside timing region
    test_data = os.urandom(payload_size)
    expected_md5 = hashlib.md5(test_data).hexdigest()
    
    # Send MD5 first
    conn.sendall(expected_md5.encode('ascii'))
    
    # E2E timing: write to file + wait for host acknowledgement (not verification)
    start_time = time.time_ns()
    write_file_message(conn, fd, test_data)
    ack = conn.recv(1)  # Wait for host read acknowledgement
    end_time = time.time_ns()
    
    elapsed_us = (end_time - start_time) // 1000
    
    # Send timing back to host
    conn.sendall(struct.pack('!Q', elapsed_us))
    
    # Now receive verification result (outside timing)
    verification = conn.recv(1)
    
    return verification == b'1'

def handle_host_to_guest(conn, fd, payload_size):
    """Handle H->G transfer: receive data, measure E2E time, send timing."""
    # Receive MD5 first
    expected_md5_bytes = conn.recv(32)
    if len(expected_md5_bytes) != 32:
        return False
    expected_md5 = expected_md5_bytes.decode('ascii')
    
    # E2E timing: read from file + send acknowledgement
    start_time = time.time_ns()
    msg = read_file_message(conn, fd)
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
    """Passive server that waits for client commands."""
    AF_VSOCK = 40
    VMADDR_CID_ANY = 0xFFFFFFFF
    PORT = 9000
    
    if not os.path.exists(KHALA_DEVICE):
        exit(1)
    
    listen_sock = socket.socket(AF_VSOCK, socket.SOCK_STREAM)
    listen_sock.bind((VMADDR_CID_ANY, PORT))
    listen_sock.listen(1)
    
    while True:
        conn, _ = listen_sock.accept()
        
        try:
            # Receive test configuration: payload_size (8 bytes), iterations (4 bytes)
            config_data = conn.recv(12)
            if len(config_data) != 12:
                break
            
            payload_size = struct.unpack('!Q', config_data[:8])[0]
            iterations = struct.unpack('!I', config_data[8:12])[0]
            
            # Special value to close connection
            if payload_size == 0:
                conn.close()
                break
            
            # Open character device once per connection (per payload size)
            fd = os.open(KHALA_DEVICE, os.O_RDWR)
            
            # Run iterations with same fd (measure cold + warm latency)
            for _ in range(iterations):
                # H->G transfer
                handle_host_to_guest(conn, fd, payload_size)
                
                # G->H transfer
                handle_guest_to_host(conn, fd, payload_size)
            
            # Cleanup after all iterations for this payload
            os.close(fd)
            
        except Exception as e:
            pass
        finally:
            conn.close()

if __name__ == "__main__":
    run_server()

