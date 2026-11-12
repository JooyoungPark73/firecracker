# Experimental Firecracker tracer

This branch adds a paused-VM `PUT /dirty-log` endpoint to Firecracker **v1.18.0**, based on the exact release commit `eaf83bb718a559d324613d566eaf776b31a62340`. It exports separate KVM/vCPU and VMM/device dirty-page bitmaps without writing a memory snapshot. It is an experimental measurement extension, not an eviction or migration implementation.

The VMM also exposes independently selectable host page-idle access tracking, residency census, and bounded raw metadata capture. The optional coordinator pairs selected metrics across epochs. Kernel layout interpretation and page attribution run in the external companion under tools/aries-attribution; guest Linux is unchanged. Dirty evidence cannot distinguish reads, and host idle tracking has documented eligibility and clear-young/TLB limitations.

## Enable and build

Use the repository-pinned Rust 1.97.0 toolchain and its standard build prerequisites. The validated target is x86_64 Linux, `x86_64-unknown-linux-musl`, using the stock Firecracker seccomp filters. For a direct build on Ubuntu with musl-tools, clang/libclang, cmake, pkg-config and libseccomp development headers:

```sh
rustup target add x86_64-unknown-linux-musl
CFLAGS_x86_64_unknown_linux_musl="-idirafter /usr/include -idirafter /usr/include/x86_64-linux-gnu" \
  cargo build --release --bin firecracker --target x86_64-unknown-linux-musl
```

The repository Cargo configuration places output under `build/cargo_target/`. Normal upstream build guidance still applies. The extra header search paths address the validated Ubuntu/musl environment; they are not required on every build system.

Set `ARIES_DIRTY_LOG=1` in the Firecracker process environment to force dirty tracking at initial guest-memory creation and prevent machine-config updates from disabling it. Alternatively, configure upstream `track_dirty_pages: true` before boot. With the environment unset, ordinary machine-config defaults are unchanged. Verify `GET /machine-config` reports tracking enabled. The endpoint rejects missing tracking; it never substitutes residency/mincore for dirty evidence.

No deployment binary is replaced automatically. Build and run an isolated binary with an isolated API socket first.

## API and lifecycle

New live collection requests require a booted, successful HTTP Pause hold (plain paused vCPU state is insufficient). Immutable export/info/ACK operations may run after Resume. Example bodies for `PUT /dirty-log`:

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

Firecracker v1.18.0's HTTP pause adapter enters a blocking API receive loop until Resume (`src/firecracker/src/api_server_adapter.rs`). Ordinary device EventManager callbacks are frozen in this state. Calling `pause_vm` directly only stops vCPUs and does not establish this complete HTTP contract.

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

## Independently selectable resources

All requests use HTTP JSON over the existing Unix API socket. The request tag is
`operation`; unknown fields are rejected. The legacy dirty endpoint above
remains independently usable. It does not open host pagemap or idle facilities.

| Resource | Operations | Additional facilities |
|---|---|---|
| `/page-idle` | arm, collect, export, ack | privileged host pagemap/kpageflags/kpagecount/page_idle |
| `/memory-census` | capture, export, ack | host pagemap and census metadata; never page_idle |
| `/metadata-capture` | begin, info, read_pages, scan, seal, abort, export, ack | bounded VMM guest-memory reads; optional census |
| `/trace-epochs` | arm, rotate, export, ack | only selected metric facilities |

Page-idle does not require KVM dirty logging. Metadata `quiescence_only` mode
does not require dirty logging either; it explicitly reports that weaker
consistency mode. Dirty-validated metadata requires a coordinator selecting
`dirty` and `metadata`. The coordinator owns reset rights for its selected
metrics until VM destruction; independent reset attempts are rejected.

Coordinator example:

```json
{"operation":"arm","epoch":1,"phase":"bootstrap","metrics":["dirty","idle","census","metadata"]}
{"operation":"rotate","vm_generation":"RETURNED_GENERATION","epoch":1,"next_epoch":2,"phase":"tool"}
```

Responses carry `schema`, `vm_generation` and bounded record references.
Rotation returns `epoch_record_id` plus selected
`access_record_id`/`dirty_record_id`/`census_record_id` aliases.
The initial arm record describes the discarded boot baseline, not a workload.
Independent page-idle uses arm/collect with the same epoch progression.
Collect, rotate and ACK require the expected generation; record IDs embed it.
Never infer an atomic common timestamp: access, dirty, census and rearm have
separate source timing and observer effects.

Census `capture` without `request_id` always samples fresh. An optional
`vm_generation` rejects a stale VM before side effects. With a nonempty
`request_id`, only an exact retry of the most recent successful manager
request returns the historical receipt without resampling; this is not a
persistent idempotency-key registry. A fresh census is rejected while metadata
is open or pending, before draining devices, because a drain may write guest
queue state. Immutable census export remains allowed.

## Bounded raw metadata capture

Begin supplies `capture_id`, mode `bootstrap` or `attribution`,
consistency `quiescence_only` or `dirty_validated`, and optional
`max_bytes`/`max_scan_bytes` and `capture_census` (default true). Set `capture_census: false` for selected raw-page reads without a census; scans require a census. Dirty-validated begin additionally supplies
the active `epoch` and `vm_generation`. All subsequent capture operations
require both capture ID and generation. By default begin captures a census before any
payload read and returns its immutable record reference and GPA regions. With
`capture_census: false`, no census is sampled or retained and the reference is null.

`read_pages` accepts 1–256 page-aligned GPAs. The first copy per GPA is cached;
repeated reads return those frozen bytes. Each result includes base64 data,
SHA256, and pre/post presence/PFN evidence. Every lowest-level guest read,
including bootstrap scans and translation-table reads, enters the read mask.

`scan` accepts a 1–256-byte base64 needle, page-aligned start/end/cursor and
a per-request scan budget of 4 KiB–1 MiB. Only pages resident in the begin census
are searched. Matches cross contiguous page and request boundaries using a
bounded tail; gaps/nonresident pages reset the tail. Matches identify their
starting page and byte offset. Per-request scanned bytes/hash and cumulative
scanned bytes are distinct. Scanned bytes are not all retained: scan logs retain
ordered read ranges/GPAs, hashes and matches, so full search-byte replay is not
claimed. Anchor bytes subsequently copied with read_pages are replayable.

Seal closes reads and freezes payloads. Quiescence-only seal permits Resume.
Dirty-validated seal creates an immutable `pending` record and blocks Resume
until the next logical coordinator rotate intersects the whole outgoing dirty
set with the complete read mask. Rotation creates a separate `valid` or
`invalid` record; the original pending record never changes. The response
lists both IDs and overlap counts. Info points to the final receipt while
retaining the sealed ID. Old capture info remains available while its records
are retained, including after Resume and a subsequent capture begin.

An explicit abort closes an open capture or discards a sealed-pending capture,
retaining original bytes and a separate `discarded` receipt with reason.
It is for failed capture/validation cleanup, not routine publication failure.
A poisoned collector rejects further measurement, including cached mutating
retries, but permits immutable export and abort. Once no open/pending capture
remains, native Resume is allowed for harness/evaluation/cleanup; restarting
the VM is still required before new measurement.

Successful HTTP Pause holds both vCPUs and the API event loop. New collection
may explicitly drain async block completions inside that hold. Rejected Resume
must not release the hold. External asynchronous I/O and conservative device
bitmap semantics remain part of the recorded consistency contract.

## Export, memory accounting, and raw evidence

Export takes `record_id`, `kind`, ordinal `offset` and `limit`,
returning frozen rows, total_rows, next_offset, complete and metadata.
It never reads live RAM or resamples PFNs. Page-data chunks are capped at 256
rows; other datasets at 4096. Export kinds include access, dirty, resident,
absent_ranges, pages, read_mask, scan_log and dirty_overlap as appropriate.
Absence ranges are separately paged, not an unbounded response header.
ACK releases record ownership only after the consumer has durably stored it.

Raw access rows preserve observed/idle/unknown and explicit unknown reasons;
dirty rows separately preserve CPU and conservative VMM positive evidence.
Residency preserves GPA aliases and zero/hidden PFNs. Census includes VMM
RSS/PSS where available. No eligibility filter removes dirty GPAs. Downstream
analysis may form an effective union, but must retain the source distinctions.

Compact bitmaps/PFN arrays are retained internally; bounded rows are generated
only for exports. The default aggregate capacity estimate limit is 1 GiB,
with at most eight retained records. Default retained metadata is 256 MiB;
scan allowance is zero for attribution and 1 GiB for bootstrap. Requested
budgets are bounded and reserved along with future seal/finalization/abort
receipt slots before side effects. The host module's approximately 258 MiB
8 GiB-RAM preflight figure is a conservative source-derived bound, not measured
RSS. Capacity estimates and operation-local Vec capacities are distinct from
OS RSS; this is not an OS-enforced heap limit.

Structured HTTP400 failures include schema/generation plus
`error.{code,message,mutation_performed,reset_performed,poisoned}`.
Only `backpressure` with all three flags false proves non-destructive
preflight rejection. The caller may finish exports/ACKs before explicitly
retrying; ambiguous transport failures must not trigger blind reset retries.


### Native lifecycle uncertainty

A failure executing native Pause or Resume can leave only some vCPUs transitioned
even if the reported VM state has not changed. Such an execution failure
permanently invalidates both new metric collection and legacy dirty logging,
clears successful HTTP-hold authority, and records a lifecycle-uncertainty
reason. A later successful Pause does not clear that measurement latch.
Immutable export/info and explicit capture discard remain available, followed
by native Resume for cleanup; new measurement requires a fresh VM.
This differs from a policy-rejected Resume while a capture is open/pending:
that request never executes the transition and preserves the valid held pause.

The source-versioned check summary is in [firecracker-tracer-validation.md](firecracker-tracer-validation.md).

## v1.18 port and feature selection

The v1.13 implementation remains on `memory-tracer-v1.13`. This branch starts
at the exact v1.18.0 release commit and adapts to its KVM/hypervisor and nested
MMIO device-manager interfaces. HTTP Pause/Resume never arms, samples or resets
trace bits. Explicit arm/collect/rotate own resets. Native fixtures exercise a
pause/resume cycle between marker writes and collection.

Selecting only dirty bits never constructs host-page inventory or metadata
capture state. Idle bits need a host PFN/presence inventory to classify reliable
access evidence; its internal census is retained even if no public census is
selected, but no independent census record is published. Census-only does not
open page-idle, kpageflags or kpagecount. Metadata capture with no census opens
neither host inventory nor idle facilities. These are residual costs to measure,
not evidence of zero overhead. KVM dirty logging and guest-memory bitmap
allocation are boot-time choices; disabling per-epoch collection cannot undo them.
