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
use slog;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::WebSocketStream;

use vmm_core::hdl::VmmHdl;
use vmm_core::mem::MemCtx;

use crate::codec::{
    DeviceState, Message, MigrateError, MigrationPreamble, ProtocolOffer,
    ProtocolSelect, TimeData,
};
use crate::pages::apply_page_batch;
use crate::protocol::PROTOCOL_RON_V0;
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
pub async fn run_destination<S>(
    stream: S,
    hdl: Arc<VmmHdl>,
    memctx: &MemCtx,
    num_cpus: u32,
    mem_size: u64,
    status: Arc<Mutex<MigrationStatus>>,
    log: &slog::Logger,
) -> Result<(), MigrateError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Upgrade to WebSocket (server role)
    let ws = tokio_tungstenite::accept_async_with_config(
        stream,
        Some(crate::limits::ws_config()),
    )
    .await
    .map_err(|e| {
        MigrateError::WebSocket(format!("WebSocket accept failed: {e}"))
    })?;

    run_destination_inner(ws, &hdl, memctx, num_cpus, mem_size, &status, log)
        .await
}

/// Inner protocol driver.
async fn run_destination_inner<S: AsyncRead + AsyncWrite + Unpin>(
    mut ws: WebSocketStream<S>,
    hdl: &Arc<VmmHdl>,
    memctx: &MemCtx,
    num_cpus: u32,
    mem_size: u64,
    status: &Arc<Mutex<MigrationStatus>>,
    log: &slog::Logger,
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

    let sel = ProtocolSelect {
        protocol: PROTOCOL_RON_V0.to_string(),
    };
    let sel_msg = Message::serialized(&sel)?;
    ws.send(sel_msg.encode()).await?;

    // Receive preamble
    let pre_msg = recv(&mut ws).await?;
    let pre_data = expect_serialized(pre_msg)?;
    let preamble: MigrationPreamble = Message::deserialize(&pre_data)?;

    // Validate compatibility. Both sides report their boot topology,
    // so a CPU or memory the source hot-added is invisible here. The
    // source refuses to start in that case.
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
            "memory size mismatch: source has {} bytes, we have {}",
            preamble.mem_size, mem_size,
        ));
        ws.send(Message::Error(err.clone()).encode()).await?;
        return Err(err);
    }

    // Check CPU feature compatibility: destination must support all
    // features the source exposes to the guest.
    if let Some(ref src_cpu) = preamble.cpu_features {
        let (
            dst_l1_ecx,
            dst_l1_edx,
            dst_l7_ebx,
            dst_l7_ecx,
            dst_l7_edx,
            dst_xcr0,
        ) = vmm_core::cpuid::query_host_features();

        // Mask out deprecated/irrelevant features that don't affect
        // guest instruction compatibility:
        // - leaf7 EBX bit 2: SGX (enclave, not migrated)
        // - leaf7 EBX bit 14: MPX (deprecated, removed from newer CPUs)
        // - xcr0 bits 3,4: MPX BNDREGS/BNDCSR
        const IGNORE_L7_EBX: u32 = (1 << 2) | (1 << 14);
        const IGNORE_XCR0: u32 = (1 << 3) | (1 << 4);

        let missing_l1_ecx = src_cpu.leaf1_ecx & !dst_l1_ecx;
        let missing_l1_edx = src_cpu.leaf1_edx & !dst_l1_edx;
        let missing_l7_ebx = (src_cpu.leaf7_ebx & !IGNORE_L7_EBX) & !dst_l7_ebx;
        let missing_l7_ecx = src_cpu.leaf7_ecx & !dst_l7_ecx;
        let missing_l7_edx = src_cpu.leaf7_edx & !dst_l7_edx;
        let missing_xcr0 = (src_cpu.xcr0 & !IGNORE_XCR0) & !dst_xcr0;

        if missing_l1_ecx != 0
            || missing_l1_edx != 0
            || missing_l7_ebx != 0
            || missing_l7_ecx != 0
            || missing_l7_edx != 0
            || missing_xcr0 != 0
        {
            // Warn but continue — the source's --cpu-baseline already
            // restricted guest-visible features. The preamble reports
            // host features which may exceed the guest's actual view.
            // A strict check would compare the VM's effective CPUID.
            slog::warn!(log, "CPU feature mismatch (continuing)";
                "leaf1_ecx" => format!("{missing_l1_ecx:#x}"),
                "leaf1_edx" => format!("{missing_l1_edx:#x}"),
                "leaf7_ebx" => format!("{missing_l7_ebx:#x}"),
                "leaf7_ecx" => format!("{missing_l7_ecx:#x}"),
                "leaf7_edx" => format!("{missing_l7_edx:#x}"),
                "xcr0" => format!("{missing_xcr0:#x}"),
            );
        } else {
            slog::info!(log, "CPU feature check passed");
        }
    }

    // Preamble accepted
    ws.send(Message::Okay.encode()).await?;

    // ── Phase: RamPushPrePause ─────────────────────────────────────
    // Receive RAM passes in a loop. The source sends one full pass
    // then iterative dirty passes. When the source is ready to pause,
    // it sends PauseSignal followed by one final post-pause pass.
    set_phase(status, MigrationPhase::RamPushPrePause);
    loop {
        let pass_result = receive_ram_pass(&mut ws, memctx, status).await?;
        match pass_result {
            RamPassResult::MorePasses => {
                // Source will send another pre-pause dirty pass
                continue;
            }
            RamPassResult::PauseSignal => {
                // Source has paused; next pass is the final one
                break;
            }
        }
    }

    // ── Phase: RamPushPostPause ───────────────────────────────────
    // Final pass after source paused — no peek for PauseSignal needed.
    set_phase(status, MigrationPhase::RamPushPostPause);
    receive_final_ram_pass(&mut ws, memctx, status).await?;

    // Pause the VM before importing state. This is critical:
    // the kernel's vlapic_data_write() checks vm_is_paused() and
    // defers timer callout scheduling to VM_RESUME. Without this,
    // the LAPIC timer is never re-armed after migration.
    slog::info!(log, "pausing VM before state import");
    hdl.pause()
        .map_err(|e| MigrateError::Io(format!("pause before import: {e}")))?;

    // ── Phase: TimeData ───────────────────────────────────────────
    // Time data MUST be imported before device state because LAPIC
    // timer targets are normalized against boot_hrtime.
    set_phase(status, MigrationPhase::TimeData);
    let time_msg = recv(&mut ws).await?;
    let time_data_bytes = expect_serialized(time_msg)?;
    let time_data: TimeData = Message::deserialize(&time_data_bytes)?;
    state::import_time_data(hdl, &time_data, log)?;

    // ── Phase: DeviceState ────────────────────────────────────────
    set_phase(status, MigrationPhase::DeviceState);
    let dev_msg = recv(&mut ws).await?;
    let dev_data_bytes = expect_serialized(dev_msg)?;
    let device_state: DeviceState = Message::deserialize(&dev_data_bytes)?;
    state::import_device_state(hdl, &device_state, log)?;

    // Resume the VM. This triggers vlapic_resume() in the kernel,
    // which calls vlapic_callout_reset() to re-arm the LAPIC timer.
    // Also resumes HPET, PIT, and RTC timers.
    slog::info!(
        log,
        "resuming VM after state import (triggers timer re-arm)"
    );
    hdl.resume()
        .map_err(|e| MigrateError::Io(format!("resume after import: {e}")))?;
    slog::info!(log, "VM resumed successfully");

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

/// Result of receiving one RAM pass from the source.
enum RamPassResult {
    /// Source will send more pre-pause passes.
    MorePasses,
    /// Source has paused; the next pass is the final one.
    PauseSignal,
}

/// Receive one RAM pass from the source.
///
/// The source sends PageBatch messages (contiguous, compressed) and
/// optionally MemFetch+PageBatch (sparse dirty pages) until MemEnd.
/// After MemEnd, we ack with MemDone. Then we check if the next
/// message is PauseSignal (source paused) or another RAM pass starts.
async fn receive_ram_pass<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
    memctx: &MemCtx,
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<RamPassResult, MigrateError> {
    let mut sparse_gpas: Option<Vec<u64>> = None;

    loop {
        let msg = recv(ws).await?;
        match msg {
            Message::MemEnd => {
                // This pass is done. Ack it.
                ws.send(Message::MemDone.encode()).await?;

                // Now wait for next action from source. Could be:
                // - PauseSignal: source paused, final pass follows
                // - MemEnd: empty convergence pass (0 dirty pages)
                // - PageBatch/MemFetch: start of next dirty pass
                loop {
                    let next = recv(ws).await?;
                    match next {
                        Message::PauseSignal => {
                            return Ok(RamPassResult::PauseSignal)
                        }
                        Message::MemEnd => {
                            // Empty pass — ack and keep waiting
                            ws.send(Message::MemDone.encode()).await?;
                            continue;
                        }
                        Message::PageBatch {
                            base_gpa,
                            page_count,
                            flags,
                            data,
                        } => {
                            apply_page_batch(
                                memctx, base_gpa, page_count, flags, &data,
                                None, status,
                            )?;
                            continue_ram_pass(ws, memctx, status).await?;
                            return Ok(RamPassResult::MorePasses);
                        }
                        Message::MemFetch(gpas) => {
                            sparse_gpas = Some(gpas);
                            continue_ram_pass_with_sparse(
                                ws,
                                memctx,
                                status,
                                &mut sparse_gpas,
                            )
                            .await?;
                            return Ok(RamPassResult::MorePasses);
                        }
                        other => {
                            return Err(MigrateError::UnexpectedMessage {
                                expected: "PauseSignal, PageBatch, or MemEnd"
                                    .to_string(),
                                got: format!("{other:?}"),
                            })
                        }
                    }
                }
            }
            Message::PageBatch {
                base_gpa,
                page_count,
                flags,
                data,
            } => {
                // Contiguous batch of pages
                apply_page_batch(
                    memctx, base_gpa, page_count, flags, &data, None, status,
                )?;
            }
            Message::MemFetch(gpas) => {
                // Sparse dirty pages: GPA list followed by PageBatch
                sparse_gpas = Some(gpas);
            }
            other => {
                return Err(MigrateError::UnexpectedMessage {
                    expected: "PageBatch, MemFetch, or MemEnd".to_string(),
                    got: format!("{other:?}"),
                });
            }
        }

        // If we received a MemFetch, the next message should be a PageBatch
        if let Some(ref gpas) = sparse_gpas {
            let batch_msg = recv(ws).await?;
            match batch_msg {
                Message::PageBatch {
                    base_gpa,
                    page_count,
                    flags,
                    data,
                } => {
                    apply_page_batch(
                        memctx,
                        base_gpa,
                        page_count,
                        flags,
                        &data,
                        Some(gpas),
                        status,
                    )?;
                }
                other => {
                    return Err(MigrateError::UnexpectedMessage {
                        expected: "PageBatch after MemFetch".to_string(),
                        got: format!("{other:?}"),
                    });
                }
            }
            sparse_gpas = None;
        }
    }
}

/// Receive the final post-pause RAM pass. No peek for PauseSignal —
/// just batches until MemEnd, then ack and return.
async fn receive_final_ram_pass<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
    memctx: &MemCtx,
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<(), MigrateError> {
    let mut sparse_gpas: Option<Vec<u64>> = None;
    loop {
        if let Some(ref gpas) = sparse_gpas {
            let batch_msg = recv(ws).await?;
            match batch_msg {
                Message::PageBatch {
                    base_gpa,
                    page_count,
                    flags,
                    data,
                } => {
                    apply_page_batch(
                        memctx,
                        base_gpa,
                        page_count,
                        flags,
                        &data,
                        Some(gpas),
                        status,
                    )?;
                }
                other => {
                    return Err(MigrateError::UnexpectedMessage {
                        expected: "PageBatch after MemFetch".to_string(),
                        got: format!("{other:?}"),
                    });
                }
            }
            sparse_gpas = None;
        }

        let msg = recv(ws).await?;
        match msg {
            Message::MemEnd => {
                ws.send(Message::MemDone.encode()).await?;
                return Ok(());
            }
            Message::PageBatch {
                base_gpa,
                page_count,
                flags,
                data,
            } => {
                apply_page_batch(
                    memctx, base_gpa, page_count, flags, &data, None, status,
                )?;
            }
            Message::MemFetch(gpas) => {
                sparse_gpas = Some(gpas);
            }
            other => {
                return Err(MigrateError::UnexpectedMessage {
                    expected: "PageBatch, MemFetch, or MemEnd".to_string(),
                    got: format!("{other:?}"),
                });
            }
        }
    }
}

/// Continue receiving a RAM pass after we already consumed one message.
async fn continue_ram_pass<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
    memctx: &MemCtx,
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<(), MigrateError> {
    let mut sparse_gpas: Option<Vec<u64>> = None;
    continue_ram_pass_with_sparse(ws, memctx, status, &mut sparse_gpas).await
}

async fn continue_ram_pass_with_sparse<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
    memctx: &MemCtx,
    status: &Arc<Mutex<MigrationStatus>>,
    sparse_gpas: &mut Option<Vec<u64>>,
) -> Result<(), MigrateError> {
    loop {
        // Handle pending sparse GPAs
        if let Some(ref gpas) = sparse_gpas {
            let batch_msg = recv(ws).await?;
            match batch_msg {
                Message::PageBatch {
                    base_gpa,
                    page_count,
                    flags,
                    data,
                } => {
                    apply_page_batch(
                        memctx,
                        base_gpa,
                        page_count,
                        flags,
                        &data,
                        Some(gpas),
                        status,
                    )?;
                }
                other => {
                    return Err(MigrateError::UnexpectedMessage {
                        expected: "PageBatch after MemFetch".to_string(),
                        got: format!("{other:?}"),
                    });
                }
            }
            *sparse_gpas = None;
        }

        let msg = recv(ws).await?;
        match msg {
            Message::MemEnd => {
                ws.send(Message::MemDone.encode()).await?;
                return Ok(());
            }
            Message::PageBatch {
                base_gpa,
                page_count,
                flags,
                data,
            } => {
                apply_page_batch(
                    memctx, base_gpa, page_count, flags, &data, None, status,
                )?;
            }
            Message::MemFetch(gpas) => {
                *sparse_gpas = Some(gpas);
            }
            other => {
                return Err(MigrateError::UnexpectedMessage {
                    expected: "PageBatch, MemFetch, or MemEnd".to_string(),
                    got: format!("{other:?}"),
                });
            }
        }
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
