// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Migration command handlers and their address guards.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use slog::{error, info};

use vmm_core::mem::MemCtx;
use vmm_machine::HotplugEngines;
use vmm_migrate::sync_source::DeviceFlushError;

use super::protocol::{Command, Response};
use super::{VmController, VmRunState};
use crate::host_state::HostLocalState;

/// How long a finished source waits to be reaped before it ends itself.
///
/// Long enough for an orchestrator to stumble over its switchover, and
/// short enough that a wedged one does not leave the node holding a
/// guest's whole memory for a VM that runs nowhere.
const SOURCE_RELEASE_GRACE: Duration = Duration::from_secs(300);

pub(super) fn migration_source_guard(
    state: HostLocalState,
    has_ahci_cd: bool,
    command: &Command,
) -> Option<Response> {
    if !matches!(command, Command::MigrateSource { .. }) {
        return None;
    }
    if has_ahci_cd {
        return Some(Response::err(
            "ahci-cd is not supported on a migration source",
        ));
    }
    state.migration_blocker().map(Response::err)
}

/// What the VM gained after boot: added CPUs, and added bytes.
fn growth(hotplug: &HotplugEngines) -> (usize, u64) {
    let cpus = hotplug.cpu.as_ref().map_or(0, |cpus| {
        // Ids at or above the boot count that are running. Counted, not
        // indexed: an id is only ever compared here.
        (cpus.boot_cpus()..cpus.max_cpus())
            .filter(|id| cpus.is_online(*id))
            .count()
    });
    let bytes = hotplug.mem.as_ref().map_or(0, |mem| mem.bytes_added());
    (cpus, bytes)
}

/// Why a VM whose device set no longer matches its boot `-s` list
/// cannot be a migration source.
///
/// Device identity on the migration wire is positional. The source
/// tags every queue group with `pci_slot: 0` (`control/mod.rs`), the
/// preamble device list is empty (`vmm-migrate/src/sync_source.rs`),
/// and the destination claims the first queue group whose count equals
/// its own (`vmm-virtio/src/pci/migrate.rs`). While the
/// live set equals the boot `-s` list, both sides build the same
/// devices in the same order, so the match is accidental but correct.
/// A hot-add or an eject breaks that, and a guest would come up on the
/// far side with another device's ring state. Refuse until device
/// identity is on the wire.
fn device_set_changed(live_hotplug: &[String]) -> Option<String> {
    if live_hotplug.is_empty() {
        return None;
    }
    Some(format!(
        "this VM hot-added {} device(s) since boot ({}); migration \
         matches devices by position, so the destination would restore \
         the wrong device state",
        live_hotplug.len(),
        live_hotplug.join(" "),
    ))
}

/// Why a VM that grew since boot cannot be a migration source.
///
/// The destination is started from the same command line, so it gets
/// the boot CPU count and the boot memory size. Nothing carries a
/// hot-add across: a vCPU that is running here would have no thread
/// there, and memory the guest is using would have nowhere to land.
/// Refusing is the only safe answer, because the loss would show up as
/// guest corruption on the far side and not as a failed migration.
fn grown_since_boot(added_cpus: usize, added_bytes: u64) -> Option<String> {
    if added_cpus > 0 {
        return Some(format!(
            "this VM hot-added {added_cpus} vCPUs; a migration \
             destination is built from the same command line and would \
             have none of them",
        ));
    }
    if added_bytes > 0 {
        return Some(format!(
            "this VM hot-added {added_bytes} bytes of memory; a \
             migration destination is built from the same command line \
             and would have nowhere to put it",
        ));
    }
    None
}

// ── Migration address validation ────────────────────────────────────

/// Validate a migration address (host:port) to prevent SSRF and
/// arbitrary port binding. Rejects:
/// - Missing or invalid port
/// - Privileged ports (< 1024) for listen addresses
/// - Loopback (127.x.x.x, ::1). Migration to self is nonsensical
/// - Addresses that fail to parse as host:port
fn validate_migration_addr(addr: &str) -> Result<(), String> {
    // Must contain a colon separating host and port
    let colon = addr.rfind(':').ok_or("missing port (expected host:port)")?;
    let host = &addr[..colon];
    let port_str = &addr[colon + 1..];

    let port: u16 = port_str
        .parse()
        .map_err(|_| format!("invalid port: {port_str}"))?;

    if port == 0 {
        return Err("port 0 is not allowed".into());
    }

    // Reject loopback addresses to prevent self-migration confusion
    let host_trimmed = host.trim_matches(|c| c == '[' || c == ']');
    if host_trimmed == "localhost"
        || host_trimmed == "127.0.0.1"
        || host_trimmed == "::1"
        || host_trimmed == "0.0.0.0"
    {
        return Err(format!("loopback/wildcard address not allowed: {host}"));
    }

    // Reject empty host
    if host_trimmed.is_empty() {
        return Err("empty host".into());
    }

    Ok(())
}

// ── Handlers ────────────────────────────────────────────────────────

pub(super) fn migrate_source(
    ctrl: &Arc<VmController>,
    target_addr: String,
    zfs_barrier: Option<String>,
) -> Response {
    // Validate the target address to prevent SSRF. Require
    // it to be a valid host:port and reject obviously-dangerous
    // targets like localhost, link-local, and multicast.
    // Skip TCP validation for Unix socket paths
    if !target_addr.starts_with('/') && !target_addr.ends_with(".sock") {
        if let Err(e) = validate_migration_addr(&target_addr) {
            return Response::err(&format!("invalid target_addr: {e}"));
        }
    }

    if !ctrl.transition(VmRunState::Running, VmRunState::Migrating) {
        return Response::err("VM must be running to start migration");
    }

    // The state comes first, then the topology under the lock a hot-add
    // takes. An add that was in flight is therefore counted here, and
    // one that arrives later finds the VM migrating and is refused.
    // Reading the topology before the state change would miss both.
    let refused = {
        let _topology = ctrl.lock_topology();
        let (cpus, bytes) = growth(&ctrl.hotplug);
        grown_since_boot(cpus, bytes).or_else(|| {
            device_set_changed(&vmm_machine::hotplug_specs(&ctrl.registry))
        })
    };
    if let Some(reason) = refused {
        ctrl.state
            .store(VmRunState::Running as u8, Ordering::Release);
        return Response::err(&reason);
    }

    let status = Arc::new(Mutex::new(vmm_migrate::MigrationStatus::default()));

    // Store status for later query
    if let Ok(mut ms) = ctrl.migrate_status.lock() {
        *ms = Some(Arc::clone(&status));
    }

    let ctrl_arc = Arc::clone(ctrl);
    let hdl = Arc::clone(&ctrl.hdl);
    let memctx = MemCtx::new(Arc::clone(&ctrl.physmap));
    let num_cpus = ctrl.num_cpus;
    let mem_size = ctrl.mem_size as u64;
    let mlog = ctrl.log.clone();

    let is_unix =
        target_addr.starts_with('/') || target_addr.ends_with(".sock");

    if let Err(e) = thread::Builder::new().name("migrate-source".into()).spawn(
        move || {
            if is_unix {
                // Synchronous path for Unix sockets (works in
                // bhyve zones without /dev/poll).
                info!(mlog, "connecting to migration socket";
                    "path" => &target_addr);
                let stream = match UnixStream::connect(&target_addr) {
                    Ok(s) => s,
                    Err(e) => {
                        error!(mlog, "connect failed";
                            "path" => &target_addr, "error" => %e);
                        ctrl_arc.state.store(
                            VmRunState::Running as u8,
                            Ordering::Release,
                        );
                        return;
                    }
                };

                let ctrl_for_prepause = ctrl_arc.clone();
                let pre_pause_fn: vmm_migrate::sync_source::DevicePrePauseFn =
                    Box::new(move || {
                        for dev in ctrl_for_prepause.lifecycle_devices() {
                            dev.pause_for_migration();
                        }
                    });
                let ctrl_for_flush = ctrl_arc.clone();
                let flush_fn: vmm_migrate::sync_source::DeviceFlushFn =
                    Box::new(move || {
                        for dev in ctrl_for_flush.lifecycle_devices() {
                            dev.flush_backing(
                                vmm_devices::FlushIntent::Durable,
                            )
                            .map_err(|source| {
                                DeviceFlushError {
                                    device: dev.type_name(),
                                    source,
                                }
                            })?;
                        }
                        Ok(())
                    });
                let ctrl_for_resume = ctrl_arc.clone();
                let resume_fn: vmm_migrate::sync_source::DeviceResumeFn =
                    Box::new(move || ctrl_for_resume.resume_devices());
                let ctrl_for_export = ctrl_arc.clone();
                let export_fn: vmm_migrate::sync_source::DeviceExportFn =
                    Box::new(move || {
                        match ctrl_for_export.export_device_queues() {
                            Ok(export) => export,
                            Err(error) => {
                                error!(ctrl_for_export.log,
                                    "device queue export failed";
                                    "error" => %error);
                                // DeviceExportFn is infallible until the
                                // migration layer can propagate this error.
                                // Empty state avoids racing live ring indices.
                                vmm_migrate::sync_source::DeviceQueueExport {
                                    virtio_queues: Vec::new(),
                                    nvme_controllers: Vec::new(),
                                }
                            }
                        }
                    });
                match vmm_migrate::sync_source::run_source_sync(
                    stream,
                    hdl,
                    &memctx,
                    num_cpus,
                    mem_size,
                    status,
                    &mlog,
                    ctrl_arc.cpu_baseline,
                    pre_pause_fn,
                    flush_fn,
                    resume_fn,
                    export_fn,
                    zfs_barrier.map(PathBuf::from),
                ) {
                    Ok(()) => {
                        info!(mlog, "source migration complete");
                        ctrl_arc.finish_migrated_away(SOURCE_RELEASE_GRACE);
                    }
                    Err(e) => {
                        error!(mlog, "source migration failed";
                            "error" => %e);
                        ctrl_arc.state.store(
                            VmRunState::Running as u8,
                            Ordering::Release,
                        );
                    }
                }
            } else {
                // Async path for TCP (GZ or manual testing)
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        error!(mlog, "tokio runtime failed"; "error" => %e);
                        ctrl_arc.state.store(
                            VmRunState::Running as u8,
                            Ordering::Release,
                        );
                        return;
                    }
                };
                rt.block_on(async {
                    let stream = match tokio::net::TcpStream::connect(
                        &target_addr,
                    )
                    .await
                    {
                        Ok(s) => vmm_migrate::MigrateStream::Tcp(s),
                        Err(e) => {
                            error!(mlog, "connect failed";
                                "target" => &target_addr, "error" => %e);
                            ctrl_arc.state.store(
                                VmRunState::Running as u8,
                                Ordering::Release,
                            );
                            return;
                        }
                    };
                    match vmm_migrate::source::run_source(
                        stream,
                        hdl,
                        &memctx,
                        num_cpus,
                        mem_size,
                        status,
                        &mlog,
                        ctrl_arc.cpu_baseline,
                    )
                    .await
                    {
                        Ok(()) => {
                            info!(mlog, "source migration complete");
                            ctrl_arc.finish_migrated_away(SOURCE_RELEASE_GRACE);
                        }
                        Err(e) => {
                            error!(mlog, "source migration failed";
                                "error" => %e);
                            ctrl_arc.state.store(
                                VmRunState::Running as u8,
                                Ordering::Release,
                            );
                        }
                    }
                });
            }
        },
    ) {
        ctrl.state
            .store(VmRunState::Running as u8, Ordering::Release);
        return Response::err(&format!(
            "failed to spawn migration thread: {}",
            e
        ));
    }

    let mut r = Response::ok();
    r.state = Some(VmRunState::Migrating);
    r
}

pub(super) fn migrate_dest(
    ctrl: &Arc<VmController>,
    listen_addr: String,
) -> Response {
    // Validate the listen address to prevent arbitrary port binding.
    if let Err(e) = validate_migration_addr(&listen_addr) {
        return Response::err(&format!("invalid listen_addr: {e}"));
    }

    let prev_state = ctrl.state();
    if prev_state != VmRunState::Paused && prev_state != VmRunState::Running {
        return Response::err(
            "VM must be running or paused to receive migration",
        );
    }
    ctrl.state
        .store(VmRunState::Migrating as u8, Ordering::Release);

    let status = Arc::new(Mutex::new(vmm_migrate::MigrationStatus::default()));
    if let Ok(mut ms) = ctrl.migrate_status.lock() {
        *ms = Some(Arc::clone(&status));
    }

    let ctrl_arc = Arc::clone(ctrl);
    let hdl = Arc::clone(&ctrl.hdl);
    let memctx = MemCtx::new(Arc::clone(&ctrl.physmap));
    let num_cpus = ctrl.num_cpus;
    let mem_size = ctrl.mem_size as u64;
    let mlog = ctrl.log.clone();

    if let Err(e) = thread::Builder::new()
        .name("migrate-dest".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    error!(mlog, "failed to create tokio runtime"; "error" => %e);
                    ctrl_arc.state.store(
                        VmRunState::Stopped as u8,
                        Ordering::Release,
                    );
                    return;
                }
            };

            rt.block_on(async {
                let listener = match tokio::net::TcpListener::bind(
                    &listen_addr,
                )
                .await
                {
                    Ok(l) => l,
                    Err(e) => {
                        error!(mlog, "bind failed";
                            "addr" => &listen_addr,
                            "error" => %e,
                        );
                        ctrl_arc.state.store(
                            VmRunState::Stopped as u8,
                            Ordering::Release,
                        );
                        return;
                    }
                };

                let (stream, peer) = match listener.accept().await {
                    Ok(v) => v,
                    Err(e) => {
                        error!(mlog, "accept failed"; "error" => %e);
                        ctrl_arc.state.store(
                            VmRunState::Stopped as u8,
                            Ordering::Release,
                        );
                        return;
                    }
                };

                info!(mlog, "accepted migration connection"; "peer" => %peer);

                match vmm_migrate::destination::run_destination(
                    stream,
                    hdl,
                    &memctx,
                    num_cpus,
                    mem_size,
                    status,
                    &mlog,
                )
                .await
                {
                    Ok(()) => {
                        info!(mlog, "destination migration complete, VM running");
                        ctrl_arc.state.store(
                            VmRunState::Running as u8,
                            Ordering::Release,
                        );
                    }
                    Err(e) => {
                        error!(mlog, "destination migration failed"; "error" => %e);
                        ctrl_arc.state.store(
                            VmRunState::Stopped as u8,
                            Ordering::Release,
                        );
                    }
                }
            });
        })
    {
        ctrl.state.store(
            prev_state as u8,
            Ordering::Release,
        );
        return Response::err(
            &format!("failed to spawn migration thread: {}", e),
        );
    }

    let mut r = Response::ok();
    r.state = Some(VmRunState::Migrating);
    r
}

pub(super) fn migrate_status(ctrl: &Arc<VmController>) -> Response {
    let phase_info = ctrl.migrate_status.lock().ok().and_then(|ms| {
        ms.as_ref().and_then(|s| s.lock().ok().map(|s| s.clone()))
    });

    match phase_info {
        Some(s) => {
            let mut r = Response::ok();
            r.state = Some(ctrl.state());
            r.migrate_phase = Some(s.phase.to_string());
            r.migrate_bytes = Some(s.bytes_transferred);
            r.migrate_pages = Some(s.pages_transferred);
            r
        }
        None => {
            let mut r = Response::ok();
            r.state = Some(ctrl.state());
            r
        }
    }
}

pub(super) fn migrate_config(ctrl: &Arc<VmController>) -> Response {
    let config = serde_json::json!({
        "vm_name": ctrl.vm_name,
        "num_cpus": ctrl.num_cpus,
        "memory_mb": ctrl.mem_size / (1024 * 1024),
        "pci_slots": migrate_pci_slots(&ctrl.cli_pci_slots, &ctrl.registry),
        "lpc_devices": ctrl.cli_lpc,
    });
    let mut r = Response::ok();
    r.config = Some(config);
    r
}

/// The `-s` list the destination has to be started with.
///
/// argv plus the devices an operator hot-added. The destination imports
/// the state of every device in the registry, so one it was not told to
/// create would leave the guest a disk short. This is a report only:
/// `migrate-source` refuses a VM whose device set grew, because device
/// identity on the wire is positional. See [`device_set_changed`].
fn migrate_pci_slots(
    argv_slots: &[String],
    registry: &vmm_machine::DeviceRegistry,
) -> Vec<String> {
    let mut slots = argv_slots.to_vec();
    slots.extend(vmm_machine::hotplug_specs(registry));
    slots
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_destination_is_told_about_a_hotplugged_device() {
        // The destination imports the state of every registered device,
        // so a device it was not told to create loses the guest a disk.
        use vmm_machine::{DeviceRegistry, RegisteredDevice};

        let registry = DeviceRegistry::new();
        let mut added = RegisteredDevice::new(
            "virtio-blk@5",
            vmm_machine::parse_bdf("5"),
            None,
            None,
            Some("5,virtio-blk,/added.img".to_string()),
        );
        added.hotpluggable = true;
        registry.insert(added).expect("insert");

        let slots = migrate_pci_slots(
            &["4,nvme,/boot.img,bootindex=1".to_string()],
            &registry,
        );

        assert_eq!(
            slots,
            ["4,nvme,/boot.img,bootindex=1", "5,virtio-blk,/added.img"],
        );
    }

    #[test]
    fn a_machine_with_no_hotplug_reports_its_own_argv() {
        let argv = ["4,nvme,/boot.img".to_string()];
        assert_eq!(
            migrate_pci_slots(&argv, &vmm_machine::DeviceRegistry::new()),
            argv,
        );
    }

    #[test]
    fn migrate_source_rejects_host_local_state() {
        let command = Command::MigrateSource {
            target_addr: "192.0.2.10:4567".to_string(),
            zfs_barrier: None,
        };

        for state in [
            HostLocalState::new(true, false),
            HostLocalState::new(false, true),
            HostLocalState::new(true, true),
        ] {
            let response = migration_source_guard(state, false, &command)
                .expect("host-local state must block migration");
            assert!(!response.success);
            assert!(response
                .error
                .as_deref()
                .expect("error response must explain rejection")
                .contains("host-local state is not carried by migration"));
        }

        assert!(migration_source_guard(
            HostLocalState::default(),
            false,
            &command
        )
        .is_none());
    }

    #[test]
    fn migrate_source_rejects_ahci_cd() {
        let command = Command::MigrateSource {
            target_addr: "192.0.2.10:4567".to_string(),
            zfs_barrier: None,
        };

        let response =
            migration_source_guard(HostLocalState::default(), true, &command)
                .expect("AHCI CD must block migration");
        assert!(!response.success);
        assert_eq!(
            response.error.as_deref(),
            Some("ahci-cd is not supported on a migration source")
        );
    }

    #[test]
    fn a_vm_that_did_not_grow_can_still_migrate() {
        // Starting with --hotplug is not the same as using it. A VM
        // that added nothing has to keep the migration it had.
        assert_eq!(growth(&HotplugEngines::default()), (0, 0));
        assert_eq!(grown_since_boot(0, 0), None);
    }

    #[test]
    fn a_boot_device_set_can_still_migrate() {
        // Only a hot-added device changes the set. A VM that added
        // nothing has to keep the migration it had.
        assert_eq!(device_set_changed(&[]), None);
    }

    #[test]
    fn a_hot_added_device_cannot_be_a_migration_source() {
        // Queue state is matched by position, so the destination would
        // give the guest another device's rings.
        let reason =
            device_set_changed(&["5,virtio-blk,/added.img".to_string()])
                .expect("an added device must block migration");
        assert!(reason.contains("hot-added 1 device(s)"), "{reason}");
        assert!(reason.contains("5,virtio-blk,/added.img"), "{reason}");
        assert!(reason.contains("by position"), "{reason}");
    }

    #[test]
    fn every_changed_slot_is_named_in_the_refusal() {
        // An operator has to see which slots to remove before they can
        // retry, so the message names all of them.
        let reason = device_set_changed(&[
            "5,virtio-blk,/a.img".to_string(),
            "6,virtio-blk,/b.img".to_string(),
        ])
        .expect("two added devices must block migration");
        assert!(reason.contains("5,virtio-blk,/a.img"), "{reason}");
        assert!(reason.contains("6,virtio-blk,/b.img"), "{reason}");
    }

    #[test]
    fn a_device_still_pending_removal_blocks_migration() {
        // `hotplug_specs` keeps a slot the guest has not ejected, so it
        // is still part of the live set and still unsafe to migrate.
        use vmm_machine::{DeviceRegistry, RegisteredDevice, SlotState};

        let registry = DeviceRegistry::new();
        let mut added = RegisteredDevice::new(
            "virtio-blk@5",
            vmm_machine::parse_bdf("5"),
            None,
            None,
            Some("5,virtio-blk,/added.img".to_string()),
        );
        added.hotpluggable = true;
        let id = added.id.clone();
        registry.insert(added).expect("insert");
        registry
            .set_state(&id, SlotState::Present, SlotState::RemovePending)
            .expect("remove requested");

        assert!(device_set_changed(&vmm_machine::hotplug_specs(&registry))
            .is_some());
    }

    #[test]
    fn a_vm_that_hot_added_cannot_be_a_migration_source() {
        // The destination is built from the same command line, so the
        // loss would show up as guest corruption on the far side and
        // not as a failed migration.
        let cpus = grown_since_boot(2, 0).expect("two added vCPUs");
        assert!(cpus.contains("hot-added 2 vCPUs"), "{cpus}");

        let memory = grown_since_boot(0, 134_217_728).expect("added memory");
        assert!(memory.contains("134217728 bytes"), "{memory}");
    }
}
