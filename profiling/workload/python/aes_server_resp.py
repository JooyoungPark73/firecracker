from concurrent import futures
import grpc
import helloworld_pb2
import helloworld_pb2_grpc
import pyaes
import time

class Greeter(helloworld_pb2_grpc.GreeterServicer):
    def AESModeCTR(self, plaintext):
        KEY = "6368616e676520746869732070617373"
        KEY = KEY.encode(encoding='UTF-8')
        counter = pyaes.Counter(initial_value=0)
        aes = pyaes.AESModeOfOperationCTR(KEY, counter=counter)
        ciphertext = aes.encrypt(plaintext)
        return ciphertext

    def SayHello(self, request, context):
        # Respond to the client
        response = helloworld_pb2.HelloReply(message="Hello, %s!" % request.name)
        
        # Schedule server shutdown
        grpc.server.stop(self.server)
        
        # Return response to client
        return response

def serve():
    port = "50051"
    server = grpc.server(futures.ThreadPoolExecutor(max_workers=1))
    helloworld_pb2_grpc.add_GreeterServicer_to_server(Greeter(), server)
    server.add_insecure_port("[::]:" + port)
    server.start()
    print("Server started, listening on " + port)

    # Store the server instance to access in Greeter
    Greeter.server = server

    # Wait for termination
    server.wait_for_termination()

    # Enter computation loop after server stops
    time.sleep(180)
    while True:
        plaintext = "example_string"
        ciphertext = Greeter().AESModeCTR(plaintext)
        print(f"AES | plaintext: {plaintext} | ciphertext = {ciphertext}")
        time.sleep(1)

if __name__ == "__main__":
    serve()
