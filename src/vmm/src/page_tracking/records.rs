// SPDX-License-Identifier: Apache-2.0
//! Compact immutable records; JSON rows are materialized only for bounded exports.
use super::capture::CaptureSnapshot;
use super::host_pages::{AccessRecord, Inventory};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub const MAX_RECORDS: usize = 8;
pub const MAX_RETAINED_BYTES: usize = 1024 * 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
pub struct DirtyPage {
    pub gpa: u64,
    pub kvm_dirty: bool,
    pub vmm_dirty: bool,
}
#[derive(Debug)]
pub struct DirtyRegion {
    pub guest_base: u64,
    pub page_count: u64,
    cpu: Vec<u8>,
    vmm: Vec<u8>,
}
#[derive(Debug, Default)]
pub struct DirtyData {
    pub regions: Vec<DirtyRegion>,
    pub positive_count: u64,
    pub metadata: Value,
}
impl DirtyData {
    pub fn from_reply(value: &Value) -> Result<Self, String> {
        let mut out = Self::default();
        let mut region_meta = Vec::new();
        for region in value["regions"].as_array().ok_or("missing dirty regions")? {
            let base = region["guest_base"].as_u64().ok_or("invalid dirty base")?;
            let pages = region["page_count"].as_u64().ok_or("invalid dirty size")?;
            let cpu = STANDARD
                .decode(
                    region["kvm_dirty_b64"]
                        .as_str()
                        .ok_or("missing CPU bitmap")?,
                )
                .map_err(|e| e.to_string())?;
            let vmm = STANDARD
                .decode(
                    region["vmm_dirty_b64"]
                        .as_str()
                        .ok_or("missing VMM bitmap")?,
                )
                .map_err(|e| e.to_string())?;
            if cpu.len() != vmm.len() || cpu.len() as u64 != pages.div_ceil(64) * 8 {
                return Err("dirty bitmap length mismatch".into());
            }
            let positive: u64 = cpu
                .iter()
                .zip(&vmm)
                .map(|(a, b)| (a | b).count_ones() as u64)
                .sum();
            out.positive_count = out
                .positive_count
                .checked_add(positive)
                .ok_or("dirty count overflow")?;
            region_meta.push(json!({"slot":region["slot"],"guest_base":base,"bytes":region["bytes"],"page_count":pages,"kvm_dirty_pages":region["kvm_dirty_pages"],"vmm_dirty_pages":region["vmm_dirty_pages"],"combined_dirty_pages":positive}));
            out.regions.push(DirtyRegion {
                guest_base: base,
                page_count: pages,
                cpu,
                vmm,
            });
        }
        out.metadata = json!({"regions":region_meta,"begin_monotonic_us":value["begin_monotonic_us"],"end_monotonic_us":value["end_monotonic_us"],"vcpu_paused":value["vcpu_paused"],"device_quiescence":value["device_quiescence"],"vmm_semantics":value["vmm_semantics"],"boot_baseline_discarded":value["boot_baseline_discarded"]});
        Ok(out)
    }
    pub fn estimated_bytes(&self) -> usize {
        self.regions
            .iter()
            .map(|r| r.cpu.capacity() + r.vmm.capacity() + 64)
            .sum()
    }
    pub fn contains(&self, gpa: u64) -> bool {
        self.regions.iter().any(|r| {
            let Some(offset) = gpa.checked_sub(r.guest_base) else {
                return false;
            };
            let page = offset / 4096;
            page < r.page_count
                && (r.cpu[page as usize / 8] | r.vmm[page as usize / 8]) & (1 << (page % 8)) != 0
        })
    }
    pub fn rows(&self, offset: u64, limit: usize) -> Vec<Value> {
        let mut skip = offset;
        let mut rows = Vec::new();
        for r in &self.regions {
            for (byte, (&cpu, &vmm)) in r.cpu.iter().zip(&r.vmm).enumerate() {
                let mut bits = cpu | vmm;
                let count = bits.count_ones() as u64;
                if skip >= count {
                    skip -= count;
                    continue;
                }
                while bits != 0 {
                    let bit = bits.trailing_zeros();
                    bits &= bits - 1;
                    let page = byte as u64 * 8 + u64::from(bit);
                    if page >= r.page_count {
                        break;
                    }
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    rows.push(json!({"gpa":r.guest_base+page*4096,"kvm_dirty":cpu&(1<<bit)!=0,"vmm_dirty":vmm&(1<<bit)!=0}));
                    if rows.len() == limit {
                        return rows;
                    }
                }
            }
        }
        rows
    }
}

#[derive(Debug)]
pub enum Payload {
    Census(Arc<Inventory>),
    Epoch {
        access: Option<Arc<AccessRecord>>,
        census: Option<Arc<Inventory>>,
        dirty: Arc<DirtyData>,
    },
    Capture {
        snapshot: Arc<CaptureSnapshot>,
        overlap: Arc<Vec<u64>>,
    },
}
#[derive(Debug)]
pub struct Record {
    pub metadata: Value,
    pub payload: Payload,
    metadata_bytes: usize,
}
impl Record {
    pub fn new(metadata: Value, payload: Payload) -> Self {
        let metadata_bytes = metadata.to_string().len();
        Self {
            metadata,
            payload,
            metadata_bytes,
        }
    }
    fn retained(&self, seen: &mut BTreeSet<usize>) -> usize {
        let mut bytes = self.metadata_bytes;
        match &self.payload {
            Payload::Census(c) => {
                if seen.insert(Arc::as_ptr(c) as usize) {
                    bytes += c.estimated_bytes()
                }
            }
            Payload::Epoch {
                access,
                census,
                dirty,
            } => {
                if let Some(a) = access {
                    if seen.insert(Arc::as_ptr(a) as usize) {
                        bytes += a
                            .estimated_bytes()
                            .saturating_sub(a.end_census.estimated_bytes());
                        if seen.insert(Arc::as_ptr(&a.end_census) as usize) {
                            bytes += a.end_census.estimated_bytes()
                        }
                    }
                }
                if let Some(c) = census {
                    if seen.insert(Arc::as_ptr(c) as usize) {
                        bytes += c.estimated_bytes()
                    }
                }
                if seen.insert(Arc::as_ptr(dirty) as usize) {
                    bytes += dirty.estimated_bytes()
                }
            }
            Payload::Capture { snapshot, overlap } => {
                if seen.insert(Arc::as_ptr(snapshot) as usize) {
                    bytes += snapshot.estimated_bytes()
                }
                if seen.insert(Arc::as_ptr(overlap) as usize) {
                    bytes += overlap.capacity() * 8
                }
            }
        }
        bytes
    }
    pub fn census(&self) -> Option<&Arc<Inventory>> {
        match &self.payload {
            Payload::Census(c) => Some(c),
            Payload::Epoch {
                census: Some(c), ..
            } => Some(c),
            Payload::Epoch {
                access: Some(a), ..
            } => Some(&a.end_census),
            _ => None,
        }
    }
    pub fn export(&self, id: &str, kind: &str, offset: u64, limit: usize) -> Result<Value, String> {
        if limit == 0 || limit > 4096 || (kind == "pages" && limit > 256) {
            return Err("invalid bounded export limit".into());
        }
        let mut rows = Vec::new();
        let total = match kind {
            "resident" => {
                let c = self.census().ok_or("record has no residency census")?;
                let total = c.resident_count();
                let mut pos = offset;
                while rows.len() < limit && pos < total {
                    let batch = c.resident_rows(pos, (limit - rows.len()).min(256))?;
                    if batch.is_empty() {
                        return Err("census export made no progress".into());
                    }
                    pos += batch.len() as u64;
                    for row in batch {
                        rows.push(serde_json::to_value(row).map_err(|e| e.to_string())?)
                    }
                }
                total
            }
            "absent_ranges" => {
                let c = self.census().ok_or("record has no residency census")?;
                let total = c.absent_range_count();
                let mut pos = offset;
                while rows.len() < limit && pos < total {
                    let batch = c.absent_ranges(pos, (limit - rows.len()).min(256))?;
                    if batch.is_empty() {
                        return Err("absence export made no progress".into());
                    }
                    pos += batch.len() as u64;
                    for row in batch {
                        rows.push(json!(row))
                    }
                }
                total
            }
            "access" => {
                let Payload::Epoch {
                    access: Some(a), ..
                } = &self.payload
                else {
                    return Err("record has no access data".into());
                };
                let total = a.row_count();
                let mut pos = offset;
                while rows.len() < limit && pos < total {
                    let batch = a.rows(pos, (limit - rows.len()).min(256))?;
                    if batch.is_empty() {
                        return Err("access export made no progress".into());
                    }
                    pos += batch.len() as u64;
                    for row in batch {
                        rows.push(serde_json::to_value(row).map_err(|e| e.to_string())?)
                    }
                }
                total
            }
            "dirty" => {
                let Payload::Epoch { dirty, .. } = &self.payload else {
                    return Err("record has no dirty data".into());
                };
                rows = dirty.rows(offset, limit);
                dirty.positive_count
            }
            "pages" | "read_mask" | "scan_log" | "dirty_overlap" => {
                let Payload::Capture { snapshot, overlap } = &self.payload else {
                    return Err("record has no capture data".into());
                };
                if kind == "dirty_overlap" {
                    for gpa in overlap
                        .iter()
                        .skip(usize::try_from(offset).map_err(|_| "offset overflow")?)
                        .take(limit)
                    {
                        rows.push(json!({"gpa":gpa}))
                    }
                    overlap.len() as u64
                } else {
                    let (total, part) = snapshot.export(kind, offset, limit)?;
                    rows = part;
                    total
                }
            }
            _ => return Err("unknown export dataset".into()),
        };
        if offset > total {
            return Err("export offset exceeds dataset".into());
        }
        let next = offset + rows.len() as u64;
        Ok(
            json!({"record_id":id,"kind":kind,"offset":offset,"next_offset":next,"total_rows":total,"complete":next==total,"rows":rows,"metadata":self.metadata}),
        )
    }
}

#[derive(Debug, Default)]
pub struct Records {
    pub records: BTreeMap<String, Record>,
    pub generation: String,
    next: u64,
}
impl Records {
    pub fn retained_bytes(&self) -> usize {
        let mut seen = BTreeSet::new();
        self.records.values().map(|r| r.retained(&mut seen)).sum()
    }
    pub fn preflight(
        &self,
        new_records: usize,
        extra_bytes: usize,
        reservation: usize,
    ) -> Result<(), String> {
        if self
            .records
            .len()
            .checked_add(new_records)
            .is_none_or(|n| n > MAX_RECORDS)
            || self
                .retained_bytes()
                .checked_add(extra_bytes)
                .and_then(|n| n.checked_add(reservation))
                .is_none_or(|n| n > MAX_RETAINED_BYTES)
        {
            return Err("immutable export backpressure: acknowledge retained records or reduce capture budget before retry".into());
        }
        Ok(())
    }
    pub fn insert(&mut self, record: Record) -> Result<String, String> {
        let mut seen = BTreeSet::new();
        let estimate = record.retained(&mut seen);
        // Callers preflight shared payloads before destructive operations. Recheck
        // actual retained storage after insertion, deduplicating Arc-owned copies.
        if self.records.len() >= MAX_RECORDS {
            return Err("immutable record count limit".into());
        }
        self.next = self
            .next
            .checked_add(1)
            .ok_or("record identifier overflow")?;
        let id = format!("{}-record-{}", self.generation, self.next);
        self.records.insert(id.clone(), record);
        if self.retained_bytes() > MAX_RETAINED_BYTES {
            self.records.remove(&id);
            return Err(format!(
                "immutable record memory cap exceeded ({estimate} candidate bytes)"
            ));
        }
        Ok(id)
    }
}
