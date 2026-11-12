// SPDX-License-Identifier: Apache-2.0
//! Shared ownership and pause guards, without mandatory selection of any metric.
use super::api::{Consistency, Metric, Request, Resource};
use super::capture::{Capture, clock_ns, hash};
use super::host_pages::HostPages;
use super::records::{DirtyData, MAX_RETAINED_BYTES, Payload, Record, Records};
use crate::Vmm;
use crate::dirty_log::{DirtyLogRequest, DirtyLogState, quiesce_devices};
use crate::vmm_config::instance_info::VmState;
use crate::vstate::memory::{Address, GuestMemoryRegion};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use vm_memory::GuestMemoryBackend;

#[derive(Debug, Clone)]
struct Epoch {
    number: u64,
    phase: String,
}
#[derive(Debug)]
pub struct Failure(pub Value);
#[derive(Debug, Default)]
pub struct Manager {
    generation: String,
    layout: Vec<(u64, u64)>,
    host: Option<HostPages>,
    host_idle: bool,
    records: Records,
    coordinator_started: bool,
    selected: Vec<Metric>,
    epoch: Option<Epoch>,
    idle_started: bool,
    idle_epoch: Option<Epoch>,
    captures: BTreeMap<String, Capture>,
    seen_captures: BTreeSet<String>,
    last: Option<(Resource, Value, Value)>,
    poisoned: Option<String>,
    mutation_performed: bool,
    reset_performed: bool,
    error_code: &'static str,
}
impl Manager {
    fn enabled(&self, m: Metric) -> bool {
        self.selected.contains(&m)
    }
    pub fn blocks_resume(&self) -> bool {
        self.captures
            .values()
            .any(|c| c.state == "open" || c.consistency == "pending")
    }
    pub fn blocks_external_mutation(&self) -> bool {
        self.captures
            .values()
            .any(|c| c.state == "open" || c.consistency == "pending")
    }
    pub fn snapshot_conflicts(&self) -> bool {
        self.coordinator_started || self.idle_started || self.blocks_external_mutation()
    }
    pub fn guard_independent_dirty(&self) -> Result<(), String> {
        if self.enabled(Metric::Dirty) {
            return Err("dirty reset owned by trace-epochs coordinator".into());
        }
        if self.blocks_external_mutation() {
            return Err("dirty reset conflicts with metadata capture".into());
        }
        Ok(())
    }
    fn generation(&self, expected: &str) -> Result<(), String> {
        if expected != self.generation {
            return Err("VM generation mismatch before mutation".into());
        }
        Ok(())
    }
    fn paused(vmm: &Vmm) -> Result<(), String> {
        if vmm.instance_info.state != VmState::Paused {
            return Err("operation requires HTTP-held paused VM".into());
        }
        Ok(())
    }
    fn init(&mut self, vmm: &Vmm) -> Result<(), String> {
        let layout: Vec<_> = vmm
            .vm
            .as_kvm()
            .ok_or("tracing requires a KVM VM")?
            .guest_memory()
            .iter()
            .map(|r| (r.start_addr().raw_value(), r.len()))
            .collect();
        if self.generation.is_empty() {
            self.generation = format!(
                "trace-{}-{}",
                &hash(vmm.instance_info.id.as_bytes())[..16],
                clock_ns()
            );
            self.records.generation = self.generation.clone();
            self.layout = layout;
        } else if self.layout != layout {
            self.poisoned = Some("guest RAM slot layout changed".into());
            return Err("guest RAM slot layout changed".into());
        }
        Ok(())
    }
    fn ensure_host(&mut self, idle: bool) -> Result<(), String> {
        if self.host.is_none() || (idle && !self.host_idle) {
            self.host = Some(HostPages::new(idle)?);
            self.host_idle = idle;
        }
        Ok(())
    }
    fn reservation(&self) -> Result<usize, String> {
        self.captures
            .values()
            .filter(|c| c.state == "open")
            .try_fold(0usize, |n, c| {
                n.checked_add(Capture::reservation_for(c.max_bytes, c.max_scan_bytes)?)
                    .ok_or_else(|| "capture capacity sum overflow".into())
            })
    }
    fn reserved_slots(&self) -> usize {
        self.captures
            .values()
            .map(|c| {
                if c.state == "open" {
                    if c.consistency_mode == Consistency::DirtyValidated {
                        2
                    } else {
                        1
                    }
                } else if c.consistency == "pending" {
                    if c.capture_record_id.is_none() { 2 } else { 1 }
                } else if c.capture_record_id.is_none() && c.snapshot.is_some() {
                    1
                } else {
                    0
                }
            })
            .sum()
    }
    fn capacity(&mut self, count: usize, extra: usize) -> Result<(), String> {
        self.capacity_consuming(count, extra, 0)
    }
    fn capacity_consuming(
        &mut self,
        count: usize,
        extra: usize,
        consume: usize,
    ) -> Result<(), String> {
        let reserved = self
            .reservation()?
            .checked_add(self.host.as_ref().map_or(0, HostPages::estimated_bytes))
            .ok_or("capacity overflow")?;
        let slots = count
            .checked_add(self.reserved_slots().saturating_sub(consume))
            .ok_or("record reservation overflow")?;
        let result = self.records.preflight(slots, extra, reserved);
        if result.is_err() {
            self.error_code = "backpressure"
        }
        result
    }
    fn phase(value: Option<String>) -> Result<String, String> {
        let p = value.unwrap_or_else(|| "unspecified".into());
        if p.is_empty() || p.len() > 128 {
            return Err("phase must contain 1..128 bytes".into());
        }
        Ok(p)
    }
    fn region_value(vmm: &Vmm) -> Result<Value, String> {
        Ok(json!(vmm.vm.as_kvm().ok_or("tracing requires a KVM VM")?.guest_memory().iter().enumerate().map(|(slot,r)|json!({"slot":slot,"guest_base":r.start_addr().raw_value(),"bytes":r.len(),"page_count":r.len()/4096})).collect::<Vec<_>>()))
    }
    pub fn invalidate_lifecycle(&mut self, reason: String) {
        self.poisoned = Some(format!("native lifecycle uncertain: {reason}; restart VM"));
    }
    pub fn transact(
        &mut self,
        vmm: &mut Vmm,
        dirty: &mut DirtyLogState,
        resource: Resource,
        raw: Value,
        http_held: bool,
    ) -> Result<Value, Failure> {
        self.mutation_performed = false;
        self.reset_performed = false;
        self.error_code = "invalid_request";
        let mut arming = false;
        let result = (|| {
            self.init(vmm)?;
            let req: Request = serde_json::from_value(raw.clone()).map_err(|e| e.to_string())?;
            arming = matches!(&req, Request::Arm { .. });
            let cacheable = !matches!(
                &req,
                Request::Capture {
                    request_id: None,
                    ..
                }
            );
            if self.poisoned.is_some()
                && !matches!(
                    req,
                    Request::Export { .. }
                        | Request::Ack { .. }
                        | Request::Info { .. }
                        | Request::Abort { .. }
                )
            {
                self.error_code = "poisoned";
                return Err(format!(
                    "collector poisoned; restart VM: {}",
                    self.poisoned.as_ref().unwrap()
                ));
            }
            if let Some((r, q, reply)) = &self.last {
                if cacheable && *r == resource && *q == raw {
                    return Ok(reply.clone());
                }
            }
            if !http_held
                && !matches!(
                    &req,
                    Request::Export { .. }
                        | Request::Ack { .. }
                        | Request::Info { .. }
                        | Request::Abort { .. }
                )
            {
                self.error_code = "conflict";
                return Err("requires successful HTTP Pause hold, not merely paused vCPUs".into());
            }
            let mut value = self.execute(vmm, dirty, resource, req)?;
            value["schema"] = json!(resource.schema());
            value["vm_generation"] = json!(self.generation);
            value["estimated_retained_record_capacity_bytes"] =
                json!(self.records.retained_bytes());
            value["record_capacity_limit_bytes"] = json!(MAX_RETAINED_BYTES);
            self.last = Some((resource, raw.clone(), value.clone()));
            Ok(value)
        })();
        result.map_err(|message:String|{
            if self.reset_performed||(arming&&self.mutation_performed){self.poisoned=Some(message.clone());self.error_code="poisoned"}
            Failure(json!({"schema":resource.schema(),"vm_generation":self.generation,"error":{"code":self.error_code,"message":message,"mutation_performed":self.mutation_performed,"reset_performed":self.reset_performed,"poisoned":self.poisoned.is_some()}}))
        })
    }
    fn execute(
        &mut self,
        vmm: &mut Vmm,
        dirty: &mut DirtyLogState,
        resource: Resource,
        req: Request,
    ) -> Result<Value, String> {
        match req {
            Request::Export {
                record_id,
                kind,
                offset,
                limit,
            } => {
                let record = self
                    .records
                    .records
                    .get(&record_id)
                    .ok_or("unknown immutable record")?;
                if resource == Resource::Metadata
                    && !matches!(record.payload, Payload::Capture { .. })
                {
                    return Err("metadata export requires capture record".into());
                }
                if resource == Resource::Census && kind != "resident" && kind != "absent_ranges" {
                    return Err("census export requires resident or absent_ranges".into());
                }
                record.export(&record_id, &kind, offset, limit)
            }
            Request::Ack {
                record_id,
                vm_generation,
            } => {
                self.generation(&vm_generation)?;
                if self.captures.values().any(|c| {
                    (c.state == "open" || c.consistency == "pending")
                        && (c.census_record_id.as_ref() == Some(&record_id)
                            || c.capture_record_id.as_ref() == Some(&record_id)
                            || c.sealed_record_id.as_ref() == Some(&record_id))
                }) {
                    self.error_code = "conflict";
                    return Err(
                        "record still referenced by open capture or pending validation".into(),
                    );
                }
                if self.records.records.remove(&record_id).is_none() {
                    return Err("unknown immutable record".into());
                }
                self.mutation_performed = true;
                let records = &self.records.records;
                self.captures.retain(|_, c| {
                    let payload_retained = c
                        .capture_record_id
                        .as_ref()
                        .is_some_and(|id| records.contains_key(id))
                        || c.sealed_record_id
                            .as_ref()
                            .is_some_and(|id| records.contains_key(id));
                    if !payload_retained {
                        c.snapshot = None;
                    }
                    payload_retained
                        || c.census_record_id
                            .as_ref()
                            .is_some_and(|id| records.contains_key(id))
                });
                Ok(json!({"record_id":record_id,"released":true}))
            }
            Request::Arm {
                epoch,
                phase,
                metrics,
            } if resource == Resource::Epochs || resource == Resource::Idle => {
                self.arm(vmm, dirty, resource, epoch, Self::phase(phase)?, metrics)
            }
            Request::Rotate {
                vm_generation,
                epoch,
                next_epoch,
                phase,
            } if resource == Resource::Epochs => {
                self.generation(&vm_generation)?;
                self.rotate(vmm, dirty, resource, epoch, next_epoch, phase)
            }
            Request::Collect {
                vm_generation,
                epoch,
                next_epoch,
                phase,
            } if resource == Resource::Idle => {
                self.generation(&vm_generation)?;
                self.rotate(vmm, dirty, resource, epoch, next_epoch, phase)
            }
            Request::Capture {
                vm_generation,
                epoch,
                request_id,
            } if resource == Resource::Census => {
                if let Some(generation) = vm_generation {
                    self.generation(&generation)?;
                }
                if request_id
                    .as_ref()
                    .is_some_and(|id| id.is_empty() || id.len() > 128)
                {
                    return Err("invalid census request_id".into());
                }
                Self::paused(vmm)?;
                if self.blocks_external_mutation() {
                    return Err("new census conflicts with open/pending metadata capture".into());
                }
                self.ensure_host(false)?;
                self.capacity(
                    1,
                    HostPages::capacity_preflight(
                        vmm.vm
                            .as_kvm()
                            .ok_or("tracing requires a KVM VM")?
                            .guest_memory(),
                    )? / 2,
                )?;
                self.mutation_performed = true;
                quiesce_devices(vmm)?;
                let census = Arc::new(
                    self.host.as_mut().unwrap().inventory(
                        vmm.vm
                            .as_kvm()
                            .ok_or("tracing requires a KVM VM")?
                            .guest_memory(),
                    )?,
                );
                let mut metadata = census.metadata_value();
                metadata["epoch"] = json!(epoch);
                metadata["request_id"] = json!(request_id);
                metadata["vm_generation"] = json!(self.generation);
                let id = self
                    .records
                    .insert(Record::new(metadata.clone(), Payload::Census(census)))?;
                Ok(
                    json!({"record_id":id,"census_record_id":id,"metadata":metadata,"epoch":epoch,"regions":Self::region_value(vmm)?}),
                )
            }
            Request::Begin {
                vm_generation,
                capture_id,
                epoch,
                mode,
                consistency,
                max_bytes,
                max_scan_bytes,
                capture_census,
            } if resource == Resource::Metadata => self.begin(
                vmm,
                capture_id,
                vm_generation,
                epoch,
                mode,
                consistency,
                max_bytes,
                max_scan_bytes,
                capture_census.unwrap_or(true),
            ),
            Request::Info {
                capture_id,
                vm_generation,
            } if resource == Resource::Metadata => {
                self.generation(&vm_generation)?;
                let cap = self.captures.get(&capture_id).ok_or("unknown capture")?;
                cap.check(&capture_id, &vm_generation)?;
                Ok(cap.info())
            }
            Request::ReadPages {
                capture_id,
                vm_generation,
                gpas,
            } if resource == Resource::Metadata => {
                Self::paused(vmm)?;
                self.generation(&vm_generation)?;
                self.ensure_host(false)?;
                let cap = self
                    .captures
                    .get_mut(&capture_id)
                    .ok_or("unknown capture")?;
                cap.check(&capture_id, &vm_generation)?;
                // Conservatively classify any read attempt as a mutation of observation state.
                self.mutation_performed = true;
                self.error_code = "capture_failure";
                let mut value = cap.read_pages(
                    self.host.as_mut().unwrap(),
                    vmm.vm
                        .as_kvm()
                        .ok_or("tracing requires a KVM VM")?
                        .guest_memory(),
                    &gpas,
                )?;
                value["capture_id"] = json!(capture_id);
                value["epoch"] = json!(cap.epoch);
                Ok(value)
            }
            Request::Scan {
                capture_id,
                vm_generation,
                needle_b64,
                start_gpa,
                end_gpa,
                cursor,
                max_scan_bytes,
                resident_only,
            } if resource == Resource::Metadata => {
                use base64::Engine;
                let needle = base64::engine::general_purpose::STANDARD
                    .decode(needle_b64)
                    .map_err(|e| e.to_string())?;
                Self::paused(vmm)?;
                self.generation(&vm_generation)?;
                self.ensure_host(false)?;
                let cap = self
                    .captures
                    .get_mut(&capture_id)
                    .ok_or("unknown capture")?;
                cap.check(&capture_id, &vm_generation)?;
                self.mutation_performed = true;
                self.error_code = "capture_failure";
                let mut value = cap.scan(
                    self.host.as_mut().unwrap(),
                    vmm.vm
                        .as_kvm()
                        .ok_or("tracing requires a KVM VM")?
                        .guest_memory(),
                    &needle,
                    start_gpa,
                    end_gpa,
                    cursor,
                    max_scan_bytes,
                    resident_only,
                )?;
                value["capture_id"] = json!(capture_id);
                value["epoch"] = json!(cap.epoch);
                Ok(value)
            }
            Request::Seal {
                capture_id,
                vm_generation,
            } if resource == Resource::Metadata => {
                Self::paused(vmm)?;
                self.generation(&vm_generation)?;
                self.close_capture(&capture_id, &vm_generation, false, None)
            }
            Request::Abort {
                capture_id,
                vm_generation,
                reason,
            } if resource == Resource::Metadata => {
                self.generation(&vm_generation)?;
                if reason.len() > 1024 {
                    return Err("abort reason exceeds bound".into());
                }
                self.close_capture(&capture_id, &vm_generation, true, Some(reason))
            }
            _ => Err("operation is not supported by this metric resource".into()),
        }
    }
    fn arm(
        &mut self,
        vmm: &mut Vmm,
        dirty: &mut DirtyLogState,
        resource: Resource,
        epoch: u64,
        phase: String,
        metrics: Option<Vec<Metric>>,
    ) -> Result<Value, String> {
        Self::paused(vmm)?;
        if epoch != 1 || self.blocks_external_mutation() {
            return Err("initial arm requires epoch1 and no live capture".into());
        }
        let coordinated = resource == Resource::Epochs;
        let selected = if coordinated {
            if self.coordinator_started {
                return Err("coordinator already owns metrics; restart VM".into());
            }
            let metrics = metrics.ok_or("coordinator arm requires selected metrics")?;
            if metrics.is_empty() || metrics.len() > 4 {
                return Err("select 1..4 metrics".into());
            }
            for (i, m) in metrics.iter().enumerate() {
                if metrics[..i].contains(m) {
                    return Err("duplicate selected metric".into());
                }
            }
            if metrics.contains(&Metric::Dirty) && dirty.started() {
                return Err("dirty-log already selected independently".into());
            }
            if metrics.contains(&Metric::Idle) && self.idle_started {
                return Err("page-idle already selected independently".into());
            }
            metrics
        } else {
            if metrics.is_some() || self.idle_started || self.enabled(Metric::Idle) {
                return Err("page-idle already owned or invalid metrics parameter".into());
            }
            vec![Metric::Idle]
        };
        let use_idle = selected.contains(&Metric::Idle);
        let use_census = selected.contains(&Metric::Census);
        let use_dirty = selected.contains(&Metric::Dirty);
        if use_idle || use_census {
            self.ensure_host(use_idle)?
        }
        let extra = if use_idle || use_census {
            HostPages::capacity_preflight(
                vmm.vm
                    .as_kvm()
                    .ok_or("tracing requires a KVM VM")?
                    .guest_memory(),
            )?
        } else {
            0
        };
        self.capacity(usize::from(use_dirty || use_census), extra)?;
        if coordinated {
            self.selected = selected.clone();
            self.coordinator_started = true
        } else {
            self.idle_started = true
        }
        self.mutation_performed = true;
        quiesce_devices(vmm)?;
        let mut dirty_data = Arc::new(DirtyData::default());
        if use_dirty {
            self.reset_performed = true;
            let reply = dirty.transact(
                vmm,
                DirtyLogRequest {
                    operation: "arm".into(),
                    epoch: 1,
                    next_epoch: None,
                },
            )?;
            dirty_data = Arc::new(DirtyData::from_reply(&reply)?);
        }
        let census = if use_census {
            Some(Arc::new(
                self.host.as_mut().unwrap().inventory(
                    vmm.vm
                        .as_kvm()
                        .ok_or("tracing requires a KVM VM")?
                        .guest_memory(),
                )?,
            ))
        } else {
            None
        };
        let arm = if use_idle {
            self.reset_performed = true;
            self.host.as_mut().unwrap().arm(
                vmm.vm
                    .as_kvm()
                    .ok_or("tracing requires a KVM VM")?
                    .guest_memory(),
            )?
        } else {
            Value::Null
        };
        let baseline = if use_dirty || use_census {
            let metadata = json!({"epoch":1,"phase":"boot_baseline","baseline_discarded":true,"regions":Self::region_value(vmm)?,"page_bytes":4096,"dirty":dirty_data.metadata,"census":census.as_ref().map(|c|c.metadata_value()),"vm_generation":self.generation});
            Some(self.records.insert(Record::new(
                metadata,
                Payload::Epoch {
                    access: None,
                    census,
                    dirty: dirty_data,
                },
            ))?)
        } else {
            None
        };
        if coordinated {
            self.selected = selected.clone();
            self.coordinator_started = true;
            self.epoch = Some(Epoch {
                number: 1,
                phase: phase.clone(),
            })
        } else {
            self.idle_started = true;
            self.idle_epoch = Some(Epoch {
                number: 1,
                phase: phase.clone(),
            })
        }
        Ok(
            json!({"epoch":1,"active_epoch":1,"phase":phase,"selected_metrics":selected,"regions":Self::region_value(vmm)?,"baseline_discarded":use_dirty,
            "epoch_record_id":baseline,"dirty_record_id":if use_dirty{baseline.clone()}else{None},"access_record_id":Value::Null,
            "census_record_id":if use_census{baseline.clone()}else{None},"arm_receipt":arm,"capture_validation":[]}),
        )
    }
    fn rotate(
        &mut self,
        vmm: &mut Vmm,
        dirty: &mut DirtyLogState,
        resource: Resource,
        epoch: u64,
        next: Option<u64>,
        phase: Option<String>,
    ) -> Result<Value, String> {
        Self::paused(vmm)?;
        let coordinated = resource == Resource::Epochs;
        let outgoing = if coordinated {
            self.epoch.clone()
        } else {
            self.idle_epoch.clone()
        }
        .ok_or("no active epoch")?;
        if outgoing.number != epoch || next.is_some_and(|n| epoch.checked_add(1) != Some(n)) {
            return Err("invalid epoch transition".into());
        }
        if !coordinated && (self.enabled(Metric::Idle) || self.blocks_external_mutation()) {
            return Err("page-idle reset owned by coordinator or conflicts with capture".into());
        }
        if self.captures.values().any(|c| c.state == "open") {
            return Err("seal or abort metadata capture before collecting".into());
        }
        let next_phase = if next.is_some() {
            Some(Self::phase(phase)?)
        } else {
            if phase.is_some() {
                return Err("closed epoch has no successor phase".into());
            }
            None
        };
        let use_idle = !coordinated || self.enabled(Metric::Idle);
        let use_dirty = coordinated && self.enabled(Metric::Dirty);
        let use_census = coordinated && self.enabled(Metric::Census);
        let pending: Vec<String> = if coordinated {
            self.captures
                .values()
                .filter(|c| c.consistency == "pending" && c.epoch == Some(epoch))
                .map(|c| c.id.clone())
                .collect()
        } else {
            Vec::new()
        };
        if use_idle || use_census {
            self.ensure_host(use_idle)?
        }
        let extra = if use_idle || use_census {
            HostPages::capacity_preflight(
                vmm.vm
                    .as_kvm()
                    .ok_or("tracing requires a KVM VM")?
                    .guest_memory(),
            )?
        } else {
            0
        };
        self.capacity_consuming(1 + pending.len(), extra, 0)?;
        self.mutation_performed = true;
        quiesce_devices(vmm)?;
        self.reset_performed = use_idle || use_dirty;
        let access = if use_idle {
            Some(Arc::new(
                self.host.as_mut().unwrap().collect(
                    vmm.vm
                        .as_kvm()
                        .ok_or("tracing requires a KVM VM")?
                        .guest_memory(),
                )?,
            ))
        } else {
            None
        };
        let dirty_data = if use_dirty {
            Arc::new(DirtyData::from_reply(&dirty.transact(
                vmm,
                DirtyLogRequest {
                    operation: "collect".into(),
                    epoch,
                    next_epoch: next,
                },
            )?)?)
        } else {
            Arc::new(DirtyData::default())
        };
        let census = if use_census {
            Some(match &access {
                Some(a) => a.end_census.clone(),
                None => Arc::new(
                    self.host.as_mut().unwrap().inventory(
                        vmm.vm
                            .as_kvm()
                            .ok_or("tracing requires a KVM VM")?
                            .guest_memory(),
                    )?,
                ),
            })
        } else {
            None
        };
        let mut validations = Vec::new();
        let mut finalized = Vec::new();
        for id in pending {
            let cap = self.captures.get(&id).unwrap();
            let snapshot = cap
                .snapshot
                .clone()
                .ok_or("sealed capture lacks frozen payload")?;
            let overlap = Arc::new(snapshot.overlap(&dirty_data));
            let consistency = if overlap.is_empty() {
                "valid"
            } else {
                "invalid"
            };
            let mut metadata = cap.info();
            metadata["consistency"] = json!(consistency);
            let sealed = cap.sealed_record_id.clone();
            let final_id = self.records.insert(Record::new(
                metadata,
                Payload::Capture {
                    snapshot,
                    overlap: overlap.clone(),
                },
            ))?;
            finalized.push((id.clone(), consistency.to_string(), final_id.clone()));
            validations.push(json!({"capture_id":id,"sealed_record_id":sealed,"finalized_capture_record_id":final_id,"consistency":consistency,"dirty_overlap_pages":overlap.len(),"dirty_overlap_gpas":overlap.iter().take(256).collect::<Vec<_>>(),"dirty_overlap_truncated":overlap.len()>256}));
        }
        let metadata = json!({"epoch":epoch,"phase":outgoing.phase,"next_epoch":next,"regions":Self::region_value(vmm)?,"page_bytes":4096,
            "access":access.as_ref().map(|a|a.metadata_value()),"dirty":dirty_data.metadata,"census":census.as_ref().map(|c|c.metadata_value()),
            "vm_generation":self.generation,"capture_validation":validations,
            "clock_note":"source-specific non-atomic envelopes; dirty reset precedes next access arm"});
        let record_id = self.records.insert(Record::new(
            metadata,
            Payload::Epoch {
                access,
                census,
                dirty: dirty_data,
            },
        ))?;
        let arm = if next.is_some() && use_idle {
            self.host.as_mut().unwrap().arm(
                vmm.vm
                    .as_kvm()
                    .ok_or("tracing requires a KVM VM")?
                    .guest_memory(),
            )?
        } else {
            Value::Null
        };
        for (id, consistency, final_id) in finalized {
            let cap = self.captures.get_mut(&id).unwrap();
            cap.consistency = consistency;
            cap.capture_record_id = Some(final_id);
        }
        let successor = next.map(|n| Epoch {
            number: n,
            phase: next_phase.clone().unwrap(),
        });
        if coordinated {
            self.epoch = successor
        } else {
            self.idle_epoch = successor
        }
        Ok(
            json!({"epoch":epoch,"next_epoch":next,"active_epoch":next,"phase":outgoing.phase,"next_phase":next_phase,"regions":Self::region_value(vmm)?,
            "epoch_record_id":record_id,"dirty_record_id":if use_dirty{Some(record_id.clone())}else{None},
            "access_record_id":if use_idle{Some(record_id.clone())}else{None},"census_record_id":if use_census{Some(record_id.clone())}else{None},
            "arm_receipt":arm,"capture_validation":validations}),
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn begin(
        &mut self,
        vmm: &mut Vmm,
        id: String,
        expected: Option<String>,
        epoch: Option<u64>,
        mode: String,
        consistency: Consistency,
        max_bytes: Option<u64>,
        max_scan: Option<u64>,
        capture_census: bool,
    ) -> Result<Value, String> {
        Self::paused(vmm)?;
        if let Some(g) = expected.as_deref() {
            self.generation(g)?
        }
        if id.is_empty() || id.len() > 128 || !matches!(mode.as_str(), "bootstrap" | "attribution")
        {
            return Err("invalid capture identifier or mode".into());
        }
        if self.seen_captures.contains(&id) || self.seen_captures.len() >= 4096 {
            return Err("capture IDs must be unique, bounded to4096 perVM".into());
        }
        if self.blocks_external_mutation() {
            return Err("another capture is open or pending validation".into());
        }
        if consistency == Consistency::DirtyValidated {
            if expected.is_none()
                || !self.enabled(Metric::Dirty)
                || !self.enabled(Metric::Metadata)
                || self.epoch.as_ref().map(|e| e.number) != epoch
            {
                return Err("dirty_validated requires matching generation and active coordinator dirty+metadata epoch".into());
            }
        }
        if self.enabled(Metric::Metadata) && self.epoch.as_ref().map(|e| e.number) != epoch {
            return Err("capture epoch does not match coordinator".into());
        }
        let max_bytes = max_bytes.unwrap_or(256 * 1024 * 1024);
        let max_scan = max_scan.unwrap_or(if mode == "bootstrap" {
            1024 * 1024 * 1024
        } else {
            0
        });
        if max_bytes == 0 || max_bytes > 1024 * 1024 * 1024 || max_scan > 1024 * 1024 * 1024 {
            return Err("capture budgets exceed supported bounds".into());
        }
        let reserve = Capture::reservation_for(max_bytes, max_scan)?;
        self.ensure_host(false)?;
        // The reservation is a conservative retained-capacity bound, not an RSS limit.
        let extra = if capture_census {
            HostPages::capacity_preflight(
                vmm.vm
                    .as_kvm()
                    .ok_or("tracing requires a KVM VM")?
                    .guest_memory(),
            )? / 2
        } else {
            0
        };
        self.capacity(
            usize::from(capture_census)
                + if consistency == Consistency::DirtyValidated {
                    2
                } else {
                    1
                },
            extra
                .checked_add(reserve)
                .ok_or("capture reservation overflow")?,
        )?;
        self.mutation_performed = true;
        quiesce_devices(vmm)?;
        let (census, census_id) = if capture_census {
            let census = Arc::new(
                self.host.as_mut().unwrap().inventory(
                    vmm.vm
                        .as_kvm()
                        .ok_or("tracing requires a KVM VM")?
                        .guest_memory(),
                )?,
            );
            let mut metadata = census.metadata_value();
            metadata["epoch"] = json!(epoch);
            metadata["vm_generation"] = json!(self.generation);
            let id = self
                .records
                .insert(Record::new(metadata, Payload::Census(census.clone())))?;
            (Some(census), Some(id))
        } else {
            (None, None)
        };
        let regions = HostPages::layout(
            vmm.vm
                .as_kvm()
                .ok_or("tracing requires a KVM VM")?
                .guest_memory(),
        )?
        .into_iter()
        .map(|(r, _)| r)
        .collect();
        let cap = Capture::new(
            id.clone(),
            epoch,
            mode,
            self.generation.clone(),
            consistency,
            census,
            census_id,
            regions,
            max_bytes,
            max_scan,
        );
        let reply = cap.info();
        self.seen_captures.insert(id.clone());
        self.captures.insert(id, cap);
        Ok(reply)
    }
    fn close_capture(
        &mut self,
        id: &str,
        generation: &str,
        abort: bool,
        reason: Option<String>,
    ) -> Result<Value, String> {
        let cap = self.captures.get(id).ok_or("unknown capture")?;
        cap.check(id, generation)?;
        if abort && cap.state != "open" && cap.consistency != "pending" {
            return Err("only open or sealed-pending captures may be explicitly discarded".into());
        }
        if !abort && cap.state != "open" && cap.capture_record_id.is_some() {
            return Err("capture is already closed".into());
        }
        self.capacity_consuming(1, 0, 1)?;
        self.mutation_performed = true;
        let cap = self.captures.get_mut(id).unwrap();
        let snapshot = if cap.state == "open" {
            cap.freeze(abort, reason)?
        } else if abort {
            cap.state = "aborted".into();
            cap.consistency = "discarded".into();
            cap.failure = reason;
            cap.snapshot
                .clone()
                .ok_or("closed capture lacks snapshot")?
        } else if cap.capture_record_id.is_none() {
            cap.snapshot
                .clone()
                .ok_or("closed capture lacks snapshot")?
        } else {
            return Err("capture is already closed".into());
        };
        let metadata = cap.info();
        let record_id = self.records.insert(Record::new(
            metadata,
            Payload::Capture {
                snapshot,
                overlap: Arc::new(Vec::new()),
            },
        ))?;
        cap.capture_record_id = Some(record_id.clone());
        if cap.sealed_record_id.is_none() {
            cap.sealed_record_id = Some(record_id);
        }
        Ok(cap.info())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vstate::memory::{Bytes, GuestAddress};
    fn fixture(tracked: bool) -> (std::sync::Arc<std::sync::Mutex<Vmm>>, crate::EventManager) {
        let (vm, events) = crate::test_utils::create_vmm(None, tracked, false, false, false);
        crate::dirty_log::test_mmio(&mut vm.lock().unwrap().device_manager)
            .virtio_devices
            .clear();
        (vm, events)
    }
    fn call(m: &mut Manager, vm: &mut Vmm, d: &mut DirtyLogState, r: Resource, q: Value) -> Value {
        m.transact(vm, d, r, q, true).unwrap()
    }
    #[test]
    fn metadata_only_without_census_reads_selected_pages_and_never_resets_bits() {
        let (vm, _events) = fixture(false);
        let mut vm = vm.lock().unwrap();
        let mut m = Manager::default();
        let mut d = DirtyLogState::default();
        let arm = call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Epochs,
            json!({"operation":"arm","epoch":1,"metrics":["metadata"]}),
        );
        assert!(m.host.is_none());
        assert!(!m.reset_performed);
        assert!(!d.started());
        assert!(arm["epoch_record_id"].is_null());
        let generation = arm["vm_generation"].clone();
        let begin = call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Metadata,
            json!({"operation":"begin","capture_id":"selected","epoch":1,"vm_generation":generation,
                   "mode":"attribution","consistency":"quiescence_only","max_bytes":4096,
                   "max_scan_bytes":0,"capture_census":false}),
        );
        assert!(begin["census_record_id"].is_null());
        assert!(m.captures["selected"].census.is_none());
        assert!(m.records.records.is_empty());
        assert!(!m.reset_performed);
        let read = call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Metadata,
            json!({"operation":"read_pages","capture_id":"selected","vm_generation":generation,"gpas":[0x5000]}),
        );
        assert_eq!(read["pages"].as_array().unwrap().len(), 1);
        call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Metadata,
            json!({"operation":"seal","capture_id":"selected","vm_generation":generation}),
        );
        let rotated = call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Epochs,
            json!({"operation":"rotate","epoch":1,"next_epoch":2,"vm_generation":generation}),
        );
        assert!(rotated["access_record_id"].is_null());
        assert!(rotated["census_record_id"].is_null());
        assert!(!m.reset_performed);
        assert!(!d.started());
    }
    #[test]
    fn page_bits_do_not_publish_unrequested_census_or_capture_metadata() {
        let (vm, _events) = fixture(true);
        let mut vm = vm.lock().unwrap();
        let mut m = Manager::default();
        let mut d = DirtyLogState::default();
        let arm = call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Epochs,
            json!({"operation":"arm","epoch":1,"metrics":["dirty","idle"]}),
        );
        assert!(arm["census_record_id"].is_null());
        let result = call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Epochs,
            json!({"operation":"rotate","epoch":1,"next_epoch":2,"vm_generation":arm["vm_generation"]}),
        );
        assert!(result["access_record_id"].is_string());
        assert!(result["census_record_id"].is_null());
        assert!(m.captures.is_empty());
        assert!(m.reset_performed);
    }
    #[test]
    fn standalone_idle_requires_neither_dirty_tracking_nor_kvm_reset() {
        let (vm, _events) = fixture(false);
        let mut vm = vm.lock().unwrap();
        let mut m = Manager::default();
        let mut d = DirtyLogState::default();
        let request = json!({"operation":"arm","epoch":1,"phase":"tool"});
        assert!(
            m.transact(&mut vm, &mut d, Resource::Idle, request.clone(), false)
                .is_err()
        );
        assert!(m.host.is_none());
        let arm = call(&mut m, &mut vm, &mut d, Resource::Idle, request);
        assert!(!d.started());
        let generation = arm["vm_generation"].as_str().unwrap().to_string();
        let wrong = m
            .transact(
                &mut vm,
                &mut d,
                Resource::Idle,
                json!({"operation":"collect","epoch":1,"next_epoch":2,"vm_generation":"stale"}),
                true,
            )
            .unwrap_err();
        assert_eq!(wrong.0["error"]["mutation_performed"], false);
        let result = call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Idle,
            json!({"operation":"collect","epoch":1,"next_epoch":2,"phase":"gap","vm_generation":generation}),
        );
        assert!(result["access_record_id"].is_string());
        assert!(!d.started());
    }
    #[test]
    fn census_fresh_capture_and_last_request_retry_are_distinct() {
        let (vm, _events) = fixture(false);
        let mut vm = vm.lock().unwrap();
        let mut m = Manager::default();
        let mut d = DirtyLogState::default();
        let first = call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Census,
            json!({"operation":"capture"}),
        );
        let generation = first["vm_generation"].as_str().unwrap().to_string();
        let request =
            json!({"operation":"capture","request_id":"stable","vm_generation":generation});
        let historical = call(&mut m, &mut vm, &mut d, Resource::Census, request.clone());
        vm.vm
            .as_kvm()
            .unwrap()
            .guest_memory()
            .write_slice(&[3u8; 4096], GuestAddress(0x6000))
            .unwrap();
        let retry = call(&mut m, &mut vm, &mut d, Resource::Census, request);
        assert_eq!(historical, retry);
        let fresh = call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Census,
            json!({"operation":"capture"}),
        );
        assert_ne!(historical["record_id"], fresh["record_id"]);
        assert_ne!(
            historical["metadata"]["inventory_begin_ns"],
            fresh["metadata"]["inventory_begin_ns"]
        );
        assert!(!d.started());
        let stale = m
            .transact(
                &mut vm,
                &mut d,
                Resource::Census,
                json!({"operation":"capture","vm_generation":"stale"}),
                true,
            )
            .unwrap_err();
        assert_eq!(stale.0["error"]["mutation_performed"], false);
    }
    #[test]
    fn metadata_first_copy_history_and_stale_ack_are_safe() {
        let (vm, _events) = fixture(false);
        let mut vm = vm.lock().unwrap();
        vm.vm
            .as_kvm()
            .unwrap()
            .guest_memory()
            .write_slice(&[7u8; 4096], GuestAddress(0x5000))
            .unwrap();
        let mut m = Manager::default();
        let mut d = DirtyLogState::default();
        let begin = call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Metadata,
            json!({"operation":"begin","capture_id":"old","mode":"attribution","consistency":"quiescence_only","max_bytes":4096,"max_scan_bytes":0}),
        );
        let generation = begin["vm_generation"].as_str().unwrap().to_string();
        assert!(!m.host_idle);
        assert!(!d.started());
        assert!(m.blocks_resume());
        let blocked = m
            .transact(
                &mut vm,
                &mut d,
                Resource::Census,
                json!({"operation":"capture"}),
                true,
            )
            .unwrap_err();
        assert_eq!(blocked.0["error"]["mutation_performed"], false);
        let query = json!({"operation":"read_pages","capture_id":"old","vm_generation":generation,"gpas":[0x5000]});
        let first = call(&mut m, &mut vm, &mut d, Resource::Metadata, query.clone());
        vm.vm
            .as_kvm()
            .unwrap()
            .guest_memory()
            .write_slice(&[9u8; 4096], GuestAddress(0x5000))
            .unwrap();
        // Interleave info so the second read exercises the first-copy cache, not request cache.
        call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Metadata,
            json!({"operation":"info","capture_id":"old","vm_generation":generation}),
        );
        let second = call(&mut m, &mut vm, &mut d, Resource::Metadata, query);
        assert_eq!(
            first["pages"][0]["data_b64"],
            second["pages"][0]["data_b64"]
        );
        assert_eq!(second["pages"][0]["cache_hit"], true);
        let seal = call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Metadata,
            json!({"operation":"seal","capture_id":"old","vm_generation":generation}),
        );
        assert!(!m.blocks_resume());
        let id = seal["capture_record_id"].as_str().unwrap().to_string();
        call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Metadata,
            json!({"operation":"begin","capture_id":"new","mode":"attribution","consistency":"quiescence_only","max_bytes":4096,"max_scan_bytes":0}),
        );
        let info = call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Metadata,
            json!({"operation":"info","capture_id":"old","vm_generation":generation}),
        );
        assert_eq!(info["consistency"], "quiescence_only");
        let exported = call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Metadata,
            json!({"operation":"export","record_id":id,"kind":"pages","offset":0,"limit":1}),
        );
        assert_eq!(
            exported["rows"][0]["data_b64"],
            first["pages"][0]["data_b64"]
        );
        let count = m.records.records.len();
        let bad = m
            .transact(
                &mut vm,
                &mut d,
                Resource::Metadata,
                json!({"operation":"ack","record_id":id,"vm_generation":"wrong-vm"}),
                true,
            )
            .unwrap_err();
        assert_eq!(bad.0["error"]["mutation_performed"], false);
        assert_eq!(m.records.records.len(), count);
        call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Metadata,
            json!({"operation":"ack","record_id":id,"vm_generation":generation}),
        );
        assert!(m.captures.contains_key("old"));
        assert!(m.captures["old"].snapshot.is_none());
        let historical = call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Metadata,
            json!({"operation":"info","capture_id":"old","vm_generation":generation}),
        );
        assert_eq!(historical["consistency"], "quiescence_only");
        assert!(m.transact(&mut vm,&mut d,Resource::Metadata,json!({"operation":"abort","capture_id":"old","vm_generation":generation,"reason":"already final"}),true).is_err());
        call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Metadata,
            json!({"operation":"ack","record_id":begin["census_record_id"],"vm_generation":generation}),
        );
        assert!(!m.captures.contains_key("old"));
    }
    #[test]
    fn dirty_capture_validation_and_reset_ownership() {
        let (vm, _events) = fixture(true);
        let mut vm = vm.lock().unwrap();
        let mut m = Manager::default();
        let mut d = DirtyLogState::default();
        let arm = call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Epochs,
            json!({"operation":"arm","epoch":1,"phase":"bootstrap","metrics":["dirty","metadata"]}),
        );
        let generation = arm["vm_generation"].as_str().unwrap().to_string();
        assert!(m.host.is_none());
        assert!(m.guard_independent_dirty().is_err());
        call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Metadata,
            json!({"operation":"begin","capture_id":"cap","epoch":1,"vm_generation":generation,"mode":"bootstrap","consistency":"dirty_validated","max_bytes":4096,"max_scan_bytes":0}),
        );
        call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Metadata,
            json!({"operation":"read_pages","capture_id":"cap","vm_generation":generation,"gpas":[0x5000]}),
        );
        let open = m
            .transact(
                &mut vm,
                &mut d,
                Resource::Epochs,
                json!({"operation":"rotate","epoch":1,"next_epoch":2,"vm_generation":generation}),
                true,
            )
            .unwrap_err();
        assert_eq!(open.0["error"]["reset_performed"], false);
        let seal = call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Metadata,
            json!({"operation":"seal","capture_id":"cap","vm_generation":generation}),
        );
        assert!(m.blocks_resume());
        let pending = seal["capture_record_id"].clone();
        vm.vm
            .as_kvm()
            .unwrap()
            .guest_memory()
            .write_obj(1u8, GuestAddress(0x5000))
            .unwrap();
        let rotated = call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Epochs,
            json!({"operation":"rotate","epoch":1,"next_epoch":2,"phase":"tool","vm_generation":generation}),
        );
        assert_eq!(rotated["capture_validation"][0]["consistency"], "invalid");
        assert!(!m.blocks_resume());
        let info = call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Metadata,
            json!({"operation":"info","capture_id":"cap","vm_generation":generation}),
        );
        assert_ne!(info["capture_record_id"], pending);
        assert_eq!(info["sealed_record_id"], pending);
        assert_eq!(
            m.records.records[pending.as_str().unwrap()].metadata["consistency"],
            "pending"
        );
    }
    #[test]
    fn poison_rejects_cached_mutation_but_discard_allows_resume() {
        let (vm, _events) = fixture(true);
        let mut vm = vm.lock().unwrap();
        let mut m = Manager::default();
        let mut d = DirtyLogState::default();
        let initial =
            json!({"operation":"arm","epoch":1,"metrics":["dirty","metadata"],"phase":"gap"});
        let arm = call(&mut m, &mut vm, &mut d, Resource::Epochs, initial.clone());
        let generation = arm["vm_generation"].as_str().unwrap().to_string();
        call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Metadata,
            json!({"operation":"begin","capture_id":"failed","epoch":1,"vm_generation":generation,"mode":"attribution","consistency":"dirty_validated","max_bytes":4096,"max_scan_bytes":0}),
        );
        let sealed = call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Metadata,
            json!({"operation":"seal","capture_id":"failed","vm_generation":generation}),
        );
        // SAFETY: this fixture never runs a vCPU; force a destructive-read failure.
        unsafe {
            vm.vm
                .as_kvm()
                .unwrap()
                .fd()
                .set_user_memory_region(kvm_bindings::kvm_userspace_memory_region {
                    slot: 0,
                    memory_size: 0,
                    ..Default::default()
                })
                .unwrap();
        }
        let failure = m
            .transact(
                &mut vm,
                &mut d,
                Resource::Epochs,
                json!({"operation":"rotate","epoch":1,"next_epoch":2,"vm_generation":generation}),
                true,
            )
            .unwrap_err();
        assert_eq!(failure.0["error"]["poisoned"], true);
        assert!(m.blocks_resume());
        // Specifically exercise a previously cached successful mutating response.
        m.last = Some((Resource::Epochs, initial.clone(), arm));
        assert!(
            m.transact(&mut vm, &mut d, Resource::Epochs, initial, true)
                .is_err()
        );
        let discarded = call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Metadata,
            json!({"operation":"abort","capture_id":"failed","vm_generation":generation,"reason":"failed coordinator rotation"}),
        );
        assert_eq!(discarded["consistency"], "discarded");
        assert_eq!(discarded["sealed_record_id"], sealed["capture_record_id"]);
        assert_ne!(discarded["capture_record_id"], sealed["capture_record_id"]);
        assert!(!m.blocks_resume());
        assert!(m.poisoned.is_some());
    }
    #[test]
    fn eight_gib_capacity_plan_fits_bounded_default() {
        let mem = crate::test_utils::arch_mem(8usize * 1024 * 1024 * 1024);
        let host = HostPages::capacity_preflight(&mem).unwrap();
        let capture = Capture::reservation_for(256 * 1024 * 1024, 1024 * 1024 * 1024).unwrap();
        let retained = 128 * 1024 * 1024usize;
        assert!(host + capture + retained < MAX_RETAINED_BYTES);
        assert!(host + capture + retained > 512 * 1024 * 1024);
    }
    #[test]
    fn capture_begin_reserves_close_slots_before_side_effects() {
        let (vm, _events) = fixture(false);
        let mut vm = vm.lock().unwrap();
        let mut m = Manager::default();
        let mut d = DirtyLogState::default();
        // Fill immutable records through the public census API, keeping zero captures.
        for _ in 0..7 {
            call(
                &mut m,
                &mut vm,
                &mut d,
                Resource::Census,
                json!({"operation":"capture"}),
            );
        }
        let result=m.transact(&mut vm,&mut d,Resource::Metadata,json!({"operation":"begin","capture_id":"no-room","mode":"attribution","consistency":"quiescence_only","max_bytes":4096,"max_scan_bytes":0}),true).unwrap_err();
        assert_eq!(result.0["error"]["code"], "backpressure");
        assert_eq!(result.0["error"]["mutation_performed"], false);
        assert!(m.captures.is_empty());
        let id = m.records.records.keys().next().unwrap().clone();
        let generation = m.generation.clone();
        call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Census,
            json!({"operation":"ack","record_id":id,"vm_generation":generation}),
        );
        call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Metadata,
            json!({"operation":"begin","capture_id":"room","mode":"attribution","consistency":"quiescence_only","max_bytes":4096,"max_scan_bytes":0}),
        );
        let aborted = call(
            &mut m,
            &mut vm,
            &mut d,
            Resource::Metadata,
            json!({"operation":"abort","capture_id":"room","vm_generation":generation,"reason":"cleanup"}),
        );
        assert_eq!(aborted["state"], "aborted");
        assert!(!m.blocks_resume());
    }
}
