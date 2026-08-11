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

/// Indicator for tracking [`Lifecycle`] states.
///
/// Provides atomic state tracking with transition validation.
/// Invalid transitions cause a panic in debug builds and are logged
/// (but not fatal) in release builds, since a state machine violation
/// in a VMM is a serious bug that should be caught during development.
#[derive(Default)]
pub struct Indicator(AtomicUsize);

impl Indicator {
    pub const fn new() -> Self {
        Self(AtomicUsize::new(IndicatedState::Init as usize))
    }

    pub fn start(&self) {
        self.transition(IndicatedState::Run);
    }

    pub fn pause(&self) {
        self.transition(IndicatedState::Pause);
    }

    pub fn resume(&self) {
        self.transition(IndicatedState::Run);
    }

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
    fn transition(&self, new: IndicatedState) {
        loop {
            let old_raw = self.0.load(Ordering::Acquire);
            let old = IndicatedState::from_usize(old_raw);

            assert!(
                IndicatedState::valid_transition(old, new),
                "invalid lifecycle transition: {old:?} -> {new:?}",
            );

            match self.0.compare_exchange_weak(
                old_raw,
                new as usize,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
