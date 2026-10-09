// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Migration command handlers and their address guards.

use std::io;
use std::net::{Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use slog::{debug, error, info};

use vmm_core::mem::MemCtx;
use vmm_machine::HotplugEngines;

use vmm_migrate::wait::unless_cancelled;

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
const UNMIGRATABLE: &[&str] =
    &["ahci-cd", "virtio-console", "virtio-fs", "virtio-vsock"];

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
            let why = match (&slot.pci, &slot.lifecycle) {
                // passthru: the state is in host hardware, and its DMA
                // writes are not dirty-tracked.
                (Some(_), None) => {
                    "a PCI device with no lifecycle carries no state".into()
                }
                (_, Some(lifecycle)) => lifecycle.migration_blocker()?,
                (None, None) => return None,
            };
            Some(format!("{}: {why}", slot.id))
        })
        .collect();
    if blocking.is_empty() {
        return None;
    }
    Some(format!("this VM cannot migrate: {}", blocking.join("; ")))
}

/// Every device whose migration an operator must accept first, unless
/// `allowed`.
pub(crate) fn waivable_blockers(
    registry: &vmm_machine::DeviceRegistry,
    allowed: bool,
) -> Option<String> {
    if allowed {
        return None;
    }
    let blocking: Vec<String> = registry
        .list()
        .into_iter()
        .filter_map(|slot| {
            let why = slot.lifecycle?.migration_waivable()?;
            Some(format!("{}: {why}", slot.id))
        })
        .collect();
    if blocking.is_empty() {
        return None;
    }
    Some(format!(
        "this VM cannot migrate: {}; send \
         allow_engine_without_migrate_v1 to accept the risk",
        blocking.join("; ")
    ))
}

/// Why this VM cannot be a migration source, or `None`.
fn source_refusal(
    hotplug: &HotplugEngines,
    registry: &vmm_machine::DeviceRegistry,
    identities: &[vmm_migrate::codec::DeviceIdentity],
    allow_unproven: bool,
) -> Option<String> {
    let (cpus, bytes) = growth(hotplug);
    device_blockers(registry)
        .or_else(|| waivable_blockers(registry, allow_unproven))
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
    allow_unproven: bool,
) -> Result<(), String> {
    run.admit_source(|| {
        source_refusal(hotplug, registry, &identities(), allow_unproven)
    })
}

/// Move a paused VM to `Migrating`, or say why it cannot take an import.
fn claim_dest(
    run: &RunControl,
    registry: &vmm_machine::DeviceRegistry,
) -> Result<(), String> {
    run.admit_import(|| {
        device_blockers(registry).or_else(|| handover_import(registry))
    })
}

/// A VM that is already running attached its ring disks at boot, and
/// the source still owns those volumes. Only a VM started with
/// `--migrate-listen` builds them detached.
fn handover_import(registry: &vmm_machine::DeviceRegistry) -> Option<String> {
    let rings: Vec<String> = registry
        .list()
        .into_iter()
        .filter(|slot| {
            slot.lifecycle
                .as_ref()
                .is_some_and(|d| d.migration_spec_options().is_some())
        })
        .map(|slot| slot.id)
        .collect();
    if rings.is_empty() {
        return None;
    }
    Some(format!(
        "migrate-dest cannot take a MantaBlock disk ({}); start the \
         destination with --migrate-listen",
        rings.join(" ")
    ))
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
    ask: SourceAsk,
) -> Response {
    let target = match MigrateTarget::parse(&target_addr) {
        Ok(target) => target,
        Err(e) => return Response::err(&format!("invalid target_addr: {e}")),
    };
    let gate = match ask.gate.as_deref().map(agent_socket).transpose() {
        Ok(gate) => gate,
        Err(e) => return Response::err(&format!("invalid gate: {e}")),
    };
    let gate_budget = match gate_budget(ask.gate_timeout_ms) {
        Ok(budget) => budget,
        Err(e) => return Response::err(&e),
    };
    let zfs_barrier = ask.zfs_barrier;

    // The claim and the new status are one step, so a migrate-cancel
    // never reads the last migration's gate for this one.
    let status = Arc::new(Mutex::new(vmm_migrate::MigrationStatus::default()));
    {
        let mut slot = ctrl.migrate_status.lock();
        if let Err(why) = claim_source(
            &ctrl.run,
            &ctrl.hotplug,
            &ctrl.registry,
            || ctrl.migrate_identities(),
            ask.allow_unproven,
        ) {
            return Response::err(&why);
        }
        if let Ok(slot) = slot.as_mut() {
            **slot = Some(Arc::clone(&status));
        }
    }
    ctrl.set_allow_unproven(ask.allow_unproven);
    let status_for_failure = Arc::clone(&status);

    let ctrl_arc = Arc::clone(ctrl);
    let hdl = Arc::clone(&ctrl.hdl);
    let memctx = MemCtx::new(Arc::clone(&ctrl.physmap));
    let mlog = ctrl.log.clone();
    let config = vmm_migrate::source::SourceConfig {
        num_cpus: ctrl.num_cpus,
        mem_size: ctrl.mem_size as u64,
        cpu_baseline: ctrl.cpu_baseline,
        devices: ctrl.migrate_identities(),
        platform: crate::platform::identities(&ctrl.registry),
        deadlines: vmm_migrate::wire::Deadlines::default(),
        zfs_barrier: zfs_barrier.map(PathBuf::from),
        gate,
        gate_budget,
        handover: ctrl.has_handover_disks(),
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
                        connect_unix(run, Path::new(&path), &mlog)
                            .map_err(SourceOutcome::Failed)
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
                        connect_tcp(run, addr, &mlog)
                            .map_err(SourceOutcome::Failed)
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
                    Err(e @ SourceOutcome::Failed(_))
                        if held_by_gate(&ctrl_arc) =>
                    {
                        // The orchestrator recorded the go. Only its
                        // order brings the guest back here.
                        vmm_migrate::record_failure(&status_for_failure, &e);
                        error!(mlog, "source migration failed after the \
                            gate's go; this VM stays paused until \
                            migrate-resume-source or a stop";
                            "error" => %e);
                        ctrl_arc.ended(
                            run.end_committed(|| ctrl_arc.release_pause()),
                        );
                    }
                    Err(e @ SourceOutcome::Failed(_)) => {
                        // A connect failure never reached the driver.
                        vmm_migrate::record_failure(&status_for_failure, &e);
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

/// Whether the gate said go and the source never committed.
fn held_by_gate(ctrl: &VmController) -> bool {
    ctrl.migration_status().is_some_and(|s| {
        s.gate == vmm_migrate::GateState::Go
            && s.phase != vmm_migrate::MigrationPhase::Committed
    })
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

/// Let a stop shut `stream` down. A send or receive blocked on a peer
/// that went quiet returns at once, and the thread rolls back.
fn shut_on_stop<S: Shut>(
    run: &RunControl,
    stream: S,
    log: &slog::Logger,
) -> io::Result<S> {
    let clone = stream.try_clone()?;
    let log = log.clone();
    run.on_stop(Box::new(move || {
        if let Err(e) = clone.shutdown(Shutdown::Both) {
            debug!(log, "the migration socket was already closed";
                "error" => %e);
        }
    }));
    Ok(stream)
}

/// Connect to the GZ agent's socket, giving up on a stop.
fn connect_unix(
    run: &RunControl,
    path: &Path,
    log: &slog::Logger,
) -> Result<UnixStream, String> {
    let stream = vmm_migrate::connect::unix(path, || run.cancelled())
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{}: {e}", path.display()))?;
    shut_on_stop(run, stream, log).map_err(|e| e.to_string())
}

/// Connect to a TCP peer, giving up on a stop.
///
/// Only the name lookup runs on a helper thread: nothing can cut it
/// short, and it holds no socket. One left behind ends on its own.
fn connect_tcp(
    run: &RunControl,
    addr: String,
    log: &slog::Logger,
) -> Result<TcpStream, String> {
    let lookup = addr.clone();
    let peers = unless_cancelled(
        "migrate-resolve",
        || run.cancelled(),
        move |_| lookup.to_socket_addrs().map(Vec::from_iter),
    )
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{addr}: {e}"))?;
    let mut last = None;
    for peer in peers {
        let connected =
            vmm_migrate::connect::tcp(peer, CONNECT_TIMEOUT, || {
                run.cancelled()
            })
            .map_err(|e| e.to_string())?;
        match connected {
            Ok(stream) => {
                return shut_on_stop(run, stream, log)
                    .map_err(|e| e.to_string())
            }
            Err(e) => last = Some(format!("{peer}: {e}")),
        }
    }
    Err(last.unwrap_or_else(|| format!("{addr}: no address")))
}

/// The device hooks the source phases drive.
fn source_hooks(ctrl: &Arc<VmController>) -> vmm_migrate::source::SourceHooks {
    let for_pre_pause = Arc::clone(ctrl);
    let for_flush = Arc::clone(ctrl);
    let for_resume = Arc::clone(ctrl);
    let for_export = Arc::clone(ctrl);
    let for_platform = Arc::clone(ctrl);
    let for_hyperv = Arc::clone(ctrl);
    let for_committed = Arc::clone(ctrl);
    let (pause_vm, resume_vm) = migration_pause_hooks(ctrl);
    vmm_migrate::source::SourceHooks {
        pre_pause: Box::new(move || {
            for_pre_pause.run.parking_devices();
            for_pre_pause.pause_devices_for_migration()
        }),
        flush: Box::new(move |left| {
            for_flush.flush_devices_for_migration(left)
        }),
        resume: Box::new(move || {
            for_resume.resume_devices_after_migration();
            for_resume.run.devices_resumed();
        }),
        export: Box::new(move || for_export.export_device_state()),
        export_platform: Box::new(move || {
            crate::platform::export(&for_platform.registry)
        }),
        committed: Box::new(move || for_committed.detach_backing()),
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

    fn on_stop(&self, shut: Box<dyn FnOnce() + Send>) {
        self.0.run.on_stop(shut);
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
    let source = match MigrateTarget::parse(&listen_addr) {
        Ok(source) => source,
        Err(e) => return Response::err(&format!("invalid listen_addr: {e}")),
    };

    if let Err(why) = claim_dest(&ctrl.run, &ctrl.registry) {
        return Response::err(&why);
    }
    // A failed import may have installed them. The next RAM passes
    // would land in the overlay pages, and an import at the same GPA
    // keeps them.
    if let Some(hyperv) = &ctrl.hyperv {
        hyperv.pause_overlays();
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
        platform: crate::platform::identities(&ctrl.registry),
        deadlines: vmm_migrate::wire::Deadlines::default(),
        hyperv: ctrl.hyperv.is_some(),
        allow_cpu_feature_mismatch,
    };

    if let Err(e) =
        thread::Builder::new()
            .name("migrate-dest".into())
            .spawn(move || {
                let lease = MigrationLease::new(ctrl_arc.as_ref());
                let hooks = dest_hooks(&ctrl_arc);
                let run = &ctrl_arc.run;
                let outcome = match source {
                    MigrateTarget::Unix(path) => {
                        connect_unix_patient(run, Path::new(&path), &mlog)
                            .and_then(|stream| {
                                vmm_migrate::destination::run_destination(
                                    stream, hdl, &memctx, config, hooks,
                                    status, &mlog,
                                )
                                .map_err(|e| e.to_string())
                            })
                    }
                    MigrateTarget::Tcp(addr) => {
                        accept_one(&addr, || run.cancelled(), &mlog)
                            .and_then(|stream| {
                                shut_on_stop(run, stream, &mlog)
                                    .map_err(|e| e.to_string())
                            })
                            .and_then(|stream| {
                                vmm_migrate::destination::run_destination(
                                    stream, hdl, &memctx, config, hooks,
                                    status, &mlog,
                                )
                                .map_err(|e| e.to_string())
                            })
                    }
                };
                match outcome {
                    Ok(()) => {
                        info!(
                            mlog,
                            "destination migration complete, VM running"
                        );
                        end_migration(&ctrl_arc);
                    }
                    Err(e) => {
                        let confirmed = ctrl_arc.source_committed();
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

/// How long a destination waits for the agent's socket to appear. The
/// agent and the request come together, so the socket may lag a little.
const AGENT_SOCKET_WAIT: Duration = Duration::from_secs(15);

/// Connect to the agent's socket, waiting for it to appear, and giving
/// up on a stop.
fn connect_unix_patient(
    run: &RunControl,
    path: &Path,
    log: &slog::Logger,
) -> Result<UnixStream, String> {
    let until = std::time::Instant::now() + AGENT_SOCKET_WAIT;
    loop {
        match connect_unix(run, path, log) {
            Ok(stream) => return Ok(stream),
            Err(_) if run.cancelled() => {
                return Err(vmm_migrate::CANCELLED.to_string())
            }
            Err(e) if std::time::Instant::now() >= until => return Err(e),
            Err(_) => thread::sleep(ACCEPT_POLL),
        }
    }
}

/// The device hooks the destination import drives.
fn dest_hooks(ctrl: &Arc<VmController>) -> vmm_migrate::destination::DestHooks {
    let for_restore = Arc::clone(ctrl);
    let platform = crate::platform::devices(&ctrl.registry);
    let for_attach = Arc::clone(ctrl);
    let for_resume = Arc::clone(ctrl);
    let for_hyperv = Arc::clone(ctrl);
    let (pause_vm, resume_vm) = migration_pause_hooks(ctrl);
    vmm_migrate::destination::DestHooks {
        restore: Box::new(move |bdf, state| {
            for_restore.restore_device_state(bdf, state)
        }),
        restore_platform: Box::new(move |id, state| {
            crate::platform::restore(&platform, id, state)
        }),
        attach: Box::new(move || {
            for_attach
                .attach_backing(std::time::Instant::now() + RING_ATTACH_DEFAULT)
        }),
        resume: Box::new(move || {
            for_resume.resume_devices();
            for_resume.run.devices_resumed();
        }),
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

/// The orchestrator's `source` order: this VM keeps the guest.
///
/// Taken only by a source that may not resume by itself: one the gate
/// said go to, or one that committed and never heard the destination
/// start the guest. A source whose destination confirmed is `stopped`
/// and refuses. After the commit the disks were given up, so each one
/// attaches anew before any device resumes.
pub(super) fn migrate_resume_source(ctrl: &Arc<VmController>) -> Response {
    match ctrl
        .resume_held_source(std::time::Instant::now() + RING_ATTACH_DEFAULT)
    {
        Ok(()) => {
            info!(
                ctrl.log,
                "the orchestrator gave the guest back to \
                this source; it runs again"
            );
            let mut r = Response::ok();
            r.state = Some(VmRunState::Running);
            r
        }
        Err(why) => Response::err(&why),
    }
}

/// Abort a source migration before the gate's go. The stream closes
/// at once and the guest keeps running here.
pub(super) fn migrate_cancel(ctrl: &Arc<VmController>) -> Response {
    let status = ctrl.migrate_status.lock().ok().and_then(|ms| ms.clone());
    // Held across the cancel: the gate records its go under this lock,
    // so a go and a cancel cannot both win.
    let held = status.as_ref().map(|s| s.lock());
    let gone = || {
        held.as_ref().is_some_and(|s| {
            s.as_ref()
                .map_or(true, |s| s.gate == vmm_migrate::GateState::Go)
        })
    };
    match ctrl.run.cancel_source(gone) {
        Ok(()) => {
            info!(
                ctrl.log,
                "migrate-cancel: the source migration is \
                cancelled"
            );
            let mut r = Response::ok();
            r.state = Some(ctrl.state());
            r
        }
        Err(why) => Response::err(why),
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
            let mut data = serde_json::json!({
                "migrate_gate": s.gate.to_string(),
            });
            if let Some(why) = &s.error {
                data["migrate_error"] = why.clone().into();
            }
            if let Some(skew) = s.clock_skew_ms {
                data["migrate_clock_skew_ms"] = skew.into();
            }
            if let Some(rings) = ctrl.rings_state() {
                data["migrate_rings"] = rings.into();
            }
            r.data = Some(data);
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
    let mut slots: Vec<String> = argv_slots
        .iter()
        .map(|spec| with_destination_options(spec, registry))
        .collect();
    slots.extend(vmm_machine::hotplug_specs(registry));
    slots
}

/// `spec` with the options its device says a destination needs, each
/// in place of one of the same key the spec already had.
fn with_destination_options(
    spec: &str,
    registry: &vmm_machine::DeviceRegistry,
) -> String {
    let id = vmm_machine::devspec::device_id(spec);
    let Some(extra) = registry
        .list()
        .into_iter()
        .find(|slot| slot.id == id)
        .and_then(|slot| slot.lifecycle?.migration_spec_options())
    else {
        return spec.to_string();
    };
    let keys: Vec<&str> = extra
        .split(',')
        .filter_map(|opt| opt.split_once('=').map(|(k, _)| k))
        .collect();
    let mut parts: Vec<&str> = spec
        .split(',')
        .filter(|opt| {
            !opt.split_once('=').is_some_and(|(k, _)| keys.contains(&k))
        })
        .collect();
    parts.push(&extra);
    parts.join(",")
}

mod ask;
pub(super) use ask::SourceAsk;
use ask::{agent_socket, gate_budget, RING_ATTACH_DEFAULT};

#[cfg(test)]
mod tests;
