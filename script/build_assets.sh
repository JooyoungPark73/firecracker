./tools/devtool build --release

./tools/devtool build_ci_artifacts rootfs
./tools/devtool build_ci_artifacts kernels 6.1

mkdir snapshots
### need to make proper unsquashed rootfs to work
### go to resources/x86_64 dir and run the following commands
# unsquashfs ubuntu-24.04.squashfs
# # create ext4 filesystem image
# sudo chown -R root:root squashfs-root
# truncate -s 4096M ubuntu-24.04.ext4
# sudo mkfs.ext4 -d squashfs-root -F ubuntu-24.04.ext4
