import mmap
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

memPath = "/dev/khala-shmem"
mmapOffset = 0
mmapSize = 16 * 1024 * 1024
hostIP = "10.0.1.2"

with open(memPath, 'r+b') as f:
    mm = mmap.mmap(f.fileno(), mmapSize, offset=mmapOffset, prot=mmap.PROT_READ | mmap.PROT_WRITE, flags=mmap.MAP_SHARED)
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.connect((hostIP, 9000))
    for i in range(4):
        message = b'Hello, world!'+str(time.time()).encode()
        print(f"Sending message: {message.decode()}")

        # write message
        start_time = time.time_ns() // 1000 
        write_mmap_message(s, mm, message)
        end_time = time.time_ns() // 1000 
        print(f"message: {message.decode()}")
        print(f"Time taken guest->host: {end_time - start_time} microseconds")

        # read message
        start_time = time.time_ns() // 1000 
        msg = read_mmap_message(s, mm)
        if msg:
            print(f"Received message: {msg.decode()}")
        else:
            print("No message received")
        end_time = time.time_ns() // 1000 
        print("")
        time.sleep(1)
    s.close()