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
use vmm_devices::lifecycle::{
    DeviceMigrateState, DeviceStateError, HypervMigrateState, PlatformIdentity,
    PlatformStatePayload, WireBdf,
};

use crate::codec::{
    DeviceIdentity, DeviceState, Message, MigrateError, MigrationPreamble,
    ProtocolOffer, ProtocolSelect, TimeData,
};
use crate::cpu;
use crate::pages::apply_page_batch;
use crate::protocol::{self, protocol_blocker, select_protocol};
use crate::state;
use crate::wire::{
    cancel_or, not_cancelled, set_phase, unexpected, Chan, Deadlines, Transport,
};
use crate::xsave::{XsaveLayout, XsavePlan};
use crate::MigrationPhase;
use crate::MigrationStatus;
use crate::StopGate;
use crate::VmPauseFn;

/// Hand one device the state its counterpart on the source exported.
///
/// Called once per payload entry, with the entry already matched to a
/// local device by PCI address.
pub type DeviceRestoreFn = Box<
    dyn Fn(WireBdf, &DeviceMigrateState) -> Result<(), DeviceStateError> + Send,
>;

/// Give one device with no PCI address the state the source exported
/// under the same registry id.
pub type PlatformRestoreFn = Box<
    dyn Fn(&str, &DeviceMigrateState) -> Result<(), DeviceStateError> + Send,
>;

/// Start the device workers once the import succeeded.
pub type DeviceResumeFn = Box<dyn Fn() + Send>;

/// Attach every disk the source gave up at its commit. Runs after the
/// commit and before any device resumes: a device resume alone can
/// submit the writes the guest left queued.
pub type DeviceAttachFn = Box<dyn Fn() -> Result<(), String> + Send>;

/// Put the Hyper-V enlightenment back, reinstalling both overlay
/// pages from the MSR values the source carried.
///
/// Answers whether this VM has an enlightenment at all, so a payload
/// that carries one for a VM started without `--hyperv` is refused
/// rather than dropped.
pub type HypervRestoreFn = Box<dyn Fn(&HypervMigrateState) -> bool + Send>;

/// What the destination needs from its owner, beyond the stream.
pub struct DestConfig {
    pub num_cpus: u32,
    pub mem_size: u64,
    pub cpu_baseline: CpuBaseline,
    /// Every local device that carries migration state. The payload's
    /// identity set must equal this.
    pub devices: Vec<DeviceIdentity>,
    /// The same for the devices with no PCI address.
    pub platform: Vec<PlatformIdentity>,
    pub deadlines: Deadlines,
    /// Whether this VM was started with `--hyperv`. The payload must
    /// carry the enlightenment exactly when it was.
    pub hyperv: bool,
    /// Accept a source whose guest saw CPU features this host lacks.
    ///
    /// The guest may already have executed one of those instructions,
    /// and will take #UD on the next. Logged whenever it is used.
    pub allow_cpu_feature_mismatch: bool,
}

/// The device hooks the import drives.
pub struct DestHooks {
    pub restore: DeviceRestoreFn,
    pub restore_platform: PlatformRestoreFn,
    pub attach: DeviceAttachFn,
    pub resume: DeviceResumeFn,
    pub hyperv: HypervRestoreFn,
    pub pause_vm: VmPauseFn,
    pub resume_vm: VmPauseFn,
    pub gate: Box<dyn StopGate>,
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
    // A send has no deadline of its own (see `Transport`). A source that
    // stops reading would hold this end, and its paused guest, forever.
    let (stream, watch) =
        crate::stall::watch(stream, config.deadlines.stall, log)
            .map_err(|e| MigrateError::Io(format!("stall watchdog: {e}")))?;
    let mut chan = Chan::new(
        stream,
        tungstenite::protocol::Role::Server,
        config.deadlines,
    )?;
    let result =
        run_inner(&mut chan, &hdl, memctx, &config, &hooks, &status, log)
            .map_err(|e| {
                if watch.stalled() {
                    MigrateError::Io(watch.reason())
                } else {
                    e
                }
            });
    if let Err(error) = &result {
        if crate::is_cancel(error) {
            chan.report_failure(error, log);
        }
    }
    result
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
    let Some(protocol) = select_protocol(&offer.protocols) else {
        let err = MigrateError::ProtocolMismatch(crate::wire::peer_text(
            &offer.protocols.join(","),
        ));
        chan.send(&Message::Error(err.clone()))?;
        return Err(err);
    };
    if let Some(why) =
        protocol_blocker(protocol, &config.devices, &config.platform)
    {
        let err = MigrateError::ProtocolMismatch(why);
        chan.send(&Message::Error(err.clone()))?;
        return Err(err);
    }
    let gate = hooks.gate.as_ref();
    chan.send_uncommitted(
        gate,
        &Message::serialized(&ProtocolSelect {
            protocol: protocol.to_string(),
        })?,
    )?;

    let preamble: MigrationPreamble = chan.expect_serialized()?;
    let checked = XsaveLayout::local(hdl).and_then(|local| {
        check_preamble(&preamble, config, &local, log)?;
        check_enlightenments(&preamble, config.hyperv, cpu::host_is_amd())?;
        let skew = check_clock(&preamble, protocol, log)?;
        Ok((local, skew))
    });
    let (local_xsave, skew_ms) = match checked {
        Ok((local, skew)) => {
            if let Ok(mut s) = status.lock() {
                s.clock_skew_ms = skew;
            }
            (local, skew)
        }
        Err(err) => {
            chan.report_failure(&err, log);
            return Err(err);
        }
    };
    chan.send_uncommitted(gate, &Message::Okay)?;

    // ── Pre-pause RAM passes ──
    set_phase(status, MigrationPhase::RamPushPrePause);
    receive_pass(chan, memctx, gate, status, PassEnd::PauseSignal)?;

    // ── Post-pause final pass ──
    set_phase(status, MigrationPhase::RamPushPostPause);
    chan.enter_post_pause()?;
    receive_pass(chan, memctx, gate, status, PassEnd::MemEnd)?;

    slog::info!(log, "pausing VM before state import");
    (hooks.pause_vm)().map_err(|e| {
        cancel_or(gate, MigrateError::Io(format!("pause: {e}")))
    })?;

    // ── TimeData ──
    set_phase(status, MigrationPhase::TimeData);
    let time_data: TimeData = chan.expect_serialized()?;
    if let Err(e) = state::import_time_data(hdl, &time_data, skew_ms, log) {
        chan.report_failure(&e, log);
        return Err(e);
    }

    // ── DeviceState ──
    set_phase(status, MigrationPhase::DeviceState);
    let mut dev_state: DeviceState = chan.expect_serialized()?;
    not_cancelled(gate)?;
    if let Err(e) = crate::vram::accept(&mut dev_state.emulated)
        .and_then(|()| restore_devices(&dev_state, config, hooks, log))
    {
        chan.report_failure(&e, log);
        return Err(e);
    }
    let xsave = xsave_plan(&preamble, &local_xsave);
    if let Err(e) = state::import_device_state(
        hdl,
        &dev_state,
        config.num_cpus,
        &xsave,
        protocol::at_least(protocol, protocol::PROTOCOL_RON_V10),
        log,
    ) {
        chan.report_failure(&e, log);
        return Err(e);
    }

    // ── RamPull ──
    set_phase(status, MigrationPhase::RamPull);
    match chan.recv()? {
        Message::MemEnd => {}
        other => return Err(unexpected("MemEnd", &other)),
    }

    take_over(chan, hooks, status, log)
}

/// Nothing starts until the source has promised never to run the guest
/// again. Resuming before that commit runs a second copy of the guest
/// against the same disk whenever the source rolls back.
fn take_over<S: Transport>(
    chan: &mut Chan<S>,
    hooks: &DestHooks,
    status: &Arc<Mutex<MigrationStatus>>,
    log: &slog::Logger,
) -> Result<(), MigrateError> {
    set_phase(status, MigrationPhase::Finish);
    hooks.gate.commit()?;
    chan.send(&Message::Okay)?;
    chan.expect_okay()?;

    set_phase(status, MigrationPhase::Committed);
    (hooks.attach)().map_err(|e| {
        slog::error!(log, "a disk did not attach after the commit; the \
            guest stays paused here"; "error" => %e);
        MigrateError::State(format!("attach after the commit: {e}"))
    })?;
    // The workers were never started on this VM, and the guest is about
    // to post I/O to every one of them.
    (hooks.resume)();
    slog::info!(log, "migration import complete, starting vCPUs");
    (hooks.resume_vm)()
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
    local_xsave: &XsaveLayout,
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
    check_topology(&preamble.platform, &config.platform)?;

    let Some(ref src) = preamble.cpu_features else {
        return Err(MigrateError::PreambleMismatch(
            "source sent no CPU feature table".into(),
        ));
    };
    // Not covered by allow_cpu_feature_mismatch: a register this host
    // has no place for cannot be put back at all.
    xsave_plan(preamble, local_xsave)
        .check()
        .map_err(MigrateError::PreambleMismatch)?;
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
         send migrate-dest with allow_cpu_feature_mismatch to accept \
         the risk",
    )))
}

/// Refuse, before any page moves, state the device state would refuse
/// only after the whole RAM transfer.
fn check_enlightenments(
    preamble: &MigrationPreamble,
    hyperv: bool,
    amd_host: bool,
) -> Result<(), MigrateError> {
    if let Some(source) = preamble.hyperv {
        if source != hyperv {
            return Err(MigrateError::PreambleMismatch(format!(
                "Hyper-V enlightenment: the source has it {}, this VM {}",
                if source { "on" } else { "off" },
                if hyperv { "on" } else { "off" },
            )));
        }
    }
    if preamble.pmu_amd == Some(true) && !amd_host {
        return Err(MigrateError::PreambleMismatch(
            "the source carries AMD PMU state, and this host is not AMD".into(),
        ));
    }
    Ok(())
}

/// The skew between the two hosts' clocks, or `None` from a source too
/// old to send its clock. Early v10 builds sent none.
fn check_clock(
    preamble: &MigrationPreamble,
    protocol: &str,
    log: &slog::Logger,
) -> Result<Option<i64>, MigrateError> {
    match preamble.wall_clock_ns {
        Some(source) => {
            crate::clock::check_skew(source, crate::clock::now_ns()?, log)
                .map(Some)
        }
        None if protocol::at_least(protocol, protocol::PROTOCOL_RON_V12) => {
            Err(MigrateError::PreambleMismatch(
                "the source sent no wall clock".into(),
            ))
        }
        None => {
            slog::warn!(
                log,
                "the source is too old to send its clock; \
                the host clock skew is unknown"
            );
            Ok(None)
        }
    }
}

/// The XSAVE components the guest can use are the ones its source
/// offered it in XCR0.
fn xsave_plan<'a>(
    preamble: &'a MigrationPreamble,
    local: &'a XsaveLayout,
) -> XsavePlan<'a> {
    XsavePlan {
        source: &preamble.xsave,
        local,
        guest: preamble
            .cpu_features
            .as_ref()
            .map_or(0, |f| u64::from(f.xcr0)),
    }
}

/// The device set the source carries must be the set this VM has.
///
/// A destination started with one disk fewer would otherwise resume the
/// guest with that disk at reset and no error anywhere.
fn check_topology<T: std::fmt::Display>(
    source: &[T],
    local: &[T],
) -> Result<(), MigrateError> {
    let named = |set: &[T]| -> Vec<String> {
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
    // These always export a state, and one restored from nothing would
    // leave the guest's driver talking to a device at reset.
    if let Some(entry) = dev_state.emulated.iter().find(|e| {
        e.state.is_none() && protocol::always_exports(expected[&e.bdf])
    }) {
        return Err(MigrateError::State(format!(
            "payload carries no {} state for {}",
            expected[&entry.bdf], entry.bdf,
        )));
    }
    // A device this VM has and the payload does not would resume at
    // reset while the guest driver believes it configured.
    if let Some(bdf) = expected.keys().find(|bdf| !seen.contains_key(bdf)) {
        return Err(MigrateError::State(format!(
            "payload carries no entry for {} ({})",
            bdf, expected[bdf],
        )));
    }

    if dev_state.hyperv.is_none() && config.hyperv {
        return Err(MigrateError::State(
            "payload carries no Hyper-V enlightenment state, and this VM \
             was started with --hyperv"
                .into(),
        ));
    }
    if let Some(hyperv) = &dev_state.hyperv {
        if !(hooks.hyperv)(hyperv) {
            return Err(MigrateError::State(
                "payload carries Hyper-V enlightenment state, and this VM \
                 was not started with --hyperv"
                    .into(),
            ));
        }
    }

    check_platform_payload(&dev_state.platform, &config.platform)?;

    slog::info!(log, "restoring device state";
        "devices" => dev_state.emulated.len(),
        "platform" => dev_state.platform.len());
    for entry in &dev_state.emulated {
        let Some(ref state) = entry.state else {
            continue;
        };
        (hooks.restore)(entry.bdf, state).map_err(|error| {
            MigrateError::State(format!("{}: {error}", entry.bdf))
        })?;
    }
    for entry in &dev_state.platform {
        (hooks.restore_platform)(&entry.id, &entry.state).map_err(|error| {
            MigrateError::State(format!(
                "{}: {error}",
                crate::wire::peer_text(&entry.id)
            ))
        })?;
    }
    Ok(())
}

/// Each local device with no PCI address takes exactly one state, and
/// the payload names no other. A UART left at reset stalls the guest's
/// console and its metadata channel.
fn check_platform_payload(
    payload: &[PlatformStatePayload],
    local: &[PlatformIdentity],
) -> Result<(), MigrateError> {
    let mut seen = std::collections::BTreeSet::new();
    for entry in payload {
        if !local.iter().any(|l| l.id == entry.id) {
            return Err(MigrateError::State(format!(
                "payload carries state for {}, which this VM has no device \
                 under",
                crate::wire::peer_text(&entry.id),
            )));
        }
        if !seen.insert(entry.id.as_str()) {
            return Err(MigrateError::State(format!(
                "payload carries two states for {}",
                entry.id,
            )));
        }
    }
    if let Some(missing) = local.iter().find(|l| !seen.contains(l.id.as_str()))
    {
        return Err(MigrateError::State(format!(
            "payload carries no entry for {missing}"
        )));
    }
    Ok(())
}

/// What ends one RAM pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PassEnd {
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
pub(crate) fn receive_pass<S: Transport>(
    chan: &mut Chan<S>,
    memctx: &MemCtx,
    gate: &dyn StopGate,
    status: &Arc<Mutex<MigrationStatus>>,
    until: PassEnd,
) -> Result<(), MigrateError> {
    let mut sparse: Option<Vec<u64>> = None;
    loop {
        let msg = chan.recv()?;
        not_cancelled(gate)?;
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
                chan.send_uncommitted(gate, &Message::MemDone)?;
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

    fn preamble(wall_clock_ns: Option<u64>) -> MigrationPreamble {
        MigrationPreamble {
            num_cpus: 1,
            mem_size: 4096,
            devices: Vec::new(),
            platform: Vec::new(),
            cpu_features: None,
            xsave: XsaveLayout {
                size: 576,
                components: Vec::new(),
            },
            wall_clock_ns,
            hyperv: None,
            pmu_amd: None,
        }
    }

    #[test]
    fn enlightenments_are_checked_before_any_page() {
        let mut p = preamble(None);
        check_enlightenments(&p, true, false).expect("an old source");
        p.hyperv = Some(true);
        check_enlightenments(&p, true, false).expect("both on");
        check_enlightenments(&p, false, false).expect_err("source only");
        p.hyperv = Some(false);
        check_enlightenments(&p, true, false).expect_err("here only");
        p.pmu_amd = Some(true);
        check_enlightenments(&p, false, true).expect("AMD to AMD");
        let error = check_enlightenments(&p, false, false)
            .expect_err("AMD PMU on Intel");
        assert!(error.to_string().contains("not AMD"), "{error}");
    }

    fn log() -> slog::Logger {
        slog::Logger::root(slog::Discard, slog::o!())
    }

    #[test]
    fn a_current_source_must_send_its_clock() {
        let now = crate::clock::now_ns().expect("clock");
        let skew =
            check_clock(&preamble(Some(now)), protocol::PROTOCOL_RON, &log())
                .expect("synced");
        assert!(skew.is_some_and(|ms| ms.abs() < 1_000), "{skew:?}");

        check_clock(&preamble(None), protocol::PROTOCOL_RON, &log())
            .expect_err("no clock from a v12 source");
        for old in [protocol::PROTOCOL_RON_V10, protocol::PROTOCOL_RON_V9] {
            let skew = check_clock(&preamble(None), old, &log()).expect(old);
            assert_eq!(skew, None, "{old}");
        }

        let hour_behind = now - 3_600 * 1_000_000_000;
        check_clock(
            &preamble(Some(hour_behind)),
            protocol::PROTOCOL_RON,
            &log(),
        )
        .expect_err("an hour of skew");
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

#[cfg(test)]
mod pass_tests {
    use super::*;
    use crate::codec::PAGE_BATCH_FLAG_ZSTD;
    use crate::protocol::PAGE_SIZE;
    use crate::wire::Deadlines;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    use vmm_core::mem::PhysMap;

    const RAM: usize = 64 * 1024;

    fn memctx() -> MemCtx {
        MemCtx::new(Arc::new(
            PhysMap::new_anon(0, RAM).expect("anonymous guest memory"),
        ))
    }

    fn status() -> Arc<Mutex<MigrationStatus>> {
        Arc::new(Mutex::new(MigrationStatus::default()))
    }

    /// A connected pair of channels, source end first.
    fn pair(deadlines: Deadlines) -> (Chan<UnixStream>, Chan<UnixStream>) {
        let (a, b) = UnixStream::pair().expect("socket pair");
        (
            Chan::new(a, tungstenite::protocol::Role::Client, deadlines)
                .expect("source channel"),
            Chan::new(b, tungstenite::protocol::Role::Server, deadlines)
                .expect("destination channel"),
        )
    }

    fn page(fill: u8) -> Vec<u8> {
        vec![fill; PAGE_SIZE]
    }

    fn read_page(memctx: &MemCtx, gpa: u64) -> Vec<u8> {
        let mut out = vec![0u8; PAGE_SIZE];
        crate::pages::read_pages(memctx, &[gpa], &mut out).expect("read back");
        out
    }

    #[test]
    fn a_whole_pass_lands_every_page_it_carried() {
        // The phase machine had no end-to-end test at all, so a
        // destination that dropped a sparse batch looked correct.
        let (mut source, mut dest) = pair(Deadlines::default());
        let sender = std::thread::spawn(move || {
            // Contiguous.
            source
                .send(&Message::PageBatch {
                    base_gpa: 0,
                    page_count: 2,
                    flags: 0,
                    data: [page(0xAA), page(0xBB)].concat(),
                })
                .expect("contiguous batch");
            // Sparse: the GPA list is the only thing that says where
            // these pages go.
            let gpas = vec![0x8000u64, 0xC000];
            source.send(&Message::MemFetch(gpas)).expect("fetch");
            source
                .send(&Message::PageBatch {
                    base_gpa: 0x8000,
                    page_count: 2,
                    flags: 0,
                    data: [page(0xCC), page(0xDD)].concat(),
                })
                .expect("sparse batch");
            source.send(&Message::MemEnd).expect("end of pass");
            source.expect_mem_done().expect("the pass is acknowledged");
            source.send(&Message::PauseSignal).expect("pause");
            source
        });
        let memctx = memctx();
        receive_pass(
            &mut dest,
            &memctx,
            &crate::NoStop,
            &status(),
            PassEnd::PauseSignal,
        )
        .expect("a well-formed pass");
        sender.join().expect("the sender finished");

        assert_eq!(read_page(&memctx, 0), page(0xAA));
        assert_eq!(read_page(&memctx, PAGE_SIZE as u64), page(0xBB));
        assert_eq!(read_page(&memctx, 0x8000), page(0xCC));
        assert_eq!(read_page(&memctx, 0xC000), page(0xDD));
    }

    #[test]
    fn a_compressed_batch_lands_the_same_bytes() {
        let (mut source, mut dest) = pair(Deadlines::default());
        let raw = [page(0x11), page(0x22)].concat();
        let data = zstd::bulk::compress(&raw, 1).expect("compress");
        let sender = std::thread::spawn(move || {
            source
                .send(&Message::PageBatch {
                    base_gpa: 0,
                    page_count: 2,
                    flags: PAGE_BATCH_FLAG_ZSTD,
                    data,
                })
                .expect("batch");
            source.send(&Message::MemEnd).expect("end");
            source
        });
        let memctx = memctx();
        receive_pass(
            &mut dest,
            &memctx,
            &crate::NoStop,
            &status(),
            PassEnd::MemEnd,
        )
        .expect("a compressed pass");
        sender.join().expect("the sender finished");
        assert_eq!(read_page(&memctx, 0), page(0x11));
    }

    #[test]
    fn anything_but_a_batch_after_a_fetch_is_refused() {
        // A MemFetch commits the peer to exactly one PageBatch. Taking
        // anything else silently discards the pages those GPAs stand
        // for.
        let (mut source, mut dest) = pair(Deadlines::default());
        let sender = std::thread::spawn(move || {
            source
                .send(&Message::MemFetch(vec![0x8000]))
                .expect("fetch");
            source.send(&Message::MemEnd).expect("not a batch");
            source
        });
        let error = receive_pass(
            &mut dest,
            &memctx(),
            &crate::NoStop,
            &status(),
            PassEnd::MemEnd,
        )
        .expect_err("a fetch without its batch");
        assert!(
            error.to_string().contains("PageBatch following MemFetch"),
            "{error}",
        );
        sender.join().expect("the sender finished");
    }

    #[test]
    fn a_pause_signal_does_not_end_the_final_pass() {
        // The final pass ends at MemEnd. A peer that sends PauseSignal
        // there is out of step, and taking it would leave the guest
        // short of its last dirty pages.
        let (mut source, mut dest) = pair(Deadlines::default());
        let sender = std::thread::spawn(move || {
            source.send(&Message::PauseSignal).expect("pause");
            source
        });
        receive_pass(
            &mut dest,
            &memctx(),
            &crate::NoStop,
            &status(),
            PassEnd::MemEnd,
        )
        .expect_err("a pause in the final pass");
        sender.join().expect("the sender finished");
    }

    #[test]
    fn a_silent_peer_does_not_hold_the_destination_for_ever() {
        // After the pause the guest is stopped on both hosts, so every
        // second of waiting is guest downtime.
        let deadlines = Deadlines {
            pre_pause: Duration::from_millis(200),
            post_pause: Duration::from_millis(200),
            ..Deadlines::default()
        };
        let (source, mut dest) = pair(deadlines);
        let started = std::time::Instant::now();
        receive_pass(
            &mut dest,
            &memctx(),
            &crate::NoStop,
            &status(),
            PassEnd::MemEnd,
        )
        .expect_err("a peer that says nothing");
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(source);
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
            self.commits
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            Ok(())
        }

        fn on_stop(&self, _shut: Box<dyn FnOnce() + Send>) {}
    }

    /// Hooks whose resumes may not run.
    fn hooks(gate: Gate) -> DestHooks {
        DestHooks {
            restore: Box::new(|_, _| Ok(())),
            restore_platform: Box::new(|_, _| Ok(())),
            attach: Box::new(|| panic!("disks attached")),
            resume: Box::new(|| panic!("devices resumed")),
            hyperv: Box::new(|_| false),
            pause_vm: Box::new(|| Ok(())),
            resume_vm: Box::new(|| panic!("vCPUs resumed")),
            gate: Box::new(gate),
        }
    }

    fn discard() -> slog::Logger {
        slog::Logger::root(slog::Discard, slog::o!())
    }

    #[test]
    fn a_cancelled_destination_never_says_it_holds_everything() {
        // Its Okay lets the source commit, and a stopped destination
        // would leave the guest running nowhere.
        let (mut source, mut dest) = pair(Deadlines::default());
        let gate = Gate {
            cancel: true,
            ..Gate::default()
        };

        let error = take_over(&mut dest, &hooks(gate), &status(), &discard())
            .expect_err("cancelled");

        assert!(crate::is_cancel(&error), "{error}");
        drop(dest);
        assert!(source.recv().is_err(), "the source heard an Okay");
    }

    #[test]
    fn a_destination_resumes_nothing_the_source_did_not_commit() {
        let (mut source, mut dest) = pair(Deadlines::default());
        let sender = std::thread::spawn(move || {
            assert!(matches!(source.recv(), Ok(Message::Okay)));
            source
                .send(&Message::Error(MigrateError::Phase(
                    "rolled back".into(),
                )))
                .expect("abort");
            source
        });

        take_over(&mut dest, &hooks(Gate::default()), &status(), &discard())
            .expect_err("the source rolled back");
        sender.join().expect("the source finished");
    }

    /// Hooks that log each step, with an attach that answers `attached`.
    fn logging(
        attached: Result<(), String>,
    ) -> (DestHooks, Arc<Mutex<Vec<&'static str>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (a, r, v) = (Arc::clone(&log), Arc::clone(&log), Arc::clone(&log));
        let hooks = DestHooks {
            attach: Box::new(move || {
                a.lock().expect("log").push("attach");
                attached.clone()
            }),
            resume: Box::new(move || r.lock().expect("log").push("devices")),
            resume_vm: Box::new(move || {
                v.lock().expect("log").push("vcpus");
                Ok(())
            }),
            ..hooks(Gate::default())
        };
        (hooks, log)
    }

    /// A source that says it holds everything and commits.
    fn committing(
        mut source: Chan<UnixStream>,
    ) -> std::thread::JoinHandle<Chan<UnixStream>> {
        std::thread::spawn(move || {
            assert!(matches!(source.recv(), Ok(Message::Okay)));
            source.send(&Message::Okay).expect("commit");
            source
        })
    }

    #[test]
    fn the_disks_attach_after_the_commit_and_before_any_resume() {
        let (source, mut dest) = pair(Deadlines::default());
        let sender = committing(source);
        let (hooks, log) = logging(Ok(()));
        let status = status();
        take_over(&mut dest, &hooks, &status, &discard()).expect("taken over");
        assert_eq!(*log.lock().expect("log"), ["attach", "devices", "vcpus"]);
        let mut source = sender.join().expect("the source finished");
        assert!(matches!(source.recv(), Ok(Message::Okay)), "no started");
    }

    #[test]
    fn a_disk_that_does_not_attach_keeps_the_guest_paused() {
        let (source, mut dest) = pair(Deadlines::default());
        let sender = committing(source);
        let (hooks, log) = logging(Err("no engine".into()));
        let status = status();
        let error = take_over(&mut dest, &hooks, &status, &discard())
            .expect_err("no attach");
        assert!(error.to_string().contains("no engine"), "{error}");
        assert_eq!(*log.lock().expect("log"), ["attach"]);
        // The source committed: this side holds the guest, and says so.
        assert_eq!(
            status.lock().expect("status").phase,
            MigrationPhase::Committed
        );
        drop(dest);
        let mut source = sender.join().expect("the source finished");
        assert!(source.recv().is_err(), "the source heard a start");
    }
}

#[cfg(test)]
mod platform_tests;
