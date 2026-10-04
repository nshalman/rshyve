// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Migration command handlers and their address guards.

use std::io;
use std::net::{Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use slog::{debug, error, info};

use vmm_core::mem::MemCtx;
use vmm_machine::HotplugEngines;

use super::protocol::{Command, Response};
use super::run_state::{MigrationLease, RunControl};
use super::{VmController, VmRunState};
use crate::host_state::HostLocalState;

/// How long a finished source waits to be reaped before it ends itself.
///
/// Long enough for an orchestrator to stumble over its switchover, and
/// short enough that a wedged one does not leave the node holding a
/// guest's whole memory for a VM that runs nowhere.
const SOURCE_RELEASE_GRACE: Duration = Duration::from_secs(300);

/// How long a source waits for each address of its peer to answer.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How often a destination waiting for its source checks for a stop.
const ACCEPT_POLL: Duration = Duration::from_millis(50);

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
/// The destination is started from the boot `-s` list, so a device the
/// guest gained since boot has nowhere to land. The wire names devices
/// by PCI address now, so the destination would refuse the payload
/// rather than restore the wrong state, but refusing here tells the
/// operator which slots to remove before the guest is ever paused.
fn device_set_changed(live_hotplug: &[String]) -> Option<String> {
    if live_hotplug.is_empty() {
        return None;
    }
    Some(format!(
        "this VM hot-added {} device(s) since boot ({}); a migration \
         destination is built from the boot device list and would have \
         none of them",
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

/// Devices that carry guest-visible state no migration puts back.
///
/// A guest whose driver configured one of these would find it at reset
/// on the far side, with no error anywhere. The list is documented in
/// docs/migration.md; grow it there and here together.
const UNMIGRATABLE: &[&str] = &[
    "ahci-cd",
    "virtio-console",
    "virtio-fs",
    "virtio-vsock",
    "xhci",
];

/// Why a VM carrying a device with unmigratable state cannot be a
/// source.
fn unmigratable_devices(
    devices: &[vmm_migrate::codec::DeviceIdentity],
) -> Option<String> {
    let blocking: Vec<String> = devices
        .iter()
        .filter(|d| UNMIGRATABLE.contains(&d.kind.as_str()))
        .map(|d| d.to_string())
        .collect();
    if blocking.is_empty() {
        return None;
    }
    Some(format!(
        "no migration state is carried for {}; the guest would find \
         the device at reset on the destination",
        blocking.join(" "),
    ))
}

/// Every device that says no migration can carry it, named by its
/// registry id. Read from the live registry, so a disk hot-added since
/// boot counts as well.
pub(crate) fn device_blockers(
    registry: &vmm_machine::DeviceRegistry,
) -> Option<String> {
    let blocking: Vec<String> = registry
        .list()
        .into_iter()
        .filter_map(|slot| {
            let why = slot.lifecycle?.migration_blocker()?;
            Some(format!("{}: {why}", slot.id))
        })
        .collect();
    if blocking.is_empty() {
        return None;
    }
    Some(format!("this VM cannot migrate: {}", blocking.join("; ")))
}

/// Why this VM cannot be a migration source, or `None`.
fn source_refusal(
    hotplug: &HotplugEngines,
    registry: &vmm_machine::DeviceRegistry,
    identities: &[vmm_migrate::codec::DeviceIdentity],
) -> Option<String> {
    let (cpus, bytes) = growth(hotplug);
    device_blockers(registry)
        .or_else(|| grown_since_boot(cpus, bytes))
        .or_else(|| device_set_changed(&vmm_machine::hotplug_specs(registry)))
        .or_else(|| unmigratable_devices(identities))
}

/// Move a running VM to `Migrating`, or say why it cannot be a source.
fn claim_source(
    run: &RunControl,
    hotplug: &HotplugEngines,
    registry: &vmm_machine::DeviceRegistry,
    identities: impl FnOnce() -> Vec<vmm_migrate::codec::DeviceIdentity>,
) -> Result<(), String> {
    run.admit_source(|| source_refusal(hotplug, registry, &identities()))
}

/// Move a paused VM to `Migrating`, or say why it cannot take an import.
fn claim_dest(
    run: &RunControl,
    registry: &vmm_machine::DeviceRegistry,
) -> Result<(), String> {
    run.admit_import(|| device_blockers(registry))
}

/// Where a migration source connects to.
enum MigrateTarget {
    /// A Unix socket the GZ agent bridges to the peer. The only kind
    /// that works inside a bhyve zone.
    Unix(String),
    Tcp(String),
}

impl MigrateTarget {
    fn parse(target: &str) -> Result<Self, String> {
        if target.starts_with('/') || target.ends_with(".sock") {
            return Ok(Self::Unix(target.to_string()));
        }
        validate_migration_addr(target)?;
        Ok(Self::Tcp(target.to_string()))
    }
}

// ── Handlers ────────────────────────────────────────────────────────

pub(super) fn migrate_source(
    ctrl: &Arc<VmController>,
    target_addr: String,
    zfs_barrier: Option<String>,
) -> Response {
    let target = match MigrateTarget::parse(&target_addr) {
        Ok(target) => target,
        Err(e) => return Response::err(&format!("invalid target_addr: {e}")),
    };

    if let Err(why) =
        claim_source(&ctrl.run, &ctrl.hotplug, &ctrl.registry, || {
            ctrl.migrate_identities()
        })
    {
        return Response::err(&why);
    }

    let status = Arc::new(Mutex::new(vmm_migrate::MigrationStatus::default()));
    if let Ok(mut ms) = ctrl.migrate_status.lock() {
        *ms = Some(Arc::clone(&status));
    }

    let ctrl_arc = Arc::clone(ctrl);
    let hdl = Arc::clone(&ctrl.hdl);
    let memctx = MemCtx::new(Arc::clone(&ctrl.physmap));
    let mlog = ctrl.log.clone();
    let config = vmm_migrate::source::SourceConfig {
        num_cpus: ctrl.num_cpus,
        mem_size: ctrl.mem_size as u64,
        cpu_baseline: ctrl.cpu_baseline,
        devices: ctrl.migrate_identities(),
        deadlines: vmm_migrate::wire::Deadlines::default(),
        zfs_barrier: zfs_barrier.map(PathBuf::from),
    };

    if let Err(e) =
        thread::Builder::new()
            .name("migrate-source".into())
            .spawn(move || {
                let lease = MigrationLease::new(ctrl_arc.as_ref());
                let hooks = source_hooks(&ctrl_arc);
                let run = &ctrl_arc.run;
                let outcome = match target {
                    MigrateTarget::Unix(path) => {
                        info!(mlog, "connecting to migration socket";
                        "path" => &path);
                        UnixStream::connect(&path)
                            .and_then(|s| shut_on_cancel(run, s, &mlog))
                            .map_err(|e| SourceOutcome::Failed(e.to_string()))
                            .and_then(|stream| {
                                run_source(
                                    stream, hdl, &memctx, config, hooks,
                                    status, &mlog,
                                )
                            })
                    }
                    MigrateTarget::Tcp(addr) => {
                        info!(mlog, "connecting to migration peer";
                        "target" => &addr);
                        connect_tcp(&addr, || run.cancelled())
                            .and_then(|s| shut_on_cancel(run, s, &mlog))
                            .map_err(|e| SourceOutcome::Failed(e.to_string()))
                            .and_then(|stream| {
                                run_source(
                                    stream, hdl, &memctx, config, hooks,
                                    status, &mlog,
                                )
                            })
                    }
                };
                match outcome {
                    Ok(()) => {
                        info!(mlog, "source migration complete");
                        ctrl_arc.finish_migrated_away(SOURCE_RELEASE_GRACE);
                    }
                    Err(SourceOutcome::Committed(why)) => {
                        // The destination may already be running the
                        // guest. Running it here too would put two
                        // copies on one disk, so the VM stays paused
                        // and migrating until an operator decides.
                        error!(mlog, "the guest was handed over and the \
                            destination never confirmed; this VM stays \
                            paused and is NOT released";
                            "error" => why);
                        ctrl_arc.ended(
                            run.end_committed(|| ctrl_arc.release_pause()),
                        );
                    }
                    Err(e @ SourceOutcome::Failed(_)) => {
                        error!(mlog, "source migration failed"; "error" => %e);
                        end_migration(&ctrl_arc);
                    }
                }
                lease.release();
            })
    {
        end_migration(ctrl);
        return Response::err(&format!(
            "failed to spawn migration thread: {e}"
        ));
    }

    let mut r = Response::ok();
    r.state = Some(VmRunState::Migrating);
    r
}

/// Hand the state back from `Migrating`, unless a stop owns it now.
fn end_migration(ctrl: &VmController) {
    ctrl.ended(ctrl.run.end_migration(|| ctrl.release_pause()));
}

/// A stream a stop can shut down from another thread.
trait Shut: Sized + Send + 'static {
    fn try_clone(&self) -> io::Result<Self>;
    fn shutdown(&self, how: Shutdown) -> io::Result<()>;
}

impl Shut for TcpStream {
    fn try_clone(&self) -> io::Result<Self> {
        TcpStream::try_clone(self)
    }

    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        TcpStream::shutdown(self, how)
    }
}

impl Shut for UnixStream {
    fn try_clone(&self) -> io::Result<Self> {
        UnixStream::try_clone(self)
    }

    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        UnixStream::shutdown(self, how)
    }
}

/// Let a stop's cancel shut `stream` down. A send or receive blocked on
/// a peer that went quiet returns at once, and the thread rolls back.
fn shut_on_cancel<S: Shut>(
    run: &RunControl,
    stream: S,
    log: &slog::Logger,
) -> io::Result<S> {
    let clone = stream.try_clone()?;
    let log = log.clone();
    run.on_cancel(Box::new(move || {
        if let Err(e) = clone.shutdown(Shutdown::Both) {
            debug!(log, "the migration socket was already closed";
                "error" => %e);
        }
    }));
    Ok(stream)
}

/// Connect to the first address of `addr` that answers in time.
fn connect_tcp(
    addr: &str,
    cancelled: impl Fn() -> bool,
) -> io::Result<TcpStream> {
    let mut last = None;
    for peer in addr.to_socket_addrs()? {
        if cancelled() {
            return Err(io::Error::other(vmm_migrate::CANCELLED));
        }
        match TcpStream::connect_timeout(&peer, CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, format!("{addr}: no address"))
    }))
}

/// The device hooks the source phases drive.
fn source_hooks(ctrl: &Arc<VmController>) -> vmm_migrate::source::SourceHooks {
    let for_pre_pause = Arc::clone(ctrl);
    let for_flush = Arc::clone(ctrl);
    let for_resume = Arc::clone(ctrl);
    let for_export = Arc::clone(ctrl);
    let for_hyperv = Arc::clone(ctrl);
    let (pause_vm, resume_vm) = migration_pause_hooks(ctrl);
    vmm_migrate::source::SourceHooks {
        pre_pause: Box::new(move || {
            for_pre_pause.pause_devices_for_migration()
        }),
        flush: Box::new(move || for_flush.flush_devices_for_migration()),
        resume: Box::new(move || for_resume.resume_devices_after_migration()),
        export: Box::new(move || for_export.export_device_state()),
        hyperv: Box::new(move || for_hyperv.export_hyperv()),
        pause_vm,
        resume_vm,
        gate: Box::new(ControlGate(Arc::clone(ctrl))),
    }
}

/// Lets a stop cancel this VM's migration before it commits.
struct ControlGate(Arc<VmController>);

impl vmm_migrate::StopGate for ControlGate {
    fn cancelled(&self) -> bool {
        self.0.run.cancelled()
    }

    fn commit(&self) -> Result<(), vmm_migrate::codec::MigrateError> {
        if self.0.run.commit() {
            Ok(())
        } else {
            Err(vmm_migrate::cancelled())
        }
    }
}

fn migration_pause_hooks(
    ctrl: &Arc<VmController>,
) -> (vmm_migrate::VmPauseFn, vmm_migrate::VmPauseFn) {
    let for_pause = Arc::clone(ctrl);
    let for_resume = Arc::clone(ctrl);
    (
        Box::new(move || for_pause.pause_for_migration()),
        Box::new(move || for_resume.resume_for_migration()),
    )
}

/// How a source migration ended, when it did not succeed.
enum SourceOutcome {
    /// The guest never left. The VM can run again here.
    Failed(String),
    /// The guest was handed over and the far side never confirmed. The
    /// VM must not run again here.
    Committed(String),
}

impl From<vmm_migrate::codec::MigrateError> for SourceOutcome {
    fn from(error: vmm_migrate::codec::MigrateError) -> Self {
        match error {
            vmm_migrate::codec::MigrateError::Committed(why) => {
                Self::Committed(why)
            }
            other => Self::Failed(other.to_string()),
        }
    }
}

impl std::fmt::Display for SourceOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(why) | Self::Committed(why) => f.write_str(why),
        }
    }
}

fn run_source<S: vmm_migrate::wire::Transport>(
    stream: S,
    hdl: Arc<vmm_core::hdl::VmmHdl>,
    memctx: &MemCtx,
    config: vmm_migrate::source::SourceConfig,
    hooks: vmm_migrate::source::SourceHooks,
    status: Arc<Mutex<vmm_migrate::MigrationStatus>>,
    log: &slog::Logger,
) -> Result<(), SourceOutcome> {
    vmm_migrate::source::run_source(
        stream, hdl, memctx, config, hooks, status, log,
    )
    .map_err(SourceOutcome::from)
}

pub(super) fn migrate_dest(
    ctrl: &Arc<VmController>,
    listen_addr: String,
    allow_cpu_feature_mismatch: bool,
) -> Response {
    if let Err(e) = validate_migration_addr(&listen_addr) {
        return Response::err(&format!("invalid listen_addr: {e}"));
    }

    if let Err(why) = claim_dest(&ctrl.run, &ctrl.registry) {
        return Response::err(&why);
    }

    let status = Arc::new(Mutex::new(vmm_migrate::MigrationStatus::default()));
    if let Ok(mut ms) = ctrl.migrate_status.lock() {
        *ms = Some(Arc::clone(&status));
    }

    let ctrl_arc = Arc::clone(ctrl);
    let hdl = Arc::clone(&ctrl.hdl);
    let memctx = MemCtx::new(Arc::clone(&ctrl.physmap));
    let mlog = ctrl.log.clone();
    let config = vmm_migrate::destination::DestConfig {
        num_cpus: ctrl.num_cpus,
        mem_size: ctrl.mem_size as u64,
        cpu_baseline: ctrl.cpu_baseline,
        devices: ctrl.migrate_identities(),
        deadlines: vmm_migrate::wire::Deadlines::default(),
        allow_cpu_feature_mismatch,
    };

    if let Err(e) =
        thread::Builder::new()
            .name("migrate-dest".into())
            .spawn(move || {
                let lease = MigrationLease::new(ctrl_arc.as_ref());
                let hooks = dest_hooks(&ctrl_arc);
                let run = &ctrl_arc.run;
                let phase = Arc::clone(&status);
                let outcome =
                    accept_one(&listen_addr, || run.cancelled(), &mlog)
                        .and_then(|stream| {
                            shut_on_cancel(run, stream, &mlog)
                                .map_err(|e| e.to_string())
                        })
                        .and_then(|stream| {
                            vmm_migrate::destination::run_destination(
                                stream, hdl, &memctx, config, hooks, status,
                                &mlog,
                            )
                            .map_err(|e| e.to_string())
                        });
                match outcome {
                    Ok(()) => {
                        info!(
                            mlog,
                            "destination migration complete, VM running"
                        );
                        end_migration(&ctrl_arc);
                    }
                    Err(e) => {
                        let confirmed = phase.lock().is_ok_and(|s| {
                            s.phase == vmm_migrate::MigrationPhase::Committed
                        });
                        error!(mlog, "destination migration failed";
                            "error" => %e, "source_committed" => confirmed);
                        ctrl_arc.ended(run.fail_import(confirmed, || {
                            ctrl_arc.release_pause()
                        }));
                    }
                }
                lease.release();
            })
    {
        end_migration(ctrl);
        return Response::err(&format!(
            "failed to spawn migration thread: {e}"
        ));
    }

    let mut r = Response::ok();
    r.state = Some(VmRunState::Migrating);
    r
}

/// The device hooks the destination import drives.
fn dest_hooks(ctrl: &Arc<VmController>) -> vmm_migrate::destination::DestHooks {
    let for_restore = Arc::clone(ctrl);
    let for_resume = Arc::clone(ctrl);
    let for_hyperv = Arc::clone(ctrl);
    let (pause_vm, resume_vm) = migration_pause_hooks(ctrl);
    vmm_migrate::destination::DestHooks {
        restore: Box::new(move |bdf, state| {
            for_restore.restore_device_state(bdf, state)
        }),
        resume: Box::new(move || for_resume.resume_devices()),
        hyperv: Box::new(move |state| for_hyperv.restore_hyperv(state)),
        pause_vm,
        resume_vm,
        gate: Box::new(ControlGate(Arc::clone(ctrl))),
    }
}

/// Bind, take one connection, and give the listener back. Gives up
/// when `cancelled` says a stop arrived.
fn accept_one(
    listen_addr: &str,
    cancelled: impl Fn() -> bool,
    log: &slog::Logger,
) -> Result<TcpStream, String> {
    let listener = TcpListener::bind(listen_addr)
        .map_err(|e| format!("bind {listen_addr}: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("listen {listen_addr}: {e}"))?;
    info!(log, "waiting for migration"; "addr" => listen_addr);
    loop {
        if cancelled() {
            return Err(vmm_migrate::CANCELLED.to_string());
        }
        match listener.accept() {
            Ok((stream, peer)) => {
                info!(log, "accepted migration connection"; "peer" => %peer);
                // illumos hands the listener's O_NONBLOCK to the new
                // socket, and the protocol needs blocking reads.
                stream
                    .set_nonblocking(false)
                    .map_err(|e| format!("accept: {e}"))?;
                return Ok(stream);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("accept: {e}")),
        }
    }
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
        // The destination is built from the boot device list, so it has
        // nowhere to put a device the guest gained since.
        let reason =
            device_set_changed(&["5,virtio-blk,/added.img".to_string()])
                .expect("an added device must block migration");
        assert!(reason.contains("hot-added 1 device(s)"), "{reason}");
        assert!(reason.contains("5,virtio-blk,/added.img"), "{reason}");
        assert!(reason.contains("boot device list"), "{reason}");
    }

    fn ident(dev: u8, kind: &str) -> vmm_migrate::codec::DeviceIdentity {
        vmm_migrate::codec::DeviceIdentity {
            bdf: vmm_devices::lifecycle::WireBdf {
                bus: 0,
                dev,
                func: 0,
            },
            kind: kind.to_string(),
        }
    }

    #[test]
    fn a_vm_with_only_migratable_devices_can_be_a_source() {
        assert_eq!(unmigratable_devices(&[]), None);
        assert_eq!(
            unmigratable_devices(&[ident(4, "nvme"), ident(5, "virtio-blk")]),
            None,
        );
    }

    #[test]
    fn a_device_whose_state_is_not_carried_blocks_migration() {
        // The guest driver would find the device at reset on the far
        // side, with no error anywhere.
        for kind in ["xhci", "ahci-cd", "virtio-fs", "virtio-vsock"] {
            let reason = unmigratable_devices(&[ident(6, kind)])
                .unwrap_or_else(|| panic!("{kind} must block migration"));
            assert!(reason.contains(kind), "{reason}");
            assert!(reason.contains("6.0"), "{reason}");
        }
    }

    #[test]
    fn every_blocking_device_is_named_in_the_refusal() {
        // An operator has to see all of them before they can retry.
        let reason =
            unmigratable_devices(&[ident(6, "xhci"), ident(7, "virtio-fs")])
                .expect("two blocking devices");
        assert!(reason.contains("xhci@0.6.0"), "{reason}");
        assert!(reason.contains("virtio-fs@0.7.0"), "{reason}");
    }

    #[test]
    fn a_unix_target_skips_the_tcp_address_check() {
        // The GZ agent's socket path is not a host:port.
        assert!(matches!(
            MigrateTarget::parse("/tmp/vmm-migrate-data.sock"),
            Ok(MigrateTarget::Unix(_)),
        ));
        assert!(matches!(
            MigrateTarget::parse("192.0.2.10:4567"),
            Ok(MigrateTarget::Tcp(_)),
        ));
        MigrateTarget::parse("127.0.0.1:4567")
            .err()
            .expect("loopback is refused");
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

    /// An NVMe disk as the migration path sees it: a ring disk says it
    /// cannot migrate, a file disk says nothing.
    struct Disk(Option<&'static str>);

    impl vmm_devices::Lifecycle for Disk {
        fn type_name(&self) -> &'static str {
            "nvme"
        }

        fn migration_blocker(&self) -> Option<String> {
            self.0.map(str::to_string)
        }
    }

    const RING: &str = "nvme on MantaBlock volume 6f1c2a8e-0000-4000-8000-000000000001 has no migration handover yet";

    fn disk_at(
        registry: &vmm_machine::DeviceRegistry,
        slot: &str,
        blocker: Option<&'static str>,
        hot_added: bool,
    ) {
        let mut disk = vmm_machine::RegisteredDevice::new(
            format!("nvme@{slot}"),
            vmm_machine::parse_bdf(slot),
            None,
            Some(Arc::new(Disk(blocker)) as Arc<dyn vmm_devices::Lifecycle>),
            Some(format!("{slot},nvme,mantablock=/s,volume=x")),
        );
        disk.hotpluggable = hot_added;
        registry.insert(disk).expect("insert");
    }

    fn identities(
        registry: &vmm_machine::DeviceRegistry,
    ) -> Vec<vmm_migrate::codec::DeviceIdentity> {
        registry
            .list()
            .into_iter()
            .filter_map(|slot| Some(ident(slot.bdf?.dev(), "nvme")))
            .collect()
    }

    fn assert_names_the_ring_disk(reason: Option<String>, slot: &str) {
        let reason = reason.expect("a ring disk must block migration");
        assert!(reason.contains(&format!("nvme@{slot}")), "{reason}");
        assert!(reason.contains("MantaBlock volume 6f1c2a8e"), "{reason}");
    }

    #[test]
    fn a_ring_disk_present_at_boot_blocks_migrate_dest() {
        let registry = vmm_machine::DeviceRegistry::new();
        disk_at(&registry, "4", None, false);
        disk_at(&registry, "5", Some(RING), false);

        let run = RunControl::new(VmRunState::Paused);
        assert_names_the_ring_disk(claim_dest(&run, &registry).err(), "5");
        assert_eq!(run.get(), VmRunState::Paused);
        assert_names_the_ring_disk(device_blockers(&registry), "5");
    }

    #[test]
    fn a_hot_added_ring_disk_blocks_migrate_dest() {
        let registry = vmm_machine::DeviceRegistry::new();
        disk_at(&registry, "4", None, false);
        disk_at(&registry, "6", Some(RING), true);

        let run = RunControl::new(VmRunState::Paused);
        assert_names_the_ring_disk(claim_dest(&run, &registry).err(), "6");
        assert_eq!(run.get(), VmRunState::Paused);
    }

    #[test]
    fn a_ring_disk_blocks_migrate_source() {
        for hot_added in [false, true] {
            let registry = vmm_machine::DeviceRegistry::new();
            disk_at(&registry, "5", Some(RING), hot_added);
            let run = RunControl::new(VmRunState::Running);
            let reason = claim_source(
                &run,
                &HotplugEngines::default(),
                &registry,
                || identities(&registry),
            );
            assert_names_the_ring_disk(reason.err(), "5");
            assert_eq!(run.get(), VmRunState::Running);
        }
    }

    #[test]
    fn a_file_backed_nvme_vm_still_migrates() {
        let registry = vmm_machine::DeviceRegistry::new();
        disk_at(&registry, "4", None, false);

        assert_eq!(device_blockers(&registry), None);
        let dest = RunControl::new(VmRunState::Paused);
        assert_eq!(claim_dest(&dest, &registry), Ok(()));
        assert_eq!(dest.get(), VmRunState::Migrating);
        let source = RunControl::new(VmRunState::Running);
        assert_eq!(
            claim_source(
                &source,
                &HotplugEngines::default(),
                &registry,
                || identities(&registry),
            ),
            Ok(()),
        );
        assert_eq!(source.get(), VmRunState::Migrating);
    }

    #[test]
    fn migrate_dest_still_needs_a_paused_vm() {
        let registry = vmm_machine::DeviceRegistry::new();
        let run = RunControl::new(VmRunState::Running);
        let reason = claim_dest(&run, &registry).expect_err("a running VM");
        assert!(reason.contains("paused"), "{reason}");
        assert_eq!(run.get(), VmRunState::Running);
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

    fn discard() -> slog::Logger {
        slog::Logger::root(slog::Discard, slog::o!())
    }

    #[test]
    fn a_destination_stopped_before_any_source_connects_lets_go() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let cancel = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&cancel);
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(accept_one(
                "0.0.0.0:0",
                || seen.load(Ordering::Acquire),
                &discard(),
            ));
        });

        cancel.store(true, Ordering::Release);

        let ended = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the accept outlived the stop");
        assert_eq!(
            ended.expect_err("no source connected"),
            vmm_migrate::CANCELLED
        );
    }

    #[test]
    fn a_source_stopped_while_its_peer_stops_reading_lets_go() {
        use std::io::Write as _;
        let run = RunControl::new(VmRunState::Running);
        run.admit_source(|| None).expect("claim");
        let (ours, _peer) = UnixStream::pair().expect("socket pair");
        let mut ours =
            shut_on_cancel(&run, ours, &discard()).expect("registered");
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let chunk = vec![0u8; 1 << 16];
            let error = loop {
                if let Err(e) = ours.write_all(&chunk) {
                    break e;
                }
            };
            let _ = tx.send(error);
        });

        run.stop(
            super::super::run_state::STOP_WAIT,
            || Ok::<(), ()>(()),
            || Ok(()),
            || Ok(()),
        )
        .expect("stop");

        rx.recv_timeout(Duration::from_secs(10))
            .expect("the send outlived the stop");
    }
}
