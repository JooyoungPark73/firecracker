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

CHUNK_SIZE = 40 * 1024 * 1024  # 40MB

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

def main_mmap(mmap_path='/tmp/firecracker-shmem', mmap_size=16*1024*1024):
    
    with open(mmap_path, 'r+b') as f:
        mm = mmap.mmap(f.fileno(), mmap_size, offset=OFFSET_MEM, prot=mmap.PROT_READ | mmap.PROT_WRITE, flags=mmap.MAP_SHARED)
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.bind(('0.0.0.0', 9000))
        s.listen(1)
        print("Server listening on port 9000")
        while True:
            conn, addr = s.accept()
            mm.seek(0)
            # receive 20 messages from guest and send 20 responses
            for _ in range(4):
                msg = read_mmap_message(conn, mm)
                if msg:
                    msg = b"Response From Host"
                    start_time = time.time_ns() // 1000  # Convert nanoseconds to microseconds
                    write_mmap_message(conn, mm, msg)
                    end_time = time.time_ns() // 1000  # Convert nanoseconds to microseconds
                    print(f"Time taken host->guest: {end_time - start_time} microseconds")
            conn.close()
        mm.seek(0)
        mm.write(b'\x00\x00\x00\x00')
        mm.flush()
        mm.close()

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

def write_large_mmap_message(sock, mm, data):
    # Split data into chunks and send via mmap + socket
    total_chunks = (len(data) + CHUNK_SIZE - 1) // CHUNK_SIZE
    
    for i in range(total_chunks):
        start = i * CHUNK_SIZE
        end = min(start + CHUNK_SIZE, len(data))
        chunk = data[start:end]
        
        # Write chunk to mmap (overwrite previous chunk)
        mm[:len(chunk)] = chunk
        
        # Send chunk length
        sock.sendall(struct.pack('!I', len(chunk)))
    # Signal end with zero-length chunk
    sock.sendall(struct.pack('!I', 0)) 

if __name__ == "__main__":
    # main()
    # main_vsock_unix()
    main_mmap()
    
    
    