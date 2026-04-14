// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Migration source protocol runner.
//!
//! Drives the source side of a live migration: negotiates the protocol,
//! iteratively pushes dirty RAM pages, pauses the VM, sends final dirty
//! pages and device state, then hands off to the destination.
//!
//! # Error recovery
//!
//! If migration fails after the VM has been paused, the source resumes
//! the VM so it continues running. The caller sees the error but the
//! guest is not disrupted.

use std::sync::{Arc, Mutex};

use futures::sink::SinkExt;
use futures::stream::StreamExt;
use slog;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::WebSocketStream;

use vmm_core::hdl::VmmHdl;
use vmm_core::mem::{MemCtx, RegionKind};

use crate::codec::{
    CpuFeatures, MemRegionDesc, Message, MigrateError, MigrationPreamble,
    ProtocolOffer, ProtocolSelect, PAGE_BATCH_FLAG_ZSTD,
};
use crate::protocol::{PageIter, PAGE_SIZE, PROTOCOL_RON_V0};
use crate::state;
use crate::MigrationPhase;
use crate::MigrationStatus;

/// Number of pages per batch for PageBatch messages.
const BATCH_SIZE: usize = 64;
/// zstd compression level (1 = fastest, good ratio for RAM).
const ZSTD_LEVEL: i32 = 1;

/// Run the source side of live migration.
///
/// Connects to the destination via a pre-established TCP stream,
/// upgrades to WebSocket, and runs the full migration protocol.
///
/// On success, the VM is paused and should be destroyed by the caller.
/// On failure, the VM is resumed (if it was paused) and the error is
/// returned.
pub async fn run_source<S>(
    stream: S,
    hdl: Arc<VmmHdl>,
    memctx: &MemCtx,
    num_cpus: u32,
    mem_size: u64,
    status: Arc<Mutex<MigrationStatus>>,
    log: &slog::Logger,
    cpu_baseline: &str,
) -> Result<(), MigrateError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Upgrade to WebSocket (client role)
    let (ws, _) = tokio_tungstenite::client_async(
        "ws://migration/live",
        stream,
    )
    .await
    .map_err(|e| MigrateError::WebSocket(format!(
        "WebSocket upgrade failed: {e}"
    )))?;

    let result = run_source_inner(
        ws, &hdl, memctx, num_cpus, mem_size, &status, log, cpu_baseline,
    )
    .await;

    // On error after pause, resume the VM so the guest keeps running.
    if result.is_err() {
        let phase = status.lock().map(|s| s.phase).unwrap_or(MigrationPhase::Error);
        if phase as u8 >= MigrationPhase::Pause as u8 {
            // best-effort: if resume fails, VM is already in bad state
            let _ = hdl.resume();
        }
    }

    result
}

/// Inner protocol driver, separated for clean error-recovery in the
/// outer function.
async fn run_source_inner<S: AsyncRead + AsyncWrite + Unpin>(
    mut ws: WebSocketStream<S>,
    hdl: &Arc<VmmHdl>,
    memctx: &MemCtx,
    num_cpus: u32,
    mem_size: u64,
    status: &Arc<Mutex<MigrationStatus>>,
    log: &slog::Logger,
    cpu_baseline: &str,
) -> Result<(), MigrateError> {
    // ── Phase: Sync ────────────────────────────────────────────────
    set_phase(status, MigrationPhase::Sync);

    // Send protocol offer
    let offer = ProtocolOffer {
        protocols: vec![PROTOCOL_RON_V0.to_string()],
    };
    let offer_msg = Message::serialized(&offer)?;
    ws.send(offer_msg.encode()).await?;

    // Receive protocol selection
    let sel_msg = recv(&mut ws).await?;
    let sel_data = expect_serialized(sel_msg)?;
    let sel: ProtocolSelect = Message::deserialize(&sel_data)?;
    if sel.protocol != PROTOCOL_RON_V0 {
        return Err(MigrateError::ProtocolMismatch(format!(
            "destination selected '{}', we only support '{}'",
            sel.protocol, PROTOCOL_RON_V0,
        )));
    }

    // Send preamble with CPU features masked by the baseline.
    // This reflects what the guest sees, not the raw host features.
    let baseline = vmm_core::cpuid::CpuBaseline::from_str(cpu_baseline)
        .unwrap_or(vmm_core::cpuid::CpuBaseline::Host);
    let (l1_ecx, l1_edx, l7_ebx, l7_ecx, l7_edx, xcr0) =
        vmm_core::cpuid::query_masked_features(baseline);
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
    let preamble_msg = Message::serialized(&preamble)?;
    ws.send(preamble_msg.encode()).await?;

    // Receive Okay (destination accepted preamble)
    let ack = recv(&mut ws).await?;
    expect_okay(ack)?;

    // ── Phase: RamPushPrePause ─────────────────────────────────────
    // First pass: send ALL pages unconditionally. The dirty bitmap
    // from track_dirty_pages() may not cover pages mapped read-only
    // or never faulted into EPT. Sending everything on the first
    // pass ensures no pages are missed.
    set_phase(status, MigrationPhase::RamPushPrePause);
    push_ram_all(&mut ws, hdl, memctx, status).await?;

    // ── Iterative convergence ────────────────────────────────────
    // Send dirty pages in a loop until the dirty rate converges
    // (< 1024 pages = 4 MiB) or we hit the max iteration count.
    // This minimizes the final post-pause delta.
    const MAX_PRE_PAUSE_ITERATIONS: u32 = 5;
    const CONVERGENCE_THRESHOLD: u64 = 1024; // pages

    for iteration in 1..=MAX_PRE_PAUSE_ITERATIONS {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let dirty_count = push_ram(&mut ws, hdl, memctx, status).await?;
        slog::info!(log, "pre-pause convergence iteration";
            "iteration" => iteration,
            "dirty_pages" => dirty_count,
        );

        if dirty_count <= CONVERGENCE_THRESHOLD {
            slog::info!(log, "converged, proceeding to pause";
                "iterations" => iteration,
                "final_dirty" => dirty_count,
            );
            break;
        }
    }

    // Signal destination: source is about to pause, next pass is final
    ws.send(Message::PauseSignal.encode()).await?;

    // ── Phase: Pause ──────────────────────────────────────────────
    set_phase(status, MigrationPhase::Pause);
    hdl.pause().map_err(|e| {
        MigrateError::Io(format!("pause failed: {e}"))
    })?;

    // ── Phase: RamPushPostPause ───────────────────────────────────
    set_phase(status, MigrationPhase::RamPushPostPause);
    let final_dirty = push_ram(&mut ws, hdl, memctx, status).await?;
    slog::info!(log, "post-pause dirty pages sent"; "pages" => final_dirty);

    // ── Phase: TimeData ───────────────────────────────────────────
    set_phase(status, MigrationPhase::TimeData);
    let time_data = state::export_time_data(hdl, log)?;
    let time_msg = Message::serialized(&time_data)?;
    ws.send(time_msg.encode()).await?;

    // ── Phase: DeviceState ────────────────────────────────────────
    set_phase(status, MigrationPhase::DeviceState);
    let device_state = state::export_device_state(hdl, num_cpus, log)?;
    let dev_msg = Message::serialized(&device_state)?;
    ws.send(dev_msg.encode()).await?;

    // ── Phase: RamPull ────────────────────────────────────────────
    // No pull needed -- pre/post push covered everything.
    set_phase(status, MigrationPhase::RamPull);
    ws.send(Message::MemEnd.encode()).await?;

    // ── Phase: Finish ─────────────────────────────────────────────
    set_phase(status, MigrationPhase::Finish);

    // Wait for destination to signal it is ready
    let finish_msg = recv(&mut ws).await?;
    expect_okay(finish_msg)?;

    // Acknowledge -- migration is complete
    ws.send(Message::Okay.encode()).await?;

    // Close WebSocket cleanly
    ws.close(None).await.ok();

    Ok(())
}

/// Push ALL RAM pages to the destination (first pass).
///
/// Sends every page in every RAM region using batched, zstd-compressed
/// PageBatch messages. The dirty bitmap is read and cleared so the
/// post-pause pass only transfers pages dirtied after this point.
async fn push_ram_all<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
    hdl: &Arc<VmmHdl>,
    memctx: &MemCtx,
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<(), MigrateError> {
    let regions = memctx.regions();

    for &(gpa, len, kind) in &regions {
        if kind != RegionKind::Ram {
            continue;
        }

        // Read and clear the dirty bitmap (so post-pause only sees new dirt)
        let bitmap_len = crate::protocol::bitmap_size(len);
        let mut bitmap = vec![0u8; bitmap_len];
        let _ = hdl.track_dirty_pages(gpa, len, &mut bitmap);

        // Send ALL pages as batched PageBatch messages
        let num_pages = len / PAGE_SIZE;
        let mut page_offset = 0usize;

        while page_offset < num_pages {
            let batch_count = BATCH_SIZE.min(num_pages - page_offset);
            let batch_gpa = gpa + (page_offset * PAGE_SIZE) as u64;

            send_page_batch(ws, memctx, batch_gpa, batch_count, status).await?;
            page_offset += batch_count;
        }
    }

    // Signal end of this RAM pass
    ws.send(Message::MemEnd.encode()).await?;

    // Wait for destination acknowledgement
    let ack = recv(ws).await?;
    match ack {
        Message::MemDone => {}
        other => {
            return Err(MigrateError::UnexpectedMessage {
                expected: "MemDone".to_string(),
                got: format!("{other:?}"),
            });
        }
    }

    Ok(())
}

/// Push dirty RAM pages to the destination.
///
/// Reads the dirty bitmap from the kernel, iterates dirty pages,
/// and sends them as batched zstd-compressed PageBatch messages.
/// Returns the number of dirty pages found.
async fn push_ram<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
    hdl: &VmmHdl,
    memctx: &MemCtx,
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<u64, MigrateError> {
    let regions = memctx.regions();
    let mut total_dirty: u64 = 0;

    for &(gpa, len, kind) in &regions {
        if kind != RegionKind::Ram {
            continue;
        }

        let bitmap_len = crate::protocol::bitmap_size(len);
        let mut bitmap = vec![0u8; bitmap_len];

        hdl.track_dirty_pages(gpa, len, &mut bitmap).map_err(|e| {
            MigrateError::Io(format!(
                "track_dirty_pages gpa={gpa:#x} len={len}: {e}"
            ))
        })?;

        // Collect dirty page GPAs from bitmap and send in batches
        let mut batch_gpas: Vec<u64> = Vec::with_capacity(BATCH_SIZE);

        for dirty_gpa in PageIter::new(&bitmap, gpa, len) {
            batch_gpas.push(dirty_gpa);
            total_dirty += 1;

            if batch_gpas.len() >= BATCH_SIZE {
                send_page_batch_sparse(ws, memctx, &batch_gpas, status).await?;
                batch_gpas.clear();
            }
        }

        // Flush remaining partial batch
        if !batch_gpas.is_empty() {
            send_page_batch_sparse(ws, memctx, &batch_gpas, status).await?;
        }
    }

    // Signal end of this RAM pass
    ws.send(Message::MemEnd.encode()).await?;

    // Wait for destination acknowledgement
    let ack = recv(ws).await?;
    match ack {
        Message::MemDone => {}
        other => {
            return Err(MigrateError::UnexpectedMessage {
                expected: "MemDone".to_string(),
                got: format!("{other:?}"),
            });
        }
    }

    Ok(total_dirty)
}

/// Send a batch of contiguous pages as a single PageBatch message.
async fn send_page_batch<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
    memctx: &MemCtx,
    base_gpa: u64,
    count: usize,
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<(), MigrateError> {
    let raw_size = count * PAGE_SIZE;
    let mut buf = vec![0u8; raw_size];

    for i in 0..count {
        let page_gpa = base_gpa + (i * PAGE_SIZE) as u64;
        let offset = i * PAGE_SIZE;
        if memctx.read(page_gpa, &mut buf[offset..offset + PAGE_SIZE]).is_err() {
            // Unmapped page — zeros already in buffer
        }
    }

    let (data, flags) = compress_batch(&buf);

    ws.send(Message::PageBatch {
        base_gpa,
        page_count: count as u32,
        flags,
        data,
    }.encode()).await?;

    if let Ok(mut s) = status.lock() {
        s.pages_transferred += count as u64;
        s.bytes_transferred += raw_size as u64;
    }

    Ok(())
}

/// Send a batch of non-contiguous dirty pages as a PageBatch message.
///
/// Pages are packed contiguously in the data buffer. The base_gpa is
/// the first page's GPA; for sparse batches the destination uses the
/// accompanying GPA list (sent as a MemXfer header before the batch).
async fn send_page_batch_sparse<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
    memctx: &MemCtx,
    gpas: &[u64],
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<(), MigrateError> {
    if gpas.is_empty() {
        return Ok(());
    }

    // Check if pages are contiguous — use simple batch if so
    let base = gpas[0];
    let is_contiguous = gpas.iter().enumerate().all(|(i, &g)| {
        g == base + (i * PAGE_SIZE) as u64
    });

    if is_contiguous {
        return send_page_batch(ws, memctx, base, gpas.len(), status).await;
    }

    // Sparse: send GPA list as MemXfer header, then PageBatch with packed data
    let count = gpas.len();
    let raw_size = count * PAGE_SIZE;
    let mut buf = vec![0u8; raw_size];

    for (i, &gpa) in gpas.iter().enumerate() {
        let offset = i * PAGE_SIZE;
        if memctx.read(gpa, &mut buf[offset..offset + PAGE_SIZE]).is_err() {
            // zeros for unmapped
        }
    }

    let (data, flags) = compress_batch(&buf);

    // Send GPA list so destination knows where each page goes
    ws.send(Message::MemFetch(gpas.to_vec()).encode()).await?;
    ws.send(Message::PageBatch {
        base_gpa: base,
        page_count: count as u32,
        flags,
        data,
    }.encode()).await?;

    if let Ok(mut s) = status.lock() {
        s.pages_transferred += count as u64;
        s.bytes_transferred += raw_size as u64;
    }

    Ok(())
}

/// Compress a batch of page data with zstd. If compression doesn't
/// help (compressed >= raw), return uncompressed with flags=0.
fn compress_batch(raw: &[u8]) -> (Vec<u8>, u32) {
    match zstd::bulk::compress(raw, ZSTD_LEVEL) {
        Ok(compressed) if compressed.len() < raw.len() => {
            (compressed, PAGE_BATCH_FLAG_ZSTD)
        }
        _ => (raw.to_vec(), 0),
    }
}

/// Receive a single message from the WebSocket.
async fn recv<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
) -> Result<Message, MigrateError> {
    let frame = ws.next().await.ok_or_else(|| {
        MigrateError::WebSocket("connection closed unexpectedly".to_string())
    })??;
    Message::decode(frame)
}

/// Expect an Okay message; return error if anything else.
fn expect_okay(msg: Message) -> Result<(), MigrateError> {
    match msg {
        Message::Okay => Ok(()),
        Message::Error(e) => Err(MigrateError::RemoteError(e.to_string())),
        other => Err(MigrateError::UnexpectedMessage {
            expected: "Okay".to_string(),
            got: format!("{other:?}"),
        }),
    }
}

/// Expect a Serialized message; return the payload bytes.
fn expect_serialized(msg: Message) -> Result<Vec<u8>, MigrateError> {
    match msg {
        Message::Serialized(data) => Ok(data),
        Message::Error(e) => Err(MigrateError::RemoteError(e.to_string())),
        other => Err(MigrateError::UnexpectedMessage {
            expected: "Serialized".to_string(),
            got: format!("{other:?}"),
        }),
    }
}

/// Update the migration phase in shared status.
fn set_phase(status: &Arc<Mutex<MigrationStatus>>, phase: MigrationPhase) {
    if let Ok(mut s) = status.lock() {
        s.phase = phase;
    }
}
