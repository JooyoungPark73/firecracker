import mmap
import os
import time
import socket
import struct
import hashlib

NEXUS_DEVICE = "/dev/nexus0"
MMAP_SIZE = 16 * 1024 * 1024  # Fixed 16MB

def read_mmap_message(sock, mm):
    """Read message from mmap region after receiving length via socket."""
    raw_len = sock.recv(4)
    if not raw_len or len(raw_len) < 4:
        return None
    msg_len = struct.unpack('!I', raw_len)[0]
    data = mm[:msg_len]
    if len(data) < msg_len:
        return None
    return data

def write_mmap_message(sock, mm, data):
    """Write message to mmap region and send length via socket."""
    mm[:len(data)] = data
    sock.sendall(struct.pack('!I', len(data)))

def handle_guest_to_host(conn, mm, payload_size):
    """Handle G->H transfer: generate data, measure E2E time, send timing."""
    # Generate data and MD5 outside timing region
    test_data = os.urandom(payload_size)
    expected_md5 = hashlib.md5(test_data).hexdigest()
    
    # Send MD5 first
    conn.sendall(expected_md5.encode('ascii'))
    
    # E2E timing: write to mmap + wait for host acknowledgement (not verification)
    start_time = time.time_ns()
    write_mmap_message(conn, mm, test_data)
    ack = conn.recv(1)  # Wait for host read acknowledgement
    end_time = time.time_ns()
    
    elapsed_us = (end_time - start_time) // 1000
    
    # Send timing back to host
    conn.sendall(struct.pack('!Q', elapsed_us))
    
    # Now receive verification result (outside timing)
    verification = conn.recv(1)
    
    return verification == b'1'

def handle_host_to_guest(conn, mm, payload_size):
    """Handle H->G transfer: receive data, measure E2E time, send timing."""
    # Receive MD5 first
    expected_md5_bytes = conn.recv(32)
    if len(expected_md5_bytes) != 32:
        return False
    expected_md5 = expected_md5_bytes.decode('ascii')
    
    # E2E timing: read from mmap + send acknowledgement
    start_time = time.time_ns()
    msg = read_mmap_message(conn, mm)
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
    
    if not os.path.exists(NEXUS_DEVICE):
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
            
            # Open and mmap character device once per connection (per payload size)
            f = os.open(NEXUS_DEVICE, os.O_RDWR)
            mm = mmap.mmap(f, MMAP_SIZE, offset=0, 
                          prot=mmap.PROT_READ | mmap.PROT_WRITE, 
                          flags=mmap.MAP_SHARED)
            
            # Run iterations with same mmap (measure cold + warm latency)
            for _ in range(iterations):
                # H->G transfer
                handle_host_to_guest(conn, mm, payload_size)
                
                # G->H transfer
                handle_guest_to_host(conn, mm, payload_size)
            
            # Cleanup mmap after all iterations for this payload
            mm.close()
            os.close(f)
            
        except Exception as e:
            pass
        finally:
            conn.close()

if __name__ == "__main__":
    run_server()

