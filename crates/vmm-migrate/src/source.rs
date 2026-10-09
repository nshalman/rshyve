// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The migration source.
//!
//! The order of the phases is the invariant this module owns:
//!
//! 1. Pause the devices and wait for them to quiesce. Every thread that
//!    writes guest memory has to be stopped before the last dirty pass,
//!    or that pass ships a used ring the destination never sees filled.
//! 2. Pause the vCPUs.
//! 3. Flush every backing store, then wait for the ZFS barrier. The
//!    destination disk has to hold every write the guest was told was
//!    complete before the guest resumes there.
//! 4. Wait at the cutover gate for the orchestrator's go.
//! 5. Ship the last dirty pass, the time data and the device state.
//! 6. Hand over, then detach every disk the destination attaches anew.
//!
//! Anything that fails from step 1 on rolls the source back: devices
//! resume, the rings a migration paused go back to work, and the vCPUs
//! run again. After the gate's go nothing rolls back: the source stays
//! paused until the orchestrator tells it which side keeps the guest.

use std::io::Read;
use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vmm_core::cpuid::CpuBaseline;
use vmm_core::hdl::VmmHdl;
use vmm_core::mem::MemCtx;
use vmm_devices::lifecycle::{
    DeviceStateError, DeviceStatePayload, HypervMigrateState, PlatformIdentity,
    PlatformStatePayload,
};
use vmm_devices::FlushError;

use crate::codec::{
    DeviceIdentity, Message, MigrateError, MigrationPreamble, ProtocolOffer,
    ProtocolSelect, PAGE_BATCH_FLAG_ZSTD,
};
use crate::cpu;
use crate::limits::{BATCH_SIZE, ZSTD_LEVEL};
use crate::pages::{
    are_contiguous, build_contiguous_gpas, page_buffer_len, read_pages,
};
use crate::protocol::{
    protocol_blocker, PageIter, PAGE_SIZE, PROTOCOL_RON, PROTOCOL_RON_V6,
    SOURCE_OFFER,
};
use crate::record_failure;
use crate::state;
use crate::wait::{unless_cancelled, Abandoned};
use crate::wire::{
    cancel_or, not_cancelled, set_phase, Chan, Deadlines, Transport,
};
use crate::xsave::XsaveLayout;
use crate::GateState;
use crate::MigrationPhase;
use crate::MigrationStatus;
use crate::StopGate;
use crate::VmPauseFn;

/// The byte the ZFS barrier reports when the final incremental landed.
pub const ZFS_BARRIER_OK: u8 = 0;

/// Stop every device worker and every kernel ring, then wait for them
/// to quiesce. Runs before the vCPUs pause.
///
/// A device that keeps running writes guest memory after the final
/// dirty pass, and a kernel ring that keeps running moves the indices
/// the export is about to read.
pub type DevicePrePauseFn =
    Box<dyn FnOnce() -> Result<(), DeviceStateError> + Send>;

/// Read out every device's migration state. A pure read: the pause
/// already happened.
pub type DeviceExportFn = Box<
    dyn FnOnce() -> Result<Vec<DeviceStatePayload>, DeviceStateError> + Send,
>;

/// Read out the state of every device with no PCI address, each under
/// its registry id. A pure read, like [`DeviceExportFn`].
pub type PlatformExportFn = Box<
    dyn FnOnce() -> Result<Vec<PlatformStatePayload>, DeviceStateError> + Send,
>;

/// Flush device writeback caches. Called after the vCPU pause has
/// quiesced the vCPUs and before the ZFS barrier, so `zfs send` sees
/// every write the guest was told was complete.
///
/// Runs on a helper a stop can leave behind, so it must stop between
/// devices once [`Abandoned`] is set.
pub type DeviceFlushFn =
    Box<dyn FnOnce(&Abandoned) -> Result<(), DeviceFlushError> + Send>;

/// Why the backing stores were not all flushed.
#[derive(Debug, thiserror::Error)]
pub enum DeviceFlushError {
    #[error("{device}: {source}")]
    Device {
        device: &'static str,
        source: FlushError,
    },
    /// The flush ended before it reached every device.
    #[error("{0}")]
    Stopped(String),
}

/// Put the devices back to work after a migration that failed once the
/// guest was already paused.
pub type DeviceResumeFn = Box<dyn Fn() + Send>;

/// Runs once, right after the commit went out: give up every disk the
/// destination attaches anew. The source cannot take the commit back,
/// so a failure is the owner's to report.
pub type CommittedFn = Box<dyn FnOnce() + Send>;

/// Read the Hyper-V enlightenment out, and drop its overlay pages
/// first so the RAM image carries the guest's own bytes.
///
/// `None` for a VM started without `--hyperv`.
pub type HypervExportFn =
    Box<dyn FnOnce() -> Option<HypervMigrateState> + Send>;

/// What the source needs from its owner, beyond the stream.
pub struct SourceConfig {
    pub num_cpus: u32,
    pub mem_size: u64,
    pub cpu_baseline: CpuBaseline,
    /// Every device that carries migration state, so the destination
    /// can compare topology before it takes a page.
    pub devices: Vec<DeviceIdentity>,
    /// The same for the devices with no PCI address.
    pub platform: Vec<PlatformIdentity>,
    pub deadlines: Deadlines,
    /// Where the GZ agent reports the final ZFS incremental.
    pub zfs_barrier: Option<PathBuf>,
    /// The cutover gate's socket. See [`crate::gate`].
    pub gate: Option<PathBuf>,
    /// How long the gate waits for an answer.
    pub gate_budget: Duration,
    /// A disk changes hands at the commit, which only a destination of
    /// the current protocol does.
    pub handover: bool,
}

/// The device lifecycle hooks the phases drive.
pub struct SourceHooks {
    pub pre_pause: DevicePrePauseFn,
    pub flush: DeviceFlushFn,
    pub export: DeviceExportFn,
    pub export_platform: PlatformExportFn,
    pub resume: DeviceResumeFn,
    pub committed: CommittedFn,
    pub hyperv: HypervExportFn,
    pub pause_vm: VmPauseFn,
    pub resume_vm: VmPauseFn,
    pub gate: Box<dyn StopGate>,
}

pub fn run_source<S: Transport>(
    stream: S,
    hdl: Arc<VmmHdl>,
    memctx: &MemCtx,
    config: SourceConfig,
    hooks: SourceHooks,
    status: Arc<Mutex<MigrationStatus>>,
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    let result = run_watched(stream, hdl, memctx, config, hooks, &status, log);
    if let Err(error) = &result {
        record_failure(&status, error);
    }
    result
}

fn run_watched<S: Transport>(
    stream: S,
    hdl: Arc<VmmHdl>,
    memctx: &MemCtx,
    config: SourceConfig,
    hooks: SourceHooks,
    status: &Arc<Mutex<MigrationStatus>>,
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    let (stream, watch) =
        crate::stall::watch(stream, config.deadlines.stall, log)
            .map_err(|e| MigrateError::Io(format!("stall watchdog: {e}")))?;
    let mut chan = Chan::new(
        stream,
        tungstenite::protocol::Role::Client,
        config.deadlines,
    )?;

    let resume = hooks.resume;
    let result = run_inner(
        &mut chan,
        &hdl,
        memctx,
        &config,
        hooks.pre_pause,
        hooks.flush,
        (hooks.export, hooks.export_platform),
        hooks.hyperv,
        hooks.committed,
        &hooks.pause_vm,
        hooks.gate.as_ref(),
        status,
        log,
    );
    let result = result.map_err(|error| stalled_or(&watch, error));
    if let Err(error) = &result {
        if crate::is_cancel(error) {
            chan.report_failure(error, log);
        }
    }

    let (phase, gate) = status
        .lock()
        .map(|s| (s.phase, s.gate))
        .unwrap_or((MigrationPhase::Error, GateState::None));
    finish_source(result, phase, gate, log, resume, hooks.resume_vm)
}

/// A send the watchdog cut short fails with the stall, not with the
/// broken pipe it caused.
fn stalled_or(
    watch: &crate::stall::Watch,
    error: MigrateError,
) -> MigrateError {
    if !watch.stalled() {
        return error;
    }
    match error {
        MigrateError::Committed(why) => {
            MigrateError::Committed(format!("{}: {why}", watch.reason()))
        }
        _ => MigrateError::Io(watch.reason()),
    }
}

/// Roll the source back when a migration failed after the pause.
fn finish_source<E, F>(
    result: Result<(), MigrateError>,
    phase: MigrationPhase,
    gate: GateState,
    log: &slog::Logger,
    device_resume_fn: DeviceResumeFn,
    resume_vm: F,
) -> Result<(), MigrateError>
where
    E: std::fmt::Display,
    F: FnOnce() -> Result<(), E>,
{
    // A failure before the pause changed nothing. One at or after the
    // commit cannot be undone: the destination may be running the
    // guest, so the source stays paused and an operator decides. After
    // the gate's go the orchestrator's record says the guest may move,
    // so only its order resumes the source.
    let held = gate == GateState::Go;
    let rollback = rollback_phase(phase) && !held;
    if result.is_err() && held && rollback_phase(phase) {
        slog::error!(
            log,
            "the migration failed after the gate's go; the \
            source stays paused until the orchestrator orders it back"
        );
    }
    if result.is_err() && rollback {
        // Device workers must be available before vCPUs can issue more I/O.
        device_resume_fn();
        if let Err(error) = resume_vm() {
            slog::warn!(log, "failed to resume source VM after migration abort";
                "error" => %error);
        }
    }

    result
}

fn rollback_phase(phase: MigrationPhase) -> bool {
    (MigrationPhase::Pause as u8..MigrationPhase::Committed as u8)
        .contains(&(phase as u8))
}

#[allow(clippy::too_many_arguments)]
fn run_inner<S: Transport>(
    chan: &mut Chan<S>,
    hdl: &Arc<VmmHdl>,
    memctx: &MemCtx,
    config: &SourceConfig,
    device_pre_pause_fn: DevicePrePauseFn,
    device_flush_fn: DeviceFlushFn,
    (device_export_fn, platform_export_fn): (DeviceExportFn, PlatformExportFn),
    hyperv_export_fn: HypervExportFn,
    committed: CommittedFn,
    pause_vm: &VmPauseFn,
    gate: &dyn StopGate,
    status: &Arc<Mutex<MigrationStatus>>,
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    // ── Sync ──
    set_phase(status, MigrationPhase::Sync);

    chan.send_uncommitted(
        gate,
        &Message::serialized(&ProtocolOffer {
            protocols: SOURCE_OFFER.iter().map(|v| v.to_string()).collect(),
        })?,
    )?;

    let sel: ProtocolSelect = chan.expect_serialized()?;
    if !SOURCE_OFFER.contains(&sel.protocol.as_str()) {
        return Err(MigrateError::ProtocolMismatch(crate::wire::peer_text(
            &sel.protocol,
        )));
    }
    // Found out here, not at the export: by then the gate may have said
    // go, and the source could not roll back by itself.
    if let Some(why) =
        protocol_blocker(&sel.protocol, &config.devices, &config.platform)
    {
        return Err(MigrateError::ProtocolMismatch(why));
    }
    if config.handover && sel.protocol == PROTOCOL_RON_V6 {
        return Err(MigrateError::ProtocolMismatch(format!(
            "the destination speaks {}, which cannot attach a disk after \
             the commit",
            crate::wire::peer_text(&sel.protocol)
        )));
    }

    let current_peer = sel.protocol == PROTOCOL_RON;
    if !current_peer && state::carries_pmu_amd(hdl)? {
        return Err(MigrateError::ProtocolMismatch(format!(
            "the destination speaks {}, which cannot carry the AMD PMU \
             state; both ends need {PROTOCOL_RON}",
            crate::wire::peer_text(&sel.protocol)
        )));
    }

    let mut devices = config.devices.clone();
    devices.sort();
    let mut platform = config.platform.clone();
    platform.sort();
    let xsave = XsaveLayout::local(hdl)?;
    chan.send_uncommitted(
        gate,
        &Message::serialized(&MigrationPreamble {
            num_cpus: config.num_cpus,
            mem_size: config.mem_size,
            devices,
            platform,
            cpu_features: Some(cpu::local_features(config.cpu_baseline)),
            xsave: xsave.clone(),
            wall_clock_ns: Some(crate::clock::now_ns()?),
        })?,
    )?;
    chan.expect_okay()?;

    // ── RamPushPrePause: send ALL pages ──
    set_phase(status, MigrationPhase::RamPushPrePause);
    let mut track = kernel_tracking(hdl);
    push_ram_all(chan, &mut track, memctx, gate, status)?;

    // ── Iterative convergence ──
    const MAX_ITERS: u32 = 5;
    const CONVERGE_THRESHOLD: u64 = 1024;

    for iter in 1..=MAX_ITERS {
        std::thread::sleep(Duration::from_millis(500));
        let dirty = push_ram(chan, &mut track, memctx, gate, status)?;
        slog::info!(log, "convergence"; "iter" => iter, "dirty" => dirty);
        if dirty <= CONVERGE_THRESHOLD {
            break;
        }
    }

    // ── Pause ──
    // From here the guest is stopped and every failure has to roll
    // back, so the phase is recorded before the first step that can
    // fail.
    set_phase(status, MigrationPhase::Pause);
    chan.enter_post_pause()?;
    device_pre_pause_fn()
        .map_err(|e| MigrateError::State(format!("device pause: {e}")))?;
    pause_vm().map_err(|e| {
        cancel_or(gate, MigrateError::Io(format!("pause: {e}")))
    })?;
    not_cancelled(gate)?;

    // zvol WCE=1 means writes the guest was told were complete may
    // still sit in cache. `zfs send` ships only committed pool state.
    // A durable sync has no deadline, so a stop does not wait for it.
    // One left behind stops after its current device, and teardown
    // waits for it.
    slog::info!(log, "flushing device backing caches");
    unless_cancelled("migrate-flush", || gate.cancelled(), device_flush_fn)?
        .map_err(|error| {
            MigrateError::Io(format!("device flush failed: {error}"))
        })?;

    if let Some(barrier) = config.zfs_barrier.as_deref() {
        zfs_barrier_wait(barrier, config.deadlines.zfs_barrier, gate, log)?;
    }

    if let Some(path) = config.gate.as_deref() {
        crate::gate::wait(path, config.gate_budget, gate, status, log)?;
    }

    // The destination holds its short post-pause deadline from this
    // signal on, so it goes out only after the waits above.
    chan.send_uncommitted(gate, &Message::PauseSignal)?;

    // The overlay pages go before the final pass: the destination
    // reinstalls them from the MSR values, so shipping ours would give
    // the guest our bytes where its own belong.
    let hyperv = hyperv_export_fn();

    // ── Post-pause dirty ──
    // Every writer stopped in the pre-pause step, so this pass is the
    // last word on guest memory.
    set_phase(status, MigrationPhase::RamPushPostPause);
    let final_dirty = push_ram(chan, &mut track, memctx, gate, status)?;
    slog::info!(log, "post-pause"; "dirty" => final_dirty);

    // ── TimeData ──
    set_phase(status, MigrationPhase::TimeData);
    let time_data = state::export_time_data(hdl, log)?;
    chan.send_uncommitted(gate, &Message::serialized(&time_data)?)?;

    // ── DeviceState ──
    set_phase(status, MigrationPhase::DeviceState);
    let mut dev_state = state::export_device_state(
        hdl,
        config.num_cpus,
        &xsave,
        current_peer,
        log,
    )?;
    dev_state.emulated = device_export_fn()
        .map_err(|e| MigrateError::State(format!("device export: {e}")))?;
    dev_state.platform = platform_export_fn()
        .map_err(|e| MigrateError::State(format!("device export: {e}")))?;
    dev_state.hyperv = hyperv;
    slog::info!(log, "exporting device state";
        "devices" => dev_state.emulated.len(),
        "platform" => dev_state.platform.len());
    chan.send_uncommitted(gate, &Message::serialized(&dev_state)?)?;

    // ── RamPull (none needed) ──
    set_phase(status, MigrationPhase::RamPull);
    chan.send_uncommitted(gate, &Message::MemEnd)?;

    hand_over(
        chan,
        gate,
        || check_clean(&mut track, memctx),
        committed,
        status,
        log,
    )
}

/// Three steps, and the order is the whole point. The destination says
/// it holds everything; this end commits, which is the promise never to
/// run the guest again; only then does the destination start it. A
/// destination that resumed before the commit would run a second copy
/// of the guest against the same disk whenever this end rolled back.
fn hand_over<S: Transport>(
    chan: &mut Chan<S>,
    gate: &dyn StopGate,
    clean: impl FnOnce() -> Result<(), MigrateError>,
    committed: CommittedFn,
    status: &Arc<Mutex<MigrationStatus>>,
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    set_phase(status, MigrationPhase::Finish);
    chan.expect_okay()?;
    if let Err(error) = clean() {
        chan.report_failure(&error, log);
        return Err(error);
    }
    gate.commit()?;

    set_phase(status, MigrationPhase::Committed);
    let sent = chan.send(&Message::Okay);
    // Whether or not the commit arrived, this end never runs the guest
    // again by itself, and the destination cannot attach a disk this
    // end still holds.
    committed();
    sent.map_err(|e| {
        MigrateError::Committed(format!(
            "the commit may not have reached the destination: {e}"
        ))
    })?;
    if let Err(e) = chan.set_read_deadline(chan.deadlines().started) {
        slog::warn!(log, "the wait for the start keeps the short deadline";
            "error" => %e);
    }
    chan.expect_okay().map_err(|e| {
        MigrateError::Committed(format!(
            "the destination never confirmed it started the guest: {e}"
        ))
    })?;

    chan.close();
    Ok(())
}

/// Wait for the GZ agent to finish the final ZFS incremental.
///
/// The connect is the signal that the vCPUs are paused; the byte back
/// is the agent's verdict. Every failure fails the migration: a guest
/// that resumes on the destination with a disk older than its own
/// memory has already acknowledged writes that disk never received.
fn zfs_barrier_wait(
    barrier_path: &Path,
    budget: Duration,
    gate: &dyn StopGate,
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    slog::info!(log, "waiting for post-pause ZFS sync";
        "barrier" => %barrier_path.display());
    let mut stream = crate::connect::unix(barrier_path, || gate.cancelled())?
        .map_err(|e| {
        MigrateError::Io(format!("ZFS barrier connect failed: {e}"))
    })?;
    let clone = stream
        .try_clone()
        .map_err(|e| MigrateError::Io(format!("ZFS barrier socket: {e}")))?;
    let shut_log = log.clone();
    gate.on_stop(Box::new(move || {
        if let Err(e) = clone.shutdown(Shutdown::Both) {
            slog::debug!(shut_log, "the ZFS barrier was already closed";
                "error" => %e);
        }
    }));
    stream.set_read_timeout(Some(budget)).map_err(|e| {
        MigrateError::Io(format!("ZFS barrier read timeout: {e}"))
    })?;
    let mut status = [0u8; 1];
    stream.read_exact(&mut status).map_err(|e| {
        cancel_or(
            gate,
            MigrateError::Io(format!("ZFS barrier reported nothing: {e}")),
        )
    })?;
    if status[0] != ZFS_BARRIER_OK {
        return Err(MigrateError::Io(format!(
            "ZFS barrier reported failure (status {})",
            status[0],
        )));
    }
    slog::info!(log, "post-pause ZFS sync complete");
    Ok(())
}

/// Reads and clears the kernel's dirty bitmap for `[gpa, gpa + len)`.
type TrackFn<'a> = dyn FnMut(u64, usize, &mut [u8]) -> std::io::Result<()> + 'a;

fn kernel_tracking(
    hdl: &VmmHdl,
) -> impl FnMut(u64, usize, &mut [u8]) -> std::io::Result<()> + '_ {
    move |gpa, len, bm| hdl.track_dirty_pages(gpa, len, bm)
}

/// Refuse the commit while a guest page is still dirty.
///
/// Every writer stopped before the final pass, so a page dirty here is
/// a write the destination never got. The bits this read clears do not
/// matter: a failed attempt rolls back, and the next one starts with a
/// full pass.
fn check_clean(
    track: &mut TrackFn<'_>,
    memctx: &MemCtx,
) -> Result<(), MigrateError> {
    let mut left: u64 = 0;
    for (gpa, len) in memctx.guest_ram() {
        let mut bm = vec![0u8; crate::protocol::bitmap_size(len)];
        track(gpa, len, &mut bm)
            .map_err(|e| MigrateError::Io(format!("track_dirty: {e}")))?;
        left += PageIter::new(&bm, gpa, len).count() as u64;
    }
    if left != 0 {
        return Err(MigrateError::State(format!(
            "{left} guest pages were written after the final pass"
        )));
    }
    Ok(())
}

/// Always every page, whatever the dirty bitmap says.
///
/// A failed attempt cleared the bits of the pages it sent, and a
/// destination of a new attempt holds none of them. A dirty-only first
/// pass would leave those pages behind.
fn push_ram_all<S: Transport>(
    chan: &mut Chan<S>,
    track: &mut TrackFn<'_>,
    memctx: &MemCtx,
    gate: &dyn StopGate,
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<(), MigrateError> {
    for (gpa, len) in memctx.guest_ram() {
        let bm_len = crate::protocol::bitmap_size(len);
        let mut bm = vec![0u8; bm_len];
        // Failing here rather than on the next pass: tracking that does
        // not work costs a full RAM copy before it is noticed.
        track(gpa, len, &mut bm).map_err(|e| {
            MigrateError::Io(format!("dirty tracking unavailable: {e}"))
        })?;

        let num_pages = len / PAGE_SIZE;
        let mut off = 0;
        while off < num_pages {
            let count = BATCH_SIZE.min(num_pages - off);
            let base = gpa + (off * PAGE_SIZE) as u64;
            send_batch(chan, memctx, gate, base, count, status)?;
            off += count;
        }
    }
    end_pass(chan, gate)
}

fn push_ram<S: Transport>(
    chan: &mut Chan<S>,
    track: &mut TrackFn<'_>,
    memctx: &MemCtx,
    gate: &dyn StopGate,
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<u64, MigrateError> {
    let mut total: u64 = 0;
    for (gpa, len) in memctx.guest_ram() {
        let bm_len = crate::protocol::bitmap_size(len);
        let mut bm = vec![0u8; bm_len];
        track(gpa, len, &mut bm)
            .map_err(|e| MigrateError::Io(format!("track_dirty: {e}")))?;
        total += send_dirty(PageIter::new(&bm, gpa, len), gate, |batch| {
            send_batch_sparse(chan, memctx, gate, batch, status)
        })?;
    }
    if let Ok(mut s) = status.lock() {
        s.dirty_pages_remaining = total;
    }
    end_pass(chan, gate)?;
    Ok(total)
}

fn send_dirty(
    pages: impl Iterator<Item = u64>,
    gate: &dyn StopGate,
    mut send: impl FnMut(&[u64]) -> Result<(), MigrateError>,
) -> Result<u64, MigrateError> {
    let mut total = 0;
    let mut batch: Vec<u64> = Vec::with_capacity(BATCH_SIZE);
    for gpa in pages {
        batch.push(gpa);
        total += 1;
        if batch.len() >= BATCH_SIZE {
            not_cancelled(gate)?;
            send(&batch)?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        not_cancelled(gate)?;
        send(&batch)?;
    }
    Ok(total)
}

fn end_pass<S: Transport>(
    chan: &mut Chan<S>,
    gate: &dyn StopGate,
) -> Result<(), MigrateError> {
    chan.send_uncommitted(gate, &Message::MemEnd)?;
    not_cancelled(gate)?;
    chan.expect_mem_done()
}

fn send_batch<S: Transport>(
    chan: &mut Chan<S>,
    memctx: &MemCtx,
    gate: &dyn StopGate,
    base_gpa: u64,
    count: usize,
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<(), MigrateError> {
    let raw_size = page_buffer_len(count)?;
    let gpas = build_contiguous_gpas(base_gpa, count)?;
    let mut buf = vec![0u8; raw_size];
    read_pages(memctx, &gpas, &mut buf)?;
    let (data, flags) = compress(&buf);
    chan.send_uncommitted(
        gate,
        &Message::PageBatch {
            base_gpa,
            page_count: page_count(count)?,
            flags,
            data,
        },
    )?;
    if let Ok(mut s) = status.lock() {
        s.pages_transferred += count as u64;
        s.bytes_transferred += raw_size as u64;
    }
    Ok(())
}

fn send_batch_sparse<S: Transport>(
    chan: &mut Chan<S>,
    memctx: &MemCtx,
    gate: &dyn StopGate,
    gpas: &[u64],
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<(), MigrateError> {
    if gpas.is_empty() {
        return Ok(());
    }
    let base = gpas[0];
    if are_contiguous(gpas) {
        return send_batch(chan, memctx, gate, base, gpas.len(), status);
    }
    let raw_size = page_buffer_len(gpas.len())?;
    let count = gpas.len();
    let mut buf = vec![0u8; raw_size];
    read_pages(memctx, gpas, &mut buf)?;
    let (data, flags) = compress(&buf);
    chan.send_uncommitted(gate, &Message::MemFetch(gpas.to_vec()))?;
    chan.send_uncommitted(
        gate,
        &Message::PageBatch {
            base_gpa: base,
            page_count: page_count(count)?,
            flags,
            data,
        },
    )?;
    if let Ok(mut s) = status.lock() {
        s.pages_transferred += count as u64;
        s.bytes_transferred += raw_size as u64;
    }
    Ok(())
}

fn page_count(count: usize) -> Result<u32, MigrateError> {
    u32::try_from(count).map_err(|_| {
        MigrateError::Codec(format!("invalid page count: {count}"))
    })
}

fn compress(raw: &[u8]) -> (Vec<u8>, u32) {
    match zstd::bulk::compress(raw, ZSTD_LEVEL) {
        Ok(c) if c.len() < raw.len() => (c, PAGE_BATCH_FLAG_ZSTD),
        _ => (raw.to_vec(), 0),
    }
}

#[cfg(test)]
mod tests;
