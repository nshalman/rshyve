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

use super::{RingState, VirtioViona, NET_NUM_QUEUES};

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
            // Read the state before the pause: the ring is running, so
            // this is what a rollback has to put back.
            let saved = inner.link_ref().ring_get_state(ring).ok();
            if let Err(e) = inner.link_ref().ring_pause(ring) {
                return Err(DeviceStateError::Export(format!(
                    "viona ring {ring} pause failed: {e}"
                )));
            }
            inner.paused_rings[usize::from(ring)] = saved;
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
            let Some(state) = inner.paused_rings[idx].take() else {
                continue;
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
        if failed.is_empty() {
            return Ok(());
        }
        Err(DeviceStateError::Invalid(format!(
            "viona rings stayed paused: {}",
            failed.join("; ")
        )))
    }

    /// Reset all viona rings (pause + reset). Call before ring_set_state.
    pub fn reset_all_rings(&self) {
        let inner = self.inner.lock().expect("viona lock");
        // The halt reset every ring already, and it holds no device
        // lock while it destroys the link.
        if inner.halted {
            return;
        }
        for ring in 0..NET_NUM_QUEUES as u16 {
            if let Err(e) = inner.link_ref().ring_pause(ring) {
                slog::warn!(inner.log(), "viona ring pause failed";
                    "ring" => ring, "error" => %e);
            }
            if let Err(e) = inner.link_ref().ring_reset(ring) {
                slog::warn!(inner.log(), "viona ring reset failed";
                    "ring" => ring, "error" => %e);
            }
        }
    }

    /// Restore ring state after migration.
    ///
    /// This programs ring addresses AND indices into the kernel, unlike
    /// ring_init which resets indices to 0.
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
    ) {
        let mut inner = self.inner.lock().expect("viona lock");
        let log = inner.log();
        // A restore that races the halt must not program a link that
        // is already destroyed.
        if inner.halted {
            return;
        }
        if !indices_hold(size, avail_idx, used_idx) {
            slog::error!(log, "viona ring state refused: the indices \
                cannot come from a running ring";
                "ring" => queue_idx, "size" => size,
                "avail_idx" => avail_idx, "used_idx" => used_idx);
            return;
        }

        // Set features (VERSION_1 sets l_modern in kernel)
        if inner.negotiated_features != 0 {
            Self::set_kernel_features(&inner, inner.negotiated_features);
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
            slog::error!(log, "viona ring set state failed";
                "ring" => queue_idx, "error" => %e);
            return;
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
                slog::warn!(log, "viona ring set msi failed";
                    "ring" => queue_idx, "error" => %e);
            }
        }

        Self::ring_kick(&mut inner, queue_idx);
        slog::info!(log, "viona ring state restored";
            "ring" => queue_idx, "avail_idx" => avail_idx,
            "used_idx" => used_idx, "msi_addr" => msix_addr);
    }

    /// Read one ring's indices for a migration export.
    ///
    /// `None` says the device has no answer. The indices are the
    /// kernel's alone, and userspace holds nothing that stands in for
    /// them: viona never moves the indices the [`VirtQueue`] keeps.
    ///
    /// [`VirtQueue`]: crate::queue::VirtQueue
    pub fn get_kernel_ring_state(&self, queue_idx: u16) -> Option<(u16, u16)> {
        let inner = self.inner.lock().expect("viona lock");
        // The guard every other link caller carries. The link is gone,
        // and the halt holds no device lock while it destroys it, so
        // an ioctl issued here could wait out an untimed kernel call
        // with the lock every vCPU needs.
        if inner.halted {
            slog::error!(inner.log(),
                "viona ring state read after the link was destroyed";
                "ring" => queue_idx);
            return None;
        }
        match inner.link_ref().ring_get_state(queue_idx) {
            Ok(state) => Some((state.vrs_avail_idx, state.vrs_used_idx)),
            Err(e) => {
                slog::error!(inner.log(), "viona ring get state failed";
                    "ring" => queue_idx, "error" => %e);
                None
            }
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
/// WHY the destination checks: the export cannot report a failed index
/// read to its caller (`get_kernel_ring_indices` answers with a pair
/// of numbers and nothing else), so a ring the kernel refused to
/// answer for reaches here as a cursor of 0 against a used index the
/// export read out of guest memory. Programmed as is, viona finds
/// every entry in the available ring unconsumed and hands the guest
/// driver descriptors it has already reclaimed, which the driver
/// reports as "id N is not a head!". The same check is what a
/// migration source that is hostile, or simply older, meets.
fn indices_hold(size: u16, avail_idx: u16, used_idx: u16) -> bool {
    avail_idx.wrapping_sub(used_idx) <= size
}
