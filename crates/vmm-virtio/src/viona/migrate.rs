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

use super::{PausedRing, RingState, VirtioViona, NET_NUM_QUEUES};

impl VirtioViona {
    /// Stop every ring worker, so the indices this reads out hold
    /// still, and keep each ring's state for a rollback.
    ///
    /// A ring that could not be paused keeps consuming available
    /// entries while the export reads its indices, so the failure has
    /// to fail the migration.
    pub(super) fn pause_rings(&self) -> Result<(), DeviceStateError> {
        let mut inner = self.inner.lock().expect("viona lock");
        // Same guard as the restore below. The halt drops the device
        // lock and sits in an untimed kernel call, so a pause that got
        // in could wait for that call inside the kernel while holding
        // the lock every vCPU needs.
        if inner.halted {
            return Ok(());
        }
        for ring in 0..NET_NUM_QUEUES as u16 {
            if let Err(e) = inner.link_ref().ring_pause(ring) {
                return Err(DeviceStateError::Export(format!(
                    "viona ring {ring} pause failed: {e}"
                )));
            }
            // The read comes after the pause. A running ring moves both
            // cursors between the two calls, so a rollback taken from a
            // read made first programs cursors the kernel has already
            // passed and replays the descriptors between them.
            let idx = usize::from(ring);
            match inner.link_ref().ring_get_state(ring) {
                Ok(state) => inner.paused_rings[idx] = PausedRing::Saved(state),
                Err(e) => {
                    // The ring is stopped and nothing can restart it: a
                    // paused ring needs a reset plus a state write, and
                    // the state is what the kernel just refused.
                    inner.paused_rings[idx] = PausedRing::Lost;
                    return Err(DeviceStateError::Export(format!(
                        "viona ring {ring} state read failed: {e}"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Put the rings [`Self::pause_rings`] stopped back to work.
    ///
    /// A paused ring is `VRS_STOP` in the kernel, and
    /// `viona_ioc_ring_kick` answers EBUSY for one, so a guest kick
    /// cannot restart it. Only a reset followed by a state write can,
    /// which is what this does from the state the pause saved.
    pub(super) fn resume_rings(&self) -> Result<(), DeviceStateError> {
        let mut inner = self.inner.lock().expect("viona lock");
        if inner.halted {
            return Ok(());
        }
        let mut failed: Vec<String> = Vec::new();
        for ring in 0..NET_NUM_QUEUES as u16 {
            let idx = usize::from(ring);
            let state = match std::mem::take(&mut inner.paused_rings[idx]) {
                PausedRing::Running => continue,
                // The export stopped this ring and holds nothing to
                // restart it from, so the source guest keeps a dead
                // ring whatever happens next. Reporting it is the only
                // honest answer.
                PausedRing::Lost => {
                    failed.push(format!("ring {ring} has no saved state"));
                    continue;
                }
                PausedRing::Saved(state) => state,
            };
            // The guest ran between the pause and the abort and may
            // have reset the device. Its rings are the driver's to
            // program again, not ours to revive.
            if inner.ring_state[idx] != RingState::Ready {
                continue;
            }
            if let Err(e) = inner.link_ref().ring_reset(ring) {
                failed.push(format!("ring {ring} reset: {e}"));
                continue;
            }
            if let Err(e) = inner.link_ref().ring_set_state(&state) {
                failed.push(format!("ring {ring} set state: {e}"));
                continue;
            }
            Self::ring_kick(&mut inner, ring);
        }
        // A BAR move during the export was held back.
        Self::retry_hook(&mut inner);
        if failed.is_empty() {
            return Ok(());
        }
        Err(DeviceStateError::Invalid(format!(
            "viona rings stayed paused: {}",
            failed.join("; ")
        )))
    }

    /// Reset all viona rings (pause + reset). Call before ring_set_state.
    ///
    /// A ring the kernel still runs would read its own addresses while
    /// the restore writes new ones over them, so a refusal here fails
    /// the migration.
    pub fn reset_all_rings(&self) -> Result<(), DeviceStateError> {
        let inner = self.inner.lock().expect("viona lock");
        // The halt reset every ring already, and it holds no device
        // lock while it destroys the link.
        if inner.halted {
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
        let mut inner = self.inner.lock().expect("viona lock");
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
            Self::set_kernel_features(&inner, features).map_err(|e| {
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
        let state = viona_api::vioc_ring_state {
            vrs_index: queue_idx,
            vrs_avail_idx: avail_idx,
            vrs_used_idx: used_idx,
            vrs_qsize: size,
            vrs_qaddr_desc: desc,
            vrs_qaddr_avail: avail,
            vrs_qaddr_used: used,
        };
        if let Err(e) = inner.link_ref().ring_set_state(&state) {
            return Err(DeviceStateError::Invalid(format!(
                "viona ring {queue_idx} set state failed: {e}"
            )));
        }

        let idx = queue_idx as usize;
        if idx < NET_NUM_QUEUES {
            inner.ring_state[idx] = RingState::Ready;
        }

        // Program MSI-X interrupt delivery for this queue
        if msix_addr != 0 {
            let msi_rc = inner.link_ref().ring_set_msi(
                queue_idx,
                msix_addr,
                u64::from(msix_data),
            );
            if let Err(e) = msi_rc {
                return Err(DeviceStateError::Invalid(format!(
                    "viona ring {queue_idx} set msi failed: {e}"
                )));
            }
        }

        Self::ring_kick(&mut inner, queue_idx);
        let log = inner.log();
        slog::info!(log, "viona ring state restored";
            "ring" => queue_idx, "avail_idx" => avail_idx,
            "used_idx" => used_idx, "msi_addr" => msix_addr);
        Ok(())
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
