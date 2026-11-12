// SPDX-License-Identifier: Apache-2.0
//! Finite-envelope host page-idle and residency observations.
//!
//! These are not exact guest access sets: no secondary-TLB flush is added,
//! endpoint PFN checks miss transient A-B-A changes, and ambiguous pages remain
//! unknown. No KVM dirty-log dependency. All kernel I/O uses seek/read/write.
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};
use utils::time::{ClockType, get_time_ns};
use vm_memory::{Address, GuestMemoryBackend, GuestMemoryRegion};

use crate::vstate::memory::GuestMemoryMmap;

const PAGE: u64 = 4096;
const PRESENT: u64 = 1 << 63;
const PFN_MASK: u64 = (1 << 55) - 1;
const LRU: u64 = 1 << 5;
const COMPOUND: u64 = (1 << 15) | (1 << 16) | (1 << 17) | (1 << 22);
const SHARED: u64 = (1 << 21) | (1 << 24);
const PROC_WORDS: usize = 8192;
// Read-only flags/count metadata can span small PFN holes. Do not apply this
// to page-idle I/O, whose reads/writes have page-tracking side effects.
const PROC_MAX_WORD_GAP: u64 = 4;
const IDLE_WORDS: usize = 512; // kernfs binary I/O is capped at one host page.
const MAX_CHUNK_ROWS: usize = 256;
const DEFAULT_COMPONENT_LIMIT: usize = 512 * 1024 * 1024;

fn mono() -> u64 {
    get_time_ns(ClockType::Monotonic)
}
fn cpu() -> u64 {
    get_time_ns(ClockType::ProcessCpu)
}
fn zeros(n: usize) -> Result<Vec<u64>, String> {
    let mut v = Vec::new();
    v.try_reserve_exact(n)
        .map_err(|e| format!("host-page allocation: {e}"))?;
    v.resize(n, 0);
    Ok(v)
}
fn bit(words: &[u64], page: usize) -> bool {
    words[page / 64] & (1 << (page % 64)) != 0
}
fn put(words: &mut [u64], page: usize) {
    words[page / 64] |= 1 << (page % 64);
}
fn io_read<T: Read + Seek>(io: &mut T, index: u64, out: &mut [u8]) -> Result<(), String> {
    let offset = index
        .checked_mul(8)
        .ok_or("kernel metadata offset overflow")?;
    io.seek(SeekFrom::Start(offset))
        .map_err(|e| format!("kernel metadata seek: {e}"))?;
    let n = io
        .read(out)
        .map_err(|e| format!("kernel metadata read: {e}"))?;
    if n != out.len() {
        return Err(format!("short kernel metadata read: {n}/{}", out.len()));
    }
    Ok(())
}
fn read_words<T: Read + Seek>(
    io: &mut T,
    indices: &[u64],
    limit: usize,
) -> Result<Vec<u64>, String> {
    if limit == 0 || limit > PROC_WORDS || indices.windows(2).any(|p| p[0] >= p[1]) {
        return Err("invalid sorted metadata word request".into());
    }
    let mut out = zeros(indices.len())?;
    let mut first = 0;
    let mut bytes = [0u8; PROC_WORDS * 8];
    while first < indices.len() {
        let mut end = first + 1;
        while end < indices.len()
            && indices[end] - indices[first] < limit as u64
            && indices[end] - indices[end - 1] <= PROC_MAX_WORD_GAP
        {
            end += 1;
        }
        let span = (indices[end - 1] - indices[first] + 1) as usize;
        io_read(io, indices[first], &mut bytes[..span * 8])?;
        if span == end - first {
            for (j, chunk) in bytes[..span * 8].chunks_exact(8).enumerate() {
                out[first + j] = u64::from_ne_bytes(chunk.try_into().unwrap());
            }
        } else {
            for j in first..end {
                let at = (indices[j] - indices[first]) as usize * 8;
                out[j] = u64::from_ne_bytes(bytes[at..at + 8].try_into().unwrap());
            }
        }
        first = end;
    }
    Ok(out)
}
fn write_idle<T: Write + Seek>(io: &mut T, words: &[(u64, u64)]) -> Result<(), String> {
    if words.windows(2).any(|p| p[0].0 >= p[1].0) {
        return Err("idle words not sorted unique".into());
    }
    let mut first = 0;
    let mut bytes = [0u8; IDLE_WORDS * 8];
    while first < words.len() {
        let mut end = first + 1;
        while end < words.len()
            && end - first < IDLE_WORDS
            && words[end - 1].0.checked_add(1) == Some(words[end].0)
        {
            end += 1;
        }
        for (i, (_, value)) in words[first..end].iter().enumerate() {
            bytes[i * 8..i * 8 + 8].copy_from_slice(&value.to_ne_bytes());
        }
        let offset = words[first]
            .0
            .checked_mul(8)
            .ok_or("idle offset overflow")?;
        io.seek(SeekFrom::Start(offset))
            .map_err(|e| format!("idle seek: {e}"))?;
        let n = io
            .write(&bytes[..(end - first) * 8])
            .map_err(|e| format!("idle write: {e}"))?;
        if n != (end - first) * 8 {
            return Err(format!(
                "short idle bitmap write: {n}/{}",
                (end - first) * 8
            ));
        }
        first = end;
    }
    Ok(())
}
fn read_idle_in_place<T: Read + Seek>(io: &mut T, words: &mut [(u64, u64)]) -> Result<(), String> {
    let mut first = 0;
    let mut bytes = [0u8; IDLE_WORDS * 8];
    while first < words.len() {
        let mut end = first + 1;
        while end < words.len()
            && end - first < IDLE_WORDS
            && words[end - 1].0.checked_add(1) == Some(words[end].0)
        {
            end += 1;
        }
        io_read(io, words[first].0, &mut bytes[..(end - first) * 8])?;
        for (i, chunk) in bytes[..(end - first) * 8].chunks_exact(8).enumerate() {
            words[first + i].1 = u64::from_ne_bytes(chunk.try_into().unwrap());
        }
        first = end;
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
/// Registered guest-RAM geometry; GPA ranges are end-exclusive.
pub struct Region {
    /// VMM memory-slot identifier.
    pub slot: u32,
    /// First guest physical byte address.
    pub guest_base: u64,
    /// Slot length in bytes.
    pub bytes: u64,
    /// Number of 4096-byte guest pages.
    pub page_count: u64,
}
#[derive(Clone, Debug, Serialize)]
/// One requested resident-row export; PFN zero remains unresolved.
pub struct ResidentPage {
    /// Guest physical page base address.
    pub gpa: u64,
    /// Host PFN; zero is masked or unresolved, not absence.
    pub pfn: u64,
    /// Raw flags when the nonzero PFN was resolved.
    pub host_kpageflags: Option<u64>,
    /// Raw map count when the nonzero PFN was resolved.
    pub host_kpagecount: Option<u64>,
}
#[derive(Clone, Debug, Serialize)]
/// Boundary backing counts and separate total VMM RSS/PSS.
pub struct Population {
    /// Resident guest-page mappings, including aliases.
    pub resident_gpa_pages: u64,
    /// Distinct resolved host PFNs; not total VMM RSS.
    pub unique_nonzero_host_pfns: u64,
    /// Resident mappings with masked/unresolved PFN zero.
    pub zero_pfn_pages: u64,
    /// Total VMM RSS in KiB, including its observer buffers.
    pub vmm_rss_kib: Option<u64>,
    /// Total VMM PSS in KiB when available.
    pub vmm_pss_kib: Option<u64>,
}
#[derive(Clone, Debug, Serialize)]
/// One bounded access-row export; absent PFNs differ from present PFN zero.
pub struct AccessRow {
    /// Guest physical page base address.
    pub gpa: u64,
    /// observed, idle, or unknown within the finite envelope.
    pub state: String,
    /// Conservative reason; never silently converted to clean idle.
    pub unknown_reason: Option<String>,
    /// Arm PFN; None means absent and Some(0) unresolved.
    pub before_pfn: Option<u64>,
    /// Intermediate collect PFN with the same zero semantics.
    pub after_pfn: Option<u64>,
    /// Final collect PFN with the same zero semantics.
    pub final_pfn: Option<u64>,
}
#[derive(Debug)]
struct SlotCensus {
    region: Region,
    present: Vec<u64>,
    rank: Vec<u64>,
    absent_starts: Vec<u64>,
    absent_rank: Vec<u64>,
    pfns: Vec<u64>,
    flags: Vec<u64>,
    counts: Vec<u64>,
}
impl SlotCensus {
    fn bytes(&self) -> usize {
        (self.present.capacity()
            + self.rank.capacity()
            + self.absent_starts.capacity()
            + self.absent_rank.capacity()
            + self.pfns.capacity()
            + self.flags.capacity()
            + self.counts.capacity())
            * 8
    }
    fn page(&self, i: usize) -> Option<PageMeta> {
        bit(&self.present, i).then(|| PageMeta {
            pfn: self.pfns[i],
            flags: self.flags[i],
            count: self.counts[i],
        })
    }
}
#[derive(Debug)]
/// Compact boundary census; large page data is exported in chunks.
pub struct Inventory {
    slots: Vec<SlotCensus>,
    /// Counts from this boundary, separate from configured RAM.
    pub resident_population: Population,
    /// Host monotonic start of the census.
    pub inventory_begin_ns: u64,
    /// Host monotonic end of the census.
    pub inventory_end_ns: u64,
    vmm_memory_error: Option<String>,
    inventory_peak_capacity_bytes: usize,
}
fn check_chunk(limit: usize) -> Result<(), String> {
    if !(1..=MAX_CHUNK_ROWS).contains(&limit) {
        Err("row limit must be1..256".into())
    } else {
        Ok(())
    }
}
fn rank_index(words: &[u64]) -> Result<Vec<u64>, String> {
    let mut rank = zeros(words.len() + 1)?;
    for (i, w) in words.iter().enumerate() {
        rank[i + 1] = rank[i] + u64::from(w.count_ones());
    }
    Ok(rank)
}
fn selected_pages(words: &[u64], rank: &[u64], offset: &mut u64, limit: usize) -> Vec<usize> {
    let total = *rank.last().unwrap_or(&0);
    if *offset >= total {
        *offset -= total;
        return Vec::new();
    }
    let start = rank.partition_point(|n| *n <= *offset).saturating_sub(1);
    let mut skip = *offset - rank[start];
    *offset = 0;
    let mut out = Vec::with_capacity(limit);
    for (wi, &word) in words.iter().enumerate().skip(start) {
        let mut w = word;
        while w != 0 {
            let b = w.trailing_zeros() as usize;
            w &= w - 1;
            if skip > 0 {
                skip -= 1;
                continue;
            }
            out.push(wi * 64 + b);
            if out.len() == limit {
                return out;
            }
        }
    }
    out
}
fn absent_starts(present: &[u64], pages: usize) -> Result<Vec<u64>, String> {
    let mut starts = zeros(present.len())?;
    let mut prior = false;
    for (i, &word) in present.iter().enumerate() {
        let mut absent = !word;
        if i + 1 == present.len() && pages % 64 != 0 {
            absent &= (1u64 << (pages % 64)) - 1;
        }
        starts[i] = absent & !((absent << 1) | u64::from(prior));
        prior = absent & (1 << 63) != 0;
    }
    Ok(starts)
}
fn next_present(words: &[u64], start: usize, pages: usize) -> usize {
    for (wi, &word) in words.iter().enumerate().skip(start / 64) {
        let mut w = word;
        if wi == start / 64 {
            w &= u64::MAX << (start % 64);
        }
        if w != 0 {
            return (wi * 64 + w.trailing_zeros() as usize).min(pages);
        }
    }
    pages
}

impl Inventory {
    /// Returns only small fixed slot geometry.
    pub fn regions(&self) -> Vec<Region> {
        self.slots.iter().map(|s| s.region.clone()).collect()
    }
    /// Counts retained buffer capacities; excludes allocator bookkeeping and caller exports.
    pub fn estimated_bytes(&self) -> usize {
        size_of::<Self>()
            + self.slots.capacity() * size_of::<SlotCensus>()
            + self.slots.iter().map(SlotCensus::bytes).sum::<usize>()
    }
    /// Returns resident GPA mapping count, including aliases.
    pub fn resident_count(&self) -> u64 {
        self.resident_population.resident_gpa_pages
    }
    /// Returns a bounded header; page rows and absent ranges use chunk exports.
    pub fn metadata_value(&self) -> Value {
        json!({"schema":"aries.host-memory-census.v1","page_bytes":PAGE,"regions":self.regions(),
            "resident_population":self.resident_population,"inventory_begin_ns":self.inventory_begin_ns,
            "inventory_end_ns":self.inventory_end_ns,"collector_buffer_capacity_bytes":self.estimated_bytes(),
            "collector_operation_peak_capacity_bytes":self.inventory_peak_capacity_bytes,
            "buffer_accounting_scope":"owned Vec capacities, excludes allocator bookkeeping and caller export/metadata buffers",
            "vmm_memory_error":self.vmm_memory_error,"absent_gpa_range_count":self.absent_range_count(),
            "absent_gpa_ranges_encoding":"paged ranges; no unbounded header array","ordinal_index":"per-word prefix ranks",
            "scope":"boundary guest backing; total VMM RSS/PSS includes collector heap; PFN zero is unknown"})
    }
    /// Exports at most256 resident rows by ordinal using prefix-rank selection.
    pub fn resident_rows(
        &self,
        mut offset: u64,
        limit: usize,
    ) -> Result<Vec<ResidentPage>, String> {
        check_chunk(limit)?;
        let mut out = Vec::with_capacity(limit);
        for s in &self.slots {
            let left = limit - out.len();
            for i in selected_pages(&s.present, &s.rank, &mut offset, left) {
                let p = s.pfns[i];
                out.push(ResidentPage {
                    gpa: s.region.guest_base + i as u64 * PAGE,
                    pfn: p,
                    host_kpageflags: (p != 0).then_some(s.flags[i]),
                    host_kpagecount: (p != 0).then_some(s.counts[i]),
                });
            }
            if out.len() == limit {
                break;
            }
        }
        Ok(out)
    }
    /// Returns exact absent-range count without a large header array.
    pub fn absent_range_count(&self) -> u64 {
        self.slots
            .iter()
            .map(|s| s.absent_rank.last().copied().unwrap_or(0))
            .sum()
    }
    /// Exports at most256 absent ranges by ordinal; end addresses are exclusive.
    pub fn absent_ranges(&self, mut offset: u64, limit: usize) -> Result<Vec<[u64; 2]>, String> {
        check_chunk(limit)?;
        let mut out = Vec::with_capacity(limit);
        for s in &self.slots {
            for first in selected_pages(
                &s.absent_starts,
                &s.absent_rank,
                &mut offset,
                limit - out.len(),
            ) {
                let end = next_present(&s.present, first, s.region.page_count as usize);
                out.push([
                    s.region.guest_base + first as u64 * PAGE,
                    s.region.guest_base + end as u64 * PAGE,
                ]);
            }
            if out.len() == limit {
                break;
            }
        }
        Ok(out)
    }
    /// Returns resident page bases in the inclusive-cursor/exclusive-end range.
    pub fn resident_gpas_from(
        &self,
        cursor: u64,
        end: u64,
        limit: usize,
    ) -> Result<Vec<u64>, String> {
        check_chunk(limit)?;
        if cursor > end {
            return Err("resident scan range reversed".into());
        }
        let mut out = Vec::with_capacity(limit);
        for s in &self.slots {
            if s.region.guest_base >= end || s.region.guest_base + s.region.bytes <= cursor {
                continue;
            }
            let first = cursor.saturating_sub(s.region.guest_base).div_ceil(PAGE) as usize;
            for wi in first / 64..s.present.len() {
                let mut w = s.present[wi];
                if wi == first / 64 {
                    w &= u64::MAX << (first % 64);
                }
                while w != 0 {
                    let b = w.trailing_zeros() as usize;
                    w &= w - 1;
                    let gpa = s.region.guest_base + (wi * 64 + b) as u64 * PAGE;
                    if gpa >= end {
                        return Ok(out);
                    }
                    out.push(gpa);
                    if out.len() == limit {
                        return Ok(out);
                    }
                }
            }
        }
        Ok(out)
    }
}

#[derive(Clone, Copy, Debug)]
struct PageMeta {
    pfn: u64,
    flags: u64,
    count: u64,
}
const ABSENT: u8 = 0;
const OBSERVED: u8 = 1;
const IDLE: u8 = 2;
const MASKED: u8 = 3;
const COMPOUND_UNKNOWN: u8 = 4;
const SHARED_UNKNOWN: u8 = 5;
const NON_LRU: u8 = 6;
const NONEXCLUSIVE: u8 = 7;
const NEW: u8 = 8;
const DISAPPEARED: u8 = 9;
const CHANGED: u8 = 10;
fn reason(code: u8) -> Option<&'static str> {
    match code {
        MASKED => Some("pfn-masked-or-zero"),
        COMPOUND_UNKNOWN => Some("compound-resolution-unsupported"),
        SHARED_UNKNOWN => Some("shared-or-zero-page"),
        NON_LRU => Some("not-on-lru"),
        NONEXCLUSIVE => Some("nonexclusive-or-transition"),
        NEW => Some("newly-resident-unarmed"),
        DISAPPEARED => Some("became-nonresident"),
        CHANGED => Some("host-mapping-changed"),
        _ => None,
    }
}
fn eligibility(p: PageMeta) -> u8 {
    if p.pfn == 0 {
        MASKED
    } else if p.flags & COMPOUND != 0 {
        COMPOUND_UNKNOWN
    } else if p.flags & SHARED != 0 {
        SHARED_UNKNOWN
    } else if p.flags & LRU == 0 {
        NON_LRU
    } else if p.count != 1 {
        NONEXCLUSIVE
    } else {
        0
    }
}
fn classify(
    before: Option<PageMeta>,
    mid: Option<PageMeta>,
    end: Option<PageMeta>,
    idle_bit: Option<bool>,
) -> Result<u8, String> {
    if before.is_none() && mid.is_none() && end.is_none() {
        return Ok(ABSENT);
    }
    let Some(a) = before else {
        return Ok(NEW);
    };
    let (Some(b), Some(c)) = (mid, end) else {
        return Ok(DISAPPEARED);
    };
    if a.pfn != b.pfn || a.pfn != c.pfn {
        return Ok(CHANGED);
    }
    for p in [a, b, c] {
        let r = eligibility(p);
        if r != 0 {
            return Ok(r);
        }
    }
    idle_bit
        .map(|set| if set { IDLE } else { OBSERVED })
        .ok_or_else(|| "eligible page missing sampled idle word".into())
}
#[derive(Debug)]
struct ArmState {
    before: Inventory,
    words: Vec<(u64, u64)>,
    begin: u64,
    end: u64,
    cpu_ns: u64,
    write_begin: u64,
    write_end: u64,
}
#[derive(Debug)]
struct AccessSlot {
    region: Region,
    codes: Vec<u8>,
    classified: Vec<u64>,
    rank: Vec<u64>,
    before_pfns: Vec<u64>,
    before_present: Vec<u64>,
    mid_pfns: Vec<u64>,
    mid_present: Vec<u64>,
}
#[derive(Debug)]
/// Compact arm/mid/final access classification with immutable end census.
pub struct AccessRecord {
    slots: Vec<AccessSlot>,
    metadata: Value,
    /// Shared immutable final census; callers must deduplicate Arc accounting.
    pub end_census: Arc<Inventory>,
}
impl AccessRecord {
    /// Returns a bounded header; page rows and absent ranges use chunk exports.
    pub fn metadata_value(&self) -> Value {
        self.metadata.clone()
    }
    /// Returns classified rows; pages absent at all samples are omitted.
    pub fn row_count(&self) -> u64 {
        self.slots
            .iter()
            .map(|s| s.rank.last().copied().unwrap_or(0))
            .sum()
    }

    /// Counts retained buffer capacities; excludes allocator bookkeeping and caller exports.
    pub fn estimated_bytes(&self) -> usize {
        size_of::<Self>()
            + self.end_census.estimated_bytes()
            + self.slots.capacity() * size_of::<AccessSlot>()
            + self
                .slots
                .iter()
                .map(|s| {
                    s.codes.capacity()
                        + (s.classified.capacity()
                            + s.rank.capacity()
                            + s.before_pfns.capacity()
                            + s.before_present.capacity()
                            + s.mid_pfns.capacity()
                            + s.mid_present.capacity())
                            * 8
                })
                .sum::<usize>()
    }
    /// Exports at most256 access rows by ordinal using prefix-rank selection.
    pub fn rows(&self, mut offset: u64, limit: usize) -> Result<Vec<AccessRow>, String> {
        check_chunk(limit)?;
        let mut out = Vec::with_capacity(limit);
        for (s, e) in self.slots.iter().zip(&self.end_census.slots) {
            for i in selected_pages(&s.classified, &s.rank, &mut offset, limit - out.len()) {
                let code = s.codes[i];
                out.push(AccessRow {
                    gpa: s.region.guest_base + i as u64 * PAGE,
                    state: if code == OBSERVED {
                        "observed"
                    } else if code == IDLE {
                        "idle"
                    } else {
                        "unknown"
                    }
                    .into(),
                    unknown_reason: reason(code).map(str::to_owned),
                    before_pfn: bit(&s.before_present, i).then_some(s.before_pfns[i]),
                    after_pfn: bit(&s.mid_present, i).then_some(s.mid_pfns[i]),
                    final_pfn: bit(&e.present, i).then_some(e.pfns[i]),
                });
            }
            if out.len() == limit {
                break;
            }
        }
        Ok(out)
    }
}

#[derive(Debug)]
/// Selected host proc/sysfs resources; no KVM dirty-log dependency.
pub struct HostPages {
    pagemap: File,
    flags: Option<File>,
    counts: Option<File>,
    idle: Option<File>,
    armed: Option<ArmState>,
    invalid: bool,
    component_limit: usize,
}
impl HostPages {
    /// Counts retained buffer capacities; excludes allocator bookkeeping and caller exports.
    pub fn estimated_bytes(&self) -> usize {
        size_of::<Self>()
            + self
                .armed
                .as_ref()
                .map(|a| {
                    a.before.estimated_bytes()
                        + a.words.capacity() * size_of::<(u64, u64)>()
                        + size_of::<ArmState>()
                })
                .unwrap_or(0)
    }
    /// Opens self pagemap; idle files only when selected, other flags/counts lazily.
    pub fn new(with_idle: bool) -> Result<Self, String> {
        if crate::arch::host_page_size() != PAGE as usize {
            return Err("host pages must be4096bytes".into());
        }
        let pagemap =
            File::open("/proc/self/pagemap").map_err(|e| format!("open self pagemap: {e}"))?;
        let mut s = Self {
            pagemap,
            flags: None,
            counts: None,
            idle: None,
            armed: None,
            invalid: false,
            component_limit: DEFAULT_COMPONENT_LIMIT,
        };
        if with_idle {
            s.ensure_flags()?;
            s.idle = Some(
                File::options()
                    .read(true)
                    .write(true)
                    .open("/sys/kernel/mm/page_idle/bitmap")
                    .map_err(|e| format!("open page-idle bitmap: {e}"))?,
            );
        }
        Ok(s)
    }
    fn ensure_flags(&mut self) -> Result<(), String> {
        if self.flags.is_none() {
            self.flags =
                Some(File::open("/proc/kpageflags").map_err(|e| format!("open kpageflags: {e}"))?);
        }
        if self.counts.is_none() {
            self.counts =
                Some(File::open("/proc/kpagecount").map_err(|e| format!("open kpagecount: {e}"))?);
        }
        Ok(())
    }
    pub(super) fn layout(mem: &GuestMemoryMmap) -> Result<Vec<(Region, u64)>, String> {
        let mut out = Vec::new();
        for (slot, r) in mem.iter().enumerate() {
            let g = r.start_addr().raw_value();
            let n = r.len();
            let h = r.as_ptr() as usize as u64;
            if n == 0
                || g % PAGE != 0
                || n % PAGE != 0
                || h % PAGE != 0
                || g.checked_add(n).is_none()
                || h.checked_add(n).is_none()
            {
                return Err("invalid RAM slot layout".into());
            }
            out.push((
                Region {
                    slot: u32::try_from(slot).map_err(|_| "slot index overflow")?,
                    guest_base: g,
                    bytes: n,
                    page_count: n / PAGE,
                },
                h / PAGE,
            ));
        }
        out.sort_by_key(|r| r.0.guest_base);
        if out.is_empty()
            || out
                .windows(2)
                .any(|w| w[0].0.guest_base + w[0].0.bytes > w[1].0.guest_base)
        {
            return Err("empty/overlapping RAM slots".into());
        }
        let mut host: Vec<_> = out
            .iter()
            .map(|(r, h)| (*h * PAGE, *h * PAGE + r.bytes))
            .collect();
        host.sort_unstable();
        if host.windows(2).any(|w| w[0].1 > w[1].0) {
            return Err("overlapping host RAM mappings unsupported".into());
        }
        Ok(out)
    }
    /// Worst-case component allocation estimate, including sparse PFN words.
    /// Coordinator must add retained records/metadata/export buffers BEFORE reset.
    pub fn capacity_preflight(mem: &GuestMemoryMmap) -> Result<usize, String> {
        let pages = Self::layout(mem)?
            .iter()
            .try_fold(0usize, |n, (r, _)| {
                n.checked_add(usize::try_from(r.page_count).ok()?)
            })
            .ok_or("RAM page count overflow")?;
        pages
            .checked_mul(128)
            .and_then(|n| n.checked_add(2 * 1024 * 1024))
            .ok_or_else(|| "host-page capacity estimate overflow".into())
    }
    fn preflight(&self, mem: &GuestMemoryMmap, collect: bool) -> Result<(), String> {
        let estimate = Self::capacity_preflight(mem)?;
        let needed = if collect {
            estimate
        } else {
            estimate / 2 + 1024 * 1024
        };
        if needed > self.component_limit {
            return Err(format!(
                "host-page buffer preflight {needed} exceeds limit {}",
                self.component_limit
            ));
        }
        Ok(())
    }
    /// Samples resident PFNs/flags/counts and VMM RSS/PSS without guest-data reads.
    pub fn inventory(&mut self, mem: &GuestMemoryMmap) -> Result<Inventory, String> {
        self.preflight(mem, false)?;
        let begin = mono();
        let mut slots = Vec::new();
        let layout = Self::layout(mem)?;
        let total_pages = layout
            .iter()
            .try_fold(0usize, |n, (r, _)| {
                n.checked_add(usize::try_from(r.page_count).ok()?)
            })
            .ok_or("page count overflow")?;
        let mut pfn_index = Vec::new();
        pfn_index
            .try_reserve_exact(total_pages)
            .map_err(|e| e.to_string())?;
        let mut resident = 0u64;
        let mut zero = 0u64;
        let mut bytes = [0u8; PROC_WORDS * 8];
        for (r, host_page) in layout {
            let n = usize::try_from(r.page_count).map_err(|_| "page count overflow")?;
            let mut s = SlotCensus {
                region: r,
                present: zeros(n.div_ceil(64))?,
                rank: Vec::new(),
                absent_starts: Vec::new(),
                absent_rank: Vec::new(),
                pfns: zeros(n)?,
                flags: zeros(n)?,
                counts: zeros(n)?,
            };
            for start in (0..n).step_by(PROC_WORDS) {
                let count = (n - start).min(PROC_WORDS);
                io_read(
                    &mut self.pagemap,
                    host_page
                        .checked_add(start as u64)
                        .ok_or("pagemap offset overflow")?,
                    &mut bytes[..count * 8],
                )?;
                for (j, c) in bytes[..count * 8].chunks_exact(8).enumerate() {
                    let e = u64::from_ne_bytes(c.try_into().unwrap());
                    if e & PRESENT != 0 {
                        let i = start + j;
                        let p = e & PFN_MASK;
                        put(&mut s.present, i);
                        s.pfns[i] = p;
                        resident += 1;
                        if p == 0 {
                            zero += 1;
                        } else {
                            pfn_index.push(p);
                        }
                    }
                }
            }
            s.rank = rank_index(&s.present)?;
            s.absent_starts = absent_starts(&s.present, n)?;
            s.absent_rank = rank_index(&s.absent_starts)?;
            slots.push(s);
        }
        pfn_index.sort_unstable();
        pfn_index.dedup();
        let mut inventory_peak_capacity_bytes = size_of::<Inventory>()
            + slots.capacity() * size_of::<SlotCensus>()
            + slots.iter().map(SlotCensus::bytes).sum::<usize>()
            + pfn_index.capacity() * 8;
        if !pfn_index.is_empty() {
            self.ensure_flags()?;
            let flags = read_words(self.flags.as_mut().unwrap(), &pfn_index, PROC_WORDS)?;
            let counts = read_words(self.counts.as_mut().unwrap(), &pfn_index, PROC_WORDS)?;
            inventory_peak_capacity_bytes += (flags.capacity() + counts.capacity()) * 8;
            for s in &mut slots {
                for i in 0..s.pfns.len() {
                    if bit(&s.present, i) && s.pfns[i] != 0 {
                        let at = pfn_index
                            .binary_search(&s.pfns[i])
                            .map_err(|_| "PFN census lookup failed")?;
                        s.flags[i] = flags[at];
                        s.counts[i] = counts[at];
                    }
                }
            }
        }
        let (rss, pss, error) = smaps();
        Ok(Inventory {
            slots,
            resident_population: Population {
                resident_gpa_pages: resident,
                unique_nonzero_host_pfns: pfn_index.len() as u64,
                zero_pfn_pages: zero,
                vmm_rss_kib: rss,
                vmm_pss_kib: pss,
            },
            inventory_begin_ns: begin,
            inventory_end_ns: mono(),
            vmm_memory_error: error,
            inventory_peak_capacity_bytes,
        })
    }
    /// Returns the containing page PFN for one in-RAM address; no guest data read.
    pub fn page_pfn(&mut self, mem: &GuestMemoryMmap, gpa: u64) -> Result<Option<u64>, String> {
        for (r, host_page) in Self::layout(mem)? {
            if gpa >= r.guest_base && gpa - r.guest_base < r.bytes {
                let at = host_page
                    .checked_add((gpa - r.guest_base) / PAGE)
                    .ok_or("pagemap offset overflow")?;
                let mut raw = [0u8; 8];
                io_read(&mut self.pagemap, at, &mut raw)?;
                let value = u64::from_ne_bytes(raw);
                return Ok((value & PRESENT != 0).then_some(value & PFN_MASK));
            }
        }
        Err("GPA outside RAM slots".into())
    }
    /// Marks eligible exclusive pages idle; rejects double arm and failed trackers.
    pub fn arm(&mut self, mem: &GuestMemoryMmap) -> Result<Value, String> {
        if self.invalid || self.armed.is_some() {
            return Err("idle tracker invalid or already armed".into());
        }
        if self.idle.is_none() {
            return Err("page-idle not selected".into());
        }
        self.preflight(mem, true)?;
        let result = self.arm_inner(mem);
        if result.is_err() {
            self.invalid = true;
        }
        result
    }
    fn arm_inner(&mut self, mem: &GuestMemoryMmap) -> Result<Value, String> {
        let begin = mono();
        let started_cpu = cpu();
        let before = self.inventory(mem)?;
        let mut words = Vec::new();
        words
            .try_reserve_exact(before.resident_count() as usize)
            .map_err(|e| e.to_string())?;
        let mut eligible = 0u64;
        for s in &before.slots {
            for i in 0..s.pfns.len() {
                if let Some(p) = s.page(i) {
                    if eligibility(p) == 0 {
                        words.push((p.pfn / 64, 1u64 << (p.pfn % 64)));
                        eligible += 1;
                    }
                }
            }
        }
        words.sort_unstable_by_key(|v| v.0);
        let mut write = 0;
        for read in 0..words.len() {
            if write > 0 && words[write - 1].0 == words[read].0 {
                words[write - 1].1 |= words[read].1;
            } else {
                words[write] = words[read];
                write += 1;
            }
        }
        words.truncate(write);
        let write_begin = mono();
        write_idle(self.idle.as_mut().unwrap(), &words)?;
        let write_end = mono();
        let end = mono();
        let cpu_ns = cpu() - started_cpu;
        let peak_capacity = before.inventory_peak_capacity_bytes.max(
            before.estimated_bytes()
                + words.capacity() * size_of::<(u64, u64)>()
                + size_of::<ArmState>(),
        );
        let result = json!({"arm_begin_ns":begin,"arm_end_ns":end,"arm_cpu_ns":cpu_ns,
            "bitmap_write_begin_ns":write_begin,"bitmap_write_end_ns":write_end,
            "resident_pages_at_arm":before.resident_count(),"eligible_pages_at_arm":eligible,
            "clock":"host-CLOCK_MONOTONIC","page_bytes":PAGE,"component_peak_preflight_bytes":Self::capacity_preflight(mem)?,
            "collector_buffer_capacity_bytes":before.estimated_bytes()+words.capacity()*size_of::<(u64,u64)>()+size_of::<ArmState>(),
            "collector_operation_peak_capacity_bytes":peak_capacity});
        self.armed = Some(ArmState {
            before,
            words,
            begin,
            end,
            cpu_ns,
            write_begin,
            write_end,
        });
        Ok(result)
    }
    /// Collects arm/mid/final evidence once; any failure invalidates this tracker.
    pub fn collect(&mut self, mem: &GuestMemoryMmap) -> Result<AccessRecord, String> {
        if self.invalid {
            return Err("idle tracker invalid".into());
        }
        self.preflight(mem, true)?;
        let armed = self.armed.take().ok_or("idle tracker is not armed")?;
        let result = self.collect_inner(mem, armed);
        if result.is_err() {
            self.invalid = true;
        }
        result
    }
    fn collect_inner(
        &mut self,
        mem: &GuestMemoryMmap,
        mut a: ArmState,
    ) -> Result<AccessRecord, String> {
        let begin = mono();
        let started_cpu = cpu();
        let arm_live = a.before.estimated_bytes()
            + a.words.capacity() * size_of::<(u64, u64)>()
            + size_of::<ArmState>();
        let mid = self.inventory(mem)?;
        let first_peak = arm_live + mid.inventory_peak_capacity_bytes;
        if a.before.regions() != mid.regions() {
            return Err("RAM layout changed during idle epoch".into());
        }
        let read_begin = mono();
        read_idle_in_place(
            self.idle.as_mut().ok_or("page-idle not selected")?,
            &mut a.words,
        )?;
        let read_end = mono();
        let end_census = self.inventory(mem)?;
        let operation_peak = first_peak
            .max(arm_live + mid.estimated_bytes() + end_census.inventory_peak_capacity_bytes);
        if a.before.regions() != end_census.regions() {
            return Err("RAM layout changed during idle epoch".into());
        }
        let mut slots = Vec::new();
        let mut totals = [0u64; 11];
        for ((before, after), last) in a
            .before
            .slots
            .into_iter()
            .zip(mid.slots)
            .zip(&end_census.slots)
        {
            let n = before.pfns.len();
            let mut codes = Vec::new();
            codes.try_reserve_exact(n).map_err(|e| e.to_string())?;
            codes.resize(n, 0);
            let mut classified = zeros(n.div_ceil(64))?;
            for (i, code) in codes.iter_mut().enumerate() {
                let prior = before.page(i);
                let m = after.page(i);
                let final_page = last.page(i);
                let idle_bit = prior.and_then(|p| {
                    a.words
                        .binary_search_by_key(&(p.pfn / 64), |w| w.0)
                        .ok()
                        .map(|j| a.words[j].1 & (1 << (p.pfn % 64)) != 0)
                });
                *code = classify(prior, m, final_page, idle_bit)?;
                totals[*code as usize] += 1;
                if *code != ABSENT {
                    put(&mut classified, i);
                }
            }
            let rank = rank_index(&classified)?;
            slots.push(AccessSlot {
                region: before.region,
                codes,
                classified,
                rank,
                before_pfns: before.pfns,
                before_present: before.present,
                mid_pfns: after.pfns,
                mid_present: after.present,
            });
        }
        let end = mono();
        let unknown: u64 = totals[3..].iter().sum();
        let unknown_counts: serde_json::Map<String, Value> = (3..=10)
            .filter(|i| totals[*i] != 0)
            .map(|i| (reason(i as u8).unwrap().into(), json!(totals[i])))
            .collect();
        let metadata = json!({"schema":"aries.host-page-idle.v1","clock":"host-CLOCK_MONOTONIC","page_bytes":PAGE,
            "arm_begin_ns":a.begin,"arm_end_ns":a.end,"scan_begin_ns":begin,"scan_end_ns":end,
            "bitmap_write_begin_ns":a.write_begin,"bitmap_write_end_ns":a.write_end,"bitmap_read_begin_ns":read_begin,"bitmap_read_end_ns":read_end,
            "arm_cpu_ns":a.cpu_ns,"scan_cpu_ns":cpu()-started_cpu,"observed_pages":totals[OBSERVED as usize],
            "idle_pages":totals[IDLE as usize],"unknown_pages":unknown,"unknown_counts":unknown_counts,
            "collector_operation_peak_capacity_bytes":operation_peak,
            "buffer_accounting_scope":"simultaneous raw Vec capacities, excludes allocator and caller export/metadata buffers",
            "coverage_scope":"stable exclusive resident host-LRU base pages in fixed VMM RAM slots",
            "access_kind":"read-or-write combined; direction not observable",
            "limitations":["No secondary-TLB flush; unchanged idle bits may miss accesses","Endpoint PFNs miss transient residency/A-B-A changes","Guest and host/VMM activity included; observer reads require explicit attribution","Unknown counted separately, never silently zero"]});
        Ok(AccessRecord {
            slots,
            metadata,
            end_census: Arc::new(end_census),
        })
    }
}
fn smaps() -> (Option<u64>, Option<u64>, Option<String>) {
    let result = (|| -> Result<(Option<u64>, Option<u64>), String> {
        let f = File::open("/proc/self/smaps_rollup").map_err(|e| e.to_string())?;
        let mut text = String::new();
        f.take(65537)
            .read_to_string(&mut text)
            .map_err(|e| e.to_string())?;
        if text.len() > 65536 {
            return Err("smaps_rollup exceeds bounded reader".into());
        }
        let (mut rss, mut pss) = (None, None);
        for line in text.lines() {
            let p: Vec<_> = line.split_whitespace().collect();
            if p.len() == 3 && p[2] == "kB" {
                if p[0] == "Rss:" {
                    rss = p[1].parse().ok();
                } else if p[0] == "Pss:" {
                    pss = p[1].parse().ok();
                }
            }
        }
        if rss.is_none() || pss.is_none() {
            return Err("RSS/PSS unavailable in smaps_rollup".into());
        }
        Ok((rss, pss))
    })();
    match result {
        Ok((r, p)) => (r, p, None),
        Err(e) => (None, None, Some(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    fn p(pfn: u64, flags: u64, count: u64) -> PageMeta {
        PageMeta { pfn, flags, count }
    }
    #[test]
    fn reference_classification_precedence_and_unknowns() {
        let good = p(100, LRU, 1);
        assert_eq!(classify(None, None, None, None).unwrap(), ABSENT);
        assert_eq!(classify(None, Some(good), None, None).unwrap(), NEW);
        assert_eq!(
            classify(Some(good), None, Some(good), None).unwrap(),
            DISAPPEARED
        );
        assert_eq!(
            classify(Some(good), Some(p(101, 0, 0)), Some(good), None).unwrap(),
            CHANGED
        );
        for (page, expected) in [
            (p(0, COMPOUND, 2), MASKED),
            (p(100, COMPOUND | SHARED, 2), COMPOUND_UNKNOWN),
            (p(100, SHARED, 2), SHARED_UNKNOWN),
            (p(100, 0, 2), NON_LRU),
            (p(100, LRU, 2), NONEXCLUSIVE),
        ] {
            assert_eq!(
                classify(Some(page), Some(page), Some(page), None).unwrap(),
                expected
            );
        }
        assert_eq!(
            classify(Some(good), Some(good), Some(good), Some(true)).unwrap(),
            IDLE
        );
        assert_eq!(
            classify(Some(good), Some(good), Some(good), Some(false)).unwrap(),
            OBSERVED
        );
        assert!(classify(Some(good), Some(good), Some(good), None).is_err());
        assert_eq!(
            classify(
                Some(p(100, COMPOUND, 1)),
                Some(p(100, SHARED, 1)),
                Some(good),
                None
            )
            .unwrap(),
            COMPOUND_UNKNOWN
        );
    }
    #[derive(Default)]
    struct CountIo {
        inner: Cursor<Vec<u8>>,
        writes: Vec<usize>,
        reads: Vec<usize>,
        short: bool,
    }
    impl Seek for CountIo {
        fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(p)
        }
    }
    impl Write for CountIo {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.writes.push(b.len());
            if self.short {
                Ok(b.len().saturating_sub(1))
            } else {
                self.inner.write(b)
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Read for CountIo {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            self.reads.push(b.len());
            self.inner.read(b)
        }
    }
    #[test]
    fn kernfs_idle_io_never_exceeds4096_and_short_is_error() {
        let words: Vec<_> = (0..513).map(|i| (i, 1)).collect();
        let mut io = CountIo::default();
        write_idle(&mut io, &words).unwrap();
        assert_eq!(io.writes, vec![4096, 8]);
        io.seek(SeekFrom::Start(0)).unwrap();
        let mut read = words.clone();
        read_idle_in_place(&mut io, &mut read).unwrap();
        assert_eq!(io.reads, vec![4096, 8]);
        io.short = true;
        assert!(write_idle(&mut io, &words).is_err());
        assert!(read_words(&mut Cursor::new(vec![0u8; 7]), &[0], PROC_WORDS).is_err());
    }
    #[test]
    fn idle_words_skip_holes_while_read_only_metadata_coalesces_small_gaps() {
        let mut io = CountIo::default();
        write_idle(&mut io, &[(0, 1), (2, 4)]).unwrap();
        assert_eq!(io.writes, vec![8, 8]);
        assert_eq!(
            read_words(&mut io, &[0, 2], PROC_WORDS).unwrap(),
            vec![1, 4]
        );
        assert_eq!(io.reads, vec![24]);
        let mut words = [(0, 0), (2, 0)];
        read_idle_in_place(&mut io, &mut words).unwrap();
        assert_eq!(words, [(0, 1), (2, 4)]);
        assert_eq!(io.reads, vec![24, 8, 8]);
        assert!(read_words(&mut io, &[2, 1], PROC_WORDS).is_err());
        assert!(read_words(&mut io, &[1, 1], PROC_WORDS).is_err());
    }
    #[test]
    fn coalesced_metadata_preserves_values_limits_large_holes_and_eof_errors() {
        let values: Vec<u64> = (0..20000).map(|i| i * 257 + 11).collect();
        let bytes: Vec<u8> = values.iter().flat_map(|word| word.to_ne_bytes()).collect();
        let mut io = CountIo {
            inner: Cursor::new(bytes),
            ..Default::default()
        };
        let selected: Vec<u64> = (0..8192).map(|i| i * 2).collect();
        assert_eq!(
            read_words(&mut io, &selected, PROC_WORDS).unwrap(),
            selected
                .iter()
                .map(|&i| values[i as usize])
                .collect::<Vec<_>>()
        );
        assert_eq!(io.reads, vec![65528, 65528]);
        io.reads.clear();
        assert_eq!(
            read_words(&mut io, &[0, 4, 9], PROC_WORDS).unwrap(),
            vec![values[0], values[4], values[9]]
        );
        assert_eq!(io.reads, vec![40, 8]);
        io.reads.clear();
        read_words(&mut io, &[0, 8193], PROC_WORDS).unwrap();
        assert_eq!(io.reads, vec![8, 8]);
        assert!(read_words(&mut Cursor::new(vec![0u8; 24 - 1]), &[0, 2], PROC_WORDS).is_err());
    }
    #[test]
    #[ignore = "local regular-file metadata I/O benchmark; no KVM or procfs writes"]
    fn benchmark_sparse_metadata_coalescing() {
        use std::time::Instant;
        use vmm_sys_util::tempfile::TempFile;
        struct CountFile {
            file: File,
            reads: usize,
            seeks: usize,
            bytes: usize,
        }
        impl Seek for CountFile {
            fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
                self.seeks += 1;
                self.file.seek(pos)
            }
        }
        impl Read for CountFile {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                let count = self.file.read(out)?;
                self.bytes += count;
                Ok(count)
            }
        }
        fn legacy(io: &mut CountFile, indices: &[u64]) -> Vec<u64> {
            let mut result = vec![0; indices.len()];
            let mut first = 0;
            let mut bytes = [0u8; PROC_WORDS * 8];
            while first < indices.len() {
                let mut end = first + 1;
                while end < indices.len()
                    && end - first < PROC_WORDS
                    && indices[end - 1] + 1 == indices[end]
                {
                    end += 1;
                }
                io_read(io, indices[first], &mut bytes[..(end - first) * 8]).unwrap();
                for (j, chunk) in bytes[..(end - first) * 8].chunks_exact(8).enumerate() {
                    result[first + j] = u64::from_ne_bytes(chunk.try_into().unwrap());
                }
                first = end;
            }
            result
        }
        for (stride, pages) in [(1u64, 65536usize), (2, 65536), (4, 65536), (5, 8192)] {
            let temp = TempFile::new().unwrap();
            let indices: Vec<_> = (0..pages).map(|i| 100000 + (i as u64) * stride).collect();
            let values: Vec<u8> = (0..=*indices.last().unwrap())
                .flat_map(|i| (i ^ 0x5a5a5a5a5a5a5a5a).to_ne_bytes())
                .collect();
            let mut file = temp.as_file().try_clone().unwrap();
            file.write_all(&values).unwrap();
            let mut old = CountFile {
                file: file.try_clone().unwrap(),
                reads: 0,
                seeks: 0,
                bytes: 0,
            };
            let start = Instant::now();
            let expected = legacy(&mut old, &indices);
            let baseline_ns = start.elapsed().as_nanos();
            let mut new = CountFile {
                file,
                reads: 0,
                seeks: 0,
                bytes: 0,
            };
            let start = Instant::now();
            let actual = read_words(&mut new, &indices, PROC_WORDS).unwrap();
            let indexed_ns = start.elapsed().as_nanos();
            assert_eq!(actual, expected);
            assert_eq!(
                actual,
                indices
                    .iter()
                    .map(|i| i ^ 0x5a5a5a5a5a5a5a5a)
                    .collect::<Vec<_>>()
            );
            println!(
                "{}",
                json!({"benchmark":"metadata_gap_coalescing","selected_pfns":pages,"stride":stride,
                "max_read_bytes":PROC_WORDS*8,"max_pfn_step":PROC_MAX_WORD_GAP,
                "baseline":{"wall_ns":baseline_ns,"read_calls":old.reads,"lseek_calls":old.seeks,"bytes":old.bytes},
                "coalesced":{"wall_ns":indexed_ns,"read_calls":new.reads,"lseek_calls":new.seeks,"bytes":new.bytes},
                "all_selected_words_equal":true,"scope":"compiled Rust regular-file fixture; not procfs latency"})
            );
        }
    }
    #[test]
    fn ordinal_chunks_skip_presence_words_without_allocating_rows() {
        let mut offset = 3;
        assert_eq!(
            selected_pages(&[0b1011, 1 << 5], &[0, 3, 4], &mut offset, 1),
            vec![69]
        );
        let mut offset = 9;
        assert!(selected_pages(&[0b1011, 1 << 5], &[0, 3, 4], &mut offset, 2).is_empty());
        assert!(check_chunk(0).is_err());
        assert!(check_chunk(257).is_err());
    }
    #[test]
    fn absent_range_index_handles_word_boundary_and_fragmentation() {
        let words = [u64::MAX ^ (1 << 63), u64::MAX ^ 3];
        let starts = absent_starts(&words, 128).unwrap();
        assert_eq!(starts, vec![1 << 63, 0]);
        assert_eq!(next_present(&words, 63, 128), 66);
        let alternating = [0x5555555555555555];
        let starts = absent_starts(&alternating, 64).unwrap();
        assert_eq!(starts[0].count_ones(), 32);
        assert_eq!(rank_index(&starts).unwrap(), vec![0, 32]);
    }
    #[test]
    fn census_or_capture_only_does_not_open_idle_or_kpage_files() {
        let s = HostPages::new(false).unwrap();
        assert!(s.idle.is_none());
        assert!(s.flags.is_none());
        assert!(s.counts.is_none());
    }
    #[test]
    fn matches_24_unchanged_external_reference_vectors() {
        // Generated by byte-exact Python IdleObserver.collect, with synthetic
        // inventories/flags/counts/idle words only; no live kernel I/O.
        let data: Value = serde_json::from_str(REFERENCE_VECTORS_JSON).unwrap();
        assert_eq!(
            data["reference_sha256"],
            "568100daa47324e5a84b7cfed961607ac8ac6ddeb023bf0acc379fe5e7266dd9"
        );
        let vectors = data["vectors"].as_array().unwrap();
        assert_eq!(vectors.len(), 24);
        let page = |v: &Value| {
            if v.is_null() {
                None
            } else {
                Some(PageMeta {
                    pfn: v["pfn"].as_u64().unwrap(),
                    flags: v["flags"].as_u64().unwrap(),
                    count: v["count"].as_u64().unwrap(),
                })
            }
        };
        for v in vectors {
            let code = classify(
                page(&v["before"]),
                page(&v["mid"]),
                page(&v["end"]),
                v["idle_bit"].as_bool(),
            )
            .unwrap();
            let state = if code == ABSENT {
                "absent"
            } else if code == OBSERVED {
                "observed"
            } else if code == IDLE {
                "idle"
            } else {
                "unknown"
            };
            assert_eq!(
                state,
                v["expected_state"].as_str().unwrap(),
                "{}",
                v["name"]
            );
            assert_eq!(
                reason(code),
                v["expected_unknown_reason"].as_str(),
                "{}",
                v["name"]
            );
        }
    }

    fn compact_inventory_fixture() -> Inventory {
        let make = |slot, base, present: Vec<u64>, pfns: Vec<u64>| {
            let starts = absent_starts(&present, pfns.len()).unwrap();
            SlotCensus {
                region: Region {
                    slot,
                    guest_base: base,
                    bytes: 4 * PAGE,
                    page_count: 4,
                },
                rank: rank_index(&present).unwrap(),
                absent_rank: rank_index(&starts).unwrap(),
                absent_starts: starts,
                present,
                pfns,
                flags: vec![LRU; 4],
                counts: vec![1; 4],
            }
        };
        Inventory {
            slots: vec![
                make(0, 0, vec![0b1011], vec![10, 10, 0, 0]),
                make(1, 1 << 32, vec![0b0010], vec![0, 20, 0, 0]),
            ],
            resident_population: Population {
                resident_gpa_pages: 4,
                unique_nonzero_host_pfns: 2,
                zero_pfn_pages: 1,
                vmm_rss_kib: None,
                vmm_pss_kib: None,
            },
            inventory_begin_ns: 1,
            inventory_end_ns: 2,
            vmm_memory_error: Some("fixture unavailable".into()),
            inventory_peak_capacity_bytes: 1,
        }
    }
    #[test]
    fn compact_census_chunks_preserve_aliases_masked_pfns_and_holes() {
        let inv = compact_inventory_fixture();
        let rows = inv.resident_rows(2, 2).unwrap();
        assert_eq!(
            rows.iter().map(|r| r.gpa).collect::<Vec<_>>(),
            vec![3 * PAGE, (1 << 32) + PAGE]
        );
        assert_eq!(rows[0].pfn, 0);
        assert_eq!(rows[0].host_kpageflags, None);
        assert_eq!(inv.resident_count(), 4);
        assert_eq!(inv.resident_population.unique_nonzero_host_pfns, 2);
        assert_eq!(inv.absent_range_count(), 3);
        assert_eq!(
            inv.absent_ranges(1, 2).unwrap(),
            vec![
                [1 << 32, (1 << 32) + PAGE],
                [(1 << 32) + 2 * PAGE, (1 << 32) + 4 * PAGE]
            ]
        );
        assert_eq!(
            inv.resident_gpas_from(1 << 32, (1 << 32) + 4 * PAGE, 1)
                .unwrap(),
            vec![(1 << 32) + PAGE]
        );
        assert!(inv.metadata_value().get("absent_gpa_ranges").is_none());
        assert!(inv.resident_rows(0, 257).is_err());
        assert!(inv.absent_ranges(0, 0).is_err());
    }
    #[test]
    fn compact_access_chunks_keep_presence_and_unknown_reasons() {
        let end = Arc::new(compact_inventory_fixture());
        let slots = vec![
            AccessSlot {
                region: end.slots[0].region.clone(),
                codes: vec![OBSERVED, IDLE, ABSENT, MASKED],
                classified: vec![0b1011],
                rank: vec![0, 3],
                before_pfns: vec![10, 10, 0, 0],
                before_present: vec![0b1011],
                mid_pfns: vec![10, 10, 0, 0],
                mid_present: vec![0b1011],
            },
            AccessSlot {
                region: end.slots[1].region.clone(),
                codes: vec![ABSENT, NEW, ABSENT, ABSENT],
                classified: vec![0b0010],
                rank: vec![0, 1],
                before_pfns: vec![0; 4],
                before_present: vec![0],
                mid_pfns: vec![0, 20, 0, 0],
                mid_present: vec![0b0010],
            },
        ];
        let record = AccessRecord {
            slots,
            metadata: json!({}),
            end_census: end,
        };
        let rows = record.rows(2, 2).unwrap();
        assert_eq!(record.row_count(), 4);
        assert_eq!(rows[0].before_pfn, Some(0));
        assert_eq!(
            rows[0].unknown_reason.as_deref(),
            Some("pfn-masked-or-zero")
        );
        assert_eq!(rows[1].before_pfn, None);
        assert_eq!(rows[1].final_pfn, Some(20));
        assert!(record.rows(100, 2).unwrap().is_empty());
        assert!(record.rows(0, 257).is_err());
    }
    const REFERENCE_VECTORS_JSON: &str = r####"{"kernel_io_performed":false,"reference_sha256":"568100daa47324e5a84b7cfed961607ac8ac6ddeb023bf0acc379fe5e7266dd9","reference_source":"external_idle_reference.py","schema":"aries.host-page-idle.external-reference-vectors.v1","scope":"same recorded/synthetic kernel inputs only; no TLB/universal kernel-truth claim","vectors":[{"before":null,"end":null,"expected_state":"absent","expected_unknown_reason":null,"idle_bit":null,"mid":null,"name":"all_absent"},{"before":{"count":1,"flags":32,"pfn":100},"end":{"count":1,"flags":32,"pfn":100},"expected_state":"observed","expected_unknown_reason":null,"idle_bit":false,"mid":{"count":1,"flags":32,"pfn":100},"name":"observed"},{"before":{"count":1,"flags":32,"pfn":100},"end":{"count":1,"flags":32,"pfn":100},"expected_state":"idle","expected_unknown_reason":null,"idle_bit":true,"mid":{"count":1,"flags":32,"pfn":100},"name":"idle"},{"before":null,"end":null,"expected_state":"unknown","expected_unknown_reason":"newly-resident-unarmed","idle_bit":null,"mid":{"count":1,"flags":32,"pfn":100},"name":"new_at_middle"},{"before":null,"end":{"count":1,"flags":32,"pfn":100},"expected_state":"unknown","expected_unknown_reason":"newly-resident-unarmed","idle_bit":null,"mid":null,"name":"new_at_end"},{"before":{"count":1,"flags":32,"pfn":100},"end":{"count":1,"flags":32,"pfn":100},"expected_state":"unknown","expected_unknown_reason":"became-nonresident","idle_bit":null,"mid":null,"name":"disappeared_mid"},{"before":{"count":1,"flags":32,"pfn":100},"end":null,"expected_state":"unknown","expected_unknown_reason":"became-nonresident","idle_bit":null,"mid":{"count":1,"flags":32,"pfn":100},"name":"disappeared_end"},{"before":{"count":1,"flags":32,"pfn":100},"end":{"count":1,"flags":32,"pfn":100},"expected_state":"unknown","expected_unknown_reason":"host-mapping-changed","idle_bit":null,"mid":{"count":1,"flags":32,"pfn":101},"name":"pfn_changed_mid"},{"before":{"count":1,"flags":32,"pfn":100},"end":{"count":1,"flags":32,"pfn":101},"expected_state":"unknown","expected_unknown_reason":"host-mapping-changed","idle_bit":null,"mid":{"count":1,"flags":32,"pfn":100},"name":"pfn_changed_end"},{"before":{"count":1,"flags":32,"pfn":0},"end":{"count":1,"flags":32,"pfn":0},"expected_state":"unknown","expected_unknown_reason":"pfn-masked-or-zero","idle_bit":null,"mid":{"count":1,"flags":32,"pfn":0},"name":"masked_or_zero"},{"before":{"count":1,"flags":32800,"pfn":100},"end":{"count":1,"flags":32800,"pfn":100},"expected_state":"unknown","expected_unknown_reason":"compound-resolution-unsupported","idle_bit":null,"mid":{"count":1,"flags":32800,"pfn":100},"name":"compound_head"},{"before":{"count":1,"flags":65568,"pfn":100},"end":{"count":1,"flags":65568,"pfn":100},"expected_state":"unknown","expected_unknown_reason":"compound-resolution-unsupported","idle_bit":null,"mid":{"count":1,"flags":65568,"pfn":100},"name":"compound_tail"},{"before":{"count":1,"flags":131104,"pfn":100},"end":{"count":1,"flags":131104,"pfn":100},"expected_state":"unknown","expected_unknown_reason":"compound-resolution-unsupported","idle_bit":null,"mid":{"count":1,"flags":131104,"pfn":100},"name":"compound_huge"},{"before":{"count":1,"flags":4194336,"pfn":100},"end":{"count":1,"flags":4194336,"pfn":100},"expected_state":"unknown","expected_unknown_reason":"compound-resolution-unsupported","idle_bit":null,"mid":{"count":1,"flags":4194336,"pfn":100},"name":"compound_thp"},{"before":{"count":1,"flags":2097184,"pfn":100},"end":{"count":1,"flags":2097184,"pfn":100},"expected_state":"unknown","expected_unknown_reason":"shared-or-zero-page","idle_bit":null,"mid":{"count":1,"flags":2097184,"pfn":100},"name":"shared_ksm"},{"before":{"count":1,"flags":16777248,"pfn":100},"end":{"count":1,"flags":16777248,"pfn":100},"expected_state":"unknown","expected_unknown_reason":"shared-or-zero-page","idle_bit":null,"mid":{"count":1,"flags":16777248,"pfn":100},"name":"shared_zero"},{"before":{"count":1,"flags":0,"pfn":100},"end":{"count":1,"flags":0,"pfn":100},"expected_state":"unknown","expected_unknown_reason":"not-on-lru","idle_bit":null,"mid":{"count":1,"flags":0,"pfn":100},"name":"non_lru"},{"before":{"count":2,"flags":32,"pfn":100},"end":{"count":2,"flags":32,"pfn":100},"expected_state":"unknown","expected_unknown_reason":"nonexclusive-or-transition","idle_bit":null,"mid":{"count":2,"flags":32,"pfn":100},"name":"nonexclusive"},{"before":{"count":0,"flags":32,"pfn":100},"end":{"count":0,"flags":32,"pfn":100},"expected_state":"unknown","expected_unknown_reason":"nonexclusive-or-transition","idle_bit":null,"mid":{"count":0,"flags":32,"pfn":100},"name":"count_zero_transition"},{"before":{"count":1,"flags":32800,"pfn":100},"end":{"count":1,"flags":32,"pfn":100},"expected_state":"unknown","expected_unknown_reason":"compound-resolution-unsupported","idle_bit":null,"mid":{"count":1,"flags":2097184,"pfn":100},"name":"arm_reason_precedes_later"},{"before":{"count":1,"flags":32,"pfn":100},"end":{"count":1,"flags":0,"pfn":100},"expected_state":"unknown","expected_unknown_reason":"shared-or-zero-page","idle_bit":null,"mid":{"count":1,"flags":2097184,"pfn":100},"name":"middle_reason_precedes_end"},{"before":{"count":1,"flags":32,"pfn":100},"end":{"count":1,"flags":0,"pfn":100},"expected_state":"unknown","expected_unknown_reason":"not-on-lru","idle_bit":null,"mid":{"count":1,"flags":32,"pfn":100},"name":"final_reason_used"},{"before":{"count":1,"flags":32,"pfn":0},"end":{"count":1,"flags":32,"pfn":100},"expected_state":"unknown","expected_unknown_reason":"host-mapping-changed","idle_bit":null,"mid":{"count":1,"flags":32,"pfn":100},"name":"changed_precedes_masked"},{"before":{"count":1,"flags":0,"pfn":100},"end":{"count":1,"flags":0,"pfn":100},"expected_state":"unknown","expected_unknown_reason":"became-nonresident","idle_bit":null,"mid":null,"name":"disappearance_precedes_unknown"}]}"####;
}
