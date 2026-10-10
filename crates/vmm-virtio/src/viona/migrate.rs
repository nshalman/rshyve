// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The ring state a migration reads out and puts back.
//!
//! The source pauses each ring and reads its indices; the destination
//! programs the addresses and the indices together, which
//! [`super::VirtioViona::ring_init`] cannot do because it resets the
//! indices to 0.

use vmm_devices::lifecycle::DeviceStateError;

use vmm_core::rings::RingGate;

use super::gate::{stop_one, StopOne};
use super::{RingState, VionaInner, VirtioViona, NET_NUM_QUEUES};

impl VirtioViona {
    /// Check that the export reads rings that hold still.
    ///
    /// The VM pause stops every ring through the gate before
    /// `VM_PAUSE`, and keeps them stopped until `VM_RESUME`, so there is
    /// nothing left to do here. A ring that is not held stopped would
    /// move the indices under the export, and the pause it lacks would
    /// have taken the VM write lock with a worker alive.
    pub(super) fn pause_rings(&self) -> Result<(), DeviceStateError> {
        let inner = self.inner.lock().expect("viona lock");
        if inner.halted || inner.stopped.is_some() {
            return Ok(());
        }
        Err(DeviceStateError::Export(
            "viona rings are not held stopped; the VM pause comes first".into(),
        ))
    }

    /// The rings start again when the VM pause lets go of them, after
    /// `VM_RESUME`. Nothing may start one sooner.
    pub(super) fn resume_rings(&self) -> Result<(), DeviceStateError> {
        Ok(())
    }

    /// Reset all viona rings (pause + reset). Call before ring_set_state.
    ///
    /// A ring the kernel still runs would read its own addresses while
    /// the restore writes new ones over them, so a refusal here fails
    /// the migration.
    pub fn reset_all_rings(&self) -> Result<(), DeviceStateError> {
        let mut inner = self.inner.lock().expect("viona lock");
        // The halt reset every ring already, and it holds no device
        // lock while it destroys it.
        if inner.halted {
            return Ok(());
        }
        // Under the gate the kernel rings are reset already. Forget
        // what they held, so the gate starts only what is restored.
        if let Some(stopped) = inner.stopped.as_mut() {
            for ring in 0..NET_NUM_QUEUES as u16 {
                stopped.set_saved(ring, None);
            }
            inner.ring_state = [RingState::Init; NET_NUM_QUEUES];
            inner.msi = [None; NET_NUM_QUEUES];
            return Ok(());
        }
        for ring in 0..NET_NUM_QUEUES as u16 {
            if let Err(e) = inner.link_ref().ring_pause(ring) {
                return Err(DeviceStateError::Invalid(format!(
                    "viona ring {ring} pause failed: {e}"
                )));
            }
            if let Err(e) = inner.link_ref().ring_reset(ring) {
                return Err(DeviceStateError::Invalid(format!(
                    "viona ring {ring} reset failed: {e}"
                )));
            }
        }
        Ok(())
    }

    /// Restore ring state after migration.
    ///
    /// This programs ring addresses AND indices into the kernel, unlike
    /// ring_init which resets indices to 0.
    ///
    /// Every failure below reaches the caller. The source commits the
    /// migration on this answer and then discards its own VM, so a
    /// destination that reported success over a refused ioctl leaves
    /// the guest a NIC nothing can put right.
    ///
    /// The destination's VM pause holds the rings stopped. The ring is
    /// still programmed here, so the kernel checks it before the
    /// commit, and then stopped again for the gate to start after
    /// `VM_RESUME`. The gate's lock keeps every write-lock ioctl out
    /// while the worker lives.
    pub fn ring_set_state(
        &self,
        queue_idx: u16,
        size: u16,
        desc: u64,
        avail: u64,
        used: u64,
        avail_idx: u16,
        used_idx: u16,
        msix_addr: u64,
        msix_data: u32,
    ) -> Result<(), DeviceStateError> {
        let state = viona_api::vioc_ring_state {
            vrs_index: queue_idx,
            vrs_avail_idx: avail_idx,
            vrs_used_idx: used_idx,
            vrs_qsize: size,
            vrs_qaddr_desc: desc,
            vrs_qaddr_avail: avail,
            vrs_qaddr_used: used,
        };
        let msi = (msix_addr != 0).then_some((msix_addr, u64::from(msix_data)));
        self.rings
            .exclusive(|| {
                let mut inner = self.inner.lock().expect("viona lock");
                Self::restore_ring(&mut inner, &self.rings, state, msi)
            })
            .map_err(|e| {
                DeviceStateError::Invalid(format!(
                    "viona ring {queue_idx} restore: {e}"
                ))
            })?
    }

    fn restore_ring(
        inner: &mut VionaInner,
        rings: &RingGate,
        state: viona_api::vioc_ring_state,
        msi: Option<(u64, u64)>,
    ) -> Result<(), DeviceStateError> {
        let queue_idx = state.vrs_index;
        let (size, avail_idx, used_idx) =
            (state.vrs_qsize, state.vrs_avail_idx, state.vrs_used_idx);
        // A restore that races the halt must not program a link that
        // is already destroyed.
        if inner.halted {
            return Err(DeviceStateError::Invalid(format!(
                "viona ring {queue_idx} restore after the link was destroyed"
            )));
        }
        if !indices_hold(size, avail_idx, used_idx) {
            return Err(DeviceStateError::Invalid(format!(
                "viona ring {queue_idx} indices cannot come from a running \
                 ring: size {size}, avail_idx {avail_idx}, used_idx {used_idx}"
            )));
        }

        // Set features (VERSION_1 sets l_modern in kernel)
        if inner.negotiated_features != 0 {
            let features = inner.negotiated_features;
            Self::set_kernel_features(inner, features).map_err(|e| {
                // VERSION_1 is what puts the kernel rings in the modern
                // layout the addresses below are read in, so a ring
                // programmed without it reads the wrong memory.
                DeviceStateError::Invalid(format!(
                    "viona features {features:#x} refused: {e}"
                ))
            })?;
        }

        // vrs_avail_idx sets vr_cur_aidx, the kernel's consumption
        // cursor. The kernel computes available entries as:
        //   num_avail = guest_memory_avail_idx - vr_cur_aidx
        //
        // We must use the REAL avail_idx from the source (not used_idx)
        // because descriptors between used_idx and avail_idx may have
        // been reclaimed by the guest driver. Replaying them causes
        // "id N is not a head!" errors in the guest.
        //
        // If avail_idx == used_idx (zero available buffers), the guest
        // driver must re-post buffers via the post-restore interrupt.
        if let Err(e) = inner.link_ref().ring_set_state(&state) {
            return Err(DeviceStateError::Invalid(format!(
                "viona ring {queue_idx} set state failed: {e}"
            )));
        }

        let idx = queue_idx as usize;
        if idx < NET_NUM_QUEUES {
            inner.ring_state[idx] = RingState::Ready;
            inner.msi[idx] = msi;
        }

        // Program MSI-X interrupt delivery for this queue
        let routed = match msi {
            Some((addr, msg)) => {
                inner.link_ref().ring_set_msi(queue_idx, addr, msg)
            }
            None => Ok(()),
        };

        // The worker the state write started goes first, whatever the
        // route did.
        if inner.stopped.is_some() {
            Self::stop_restored(inner, rings, queue_idx)?;
        }
        if let Err(e) = routed {
            return Err(DeviceStateError::Invalid(format!(
                "viona ring {queue_idx} set msi failed: {e}"
            )));
        }

        Self::ring_kick(inner, queue_idx);
        let log = inner.log();
        slog::info!(log, "viona ring state restored";
            "ring" => queue_idx, "avail_idx" => avail_idx,
            "used_idx" => used_idx,
            "msi_addr" => msi.map_or(0, |(addr, _)| addr));
        Ok(())
    }

    /// Take away the worker a restore under the gate started, and keep
    /// what the kernel holds for the gate to start.
    ///
    /// A ring that will not stop fails the restore, and the import with
    /// it. One whose reset was refused may still have a worker, so the
    /// gate is tainted: under the hold it would otherwise take the
    /// write lock beside that worker.
    fn stop_restored(
        inner: &mut VionaInner,
        rings: &RingGate,
        ring: u16,
    ) -> Result<(), DeviceStateError> {
        let failed = match stop_one(inner.link_ref(), ring) {
            StopOne::Kept(state) => {
                if let Some(stopped) = inner.stopped.as_mut() {
                    stopped.set_saved(ring, Some(state));
                }
                return Ok(());
            }
            StopOne::Lost(e) => e,
            StopOne::Escaped(e) => {
                rings.taint();
                e
            }
        };
        if let Some(stopped) = inner.stopped.as_mut() {
            stopped.set_saved(ring, None);
        }
        if let Some(s) = inner.ring_state.get_mut(usize::from(ring)) {
            *s = RingState::Error;
        }
        Err(DeviceStateError::Invalid(format!(
            "viona ring {ring} did not stop after its restore: {failed}"
        )))
    }

    /// Read one ring's `(avail_idx, used_idx)` for a migration export.
    ///
    /// The indices are the kernel's alone, and userspace holds nothing
    /// that stands in for them: viona never moves the indices the
    /// [`VirtQueue`] keeps. So a failure here has to fail the export.
    ///
    /// [`VirtQueue`]: crate::queue::VirtQueue
    pub fn kernel_ring_state(
        &self,
        queue_idx: u16,
    ) -> Result<(u16, u16), DeviceStateError> {
        let inner = self.inner.lock().expect("viona lock");
        // The guard every other link caller carries. The link is gone,
        // and the halt holds no device lock while it destroys it, so
        // an ioctl issued here could wait out an untimed kernel call
        // with the lock every vCPU needs.
        if inner.halted {
            return Err(DeviceStateError::Export(format!(
                "viona ring {queue_idx} state read after the link was \
                 destroyed"
            )));
        }
        // Under the gate the kernel ring is reset, and its indices are
        // the ones the gate read when it stopped it. A ring it holds
        // nothing for was never programmed.
        if let Some(stopped) = inner.stopped.as_ref() {
            return Ok(stopped
                .saved(queue_idx)
                .map_or((0, 0), |s| (s.vrs_avail_idx, s.vrs_used_idx)));
        }
        match inner.link_ref().ring_get_state(queue_idx) {
            Ok(state) => Ok((state.vrs_avail_idx, state.vrs_used_idx)),
            Err(e) => Err(DeviceStateError::Export(format!(
                "viona ring {queue_idx} state read failed: {e}"
            ))),
        }
    }
}

/// Whether a migration payload's indices can have come from a running
/// ring.
///
/// `avail_idx` is the kernel's consumption cursor and `used_idx` its
/// completion cursor, so the difference between them is the number of
/// descriptor chains in flight. A ring cannot hold more of those than
/// it has descriptors. Both cursors count in a u16 that wraps, so the
/// difference wraps with them.
///
/// WHY the destination checks: the payload comes from an
/// unauthenticated peer (OSF-1), so the pair may be anything. A cursor
/// that claims more chains in flight than the ring has descriptors
/// makes viona find entries in the available ring unconsumed and hand
/// the guest driver descriptors it has already reclaimed, which the
/// driver reports as "id N is not a head!".
fn indices_hold(size: u16, avail_idx: u16, used_idx: u16) -> bool {
    avail_idx.wrapping_sub(used_idx) <= size
}
