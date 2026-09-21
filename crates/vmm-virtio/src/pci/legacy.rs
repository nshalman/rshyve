// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The legacy (pre-1.0) face of the transitional device: the BAR0
//! register file.
//!
//! The read side and the write side share one map. This module owns
//! both so they cannot drift apart. Two parts of that map are not
//! fixed.
//!
//! Device-specific config starts at 0x14, and MSI-X moves it to 0x18
//! because two vector registers go in ahead of it.
//! [`device_config_offset`](VirtioPciDevice::device_config_offset)
//! decides that for both sides.
//!
//! The offered feature set is the other. Legacy strips EVENT_IDX and
//! INDIRECT_DESC, because a Linux 6.14 legacy guest deadlocks on them.
//! A modern driver gets both through BAR2 instead. The read reports
//! the stripped set, and the write masks the guest value with the same
//! set. The mask on the write is the one that has to hold: `val` comes
//! from the guest, so a device that only stripped on the read would
//! let a driver turn on a feature it was never offered.

use super::bits;
use super::{VirtioDevice, VirtioPciDevice};

impl<D: VirtioDevice> VirtioPciDevice<D> {
    /// Whether the driver turned MSI-X on.
    ///
    /// The legacy layout follows the enable bit in Message Control, not
    /// the presence of the capability. Every device here is built with
    /// an MSI-X table, so a presence test moves device config for a
    /// driver that never enabled MSI-X and reads it at 0x14.
    fn msix_enabled(&self) -> bool {
        self.msix.as_ref().is_some_and(|msix| msix.is_enabled())
    }

    /// Return the BAR0 offset where device-specific config begins.
    ///
    /// With MSI-X enabled, two extra 16-bit vector registers are
    /// inserted between ISR and device config (at 0x14 and 0x16),
    /// pushing device config to 0x18.
    pub(super) fn device_config_offset(&self) -> u16 {
        if self.msix_enabled() {
            bits::LEGACY_REG_DEVICE_CONFIG_MSIX
        } else {
            bits::LEGACY_REG_DEVICE_CONFIG
        }
    }

    /// Handle a read from the legacy virtio config BAR.
    pub(super) fn bar_read(&self, offset: u16, len: usize) -> u32 {
        let vs = self.virtio_state.lock().expect("virtio lock poisoned");

        let dev_cfg_off = self.device_config_offset();
        if offset >= dev_cfg_off {
            // Device-specific config read
            let dev_offset = offset - dev_cfg_off;
            return self.device.cfg_read(dev_offset, len as u8);
        }

        match offset {
            bits::LEGACY_REG_DEVICE_FEATURES => {
                // Truncate to 32-bit for legacy transport.
                // Strip EVENT_IDX and INDIRECT_DESC: these cause
                // deadlock with legacy transport on Linux 6.14.
                // They are available via the modern BAR2 path.
                (self.device.device_features() as u32)
                    & !bits::VIRTIO_F_RING_EVENT_IDX_U32
                    & !bits::VIRTIO_F_RING_INDIRECT_DESC_U32
            }
            bits::LEGACY_REG_GUEST_FEATURES => vs.guest_features_u32(),
            bits::LEGACY_REG_QUEUE_PFN => {
                vs.selected_queue().map(|q| q.pfn()).unwrap_or(0)
            }
            bits::LEGACY_REG_QUEUE_SIZE => vs
                .selected_queue()
                .map(|q| u32::from(q.size()))
                .unwrap_or(0),
            bits::LEGACY_REG_QUEUE_SELECT => u32::from(vs.queue_select),
            bits::LEGACY_REG_QUEUE_NOTIFY => {
                // Write-only register, read returns 0
                0
            }
            bits::LEGACY_REG_DEVICE_STATUS => u32::from(vs.readable_status()),
            bits::LEGACY_REG_ISR_STATUS => {
                // The line guard is taken with the transport lock
                // already given up: a raise holds the line guard and
                // an ISR read must not be able to hold the transport
                // lock behind it.
                drop(vs);
                u32::from(self.read_isr())
            }
            bits::LEGACY_REG_MSIX_CONFIG_VECTOR if self.msix_enabled() => {
                u32::from(vs.config_msix_vector)
            }
            bits::LEGACY_REG_MSIX_QUEUE_VECTOR if self.msix_enabled() => {
                let idx = vs.queue_select as usize;
                u32::from(
                    vs.queue_msix_vectors
                        .get(idx)
                        .copied()
                        .unwrap_or(bits::VIRTIO_MSI_NO_VECTOR),
                )
            }
            _ => 0,
        }
    }

    /// Handle a write to the legacy virtio config BAR.
    pub(super) fn bar_write(&self, offset: u16, val: u32, len: usize) {
        let Some(mut vs) = self.lock_for_write() else {
            return;
        };

        let dev_cfg_off = self.device_config_offset();
        if offset >= dev_cfg_off {
            // Device-specific config write
            let dev_offset = offset - dev_cfg_off;
            self.device.cfg_write(dev_offset, val, len as u8);
            return;
        }

        match offset {
            bits::LEGACY_REG_DEVICE_FEATURES => {
                // Read-only register, ignore writes
            }
            bits::LEGACY_REG_GUEST_FEATURES => {
                // Legacy has no FEATURES_OK, so negotiation ends where
                // the driver declares itself live. A write after that
                // would change what the device reads and writes while
                // the driver is using the rings.
                if vs.status & bits::STATUS_DRIVER_OK != 0 {
                    return;
                }
                // Mask: only accept features the device offered via legacy.
                // Strip EVENT_IDX/INDIRECT_DESC from legacy offered set.
                let offered = (self.device.device_features() as u32)
                    & !bits::VIRTIO_F_RING_EVENT_IDX_U32
                    & !bits::VIRTIO_F_RING_INDIRECT_DESC_U32;
                let accepted = val & offered;
                vs.guest_features = u64::from(accepted);
                vs.apply_features();
                self.device.set_features(u64::from(accepted));
            }
            bits::LEGACY_REG_QUEUE_PFN => {
                let queue_sel = vs.queue_select;
                let idx = queue_sel as usize;
                let log = self.log.clone();
                // A zero PFN is how a legacy driver takes the ring
                // down, so only a ring the guest is programming is
                // checked.
                let verdict = vs.selected_queue_mut().map(|q| {
                    q.set_log(log);
                    q.set_addr_legacy(val);
                    if val == 0 {
                        Ok(())
                    } else {
                        q.validate_addrs(&self.physmap)
                    }
                });
                match verdict {
                    None => {}
                    Some(Err(error)) => {
                        if let Some(q) = vs.selected_queue_mut() {
                            q.reset();
                        }
                        if let Some(live) = vs.queue_enabled.get_mut(idx) {
                            *live = false;
                        }
                        self.refuse_queue(vs, idx, Some(error));
                    }
                    Some(Ok(())) => {
                        // Legacy has no QUEUE_ENABLE: the PFN is what
                        // makes a queue live. Leaving this false let a
                        // migration export record every legacy queue as
                        // disabled, so the destination restored none of
                        // them and woke none of them.
                        if let Some(live) = vs.queue_enabled.get_mut(idx) {
                            *live = val != 0;
                        }
                        // Notify the device of the new queue address
                        // (kernel-accelerated devices need to program
                        // their ring addresses at this point)
                        if let Some(q) = vs.queues.get(idx) {
                            self.device.queue_addr_set(queue_sel, q);
                        }
                    }
                }
            }
            bits::LEGACY_REG_QUEUE_SELECT => {
                vs.queue_select = val as u16;
            }
            bits::LEGACY_REG_QUEUE_NOTIFY => {
                let queue_idx = val as u16;
                if !Self::can_notify(&vs, queue_idx) {
                    return;
                }
                let did_work = self.device.notify_queue(
                    queue_idx,
                    &mut vs.queues,
                    &self.physmap,
                );
                if Self::queue_needs_reset(&vs, queue_idx) {
                    // The queue logged its own reason when it refused.
                    self.refuse_queue(vs, queue_idx as usize, None);
                    return;
                }
                if did_work {
                    // Sampled under the guard, so a reset that ends
                    // this session is always the later of the two.
                    let session = self.intr.session();
                    drop(vs);
                    self.raise_queue_interrupt_in(session, queue_idx);
                }
            }
            bits::LEGACY_REG_DEVICE_STATUS => {
                let new_status = val as u8;
                if new_status == 0 {
                    self.reset_device(vs);
                } else {
                    self.set_status(vs, new_status);
                }
            }
            bits::LEGACY_REG_ISR_STATUS => {
                // Read-only register, ignore writes
            }
            bits::LEGACY_REG_MSIX_CONFIG_VECTOR if self.msix_enabled() => {
                vs.config_msix_vector = val as u16;
                self.msix_config_vector
                    .store(val as u16, std::sync::atomic::Ordering::Release);
            }
            bits::LEGACY_REG_MSIX_QUEUE_VECTOR if self.msix_enabled() => {
                let idx = vs.queue_select as usize;
                if idx < vs.queue_msix_vectors.len() {
                    vs.queue_msix_vectors[idx] = val as u16;
                    if let Some(a) = self.msix_queue_vectors.get(idx) {
                        a.store(
                            val as u16,
                            std::sync::atomic::Ordering::Release,
                        );
                    }
                }
            }
            _ => {}
        }
    }
}
