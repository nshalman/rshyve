// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Synchronous migration source — runs over a Unix socket without tokio.
//!
//! Used inside bhyve zones where `/dev/poll` is unavailable. The GZ
//! migration agent bridges the Unix socket to TCP.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tungstenite::WebSocket;

use vmm_core::cpuid::CpuBaseline;
use vmm_core::hdl::VmmHdl;
use vmm_core::mem::{MemCtx, RegionKind};
use vmm_devices::FlushError;

use crate::codec::{
    CpuFeatures, Message, MigrateError, MigrationPreamble, ProtocolOffer,
    ProtocolSelect, PAGE_BATCH_FLAG_ZSTD,
};
use crate::limits;
use crate::pages::{
    are_contiguous, build_contiguous_gpas, page_buffer_len, read_pages,
};
use crate::protocol::{PageIter, PAGE_SIZE, PROTOCOL_RON_V0};
use crate::state;
use crate::MigrationPhase;
use crate::MigrationStatus;

use crate::limits::{BATCH_SIZE, ZSTD_LEVEL};

/// Run the source side of live migration over a synchronous stream.
///
/// `stream` is typically a `std::os::unix::net::UnixStream`.
/// Device queue state exported after vCPU pause.
pub struct DeviceQueueExport {
    pub virtio_queues: Vec<crate::codec::VirtioQueueState>,
    pub nvme_controllers: Vec<crate::codec::NvmeControllerState>,
}

/// Callback to pause device rings before vCPU pause. This ensures
/// the kernel ring state is consistent with the guest's state —
/// no entries are consumed between vCPU pause and state capture.
pub type DevicePrePauseFn = Box<dyn FnOnce() + Send>;

/// Callback to export device queue state. Called after vCPU pause.
pub type DeviceExportFn = Box<dyn FnOnce() -> DeviceQueueExport + Send>;

/// Callback to flush device writeback caches. Called after `hdl.pause()`
/// has quiesced vCPUs and before the ZFS barrier / device-state export
/// so `zfs send` sees every write the guest was told was complete.
pub type DeviceFlushFn =
    Box<dyn FnOnce() -> Result<(), DeviceFlushError> + Send>;

/// One device's backing store could not be flushed.
#[derive(Debug, thiserror::Error)]
#[error("{device}: {source}")]
pub struct DeviceFlushError {
    pub device: &'static str,
    pub source: FlushError,
}

/// Callback to resume device workers when a paused migration aborts.
pub type DeviceResumeFn = Box<dyn Fn() + Send>;

// These parameters are distinct migration inputs and lifecycle callbacks.
#[allow(clippy::too_many_arguments)]
pub fn run_source_sync<S: Read + Write>(
    stream: S,
    hdl: Arc<VmmHdl>,
    memctx: &MemCtx,
    num_cpus: u32,
    mem_size: u64,
    status: Arc<Mutex<MigrationStatus>>,
    log: &slog::Logger,
    cpu_baseline: CpuBaseline,
    device_pre_pause_fn: DevicePrePauseFn,
    device_flush_fn: DeviceFlushFn,
    device_resume_fn: DeviceResumeFn,
    device_export_fn: DeviceExportFn,
    zfs_barrier: Option<PathBuf>,
) -> Result<(), MigrateError> {
    let mut ws = WebSocket::from_raw_socket(
        stream,
        tungstenite::protocol::Role::Client,
        Some(limits::ws_config()),
    );

    let result = run_inner(
        &mut ws,
        &hdl,
        memctx,
        num_cpus,
        mem_size,
        &status,
        log,
        cpu_baseline,
        device_pre_pause_fn,
        device_flush_fn,
        device_export_fn,
        zfs_barrier,
    );

    let phase = status
        .lock()
        .map(|s| s.phase)
        .unwrap_or(MigrationPhase::Error);
    finish_source_sync(result, phase, log, device_resume_fn, || hdl.resume())
}

fn finish_source_sync<E, F>(
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
    if result.is_err() && phase as u8 >= MigrationPhase::Pause as u8 {
        // Device workers must be available before vCPUs can issue more I/O.
        device_resume_fn();
        if let Err(error) = resume_vm() {
            slog::warn!(log, "failed to resume source VM after migration abort";
                "error" => %error);
        }
    }

    result
}

fn run_inner<S: Read + Write>(
    ws: &mut WebSocket<S>,
    hdl: &Arc<VmmHdl>,
    memctx: &MemCtx,
    num_cpus: u32,
    mem_size: u64,
    status: &Arc<Mutex<MigrationStatus>>,
    log: &slog::Logger,
    cpu_baseline: CpuBaseline,
    device_pre_pause_fn: DevicePrePauseFn,
    device_flush_fn: DeviceFlushFn,
    device_export_fn: DeviceExportFn,
    zfs_barrier: Option<PathBuf>,
) -> Result<(), MigrateError> {
    // ── Sync ──
    set_phase(status, MigrationPhase::Sync);

    let offer = ProtocolOffer {
        protocols: vec![PROTOCOL_RON_V0.to_string()],
    };
    send(ws, &Message::serialized(&offer)?)?;

    let sel_data = expect_serialized(recv(ws)?)?;
    let sel: ProtocolSelect = Message::deserialize(&sel_data)?;
    if sel.protocol != PROTOCOL_RON_V0 {
        return Err(MigrateError::ProtocolMismatch(sel.protocol));
    }

    let (l1_ecx, l1_edx, l7_ebx, l7_ecx, l7_edx, xcr0) =
        vmm_core::cpuid::query_masked_features(cpu_baseline);

    let preamble = MigrationPreamble {
        num_cpus,
        mem_size,
        devices: Vec::new(),
        cpu_features: Some(CpuFeatures {
            leaf1_ecx: l1_ecx,
            leaf1_edx: l1_edx,
            leaf7_ebx: l7_ebx,
            leaf7_ecx: l7_ecx,
            leaf7_edx: l7_edx,
            xcr0,
        }),
    };
    send(ws, &Message::serialized(&preamble)?)?;
    expect_okay(recv(ws)?)?;

    // ── RamPushPrePause: send ALL pages ──
    set_phase(status, MigrationPhase::RamPushPrePause);
    push_ram_all(ws, hdl, memctx, status)?;

    // ── Iterative convergence ──
    const MAX_ITERS: u32 = 5;
    const CONVERGE_THRESHOLD: u64 = 1024;

    for iter in 1..=MAX_ITERS {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let dirty = push_ram(ws, hdl, memctx, status)?;
        slog::info!(log, "convergence"; "iter" => iter, "dirty" => dirty);
        if dirty <= CONVERGE_THRESHOLD {
            break;
        }
    }

    send(ws, &Message::PauseSignal)?;

    // ── Pause ──
    // Pause device rings FIRST so the kernel stops consuming avail
    // entries. This ensures the guest's descriptor state is consistent
    // with what the kernel has consumed — no descriptors are consumed
    // between vCPU pause and state capture.
    set_phase(status, MigrationPhase::Pause);
    device_pre_pause_fn();
    hdl.pause()
        .map_err(|e| MigrateError::Io(format!("pause: {e}")))?;

    // ── Flush device writeback caches ──
    // zvol WCE=1 means writes we told the guest were complete may still
    // sit in cache. `zfs send` ships only committed pool state, so we
    // must fsync every backing fd before the GZ agent runs the ZFS
    // incremental — otherwise the destination would see a disk that's
    // older than the guest memory we're about to ship.
    slog::info!(log, "flushing device backing caches");
    device_flush_fn().map_err(|error| {
        MigrateError::Io(format!("device flush failed: {error}"))
    })?;

    // ── Post-pause ZFS barrier ──
    // Wait for the GZ agent to complete the final ZFS incremental.
    // The agent listens on the barrier socket; our connect signals
    // "vCPUs are paused." We block on read until the agent writes
    // back (ZFS send complete). This ensures the dest disk has all
    // writes up to the pause point.
    if let Some(ref barrier_path) = zfs_barrier {
        slog::info!(log, "waiting for post-pause ZFS sync";
            "barrier" => %barrier_path.display());
        match std::os::unix::net::UnixStream::connect(barrier_path) {
            Ok(mut stream) => {
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(120)))
                    .ok();
                let mut buf = [0u8; 1];
                match stream.read_exact(&mut buf) {
                    Ok(()) => slog::info!(log, "post-pause ZFS sync complete"),
                    Err(e) => {
                        slog::warn!(log, "ZFS barrier read failed (continuing)";
                        "error" => %e)
                    }
                }
            }
            Err(e) => {
                slog::warn!(log, "ZFS barrier connect failed (continuing)";
                    "error" => %e);
            }
        }
    }

    // ── Post-pause dirty ──
    set_phase(status, MigrationPhase::RamPushPostPause);
    let final_dirty = push_ram(ws, hdl, memctx, status)?;
    slog::info!(log, "post-pause"; "dirty" => final_dirty);

    // ── TimeData ──
    set_phase(status, MigrationPhase::TimeData);
    let time_data = state::export_time_data(hdl, log)?;
    send(ws, &Message::serialized(&time_data)?)?;

    // ── DeviceState ──
    set_phase(status, MigrationPhase::DeviceState);
    let mut dev_state = state::export_device_state(hdl, num_cpus, log)?;
    // Export device queue state NOW (after pause) so ring indices
    // are captured while the guest is quiesced. If done before
    // pause, kernel ring threads drain all available buffers,
    // leaving avail_idx == used_idx and the destination with an
    // empty rx ring.
    let export = device_export_fn();
    dev_state.virtio_queues = export.virtio_queues;
    dev_state.nvme_controllers = export.nvme_controllers;
    slog::info!(log, "exporting device queue state";
        "virtio_queues" => dev_state.virtio_queues.len(),
        "nvme_controllers" => dev_state.nvme_controllers.len());
    send(ws, &Message::serialized(&dev_state)?)?;

    // ── RamPull (none needed) ──
    set_phase(status, MigrationPhase::RamPull);
    send(ws, &Message::MemEnd)?;

    // ── Finish ──
    set_phase(status, MigrationPhase::Finish);
    expect_okay(recv(ws)?)?;
    send(ws, &Message::Okay)?;

    let _ = ws.close(None);
    Ok(())
}

fn push_ram_all<S: Read + Write>(
    ws: &mut WebSocket<S>,
    hdl: &Arc<VmmHdl>,
    memctx: &MemCtx,
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<(), MigrateError> {
    for &(gpa, len, kind) in &memctx.regions() {
        if kind != RegionKind::Ram {
            continue;
        }
        let bm_len = crate::protocol::bitmap_size(len);
        let mut bm = vec![0u8; bm_len];
        let _ = hdl.track_dirty_pages(gpa, len, &mut bm);

        let num_pages = len / PAGE_SIZE;
        let mut off = 0;
        while off < num_pages {
            let count = BATCH_SIZE.min(num_pages - off);
            let base = gpa + (off * PAGE_SIZE) as u64;
            send_batch(ws, memctx, base, count, status)?;
            off += count;
        }
    }
    send(ws, &Message::MemEnd)?;
    expect_mem_done(recv(ws)?)?;
    Ok(())
}

fn push_ram<S: Read + Write>(
    ws: &mut WebSocket<S>,
    hdl: &VmmHdl,
    memctx: &MemCtx,
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

        let mut batch: Vec<u64> = Vec::with_capacity(BATCH_SIZE);
        for dirty_gpa in PageIter::new(&bm, gpa, len) {
            batch.push(dirty_gpa);
            total += 1;
            if batch.len() >= BATCH_SIZE {
                send_batch_sparse(ws, memctx, &batch, status)?;
                batch.clear();
            }
        }
        if !batch.is_empty() {
            send_batch_sparse(ws, memctx, &batch, status)?;
        }
    }
    send(ws, &Message::MemEnd)?;
    expect_mem_done(recv(ws)?)?;
    Ok(total)
}

fn send_batch<S: Read + Write>(
    ws: &mut WebSocket<S>,
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
    send(
        ws,
        &Message::PageBatch {
            base_gpa,
            page_count: u32::try_from(count).map_err(|_| {
                MigrateError::Codec(format!("invalid page count: {count}"))
            })?,
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

fn send_batch_sparse<S: Read + Write>(
    ws: &mut WebSocket<S>,
    memctx: &MemCtx,
    gpas: &[u64],
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<(), MigrateError> {
    if gpas.is_empty() {
        return Ok(());
    }
    let raw_size = page_buffer_len(gpas.len())?;
    let base = gpas[0];
    let contig = are_contiguous(gpas);
    if contig {
        return send_batch(ws, memctx, base, gpas.len(), status);
    }
    let count = gpas.len();
    let mut buf = vec![0u8; raw_size];
    read_pages(memctx, gpas, &mut buf)?;
    let (data, flags) = compress(&buf);
    send(ws, &Message::MemFetch(gpas.to_vec()))?;
    send(
        ws,
        &Message::PageBatch {
            base_gpa: base,
            page_count: u32::try_from(count).map_err(|_| {
                MigrateError::Codec(format!("invalid page count: {count}"))
            })?,
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

fn compress(raw: &[u8]) -> (Vec<u8>, u32) {
    match zstd::bulk::compress(raw, ZSTD_LEVEL) {
        Ok(c) if c.len() < raw.len() => (c, PAGE_BATCH_FLAG_ZSTD),
        _ => (raw.to_vec(), 0),
    }
}

fn send<S: Read + Write>(
    ws: &mut WebSocket<S>,
    msg: &Message,
) -> Result<(), MigrateError> {
    let encoded = msg.encode();
    let data = match encoded {
        tungstenite::Message::Binary(d) => d,
        _ => return Err(MigrateError::Codec("non-binary encode".into())),
    };
    ws.send(tungstenite::Message::Binary(data))
        .map_err(|e| MigrateError::WebSocket(e.to_string()))
}

fn recv<S: Read + Write>(
    ws: &mut WebSocket<S>,
) -> Result<Message, MigrateError> {
    let frame = ws
        .read()
        .map_err(|e| MigrateError::WebSocket(e.to_string()))?;
    let data = match frame {
        tungstenite::Message::Binary(d) => d.to_vec(),
        tungstenite::Message::Close(_) => {
            return Err(MigrateError::WebSocket("closed".into()));
        }
        _ => return Err(MigrateError::Codec("non-binary frame".into())),
    };
    Message::decode(tungstenite::Message::Binary(data))
}

fn expect_okay(msg: Message) -> Result<(), MigrateError> {
    match msg {
        Message::Okay => Ok(()),
        Message::Error(e) => Err(MigrateError::RemoteError(e.to_string())),
        other => Err(MigrateError::UnexpectedMessage {
            expected: "Okay".into(),
            got: format!("{other:?}"),
        }),
    }
}

fn expect_serialized(msg: Message) -> Result<Vec<u8>, MigrateError> {
    match msg {
        Message::Serialized(d) => Ok(d),
        Message::Error(e) => Err(MigrateError::RemoteError(e.to_string())),
        other => Err(MigrateError::UnexpectedMessage {
            expected: "Serialized".into(),
            got: format!("{other:?}"),
        }),
    }
}

fn expect_mem_done(msg: Message) -> Result<(), MigrateError> {
    match msg {
        Message::MemDone => Ok(()),
        other => Err(MigrateError::UnexpectedMessage {
            expected: "MemDone".into(),
            got: format!("{other:?}"),
        }),
    }
}

fn set_phase(status: &Arc<Mutex<MigrationStatus>>, phase: MigrationPhase) {
    if let Ok(mut s) = status.lock() {
        s.phase = phase;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn flush_failure_resumes_devices_and_vcpus() {
        let flush_fn: DeviceFlushFn = Box::new(|| {
            Err(DeviceFlushError {
                device: "virtio-blk",
                source: FlushError::NotQuiesced("virtio-blk"),
            })
        });
        let result = flush_fn().map_err(|error| {
            MigrateError::Io(format!("device flush failed: {error}"))
        });
        let devices_resumed = Arc::new(AtomicBool::new(false));
        let vcpus_resumed = Arc::new(AtomicBool::new(false));
        let devices_resumed_by_callback = Arc::clone(&devices_resumed);
        let vcpus_resumed_by_callback = Arc::clone(&vcpus_resumed);
        let devices_resumed_before_vcpus = Arc::clone(&devices_resumed);
        let resume_fn: DeviceResumeFn = Box::new(move || {
            devices_resumed_by_callback.store(true, Ordering::Release);
        });
        let log = slog::Logger::root(slog::Discard, slog::o!());

        let result = finish_source_sync(
            result,
            MigrationPhase::Pause,
            &log,
            resume_fn,
            move || {
                assert!(devices_resumed_before_vcpus.load(Ordering::Acquire));
                vcpus_resumed_by_callback.store(true, Ordering::Release);
                Ok::<(), &'static str>(())
            },
        );

        assert!(matches!(
            result,
            Err(MigrateError::Io(ref error))
                if error
                    == "device flush failed: virtio-blk: workers did not quiesce in time"
        ));
        assert!(devices_resumed.load(Ordering::Acquire));
        assert!(vcpus_resumed.load(Ordering::Acquire));
    }
}
