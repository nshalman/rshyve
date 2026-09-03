// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Synchronous migration destination — runs over a Unix socket without tokio.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use tungstenite::WebSocket;

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

/// Run the destination side of live migration over a synchronous stream.
/// Callback for restoring device queue state on destination devices.
pub type DeviceRestoreFn = Box<
    dyn Fn(
            &[crate::codec::VirtioQueueState],
            &[crate::codec::NvmeControllerState],
        ) + Send,
>;

pub fn run_destination_sync<S: Read + Write>(
    stream: S,
    hdl: Arc<VmmHdl>,
    memctx: &MemCtx,
    num_cpus: u32,
    mem_size: u64,
    status: Arc<Mutex<MigrationStatus>>,
    log: &slog::Logger,
    device_restore: Option<DeviceRestoreFn>,
) -> Result<(), MigrateError> {
    let mut ws = WebSocket::from_raw_socket(
        stream,
        tungstenite::protocol::Role::Server,
        Some(crate::limits::ws_config()),
    );

    run_inner(
        &mut ws,
        &hdl,
        memctx,
        num_cpus,
        mem_size,
        &status,
        log,
        device_restore,
    )
}

fn run_inner<S: Read + Write>(
    ws: &mut WebSocket<S>,
    hdl: &Arc<VmmHdl>,
    memctx: &MemCtx,
    num_cpus: u32,
    mem_size: u64,
    status: &Arc<Mutex<MigrationStatus>>,
    log: &slog::Logger,
    device_restore: Option<DeviceRestoreFn>,
) -> Result<(), MigrateError> {
    // ── Sync ──
    set_phase(status, MigrationPhase::Sync);

    let offer_data = expect_serialized(recv(ws)?)?;
    let offer: ProtocolOffer = Message::deserialize(&offer_data)?;
    if !offer.protocols.iter().any(|p| p == PROTOCOL_RON_V0) {
        let err =
            MigrateError::ProtocolMismatch(format!("{:?}", offer.protocols));
        send(ws, &Message::Error(err.clone()))?;
        return Err(err);
    }
    send(
        ws,
        &Message::serialized(&ProtocolSelect {
            protocol: PROTOCOL_RON_V0.to_string(),
        })?,
    )?;

    let pre_data = expect_serialized(recv(ws)?)?;
    let preamble: MigrationPreamble = Message::deserialize(&pre_data)?;
    // Both sides report their boot topology, so a CPU or memory the
    // source hot-added is invisible here. The source refuses to start
    // in that case, which is the only place the growth can be seen.
    if preamble.num_cpus != num_cpus {
        let err = MigrateError::PreambleMismatch(format!(
            "CPU count mismatch: source has {}, we have {}",
            preamble.num_cpus, num_cpus
        ));
        send(ws, &Message::Error(err.clone()))?;
        return Err(err);
    }
    if preamble.mem_size != mem_size {
        let err = MigrateError::PreambleMismatch(format!(
            "memory size mismatch: source has {} bytes, we have {}",
            preamble.mem_size, mem_size
        ));
        send(ws, &Message::Error(err.clone()))?;
        return Err(err);
    }

    // CPU feature check (warn only)
    if let Some(ref src) = preamble.cpu_features {
        // OSF-3 in docs/security/open-findings.md tracks extending the sync
        // compatibility policy beyond leaf 7; retain these values for it.
        let (_d1e, _d1d, d7b, d7c, d7d, _dxcr0) =
            vmm_core::cpuid::query_host_features();
        let m7b = src.leaf7_ebx & !d7b;
        let m7c = src.leaf7_ecx & !d7c;
        let m7d = src.leaf7_edx & !d7d;
        if m7b != 0 || m7c != 0 || m7d != 0 {
            slog::warn!(log, "CPU feature mismatch (continuing)";
                "leaf7_ebx" => format!("{m7b:#x}"),
                "leaf7_ecx" => format!("{m7c:#x}"),
                "leaf7_edx" => format!("{m7d:#x}"),
            );
        } else {
            slog::info!(log, "CPU feature check passed");
        }
    }

    send(ws, &Message::Okay)?;

    // ── Pre-pause RAM passes ──
    set_phase(status, MigrationPhase::RamPushPrePause);
    loop {
        let result = receive_ram_pass(ws, memctx, status)?;
        if result == PassResult::PauseSignal {
            break;
        }
    }

    // ── Post-pause final pass ──
    set_phase(status, MigrationPhase::RamPushPostPause);
    receive_final_pass(ws, memctx, status)?;

    // Pause VM before importing state
    slog::info!(log, "pausing VM before state import");
    hdl.pause()
        .map_err(|e| MigrateError::Io(format!("pause: {e}")))?;

    // ── TimeData ──
    set_phase(status, MigrationPhase::TimeData);
    let time_bytes = expect_serialized(recv(ws)?)?;
    let time_data: TimeData = Message::deserialize(&time_bytes)?;
    if let Err(e) = state::import_time_data(hdl, &time_data, log) {
        report_failure(ws, &e, log);
        return Err(e);
    }

    // ── DeviceState ──
    set_phase(status, MigrationPhase::DeviceState);
    let dev_bytes = expect_serialized(recv(ws)?)?;
    let dev_state: DeviceState = Message::deserialize(&dev_bytes)?;

    // The guest's vCPUs must not run until virtio rings and NVMe queues
    // are programmed. Without the callback nothing can program them, and
    // the guest resumes into filesystem and network hangs, so refuse.
    let has_virtio = !dev_state.virtio_queues.is_empty();
    let has_nvme = !dev_state.nvme_controllers.is_empty();
    if (has_virtio || has_nvme) && device_restore.is_none() {
        let e = MigrateError::State(format!(
            "payload has {} virtio queues and {} NVMe controllers, but no \
             device restore callback was installed",
            dev_state.virtio_queues.len(),
            dev_state.nvme_controllers.len(),
        ));
        report_failure(ws, &e, log);
        return Err(e);
    }

    if let Err(e) = state::import_device_state(hdl, &dev_state, log) {
        report_failure(ws, &e, log);
        return Err(e);
    }

    // ── Restore device queue state BEFORE resume ──
    if has_virtio || has_nvme {
        slog::info!(log, "restoring device queue state";
            "virtio_queues" => dev_state.virtio_queues.len(),
            "nvme_controllers" => dev_state.nvme_controllers.len());
        if let Some(ref restore_fn) = device_restore {
            restore_fn(&dev_state.virtio_queues, &dev_state.nvme_controllers);
        }
    }

    // Resume — guest starts with correct ring state
    slog::info!(log, "migration import complete, starting vCPUs");
    hdl.resume()
        .map_err(|e| MigrateError::Io(format!("resume: {e}")))?;

    // ── RamPull ──
    set_phase(status, MigrationPhase::RamPull);
    let pull = recv(ws)?;
    match pull {
        Message::MemEnd => {}
        other => {
            return Err(MigrateError::UnexpectedMessage {
                expected: "MemEnd".into(),
                got: format!("{other:?}"),
            });
        }
    }

    // ── Finish ──
    set_phase(status, MigrationPhase::Finish);
    send(ws, &Message::Okay)?;
    expect_okay(recv(ws)?)?;
    let _ = ws.close(None);
    Ok(())
}

/// Tell the source why the import stopped.
///
/// The source resumes the guest when the migration aborts. It needs the
/// reason: a dropped connection alone does not say whether the state
/// was rejected or the network failed.
fn report_failure<S: Read + Write>(
    ws: &mut WebSocket<S>,
    err: &MigrateError,
    log: &slog::Logger,
) {
    slog::error!(log, "migration import failed"; "error" => %err);
    if let Err(send_err) = send(ws, &Message::Error(err.clone())) {
        slog::warn!(log, "could not report the failure to the source";
            "error" => %send_err,
        );
    }
}

#[derive(PartialEq)]
enum PassResult {
    // Retained for the pending migration-engine restructuring; removing a
    // protocol state here would prejudge that work.
    #[allow(dead_code)]
    MorePasses,
    PauseSignal,
}

fn receive_ram_pass<S: Read + Write>(
    ws: &mut WebSocket<S>,
    memctx: &MemCtx,
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<PassResult, MigrateError> {
    let mut sparse: Option<Vec<u64>> = None;
    loop {
        // Handle pending sparse
        if let Some(ref gpas) = sparse {
            // A MemFetch commits the peer to sending exactly one PageBatch
            // next. Dropping anything else here would silently discard the
            // pages those GPAs stand for.
            match recv(ws)? {
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
                        expected: "PageBatch following MemFetch".into(),
                        got: format!("{other:?}"),
                    });
                }
            }
            sparse = None;
            continue;
        }

        let msg = recv(ws)?;
        match msg {
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
                sparse = Some(gpas);
            }
            Message::MemEnd => {
                send(ws, &Message::MemDone)?;
                // Wait for next action
                loop {
                    let next = recv(ws)?;
                    match next {
                        Message::PauseSignal => {
                            return Ok(PassResult::PauseSignal)
                        }
                        Message::MemEnd => {
                            send(ws, &Message::MemDone)?;
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
                            break; // continue outer loop for rest of pass
                        }
                        Message::MemFetch(gpas) => {
                            sparse = Some(gpas);
                            break;
                        }
                        other => {
                            return Err(MigrateError::UnexpectedMessage {
                                expected: "PauseSignal/PageBatch/MemEnd".into(),
                                got: format!("{other:?}"),
                            })
                        }
                    }
                }
            }
            other => {
                return Err(MigrateError::UnexpectedMessage {
                    expected: "PageBatch/MemFetch/MemEnd".into(),
                    got: format!("{other:?}"),
                })
            }
        }
    }
}

fn receive_final_pass<S: Read + Write>(
    ws: &mut WebSocket<S>,
    memctx: &MemCtx,
    status: &Arc<Mutex<MigrationStatus>>,
) -> Result<(), MigrateError> {
    let mut sparse: Option<Vec<u64>> = None;
    loop {
        if let Some(ref gpas) = sparse {
            // A MemFetch commits the peer to sending exactly one PageBatch
            // next. Dropping anything else here would silently discard the
            // pages those GPAs stand for.
            match recv(ws)? {
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
                        expected: "PageBatch following MemFetch".into(),
                        got: format!("{other:?}"),
                    });
                }
            }
            sparse = None;
            continue;
        }
        let msg = recv(ws)?;
        match msg {
            Message::MemEnd => {
                send(ws, &Message::MemDone)?;
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
                sparse = Some(gpas);
            }
            other => {
                return Err(MigrateError::UnexpectedMessage {
                    expected: "PageBatch/MemEnd".into(),
                    got: format!("{other:?}"),
                })
            }
        }
    }
}

fn send<S: Read + Write>(
    ws: &mut WebSocket<S>,
    msg: &Message,
) -> Result<(), MigrateError> {
    let encoded = msg.encode();
    let data = match encoded {
        tungstenite::Message::Binary(d) => d,
        _ => return Err(MigrateError::Codec("non-binary".into())),
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
            return Err(MigrateError::WebSocket("closed".into()))
        }
        _ => return Err(MigrateError::Codec("non-binary".into())),
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

fn set_phase(status: &Arc<Mutex<MigrationStatus>>, phase: MigrationPhase) {
    if let Ok(mut s) = status.lock() {
        s.phase = phase;
    }
}
