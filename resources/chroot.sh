#!/bin/bash
# Copyright 2023 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0

# fail if we encounter an error, uninitialized variable or a pipe breaks
set -eu -o pipefail

# be verbose
set -x
PS4='+\t '

cp -ruv $rootfs/* /

packages="udev systemd-sysv openssh-server iproute2 curl socat python3-minimal iperf3 iputils-ping fio kmod tmux hwloc-nox vim-tiny trace-cmd linuxptp strace python3-boto3 pciutils"

# Add nexus workload dependencies
packages="$packages python3-pip htop git wget net-tools rsync numactl chrony"
# Add OpenCV library dependencies
packages="$packages libegl1 libgl1"

# msr-tools is only supported on x86-64.
arch=$(uname -m)
if [ "${arch}" == "x86_64" ]; then
    packages="$packages msr-tools cpuid"
fi

export DEBIAN_FRONTEND=noninteractive
apt update
apt install -y --no-install-recommends $packages
apt autoremove

# Set a hostname.
echo "ubuntu-fc-uvm" > /etc/hostname

# set chrony
# echo 'refclock PHC /dev/ptp0 poll 0 dpoll 0 offset 0 prefer' >> /etc/chrony/chrony.conf
cat <<'EOF' > "/etc/chrony/chrony.conf"
refclock PHC /dev/ptp0 poll 3 trust offset 0 prefer
makestep 0.1 -1
cmdport 0
leapsectz right/UTC
driftfile /var/lib/chrony/chrony.drift
logdir /var/log/chrony
EOF

cat <<'EOF' > "/etc/udev/rules.d/99-vmgenid-resync-clock.rules"
ACTION=="change", SUBSYSTEM=="platform", DRIVER=="vmgenid", ENV{NEW_VMGENID}=="1", RUN+="/usr/bin/logger VMGenID clock sync trigger"
ACTION=="change", SUBSYSTEM=="platform", DRIVER=="vmgenid", ENV{NEW_VMGENID}=="1", RUN+="/bin/systemctl start ptp-sync.service"
EOF

passwd -d root

# Install pylon workload dependencies
pip_packages="grpcio==1.71.0 grpcio-tools==1.71.0 boto3==1.38.27"
pip_packages="$pip_packages pyaes==1.6.1 pillow==11.2.1 scikit-learn==1.5.2 opencv-python-headless==4.11.0.86 pandas==2.2.3 imgaug==0.4.0 psutil==7.0.0 minio==7.2.15 chameleon==4.6.0"
pip3 install $pip_packages --break-system-packages

pip3 install torch==2.7.0 torchvision==0.22.0 --index-url https://download.pytorch.org/whl/cpu --break-system-packages

pip3 cache purge

# The serial getty service hooks up the login prompt to the kernel console
# at ttyS0 (where Firecracker connects its serial console). We'll set it up
# for autologin to avoid the login prompt.
for console in ttyS0; do
    mkdir "/etc/systemd/system/serial-getty@$console.service.d/"
    cat <<'EOF' > "/etc/systemd/system/serial-getty@$console.service.d/override.conf"
[Service]
# systemd requires this empty ExecStart line to override
ExecStart=
ExecStart=-/sbin/agetty --autologin root -o '-p -- \\u' --keep-baud 115200,38400,9600 %I dumb
EOF
done

# Setup fcnet service. This is a custom Firecracker setup for assigning IPs
# to the network interfaces in the guests spawned by the CI.
ln -s /etc/systemd/system/fcnet.service /etc/systemd/system/sysinit.target.wants/fcnet.service

# Disable resolved and ntpd
#
rm -f /etc/systemd/system/multi-user.target.wants/systemd-resolved.service
rm -f /etc/systemd/system/dbus-org.freedesktop.resolve1.service
rm -f /etc/systemd/system/sysinit.target.wants/systemd-timesyncd.service

# make /tmp a tmpfs
ln -s /usr/share/systemd/tmp.mount /etc/systemd/system/tmp.mount
systemctl enable tmp.mount

# don't need this
systemctl disable e2scrub_reap.service
rm -vf /etc/systemd/system/timers.target.wants/*
# systemctl list-units --failed
# /lib/systemd/system/systemd-random-seed.service

systemctl enable var-lib-systemd.mount

# disable Predictable Network Interface Names to keep ethN names
# even with PCI enabled
ln -s /dev/null /etc/systemd/network/99-default.link


#### trim image https://wiki.ubuntu.com/ReducingDiskFootprint
# this does not save much, but oh well
rm -rf /usr/share/{doc,man,info,locale}

cat >> /etc/sysctl.conf <<EOF
# This avoids a SPECTRE vuln
kernel.unprivileged_bpf_disabled=1
EOF

# Build a manifest
dpkg-query --show >/root/manifest
