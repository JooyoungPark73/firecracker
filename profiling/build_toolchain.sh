./../tools/devtool build
cp ../build/cargo_target/x86_64-unknown-linux-musl/debug/firecracker .

bash ../resources/rebuild.sh
cp ../resources/x86_64/* ./ubuntu/.

ssh-keygen -f "/users/nehalem/.ssh/known_hosts" -R "172.16.0.2"