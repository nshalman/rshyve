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
///   [`Indicator::halt`] panic on an illegal edge. Use them where the
///   caller owns the device and the current state, such as construction,
///   migration import and the ordered teardown path. There a bad edge is a
///   programming error and must not stay silent.
/// * [`Indicator::try_transition`] returns [`InvalidTransition`] instead.
///   Use it where the state is decided by the guest or the operator, such
///   as a runtime unplug. There a bad edge must not kill the VMM.
#[derive(Default)]
pub struct Indicator(AtomicUsize);

impl Indicator {
    pub const fn new() -> Self {
        Self(AtomicUsize::new(IndicatedState::Init as usize))
    }

    /// Panics unless the device is in `Init` or `Pause`.
    pub fn start(&self) {
        self.transition(IndicatedState::Run);
    }

    /// Panics unless the device is in `Init` or `Run`.
    pub fn pause(&self) {
        self.transition(IndicatedState::Pause);
    }

    /// Panics unless the device is in `Pause`.
    pub fn resume(&self) {
        self.transition(IndicatedState::Run);
    }

    /// Panics unless the device is in `Pause`.
    pub fn halt(&self) {
        self.transition(IndicatedState::Halt);
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

    fn transition(&self, new: IndicatedState) {
        if let Err(InvalidTransition { from, to }) = self.try_transition(new) {
            panic!("invalid lifecycle transition: {from:?} -> {to:?}");
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
/// Both variants leave the device paused and NOT halted, so the caller can
/// retry [`prepare_unplug`] or leave the device attached.
#[derive(Debug)]
pub enum UnplugError {
    /// The device still had work in flight when the budget expired.
    NotQuiesced { device: &'static str },
    /// The device could not flush its backing store.
    Flush {
        device: &'static str,
        source: FlushError,
    },
}

/// Make a device safe to detach at runtime, then halt it.
///
/// Pauses the device, waits up to `budget` for its background work to
/// drain, flushes the backing store best effort, and halts. The halt only
/// happens after the device reports quiescence, so an unplug never tears
/// in-flight DMA out from under the guest. There is no force path.
///
/// The pause is conditional because `Pause -> Pause` is an illegal edge and
/// panics an [`Indicator`] backed device. A device in `Init` is expected:
/// nothing in the tree calls [`Lifecycle::start`], so an Indicator backed
/// device stays in `Init` for the life of the VM. `Init -> Pause` is legal.
pub fn prepare_unplug(
    dev: &Arc<dyn Lifecycle>,
    budget: Duration,
) -> Result<(), UnplugError> {
    let device = dev.type_name();
    let state = dev.lifecycle_state();

    // A halted device gave up its resources already. Repeating the pause or
    // the halt hits an illegal edge, so report success instead.
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
    Ok(())
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
mod tests {
    use super::*;

    use std::sync::atomic::AtomicBool;

    #[test]
    fn valid_transitions() {
        let ind = Indicator::new();
        assert_eq!(ind.state(), IndicatedState::Init);

        ind.start();
        assert_eq!(ind.state(), IndicatedState::Run);

        ind.pause();
        assert_eq!(ind.state(), IndicatedState::Pause);

        ind.resume();
        assert_eq!(ind.state(), IndicatedState::Run);

        ind.pause();
        ind.halt();
        assert_eq!(ind.state(), IndicatedState::Halt);
    }

    #[test]
    fn init_to_pause_for_migration() {
        let ind = Indicator::new();
        ind.pause(); // Init -> Pause is valid (migration target)
        assert_eq!(ind.state(), IndicatedState::Pause);
    }

    #[test]
    #[should_panic(expected = "invalid lifecycle transition")]
    fn invalid_init_to_halt() {
        let ind = Indicator::new();
        ind.halt(); // Init -> Halt is not valid
    }

    #[test]
    #[should_panic(expected = "invalid lifecycle transition")]
    fn invalid_run_to_halt() {
        let ind = Indicator::new();
        ind.start();
        ind.halt(); // Run -> Halt is not valid (must pause first)
    }

    const ALL_STATES: [IndicatedState; 4] = [
        IndicatedState::Init,
        IndicatedState::Run,
        IndicatedState::Pause,
        IndicatedState::Halt,
    ];

    fn indicator_in(state: IndicatedState) -> Indicator {
        let ind = Indicator::new();
        match state {
            IndicatedState::Init => {}
            IndicatedState::Run => ind.start(),
            IndicatedState::Pause => ind.pause(),
            IndicatedState::Halt => {
                ind.pause();
                ind.halt();
            }
        }
        assert_eq!(ind.state(), state);
        ind
    }

    #[test]
    fn try_transition_rejects_every_illegal_edge() {
        for from in ALL_STATES {
            for to in ALL_STATES {
                let ind = indicator_in(from);
                let result = ind.try_transition(to);

                if IndicatedState::valid_transition(from, to) {
                    assert!(result.is_ok(), "{from:?} -> {to:?}");
                    assert_eq!(ind.state(), to);
                } else {
                    assert_eq!(
                        result.expect_err("illegal edge must not be taken"),
                        InvalidTransition { from, to },
                    );
                    assert_eq!(ind.state(), from, "{from:?} -> {to:?}");
                }
            }
        }
    }

    #[test]
    fn try_transition_reports_run_to_halt_without_panicking() {
        let ind = indicator_in(IndicatedState::Run);

        let error = ind
            .try_transition(IndicatedState::Halt)
            .expect_err("Run -> Halt is not a legal edge");

        assert_eq!(error.from, IndicatedState::Run);
        assert_eq!(error.to, IndicatedState::Halt);
        assert_eq!(ind.state(), IndicatedState::Run);
    }

    struct UnplugTestDevice {
        indicator: Indicator,
        quiesced: AtomicBool,
        pauses: AtomicUsize,
        halts: AtomicUsize,
        flushes: AtomicUsize,
        flush_fails: bool,
    }

    impl UnplugTestDevice {
        fn new(quiesced: bool) -> Self {
            Self {
                indicator: Indicator::new(),
                quiesced: AtomicBool::new(quiesced),
                pauses: AtomicUsize::new(0),
                halts: AtomicUsize::new(0),
                flushes: AtomicUsize::new(0),
                flush_fails: false,
            }
        }

        fn count(counter: &AtomicUsize) -> usize {
            counter.load(Ordering::Acquire)
        }
    }

    impl Lifecycle for UnplugTestDevice {
        fn type_name(&self) -> &'static str {
            "unplug-test"
        }

        fn lifecycle_state(&self) -> Option<IndicatedState> {
            Some(self.indicator.state())
        }

        fn is_quiesced(&self) -> bool {
            self.quiesced.load(Ordering::Acquire)
        }

        fn start(&self) -> anyhow::Result<()> {
            self.indicator.start();
            Ok(())
        }

        fn pause(&self) {
            self.pauses.fetch_add(1, Ordering::AcqRel);
            self.indicator.pause();
        }

        fn halt(&self) {
            self.halts.fetch_add(1, Ordering::AcqRel);
            self.indicator.halt();
        }

        fn flush_backing(&self, intent: FlushIntent) -> Result<(), FlushError> {
            assert_eq!(intent, FlushIntent::BestEffort);
            self.flushes.fetch_add(1, Ordering::AcqRel);
            if self.flush_fails {
                return Err(FlushError::NotQuiesced(self.type_name()));
            }
            Ok(())
        }
    }

    const UNPLUG_BUDGET: Duration = Duration::from_millis(50);

    #[test]
    fn unplug_halts_a_running_device_that_quiesces() {
        let device = Arc::new(UnplugTestDevice::new(true));
        device.start().expect("start unplug test device");
        let dev: Arc<dyn Lifecycle> = device.clone();

        prepare_unplug(&dev, UNPLUG_BUDGET).expect("unplug quiesced device");

        assert_eq!(UnplugTestDevice::count(&device.pauses), 1);
        assert_eq!(UnplugTestDevice::count(&device.flushes), 1);
        assert_eq!(UnplugTestDevice::count(&device.halts), 1);
        assert_eq!(device.indicator.state(), IndicatedState::Halt);
    }

    #[test]
    fn unplug_handles_a_device_that_never_started() {
        let device = Arc::new(UnplugTestDevice::new(true));
        assert_eq!(device.indicator.state(), IndicatedState::Init);
        let dev: Arc<dyn Lifecycle> = device.clone();

        prepare_unplug(&dev, UNPLUG_BUDGET).expect("unplug device in Init");

        assert_eq!(UnplugTestDevice::count(&device.pauses), 1);
        assert_eq!(device.indicator.state(), IndicatedState::Halt);
    }

    #[test]
    fn unplug_does_not_pause_an_already_paused_device() {
        let device = Arc::new(UnplugTestDevice::new(true));
        device.pause();
        let dev: Arc<dyn Lifecycle> = device.clone();

        prepare_unplug(&dev, UNPLUG_BUDGET).expect("unplug paused device");

        assert_eq!(UnplugTestDevice::count(&device.pauses), 1);
        assert_eq!(device.indicator.state(), IndicatedState::Halt);
    }

    #[test]
    fn unplug_leaves_a_stuck_device_running() {
        let device = Arc::new(UnplugTestDevice::new(false));
        device.start().expect("start unplug test device");
        let dev: Arc<dyn Lifecycle> = device.clone();
        let started = Instant::now();

        let error = prepare_unplug(&dev, UNPLUG_BUDGET)
            .expect_err("a device with in-flight work must not be halted");

        assert!(matches!(
            error,
            UnplugError::NotQuiesced {
                device: "unplug-test"
            }
        ));
        assert!(started.elapsed() < UNPLUG_BUDGET * 4);
        assert_eq!(UnplugTestDevice::count(&device.flushes), 0);
        assert_eq!(UnplugTestDevice::count(&device.halts), 0);
        assert_eq!(device.indicator.state(), IndicatedState::Pause);
    }

    #[test]
    fn unplug_does_not_halt_a_device_whose_flush_failed() {
        let mut device = UnplugTestDevice::new(true);
        device.flush_fails = true;
        let device = Arc::new(device);
        let dev: Arc<dyn Lifecycle> = device.clone();

        let error = prepare_unplug(&dev, UNPLUG_BUDGET)
            .expect_err("a failed flush must not halt the device");

        assert!(matches!(error, UnplugError::Flush { .. }));
        assert_eq!(UnplugTestDevice::count(&device.halts), 0);
        assert_eq!(device.indicator.state(), IndicatedState::Pause);
    }

    #[test]
    fn unplug_of_a_halted_device_is_idempotent() {
        let device = Arc::new(UnplugTestDevice::new(true));
        let dev: Arc<dyn Lifecycle> = device.clone();
        prepare_unplug(&dev, UNPLUG_BUDGET).expect("first unplug");

        prepare_unplug(&dev, UNPLUG_BUDGET).expect("second unplug");

        assert_eq!(UnplugTestDevice::count(&device.halts), 1);
        assert_eq!(UnplugTestDevice::count(&device.flushes), 1);
    }
}
