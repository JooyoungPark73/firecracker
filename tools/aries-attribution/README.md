# ARIES attribution companion

Experimental external Linux metadata decoder for the Firecracker tracer fork.
Kernel-specific interpretation lives here; Firecracker owns live guest-memory
reads, host residency/access/dirty state, capture IDs and bounded immutable bytes.
This directory does not read host /proc process memory or pagemap.

## Components

- capture_memory.py: provider-owned guest bytes, canonical four-level x86-64
  GVA translation, 4KiB/2MiB/1GiB leaves, authoritative GPA-region validation,
  first-copy cache and batches of at most 256 pages.
- memory.py: offline pinned ELF symbols and kernel-anchor validation. The
  provider performs bounded resident-pattern search; full scanned RAM is not
  retained. Replay validates retained anchors/metadata, not the omitted full scan.
- decoder.py: guest page categories and process/VMA associations.
- owners.py: mm sharing/thread roster, kernel-global labels, stack/page-table and
  compound-page evidence. No fictitious PID for a global or unresolved page.
- file_names.py: inode aliases, mount namespaces, internal names and explicit
  missing-name reasons. Unnamed is not automatically intrinsically nameless.
- compression.py: metadata-only sample selection from the immutable VMM census;
  no payload compression.
- replay.py: retained-byte replay with no live guest connection.

The transport binding uses the separate metadata-capture and memory-census
APIs. A controlled native VM fixture checks bootstrap consistency, a known
named-file/process association, anonymous ownership and exact retained replay. An optional trace-epochs coordinator
binds independently selectable access/dirty/census/capture metrics for the
study. Metadata-only users must not require page-idle permissions; their
quiescence/consistency qualification is explicitly weaker without dirty evidence.

## Capture semantics

Bootstrap runs in an explicit premeasurement paused epoch. A bounded VMM pattern
scan finds candidate kernel banners; pinned symbols, KASLR offsets, kernel page
tables and VMCOREINFO validate anchors. Scan bytes/read masks and retained
metadata have separate budgets. LA57 and non-x86-64 layouts are unsupported.

A metadata capture returns first-copied immutable pages for each GPA. Adjacent
prefetch is bounded (default 4 pages) and contained within one RAM region; known
struct-page frontiers are batched. All prefetched and failed/partial reads remain
in VMM provenance, even if rich decoding does not consume them.

Sealing forbids further live reads. With coordinated metrics, attribution stays
pending until the next logical dirty rotation validates the full boundary-to-seal
read mask; no hidden dirty reset happens at seal. Any overlap means possible
inconsistency, not proven corruption. Read-induced host access/residency effects
are retained, and must not be subtracted to claim guest-only gap accesses.

The initial adapter still performs rich row materialization during discovery.
Serialization is outside the live-read interval, and sealed captures permit
offline replay. Batching reduces transport calls but does not yet establish a
short constant pause for large workloads; a dedicated closure walker is future
work. Budget exhaustion is an invalid/partial capture, not successful empty data.

## Local verification

Run on an experimental worker:

    PYTHONPATH=tools/aries-attribution python3 -m unittest discover -s tools/aries-attribution -p 'test_*.py'

Replay an existing capture with --step, --bootstrap and --output using replay.py.
Exact row parity and declared summary-version differences are reported separately.
All layouts and kernel ELF inputs must match the running guest kernel; no guest
kernel modification is required.


## Capture command

The controller pauses and drains the VM, arms the chosen metrics, and supplies
the returned generation/epoch. For example, after a coordinated bootstrap arm:

    python3 tools/aries-attribution/capture.py bootstrap \
      --socket /path/to/api.sock --capture-id bootstrap-1 \
      --vm-generation GENERATION --epoch 1 --config kernel-layout-config.json \
      --output retained/bootstrap

For metadata-only use, select --consistency quiescence_only explicitly. This
does not initialize idle or dirty tracking and must not be reported as
dirty-validated coherence. The CLI checks required inputs and rejects payload
compression before opening a capture.

Use attribute with --bootstrap and --reading for a tool-end capture. Supply
--source-epoch and --source-record-id from the immutable tool epoch, and --epoch
for the immediately following paused capture epoch. The reading must carry the
same vm_generation, source_epoch, source_record_id, capture_epoch, authoritative
regions and layout_identity as the bootstrap and controller. These bindings are
checked before beginning a live capture; legacy offline replay remains separate. The
controller may wait for the atomically published capture-seal.json rather than
all artifact serialization. It must perform the next logical rotate before
resuming a dirty-validated capture. Pending and finalized records have distinct
IDs; finalize_capture checks identity and the explicit verdict, not just an ID
change. Invalid/discarded verdicts remain invalid.

process_adapter.py owns only its child process. It bounds a stuck read barrier,
reaps the reader, then sends an explicit capture-abort reason. It never resumes
the VM or changes epochs. epochs.py is an optional coordinator binding; its
immutable exports are bounded JSONL-gzip datasets with hashes and a durable
manifest. Standalone capture users do not import or initialize that coordinator.

Capture exports include the complete VMM first-copy set, including speculative
prefetch, plus read/scan masks and search receipts. The decoder's replay metadata
is a separate consumed-byte closure. Neither artifact format substitutes the
dirty-consistency verdict. Record acknowledgements require the expected VM
generation and must follow durable artifact retention.


A publication failure after seal retains the sealed capture and never implicitly
aborts it. A distinct discard_for_cleanup operation is reserved for failed or
poisoned epoch rotation: it reaps the reader, explicitly requests a discarded
receipt, preserves the original sealed evidence, and makes no successful
consistency claim. The controller still owns any subsequent Resume.

Structured VMM errors retain their response body and phase timings. Only an
explicit matching-generation backpressure response with mutation_performed,
reset_performed and poisoned all false is eligible for a caller-controlled
wait/export/ack/retry. Transport timeouts and malformed errors are never inferred
safe. Attribution defaults to a zero scan budget; bootstrap requests its separate
bounded scan budget. Accepted server budgets and aggregate backpressure govern
actual capacity.

## Kernel layout inputs

A small pinned profile is included under layouts/linux-5.10.239/; no kernel
binary or raw capture is shipped. Read its provenance README, including the
GCC 11.5 offset-generator versus GCC 11.4 deployed-kernel limitation. This is a
hash-pinned validated profile, not a claim that an exact kernel rebuild is
available. Supply the original matching guest ELF and create a configuration:

    python3 tools/aries-attribution/make_config.py \
      --kernel-elf /path/to/the-actual-guest-vmlinux \
      --output /path/to/kernel-layout-config.json

The command refuses a different ELF hash or an existing output. Other kernels
need independently generated and validated base, owner and name layouts. Keep
bootstrap ELF-hash, VMCOREINFO, KASLR/page-table and process/VMA checks enabled.
