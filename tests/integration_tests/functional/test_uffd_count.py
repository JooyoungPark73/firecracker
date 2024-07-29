# Copyright 2022 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Test UFFD related functionality when resuming from snapshot."""

import os
import re

import pytest
import requests

from framework.utils import Timeout, UffdHandler, run_cmd

SOCKET_PATH = "/firecracker-uffd.sock"


@pytest.fixture(scope="function", name="snapshot")
def snapshot_fxt(microvm_factory, guest_kernel_linux_5_10, rootfs_ubuntu_22):
    """Create a snapshot of a microVM."""

    basevm = microvm_factory.build(guest_kernel_linux_5_10, rootfs_ubuntu_22)
    basevm.spawn()
    basevm.basic_config(vcpu_count=2, mem_size_mib=256)
    basevm.add_net_iface()

    # Add a memory balloon.
    basevm.api.balloon.put(
        amount_mib=0, deflate_on_oom=True, stats_polling_interval_s=0
    )

    basevm.start()
    basevm.wait_for_up()

    # Create base snapshot.
    snapshot = basevm.snapshot_full()
    basevm.kill()

    yield snapshot


def spawn_pf_handler(vm, handler_path, mem_path):
    """Spawn page fault handler process."""
    # Copy snapshot memory file into chroot of microVM.
    jailed_mem = vm.create_jailed_resource(mem_path)
    # Copy the valid page fault binary into chroot of microVM.
    jailed_handler = vm.create_jailed_resource(handler_path)
    handler_name = os.path.basename(jailed_handler)

    uffd_handler = UffdHandler(
        handler_name, SOCKET_PATH, jailed_mem, vm.chroot(), "uffd.log"
    )
    uffd_handler.spawn(vm.jailer.uid, vm.jailer.gid)

    return uffd_handler

def test_valid_handler(uvm_plain, snapshot, uffd_handler_paths):
    """
    Test valid uffd handler scenario.
    """
    vm = uvm_plain
    vm.memory_monitor = None
    vm.spawn()

    # Spawn page fault handler process.
    _pf_handler = spawn_pf_handler(
        vm, uffd_handler_paths["valid_handler"], snapshot.mem
    )

    vm.restore_from_snapshot(snapshot, resume=True, uffd_path=SOCKET_PATH)

    # Inflate balloon.
    vm.api.balloon.patch(amount_mib=200)

    # Deflate balloon.
    vm.api.balloon.patch(amount_mib=0)

    # Verify if the restored guest works.
    vm.wait_for_up()

def test_valid_count_handler(uvm_plain, snapshot, uffd_handler_paths):
    """
    Test valid uffd handler scenario.
    """
    vm = uvm_plain
    vm.memory_monitor = None
    vm.spawn()

    # Spawn page fault handler process.
    _pf_handler = spawn_pf_handler(
        vm, uffd_handler_paths["valid_count_handler"], snapshot.mem
    )

    vm.restore_from_snapshot(snapshot, resume=True, uffd_path=SOCKET_PATH)

    # Inflate balloon.
    vm.api.balloon.patch(amount_mib=200)

    # Deflate balloon.
    vm.api.balloon.patch(amount_mib=0)

    # Verify if the restored guest works.
    vm.wait_for_up()