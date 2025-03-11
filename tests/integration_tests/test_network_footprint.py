# Copyright 2022 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Test UFFD related functionality when resuming from snapshot."""

import os
import re
import time
from collections import defaultdict
from pathlib import Path
import pprint
from socket import AF_UNIX, SOCK_STREAM, socket

import psutil
import pytest
import requests

from framework.utils import Timeout, check_output
from framework.utils_uffd import SOCKET_PATH, spawn_pf_handler, uffd_handler

from tenacity import retry, stop_after_attempt, wait_fixed

from framework.utils_vsock import (
    VSOCK_UDS_PATH,
    _vsock_connect_to_guest
)

ACCESS_KEY = ""
SECRET_KEY = ""
REGION = "us-east-1"
OUTPUT_FORMAT = "json"

# If guest memory is >3328MB, it is split in a 2nd region
X86_MEMORY_GAP_START = 3328 * 2**20

def get_rss_from_pmap(pid):
    _, output, _ = check_output("pmap -X {}".format(pid))
    return int(output.split("\n")[-2].split()[1], 10)

@retry(wait=wait_fixed(0.5), stop=stop_after_attempt(10), reraise=True)
def get_stable_rss_mem_by_pid(pid, percentage_delta=1):
    """
    Get the RSS memory that a guest uses, given the pid of the guest.

    Wait till the fluctuations in RSS drop below percentage_delta. If timeout
    is reached before the fluctuations drop, raise an exception.
    """

    # All values are reported as KiB

    def get_rss_from_pmap():
        _, output, _ = check_output("pmap -X {}".format(pid))
        return int(output.split("\n")[-2].split()[1], 10)

    first_rss = get_rss_from_pmap()
    time.sleep(1)
    second_rss = get_rss_from_pmap()
    # print(f"RSS readings: {first_rss}, {second_rss}")
    abs_diff = abs(first_rss - second_rss)
    abs_delta = 100 * abs_diff / first_rss
    assert abs_delta < percentage_delta or abs_diff < 2**10
    return second_rss

def _get_pmem_stats(vm, mem_size_mib):
    guest_mem_bytes = mem_size_mib * 2**20
    guest_mem_splits = {
        guest_mem_bytes,
        X86_MEMORY_GAP_START,
    }
    if guest_mem_bytes > X86_MEMORY_GAP_START:
        guest_mem_splits.add(guest_mem_bytes - X86_MEMORY_GAP_START)

    mem_stats = defaultdict(int)
    ps = psutil.Process(vm.firecracker_pid)
    for pmmap in ps.memory_maps(grouped=False):
        # We publish 'size' and 'rss' (resident). size would be the worst case,
        # whereas rss is the current paged-in memory.
        
        # mem_stats["total_size"] += pmmap.size
        mem_stats["total_rss"] += pmmap.rss
        pmmap_path = Path(pmmap.path)
        if pmmap_path.exists() and pmmap_path.name.startswith("firecracker"):
            # mem_stats["binary_size"] += pmmap.size
            mem_stats["binary_rss"] += pmmap.rss

        if pmmap.size not in guest_mem_splits:
            # mem_stats["overhead_size"] += pmmap.size
            mem_stats["overhead_rss"] += pmmap.rss
        if pmmap.size in guest_mem_splits:
            # mem_stats["guest_size"] += pmmap.size
            mem_stats["guest_rss"] += pmmap.rss
    
    for key in mem_stats:
        mem_stats[key] = int(mem_stats[key] / 2**10)

    return mem_stats

def _prepare_experiment(vm):
    vm.ssh.check_output("mkdir -p /root/.aws")
    vm.ssh.check_output("touch /root/.aws/credentials")
    vm.ssh.check_output("echo '[default]' >> /root/.aws/credentials")
    vm.ssh.check_output(f"echo 'aws_access_key_id = {ACCESS_KEY}' >> /root/.aws/credentials")
    vm.ssh.check_output(f"echo 'aws_secret_access_key = {SECRET_KEY}' >> /root/.aws/credentials")
    vm.ssh.check_output(f"echo 'aws_region = {REGION}' >> /root/.aws/credentials")
    
    vm.ssh.check_output("touch /var/lib/dpkg/status")
    vm.ssh.check_output("apt update && apt install -y -q --no-install-recommends git python3-pip")
    vm.ssh.check_output("pip3 install --break-system-packages grpcio grpcio-tools boto3")
    vm.ssh.check_output("git clone -b hotos25 --depth 1 https://github.com/JooyoungPark73/fc_benchmark.git")
    
    vm.netns.check_output("apt update && apt install -y -q --no-install-recommends git python3-pip")
    vm.netns.check_output("pip3 install --break-system-packages grpcio grpcio-tools")
    vm.netns.check_output("rm -rf fc_benchmark")
    vm.netns.check_output("git clone -b hotos25 --depth 1 https://github.com/JooyoungPark73/fc_benchmark.git")


def test_do_nothing_footprint(uvm_plain_rw, microvm_factory, guest_kernel_linux_6_1, rootfs_rw):
    vm = microvm_factory.build(guest_kernel_linux_6_1, rootfs_rw)
    mem_size_mib = 1024
    vm.help.resize_disk(vm.rootfs_file, 1 * 2**30)
    vm.spawn()
    vm.basic_config(vcpu_count=2, mem_size_mib=mem_size_mib)
    vm.add_net_iface()
    vm.start()
    vm.help.enable_ip_forwarding()
    
    _prepare_experiment(vm)

    print("\nBooted before echo:", get_rss_from_pmap(vm.firecracker_pid))
    vm.ssh.check_output("echo 'Hello, world!'")

    print("Booted after echo:", get_rss_from_pmap(vm.firecracker_pid))
    print(_get_pmem_stats(vm, mem_size_mib))

    # Create base snapshot.
    snapshot = vm.snapshot_full()
    vm.kill()
    vm = uvm_plain_rw
    vm.memory_monitor = None
    vm.spawn()

    # Spawn page fault handler process.
    _pf_handler = spawn_pf_handler(vm, uffd_handler("on_demand"), snapshot.mem)

    vm.restore_from_snapshot(snapshot, resume=True, uffd_path=SOCKET_PATH)

    print("Restore snapshot:", get_rss_from_pmap(vm.firecracker_pid))
    print("Restore snapshot:", vm.netns.check_output(f"grep VmRSS /proc/{vm.firecracker_pid}/status").stdout)
    print(_get_pmem_stats(vm, mem_size_mib))
    print("Stable RSS:", get_stable_rss_mem_by_pid(vm.firecracker_pid))
    vm.kill()
    time.sleep(5)


def test_http_server_footprint(uvm_plain_rw, microvm_factory, guest_kernel_linux_6_1, rootfs_rw):
    vm = microvm_factory.build(guest_kernel_linux_6_1, rootfs_rw)
    mem_size_mib = 1024
    vm.help.resize_disk(vm.rootfs_file, 1 * 2**30)
    vm.spawn()
    vm.basic_config(vcpu_count=2, mem_size_mib=mem_size_mib)
    vm.add_net_iface()
    vm.start()
    vm.help.enable_ip_forwarding()
    
    _prepare_experiment(vm)

    print("\nBooted before python:", get_rss_from_pmap(vm.firecracker_pid))
    vm.ssh.check_output("nohup python3 -m http.server 0<&- &>/dev/null &")

    print("Booted after python:", get_rss_from_pmap(vm.firecracker_pid))
    print("Stable RSS:", get_stable_rss_mem_by_pid(vm.firecracker_pid))
    vm.netns.check_output("curl 192.168.0.2:8000")
    print("Booted after curl:", get_rss_from_pmap(vm.firecracker_pid))
    print(_get_pmem_stats(vm, mem_size_mib))

    # Create base snapshot.
    snapshot = vm.snapshot_full()
    vm.kill()
    vm = uvm_plain_rw
    vm.memory_monitor = None
    vm.spawn()

    # Spawn page fault handler process.
    _pf_handler = spawn_pf_handler(vm, uffd_handler("on_demand"), snapshot.mem)

    vm.restore_from_snapshot(snapshot, resume=True, uffd_path=SOCKET_PATH)

    print("Restore before curl:", get_rss_from_pmap(vm.firecracker_pid))
    print(_get_pmem_stats(vm, mem_size_mib))
    
    vm.help.enable_ip_forwarding()
    vm.netns.check_output("curl 192.168.0.2:8000")
    print("Restore after curl:", get_rss_from_pmap(vm.firecracker_pid))
    print("Restore after curl:", vm.netns.check_output(f"grep VmRSS /proc/{vm.firecracker_pid}/status").stdout)
    print(_get_pmem_stats(vm, mem_size_mib))
    print("Stable RSS:", get_stable_rss_mem_by_pid(vm.firecracker_pid))
    vm.kill()
    time.sleep(5)


def test_grpc_server_footprint(uvm_plain_rw, microvm_factory, guest_kernel_linux_6_1, rootfs_rw):
    vm = microvm_factory.build(guest_kernel_linux_6_1, rootfs_rw)
    mem_size_mib = 1024
    vm.help.resize_disk(vm.rootfs_file, 1 * 2**30)
    vm.spawn()
    vm.basic_config(vcpu_count=2, mem_size_mib=mem_size_mib)
    vm.add_net_iface()
    vm.start()
    vm.help.enable_ip_forwarding()
    
    _prepare_experiment(vm)
    print("\nBooted before python:", get_rss_from_pmap(vm.firecracker_pid))
    vm.ssh.check_output("nohup python3 fc_benchmark/workload/python/helloworld_py_grpc/server.py 0<&- &>/dev/null &")

    print("Booted after python:", get_rss_from_pmap(vm.firecracker_pid))
    print("Stable RSS:", get_stable_rss_mem_by_pid(vm.firecracker_pid))
    vm.netns.check_output("python3 fc_benchmark/workload/python/helloworld_py_grpc/client.py")
    print("Booted after grpc client:", get_rss_from_pmap(vm.firecracker_pid))
    print(_get_pmem_stats(vm, mem_size_mib))

    # Create base snapshot.
    snapshot = vm.snapshot_full()
    vm.kill()
    vm = uvm_plain_rw
    vm.memory_monitor = None
    vm.spawn()

    # Spawn page fault handler process.
    _pf_handler = spawn_pf_handler(vm, uffd_handler("on_demand"), snapshot.mem)

    vm.restore_from_snapshot(snapshot, resume=True, uffd_path=SOCKET_PATH)

    print("Restore before grpc client:", get_rss_from_pmap(vm.firecracker_pid))
    print(_get_pmem_stats(vm, mem_size_mib))
    
    vm.help.enable_ip_forwarding()
    vm.netns.check_output("python3 fc_benchmark/workload/python/helloworld_py_grpc/client.py")
        
    print("Restore after grpc client:", get_rss_from_pmap(vm.firecracker_pid))
    print("Restore after grpc client:", vm.netns.check_output(f"grep VmRSS /proc/{vm.firecracker_pid}/status").stdout)
    print(_get_pmem_stats(vm, mem_size_mib))
    print("Stable RSS:", get_stable_rss_mem_by_pid(vm.firecracker_pid))
    vm.kill()
    time.sleep(5)

def test_grpc_server_footprint_s3(uvm_plain_rw, microvm_factory, guest_kernel_linux_6_1, rootfs_rw):
    vm = microvm_factory.build(guest_kernel_linux_6_1, rootfs_rw)
    mem_size_mib = 1024
    vm.help.resize_disk(vm.rootfs_file, 1 * 2**30)
    vm.spawn()
    vm.basic_config(vcpu_count=2, mem_size_mib=mem_size_mib)
    vm.add_net_iface()
    vm.start()
    vm.help.enable_ip_forwarding()
    
    _prepare_experiment(vm)
    print("\nBooted before python:", get_rss_from_pmap(vm.firecracker_pid))
    vm.ssh.check_output("nohup python3 fc_benchmark/workload/python/helloworld_py_grpc_s3/server.py 0<&- &>/dev/null &")

    print("Booted after python:", get_rss_from_pmap(vm.firecracker_pid))
    print("Stable RSS:", get_stable_rss_mem_by_pid(vm.firecracker_pid))
    vm.netns.check_output("python3 fc_benchmark/workload/python/helloworld_py_grpc_s3/client.py")
    print("Booted after grpc server:", get_rss_from_pmap(vm.firecracker_pid))
    print(_get_pmem_stats(vm, mem_size_mib))

    # Create base snapshot.
    snapshot = vm.snapshot_full()
    vm.kill()
    vm = uvm_plain_rw
    vm.memory_monitor = None
    vm.spawn()

    # Spawn page fault handler process.
    _pf_handler = spawn_pf_handler(vm, uffd_handler("on_demand"), snapshot.mem)

    vm.restore_from_snapshot(snapshot, resume=True, uffd_path=SOCKET_PATH)
    print("Restore before grpc client:", get_rss_from_pmap(vm.firecracker_pid))
    print(_get_pmem_stats(vm, mem_size_mib))

    vm.help.enable_ip_forwarding()
    vm.netns.check_output("python3 fc_benchmark/workload/python/helloworld_py_grpc_s3/client.py")
    
    print("Restore after grpc client:", get_rss_from_pmap(vm.firecracker_pid))
    print("Restore after grpc client:", vm.netns.check_output(f"grep VmRSS /proc/{vm.firecracker_pid}/status").stdout)
    print(_get_pmem_stats(vm, mem_size_mib))
    print("Stable RSS:", get_stable_rss_mem_by_pid(vm.firecracker_pid))
    vm.kill()
    time.sleep(5)



# def test_grpc_server_footprint_s3_trainlr(uvm_plain_rw, microvm_factory, guest_kernel_linux_6_1, rootfs_rw):
#     vm = microvm_factory.build(guest_kernel_linux_6_1, rootfs_rw)
#     mem_size_mib = 1024
#     vm.help.resize_disk(vm.rootfs_file, 1 * 2**30)
#     vm.spawn()
#     vm.basic_config(vcpu_count=2, mem_size_mib=mem_size_mib)
#     vm.add_net_iface()
#     vm.start()
#     vm.help.enable_ip_forwarding()
    
#     _prepare_experiment(vm)
#     print("\nBooted before python:", get_rss_from_pmap(vm.firecracker_pid))
#     vm.ssh.check_output("nohup python3 fc_benchmark/workload/python/lr_training_py_grpc_s3/server.py 0<&- &>/dev/null &")

#     print("Booted after python:", get_rss_from_pmap(vm.firecracker_pid))
#     print("Stable RSS:", get_stable_rss_mem_by_pid(vm.firecracker_pid))
#     vm.netns.check_output("python3 fc_benchmark/workload/python/lr_training_py_grpc_s3/client.py")
#     print("Booted after grpc server:", get_rss_from_pmap(vm.firecracker_pid))
#     print(_get_pmem_stats(vm, mem_size_mib))

#     # Create base snapshot.
#     snapshot = vm.snapshot_full()
#     vm.kill()
#     vm = uvm_plain_rw
#     vm.memory_monitor = None
#     vm.spawn()

#     # Spawn page fault handler process.
#     _pf_handler = spawn_pf_handler(vm, uffd_handler("on_demand"), snapshot.mem)

#     vm.restore_from_snapshot(snapshot, resume=True, uffd_path=SOCKET_PATH)
#     print("Restore before grpc client:", get_rss_from_pmap(vm.firecracker_pid))
#     print(_get_pmem_stats(vm, mem_size_mib))

#     vm.help.enable_ip_forwarding()
#     vm.netns.check_output("python3 fc_benchmark/workload/python/lr_training_py_grpc_s3/client.py")
    
#     print("Restore after grpc client:", get_rss_from_pmap(vm.firecracker_pid))
#     print("Restore after grpc client:", vm.netns.check_output(f"grep VmRSS /proc/{vm.firecracker_pid}/status").stdout)
#     print(_get_pmem_stats(vm, mem_size_mib))
#     print("Stable RSS:", get_stable_rss_mem_by_pid(vm.firecracker_pid))
#     vm.kill()
#     time.sleep(5)

# def test_vsock_server_footprint(uvm_plain_rw, microvm_factory, guest_kernel_linux_6_1, rootfs_rw):
#     vm = microvm_factory.build(guest_kernel_linux_6_1, rootfs_rw)
#     mem_size_mib = 1024
#     vm.help.resize_disk(vm.rootfs_file, 1 * 2**30)
#     vm.spawn()
#     vm.basic_config(vcpu_count=2, mem_size_mib=mem_size_mib)
#     vm.add_net_iface()
#     vm.api.vsock.put(vsock_id="vsock0", guest_cid=2, uds_path=f"{VSOCK_UDS_PATH}")
#     vm.start()
#     vm.help.enable_ip_forwarding()
    
#     _prepare_experiment(vm)
#     print("\nBooted before python:", get_rss_from_pmap(vm.firecracker_pid))
#     vm.ssh.check_output("nohup python3 fc_benchmark/workload/python/helloworld_py_vsock/server.py 0<&- &>/dev/null &")
#     print("Booted after python:", get_rss_from_pmap(vm.firecracker_pid))
    
#     host_socket_path = os.path.join(vm.jailer.chroot_path(), VSOCK_UDS_PATH)
#     if not os.path.exists(host_socket_path):
#         raise Exception(f"Socket path {host_socket_path} does not exist")
#     sock = socket(AF_UNIX, SOCK_STREAM)
#     sock.connect(host_socket_path)
#     buf = bytearray("CONNECT 50051\n".encode("utf-8"))
#     sock.send(buf)
#     ack_buf = sock.recv(32)
#     assert re.match("^OK [0-9]+\n$", ack_buf.decode("utf-8")) is not None
#     sock.sendall("Hello from host!\n".encode("utf-8"))
#     response = sock.recv(1024).decode()
#     print(response)
#     print("Booted after grpc server:", get_rss_from_pmap(vm.firecracker_pid))

#     # Create base snapshot.
#     snapshot = vm.snapshot_full()
#     vm.kill()
#     vm = uvm_plain_rw
#     vm.memory_monitor = None
#     vm.spawn()

#     # Spawn page fault handler process.
#     _pf_handler = spawn_pf_handler(vm, uffd_handler("on_demand"), snapshot.mem)

#     vm.restore_from_snapshot(snapshot, resume=True, uffd_path=SOCKET_PATH)
#     print("Restore before grpc client:", get_rss_from_pmap(vm.firecracker_pid))
#     print(_get_pmem_stats(vm, mem_size_mib))

#     vm.help.enable_ip_forwarding()
#     sock = socket(AF_UNIX, SOCK_STREAM)
#     sock.connect(host_socket_path)
#     buf = bytearray("CONNECT 50051\n".encode("utf-8"))
#     sock.send(buf)
#     ack_buf = sock.recv(32)
#     assert re.match("^OK [0-9]+\n$", ack_buf.decode("utf-8")) is not None
#     sock.sendall("Hello from host!\n".encode("utf-8"))
#     response = sock.recv(1024).decode()
#     print(response)
    
#     print("Restore after grpc client:", get_rss_from_pmap(vm.firecracker_pid))
#     print(_get_pmem_stats(vm, mem_size_mib))
#     vm.kill()
#     time.sleep(5)