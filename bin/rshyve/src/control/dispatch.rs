// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Command dispatch: maps a decoded command to its handler.

use std::sync::Arc;

use slog::error;
use vmm_core::hdl::SuspendHow;

use vmm_machine::inventory::{
    cpu_inventory, MemWindowReport, CPU_HOTPLUG_OFF, MEM_HOTPLUG_OFF,
    NO_CPU_REMOVE, NO_MEM_REMOVE,
};
use vmm_machine::{
    CpuHotplugError, CpuSlots, DeviceRegistry, HotplugError, MemHotplugEngine,
    SlotState,
};

use super::migrate::{
    migrate_cancel, migrate_config, migrate_dest, migrate_resume_source,
    migrate_source, migrate_status, migration_source_guard, SourceAsk,
};
use super::protocol::{Command, Response};
use super::run_state::{
    ChangeFailed, StepFailed, CHANGE_IN_PROGRESS, IMPORT_FAILED, IN_DOUBT,
};
use super::{
    park_all, unpark_all, StopFailure, VmController, VmRunState,
    DEVICE_PAUSE_BUDGET,
};

pub(super) fn dispatch(ctrl: &Arc<VmController>, cmd: Command) -> Response {
    if let Some(response) =
        migration_source_guard(ctrl.host_local_state, ctrl.has_ahci_cd, &cmd)
    {
        return response;
    }

    match cmd {
        Command::Status => {
            let mut r = Response::ok();
            r.state = Some(ctrl.state());
            r.vm_name = Some(ctrl.vm_name.clone());
            r.num_cpus = Some(ctrl.num_cpus);
            r.memory_bytes = Some(ctrl.mem_size);
            r.uptime_secs = Some(ctrl.start_time.elapsed().as_secs());
            let rings = ctrl.hdl.rings();
            r.data = status_data(
                ctrl.release_stuck(),
                rings.tainted(),
                rings.leaks(),
            );
            r
        }

        Command::Pause => match ctrl.run.change(
            VmRunState::Running,
            VmRunState::Paused,
            |held| pause_step(ctrl, held),
        ) {
            Ok(()) => {
                let mut r = Response::ok();
                r.state = Some(VmRunState::Paused);
                r
            }
            Err(ChangeFailed::WrongState) => Response::err("VM is not running"),
            Err(ChangeFailed::Step(why) | ChangeFailed::Faulted(why)) => {
                Response::err(&why)
            }
        },

        Command::Resume { force } => resume(ctrl, force),

        Command::Shutdown => guest_stop(ctrl, SuspendHow::PowerOff, "shutdown"),

        Command::Reset => guest_stop(ctrl, SuspendHow::Reset, "reset"),

        Command::Stop => latch_stop(
            ctrl,
            SuspendHow::Halt,
            "stop",
            |state| stop_refusal(state, ctrl.migrated_away()).is_none(),
            "VM is already stopped",
        ),

        Command::MigrateSource {
            target_addr,
            zfs_barrier,
            gate,
            gate_timeout_ms,
            allow_engine_without_migrate_v1,
        } => migrate_source(
            ctrl,
            target_addr,
            SourceAsk {
                zfs_barrier,
                gate,
                gate_timeout_ms,
                allow_unproven: allow_engine_without_migrate_v1,
            },
        ),

        Command::MigrateDest {
            listen_addr,
            allow_cpu_feature_mismatch,
        } => migrate_dest(ctrl, listen_addr, allow_cpu_feature_mismatch),

        Command::MigrateResumeSource => migrate_resume_source(ctrl),

        Command::MigrateCancel => migrate_cancel(ctrl),

        // The listener answers both of these itself, as raw text rather
        // than a typed Response. This arm is what a listener that
        // stopped doing so would fall back to.
        Command::Metrics | Command::MetricsPrometheus => {
            let mut r = Response::ok();
            r.state = Some(ctrl.state());
            r
        }

        Command::MigrateStatus => migrate_status(ctrl),

        Command::MigrateConfig => migrate_config(ctrl),

        Command::DeviceList => Response::ok_data(device_list(&ctrl.registry)),

        // SECURITY: device-add takes a host path from a socket peer and
        // opens it. That is the same file-open authority argv has, and
        // it is deliberately not wider: the spec goes through the same
        // catalog closure the `-s` pass uses. The peercred check in
        // `listener` is the only thing that grants it, so the peer must
        // share this process's uid and be in this zone or the global
        // zone.
        Command::DeviceAdd { spec } => match ctrl.hotplug.pci.as_ref() {
            Some(engine) => hot_add(ctrl, || added(engine.add_device(&spec))),
            None => hotplug_disabled(),
        },

        // Advisory: the device leaves when the guest runs _EJ0, and
        // never before.
        Command::DeviceRemove { id } => match ctrl.hotplug.pci.as_ref() {
            Some(engine) => {
                hot_add(ctrl, || removing(&id, engine.request_remove(&id)))
            }
            None => hotplug_disabled(),
        },

        Command::CpuList => Response::ok_data(cpu_list(
            ctrl.num_cpus,
            ctrl.max_cpus,
            ctrl.hotplug.cpu.as_ref(),
        )),

        // The id was decoded as a u32, so nothing here can be negative
        // or fractional, and it is never used as an index: the engine
        // range-checks it against the slots the tables describe.
        Command::CpuAdd { id } => match ctrl.hotplug.cpu.as_ref() {
            Some(cpus) => hot_add(ctrl, || cpu_added(id, cpus.add(id))),
            None => Response::err(CPU_HOTPLUG_OFF),
        },

        Command::MemList => Response::ok_data(mem_list(
            ctrl.mem_size,
            ctrl.hotplug.mem.as_ref(),
        )),

        Command::MemAdd { bytes } => match ctrl.hotplug.mem.as_ref() {
            Some(engine) => {
                hot_add(ctrl, || mem_added(engine.add_memory(bytes)))
            }
            None => Response::err(MEM_HOTPLUG_OFF),
        },

        // Neither removal is possible at the kernel boundary. Saying so
        // is the whole point of accepting the command.
        Command::CpuRemove => Response::err(NO_CPU_REMOVE),
        Command::MemRemove => Response::err(NO_MEM_REMOVE),
    }
}

/// Run one hot-add, if the VM is in a state that can take it.
///
/// The topology lock is held for the whole add. `migrate-source` takes
/// it after it claims the state, and `pause` before it, so an add
/// either finishes before either reads the device set or finds the VM
/// no longer running and is refused.
fn hot_add(ctrl: &VmController, add: impl FnOnce() -> Response) -> Response {
    let _topology = ctrl.run.lock_topology();
    match hot_add_blocked(ctrl.state()) {
        Some(refusal) => refusal,
        None => add(),
    }
}

fn pause_step(
    ctrl: &VmController,
    held: &mut bool,
) -> Result<(), StepFailed<String>> {
    park_all(
        &ctrl.named_devices(),
        DEVICE_PAUSE_BUDGET,
        || ctrl.hold_pause(),
        held,
        &ctrl.log,
    )
}

/// How long an operator resume waits for a detached ring disk.
const RESUME_ATTACH_WAIT: std::time::Duration =
    std::time::Duration::from_secs(30);

fn resume_step(
    ctrl: &VmController,
    held: &mut bool,
) -> Result<(), StepFailed<String>> {
    // The disks attached before the change began; one that went away
    // since keeps the guest paused.
    if ctrl
        .named_devices()
        .iter()
        .any(|d| !d.device.backing_attached())
    {
        return Err(StepFailed::Undone(
            "a disk is not attached; resume again".to_string(),
        ));
    }
    unpark_all(
        &ctrl.named_devices(),
        DEVICE_PAUSE_BUDGET,
        || ctrl.release_pause(),
        held,
        &ctrl.log,
    )
}

/// The state a resume starts from, or why there is none.
///
/// A VM whose import is in doubt resumes only with `force`, once the
/// operator has made sure the source did not keep the guest. One whose
/// source committed resumes as a paused VM does.
fn resume_from(
    state: VmRunState,
    force: bool,
) -> Result<VmRunState, &'static str> {
    match state {
        VmRunState::ImportCommitted => Ok(state),
        VmRunState::ImportInDoubt if force => Ok(state),
        VmRunState::ImportInDoubt => Err(IN_DOUBT),
        VmRunState::ImportFailed => Err(IMPORT_FAILED),
        _ => Ok(VmRunState::Paused),
    }
}

fn resume(ctrl: &VmController, force: bool) -> Response {
    let from = match resume_from(ctrl.state(), force) {
        Ok(from) => from,
        Err(why) => return Response::err(why),
    };
    // An import whose disks did not attach after the commit holds the
    // only copy of the guest; it runs only once they are back. The
    // attach can take its whole wait, so it runs before the state
    // change, which a stop also needs. Only a paused VM attaches: a
    // held or migrated-away source must not take its disks back here.
    let paused = matches!(
        ctrl.state(),
        VmRunState::Paused
            | VmRunState::ImportCommitted
            | VmRunState::ImportInDoubt
    );
    if !paused {
        return Response::err("VM is not paused");
    }
    if let Err(why) =
        ctrl.attach_backing(std::time::Instant::now() + RESUME_ATTACH_WAIT)
    {
        return Response::err(&why);
    }
    match ctrl
        .run
        .change(from, VmRunState::Running, |held| resume_step(ctrl, held))
    {
        Ok(()) => {
            let mut r = Response::ok();
            r.state = Some(VmRunState::Running);
            r
        }
        Err(ChangeFailed::WrongState) => Response::err("VM is not paused"),
        Err(ChangeFailed::Step(why) | ChangeFailed::Faulted(why)) => {
            Response::err(&why)
        }
    }
}

/// `shutdown` and `reset`, which only a running or paused guest takes.
fn guest_stop(
    ctrl: &Arc<VmController>,
    how: SuspendHow,
    what: &str,
) -> Response {
    latch_stop(
        ctrl,
        how,
        what,
        |state| matches!(state, VmRunState::Running | VmRunState::Paused),
        "VM is not running or paused",
    )
}

/// Latch a stop and report what happened.
///
/// One helper for `shutdown`, `reset` and `stop`: the three differ only
/// in the `SuspendHow` they latch, and three copies of the ordering had
/// already drifted from the one [`VmController::latch_stop`] documents.
fn latch_stop(
    ctrl: &Arc<VmController>,
    how: SuspendHow,
    what: &str,
    allowed: impl FnOnce(VmRunState) -> bool,
    refusal: &str,
) -> Response {
    match ctrl.latch_stop(how, allowed) {
        Ok(state) => {
            let mut r = Response::ok();
            r.state = Some(state);
            r
        }
        Err(StopFailure::Busy) => Response::err(CHANGE_IN_PROGRESS),
        Err(StopFailure::WrongState(_)) => Response::err(refusal),
        Err(StopFailure::NotLatched(e)) => {
            error!(ctrl.log, "{what} failed"; "error" => %e);
            Response::err("suspend ioctl failed")
        }
        // The guest is going to stop; it just has not been let back
        // into the kernel to see it. Saying the stop failed would send
        // the operator after the wrong thing.
        Err(StopFailure::NotReleased(e)) => {
            error!(ctrl.log, "{what} latched but the VM did not resume";
                "error" => %e);
            Response::err("the stop is latched but the VM did not resume")
        }
    }
}

/// What `status` adds when something needs an operator: a stop that
/// cannot give its pause back, a ring gate that refuses every
/// write-lock ioctl, or a device left in the kernel.
fn status_data(
    stuck: bool,
    tainted: Option<String>,
    leaks: u32,
) -> Option<serde_json::Value> {
    let mut data = serde_json::Map::new();
    if stuck {
        data.insert("release_stuck".into(), true.into());
    }
    if let Some(why) = tainted {
        data.insert("ring_gate_tainted".into(), why.into());
    }
    if leaks > 0 {
        data.insert("ring_devices_leaked".into(), leaks.into());
    }
    (!data.is_empty()).then_some(serde_json::Value::Object(data))
}

/// Why `stop` cannot run now, or `None` when it can.
///
/// A source whose guest moved to another host is left paused and
/// holding the whole guest memory, and no other command reaches that
/// state. `stop` is what releases it. A stop on `Stopping` retries a
/// release an earlier one could not make.
fn stop_refusal(
    state: VmRunState,
    migrated_away: bool,
) -> Option<&'static str> {
    match state {
        VmRunState::Stopped if !migrated_away => Some("VM is already stopped"),
        _ => None,
    }
}

/// Why a hot-add cannot run in this state.
///
/// Only a running VM may grow. A paused VM cannot enter the new vCPU
/// thread, and a migrating VM would leave the addition behind, because
/// the destination is built from the boot command line.
fn hot_add_blocked(state: VmRunState) -> Option<Response> {
    if state == VmRunState::Running {
        return None;
    }
    Some(Response::err(&format!(
        "the VM must be running to hot-add; it is {state:?}",
    )))
}

/// A hotplug request on a VM that publishes no hotplug interface.
fn hotplug_disabled() -> Response {
    Response::err(&HotplugError::Disabled.to_string())
}

/// The answer to `cpu-add`. The CPU is active in the kernel and the
/// guest has been told, but the guest still has to online it.
fn cpu_added(id: u32, result: Result<(), CpuHotplugError>) -> Response {
    match result {
        Ok(()) => Response::ok_data(serde_json::json!({
            "id": id,
            "state": "present",
        })),
        Err(e) => Response::err(&e.to_string()),
    }
}

/// The answer to `mem-add`: the slot the memory went in.
fn mem_added(result: Result<usize, vmm_machine::MemHotplugError>) -> Response {
    match result {
        Ok(slot) => Response::ok_data(serde_json::json!({ "slot": slot })),
        Err(e) => Response::err(&e.to_string()),
    }
}

/// The answer to `device-add`. The id is what `device-remove` takes.
fn added(result: Result<String, HotplugError>) -> Response {
    match result {
        Ok(id) => Response::ok_data(serde_json::json!({ "id": id })),
        Err(e) => Response::err(&e.to_string()),
    }
}

/// The answer to `device-remove`.
///
/// The state says the request was recorded, not that the device is
/// gone: only the guest can finish an eject.
fn removing(id: &str, result: Result<(), HotplugError>) -> Response {
    match result {
        Ok(()) => Response::ok_data(serde_json::json!({
            "id": id,
            "state": SlotState::RemovePending.to_string(),
        })),
        Err(e) => Response::err(&e.to_string()),
    }
}

/// Every registered device, in slot order.
///
/// Read from the registry, not from the `-s` arguments: after a hotplug
/// add or remove the two no longer agree, and the registry is the one
/// the machine runs on.
fn device_list(registry: &DeviceRegistry) -> serde_json::Value {
    let devices: Vec<serde_json::Value> = registry
        .list()
        .into_iter()
        .map(|device| {
            serde_json::json!({
                "id": device.id,
                // Null for a backend with no PCI face, such as the UEFI
                // variable store.
                "bdf": device.bdf.map(|bdf| bdf.to_string()),
                // A slot stuck in remove-pending is a guest that has not
                // run _EJ0, so the state has to be visible.
                "state": device.state.to_string(),
                "hotpluggable": device.hotpluggable,
            })
        })
        .collect();
    serde_json::json!({ "devices": devices })
}

/// Every CPU slot the machine has, with the state of each.
///
/// `num_cpus` keeps its old meaning, the boot CPU count, because a
/// client that shipped before hot-add reads it.
fn cpu_list(
    boot_cpus: u32,
    max_cpus: u32,
    cpus: Option<&CpuSlots>,
) -> serde_json::Value {
    let inv = cpu_inventory(boot_cpus, max_cpus, cpus);
    let listed: Vec<serde_json::Value> = inv
        .slots
        .iter()
        .map(|slot| {
            slot_json(slot.id, &slot.state.to_string(), &slot.kind.to_string())
        })
        .collect();
    serde_json::json!({
        "num_cpus": inv.boot_cpus,
        "max_cpus": inv.max_cpus,
        "hotplug": inv.hotplug,
        "cpus": listed,
    })
}

fn slot_json(id: u32, state: &str, kind: &str) -> serde_json::Value {
    serde_json::json!({ "id": id, "state": state, "kind": kind })
}

/// The boot memory, and the hot-add window when the VM has one.
fn mem_list(
    mem_size: usize,
    engine: Option<&Arc<MemHotplugEngine>>,
) -> serde_json::Value {
    let mut value = serde_json::json!({
        "memory_bytes": mem_size,
        "slots": [
            { "id": "boot", "bytes": mem_size, "state": "present" },
        ],
        "hotplug": engine.is_some(),
    });
    if let Some(engine) = engine {
        let window = window_json(&MemWindowReport::of(engine));
        // The value above is an object literal, so this cannot fail.
        if let Some(map) = value.as_object_mut() {
            map.insert("window".to_string(), window);
        }
    }
    value
}

/// What `mem-list` says about the hot-add window.
///
/// Split from the engine so the wire shape can be tested: every part of
/// a real engine is an ioctl on a live VM.
fn window_json(report: &MemWindowReport) -> serde_json::Value {
    serde_json::json!({
        "base": report.base,
        "bytes": report.bytes,
        "slot_bytes": report.slot_bytes,
        "slots": report.slots,
        "slots_used": report.slots_used,
        "added_bytes": report.added_bytes,
    })
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};

    use vmm_machine::{
        CpuHotplugEngine, CpuHotplugError, CpuOnline, CpuSlots, DeviceRegistry,
        HotplugError, RegisteredDevice, SlotState, VcpuError,
    };

    use super::Response;
    use super::VmRunState;
    use super::{
        added, cpu_added, cpu_list, device_list, hot_add_blocked,
        hotplug_disabled, mem_added, mem_list, removing, resume_from,
        stop_refusal, stuck_data, window_json, MemWindowReport,
        CPU_HOTPLUG_OFF, MEM_HOTPLUG_OFF, NO_CPU_REMOVE, NO_MEM_REMOVE,
    };

    /// A stand-in CPU set. Every step of a real add is an ioctl, so a
    /// control plane can only be tested against one of these.
    struct FakeCpus {
        possible: u32,
        boot: u32,
        online: Mutex<BTreeSet<u32>>,
        refuse: Option<VcpuError>,
    }

    impl FakeCpus {
        fn new(possible: u32, boot: u32) -> Arc<Self> {
            Arc::new(Self {
                possible,
                boot,
                online: Mutex::new((0..boot).collect()),
                refuse: None,
            })
        }

        fn refusing(possible: u32, boot: u32, error: VcpuError) -> Arc<Self> {
            Arc::new(Self {
                possible,
                boot,
                online: Mutex::new((0..boot).collect()),
                refuse: Some(error),
            })
        }
    }

    impl CpuOnline for FakeCpus {
        fn possible_cpus(&self) -> u32 {
            self.possible
        }

        fn boot_cpus(&self) -> u32 {
            self.boot
        }

        fn is_online(&self, id: u32) -> bool {
            self.online.lock().expect("test lock").contains(&id)
        }

        fn add_vcpu(&self, id: u32) -> Result<(), VcpuError> {
            if let Some(error) = self.refuse.clone() {
                return Err(error);
            }
            self.online.lock().expect("test lock").insert(id);
            Ok(())
        }
    }

    /// Discards every general purpose event. These tests read the JSON
    /// answer, not the guest's SCI.
    struct SilentSink;

    impl vmm_devices::acpi_gpe::HotplugEventSink for SilentSink {
        fn raise(&self, _bit: vmm_devices::acpi_gpe::GpeBit) {}
    }

    fn cpu_slots(cpus: Arc<FakeCpus>) -> CpuSlots {
        let log = slog::Logger::root(slog::Discard, slog::o!());
        let regs = vmm_devices::hotplug::cpu::CpuHotplug::new(
            cpus.possible_cpus(),
            Arc::new(SilentSink),
            log.clone(),
        );
        regs.set_boot_cpus(cpus.boot_cpus());
        let engine = CpuHotplugEngine::start_online(
            Arc::clone(&cpus) as Arc<dyn CpuOnline>,
            regs,
            log,
        );
        CpuSlots::new(engine, cpus)
    }

    fn line(response: Response) -> String {
        serde_json::to_string(&response).expect("serializes")
    }

    fn registry_with(id: &str, slot: &str) -> DeviceRegistry {
        let registry = DeviceRegistry::new();
        registry
            .insert(RegisteredDevice::new(
                id,
                vmm_machine::parse_bdf(slot),
                None,
                None,
                Some(format!("{slot},{id}")),
            ))
            .expect("first insert");
        registry
    }

    #[test]
    fn device_list_reports_id_bdf_and_state() {
        let registry = registry_with("virtio-blk@4", "4");

        let value = device_list(&registry);

        assert_eq!(
            serde_json::to_string(&value).expect("serializes"),
            r#"{"devices":[{"bdf":"0.4.0","hotpluggable":false,"id":"virtio-blk@4","state":"present"}]}"#
        );
    }

    #[test]
    fn a_device_with_no_pci_face_reports_a_null_bdf() {
        // The UEFI variable store is a Lifecycle with no BDF. Reporting
        // it as slot 0 would name the hostbridge.
        let registry = DeviceRegistry::new();
        registry
            .insert(RegisteredDevice::new("varstore", None, None, None, None))
            .expect("insert");

        let value = device_list(&registry);

        assert_eq!(
            serde_json::to_string(&value).expect("serializes"),
            r#"{"devices":[{"bdf":null,"hotpluggable":false,"id":"varstore","state":"present"}]}"#
        );
    }

    #[test]
    fn device_list_keeps_slot_order() {
        let registry = registry_with("virtio-blk@4", "4");
        registry
            .insert(RegisteredDevice::new(
                "virtio-net@5",
                vmm_machine::parse_bdf("5"),
                None,
                None,
                None,
            ))
            .expect("second insert");

        let value = device_list(&registry);
        let ids: Vec<&str> = value["devices"]
            .as_array()
            .expect("an array")
            .iter()
            .map(|d| d["id"].as_str().expect("an id"))
            .collect();

        assert_eq!(ids, ["virtio-blk@4", "virtio-net@5"]);
    }

    #[test]
    fn an_empty_registry_lists_no_devices() {
        let value = device_list(&DeviceRegistry::new());
        assert_eq!(
            serde_json::to_string(&value).expect("serializes"),
            r#"{"devices":[]}"#
        );
    }

    #[test]
    fn a_vm_without_hotplug_refuses_an_add_or_a_remove() {
        // The default. Without --hotplug the guest has no interface to
        // answer on, so the request has to be refused and not queued.
        let refused = r#"{"success":false,"error":"this VM was not started with --hotplug"}"#;
        assert_eq!(line(hotplug_disabled()), refused);
    }

    #[test]
    fn device_add_answers_with_the_id_device_remove_takes() {
        assert_eq!(
            line(added(Ok("virtio-blk@5".to_string()))),
            r#"{"success":true,"id":"virtio-blk@5"}"#
        );
    }

    #[test]
    fn a_refused_add_reports_why() {
        assert_eq!(
            line(added(Err(HotplugError::FixedSlot(1)))),
            r#"{"success":false,"error":"PCI slot 1 is fixed and cannot be plugged"}"#
        );
    }

    #[test]
    fn device_remove_answers_that_the_guest_was_asked() {
        // Not that the device is gone: the guest finishes the eject.
        assert_eq!(
            line(removing("virtio-blk@5", Ok(()))),
            r#"{"success":true,"id":"virtio-blk@5","state":"remove-pending"}"#
        );
        assert_eq!(
            line(removing(
                "virtio-blk@4",
                Err(HotplugError::NotHotpluggable("virtio-blk@4".to_string())),
            )),
            r#"{"success":false,"error":"device virtio-blk@4 was not hot-added and cannot be removed"}"#
        );
    }

    #[test]
    fn device_list_shows_a_guest_that_has_not_run_ej0() {
        // The one way an operator sees a stuck removal.
        let registry = registry_with("virtio-blk@5", "5");
        registry
            .set_state(
                "virtio-blk@5",
                SlotState::Present,
                SlotState::RemovePending,
            )
            .expect("requested");

        let value = device_list(&registry);

        assert_eq!(value["devices"][0]["state"], "remove-pending");
    }

    #[test]
    fn cpu_list_reports_the_madt_slots_even_with_no_engine() {
        // -c cpus=2,maxcpus=8 without --hotplug still gives the guest
        // eight LAPIC entries. Hiding six of them here would contradict
        // what the guest reads, so the `hotplug` field carries the
        // "nothing can be added" answer instead.
        let value = cpu_list(2, 8, None);

        assert_eq!(value["max_cpus"], 8);
        assert_eq!(value["hotplug"], false);
        let cpus = value["cpus"].as_array().expect("an array");
        assert_eq!(cpus.len(), 8);
        assert_eq!(cpus[1]["state"], "present");
        assert_eq!(cpus[2]["state"], "absent");
        assert_eq!(cpus[2]["kind"], "hotplug");
    }

    #[test]
    fn cpu_list_reports_one_entry_per_boot_cpu() {
        // No engine, so no slot a hot-add could fill. `num_cpus` keeps
        // the meaning a client that shipped before hot-add reads.
        let value = cpu_list(2, 2, None);

        assert_eq!(
            serde_json::to_string(&value).expect("serializes"),
            r#"{"cpus":[{"id":0,"kind":"boot","state":"present"},{"id":1,"kind":"boot","state":"present"}],"hotplug":false,"max_cpus":2,"num_cpus":2}"#
        );
    }

    #[test]
    fn cpu_list_reports_every_slot_a_hot_add_can_take() {
        let slots = cpu_slots(FakeCpus::new(4, 2));

        let value = cpu_list(2, 4, Some(&slots));

        assert_eq!(value["num_cpus"], 2);
        assert_eq!(value["max_cpus"], 4);
        assert_eq!(value["hotplug"], true);
        let cpus = value["cpus"].as_array().expect("an array");
        assert_eq!(cpus.len(), 4);
        assert_eq!(cpus[2]["state"], "absent");
        assert_eq!(cpus[2]["kind"], "hotplug");
    }

    #[test]
    fn cpu_list_shows_a_cpu_that_was_hot_added() {
        let slots = cpu_slots(FakeCpus::new(4, 2));

        assert_eq!(
            line(cpu_added(3, slots.add(3))),
            r#"{"success":true,"id":3,"state":"present"}"#,
        );

        let value = cpu_list(2, 4, Some(&slots));
        assert_eq!(value["cpus"][3]["state"], "present");
        assert_eq!(value["cpus"][2]["state"], "absent");
    }

    #[test]
    fn cpu_list_shows_an_id_a_failed_add_spent() {
        // ThreadGone means VM_ACTIVATE_CPU went through and the thread
        // did not. illumos cannot deactivate a vCPU, so the id is gone
        // and an operator has to be able to see that.
        let slots =
            cpu_slots(FakeCpus::refusing(4, 2, VcpuError::ThreadGone(3)));

        assert!(slots.add(3).is_err());

        let value = cpu_list(2, 4, Some(&slots));
        assert_eq!(value["cpus"][3]["state"], "consumed");
    }

    #[test]
    fn an_out_of_range_cpu_id_is_refused_with_a_readable_error() {
        let slots = cpu_slots(FakeCpus::new(4, 2));

        assert_eq!(
            line(cpu_added(9, slots.add(9))),
            r#"{"success":false,"error":"vCPU 9 is not one of the 4 CPU slots"}"#,
        );
        assert_eq!(
            line(cpu_added(0, slots.add(0))),
            r#"{"success":false,"error":"vCPU 0 is a boot CPU and is online already"}"#,
        );
    }

    #[test]
    fn a_vm_without_cpu_slots_refuses_an_add() {
        // Two starts land here: no --hotplug, and a maxcpus that is not
        // above the boot count. The answer names both.
        assert_eq!(
            line(Response::err(CPU_HOTPLUG_OFF)),
            r#"{"success":false,"error":"CPU hot-add needs --hotplug and -c cpus=N,maxcpus=M"}"#,
        );
    }

    #[test]
    fn mem_list_reports_the_boot_memory() {
        let value = mem_list(1_073_741_824, None);

        assert_eq!(
            serde_json::to_string(&value).expect("serializes"),
            r#"{"hotplug":false,"memory_bytes":1073741824,"slots":[{"bytes":1073741824,"id":"boot","state":"present"}]}"#
        );
    }

    #[test]
    fn mem_list_reports_the_window_the_slots_and_what_is_used() {
        // The engine itself needs a live VM, so the wire shape is
        // pinned against the numbers a live one would report.
        let window = vmm_machine::hot_add_window(
            1024 * 1024 * 1024,
            4 * 1024 * 1024 * 1024,
            1024 * 1024 * 1024,
        )
        .expect("4 slots of 1 GiB");

        let value =
            window_json(&MemWindowReport::new(&window, 1, 1024 * 1024 * 1024));

        assert_eq!(
            serde_json::to_string(&value).expect("serializes"),
            r#"{"added_bytes":1073741824,"base":4294967296,"bytes":4294967296,"slot_bytes":1073741824,"slots":4,"slots_used":1}"#
        );
    }

    #[test]
    fn a_vm_without_a_memory_window_refuses_an_add() {
        assert_eq!(
            line(Response::err(MEM_HOTPLUG_OFF)),
            r#"{"success":false,"error":"memory hot-add needs --hotplug and -o hotplug.maxmem=SIZE"}"#,
        );
    }

    #[test]
    fn a_memory_add_answers_with_the_slot_it_went_in() {
        assert_eq!(line(mem_added(Ok(2))), r#"{"success":true,"slot":2}"#);
        assert_eq!(
            line(mem_added(Err(vmm_machine::MemHotplugError::NoSlots {
                slots: 8
            }))),
            r#"{"success":false,"error":"all 8 memory slots are full"}"#,
        );
    }

    #[test]
    fn a_hot_add_needs_a_running_vm() {
        // A migrating VM is the one that matters: its destination is
        // already being built from the boot command line, so an add
        // here would be lost. A paused VM cannot enter the new thread.
        assert!(hot_add_blocked(VmRunState::Running).is_none());

        for state in [
            VmRunState::Paused,
            VmRunState::Migrating,
            VmRunState::Stopping,
            VmRunState::Stopped,
            VmRunState::Faulted,
        ] {
            let refusal =
                hot_add_blocked(state).expect("only a running VM may grow");
            let answer = line(refusal);
            assert!(answer.contains("must be running to hot-add"), "{answer}");
            assert!(answer.contains(&format!("{state:?}")), "{answer}");
        }
    }

    #[test]
    fn no_refusal_carries_a_run_of_spaces() {
        // A `\` line continuation in a string literal eats the newline
        // AND the indent that follows it. A message rebuilt without one
        // keeps that indent and reaches the operator as a gap.
        for message in [
            CPU_HOTPLUG_OFF,
            MEM_HOTPLUG_OFF,
            NO_CPU_REMOVE,
            NO_MEM_REMOVE,
        ] {
            assert!(!message.contains("  "), "{message}");
        }
    }

    #[test]
    fn a_removal_says_why_this_platform_cannot_do_it() {
        // An unknown-command error would read as a typo an operator
        // would try to fix.
        for reason in [NO_CPU_REMOVE, NO_MEM_REMOVE] {
            let answer = line(Response::err(reason));
            assert!(answer.contains("illumos has no"), "{answer}");
            assert!(answer.contains(r#""success":false"#), "{answer}");
        }
    }

    #[test]
    fn a_consumed_cpu_error_names_the_reason_the_id_is_gone() {
        assert_eq!(
            line(cpu_added(3, Err(CpuHotplugError::Consumed(3)))),
            r#"{"success":false,"error":"vCPU 3 was consumed by a failed add: the kernel cannot deactivate a vCPU, so the id cannot be reused"}"#,
        );
    }

    #[test]
    fn stop_releases_a_source_the_guest_has_left() {
        // A migrated-away source is left paused and holding its memory.
        // `stop` is the only command that can reach it, so refusing it
        // leaves a zombie no operator can clear.
        assert_eq!(stop_refusal(VmRunState::Stopped, true), None);
    }

    #[test]
    fn stop_refuses_a_vm_that_is_already_done() {
        assert!(stop_refusal(VmRunState::Stopped, false).is_some());
    }

    #[test]
    fn stop_reaches_a_live_vm_in_every_other_state() {
        // Stopping is there so a stop can retry a release that failed,
        // and Faulted because nothing else is taken there.
        for state in [
            VmRunState::Running,
            VmRunState::Paused,
            VmRunState::Migrating,
            VmRunState::Stopping,
            VmRunState::Faulted,
        ] {
            assert_eq!(stop_refusal(state, false), None, "{state:?}");
            assert_eq!(stop_refusal(state, true), None, "{state:?}");
        }
    }

    #[test]
    fn a_failed_import_never_resumes() {
        // The guest is half written, and the source may run it too.
        for force in [false, true] {
            assert_eq!(
                resume_from(VmRunState::ImportFailed, force),
                Err(super::IMPORT_FAILED)
            );
        }
    }

    #[test]
    fn an_import_in_doubt_resumes_only_when_forced() {
        assert_eq!(
            resume_from(VmRunState::ImportInDoubt, false),
            Err(super::IN_DOUBT)
        );
        assert_eq!(
            resume_from(VmRunState::ImportInDoubt, true),
            Ok(VmRunState::ImportInDoubt)
        );
        assert_eq!(
            resume_from(VmRunState::Paused, true),
            Ok(VmRunState::Paused)
        );
    }

    #[test]
    fn an_import_the_source_committed_to_resumes_plainly() {
        // The source has committed and stays paused, so this is the
        // only copy and nothing else can run it.
        for force in [false, true] {
            assert_eq!(
                resume_from(VmRunState::ImportCommitted, force),
                Ok(VmRunState::ImportCommitted)
            );
        }
        assert_eq!(stop_refusal(VmRunState::ImportCommitted, false), None);
    }

    #[test]
    fn status_reports_a_stuck_release_beside_the_state() {
        // `data` is flattened into the line.
        let mut stuck = Response::ok();
        stuck.state = Some(VmRunState::Stopping);
        stuck.data = status_data(true, None, 0);
        assert_eq!(
            line(stuck),
            r#"{"success":true,"state":"stopping","release_stuck":true}"#
        );

        let mut fine = Response::ok();
        fine.data = status_data(false, None, 0);
        assert_eq!(line(fine), r#"{"success":true}"#);
    }

    #[test]
    fn status_reports_a_tainted_gate_and_leaked_devices() {
        let mut r = Response::ok();
        r.data = status_data(false, Some("viona ring 1".into()), 2);
        assert_eq!(
            line(r),
            r#"{"success":true,"ring_devices_leaked":2,"ring_gate_tainted":"viona ring 1"}"#
        );
    }
}
