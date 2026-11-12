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

## Split-resource fixtures

Use the same explicit binary/kernel/initrd arguments and a fresh output directory
for each script:

- `integrated_fixture.py` selects dirty, idle, census and metadata through
  /trace-epochs. It checks rejected Resume retains the event-loop hold, queued
  network payload appears only after a permitted Resume, pending metadata
  validates at rotation, frozen bytes export after Resume, and real marker
  dirty/access evidence.
- `standalone_fixture.py` explicitly disables KVM dirty tracking and proves
  page-idle, census, raw metadata and resident scan work independently. Three
  access epochs check four positive markers and one untouched negative page.
  It also checks the union read mask for metadata and scan reads.

These scripts currently target the simple 256MiB marker fixture; the legacy
script retains its separate 8GiB two-slot layout mode. The new marker helper
adds an untouched fifth page, so rebuild the initrd before the repeated-idle
fixture. Stock seccomp remains enabled. Scan tests deliberately use an isolated
zero-filled marker page; real bootstrap needles should be selective.

Rust unit tests additionally exercise cross-page/request scans and GPA holes,
raw reference classification, stale generations, record reservations, dense
match capacity, source poisoning and explicit discard cleanup. Fixture success
does not establish universal no-miss idle detection or arbitrary guest/kernel
compatibility. Keep individual source/binary hashes with every receipt.

The standalone fixture defaults to unmodified guest accesses. Secondary TLB
state can hide repeated CPU reads from page-idle; a failed positive marker
assertion is retained as failed evidence, not repaired into a zero-access claim.
`--flush-marker-tlb` selects a fixture-only mprotect PROT_NONE/read-write cycle
before the four marker accesses. It provides a controlled positive oracle for
repeatable page-idle testing; it does not establish general read coverage. The
untouched fifth page remains outside that cycle.

`--no-capture-census` validates raw reads and scans with bounded per-request
pagemap presence probes, no capture census record, and the complete read mask.
It can be combined with the explicit marker TLB positive control above.
