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
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;

use vmm_core::hdl::VmmHdl;
use vmm_core::mem::{MemCtx, RegionKind};

use crate::codec::{
    CpuFeatures, MemRegionDesc, Message, MigrateError, MigrationPreamble,
    ProtocolOffer, ProtocolSelect,
};
use crate::protocol::{PAGE_SIZE, PROTOCOL_RON_V0};
use crate::state;
use crate::MigrationPhase;
use crate::MigrationStatus;

/// Run the source side of live migration.
///
/// Connects to the destination via a pre-established TCP stream,
/// upgrades to WebSocket, and runs the full migration protocol.
///
/// On success, the VM is paused and should be destroyed by the caller.
/// On failure, the VM is resumed (if it was paused) and the error is
/// returned.
pub async fn run_source(
    stream: TcpStream,
    hdl: Arc<VmmHdl>,
    memctx: &MemCtx,
    num_cpus: u32,
    mem_size: u64,
    status: Arc<Mutex<MigrationStatus>>,
) -> Result<(), MigrateError> {
    // Upgrade TCP to WebSocket (client role)
    let (ws, _) = tokio_tungstenite::client_async(
        "ws://migration/live",
        stream,
    )
    .await
    .map_err(|e| MigrateError::WebSocket(format!(
        "WebSocket upgrade failed: {e}"
    )))?;

    let result = run_source_inner(
        ws, &hdl, memctx, num_cpus, mem_size, &status,
    )
    .await;

    // On error after pause, resume the VM so the guest keeps running.
    if result.is_err() {
        let phase = status.lock().map(|s| s.phase).unwrap_or(MigrationPhase::Error);
        if phase as u8 >= MigrationPhase::Pause as u8 {
            let _ = hdl.resume();
        }
    }

    result
}

/// Inner protocol driver, separated for clean error-recovery in the
/// outer function.
async fn run_source_inner(
    mut ws: WebSocketStream<TcpStream>,
    hdl: &Arc<VmmHdl>,
    memctx: &MemCtx,
    num_cpus: u32,
    mem_size: u64,
    status: &Arc<Mutex<MigrationStatus>>,
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

    // Send preamble with CPU feature info for compatibility check
    let (l1_ecx, l1_edx, l7_ebx, l7_ecx, l7_edx, xcr0) =
        vmm_core::cpuid::query_host_features();
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
    // First pass: send ALL pages unconditionally (like QEMU).
    // The dirty bitmap from track_dirty_pages() may not cover pages
    // that were mapped read-only or never faulted into EPT. Sending
    // everything on the first pass ensures no pages are missed.
    set_phase(status, MigrationPhase::RamPushPrePause);
    push_ram_all(&mut ws, hdl, memctx, status).await?;

    // ── Phase: Pause ──────────────────────────────────────────────
    set_phase(status, MigrationPhase::Pause);
    hdl.pause().map_err(|e| {
        MigrateError::Io(format!("pause failed: {e}"))
    })?;

    // ── Phase: RamPushPostPause ───────────────────────────────────
    set_phase(status, MigrationPhase::RamPushPostPause);
    push_ram(&mut ws, hdl, memctx, status).await?;

    // ── Phase: TimeData ───────────────────────────────────────────
    set_phase(status, MigrationPhase::TimeData);
    let time_data = state::export_time_data(hdl)?;
    let time_msg = Message::serialized(&time_data)?;
    ws.send(time_msg.encode()).await?;

    // ── Phase: DeviceState ────────────────────────────────────────
    set_phase(status, MigrationPhase::DeviceState);
    let device_state = state::export_device_state(hdl, num_cpus)?;
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
/// Sends every page in every RAM region, regardless of dirty status.
/// This ensures the destination has a complete copy of guest memory.
/// The dirty bitmap is still read (and cleared) so that the post-pause
/// pass only transfers pages dirtied since this point.
async fn push_ram_all(
    ws: &mut WebSocketStream<TcpStream>,
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

        // Set ALL bits — we're sending everything
        let all_dirty = vec![0xFFu8; bitmap_len];

        let num_pages = len / PAGE_SIZE;
        if let Ok(mut s) = status.lock() {
            s.dirty_pages_remaining += num_pages as u64;
        }

        let offer = Message::MemOffer {
            region: MemRegionDesc { gpa, len: len as u64 },
            bitmap: all_dirty,
        };
        ws.send(offer.encode()).await?;
    }

    ws.send(Message::MemEnd.encode()).await?;

    // Serve page requests
    loop {
        let msg = recv(ws).await?;
        match msg {
            Message::MemDone => break,
            Message::MemFetch(gpas) => {
                let count = gpas.len() as u32;
                let xfer_gpa = gpas.first().copied().unwrap_or(0);
                ws.send(Message::MemXfer { gpa: xfer_gpa, count }.encode()).await?;

                for page_gpa in &gpas {
                    let mut page_buf = vec![0u8; PAGE_SIZE];
                    if let Err(_) = memctx.read(*page_gpa, &mut page_buf) {
                        // Page not mapped — send zeros
                        page_buf.fill(0);
                    }
                    ws.send(Message::Page(page_buf).encode()).await?;

                    if let Ok(mut s) = status.lock() {
                        s.pages_transferred += 1;
                        s.bytes_transferred += PAGE_SIZE as u64;
                        if s.dirty_pages_remaining > 0 {
                            s.dirty_pages_remaining -= 1;
                        }
                    }
                }
            }
            other => {
                return Err(MigrateError::UnexpectedMessage {
                    expected: "MemFetch or MemDone".to_string(),
                    got: format!("{other:?}"),
                });
            }
        }
    }

    Ok(())
}

/// Push dirty RAM pages to the destination.
///
/// For each memory region:
/// 1. Read the dirty bitmap from the kernel.
/// 2. Send a MemOffer with the region descriptor and bitmap.
/// 3. Send MemEnd to signal end of offers.
/// 4. Receive MemFetch requests and respond with MemXfer + Page data.
/// 5. Receive MemDone when the destination is satisfied.
async fn push_ram(
    ws: &mut WebSocketStream<TcpStream>,
    hdl: &VmmHdl,
    memctx: &MemCtx,
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<(), MigrateError> {
    let regions = memctx.regions();

    // Send MemOffer for each RAM region with its dirty bitmap
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

        // Count dirty pages for status
        let dirty_count: u64 = bitmap.iter().map(|b| b.count_ones() as u64).sum();
        if let Ok(mut s) = status.lock() {
            s.dirty_pages_remaining += dirty_count;
        }

        let offer = Message::MemOffer {
            region: MemRegionDesc { gpa, len: len as u64 },
            bitmap,
        };
        ws.send(offer.encode()).await?;
    }

    // Signal end of offers
    ws.send(Message::MemEnd.encode()).await?;

    // Serve MemFetch requests until we get MemDone
    loop {
        let msg = recv(ws).await?;
        match msg {
            Message::MemDone => break,
            Message::MemFetch(gpas) => {
                // Send MemXfer header
                let count = gpas.len() as u32;
                let xfer_gpa = gpas.first().copied().unwrap_or(0);
                let xfer = Message::MemXfer { gpa: xfer_gpa, count };
                ws.send(xfer.encode()).await?;

                // Send each page
                for page_gpa in &gpas {
                    let mut page_buf = vec![0u8; PAGE_SIZE];
                    memctx.read(*page_gpa, &mut page_buf).map_err(|e| {
                        MigrateError::Io(format!(
                            "read page gpa={page_gpa:#x}: {e}"
                        ))
                    })?;
                    ws.send(Message::Page(page_buf).encode()).await?;

                    // Update status
                    if let Ok(mut s) = status.lock() {
                        s.pages_transferred += 1;
                        s.bytes_transferred += PAGE_SIZE as u64;
                        if s.dirty_pages_remaining > 0 {
                            s.dirty_pages_remaining -= 1;
                        }
                    }
                }
            }
            other => {
                return Err(MigrateError::UnexpectedMessage {
                    expected: "MemFetch or MemDone".to_string(),
                    got: format!("{other:?}"),
                });
            }
        }
    }

    Ok(())
}

/// Receive a single message from the WebSocket.
async fn recv(
    ws: &mut WebSocketStream<TcpStream>,
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
