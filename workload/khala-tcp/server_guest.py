import os
import time
import socket
import struct
import hashlib

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

def handle_guest_to_host(conn, payload_size):
    """Handle G->H transfer: generate data, measure E2E time, send timing."""
    # Generate data and MD5 outside timing region
    test_data = os.urandom(payload_size)
    expected_md5 = hashlib.md5(test_data).hexdigest()
    
    # Send MD5 first
    conn.sendall(expected_md5.encode('ascii'))
    
    # E2E timing: send data over TCP + wait for host acknowledgement
    start_time = time.time_ns()
    send_data(conn, test_data)
    ack = conn.recv(1)  # Wait for host read acknowledgement
    end_time = time.time_ns()
    
    elapsed_us = (end_time - start_time) // 1000
    
    # Send timing back to host
    conn.sendall(struct.pack('!Q', elapsed_us))
    
    # Now receive verification result (outside timing)
    verification = conn.recv(1)
    
    return verification == b'1'

def handle_host_to_guest(conn, payload_size):
    """Handle H->G transfer: receive data, measure E2E time, send timing."""
    # Receive MD5 first
    expected_md5_bytes = conn.recv(32)
    if len(expected_md5_bytes) != 32:
        return False
    expected_md5 = expected_md5_bytes.decode('ascii')
    
    # E2E timing: receive data over TCP + send acknowledgement
    start_time = time.time_ns()
    msg = recv_data(conn)
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
    listen_sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listen_sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listen_sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    listen_sock.bind((GUEST_IP, PORT))
    listen_sock.listen(1)
    
    while True:
        conn, _ = listen_sock.accept()
        conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        
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
            
            # Run iterations (measure cold + warm latency)
            for _ in range(iterations):
                # H->G transfer
                handle_host_to_guest(conn, payload_size)
                
                # G->H transfer
                handle_guest_to_host(conn, payload_size)
            
        except Exception as e:
            pass
        finally:
            conn.close()

if __name__ == "__main__":
    run_server()
