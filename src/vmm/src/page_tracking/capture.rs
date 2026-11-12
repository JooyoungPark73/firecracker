// SPDX-License-Identifier: Apache-2.0
//! Bounded, instrumented guest-physical metadata copies. No guest writes.
use super::api::Consistency;
use super::host_pages::{HostPages, Inventory};
use crate::vstate::memory::{Bytes, GuestAddress, GuestMemoryMmap};
use aws_lc_rs::digest::{Context, SHA256, digest};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use vm_memory::GuestMemoryBackend;

pub const PAGE: usize = 4096;
pub fn hash(bytes: &[u8]) -> String {
    digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn context_hash(ctx: Context) -> String {
    ctx.finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub fn clock_ns() -> u64 {
    utils::time::get_time_us(utils::time::ClockType::Monotonic).saturating_mul(1000)
}
pub fn valid_gpa(mem: &GuestMemoryMmap, gpa: u64) -> bool {
    gpa % PAGE as u64 == 0 && mem.get_slice(GuestAddress(gpa), PAGE).is_ok()
}

#[derive(Debug)]
pub struct PageCopy {
    pub gpa: u64,
    pub bytes: Vec<u8>,
    pub sha256: String,
}
impl PageCopy {
    fn value(&self) -> Value {
        json!({"gpa":self.gpa,"data_b64":STANDARD.encode(&self.bytes),"sha256":self.sha256})
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct ReadMask {
    pub gpa: u64,
    pub metadata_read: bool,
    pub scan_read: bool,
    pub read_count: u64,
    pub before_present: bool,
    pub before_pfn: Option<u64>,
    pub after_present: Option<bool>,
    pub after_pfn: Option<u64>,
}
#[derive(Debug, Clone, Serialize)]
pub struct Match {
    pub gpa: u64,
    pub offset: u64,
}
#[derive(Debug, Serialize)]
pub struct ScanLog {
    pub cursor: u64,
    pub next_cursor: u64,
    pub start_gpa: u64,
    pub end_gpa: u64,
    pub scanned_gpas: Vec<u64>,
    pub scanned_bytes: u64,
    pub scan_sha256: String,
    pub matches: Vec<Match>,
    pub needle_sha256: String,
}
#[derive(Debug)]
struct Tail {
    needle: Vec<u8>,
    start: u64,
    end: u64,
    next: u64,
    last_end: Option<u64>,
    bytes: Vec<u8>,
}
#[derive(Debug)]
pub struct CaptureSnapshot {
    pub pages: Vec<PageCopy>,
    pub masks: Vec<ReadMask>,
    pub scans: Vec<ScanLog>,
}
impl CaptureSnapshot {
    pub fn estimated_bytes(&self) -> usize {
        self.pages
            .iter()
            .map(|p| p.bytes.capacity() + p.sha256.capacity() + 128)
            .sum::<usize>()
            + self.masks.capacity() * std::mem::size_of::<ReadMask>()
            + self
                .scans
                .iter()
                .map(|s| {
                    256 + s.scanned_gpas.capacity() * 8
                        + s.matches.capacity() * std::mem::size_of::<Match>()
                })
                .sum::<usize>()
    }
    pub fn export(
        &self,
        kind: &str,
        offset: u64,
        limit: usize,
    ) -> Result<(u64, Vec<Value>), String> {
        let offset = usize::try_from(offset).map_err(|_| "offset overflow")?;
        match kind {
            "pages" => Ok((
                self.pages.len() as u64,
                self.pages
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .map(PageCopy::value)
                    .collect(),
            )),
            "read_mask" => Ok((
                self.masks.len() as u64,
                self.masks
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .map(|r| serde_json::to_value(r).unwrap())
                    .collect(),
            )),
            "scan_log" => Ok((
                self.scans.len() as u64,
                self.scans
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .map(|r| serde_json::to_value(r).unwrap())
                    .collect(),
            )),
            _ => Err("unknown capture dataset".into()),
        }
    }
    pub fn overlap(&self, dirty: &super::records::DirtyData) -> Vec<u64> {
        self.masks
            .iter()
            .filter(|m| dirty.contains(m.gpa))
            .map(|m| m.gpa)
            .collect()
    }
}
#[derive(Debug)]
pub struct Capture {
    pub id: String,
    pub epoch: Option<u64>,
    pub mode: String,
    pub generation: String,
    pub consistency_mode: Consistency,
    pub census: Option<Arc<Inventory>>,
    regions: Vec<super::host_pages::Region>,
    pub census_record_id: Option<String>,
    pub max_bytes: u64,
    pub max_scan_bytes: u64,
    pub state: String,
    pub consistency: String,
    pub failure: Option<String>,
    pub capture_record_id: Option<String>,
    pub sealed_record_id: Option<String>,
    pub snapshot: Option<Arc<CaptureSnapshot>>,
    pub scan_bytes: u64,
    pages: BTreeMap<u64, PageCopy>,
    masks: BTreeMap<u64, ReadMask>,
    scans: Vec<ScanLog>,
    tail: Option<Tail>,
    matches_count: usize,
}
impl Capture {
    pub fn reservation_for(max_bytes: u64, max_scan_bytes: u64) -> Result<usize, String> {
        // BTree entries, masks, scan vectors and dense matches are distinct from payload.
        // Empty scans can still consume one log entry, so reserve the bounded log count.
        let scan_reserve = if max_scan_bytes == 0 {
            0
        } else {
            (max_scan_bytes / 4096)
                .checked_mul(1024)
                .and_then(|n| n.checked_add(65536 * 512))
                .and_then(|n| n.checked_add(65536 * 32))
                .ok_or("capture reservation overflow")?
        };
        usize::try_from(
            max_bytes
                .checked_add((max_bytes / 4096) * 384)
                .and_then(|n| n.checked_add(scan_reserve))
                .ok_or("capture reservation overflow")?,
        )
        .map_err(|_| "capture reservation overflow".into())
    }
    pub fn new(
        id: String,
        epoch: Option<u64>,
        mode: String,
        generation: String,
        consistency_mode: Consistency,
        census: Option<Arc<Inventory>>,
        census_record_id: Option<String>,
        regions: Vec<super::host_pages::Region>,
        max_bytes: u64,
        max_scan_bytes: u64,
    ) -> Self {
        Self {
            id,
            epoch,
            mode,
            generation,
            consistency_mode,
            census,
            regions,
            census_record_id,
            max_bytes,
            max_scan_bytes,
            state: "open".into(),
            consistency: "unsealed".into(),
            failure: None,
            capture_record_id: None,
            sealed_record_id: None,
            snapshot: None,
            scan_bytes: 0,
            pages: BTreeMap::new(),
            masks: BTreeMap::new(),
            scans: Vec::new(),
            tail: None,
            matches_count: 0,
        }
    }
    pub fn info(&self) -> Value {
        json!({"capture_id":self.id,"epoch":self.epoch,"vm_generation":self.generation,"mode":self.mode,"consistency_mode":self.consistency_mode,
            "state":self.state,"consistency":self.consistency,"failure":self.failure,"census_record_id":self.census_record_id,
            "capture_record_id":self.capture_record_id,"sealed_record_id":self.sealed_record_id,"regions":self.regions,
            "budgets":{"max_bytes":self.max_bytes,"max_scan_bytes":self.max_scan_bytes},
            "retained_bytes":self.snapshot.as_ref().map_or(self.pages.len()*PAGE,|s|s.pages.len()*PAGE),"cumulative_scan_bytes":self.scan_bytes})
    }
    pub fn check(&self, id: &str, generation: &str) -> Result<(), String> {
        if self.id != id || self.generation != generation {
            return Err("capture identity or VM generation mismatch".into());
        }
        Ok(())
    }
    fn open(&self) -> Result<(), String> {
        if self.state != "open" {
            return Err("capture is closed; export frozen records instead".into());
        }
        if let Some(e) = &self.failure {
            return Err(format!("capture read failed; abort required: {e}"));
        }
        Ok(())
    }
    fn copy_page(
        &mut self,
        host: &mut HostPages,
        mem: &GuestMemoryMmap,
        gpa: u64,
        scan: bool,
    ) -> Result<(Vec<u8>, ReadMask), String> {
        let before = host.page_pfn(mem, gpa)?;
        let mask = self.masks.entry(gpa).or_insert(ReadMask {
            gpa,
            metadata_read: false,
            scan_read: false,
            read_count: 0,
            before_present: before.is_some(),
            before_pfn: before,
            after_present: None,
            after_pfn: None,
        });
        mask.metadata_read |= !scan;
        mask.scan_read |= scan;
        mask.read_count = mask
            .read_count
            .checked_add(1)
            .ok_or("read counter overflow")?;
        let mut bytes = vec![0; PAGE];
        // The only live guest payload read in this module. All callers use it.
        if let Err(e) = mem.read_slice(&mut bytes, GuestAddress(gpa)) {
            self.failure = Some(e.to_string());
            return Err(e.to_string());
        }
        let after = match host.page_pfn(mem, gpa) {
            Ok(v) => v,
            Err(e) => {
                self.failure = Some(e.clone());
                return Err(e);
            }
        };
        let mask = self.masks.get_mut(&gpa).unwrap();
        mask.after_present = Some(after.is_some());
        mask.after_pfn = after;
        Ok((bytes, mask.clone()))
    }
    pub fn read_pages(
        &mut self,
        host: &mut HostPages,
        mem: &GuestMemoryMmap,
        gpas: &[u64],
    ) -> Result<Value, String> {
        self.open()?;
        if gpas.is_empty() || gpas.len() > 256 {
            return Err("read_pages requires 1..256 GPAs".into());
        }
        let unique: BTreeSet<u64> = gpas.iter().copied().collect();
        if unique.iter().any(|gpa| !valid_gpa(mem, *gpa)) {
            return Err("invalid or out-of-slot GPA".into());
        }
        let new = unique
            .iter()
            .filter(|g| !self.pages.contains_key(g))
            .count();
        if ((self.pages.len() + new) as u64)
            .checked_mul(PAGE as u64)
            .is_none_or(|n| n > self.max_bytes)
        {
            return Err("retained metadata byte budget exceeded".into());
        }
        let begin = clock_ns();
        let mut rows = Vec::new();
        let mut actually_read = 0;
        for &gpa in gpas {
            let cached = self.pages.contains_key(&gpa);
            if !cached {
                let (bytes, _) = self.copy_page(host, mem, gpa, false)?;
                let sha256 = hash(&bytes);
                self.pages.insert(gpa, PageCopy { gpa, bytes, sha256 });
                actually_read += PAGE;
            }
            let mut value = self.pages[&gpa].value();
            let m = &self.masks[&gpa];
            value["status"] = json!("ok");
            value["cache_hit"] = json!(cached);
            value["before_present"] = json!(m.before_present);
            value["before_pfn"] = json!(m.before_pfn);
            value["after_present"] = json!(m.after_present);
            value["after_pfn"] = json!(m.after_pfn);
            rows.push(value);
        }
        Ok(
            json!({"pages":rows,"bytes_read":actually_read,"new_unique_bytes":new*PAGE,"read_begin_ns":begin,"read_end_ns":clock_ns()}),
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn scan(
        &mut self,
        host: &mut HostPages,
        mem: &GuestMemoryMmap,
        needle: &[u8],
        start: u64,
        end: u64,
        cursor: u64,
        max_scan: u64,
        resident_only: bool,
    ) -> Result<Value, String> {
        self.open()?;
        if self.max_scan_bytes == 0
            || needle.is_empty()
            || needle.len() > 256
            || !resident_only
            || start >= end
            || start % 4096 != 0
            || end % 4096 != 0
            || cursor < start
            || cursor > end
            || cursor % 4096 != 0
            || max_scan < 4096
            || max_scan > 1048576
            || max_scan % 4096 != 0
        {
            return Err("invalid bounded resident scan request".into());
        }
        if self.scans.len() >= 65536 {
            return Err("capture scan request count cap exceeded".into());
        }
        let mut tail = match self.tail.take() {
            Some(t)
                if t.needle == needle && t.start == start && t.end == end && t.next == cursor =>
            {
                t
            }
            _ if cursor == start => Tail {
                needle: needle.to_vec(),
                start,
                end,
                next: cursor,
                last_end: None,
                bytes: Vec::new(),
            },
            other => {
                self.tail = other;
                return Err("scan cursor/query mismatch; restart at range start".into());
            }
        };
        let (gpas, next, complete) = if let Some(census) = &self.census {
            let gpas = census.resident_gpas_from(cursor, end, (max_scan / 4096) as usize)?;
            let next = gpas.last().map_or(end, |g| g + 4096);
            let complete = census.resident_gpas_from(next, end, 1)?.is_empty();
            (gpas, next, complete)
        } else {
            // A capture without a census must not construct a RAM-wide inventory
            // merely to scan. Bound both the presence probes and payload reads by
            // this request's page budget, advancing across absent pages and holes.
            let mut gpas = Vec::new();
            let mut next = cursor;
            let mut examined = 0;
            for region in &self.regions {
                let mut gpa = cursor.max(region.guest_base);
                let limit = end.min(region.guest_base + region.bytes);
                while gpa < limit && examined < max_scan / 4096 {
                    if host.page_pfn(mem, gpa)?.is_some() {
                        gpas.push(gpa);
                    }
                    examined += 1;
                    gpa += 4096;
                    next = gpa;
                }
                if examined == max_scan / 4096 {
                    break;
                }
            }
            let complete = !self
                .regions
                .iter()
                .any(|r| r.guest_base.max(next.max(cursor)) < end.min(r.guest_base + r.bytes));
            if complete {
                next = end;
            }
            (gpas, next, complete)
        };
        let bytes = (gpas.len() as u64) * 4096;
        if self
            .scan_bytes
            .checked_add(bytes)
            .is_none_or(|n| n > self.max_scan_bytes)
        {
            self.tail = Some(tail);
            return Err("scan cumulative byte budget exceeded".into());
        }
        let mut matches = Vec::new();
        let mut ctx = Context::new(&SHA256);
        for &gpa in &gpas {
            let (page, _) = self.copy_page(host, mem, gpa, true)?;
            self.scan_bytes += 4096;
            ctx.update(&page);
            if tail.last_end != Some(gpa) {
                tail.bytes.clear()
            }
            let prefix_len = tail.bytes.len();
            let mut joined = std::mem::take(&mut tail.bytes);
            joined.extend_from_slice(&page);
            let base = gpa
                .checked_sub(prefix_len as u64)
                .ok_or("scan prefix underflow")?;
            for (offset, part) in joined.windows(needle.len()).enumerate() {
                if part == needle {
                    self.matches_count += 1;
                    if matches.len() >= 4096 || self.matches_count > 65536 {
                        self.failure = Some("scan match cap exceeded; narrow the search".into());
                        return Err(self.failure.clone().unwrap());
                    }
                    let addr = base + offset as u64;
                    matches.push(Match {
                        gpa: addr & !4095,
                        offset: addr % 4096,
                    });
                }
            }
            let keep = (needle.len() - 1).min(joined.len());
            tail.bytes = joined[joined.len() - keep..].to_vec();
            tail.last_end = Some(gpa + 4096);
        }
        tail.next = next;
        self.tail = Some(tail);
        let sha = context_hash(ctx);
        self.scans.push(ScanLog {
            cursor,
            next_cursor: next,
            start_gpa: start,
            end_gpa: end,
            scanned_gpas: gpas.clone(),
            scanned_bytes: bytes,
            scan_sha256: sha.clone(),
            matches: matches.clone(),
            needle_sha256: hash(needle),
        });
        Ok(
            json!({"matches":matches,"next_cursor":next,"complete":complete,"scanned_pages":gpas.len(),"scanned_bytes":bytes,"cumulative_scan_bytes":self.scan_bytes,"scan_sha256":sha,"scan_residency_source":if self.census.is_some(){"begin_census"}else{"per_request_pagemap"}}),
        )
    }
    pub fn freeze(
        &mut self,
        abort: bool,
        reason: Option<String>,
    ) -> Result<Arc<CaptureSnapshot>, String> {
        if self.state != "open" {
            return Err("capture already closed".into());
        }
        if !abort && self.failure.is_some() {
            return Err("failed capture must be aborted".into());
        }
        let snapshot = Arc::new(CaptureSnapshot {
            pages: std::mem::take(&mut self.pages).into_values().collect(),
            masks: std::mem::take(&mut self.masks).into_values().collect(),
            scans: std::mem::take(&mut self.scans),
        });
        self.tail = None;
        self.census = None;
        self.state = if abort { "aborted" } else { "sealed" }.into();
        self.consistency = if abort {
            "discarded"
        } else if self.consistency_mode == Consistency::DirtyValidated {
            "pending"
        } else {
            "quiescence_only"
        }
        .into();
        if abort {
            self.failure = reason
        }
        self.snapshot = Some(snapshot.clone());
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn capture(mem: &GuestMemoryMmap) -> (HostPages, Capture) {
        let mut host = HostPages::new(false).unwrap();
        let census = Arc::new(host.inventory(mem).unwrap());
        let c = Capture::new(
            "fixture".into(),
            None,
            "bootstrap".into(),
            "generation".into(),
            Consistency::QuiescenceOnly,
            Some(census.clone()),
            Some("record".into()),
            census.regions(),
            4096,
            1024 * 1024,
        );
        (host, c)
    }
    #[test]
    fn scan_without_census_bounds_presence_probes_and_crosses_holes() {
        let mem = crate::test_utils::multi_region_mem(&[
            (GuestAddress(0), 2 * PAGE),
            (GuestAddress(4 * PAGE as u64), 2 * PAGE),
        ]);
        mem.write_slice(b"needle", GuestAddress(0)).unwrap();
        mem.write_slice(b"needle", GuestAddress(5 * PAGE as u64))
            .unwrap();
        let mut host = HostPages::new(false).unwrap();
        let regions = HostPages::layout(&mem)
            .unwrap()
            .into_iter()
            .map(|(r, _)| r)
            .collect();
        let mut c = Capture::new(
            "no-census".into(),
            None,
            "bootstrap".into(),
            "generation".into(),
            Consistency::QuiescenceOnly,
            None,
            None,
            regions,
            PAGE as u64,
            4 * PAGE as u64,
        );
        let mut cursor = 0;
        let mut matches = Vec::new();
        let mut replies = 0;
        loop {
            let r = c
                .scan(
                    &mut host,
                    &mem,
                    b"needle",
                    0,
                    6 * PAGE as u64,
                    cursor,
                    PAGE as u64,
                    true,
                )
                .unwrap();
            assert_eq!(r["scan_residency_source"], "per_request_pagemap");
            assert!(r["scanned_pages"].as_u64().unwrap() <= 1);
            let next = r["next_cursor"].as_u64().unwrap();
            assert!(next > cursor);
            matches.extend(r["matches"].as_array().unwrap().iter().cloned());
            cursor = next;
            replies += 1;
            if r["complete"] == true {
                break;
            }
        }
        assert_eq!(replies, 4);
        assert_eq!(
            matches,
            vec![
                json!({"gpa":0,"offset":0}),
                json!({"gpa":5 * PAGE,"offset":0})
            ]
        );
        assert!(c.census.is_none());
        assert_eq!(c.scans.len(), 4);
        assert!(c.masks[&0].scan_read && c.masks[&(5 * PAGE as u64)].scan_read);
        let r = c
            .scan(
                &mut host,
                &mem,
                b"needle",
                0,
                6 * PAGE as u64,
                cursor,
                PAGE as u64,
                true,
            )
            .unwrap();
        assert_eq!(r["complete"], true);
        assert_eq!(r["scanned_bytes"], 0);
    }
    #[test]
    fn dense_scan_and_empty_log_capacity_are_reserved() {
        let mem = crate::test_utils::multi_region_mem(&[(GuestAddress(0), 4096)]);
        mem.write_slice(&[0u8; 4096], GuestAddress(0)).unwrap();
        let (mut host, mut c) = capture(&mem);
        c.max_scan_bytes = 4096;
        let reply = c
            .scan(&mut host, &mem, &[0], 0, 4096, 0, 4096, true)
            .unwrap();
        assert_eq!(reply["matches"].as_array().unwrap().len(), 4096);
        let snapshot = c.freeze(false, None).unwrap();
        assert!(snapshot.estimated_bytes() < Capture::reservation_for(4096, 4096).unwrap());
        let (_, mut disabled) = capture(&mem);
        disabled.max_scan_bytes = 0;
        assert!(
            disabled
                .scan(&mut host, &mem, &[0], 0, 4096, 4096, 4096, true)
                .is_err()
        );
        assert!(disabled.scans.is_empty());
    }

    #[test]
    fn scan_finds_page_and_request_straddles_without_rereading_tail() {
        let mem = crate::test_utils::multi_region_mem(&[(GuestAddress(0), 3 * 4096)]);
        mem.write_slice(&vec![0u8; 3 * 4096], GuestAddress(0))
            .unwrap();
        let needle = b"cross-page-needle";
        mem.write_slice(needle, GuestAddress(4090)).unwrap();
        let (mut host, mut c) = capture(&mem);
        let a = c
            .scan(&mut host, &mem, needle, 0, 3 * 4096, 0, 4096, true)
            .unwrap();
        assert_eq!(a["matches"].as_array().unwrap().len(), 0);
        let b = c
            .scan(&mut host, &mem, needle, 0, 3 * 4096, 4096, 4096, true)
            .unwrap();
        assert_eq!(b["matches"], json!([{"gpa":0,"offset":4090}]));
        assert_eq!(c.masks[&0].read_count, 1);
        assert_eq!(c.masks[&4096].read_count, 1);
        let restart = c
            .scan(&mut host, &mem, needle, 0, 3 * 4096, 0, 8192, true)
            .unwrap();
        assert_eq!(restart["matches"], json!([{"gpa":0,"offset":4090}]));
    }
    #[test]
    fn scan_tail_resets_across_gpa_holes() {
        let mem = crate::test_utils::multi_region_mem(&[
            (GuestAddress(0), 4096),
            (GuestAddress(8192), 4096),
        ]);
        mem.write_slice(&vec![0u8; 4096], GuestAddress(0)).unwrap();
        mem.write_slice(&vec![0u8; 4096], GuestAddress(8192))
            .unwrap();
        mem.write_slice(b"AB", GuestAddress(4094)).unwrap();
        mem.write_slice(b"CD", GuestAddress(8192)).unwrap();
        let (mut host, mut c) = capture(&mem);
        let r = c
            .scan(&mut host, &mem, b"ABCD", 0, 12288, 0, 8192, true)
            .unwrap();
        assert!(r["matches"].as_array().unwrap().is_empty());
        assert_eq!(c.masks.len(), 2);
        assert!(!c.masks.contains_key(&4096));
    }
    #[test]
    fn invalid_batch_is_rejected_before_any_payload_read() {
        let mem = crate::test_utils::multi_region_mem(&[(GuestAddress(0), 4096)]);
        mem.write_obj(1u8, GuestAddress(0)).unwrap();
        let (mut host, mut c) = capture(&mem);
        assert!(c.read_pages(&mut host, &mem, &[0, 4096]).is_err());
        assert!(c.masks.is_empty());
        let one = c.read_pages(&mut host, &mem, &[0]).unwrap();
        assert_eq!(one["bytes_read"], 4096);
        let two = c.read_pages(&mut host, &mem, &[0]).unwrap();
        assert_eq!(two["bytes_read"], 0);
        assert_eq!(c.masks[&0].read_count, 1);
    }
}
