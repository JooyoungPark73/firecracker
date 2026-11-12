# v1.18 tracer validation

The port is based on exact upstream v1.18.0 commit
`eaf83bb718a559d324613d566eaf776b31a62340`, Rust 1.97.0, target
`x86_64-unknown-linux-musl`, and the upstream v93 build container
`public.ecr.aws/firecracker/fcuvm:v93`. The preserved v1.13 branch and the
historical receipts below remain independent evidence. Current native unit,
HTTP fixture and firecracker-containerd gate receipts live in the downstream
ARIES repository under `runs/tracer-v118-port/`; consult their exact source and
binary hashes before reusing a result. The port alone does not prove workload
coverage or attribution-decoder correctness.

# Historical v1.13 tracer validation record

This is a source-only summary; compiled binaries, kernel images, initrds,
credentials and bulk run logs are not part of the repository. All builds and
tests ran on isolated KVM workers, not the controlling node.

## Source and environment

- Upstream base: Firecracker v1.13.1,
  `f8d253a0750f2cbad678c74a1153caa7596475c0`.
- Target: x86_64 Linux musl, Rust 1.87.0, stock Firecracker seccomp enabled.
- Pre-lifecycle native candidate binary SHA256:
  `b23a56fa3448aa3c1a3f46c49c9657a556cd561ac95d2104eb1520e7da9b79ed`.
- Pre-lifecycle runtime source manifest SHA256:
  `c79a25df477d70e8ef5dd32afadaee177777183745bd9ad3c495b464dc386f08`.
- Host kernel: Ubuntu `6.8.0-147-generic`.
- External guest kernel: Linux 5.10.239, SHA256
  `0e524afe5a90f4ce4b13091a8b5acf7fbe9e7584e711f609d27b69b78564117d`.
  This was a deployment-supplied image, not a reconstructed kernel build.
  Reproductions must record their own source/config/build provenance.

The release build used:

```sh
CFLAGS_x86_64_unknown_linux_musl="-idirafter /usr/include -idirafter /usr/include/x86_64-linux-gnu" \
  cargo build --release --target x86_64-unknown-linux-musl -p firecracker
```

## Checks and evidence scope

| Check | Result and scope |
|---|---|
| Full VMM suite before final lifecycle-uncertainty latch | 588 passed; includes sticky completion-error poison, real sync/async block data, empty observer drain, compact idle reference vectors, scan/capture/state/capacity tests |
| Split metric focused suite | 20 passed; includes dense matches, cross-page/request scan, GPA hole, generation, old history ACK/release, discard cleanup and 8GiB capacity arithmetic |
| HTTP parsing/conversion | 35 passed, including all four new route families and malformed JSON |
| Pre-lifecycle native legacy API | PASS; CPU read/write/restore, async block, deferred net/vsock, exact historical retry after Resume and fresh running collect rejection |
| Pre-lifecycle native coordinator | PASS; rejected Resume retains event-loop hold, queued network marker appears after permitted Resume, raw access/dirty markers, pending-to-valid capture, immutable export after Resume |
| Pre-lifecycle native independent resources | PASS with KVM dirty disabled; three idle epochs, 12/12 positive marker observations and 3/3 untouched controls idle, standalone census/capture/scan/read mask and export |
| Companion unit tests | 29 passed; transport identity, bounded reads/frontiers, capture publication, configuration and coordinator contracts |
| Archived RStan offline replay | 1,960,594 rich rows reproduced exactly; the original comparison failure is retained for two predeclared summary-text differences. This is archived offline parity, not live validation |
| Real companion bootstrap/attribution | PASS on preceding `e07d8ad5...` binary; strict all-read-mask validation, controlled filename/process/anonymous ownership and exact offline replay; subsequent runtime changes are legacy dirty cached-retry compatibility and fail-closed handling of native lifecycle errors |

The three native rows above use the exact pre-lifecycle candidate binary hash.
Their receipts are named `final-native-001`, `final-integrated-001` and
`final-standalone-001` in the archived study evidence. Each records
API requests/responses, guest serial output, kernel/binary hashes and owned
VM/TAP cleanup. Source fixtures and reproduction commands are in
[tests/aries_dirty_log](../tests/aries_dirty_log/README.md).

Reproduce Rust checks with the same target/header environment:

```sh
cargo test --release --target x86_64-unknown-linux-musl -p vmm --lib -- --test-threads=1
cargo test --release --target x86_64-unknown-linux-musl -p firecracker api_server::parsed_request::tests -- --test-threads=1
```

## Failures retained and corrected

The first continuous dirty-device fixture found cached virtqueue used-ring
writes missing bitmap marks after resets. Write-site marking fixes that gap.
Review then found that io_uring CQ-pop errors can consume a completion before
payload marking; a sticky error latch now poisons later collection, including
errors first observed in ordinary event-loop processing.

The first broad companion bootstrap correctly returned invalid: its scan mask
intersected an observer-generated block used-ring write. An empty completion
drain had republished the unchanged used index. The observer-only empty-drain
path now skips that write; real completions, error latching and the ordinary
event-loop path retain their behavior. The failed receipt is preserved, and
the strict overlap test was not weakened.

Capacity review found dense-match and empty-scan-log overhead needed explicit
reservation. The bounded estimate now includes both, and zero scan allowance
rejects scan operations. It is a conservative capacity estimate, not measured
RSS or an OS heap limit. Full source/binary versions distinguish historical
checks from the final candidate.

## Limits

These fixtures do not prove universal read-access detection, arbitrary guest
kernel/type compatibility, eviction safety or actual memory savings. Host idle
eligibility, PFN changes/hidden PFNs, clear-young/TLB behavior and source timing
remain explicit. VMM dirty maps include conservative potential writes.
Fresh boot, fixed slots, 4KiB host pages and the documented standard MMIO
block/net/vsock devices are the supported validation envelope. Snapshot restore,
hugepage-backed RAM, hotplug, aarch64, external vhost/PCI and other device writers
must not be claimed supported. The historical 8GiB test covers two-slot dirty
layout/CPU/block; the integrated 8GiB capacity test is arithmetic, not a full
8GiB metadata attribution stress run.


## Final lifecycle correction

Independent review found that native Resume can send events to vCPUs before a
later ACK failure, leaving reported VM state stale. Actual Pause/Resume
execution errors now permanently invalidate measurement and successful-hold
authority, with an explicit lifecycle-uncertainty reason. Policy-rejected
Resume remains distinct and preserves the hold. Frozen export, explicit
capture discard and native cleanup remain available; a later successful Pause
cannot clear the measurement latch.

Two targeted tests use real vCPU messages and mismatched pending responses,
rather than returning a synthetic error before transition. They check stale
Paused state after Resume failure, fresh/legacy collection rejection,
immutable export, explicit capture abort, policy rejection, and cleanup.
A first Pause test used Resume as the stale response, but the tiny mock guest
could exit before cleanup; that failed fixture is retained. The deterministic
Pause case uses a pending SaveState response while the vCPU remains paused.

Final lifecycle-targeted result: **2 passed**, 588 filtered, on the final source.
The complete 588-test regression passed immediately before the lifecycle-only
change; it is not represented as a 590-test rerun.

```sh
cargo test --release --target x86_64-unknown-linux-musl -p vmm --lib test_lifecycle_ -- --test-threads=1
```

Final lifecycle-corrected binary SHA256:
`b056022604b4f43f2af03583fd8528041e5d438241dde0e9498bd730638898cb`.
Its runtime source manifest SHA256:
`2ee04730ffda0dd2b4a1eb94a735d40cbce3a484e3a66c09d3ef697737ce6162`.
The source build and focused receipt are archived separately from the preceding
successful native fixtures. The final downstream harness gate is external to
this repository's unit/native fixture evidence.

## Downstream native harness gate

The no-cost ARIES native harness gate passed using final binary SHA256
`b056022604b4f43f2af03583fd8528041e5d438241dde0e9498bd730638898cb`.
The harness completed successfully, confirmed sandbox isolation, and completed
cleanup. The fixture's task score was **0**, as expected: it exercises tracer
and harness integration and does not solve the fix-git task. This establishes
the bounded integration gate, not benchmark task success or a paid workload
result. Capture/replay and chronology audit receipts are retained separately
with the downstream run evidence; bulk logs are not included in this repository.
