from concurrent import futures
import logging

import grpc
import helloworld_pb2
import helloworld_pb2_grpc

import pyaes
import time

class Greeter(helloworld_pb2_grpc.GreeterServicer):
    def AESModeCTR(self, plaintext):
        KEY = "6368616e676520746869732070617373"
        KEY = KEY.encode(encoding = 'UTF-8')
        counter = pyaes.Counter(initial_value = 0)
        aes = pyaes.AESModeOfOperationCTR(KEY, counter = counter)
        ciphertext = aes.encrypt(plaintext)
        return ciphertext

    def SayHello(self, request, context):
        plaintext = "example_string"
        ciphertext = self.AESModeCTR(plaintext)
        # print(f"AES | plaintext: {plaintext} | ciphertext = {ciphertext}")
        return helloworld_pb2.HelloReply(message="Hello, %s!" % ciphertext)


def serve():
    port = "50051"
    server = grpc.server(futures.ThreadPoolExecutor(max_workers=1))
    helloworld_pb2_grpc.add_GreeterServicer_to_server(Greeter(), server)
    server.add_insecure_port("[::]:" + port)
    server.start()
    print("Server started, listening on " + port)
    server.wait_for_termination()


if __name__ == "__main__":
    logging.basicConfig()
    serve()