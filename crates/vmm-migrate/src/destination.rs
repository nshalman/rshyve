// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Migration destination protocol runner.
//!
//! Drives the destination side of a live migration: accepts the
//! protocol negotiation, receives dirty RAM pages, imports device
//! state, and resumes the VM.

use std::sync::{Arc, Mutex};

use futures::sink::SinkExt;
use futures::stream::StreamExt;
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;

use vmm_core::hdl::VmmHdl;
use vmm_core::mem::MemCtx;

use crate::codec::{
    DeviceState, Message, MigrateError, MigrationPreamble, ProtocolOffer,
    ProtocolSelect, TimeData,
};
use crate::protocol::{PageIter, PAGE_SIZE, PROTOCOL_RON_V0};
use crate::state;
use crate::MigrationPhase;
use crate::MigrationStatus;

/// Run the destination side of live migration.
///
/// Accepts a pre-established TCP connection from the source,
/// upgrades to WebSocket (server role), and runs the full protocol.
///
/// On success, the VM is running with all source state restored.
/// On failure, the VM is in an indeterminate state and should be
/// destroyed by the caller.
pub async fn run_destination(
    stream: TcpStream,
    hdl: Arc<VmmHdl>,
    memctx: &MemCtx,
    num_cpus: u32,
    mem_size: u64,
    status: Arc<Mutex<MigrationStatus>>,
) -> Result<(), MigrateError> {
    // Upgrade TCP to WebSocket (server role)
    let ws = tokio_tungstenite::accept_async(stream)
        .await
        .map_err(|e| MigrateError::WebSocket(format!(
            "WebSocket accept failed: {e}"
        )))?;

    run_destination_inner(
        ws, &hdl, memctx, num_cpus, mem_size, &status,
    )
    .await
}

/// Inner protocol driver.
async fn run_destination_inner(
    mut ws: WebSocketStream<TcpStream>,
    hdl: &Arc<VmmHdl>,
    memctx: &MemCtx,
    num_cpus: u32,
    mem_size: u64,
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<(), MigrateError> {
    // ── Phase: Sync ────────────────────────────────────────────────
    set_phase(status, MigrationPhase::Sync);

    // Receive protocol offer
    let offer_msg = recv(&mut ws).await?;
    let offer_data = expect_serialized(offer_msg)?;
    let offer: ProtocolOffer = Message::deserialize(&offer_data)?;

    // Select our preferred protocol
    if !offer.protocols.iter().any(|p| p == PROTOCOL_RON_V0) {
        let err = MigrateError::ProtocolMismatch(format!(
            "source offers {:?}, we require '{}'",
            offer.protocols, PROTOCOL_RON_V0,
        ));
        ws.send(Message::Error(err.clone()).encode()).await?;
        return Err(err);
    }

    let sel = ProtocolSelect { protocol: PROTOCOL_RON_V0.to_string() };
    let sel_msg = Message::serialized(&sel)?;
    ws.send(sel_msg.encode()).await?;

    // Receive preamble
    let pre_msg = recv(&mut ws).await?;
    let pre_data = expect_serialized(pre_msg)?;
    let preamble: MigrationPreamble = Message::deserialize(&pre_data)?;

    // Validate compatibility
    if preamble.num_cpus != num_cpus {
        let err = MigrateError::PreambleMismatch(format!(
            "CPU count mismatch: source has {}, we have {}",
            preamble.num_cpus, num_cpus,
        ));
        ws.send(Message::Error(err.clone()).encode()).await?;
        return Err(err);
    }
    if preamble.mem_size != mem_size {
        let err = MigrateError::PreambleMismatch(format!(
            "memory size mismatch: source has {}, we have {}",
            preamble.mem_size, mem_size,
        ));
        ws.send(Message::Error(err.clone()).encode()).await?;
        return Err(err);
    }

    // Preamble accepted
    ws.send(Message::Okay.encode()).await?;

    // ── Phase: RamPushPrePause ─────────────────────────────────────
    set_phase(status, MigrationPhase::RamPushPrePause);
    receive_ram(&mut ws, memctx, status).await?;

    // ── Phase: RamPushPostPause ───────────────────────────────────
    set_phase(status, MigrationPhase::RamPushPostPause);
    receive_ram(&mut ws, memctx, status).await?;

    // Pause the VM before importing state. This is critical:
    // the kernel's vlapic_data_write() checks vm_is_paused() and
    // defers timer callout scheduling to VM_RESUME. Without this,
    // the LAPIC timer is never re-armed after migration.
    eprintln!("[migrate] pausing VM before state import");
    hdl.pause().map_err(|e| {
        MigrateError::Io(format!("pause before import: {e}"))
    })?;

    // ── Phase: TimeData ───────────────────────────────────────────
    // Time data MUST be imported before device state because LAPIC
    // timer targets are normalized against boot_hrtime.
    set_phase(status, MigrationPhase::TimeData);
    let time_msg = recv(&mut ws).await?;
    let time_data_bytes = expect_serialized(time_msg)?;
    let time_data: TimeData = Message::deserialize(&time_data_bytes)?;
    state::import_time_data(hdl, &time_data)?;

    // ── Phase: DeviceState ────────────────────────────────────────
    set_phase(status, MigrationPhase::DeviceState);
    let dev_msg = recv(&mut ws).await?;
    let dev_data_bytes = expect_serialized(dev_msg)?;
    let device_state: DeviceState = Message::deserialize(&dev_data_bytes)?;
    state::import_device_state(hdl, &device_state)?;

    // Resume the VM. This triggers vlapic_resume() in the kernel,
    // which calls vlapic_callout_reset() to re-arm the LAPIC timer.
    // Also resumes HPET, PIT, and RTC timers.
    eprintln!("[migrate] resuming VM after state import (triggers timer re-arm)");
    hdl.resume().map_err(|e| {
        MigrateError::Io(format!("resume after import: {e}"))
    })?;
    eprintln!("[migrate] VM resumed successfully");

    // ── Phase: RamPull ────────────────────────────────────────────
    set_phase(status, MigrationPhase::RamPull);
    // Receive MemEnd from source (nothing to pull)
    let pull_msg = recv(&mut ws).await?;
    match pull_msg {
        Message::MemEnd => {}
        other => {
            return Err(MigrateError::UnexpectedMessage {
                expected: "MemEnd".to_string(),
                got: format!("{other:?}"),
            });
        }
    }

    // ── Phase: Finish ─────────────────────────────────────────────
    set_phase(status, MigrationPhase::Finish);

    // Signal that we are ready
    ws.send(Message::Okay.encode()).await?;

    // Wait for source acknowledgement
    let ack = recv(&mut ws).await?;
    expect_okay(ack)?;

    // Don't call hdl.resume() here — the destination VM was never
    // paused (it was in init state). The caller (main.rs) will set
    // each vCPU to VRS_RUN after this function returns.

    // Close WebSocket cleanly
    ws.close(None).await.ok();

    Ok(())
}

/// Receive dirty RAM pages from the source.
///
/// Protocol:
/// 1. Receive MemOffer messages (region + dirty bitmap) until MemEnd.
/// 2. Build the list of dirty pages from all bitmaps.
/// 3. Send MemFetch for all dirty pages.
/// 4. Receive MemXfer + Page data.
/// 5. Send MemDone.
async fn receive_ram(
    ws: &mut WebSocketStream<TcpStream>,
    memctx: &MemCtx,
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<(), MigrateError> {
    // Collect all dirty page GPAs from MemOffer messages
    let mut dirty_gpas: Vec<u64> = Vec::new();

    loop {
        let msg = recv(ws).await?;
        match msg {
            Message::MemEnd => break,
            Message::MemOffer { region, bitmap } => {
                // Iterate the bitmap to find dirty pages
                let iter = PageIter::new(
                    &bitmap,
                    region.gpa,
                    region.len as usize,
                );
                dirty_gpas.extend(iter);
            }
            other => {
                return Err(MigrateError::UnexpectedMessage {
                    expected: "MemOffer or MemEnd".to_string(),
                    got: format!("{other:?}"),
                });
            }
        }
    }

    if let Ok(mut s) = status.lock() {
        s.dirty_pages_remaining += dirty_gpas.len() as u64;
    }

    // If no dirty pages, just acknowledge
    if dirty_gpas.is_empty() {
        ws.send(Message::MemDone.encode()).await?;
        return Ok(());
    }

    // Send MemFetch for all dirty pages in batches to avoid
    // excessive message sizes. 8192 pages per batch ~ 32 MiB.
    const FETCH_BATCH_SIZE: usize = 8192;

    for chunk in dirty_gpas.chunks(FETCH_BATCH_SIZE) {
        ws.send(Message::MemFetch(chunk.to_vec()).encode()).await?;

        // Receive MemXfer header
        let xfer_msg = recv(ws).await?;
        let page_count = match xfer_msg {
            Message::MemXfer { count, .. } => count,
            other => {
                return Err(MigrateError::UnexpectedMessage {
                    expected: "MemXfer".to_string(),
                    got: format!("{other:?}"),
                });
            }
        };

        // Receive page data
        for i in 0..page_count {
            let page_msg = recv(ws).await?;
            let page_data = match page_msg {
                Message::Page(data) => data,
                other => {
                    return Err(MigrateError::UnexpectedMessage {
                        expected: "Page".to_string(),
                        got: format!("{other:?}"),
                    });
                }
            };

            if page_data.len() != PAGE_SIZE {
                return Err(MigrateError::Codec(format!(
                    "page data wrong size: expected {PAGE_SIZE}, got {}",
                    page_data.len(),
                )));
            }

            // Write page to guest memory. Skip pages that aren't
            // mapped on the destination (e.g., SEC temp RAM, ROM regions
            // that only exist on the source).
            let page_gpa = chunk[i as usize];
            if let Err(_) = memctx.write(page_gpa, &page_data) {
                // Skip unmapped page — not an error
            }

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

    // Signal that we are done receiving pages
    ws.send(Message::MemDone.encode()).await?;

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
