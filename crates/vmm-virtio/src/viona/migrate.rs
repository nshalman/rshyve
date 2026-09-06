// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The ring state a migration reads out and puts back.
//!
//! The source pauses each ring and reads its indices; the destination
//! programs the addresses and the indices together, which
//! [`super::VirtioViona::ring_init`] cannot do because it resets the
//! indices to 0.

use super::{RingState, VirtioViona, NET_NUM_QUEUES};

impl VirtioViona {
    /// Stop every ring worker, so the indices this reads out hold
    /// still.
    pub(super) fn pause_rings(&self) {
        let inner = self.inner.lock().expect("viona lock");
        for ring in 0..NET_NUM_QUEUES as u16 {
            if let Err(e) = inner.link_ref().ring_pause(ring) {
                slog::warn!(inner.log(), "viona ring pause failed";
                    "ring" => ring, "error" => %e);
            }
        }
    }

    /// Reset all viona rings (pause + reset). Call before ring_set_state.
    pub fn reset_all_rings(&self) {
        let inner = self.inner.lock().expect("viona lock");
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

        Self::ring_kick(&inner, queue_idx);
        slog::info!(log, "viona ring state restored";
            "ring" => queue_idx, "avail_idx" => avail_idx,
            "used_idx" => used_idx, "msi_addr" => msix_addr);
    }

    /// Get ring state from kernel (avail_idx, used_idx) for migration export.
    pub fn get_kernel_ring_state(&self, queue_idx: u16) -> (u16, u16) {
        let inner = self.inner.lock().expect("viona lock");
        match inner.link_ref().ring_get_state(queue_idx) {
            Ok(state) => (state.vrs_avail_idx, state.vrs_used_idx),
            // The caller writes these into the migration payload, so a
            // failure here moves a ring the destination cannot resume.
            Err(e) => {
                slog::error!(inner.log(), "viona ring get state failed";
                    "ring" => queue_idx, "error" => %e);
                (0, 0)
            }
        }
    }

    /// Kick all ready queues after migration to wake kernel processing.
    pub fn kick_all_queues(&self) {
        let inner = self.inner.lock().expect("viona lock");
        for idx in 0..NET_NUM_QUEUES as u16 {
            if inner.ring_state[idx as usize] == RingState::Ready {
                Self::ring_kick(&inner, idx);
            }
        }
    }
}
