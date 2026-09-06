// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The ring state a migration reads out and puts back.
//!
//! These calls are the kernel's own, and they run only on illumos.
//! The source pauses each ring and reads its indices; the destination
//! programs the addresses and the indices together, which
//! [`super::VirtioViona::ring_init`] cannot do because it resets the
//! indices to 0.

use super::{RingState, VirtioViona, NET_NUM_QUEUES};

impl VirtioViona {
    /// Stop every ring worker, so the indices this reads out hold
    /// still.
    #[cfg(target_os = "illumos")]
    pub(super) fn pause_rings(&self) {
        let inner = self.inner.lock().expect("viona lock");
        for idx in 0..NET_NUM_QUEUES {
            if let Err(e) = inner.viona_fd.ring_pause(idx) {
                slog::warn!(inner.log(), "viona ring pause failed";
                    "ring" => idx, "error" => %e);
            }
        }
    }

    #[cfg(not(target_os = "illumos"))]
    pub(super) fn pause_rings(&self) {}

    /// Reset all viona rings (pause + reset). Call before ring_set_state.
    #[cfg(target_os = "illumos")]
    pub fn reset_all_rings(&self) {
        let inner = self.inner.lock().expect("viona lock");
        for idx in 0..NET_NUM_QUEUES {
            if let Err(e) = inner.viona_fd.ring_pause(idx) {
                slog::warn!(inner.log(), "viona ring pause failed";
                    "ring" => idx, "error" => %e);
            }
            if let Err(e) = inner.viona_fd.ring_reset(idx) {
                slog::warn!(inner.log(), "viona ring reset failed";
                    "ring" => idx, "error" => %e);
            }
        }
    }

    #[cfg(not(target_os = "illumos"))]
    pub fn reset_all_rings(&self) {}

    /// Restore ring state after migration using VNA_IOC_RING_SET_STATE.
    ///
    /// This programs ring addresses AND indices into the kernel, unlike
    /// ring_init which resets indices to 0.
    #[cfg(target_os = "illumos")]
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

        // Program ring state (addresses + indices).
        //
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
        let mut state = viona_api::vioc_ring_state {
            vrs_index: queue_idx,
            vrs_avail_idx: avail_idx,
            vrs_used_idx: used_idx,
            vrs_qsize: size,
            vrs_qaddr_desc: desc,
            vrs_qaddr_avail: avail,
            vrs_qaddr_used: used,
        };
        // Safety: the command copies in one vioc_ring_state, and that
        // is what `state` is.
        let rc = unsafe {
            inner
                .viona_fd
                .ioctl(viona_api::VNA_IOC_RING_SET_STATE, &mut state)
        };
        if let Err(e) = rc {
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
            let mut msi = viona_api::vioc_ring_msi {
                rm_index: queue_idx,
                rm_addr: msix_addr,
                rm_msg: msix_data as u64,
                ..Default::default()
            };
            // Safety: the command copies in one vioc_ring_msi, and
            // that is what `msi` is.
            let msi_rc = unsafe {
                inner
                    .viona_fd
                    .ioctl(viona_api::VNA_IOC_RING_SET_MSI, &mut msi)
            };
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

    #[cfg(not(target_os = "illumos"))]
    pub fn ring_set_state(
        &self,
        _: u16,
        _: u16,
        _: u64,
        _: u64,
        _: u64,
        _: u16,
        _: u16,
        _: u64,
        _: u32,
    ) {
    }

    /// Get ring state from kernel (avail_idx, used_idx) for migration export.
    #[cfg(target_os = "illumos")]
    pub fn get_kernel_ring_state(&self, queue_idx: u16) -> (u16, u16) {
        let inner = self.inner.lock().expect("viona lock");
        let mut state = viona_api::vioc_ring_state {
            vrs_index: queue_idx,
            ..Default::default()
        };
        // Safety: the command copies out one vioc_ring_state, and that
        // is what `state` is.
        let rc = unsafe {
            inner
                .viona_fd
                .ioctl(viona_api::VNA_IOC_RING_GET_STATE, &mut state)
        };
        match rc {
            Ok(_) => (state.vrs_avail_idx, state.vrs_used_idx),
            // The caller writes these into the migration payload, so a
            // failure here moves a ring the destination cannot resume.
            Err(e) => {
                slog::error!(inner.log(), "viona ring get state failed";
                    "ring" => queue_idx, "error" => %e);
                (0, 0)
            }
        }
    }

    #[cfg(not(target_os = "illumos"))]
    pub fn get_kernel_ring_state(&self, _queue_idx: u16) -> (u16, u16) {
        (0, 0)
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
