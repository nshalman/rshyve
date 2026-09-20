// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The migration destination.
//!
//! Every check happens before the guest runs: topology in the preamble,
//! CPU features in the preamble, then the device payload's identity set
//! against the local device set. A guest is only resumed once every
//! device holds its own state.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use vmm_core::cpuid::CpuBaseline;
use vmm_core::hdl::VmmHdl;
use vmm_core::mem::MemCtx;
use vmm_devices::lifecycle::{DeviceMigrateState, DeviceStateError, WireBdf};

use crate::codec::{
    DeviceIdentity, DeviceState, Message, MigrateError, MigrationPreamble,
    ProtocolOffer, ProtocolSelect, TimeData,
};
use crate::cpu;
use crate::pages::apply_page_batch;
use crate::protocol::PROTOCOL_RON;
use crate::state;
use crate::wire::{set_phase, unexpected, Chan, Deadlines, Transport};
use crate::MigrationPhase;
use crate::MigrationStatus;

/// Hand one device the state its counterpart on the source exported.
///
/// Called once per payload entry, with the entry already matched to a
/// local device by PCI address.
pub type DeviceRestoreFn = Box<
    dyn Fn(WireBdf, &DeviceMigrateState) -> Result<(), DeviceStateError> + Send,
>;

/// Start the device workers once the import succeeded.
pub type DeviceResumeFn = Box<dyn Fn() + Send>;

/// What the destination needs from its owner, beyond the stream.
pub struct DestConfig {
    pub num_cpus: u32,
    pub mem_size: u64,
    pub cpu_baseline: CpuBaseline,
    /// Every local device that carries migration state. The payload's
    /// identity set must equal this.
    pub devices: Vec<DeviceIdentity>,
    pub deadlines: Deadlines,
    /// Accept a source whose guest saw CPU features this host lacks.
    ///
    /// The guest may already have executed one of those instructions,
    /// and will take #UD on the next. Logged whenever it is used.
    pub allow_cpu_feature_mismatch: bool,
}

/// The device hooks the import drives.
pub struct DestHooks {
    pub restore: DeviceRestoreFn,
    pub resume: DeviceResumeFn,
}

pub fn run_destination<S: Transport>(
    stream: S,
    hdl: Arc<VmmHdl>,
    memctx: &MemCtx,
    config: DestConfig,
    hooks: DestHooks,
    status: Arc<Mutex<MigrationStatus>>,
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    let mut chan = Chan::new(
        stream,
        tungstenite::protocol::Role::Server,
        config.deadlines,
    )?;
    run_inner(&mut chan, &hdl, memctx, &config, &hooks, &status, log)
}

fn run_inner<S: Transport>(
    chan: &mut Chan<S>,
    hdl: &Arc<VmmHdl>,
    memctx: &MemCtx,
    config: &DestConfig,
    hooks: &DestHooks,
    status: &Arc<Mutex<MigrationStatus>>,
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    // ── Sync ──
    set_phase(status, MigrationPhase::Sync);

    let offer: ProtocolOffer = chan.expect_serialized()?;
    if !offer.protocols.iter().any(|p| p == PROTOCOL_RON) {
        let err = MigrateError::ProtocolMismatch(crate::wire::peer_text(
            &offer.protocols.join(","),
        ));
        chan.send(&Message::Error(err.clone()))?;
        return Err(err);
    }
    chan.send(&Message::serialized(&ProtocolSelect {
        protocol: PROTOCOL_RON.to_string(),
    })?)?;

    let preamble: MigrationPreamble = chan.expect_serialized()?;
    if let Err(err) = check_preamble(&preamble, config, log) {
        chan.report_failure(&err, log);
        return Err(err);
    }
    chan.send(&Message::Okay)?;

    // ── Pre-pause RAM passes ──
    set_phase(status, MigrationPhase::RamPushPrePause);
    receive_pass(chan, memctx, status, PassEnd::PauseSignal)?;

    // ── Post-pause final pass ──
    set_phase(status, MigrationPhase::RamPushPostPause);
    chan.enter_post_pause()?;
    receive_pass(chan, memctx, status, PassEnd::MemEnd)?;

    slog::info!(log, "pausing VM before state import");
    hdl.pause()
        .map_err(|e| MigrateError::Io(format!("pause: {e}")))?;

    // ── TimeData ──
    set_phase(status, MigrationPhase::TimeData);
    let time_data: TimeData = chan.expect_serialized()?;
    if let Err(e) = state::import_time_data(hdl, &time_data, log) {
        chan.report_failure(&e, log);
        return Err(e);
    }

    // ── DeviceState ──
    set_phase(status, MigrationPhase::DeviceState);
    let dev_state: DeviceState = chan.expect_serialized()?;
    if let Err(e) = restore_devices(&dev_state, config, hooks, log) {
        chan.report_failure(&e, log);
        return Err(e);
    }
    if let Err(e) =
        state::import_device_state(hdl, &dev_state, config.num_cpus, log)
    {
        chan.report_failure(&e, log);
        return Err(e);
    }

    // ── RamPull ──
    set_phase(status, MigrationPhase::RamPull);
    match chan.recv()? {
        Message::MemEnd => {}
        other => return Err(unexpected("MemEnd", &other)),
    }

    // ── Finish ──
    // Nothing starts until the source has promised never to run the
    // guest again. Resuming before that commit runs a second copy of
    // the guest against the same disk whenever the source rolls back.
    set_phase(status, MigrationPhase::Finish);
    chan.send(&Message::Okay)?;
    chan.expect_okay()?;

    set_phase(status, MigrationPhase::Committed);
    // The workers were never started on this VM, and the guest is about
    // to post I/O to every one of them.
    (hooks.resume)();
    slog::info!(log, "migration import complete, starting vCPUs");
    hdl.resume()
        .map_err(|e| MigrateError::Io(format!("resume: {e}")))?;

    // The source stays paused until this arrives, so it is what lets a
    // finished source be reaped.
    chan.send(&Message::Okay)?;
    chan.close();
    Ok(())
}

/// Refuse a source this VM cannot stand in for, before a single page is
/// accepted.
fn check_preamble(
    preamble: &MigrationPreamble,
    config: &DestConfig,
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    // Both sides report their boot topology, so a CPU or memory the
    // source hot-added is invisible here. The source refuses to start
    // in that case, which is the only place the growth can be seen.
    if preamble.num_cpus != config.num_cpus {
        return Err(MigrateError::PreambleMismatch(format!(
            "CPU count mismatch: source has {}, we have {}",
            preamble.num_cpus, config.num_cpus,
        )));
    }
    if preamble.mem_size != config.mem_size {
        return Err(MigrateError::PreambleMismatch(format!(
            "memory size mismatch: source has {} bytes, we have {}",
            preamble.mem_size, config.mem_size,
        )));
    }
    check_topology(&preamble.devices, &config.devices)?;

    let Some(ref src) = preamble.cpu_features else {
        return Err(MigrateError::PreambleMismatch(
            "source sent no CPU feature table".into(),
        ));
    };
    let gap = cpu::missing(src, &cpu::local_features(config.cpu_baseline));
    if gap.is_empty() {
        slog::info!(log, "CPU feature check passed");
        return Ok(());
    }
    if config.allow_cpu_feature_mismatch {
        slog::warn!(log, "accepting a CPU feature mismatch by request; \
            the guest takes #UD on an instruction this host lacks";
            "missing" => %gap);
        return Ok(());
    }
    Err(MigrateError::PreambleMismatch(format!(
        "this host lacks CPU features the guest already sees ({gap}); \
         start the destination with --allow-cpu-feature-mismatch to \
         accept the risk",
    )))
}

/// The device set the source carries must be the set this VM has.
///
/// A destination started with one disk fewer would otherwise resume the
/// guest with that disk at reset and no error anywhere.
fn check_topology(
    source: &[DeviceIdentity],
    local: &[DeviceIdentity],
) -> Result<(), MigrateError> {
    let named = |set: &[DeviceIdentity]| -> Vec<String> {
        let mut v: Vec<String> = set.iter().map(|d| d.to_string()).collect();
        v.sort();
        v
    };
    let src = named(source);
    let dst = named(local);
    if src == dst {
        return Ok(());
    }
    let only_src: Vec<&String> =
        src.iter().filter(|d| !dst.contains(d)).collect();
    let only_dst: Vec<&String> =
        dst.iter().filter(|d| !src.contains(d)).collect();
    Err(MigrateError::PreambleMismatch(format!(
        "device set mismatch: only on the source {only_src:?}, \
         only here {only_dst:?}",
    )))
}

/// Give every device the state its counterpart exported, matched by PCI
/// address alone.
fn restore_devices(
    dev_state: &DeviceState,
    config: &DestConfig,
    hooks: &DestHooks,
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    let expected: BTreeMap<WireBdf, &str> = config
        .devices
        .iter()
        .map(|d| (d.bdf, d.kind.as_str()))
        .collect();
    let mut seen: BTreeMap<WireBdf, ()> = BTreeMap::new();
    for entry in &dev_state.emulated {
        if !expected.contains_key(&entry.bdf) {
            return Err(MigrateError::State(format!(
                "payload carries state for {}, which this VM has no device \
                 at",
                entry.bdf,
            )));
        }
        if seen.insert(entry.bdf, ()).is_some() {
            return Err(MigrateError::State(format!(
                "payload carries two states for {}",
                entry.bdf,
            )));
        }
    }
    // A device this VM has and the payload does not would resume at
    // reset while the guest driver believes it configured.
    if let Some(bdf) = expected.keys().find(|bdf| !seen.contains_key(bdf)) {
        return Err(MigrateError::State(format!(
            "payload carries no entry for {} ({})",
            bdf, expected[bdf],
        )));
    }

    slog::info!(log, "restoring device state";
        "devices" => dev_state.emulated.len());
    for entry in &dev_state.emulated {
        let Some(ref state) = entry.state else {
            continue;
        };
        (hooks.restore)(entry.bdf, state).map_err(|error| {
            MigrateError::State(format!("{}: {error}", entry.bdf))
        })?;
    }
    Ok(())
}

/// What ends one RAM pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PassEnd {
    /// Keep taking passes until the source says it has paused.
    PauseSignal,
    /// Take exactly one pass: the final one.
    MemEnd,
}

/// Take RAM passes until the pass end arrives.
///
/// A `MemFetch` commits the peer to sending exactly one `PageBatch`
/// next, and the GPA list it carries is the only thing that says where
/// those pages go. Anything else after it would silently discard them.
fn receive_pass<S: Transport>(
    chan: &mut Chan<S>,
    memctx: &MemCtx,
    status: &Arc<Mutex<MigrationStatus>>,
    until: PassEnd,
) -> Result<(), MigrateError> {
    let mut sparse: Option<Vec<u64>> = None;
    loop {
        let msg = chan.recv()?;
        match (msg, sparse.take()) {
            (
                Message::PageBatch {
                    base_gpa,
                    page_count,
                    flags,
                    data,
                },
                gpas,
            ) => {
                apply_page_batch(
                    memctx,
                    base_gpa,
                    page_count,
                    flags,
                    &data,
                    gpas.as_deref(),
                    status,
                )?;
            }
            (Message::MemFetch(gpas), None) => sparse = Some(gpas),
            (Message::MemEnd, None) => {
                chan.send(&Message::MemDone)?;
                if until == PassEnd::MemEnd {
                    return Ok(());
                }
            }
            (Message::PauseSignal, None) if until == PassEnd::PauseSignal => {
                return Ok(());
            }
            (other, Some(_)) => {
                return Err(unexpected("PageBatch following MemFetch", &other));
            }
            (other, None) => {
                return Err(unexpected(
                    "PageBatch/MemFetch/MemEnd/PauseSignal",
                    &other,
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident(dev: u8, kind: &str) -> DeviceIdentity {
        DeviceIdentity {
            bdf: WireBdf {
                bus: 0,
                dev,
                func: 0,
            },
            kind: kind.to_string(),
        }
    }

    #[test]
    fn a_matching_device_set_is_accepted() {
        let set = vec![ident(4, "nvme"), ident(5, "virtio-blk")];
        check_topology(&set, &set).expect("same set");
    }

    #[test]
    fn set_order_does_not_matter() {
        let src = vec![ident(5, "virtio-blk"), ident(4, "nvme")];
        let dst = vec![ident(4, "nvme"), ident(5, "virtio-blk")];
        check_topology(&src, &dst).expect("same set, other order");
    }

    #[test]
    fn a_destination_started_with_one_disk_fewer_is_refused() {
        // The guest would otherwise arrive with that disk at reset and
        // no error anywhere.
        let src = vec![ident(4, "nvme"), ident(5, "virtio-blk")];
        let dst = vec![ident(4, "nvme")];
        let error = check_topology(&src, &dst).expect_err("must refuse");
        assert!(error.to_string().contains("virtio-blk@0.5.0"), "{error}");
    }

    #[test]
    fn a_device_of_another_kind_at_the_same_address_is_refused() {
        let src = vec![ident(4, "nvme")];
        let dst = vec![ident(4, "virtio-blk")];
        check_topology(&src, &dst).expect_err("kind is part of identity");
    }
}
