// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What this transport puts on the migration wire, and the order a
//! restore puts it back in.
//!
//! The order of the restore is the invariant this module owns. Nothing
//! in it is a preference:
//!
//! 1. Config space first. The BAR writes it replays are what put the
//!    device back on the PIO and MMIO buses. A queue programmed
//!    before them belongs to a device that decodes nothing.
//! 2. Features next, pushed into every queue. A guest does this in the
//!    feature register handler, and a restore does not go through that
//!    handler. A queue that never learns INDIRECT_DESC drops every
//!    indirect chain in silence, which is all Linux virtio-blk I/O.
//! 3. Ring addresses and indices, then the MSI-X table. The backend is
//!    told to restore a ring only after both, because it programs
//!    hardware from them.
//! 4. An interrupt on each live queue, last. A driver that comes back
//!    with its rings caught up has nothing to poll for. Without the
//!    wake it posts no new receive buffers, and the kernel viona
//!    driver drops arriving packets with "no_space".
//!
//! Every field is checked before any of it is applied. The peer is
//! unauthenticated (OSF-1), so a payload that the destination cannot
//! hold has to fail the migration, not half-program a device.

use std::sync::atomic::Ordering;

use vmm_devices::lifecycle::{
    DeviceStateError, MigratePciState, VirtioMigrateQueue, VirtioMigrateState,
};
use vmm_devices::pci::device::replay_migrate_pci_state;
use vmm_devices::pci::BarN;

use super::bits;
use super::{VirtioDevice, VirtioPciDevice};

impl<D: VirtioDevice> VirtioPciDevice<D> {
    /// Replay BAR and command-register writes so bus registrations are
    /// restored. The writes go through `cfg_write`, the same path a
    /// guest BAR write takes, so the bus applies its own rules.
    fn restore_migrate_pci_state(&self, state: &MigratePciState) {
        let writes = self
            .pci_state
            .lock()
            .expect("pci lock")
            .bar_replay_writes(&state.bar_addrs);
        replay_migrate_pci_state(self, &writes, state.command);
    }

    /// Snapshot the register file the driver programmed and every queue
    /// it configured.
    pub(super) fn export_migrate_queues(&self) -> VirtioMigrateState {
        let vs = self.virtio_state.lock().expect("virtio lock");
        let modern = vs.guest_features & bits::VIRTIO_F_VERSION_1 != 0;
        let mut queues = Vec::new();
        for idx in 0..self.num_queues as usize {
            let q = &vs.queues[idx];
            if !q.is_configured() {
                continue;
            }
            let (k_avail, k_used) =
                self.device.get_kernel_ring_indices(idx as u16);
            // A software device keeps its completion cursor in
            // VirtioCompletion, which writes the used ring directly and
            // never moves the VirtQueue shadow. Guest memory is
            // therefore the only place that cursor can be read from.
            let used_idx = if k_used != 0 {
                k_used
            } else {
                let guest_used = q.read_used_ring_idx(&self.physmap);
                if guest_used != 0 {
                    guest_used
                } else {
                    q.shadow_used_idx()
                }
            };
            // A kernel ring's consumption cursor is the kernel's. The
            // entries between it and the guest's avail idx are posted
            // but unconsumed, and stay available on the destination.
            let avail_idx = if k_used != 0 {
                k_avail
            } else {
                q.last_avail_idx()
            };

            let vector = self
                .msix_queue_vectors
                .get(idx)
                .map(|a| a.load(Ordering::Acquire))
                .unwrap_or(bits::VIRTIO_MSI_NO_VECTOR);

            queues.push(VirtioMigrateQueue {
                queue_idx: idx as u16,
                queue_size: q.size(),
                desc_addr: q.desc_addr(),
                avail_addr: q.avail_addr(),
                used_addr: q.used_addr(),
                // The legacy transport has no enable register: writing
                // QUEUE_PFN is what makes the ring live. Reading
                // `queue_enabled` alone migrates every legacy queue as
                // dead, and the destination then skips its backend
                // restore and its wake.
                live: if modern {
                    vs.queue_enabled.get(idx).copied().unwrap_or(false)
                } else {
                    true
                },
                avail_idx,
                used_idx,
                msix_vector: vector,
            });
        }

        VirtioMigrateState {
            status: vs.status,
            features: vs.guest_features,
            config_msix_vector: self.msix_config_vector.load(Ordering::Acquire),
            pci: self.pci_state.lock().expect("pci lock").migrate_state(),
            msix: self.msix.as_ref().map(|msix| msix.export_state()),
            queues,
        }
    }

    /// Everything the payload must satisfy before any of it is applied.
    fn check_migrate_state(
        &self,
        state: &VirtioMigrateState,
    ) -> Result<(), DeviceStateError> {
        let nq = self.num_queues;
        let table_len = self.msix.as_ref().map_or(0, |m| m.count());
        if state.msix.is_some() != self.msix.is_some() {
            return Err(DeviceStateError::invalid(format!(
                "payload {} an MSI-X table, this device {} one",
                if state.msix.is_some() {
                    "has"
                } else {
                    "has no"
                },
                if self.msix.is_some() {
                    "has"
                } else {
                    "has none"
                },
            )));
        }
        check_vector("config", state.config_msix_vector, table_len)?;

        let vs = self.virtio_state.lock().expect("virtio lock");
        let mut seen = vec![false; nq as usize];
        for qs in &state.queues {
            let idx = usize::from(qs.queue_idx);
            if qs.queue_idx >= nq {
                return Err(DeviceStateError::invalid(format!(
                    "queue {} is past this device's {nq} queues",
                    qs.queue_idx,
                )));
            }
            if std::mem::replace(&mut seen[idx], true) {
                return Err(DeviceStateError::invalid(format!(
                    "queue {} appears twice",
                    qs.queue_idx,
                )));
            }
            // The destination is built from the same command line, so
            // its own queue size is the only one it can serve.
            let own = vs.queues[idx].size();
            if qs.queue_size != own {
                return Err(DeviceStateError::invalid(format!(
                    "queue {} has size {}, this device's queue is {own}",
                    qs.queue_idx, qs.queue_size,
                )));
            }
            // The difference between the cursors is the number of
            // chains in flight, and a ring holds no more of those than
            // it has descriptors. Both cursors wrap in a u16.
            let in_flight = qs.avail_idx.wrapping_sub(qs.used_idx);
            if in_flight > qs.queue_size {
                return Err(DeviceStateError::invalid(format!(
                    "queue {} reports {in_flight} chains in flight on a \
                     {}-descriptor ring",
                    qs.queue_idx, qs.queue_size,
                )));
            }
            check_vector("queue", qs.msix_vector, table_len)?;
            if qs.live {
                let mut probe = crate::queue::VirtQueue::new(own);
                probe.set_addr_modern(
                    qs.desc_addr,
                    qs.avail_addr,
                    qs.used_addr,
                );
                probe.set_event_idx(
                    state.features & bits::VIRTIO_F_RING_EVENT_IDX != 0,
                );
                probe.validate_addrs(&self.physmap).map_err(|error| {
                    DeviceStateError::invalid(format!(
                        "queue {} rings: {error}",
                        qs.queue_idx,
                    ))
                })?;
            }
        }
        Ok(())
    }

    /// Rebuild the transport from an export, in the order the module
    /// doc gives.
    pub(super) fn restore_migrate_queues(
        &self,
        state: &VirtioMigrateState,
    ) -> Result<(), DeviceStateError> {
        self.check_migrate_state(state)?;
        self.restore_migrate_pci_state(&state.pci);

        let mut vs = self.virtio_state.lock().expect("virtio lock");
        self.device.reset_all_rings();

        vs.guest_features = state.features;
        self.device.set_features(state.features);
        // The guest does this in the feature register handler, which a
        // restore does not go through. A queue that never learns
        // INDIRECT_DESC drops every indirect chain in silence.
        let event_idx = state.features & bits::VIRTIO_F_RING_EVENT_IDX != 0;
        let indirect = state.features & bits::VIRTIO_F_RING_INDIRECT_DESC != 0;
        for q in &mut vs.queues {
            q.set_event_idx(event_idx);
            q.set_indirect_supported(indirect);
        }

        for qs in &state.queues {
            let idx = usize::from(qs.queue_idx);
            vs.queues[idx].set_addr_modern(
                qs.desc_addr,
                qs.avail_addr,
                qs.used_addr,
            );
            // set_addr_modern zeroes the cursors; the guest continues
            // from where the source left them.
            vs.queues[idx].set_last_avail_idx(qs.avail_idx);
            vs.queues[idx].set_shadow_used_idx(qs.used_idx);
            if idx < vs.queue_desc.len() {
                vs.queue_desc[idx] = qs.desc_addr;
                vs.queue_avail[idx] = qs.avail_addr;
                vs.queue_used[idx] = qs.used_addr;
            }
            if idx < vs.queue_enabled.len() {
                vs.queue_enabled[idx] = qs.live;
            }
            if idx < vs.queue_msix_vectors.len() {
                vs.queue_msix_vectors[idx] = qs.msix_vector;
            }
            if let Some(a) = self.msix_queue_vectors.get(idx) {
                a.store(qs.msix_vector, Ordering::Release);
            }
        }

        // The status byte is the driver's, not ours. Synthesising
        // DRIVER_OK tells a guest whose driver never finished the
        // handshake that the device is ready.
        vs.status = state.status;
        vs.config_msix_vector = state.config_msix_vector;
        self.msix_config_vector
            .store(state.config_msix_vector, Ordering::Release);

        // Masks and pending bits come back with the table. An INTx
        // guest arrives with MSI-X still disabled, which is where it
        // has to stay: enabling it moves device config from 0x14 to
        // 0x18 and routes every queue interrupt to NO_VECTOR.
        if let (Some(msix), Some(table)) = (self.msix.as_ref(), &state.msix) {
            msix.import_state(table).map_err(|error| {
                DeviceStateError::invalid(format!("MSI-X table: {error}"))
            })?;
        }

        for qs in &state.queues {
            if !qs.live {
                continue;
            }
            let (msix_addr, msix_data) = self.msix_entry(qs.msix_vector);
            self.device.restore_ring_state(
                qs.queue_idx,
                qs.queue_size,
                qs.desc_addr,
                qs.avail_addr,
                qs.used_addr,
                qs.avail_idx,
                qs.used_idx,
                msix_addr,
                msix_data,
            );
        }

        let (bar0_port, bar2_addr) = self.notify_bases();
        if bar0_port != 0 || bar2_addr != 0 {
            let notify_pio = if bar0_port != 0 {
                bar0_port + bits::LEGACY_REG_QUEUE_NOTIFY
            } else {
                0
            };
            let notify_mmio = if bar2_addr != 0 {
                bar2_addr + bits::MODERN_BAR_NOTIFY_OFFSET as u64
            } else {
                0
            };
            self.device.set_notify_addrs(notify_pio, notify_mmio);
        }

        // Rings, vectors and notification addresses are all programmed.
        self.device.start_poll_deferred();

        drop(vs);
        // After migration the rx ring may hold no available buffers.
        // The driver needs an interrupt to process the caught-up used
        // ring and post new ones, or the kernel viona driver drops
        // arriving packets with "no_space".
        for qs in &state.queues {
            if qs.live {
                self.raise_queue_interrupt_current(qs.queue_idx);
            }
        }
        Ok(())
    }

    /// The MSI-X address and data a vector routes to, after the table
    /// has been restored. `(0, 0)` when the queue has no vector.
    fn msix_entry(&self, vector: u16) -> (u64, u32) {
        if vector == bits::VIRTIO_MSI_NO_VECTOR {
            return (0, 0);
        }
        match self.msix.as_ref() {
            Some(msix) => {
                let (addr, data) = msix.read_entry(vector);
                (addr, data as u32)
            }
            None => (0, 0),
        }
    }

    /// The PIO port and MMIO base the notification registers sit in.
    fn notify_bases(&self) -> (u16, u64) {
        let pci = self.pci_state.lock().expect("pci lock");
        let bar0_port = pci
            .bars()
            .get(BarN::BAR0)
            .and_then(|(def, addr)| def.is_pio().then_some(addr as u16))
            .unwrap_or(0);
        let bar2_addr = pci
            .bars()
            .get(BarN::BAR2)
            .and_then(|(def, addr)| def.is_mmio().then_some(addr))
            .unwrap_or(0);
        (bar0_port, bar2_addr)
    }
}

/// An MSI-X vector the destination's table cannot hold.
fn check_vector(
    what: &str,
    vector: u16,
    table_len: u16,
) -> Result<(), DeviceStateError> {
    if vector == bits::VIRTIO_MSI_NO_VECTOR || vector < table_len {
        return Ok(());
    }
    Err(DeviceStateError::invalid(format!(
        "{what} MSI-X vector {vector} is past the {table_len}-entry table",
    )))
}
