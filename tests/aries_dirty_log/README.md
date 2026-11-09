# Native dirty-epoch fixture

This root-only, no-LLM fixture launches a disposable Firecracker VM, isolated API/vsock sockets, an 8MiB test disk and a TAP device. Run it only on an idle KVM worker. It never changes a containerd runtime config or replaces a deployed binary. A pre-existing output directory or TAP name causes failure rather than reuse.

Dependencies: Python 3, iproute2, cpio, a static BusyBox, musl-gcc and Linux UAPI headers. On Ubuntu, packages include `busybox-static musl-tools cpio iproute2`. Build the helper/initrd outside this repository:

```sh
CFLAGS="-idirafter /usr/include -idirafter /usr/include/x86_64-linux-gnu" \
  bash tests/aries_dirty_log/build_initrd.sh /tmp/aries-dirty-initrd
```

Supply your own x86_64 Linux kernel with virtio MMIO block/net/vsock support and initrd boot enabled. Preserve the kernel source commit/URL, config and build reference; `--kernel-source` records that declared provenance, and the fixture independently records image SHA256. No kernel download, credential handling or binary vendoring is performed.

```sh
sudo python3 tests/aries_dirty_log/native_fixture.py \
  --binary "$PWD/build/cargo_target/x86_64-unknown-linux-musl/release/firecracker" \
  --kernel /absolute/path/to/vmlinux \
  --kernel-source "https://example.org/your-pinned-kernel-build-reference" \
  --initrd /tmp/aries-dirty-initrd/fixture-initrd.cpio \
  --out /tmp/aries-dirty-native-001
```

The default TAP name is `ariesdirty`; use `--tap` to choose another absent name. The fixture uses the isolated test subnet 10.231.218.0/24; confirm it does not conflict with the worker's routes. The default 256MiB run checks CPU read-only/write/write-restore pages, a real async O_DIRECT block-read destination, paused network/vsock non-delivery, and the identical pending payloads appearing in VMM-dirty RAM after Resume. Repeated HTTP requests test idempotency and no-change CPU resets.

`--mem-mib 8192` instead checks two-slot RAM layout plus CPU/block markers and reset. It does not run the payload scanner, whose host-VMA matching is deliberately restricted to the simple 256MiB fixture layout.

Keep `result.json`, `api-calls.json`, `serial.log` and metrics, including failed runs. Guest marker PFNs are fixture-only instrumentation and not a proposed workload measurement mechanism. Cleanup terminates only the owned VM and removes only a TAP successfully created by this invocation. If the fixture itself is forcibly killed, inspect its owned PID/TAP before any manual cleanup. Do not delete other workloads or automatically retry failures.
