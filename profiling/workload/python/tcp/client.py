import socket
import argparse

def start_client(host, port, name):
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.connect((host, port))
        s.sendall(name.encode('utf-8'))
        data = s.recv(1024)
        print(f'Received {data.decode("utf-8")}')

if __name__ == "__main__":
    parser = argparse.ArgumentParser(description='Start a TCP client.')
    parser.add_argument('--host', type=str, required=True, help='Server host address')
    parser.add_argument('--port', type=int, required=True, help='Server port number')
    parser.add_argument('--name', type=str, required=True, help='Your name')
    args = parser.parse_args()
    
    start_client(args.host, args.port, args.name)
