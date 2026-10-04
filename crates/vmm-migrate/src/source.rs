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
//! 4. Ship the last dirty pass, the time data and the device state.
//! 5. Hand over.
//!
//! Anything that fails from step 1 on rolls the source back: devices
//! resume, the rings a migration paused go back to work, and the vCPUs
//! run again.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vmm_core::cpuid::CpuBaseline;
use vmm_core::hdl::VmmHdl;
use vmm_core::mem::{MemCtx, RegionKind};
use vmm_devices::lifecycle::{
    DeviceStateError, DeviceStatePayload, HypervMigrateState,
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
use crate::protocol::{payload_refusal, PageIter, PAGE_SIZE, SOURCE_OFFER};
use crate::state;
use crate::wire::{
    cancel_or, not_cancelled, set_phase, Chan, Deadlines, Transport,
};
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

/// Flush device writeback caches. Called after the vCPU pause has
/// quiesced the vCPUs and before the ZFS barrier, so `zfs send` sees
/// every write the guest was told was complete.
pub type DeviceFlushFn =
    Box<dyn FnOnce() -> Result<(), DeviceFlushError> + Send>;

/// One device's backing store could not be flushed.
#[derive(Debug, thiserror::Error)]
#[error("{device}: {source}")]
pub struct DeviceFlushError {
    pub device: &'static str,
    pub source: FlushError,
}

/// Put the devices back to work after a migration that failed once the
/// guest was already paused.
pub type DeviceResumeFn = Box<dyn Fn() + Send>;

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
    pub deadlines: Deadlines,
    /// Where the GZ agent reports the final ZFS incremental.
    pub zfs_barrier: Option<PathBuf>,
}

/// The device lifecycle hooks the phases drive.
pub struct SourceHooks {
    pub pre_pause: DevicePrePauseFn,
    pub flush: DeviceFlushFn,
    pub export: DeviceExportFn,
    pub resume: DeviceResumeFn,
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
        hooks.export,
        hooks.hyperv,
        &hooks.pause_vm,
        hooks.gate.as_ref(),
        &status,
        log,
    );
    if let Err(error) = &result {
        if crate::is_cancel(error) {
            chan.report_failure(error, log);
        }
    }

    let phase = status
        .lock()
        .map(|s| s.phase)
        .unwrap_or(MigrationPhase::Error);
    finish_source(result, phase, log, resume, hooks.resume_vm)
}

/// Roll the source back when a migration failed after the pause.
fn finish_source<E, F>(
    result: Result<(), MigrateError>,
    phase: MigrationPhase,
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
    // guest, so the source stays paused and an operator decides.
    let rollback = (MigrationPhase::Pause as u8
        ..MigrationPhase::Committed as u8)
        .contains(&(phase as u8));
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

#[allow(clippy::too_many_arguments)]
fn run_inner<S: Transport>(
    chan: &mut Chan<S>,
    hdl: &Arc<VmmHdl>,
    memctx: &MemCtx,
    config: &SourceConfig,
    device_pre_pause_fn: DevicePrePauseFn,
    device_flush_fn: DeviceFlushFn,
    device_export_fn: DeviceExportFn,
    hyperv_export_fn: HypervExportFn,
    pause_vm: &VmPauseFn,
    gate: &dyn StopGate,
    status: &Arc<Mutex<MigrationStatus>>,
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    // ── Sync ──
    set_phase(status, MigrationPhase::Sync);

    chan.send(&Message::serialized(&ProtocolOffer {
        protocols: SOURCE_OFFER.iter().map(|v| v.to_string()).collect(),
    })?)?;

    let sel: ProtocolSelect = chan.expect_serialized()?;
    if !SOURCE_OFFER.contains(&sel.protocol.as_str()) {
        return Err(MigrateError::ProtocolMismatch(crate::wire::peer_text(
            &sel.protocol,
        )));
    }

    let mut devices = config.devices.clone();
    devices.sort();
    chan.send(&Message::serialized(&MigrationPreamble {
        num_cpus: config.num_cpus,
        mem_size: config.mem_size,
        devices,
        cpu_features: Some(cpu::local_features(config.cpu_baseline)),
    })?)?;
    chan.expect_okay()?;

    // ── RamPushPrePause: send ALL pages ──
    set_phase(status, MigrationPhase::RamPushPrePause);
    push_ram_all(chan, hdl, memctx, gate, status)?;

    // ── Iterative convergence ──
    const MAX_ITERS: u32 = 5;
    const CONVERGE_THRESHOLD: u64 = 1024;

    for iter in 1..=MAX_ITERS {
        std::thread::sleep(Duration::from_millis(500));
        let dirty = push_ram(chan, hdl, memctx, gate, status)?;
        slog::info!(log, "convergence"; "iter" => iter, "dirty" => dirty);
        if dirty <= CONVERGE_THRESHOLD {
            break;
        }
    }

    not_cancelled(gate)?;
    chan.send(&Message::PauseSignal)?;

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
    slog::info!(log, "flushing device backing caches");
    device_flush_fn().map_err(|error| {
        MigrateError::Io(format!("device flush failed: {error}"))
    })?;

    if let Some(barrier) = config.zfs_barrier.as_deref() {
        zfs_barrier_wait(barrier, config.deadlines.zfs_barrier, log)?;
    }

    // The overlay pages go before the final pass: the destination
    // reinstalls them from the MSR values, so shipping ours would give
    // the guest our bytes where its own belong.
    let hyperv = hyperv_export_fn();

    // ── Post-pause dirty ──
    // Every writer stopped in the pre-pause step, so this pass is the
    // last word on guest memory.
    set_phase(status, MigrationPhase::RamPushPostPause);
    let final_dirty = push_ram(chan, hdl, memctx, gate, status)?;
    slog::info!(log, "post-pause"; "dirty" => final_dirty);

    // ── TimeData ──
    set_phase(status, MigrationPhase::TimeData);
    let time_data = state::export_time_data(hdl, log)?;
    chan.send(&Message::serialized(&time_data)?)?;

    // ── DeviceState ──
    set_phase(status, MigrationPhase::DeviceState);
    not_cancelled(gate)?;
    let mut dev_state = state::export_device_state(hdl, config.num_cpus, log)?;
    dev_state.emulated = device_export_fn()
        .map_err(|e| MigrateError::State(format!("device export: {e}")))?;
    if let Some(why) = payload_refusal(&sel.protocol, &dev_state.emulated) {
        return Err(MigrateError::State(why));
    }
    dev_state.hyperv = hyperv;
    slog::info!(log, "exporting device state";
        "devices" => dev_state.emulated.len());
    chan.send(&Message::serialized(&dev_state)?)?;

    // ── RamPull (none needed) ──
    set_phase(status, MigrationPhase::RamPull);
    not_cancelled(gate)?;
    chan.send(&Message::MemEnd)?;

    hand_over(chan, gate, status)
}

/// Three steps, and the order is the whole point. The destination says
/// it holds everything; this end commits, which is the promise never to
/// run the guest again; only then does the destination start it. A
/// destination that resumed before the commit would run a second copy
/// of the guest against the same disk whenever this end rolled back.
fn hand_over<S: Transport>(
    chan: &mut Chan<S>,
    gate: &dyn StopGate,
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<(), MigrateError> {
    set_phase(status, MigrationPhase::Finish);
    chan.expect_okay()?;
    gate.commit()?;

    set_phase(status, MigrationPhase::Committed);
    chan.send(&Message::Okay).map_err(|e| {
        MigrateError::Committed(format!(
            "the commit may not have reached the destination: {e}"
        ))
    })?;
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
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    slog::info!(log, "waiting for post-pause ZFS sync";
        "barrier" => %barrier_path.display());
    let mut stream = std::os::unix::net::UnixStream::connect(barrier_path)
        .map_err(|e| {
            MigrateError::Io(format!("ZFS barrier connect failed: {e}"))
        })?;
    stream.set_read_timeout(Some(budget)).map_err(|e| {
        MigrateError::Io(format!("ZFS barrier read timeout: {e}"))
    })?;
    let mut status = [0u8; 1];
    stream.read_exact(&mut status).map_err(|e| {
        MigrateError::Io(format!("ZFS barrier reported nothing: {e}"))
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

fn push_ram_all<S: Transport>(
    chan: &mut Chan<S>,
    hdl: &Arc<VmmHdl>,
    memctx: &MemCtx,
    gate: &dyn StopGate,
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<(), MigrateError> {
    for &(gpa, len, kind) in &memctx.regions() {
        if kind != RegionKind::Ram {
            continue;
        }
        let bm_len = crate::protocol::bitmap_size(len);
        let mut bm = vec![0u8; bm_len];
        // Failing here rather than on the next pass: tracking that does
        // not work costs a full RAM copy before it is noticed.
        hdl.track_dirty_pages(gpa, len, &mut bm).map_err(|e| {
            MigrateError::Io(format!("dirty tracking unavailable: {e}"))
        })?;

        let num_pages = len / PAGE_SIZE;
        let mut off = 0;
        while off < num_pages {
            let count = BATCH_SIZE.min(num_pages - off);
            let base = gpa + (off * PAGE_SIZE) as u64;
            not_cancelled(gate)?;
            send_batch(chan, memctx, base, count, status)?;
            off += count;
        }
    }
    end_pass(chan, gate)
}

fn push_ram<S: Transport>(
    chan: &mut Chan<S>,
    hdl: &VmmHdl,
    memctx: &MemCtx,
    gate: &dyn StopGate,
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<u64, MigrateError> {
    let mut total: u64 = 0;
    for &(gpa, len, kind) in &memctx.regions() {
        if kind != RegionKind::Ram {
            continue;
        }
        let bm_len = crate::protocol::bitmap_size(len);
        let mut bm = vec![0u8; bm_len];
        hdl.track_dirty_pages(gpa, len, &mut bm)
            .map_err(|e| MigrateError::Io(format!("track_dirty: {e}")))?;
        total += send_dirty(PageIter::new(&bm, gpa, len), gate, |batch| {
            send_batch_sparse(chan, memctx, batch, status)
        })?;
    }
    if let Ok(mut s) = status.lock() {
        s.dirty_pages_remaining = total;
    }
    end_pass(chan, gate)?;
    Ok(total)
}

/// Send `pages` in batches and count them.
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
    not_cancelled(gate)?;
    chan.send(&Message::MemEnd)?;
    not_cancelled(gate)?;
    chan.expect_mem_done()
}

fn send_batch<S: Transport>(
    chan: &mut Chan<S>,
    memctx: &MemCtx,
    base_gpa: u64,
    count: usize,
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<(), MigrateError> {
    let raw_size = page_buffer_len(count)?;
    let gpas = build_contiguous_gpas(base_gpa, count)?;
    let mut buf = vec![0u8; raw_size];
    read_pages(memctx, &gpas, &mut buf)?;
    let (data, flags) = compress(&buf);
    chan.send(&Message::PageBatch {
        base_gpa,
        page_count: page_count(count)?,
        flags,
        data,
    })?;
    if let Ok(mut s) = status.lock() {
        s.pages_transferred += count as u64;
        s.bytes_transferred += raw_size as u64;
    }
    Ok(())
}

fn send_batch_sparse<S: Transport>(
    chan: &mut Chan<S>,
    memctx: &MemCtx,
    gpas: &[u64],
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<(), MigrateError> {
    if gpas.is_empty() {
        return Ok(());
    }
    let base = gpas[0];
    if are_contiguous(gpas) {
        return send_batch(chan, memctx, base, gpas.len(), status);
    }
    let raw_size = page_buffer_len(gpas.len())?;
    let count = gpas.len();
    let mut buf = vec![0u8; raw_size];
    read_pages(memctx, gpas, &mut buf)?;
    let (data, flags) = compress(&buf);
    chan.send(&Message::MemFetch(gpas.to_vec()))?;
    chan.send(&Message::PageBatch {
        base_gpa: base,
        page_count: page_count(count)?,
        flags,
        data,
    })?;
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
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::atomic::{AtomicBool, Ordering};

    fn discard() -> slog::Logger {
        slog::Logger::root(slog::Discard, slog::o!())
    }

    /// A barrier socket that answers with `reply`, or with nothing when
    /// `reply` is `None`.
    fn barrier(reply: Option<u8>) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("barrier.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                if let Some(byte) = reply {
                    let _ = stream.write_all(&[byte]);
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        (dir, path)
    }

    #[test]
    fn a_reported_zfs_failure_fails_the_migration() {
        // The guest would resume on the destination having already
        // acknowledged writes the destination disk never received.
        let (_dir, path) = barrier(Some(1));
        let error = zfs_barrier_wait(&path, Duration::from_secs(5), &discard())
            .expect_err("a non-zero status must fail");
        assert!(error.to_string().contains("reported failure"), "{error}");
    }

    #[test]
    fn a_silent_zfs_barrier_fails_the_migration() {
        let (_dir, path) = barrier(None);
        let error =
            zfs_barrier_wait(&path, Duration::from_millis(200), &discard())
                .expect_err("a timeout must fail");
        assert!(error.to_string().contains("reported nothing"), "{error}");
    }

    #[test]
    fn a_missing_zfs_barrier_fails_the_migration() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error = zfs_barrier_wait(
            &dir.path().join("absent.sock"),
            Duration::from_secs(1),
            &discard(),
        )
        .expect_err("a missing socket must fail");
        assert!(error.to_string().contains("connect failed"), "{error}");
    }

    #[test]
    fn an_agreeing_zfs_barrier_lets_the_migration_go_on() {
        let (_dir, path) = barrier(Some(ZFS_BARRIER_OK));
        zfs_barrier_wait(&path, Duration::from_secs(5), &discard())
            .expect("status 0 is the go-ahead");
    }

    #[test]
    fn flush_failure_resumes_devices_and_vcpus() {
        let result: Result<(), MigrateError> =
            Err(MigrateError::Io("device flush failed".into()));
        let devices_resumed = Arc::new(AtomicBool::new(false));
        let vcpus_resumed = Arc::new(AtomicBool::new(false));
        let devices_resumed_by_callback = Arc::clone(&devices_resumed);
        let vcpus_resumed_by_callback = Arc::clone(&vcpus_resumed);
        let devices_resumed_before_vcpus = Arc::clone(&devices_resumed);
        let resume_fn: DeviceResumeFn = Box::new(move || {
            devices_resumed_by_callback.store(true, Ordering::Release);
        });

        let result = finish_source(
            result,
            MigrationPhase::Pause,
            &discard(),
            resume_fn,
            move || {
                assert!(devices_resumed_before_vcpus.load(Ordering::Acquire));
                vcpus_resumed_by_callback.store(true, Ordering::Release);
                Ok::<(), &'static str>(())
            },
        );

        assert!(result.is_err());
        assert!(devices_resumed.load(Ordering::Acquire));
        assert!(vcpus_resumed.load(Ordering::Acquire));
    }

    #[test]
    fn a_committed_source_is_never_resumed() {
        // The destination may be running the guest. Two copies against
        // one disk is worse than a guest that needs an operator.
        let resumed = Arc::new(AtomicBool::new(false));
        let by_callback = Arc::clone(&resumed);
        let resume_fn: DeviceResumeFn = Box::new(move || {
            by_callback.store(true, Ordering::Release);
        });
        let result = finish_source(
            Err(MigrateError::Committed("no confirmation".into())),
            MigrationPhase::Committed,
            &discard(),
            resume_fn,
            || -> Result<(), &'static str> {
                panic!("a committed source must not resume its vCPUs")
            },
        );
        assert!(result.is_err());
        assert!(!resumed.load(Ordering::Acquire));
    }

    #[test]
    fn a_failure_in_finish_still_rolls_back() {
        // Nothing was promised yet: the destination waits for the
        // commit before it starts the guest.
        let resumed = Arc::new(AtomicBool::new(false));
        let by_callback = Arc::clone(&resumed);
        let resume_fn: DeviceResumeFn = Box::new(move || {
            by_callback.store(true, Ordering::Release);
        });
        let result = finish_source(
            Err(MigrateError::WebSocket("closed".into())),
            MigrationPhase::Finish,
            &discard(),
            resume_fn,
            || Ok::<(), &'static str>(()),
        );
        assert!(result.is_err());
        assert!(resumed.load(Ordering::Acquire));
    }

    /// A gate a test can cancel, counting the commits it let through.
    #[derive(Default)]
    struct Gate {
        cancel: bool,
        commits: std::sync::atomic::AtomicUsize,
    }

    impl StopGate for Gate {
        fn cancelled(&self) -> bool {
            self.cancel
        }

        fn commit(&self) -> Result<(), MigrateError> {
            if self.cancel {
                return Err(crate::cancelled());
            }
            self.commits.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    /// A connected pair of channels, source end first.
    fn pair() -> (Chan<UnixStream>, Chan<UnixStream>) {
        let (a, b) = UnixStream::pair().expect("socket pair");
        let deadlines = Deadlines {
            post_pause: Duration::from_secs(5),
            ..Deadlines::default()
        };
        (
            Chan::new(a, tungstenite::protocol::Role::Client, deadlines)
                .expect("source channel"),
            Chan::new(b, tungstenite::protocol::Role::Server, deadlines)
                .expect("destination channel"),
        )
    }

    fn status() -> Arc<Mutex<MigrationStatus>> {
        Arc::new(Mutex::new(MigrationStatus::default()))
    }

    fn phase(status: &Arc<Mutex<MigrationStatus>>) -> MigrationPhase {
        status.lock().expect("status").phase
    }

    #[test]
    fn a_cancelled_source_never_commits() {
        let (mut source, mut dest) = pair();
        dest.send(&Message::Okay)
            .expect("the destination holds it all");
        let gate = Gate {
            cancel: true,
            ..Gate::default()
        };
        let status = status();

        let error =
            hand_over(&mut source, &gate, &status).expect_err("cancelled");

        assert!(crate::is_cancel(&error), "{error}");
        assert_eq!(phase(&status), MigrationPhase::Finish, "not rolled back");
        drop(source);
        assert!(dest.recv().is_err(), "the destination heard a commit");
    }

    #[test]
    fn a_destination_abort_rolls_the_source_back() {
        let (mut source, mut dest) = pair();
        dest.send(&Message::Error(crate::cancelled()))
            .expect("abort");
        let gate = Gate::default();
        let status = status();

        let error = hand_over(&mut source, &gate, &status)
            .expect_err("the destination aborted");

        assert_eq!(gate.commits.load(Ordering::Acquire), 0, "committed");
        assert_eq!(phase(&status), MigrationPhase::Finish);
        let rolled_back = Arc::new(AtomicBool::new(false));
        let by_callback = Arc::clone(&rolled_back);
        let _ = finish_source(
            Err(error),
            phase(&status),
            &discard(),
            Box::new(move || by_callback.store(true, Ordering::Release)),
            || Ok::<(), &'static str>(()),
        );
        assert!(rolled_back.load(Ordering::Acquire));
    }

    #[test]
    fn a_commit_the_peer_may_have_missed_is_never_rolled_back() {
        // The destination may have the commit and be running the guest.
        let (mut source, mut dest) = pair();
        dest.send(&Message::Okay).expect("holds it all");
        drop(dest);
        let status = status();

        let error = hand_over(&mut source, &Gate::default(), &status)
            .expect_err("the peer left");

        assert!(matches!(error, MigrateError::Committed(_)), "{error}");
        assert_eq!(phase(&status), MigrationPhase::Committed);
    }

    #[test]
    fn a_commit_goes_through_the_gate() {
        let (mut source, mut dest) = pair();
        let peer = std::thread::spawn(move || {
            dest.send(&Message::Okay).expect("holds it all");
            assert!(matches!(dest.recv(), Ok(Message::Okay)));
            dest.send(&Message::Okay).expect("started");
            dest
        });
        let gate = Gate::default();
        let status = status();

        hand_over(&mut source, &gate, &status).expect("handed over");

        assert_eq!(gate.commits.load(Ordering::Acquire), 1);
        assert_eq!(phase(&status), MigrationPhase::Committed);
        peer.join().expect("the destination finished");
    }

    #[test]
    fn a_cancel_stops_the_last_partial_batch() {
        let gate = Gate {
            cancel: true,
            ..Gate::default()
        };
        let mut sent = 0;
        let error = send_dirty((0..10).map(|p| p * 4096), &gate, |_| {
            sent += 1;
            Ok(())
        })
        .expect_err("cancelled");
        assert!(crate::is_cancel(&error), "{error}");
        assert_eq!(sent, 0);
    }

    #[test]
    fn every_dirty_page_is_sent_once() {
        let mut sizes = Vec::new();
        let total = send_dirty(
            (0..(BATCH_SIZE as u64 + 6)).map(|p| p * 4096),
            &Gate::default(),
            |batch| {
                sizes.push(batch.len());
                Ok(())
            },
        )
        .expect("sent");
        assert_eq!(total, BATCH_SIZE as u64 + 6);
        assert_eq!(sizes, [BATCH_SIZE, 6]);
    }

    #[test]
    fn a_cancelled_pass_sends_no_mem_end() {
        let (mut source, mut dest) = pair();
        let gate = Gate {
            cancel: true,
            ..Gate::default()
        };
        let error = end_pass(&mut source, &gate).expect_err("cancelled");
        assert!(crate::is_cancel(&error), "{error}");
        drop(source);
        assert!(dest.recv().is_err(), "the destination heard a MemEnd");
    }

    #[test]
    fn a_pause_a_stop_refused_is_a_cancel() {
        let gate = Gate {
            cancel: true,
            ..Gate::default()
        };
        let error = cancel_or(&gate, MigrateError::Io("pause: refused".into()));
        assert!(crate::is_cancel(&error), "{error}");
        let error =
            cancel_or(&Gate::default(), MigrateError::Io("pause: EIO".into()));
        assert!(!crate::is_cancel(&error), "{error}");
    }

    #[test]
    fn a_pre_pause_failure_does_not_resume_a_running_guest() {
        // Nothing was paused yet, so a resume here would be a state
        // change on a VM that never stopped.
        let resumed = Arc::new(AtomicBool::new(false));
        let by_callback = Arc::clone(&resumed);
        let resume_fn: DeviceResumeFn = Box::new(move || {
            by_callback.store(true, Ordering::Release);
        });
        let result = finish_source(
            Err(MigrateError::Io("connect failed".into())),
            MigrationPhase::Sync,
            &discard(),
            resume_fn,
            || Ok::<(), &'static str>(()),
        );
        assert!(result.is_err());
        assert!(!resumed.load(Ordering::Acquire));
    }
}
