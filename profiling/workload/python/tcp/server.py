import socket
import ctypes
import time

def start_server(host='127.0.0.1', port=65432):
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind((host, port))
        s.listen()
        print(f'Server listening on {host}:{port}')

        while True:
            conn, addr = s.accept()
            with conn:
                print(f'Connected by {addr}')
                while True:
                    data = conn.recv(1024)
                    if not data:
                        break
                    name = data.decode('utf-8')
                    response = f'hello, {name}'
                    conn.sendall(response.encode('utf-8'))

if __name__ == "__main__":
    start_server()
