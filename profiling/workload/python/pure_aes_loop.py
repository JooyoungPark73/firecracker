import pyaes
import time

class AESLoop():
    def AESModeCTR(self, plaintext):
        KEY = "6368616e676520746869732070617373"
        KEY = KEY.encode(encoding = 'UTF-8')
        counter = pyaes.Counter(initial_value = 0)
        aes = pyaes.AESModeOfOperationCTR(KEY, counter = counter)
        ciphertext = aes.encrypt(plaintext)
        return ciphertext

    def SayHello(self):
        plaintext = "example_string"
        while True:
            ciphertext = self.AESModeCTR(plaintext)
            # print(f"AES | plaintext: {plaintext} | ciphertext = {ciphertext}")
            time.sleep(1)


if __name__ == "__main__":
    aes_loop = AESLoop()
    aes_loop.SayHello()
    