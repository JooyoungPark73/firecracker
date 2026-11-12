// SPDX-License-Identifier: Apache-2.0

//! Paused, continuous dirty epochs for the ARIES measurement build.
use crate::Vmm;
use crate::devices::virtio::block::device::Block;
use crate::devices::virtio::device::{VirtioDevice, VirtioDeviceType};
use crate::devices::virtio::net::Net;
use crate::vmm_config::instance_info::VmState;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vm_memory::bitmap::Bitmap;
use vm_memory::{Address, GuestMemoryBackend, GuestMemoryRegion};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// One idempotent request to arm or rotate a measurement epoch.
pub struct DirtyLogRequest {
    /// Initial arm or collect-and-optionally-rotate.
    pub operation: String,
    /// Outgoing epoch identifier; the first arm is one.
    pub epoch: u64,
    #[serde(default)]
    /// Successor epoch, exactly outgoing plus one; absence closes tracking.
    pub next_epoch: Option<u64>,
}
#[derive(Debug, Default)]
/// Per-VM epoch state; destructive failures permanently poison this instance.
pub struct DirtyLogState {
    active: Option<u64>,
    last: Option<(DirtyLogRequest, Value)>,
    poisoned: bool,
    lifecycle_failure: Option<String>,
}
fn packed(words: &[u64]) -> String {
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
impl DirtyLogState {
    /// Whether snapshot operations could interfere with dirty measurement.
    pub fn started(&self) -> bool {
        self.last.is_some() || self.poisoned
    }
    /// A failed native lifecycle transition makes subsequent epoch evidence unsafe.
    pub fn invalidate_lifecycle(&mut self, reason: String) {
        self.poisoned = true;
        self.lifecycle_failure = Some(format!("native lifecycle uncertain: {reason}; restart VM"));
    }
    /// Native transition uncertainty is distinct from bitmap collection failure.
    pub fn lifecycle_failure(&self) -> Option<&str> {
        self.lifecycle_failure.as_deref()
    }
    /// Replay only the most recent immutable successful response, without live access.
    pub fn cached_reply(&self, req: &DirtyLogRequest) -> Option<Value> {
        if self.poisoned {
            return None;
        }
        self.last
            .as_ref()
            .and_then(|(prior, value)| (prior == req).then(|| value.clone()))
    }
    /// Collect under HTTP held-pause, drain async block, and rotate both maps.
    pub fn transact(&mut self, vmm: &mut Vmm, req: DirtyLogRequest) -> Result<Value, String> {
        if let Some(reason) = &self.lifecycle_failure {
            return Err(reason.clone());
        }
        if self.poisoned {
            return Err("dirty epoch invalidated by prior collection failure; restart VM".into());
        }
        if let Some((prior, value)) = &self.last {
            if *prior == req {
                return Ok(value.clone());
            }
        }
        if vmm.instance_info.state != VmState::Paused {
            return Err("dirty-log requires paused vCPUs".into());
        }
        match req.operation.as_str() {
            "arm"
                if self.active.is_none()
                    && self.last.is_none()
                    && req.epoch == 1
                    && req.next_epoch.is_none() =>
            {
                ()
            }
            "collect"
                if self.active == Some(req.epoch)
                    && req
                        .next_epoch
                        .is_none_or(|n| req.epoch.checked_add(1) == Some(n)) =>
            {
                ()
            }
            _ => return Err("invalid dirty epoch transition".into()),
        }
        if vmm
            .vm
            .as_kvm()
            .ok_or("tracing requires a KVM VM")?
            .guest_memory()
            .iter()
            .any(|r| r.bitmap().is_none())
        {
            return Err(
                "track_dirty_pages must be enabled from boot; mincore is not dirty evidence".into(),
            );
        }
        let begin = utils::time::get_time_us(utils::time::ClockType::Monotonic);
        // The runtime API executes on the VMM event loop. vCPUs are stopped;
        // synchronous devices cannot run concurrently. Drain asynchronous block
        // reads before sampling/resetting the host-maintained bitmap.
        let quiesce = quiesce_devices(vmm);
        if let Err(e) = quiesce {
            self.poisoned = true;
            return Err(e);
        }
        let mem = vmm
            .vm
            .as_kvm()
            .ok_or("tracing requires a KVM VM")?
            .guest_memory();
        let page_bytes = crate::arch::host_page_size();
        if page_bytes != 4096 {
            return Err("dirty measurement requires 4096-byte host pages".into());
        }
        let mut regions = Vec::new();
        for (slot, region) in mem.iter().enumerate() {
            let mut cpu = match vmm
                .vm
                .as_kvm()
                .ok_or("tracing requires a KVM VM")?
                .fd()
                .get_dirty_log(slot as u32, region.len() as usize)
            {
                Ok(bits) => bits,
                Err(e) => {
                    self.poisoned = true;
                    return Err(format!(
                        "KVM_GET_DIRTY_LOG failed after possible partial reset: {e}"
                    ));
                }
            };
            let pages = (region.len() as usize).div_ceil(page_bytes);
            let mut device = vec![0u64; pages.div_ceil(64)];
            for page in 0..pages {
                if region.bitmap().dirty_at(page * page_bytes) {
                    device[page / 64] |= 1u64 << (page % 64);
                }
            }
            if pages % 64 != 0 {
                if let Some(last) = cpu.last_mut() {
                    *last &= (1u64 << (pages % 64)) - 1;
                }
            }
            if cpu.len() != device.len() {
                self.poisoned = true;
                return Err("KVM/VMM bitmap word count mismatch".into());
            }
            let union: Vec<u64> = cpu.iter().zip(&device).map(|(a, b)| a | b).collect();
            regions.push(json!({
                "slot": slot, "guest_base": region.start_addr().raw_value(),
                "bytes": region.len(), "page_count": pages,
                "kvm_dirty_b64": packed(&cpu), "vmm_dirty_b64": packed(&device),
                "combined_dirty_b64": packed(&union),
                "kvm_dirty_pages": cpu.iter().map(|w| w.count_ones() as u64).sum::<u64>(),
                "vmm_dirty_pages": device.iter().map(|w| w.count_ones() as u64).sum::<u64>(),
                "combined_dirty_pages": union.iter().map(|w| w.count_ones() as u64).sum::<u64>()
            }));
        }
        use crate::vstate::memory::GuestMemoryExtension;
        mem.reset_dirty();
        self.active = if req.operation == "arm" {
            Some(req.epoch)
        } else {
            req.next_epoch
        };
        let value = json!({
            "schema":"aries.firecracker.dirty.v1", "operation":req.operation,
            "epoch":req.epoch, "next_epoch":req.next_epoch, "active_epoch":self.active,
            "boot_baseline_discarded":req.operation=="arm", "page_bytes":page_bytes,
            "bitmap_encoding":"base64-packed-u64-little-endian-lsb-first",
            "vmm_semantics":"conservative potential writes; writable descriptor capacity may be marked before IO",
            "begin_monotonic_us":begin,
            "end_monotonic_us":utils::time::get_time_us(utils::time::ClockType::Monotonic),
            "vcpu_paused":true, "device_quiescence":"http-held-pause-event-loop-frozen-and-checked-block-drain",
            "regions":regions
        });
        self.last = Some((req, value.clone()));
        Ok(value)
    }
}

/// Stop outstanding supported writers inside the HTTP-held pause loop.
/// This does not require KVM dirty logging and is reused by independent captures.
pub(crate) fn quiesce_devices(vmm: &mut Vmm) -> Result<(), String> {
    if vmm.instance_info.state != VmState::Paused {
        return Err("requires HTTP-paused VM".into());
    }
    let crate::device_manager::VirtioDevices::Mmio(mmio) = &mut vmm.device_manager.virtio_devices
    else {
        return Err("non-MMIO quiescence is not validated".into());
    };
    mmio.virtio_devices
        .iter()
        .try_for_each(|((kind, _), entry)| {
            let transport = entry
                .inner
                .lock()
                .map_err(|_| "transport lock poisoned".to_string())?;
            let mut dev = transport.locked_device();
            match *kind {
                VirtioDeviceType::Block => {
                    let block = dev
                        .as_mut_any()
                        .downcast_mut::<Block>()
                        .ok_or("unknown block implementation")?;
                    match block {
                        Block::Virtio(b) => b.quiesce_dirty_log()?,
                        Block::VhostUser(_) => {
                            return Err("external vhost-user writer unsupported".into());
                        }
                    }
                }
                VirtioDeviceType::Net => {
                    dev.as_mut_any()
                        .downcast_mut::<Net>()
                        .ok_or("unknown net implementation")?
                        .prepare_save();
                }
                VirtioDeviceType::Vsock => (),
                _ => return Err(format!("unvalidated virtio writer type {kind:?}")),
            }
            Ok::<(), String>(())
        })
}

#[cfg(test)]
pub(crate) fn test_mmio(
    manager: &mut crate::device_manager::DeviceManager,
) -> &mut crate::device_manager::mmio::MMIOVirtioDevices {
    let crate::device_manager::VirtioDevices::Mmio(mmio) = &mut manager.virtio_devices else {
        panic!("expected MMIO test fixture")
    };
    mmio
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vstate::memory::{Bytes, GuestAddress};
    use kvm_ioctls::VcpuExit;
    fn request(operation: &str, epoch: u64, next_epoch: Option<u64>) -> DirtyLogRequest {
        DirtyLogRequest {
            operation: operation.into(),
            epoch,
            next_epoch,
        }
    }
    fn has(value: &Value, source: &str, page: usize) -> bool {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(value["regions"][0][source].as_str().unwrap())
            .unwrap();
        raw[page / 8] & (1 << (page % 8)) != 0
    }
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn exact_cpu_write_restore_vmm_and_rotate() {
        let (vm, _events) = crate::test_utils::create_vmm(None, true, false, false, false);
        let mut vm = vm.lock().unwrap();
        crate::dirty_log::test_mmio(&mut vm.device_manager)
            .virtio_devices
            .retain(|(kind, _), _| {
                !matches!(kind, VirtioDeviceType::Balloon | VirtioDeviceType::Rng)
            });
        let mut cpu = vm.vm.as_kvm().unwrap().fd().create_vcpu(7).unwrap();
        cpu.set_mp_state(kvm_bindings::kvm_mp_state {
            mp_state: kvm_bindings::KVM_MP_STATE_RUNNABLE,
        })
        .unwrap();
        let mut sr = cpu.get_sregs().unwrap();
        sr.cs.base = 0;
        sr.cs.selector = 0;
        cpu.set_sregs(&sr).unwrap();
        cpu.set_regs(&kvm_bindings::kvm_regs {
            rip: 0x1000,
            rflags: 2,
            ..Default::default()
        })
        .unwrap();
        let code = [
            0xa0, 0x00, 0x30, 0xc6, 0x06, 0x00, 0x40, 0x55, 0xc6, 0x06, 0x00, 0x50, 0x33, 0xc6,
            0x06, 0x00, 0x50, 0x00, 0xe6, 0xe9,
        ];
        vm.vm
            .as_kvm()
            .unwrap()
            .guest_memory()
            .write_slice(&code, GuestAddress(0x1000))
            .unwrap();
        vm.vm
            .as_kvm()
            .unwrap()
            .guest_memory()
            .write_obj(0u8, GuestAddress(0x5000))
            .unwrap();
        let mut log = DirtyLogState::default();
        log.transact(&mut vm, request("arm", 1, None)).unwrap();
        match cpu.run().unwrap() {
            VcpuExit::IoOut(0xe9, _) => (),
            other => panic!("{other:?}"),
        }
        vm.vm
            .as_kvm()
            .unwrap()
            .guest_memory()
            .write_obj(42u8, GuestAddress(0x6000))
            .unwrap();
        vm.vm
            .as_kvm()
            .unwrap()
            .guest_memory()
            .write_obj(0u8, GuestAddress(0x6000))
            .unwrap();
        let r = log
            .transact(&mut vm, request("collect", 1, Some(2)))
            .unwrap();
        assert!(!has(&r, "kvm_dirty_b64", 3));
        assert!(has(&r, "kvm_dirty_b64", 4));
        assert!(has(&r, "kvm_dirty_b64", 5));
        assert!(!has(&r, "kvm_dirty_b64", 6));
        assert!(has(&r, "vmm_dirty_b64", 6));
        assert!(has(&r, "combined_dirty_b64", 4));
        assert!(has(&r, "combined_dirty_b64", 6));
        assert_eq!(
            vm.vm
                .as_kvm()
                .unwrap()
                .guest_memory()
                .read_obj::<u8>(GuestAddress(0x5000))
                .unwrap(),
            0
        );
        assert_eq!(
            r,
            log.transact(&mut vm, request("collect", 1, Some(2)))
                .unwrap()
        );
        let clean = log
            .transact(&mut vm, request("collect", 2, Some(3)))
            .unwrap();
        assert_eq!(clean["regions"][0]["combined_dirty_pages"], 0);
        assert!(
            log.transact(&mut vm, request("collect", 2, Some(3)))
                .is_ok()
        );
        assert!(log.transact(&mut vm, request("collect", 2, None)).is_err());
    }
    #[test]
    fn rejects_epoch_gaps_and_closure_but_replays_last_reply() {
        let (vm, _events) = crate::test_utils::create_vmm(None, true, false, false, false);
        let mut vm = vm.lock().unwrap();
        crate::dirty_log::test_mmio(&mut vm.device_manager)
            .virtio_devices
            .retain(|(kind, _), _| {
                !matches!(kind, VirtioDeviceType::Balloon | VirtioDeviceType::Rng)
            });
        let mut log = DirtyLogState::default();
        for req in [
            request("arm", 0, None),
            request("collect", 1, Some(2)),
            request("arm", 1, Some(2)),
        ] {
            assert!(log.transact(&mut vm, req).is_err());
        }
        log.transact(&mut vm, request("arm", 1, None)).unwrap();
        for req in [
            request("collect", 1, Some(3)),
            request("collect", 2, None),
            request("arm", 1, None),
        ] {
            // An exact arm retry remains idempotent; a changed transition does not.
            if req.operation == "arm" {
                assert!(log.transact(&mut vm, req).is_ok());
            } else {
                assert!(log.transact(&mut vm, req).is_err());
            }
        }
        let closed = log.transact(&mut vm, request("collect", 1, None)).unwrap();
        assert!(log.transact(&mut vm, request("arm", 1, None)).is_err());
        assert!(log.transact(&mut vm, request("collect", 2, None)).is_err());
        vm.instance_info.state = VmState::Running;
        assert_eq!(
            closed,
            log.transact(&mut vm, request("collect", 1, None)).unwrap()
        );
        vm.instance_info.state = VmState::Paused;
        // Directly exercise the otherwise unreachable-in-practice u64 boundary.
        log.active = Some(u64::MAX);
        log.last = None;
        assert!(
            log.transact(&mut vm, request("collect", u64::MAX, Some(0)))
                .is_err()
        );
    }

    #[test]
    fn kvm_sampling_failure_permanently_poisons_collector() {
        let (vm, _events) = crate::test_utils::create_vmm(None, true, false, false, false);
        let mut vm = vm.lock().unwrap();
        crate::dirty_log::test_mmio(&mut vm.device_manager)
            .virtio_devices
            .retain(|(kind, _), _| {
                !matches!(kind, VirtioDeviceType::Balloon | VirtioDeviceType::Rng)
            });
        let mut log = DirtyLogState::default();
        log.transact(&mut vm, request("arm", 1, None)).unwrap();
        // SAFETY: no vCPU is running; unregister this fixture-only RAM slot to
        // make KVM_GET_DIRTY_LOG fail without changing the owned guest mapping.
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
        let error = log
            .transact(&mut vm, request("collect", 1, Some(2)))
            .unwrap_err();
        assert!(error.contains("KVM_GET_DIRTY_LOG failed"));
        assert!(
            log.transact(&mut vm, request("collect", 1, Some(2)))
                .unwrap_err()
                .contains("invalidated by prior")
        );
    }

    #[test]
    fn rejects_missing_tracking_and_running_vcpu_state() {
        let (vm, _events) = crate::test_utils::create_vmm(None, false, false, false, false);
        let mut vm = vm.lock().unwrap();
        let mut log = DirtyLogState::default();
        assert!(
            log.transact(&mut vm, request("arm", 1, None))
                .unwrap_err()
                .contains("track_dirty_pages")
        );
        vm.instance_info.state = VmState::Running;
        assert!(
            log.transact(&mut vm, request("arm", 1, None))
                .unwrap_err()
                .contains("paused")
        );
    }
}
