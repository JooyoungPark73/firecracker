# Experimental ARIES dirty-page epochs

This branch adds a paused-VM `PUT /dirty-log` endpoint to Firecracker **v1.13.1**, based on commit `f8d253a0750f2cbad678c74a1153caa7596475c0`. It exports separate KVM/vCPU and VMM/device dirty-page bitmaps without writing a memory snapshot. It is an experimental measurement extension, not an eviction or migration implementation.

Host page-idle access tracking belongs to an external observer and is not included here. The API cannot distinguish reads, infer page ownership, or prove that an unchanged page was not accessed.

## Enable and build

Use the repository-pinned Rust 1.87.0 toolchain and its standard build prerequisites. The validated target is x86_64 Linux, `x86_64-unknown-linux-musl`, using the stock Firecracker seccomp filters. For a direct build on Ubuntu with musl-tools, clang/libclang, cmake, pkg-config and libseccomp development headers:

```sh
rustup target add x86_64-unknown-linux-musl
CFLAGS_x86_64_unknown_linux_musl="-idirafter /usr/include -idirafter /usr/include/x86_64-linux-gnu" \
  cargo build --release --bin firecracker --target x86_64-unknown-linux-musl
```

The repository Cargo configuration places output under `build/cargo_target/`. Normal upstream build guidance still applies. The extra header search paths address the validated Ubuntu/musl environment; they are not required on every build system.

Set `ARIES_DIRTY_LOG=1` in the Firecracker process environment to force dirty tracking at initial guest-memory creation and prevent machine-config updates from disabling it. Alternatively, configure upstream `track_dirty_pages: true` before boot. With the environment unset, ordinary machine-config defaults are unchanged. Verify `GET /machine-config` reports tracking enabled. The endpoint rejects missing tracking; it never substitutes residency/mincore for dirty evidence.

No deployment binary is replaced automatically. Build and run an isolated binary with an isolated API socket first.

## API and lifecycle

All new requests require a booted, HTTP-paused VM. Example bodies for `PUT /dirty-log`:

```json
{"operation":"arm","epoch":1}
{"operation":"collect","epoch":1,"next_epoch":2}
{"operation":"collect","epoch":2}
```

1. HTTP `PATCH /vm {"state":"Paused"}`.
2. Initial `arm` clears the boot dirty baseline and starts epoch 1. Its returned maps describe the discarded baseline, not workload activity.
3. Resume and execute work. Pause again, then `collect` returns and resets the outgoing maps. Supplying `next_epoch = epoch + 1` starts the next epoch immediately. This supports consecutive tool and paused-gap intervals.
4. Omitting `next_epoch` closes the sequence. A new initial arm on that VM is rejected.
5. An exact retry of the **most recent** successful request returns its cached result without another reset, even if VM state subsequently changed. This is replay of historical evidence, not a new sample. Earlier or altered requests are rejected.

The API rejects preboot requests, running-state new requests, invalid epochs, unknown JSON fields and unsupported device configurations. Failures during quiescence or destructive bitmap sampling poison the collector; restart the VM rather than assuming missing pages are clean. Snapshot creation is rejected after dirty measurement starts, including after a closing collect, because snapshot operations also reset dirty state.

Only fresh-boot, fixed guest-RAM slot layouts have been validated. Snapshot-restored VMs, memory hotplug, non-4KiB host pages, hugepage-backed RAM and aarch64 have **not** been validated. They must not be presented as supported by this study. The API checks the host page size and rejects PCI/external/unknown devices; it does not claim comprehensive automatic detection of every unvalidated deployment mode. Keep deployments within the documented contract.

## Response

The response has `schema: "aries.firecracker.dirty.v1"`, request/active epoch identifiers, `boot_baseline_discarded`, `page_bytes: 4096`, host-monotonic begin/end microseconds and quiescence metadata. Each `regions` entry supplies:

| Field | Meaning |
|---|---|
| `slot`, `guest_base`, `bytes`, `page_count` | Fresh-boot RAM region identity and extent |
| `kvm_dirty_b64` | CPU dirty log from `KVM_GET_DIRTY_LOG` |
| `vmm_dirty_b64` | Host-maintained guest-memory bitmap |
| `combined_dirty_b64` | Bitwise union of both sources |
| corresponding `*_dirty_pages` | Population count of each bitmap |

Encoding is base64 of packed **little-endian u64 words, least-significant bit first**. Bit zero denotes `guest_base`; bit `i` denotes `guest_base + 4096*i`. Padding bits beyond `page_count` are zero. A consumer must verify region base/size against its own captured KVM memory layout. There is no residency, host-page-idle eligibility, or page-category filter.

The CPU bitmap is evidence of writes, including write-then-restore. The VMM bitmap is **conservative potential-write coverage**: writable descriptor capacity can be marked before actual IO. Therefore combined dirty is not an exact changed-byte count, and counts can include pages whose bytes ended unchanged.

## Boundary correctness and supported writers

Firecracker v1.13.1's HTTP pause adapter enters a blocking API receive loop until Resume (`src/firecracker/src/api_server_adapter.rs`). Ordinary device EventManager callbacks are frozen in this state. Calling `pause_vm` directly only stops vCPUs and does not establish this complete HTTP contract.

Host io_uring block reads can still write guest RAM after that pause. Every arm/collect explicitly drains outstanding native block operations, propagates drain errors, then handles completions before reading/resetting maps. Completion-pop failures are latched even when they occur in the ordinary event loop before a barrier; all later dirty collection rejects that device. This covers failures after a completion entry was consumed but before its guest-memory dirty marking was confirmed. These explicit operations and net queue preparation are exceptions to the ordinary event-loop freeze.

Supported and validated native virtio MMIO device types are network (1), block (2; sync and async) and vsock (19). Vhost-user, PCI, RNG, balloon and other types are rejected. Net preparation clears cached writable RX descriptor/iovec state so subsequent writes reparse and mark again. Vsock reloads RX buffers per packet. Async block completion handling marks DMA destinations.

This branch also fixes a continuous-reset hazard in shared virtqueues: cached raw used-ring pointers previously bypassed bitmap marking after initialization. Each used index, used element and avail_event write now marks its actual guest-memory range. An initial disk fixture caught dirty payload/status with a falsely clean used ring; the new regression tests cover this case.

Resetting measurement maps does not mean a guest page can be discarded. A hypothetical preservation set must accumulate dirty evidence across all relevant epochs, including gaps, until a real preservation action is independently verified. No actual memory savings or eviction correctness is claimed.

## Validation and reproduction

Run the VMM unit tests on a KVM-enabled worker with appropriate root/TAP permissions and `ARIES_DIRTY_LOG` unset:

```sh
cargo test --release --target x86_64-unknown-linux-musl -p vmm --lib -- --test-threads=1
```

Apply the same build-environment header flags if required. The new CPU oracle uses the repository's standard mock kernel, not a deployment-specific absolute path. The real-mode CPU instruction oracle is x86_64-only. Focused filters are `dirty_log::tests` and `aries_`. Tests cover CPU read/write/restore, tracked VMM writes, idempotent rotation, no-change reset, invalid state, actual sync/async block reads and TAP RX reuse after reset.

A separate source-only native fixture is in [tests/aries_dirty_log](../tests/aries_dirty_log/README.md). It boots an **externally supplied Linux kernel**, records its image SHA256 and declared source/build provenance, and checks the actual HTTP API with disk/TAP/vsock traffic. No kernel, initrd, compiled helper, binary or bulk run log is vendored.

Historical evidence before the completion-error-latch review fix (kept separate from the new build's validation): 564 VMM tests passed; the full 256MiB Linux fixture passed CPU/block and deferred network/vsock delivery checks; an 8GiB fixture passed two-slot layout, CPU/block and reset checks (3GiB at GPA 0, 5GiB at GPA 4GiB). The 8GiB fixture intentionally does not claim the 256MiB host-mapping payload scanner supports that layout. Independent standalone backend/observer smokes also passed on four additional workers. Those host-idle results are external and do not strengthen this API into a universal read-access detector.

The validated external kernel image was Linux 5.10.239, SHA256 `0e524afe5a90f4ce4b13091a8b5acf7fbe9e7584e711f609d27b69b78564117d`. Its original build provenance was deployment-supplied rather than reconstructed here. Reproductions must retain their own source/config/build reference and image hash. Validation is bounded to the tested host/guest/device combinations; it does not establish full Linux/device compatibility.
