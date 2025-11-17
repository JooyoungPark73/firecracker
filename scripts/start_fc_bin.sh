API_SOCKET="/tmp/firecracker.socket"

# Remove API unix socket
sudo rm -f $API_SOCKET

cd ~/firecracker 
cp build/cargo_target/x86_64-unknown-linux-musl/release/firecracker .

# Run firecracker
sudo ./firecracker --api-sock "${API_SOCKET}"