// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Device lifecycle trait.
//!
//! Adopted from propolis — defines the contract for device state
//! transitions during the VM lifecycle.
//!
//! # State machine
//!
//! ```text
//!   Init ──→ Run ──→ Pause ──→ Run   (normal pause/resume)
//!                       │
//!                       └──→ Halt     (shutdown)
//!
//!   Init ──→ Pause                    (migration target: import before start)
//! ```
//!
//! Note: `Init -> Run` and `Init -> Pause` are both valid. The latter
//! is used when a device is created on a migration target and must
//! import state before starting.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::migrate::Migrator;

/// Durability required from a device backing-file flush.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushIntent {
    /// Migration: the fsync barrier MUST cover every write the guest saw
    /// completed. Failing loudly beats shipping an unsynced disk.
    Durable,
    /// Teardown: dropping unflushed writes is power-loss equivalent and
    /// within the volatile-write-cache contract we advertise (VWC=1 at
    /// nvme/bits.rs:253, VIRTIO_BLK_F_FLUSH at virtio/block.rs:326).
    /// C bhyve does not flush here at all (vmexit.c:536-546 calls exit(0)).
    BestEffort,
}

/// Failure to quiesce or sync a device backing file.
#[derive(Debug)]
pub enum FlushError {
    /// Device workers did not quiesce before the intent-specific deadline.
    NotQuiesced(&'static str),
    /// The backing file could not be cloned or synced.
    Sync(std::io::Error),
    /// A best-effort sync outlived its teardown watchdog.
    TimedOut,
}

/// Virtio PCI transport state for migration export/import.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VirtioMigratePciState {
    /// PCI command register bits that gate BAR decoding and bus mastering.
    pub command: u16,
    /// BAR base addresses indexed by BAR number (BAR0..BAR5).
    pub bar_addrs: [u64; 6],
}

/// Virtio queue state for migration export/import.
#[derive(Debug, Clone, Default)]
pub struct VirtioMigrateQueue {
    pub queue_idx: u16,
    pub queue_size: u16,
    pub desc_addr: u64,
    pub avail_addr: u64,
    pub used_addr: u64,
    pub enabled: bool,
    pub features: u64,
    pub avail_idx: u16,
    pub used_idx: u16,
    pub msix_addr: u64,
    pub msix_data: u32,
    /// MSI-X vector number for this queue (VIRTIO_MSI_NO_VECTOR = 0xFFFF if none).
    pub msix_vector: u16,
    pub pci_state: Option<VirtioMigratePciState>,
}

/// NVMe controller state for migration export/import.
#[derive(Debug, Clone, Default)]
pub struct NvmeMigrateState {
    pub cc: u32,
    pub csts: u32,
    pub aqa: u32,
    pub asq_base: u64,
    pub acq_base: u64,
    pub admin_sq: Option<NvmeMigrateSq>,
    pub admin_cq: Option<NvmeMigrateCq>,
    pub io_sqs: Vec<Option<NvmeMigrateSq>>,
    pub io_cqs: Vec<Option<NvmeMigrateCq>>,
    pub num_io_queues: u16,
    pub pci_command: u16,
    pub pci_bar_addrs: [u64; 6],
    pub msix_entries: Vec<(u64, u64)>,
    pub msix_enabled: bool,
}

/// NVMe submission queue state for migration.
#[derive(Debug, Clone)]
pub struct NvmeMigrateSq {
    pub base: u64,
    pub size: u16,
    pub head: u16,
    pub tail: u16,
    pub cq_id: u16,
}

/// NVMe completion queue state for migration.
#[derive(Debug, Clone)]
pub struct NvmeMigrateCq {
    pub base: u64,
    pub size: u16,
    pub head: u16,
    pub tail: u16,
    pub phase: bool,
    pub iv: u16,
}

/// General trait for emulated devices in the system.
///
/// As the VM goes through its lifecycle, the emulated devices which it
/// contains are driven through those phases via various event functions
/// (`start`, `pause`, `resume`, etc).
///
/// Default implementations are provided for most methods. Devices that
/// hold no internal state can rely on the defaults. Devices with
/// background tasks or internal state **must** implement at least
/// `pause` and `is_quiesced` to ensure clean quiescence.
pub trait Lifecycle: Send + Sync + 'static {
    /// Unique name for devices of a given type.
    fn type_name(&self) -> &'static str;

    /// Return the device's tracked lifecycle state, if it has one.
    ///
    /// Stateless devices may use the default. Devices backed by an
    /// [`Indicator`] should expose its state so lifecycle coordinators can
    /// avoid requesting a transition that has already completed.
    fn lifecycle_state(&self) -> Option<IndicatedState> {
        None
    }

    /// Returns true once the device's background work has drained.
    ///
    /// MUST NOT BLOCK. Callers poll this against a deadline; a device that
    /// waits inside this method makes that deadline unenforceable. Implement
    /// it as an atomic load. If you need to join a thread, do it in `halt()`.
    fn is_quiesced(&self) -> bool {
        true
    }

    /// Called when an instance is about to begin running, just before any
    /// vCPU threads are started.
    fn start(&self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Directs this device to pause. A paused device must stop producing
    /// work but must accept and queue new work from other devices.
    fn pause(&self) {}

    /// Directs this device to resume servicing the guest after pausing.
    fn resume(&self) {}

    /// Directs this device to reset itself to cold-start state.
    /// Only called on paused devices.
    fn reset(&self) {}

    /// Indicates that the device's instance is stopping.
    /// Only called on paused devices.
    fn halt(&self) {}

    /// Return the Migrator for exporting/importing this device's state.
    fn migrate(&'_ self) -> Migrator<'_> {
        Migrator::Empty
    }

    /// Pause device rings before vCPU pause for migration.
    /// Kernel-accelerated devices (viona) pause their ring threads
    /// so no entries are consumed between this call and state export.
    fn pause_for_migration(&self) {}

    /// Flush any device-level writeback cache to durable storage.
    ///
    /// Called with [`FlushIntent::Durable`] during migration pause AFTER
    /// `hdl.pause()` has quiesced vCPUs and BEFORE device state is exported /
    /// the ZFS barrier is hit. Teardown uses [`FlushIntent::BestEffort`].
    /// Default: no-op.
    ///
    /// With zvol `DKIOCSETWCE=1` (enabled by `blkdev::DiskCache`), writes
    /// return OK from the zvol cache without hitting the ZFS pool. A
    /// `zfs send` incremental only captures pool state, so any buffered
    /// writes not fsynced before the send would be lost on the destination.
    ///
    /// Implementors must also ensure in-flight worker writes complete
    /// before syncing so the fsync barrier covers the same set of writes
    /// the guest saw completions for.
    fn flush_backing(&self, _intent: FlushIntent) -> Result<(), FlushError> {
        Ok(())
    }

    /// Export virtio queue state for migration.
    fn export_virtio_state(&self) -> Vec<VirtioMigrateQueue> {
        Vec::new()
    }

    /// Restore virtio queue state after migration import.
    fn restore_virtio_state(&self, _states: &[VirtioMigrateQueue]) {}

    /// Post-restore kick: called after restore_virtio_state to wake
    /// rings and start interrupt polling. Default: no-op.
    fn post_restore_kick(&self) {}

    /// Export NVMe controller state for migration.
    fn export_nvme_state(&self) -> Option<NvmeMigrateState> {
        None
    }

    /// Restore NVMe controller state after migration import.
    fn restore_nvme_state(&self, _state: &NvmeMigrateState) {}
}

/// A transition the [`IndicatedState`] machine does not allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidTransition {
    pub from: IndicatedState,
    pub to: IndicatedState,
}

/// Indicator for tracking [`Lifecycle`] states.
///
/// Provides atomic state tracking with transition validation.
///
/// Two families of mutators share one validated core:
///
/// * [`Indicator::start`], [`Indicator::pause`], [`Indicator::resume`] and
///   [`Indicator::halt`] name a wanted end state. They never panic and
///   never block. A device already in that state is a satisfied request,
///   because two threads may ask one device to pause and both are right.
///   A request the machine does not allow leaves the state alone; the
///   caller reads [`Indicator::state`] when the outcome matters.
/// * [`Indicator::request`] is the same thing with the outcome returned,
///   for a caller that must know whether the edge was taken.
/// * [`Indicator::try_transition`] enforces the strict machine: a
///   repeated request is an error there, not a satisfied one. Use it
///   where the caller owns both the device and the current state.
///
/// None of them panic. The release profile sets `panic = "abort"`, so a
/// panic here kills every guest on the host, and a lifecycle request
/// that races another thread is ordinary concurrency, not a bug.
#[derive(Default)]
pub struct Indicator(AtomicUsize);

impl Indicator {
    pub const fn new() -> Self {
        Self(AtomicUsize::new(IndicatedState::Init as usize))
    }

    /// Ask for `Run` from `Init` or `Pause`. A no-op when running.
    pub fn start(&self) {
        self.request_or_leave(IndicatedState::Run);
    }

    /// Ask for `Pause` from `Init` or `Run`. A no-op when paused.
    pub fn pause(&self) {
        self.request_or_leave(IndicatedState::Pause);
    }

    /// Ask for `Run` from `Pause`. A no-op when running.
    pub fn resume(&self) {
        self.request_or_leave(IndicatedState::Run);
    }

    /// Ask for `Halt` from `Pause`. A no-op when halted.
    pub fn halt(&self) {
        self.request_or_leave(IndicatedState::Halt);
    }

    /// Ask for `want` and drop a refusal.
    ///
    /// The refusal is not lost: a refused request does not move the
    /// state, so [`Indicator::state`] still reports the truth. This
    /// shape exists because `Lifecycle::pause`, `resume` and `halt`
    /// return `()`. A caller that must act on the outcome calls
    /// [`Indicator::request`].
    fn request_or_leave(&self, want: IndicatedState) {
        let _refused = self.request(want);
    }

    /// Drive the device toward `want` and say what happened.
    ///
    /// `Ok(true)` means this call took the edge. `Ok(false)` means the
    /// device was in `want` already, so the request is met and the
    /// caller must not undo a transition it did not make. `Err` means
    /// the machine has no edge from the current state to `want`, and
    /// the state is untouched.
    pub fn request(
        &self,
        want: IndicatedState,
    ) -> Result<bool, InvalidTransition> {
        match self.try_transition(want) {
            Ok(()) => Ok(true),
            // Reread rather than trusting the reported `from`: another
            // thread may have reached `want` since the failed compare.
            Err(refused) if self.state() == want => {
                let _lost_race = refused;
                Ok(false)
            }
            Err(refused) => Err(refused),
        }
    }

    pub fn state(&self) -> IndicatedState {
        IndicatedState::from_usize(self.0.load(Ordering::Acquire))
    }

    /// Attempt a state transition, validating before mutating.
    ///
    /// Uses compare_exchange to ensure atomicity: the old state is
    /// validated before the new state is stored, preventing the
    /// swap-then-validate race in the original implementation.
    pub fn try_transition(
        &self,
        new: IndicatedState,
    ) -> Result<(), InvalidTransition> {
        loop {
            let old_raw = self.0.load(Ordering::Acquire);
            let old = IndicatedState::from_usize(old_raw);

            if !IndicatedState::valid_transition(old, new) {
                return Err(InvalidTransition { from: old, to: new });
            }

            match self.0.compare_exchange_weak(
                old_raw,
                new as usize,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                // Another thread changed the state between our load and
                // CAS — retry with the new value.
                Err(_) => continue,
            }
        }
    }
}

/// Current state held in [`Indicator`].
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
#[repr(usize)]
pub enum IndicatedState {
    /// Created but not yet started.
    Init = 0,
    /// Running.
    Run = 1,
    /// Paused (quiesced).
    Pause = 2,
    /// Halted (terminal state).
    Halt = 3,
}

impl IndicatedState {
    /// Valid transitions:
    /// - Init -> Run    (normal startup)
    /// - Init -> Pause  (migration target: import state before starting)
    /// - Run  -> Pause  (pause for migration or reset)
    /// - Pause -> Run   (resume after pause)
    /// - Pause -> Halt  (shutdown from paused state)
    const fn valid_transition(old: Self, new: Self) -> bool {
        matches!(
            (old, new),
            (Self::Init, Self::Run)
                | (Self::Init, Self::Pause)
                | (Self::Run, Self::Pause)
                | (Self::Pause, Self::Run)
                | (Self::Pause, Self::Halt)
        )
    }

    fn from_usize(raw: usize) -> Self {
        match raw {
            0 => Self::Init,
            1 => Self::Run,
            2 => Self::Pause,
            3 => Self::Halt,
            _ => panic!(
                "corrupted IndicatedState value: {raw} \
                 (expected 0..=3, possible memory corruption)"
            ),
        }
    }
}

/// Poll interval while an unplug waits for a device to go quiet.
const UNPLUG_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Failure to make a device safe to remove.
///
/// No variant halts the device, so the caller can retry
/// [`prepare_unplug`] or leave the device attached.
#[derive(Debug)]
pub enum UnplugError {
    /// The device still had work in flight when the budget expired.
    NotQuiesced { device: &'static str },
    /// The device could not flush its backing store.
    Flush {
        device: &'static str,
        source: FlushError,
    },
    /// The device left `Pause` under the unplug, so the halt was refused.
    ///
    /// An operator `resume` that lands between the quiesce wait and the
    /// halt puts the device's workers back to work. Halting it there
    /// would stop a device with live DMA, so the unplug gives up.
    Refused {
        device: &'static str,
        state: IndicatedState,
    },
}

/// Make a device safe to detach at runtime, then halt it.
///
/// Pauses the device, waits up to `budget` for its background work to
/// drain, flushes the backing store best effort, and halts. The halt only
/// happens after the device reports quiescence, so an unplug never tears
/// in-flight DMA out from under the guest. There is no force path.
///
/// The pause is conditional so a device that is paused already does not
/// run its worker-stopping code twice. A device in `Init` is expected:
/// nothing in the tree calls [`Lifecycle::start`], so an Indicator backed
/// device stays in `Init` for the life of the VM. `Init -> Pause` is legal.
///
/// The halt is checked, not assumed. An operator `resume` can land
/// between the quiesce wait and the halt, which puts the device's
/// workers back to work; [`Indicator`] then refuses `Run -> Halt` and
/// leaves the device running. Detaching its BARs there would strip a
/// live device, so the unplug reports [`UnplugError::Refused`] instead.
pub fn prepare_unplug(
    dev: &Arc<dyn Lifecycle>,
    budget: Duration,
) -> Result<(), UnplugError> {
    let device = dev.type_name();
    let state = dev.lifecycle_state();

    // A halted device gave up its resources already.
    if state == Some(IndicatedState::Halt) {
        return Ok(());
    }

    if state != Some(IndicatedState::Pause) {
        dev.pause();
    }

    if !wait_device_quiesced(dev.as_ref(), budget) {
        return Err(UnplugError::NotQuiesced { device });
    }

    dev.flush_backing(FlushIntent::BestEffort)
        .map_err(|source| UnplugError::Flush { device, source })?;

    dev.halt();
    match dev.lifecycle_state() {
        // A device with no Indicator cannot report, so its halt stands.
        None | Some(IndicatedState::Halt) => Ok(()),
        Some(state) => Err(UnplugError::Refused { device, state }),
    }
}

/// Poll `is_quiesced` against a deadline.
///
/// The poll must not block: [`Lifecycle::is_quiesced`] is an atomic load, and
/// this loop sleeps between loads so the budget stays enforceable.
fn wait_device_quiesced(dev: &dyn Lifecycle, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if dev.is_quiesced() {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        thread::sleep(UNPLUG_POLL_INTERVAL.min(deadline - now));
    }
}

#[cfg(test)]
mod tests;
