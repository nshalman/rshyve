// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What this transport puts on the migration wire, and the order a
//! restore puts it back in.
//!
//! An export is one record for each configured queue. Every record
//! carries a copy of the config-space state, so a restore can start
//! from any one of them.
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
//! 3. Ring addresses and indices, then the MSI-X table entry. The
//!    backend is told to restore a ring only after both, because it
//!    programs hardware from them.
//! 4. An interrupt on each enabled queue, last. A driver that comes
//!    back with its rings caught up has nothing to poll for. Without
//!    the wake it posts no new receive buffers, and the kernel viona
//!    driver drops arriving packets with "no_space".

use vmm_devices::pci::bar::BarDefine;
use vmm_devices::pci::device::PciDevice;
use vmm_devices::pci::BarN;

use super::bits;
use super::{VirtioDevice, VirtioPciDevice};

impl<D: VirtioDevice> VirtioPciDevice<D> {
    /// Snapshot PCI BAR addresses and command bits for migration.
    fn export_migrate_pci_state(
        &self,
    ) -> vmm_devices::lifecycle::VirtioMigratePciState {
        let pci = self.pci_state.lock().expect("pci lock");
        let mut bar_addrs = [0u64; 6];
        for (idx, addr) in bar_addrs.iter_mut().enumerate() {
            let bar = BarN::try_from(idx as u8).expect("valid BAR index");
            *addr = pci.bars().get(bar).map(|(_, value)| value).unwrap_or(0);
        }

        vmm_devices::lifecycle::VirtioMigratePciState {
            command: pci.command().bits(),
            bar_addrs,
        }
    }

    /// Replay BAR and command-register writes so bus registrations are restored.
    fn restore_migrate_pci_state(
        &self,
        state: &vmm_devices::lifecycle::VirtioMigratePciState,
    ) {
        let mut bar_writes = Vec::with_capacity(7);
        {
            let pci = self.pci_state.lock().expect("pci lock");
            for (idx, addr) in state.bar_addrs.iter().copied().enumerate() {
                let bar = BarN::try_from(idx as u8).expect("valid BAR index");
                let Some((def, _)) = pci.bars().get(bar) else {
                    continue;
                };

                let offset = vmm_devices::pci::bits::REG_BAR0 + (idx as u8 * 4);
                match def {
                    BarDefine::Pio(_) | BarDefine::Mmio(_) => {
                        bar_writes.push((offset, addr as u32));
                    }
                    BarDefine::Mmio64(_) => {
                        bar_writes.push((offset, addr as u32));
                        bar_writes.push((offset + 4, (addr >> 32) as u32));
                    }
                }
            }
        }

        for (offset, value) in bar_writes {
            self.cfg_write(offset, 4, value);
        }
        self.cfg_write(
            vmm_devices::pci::bits::REG_COMMAND,
            2,
            u32::from(state.command),
        );
    }

    /// Snapshot every configured queue, plus the config-space
    /// state a restore replays before it touches any of them.
    pub(super) fn export_migrate_queues(
        &self,
    ) -> Vec<vmm_devices::lifecycle::VirtioMigrateQueue> {
        use vmm_devices::lifecycle::VirtioMigrateQueue;
        // Ring threads are already paused by pause_for_migration()
        // which runs BEFORE vCPU pause. This ensures the guest's
        // descriptor state is consistent with the kernel's indices.
        let vs = self.virtio_state.lock().expect("virtio lock");
        let pci_state = self.export_migrate_pci_state();
        let mut result = Vec::new();
        for idx in 0..self.num_queues as usize {
            let q = &vs.queues[idx];
            if q.is_configured() {
                let (k_avail, k_used) =
                    self.device.get_kernel_ring_indices(idx as u16);
                // For software devices (blk), read used_idx from guest
                // memory because VirtioCompletion manages the used ring
                // separately from VirtQueue's shadow_used_idx.
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
                // For kernel-managed queues (viona), use the kernel's
                // actual consumed position (k_avail). The delta between
                // k_avail and guest memory's avail_ring->idx represents
                // entries the guest posted but the kernel hasn't consumed.
                // On the dest, these entries are immediately available —
                // their descriptors are valid (guest set them up, kernel
                // hasn't touched them). Using guest memory's avail_ring->idx
                // instead would make the dest kernel see 0 available
                // entries, requiring complex workarounds.
                let avail_idx = if k_used != 0 {
                    k_avail
                } else {
                    q.last_avail_idx()
                };

                // Get MSI-X entry for this queue's vector
                let vector = self
                    .msix_queue_vectors
                    .get(idx)
                    .map(|a| a.load(std::sync::atomic::Ordering::Acquire))
                    .unwrap_or(bits::VIRTIO_MSI_NO_VECTOR);
                let (msix_addr, msix_data) = if let Some(ref msix) = self.msix {
                    if vector != bits::VIRTIO_MSI_NO_VECTOR {
                        msix.read_entry(vector)
                    } else {
                        (0, 0)
                    }
                } else {
                    (0, 0)
                };

                result.push(VirtioMigrateQueue {
                    pci_slot: 0,
                    queue_idx: idx as u16,
                    queue_size: q.size(),
                    desc_addr: q.desc_addr(),
                    avail_addr: q.avail_addr(),
                    used_addr: q.used_addr(),
                    enabled: vs
                        .queue_enabled
                        .get(idx)
                        .copied()
                        .unwrap_or(false),
                    features: vs.guest_features,
                    avail_idx,
                    used_idx,
                    msix_addr,
                    msix_data: msix_data as u32,
                    msix_vector: vector,
                    pci_state: Some(pci_state.clone()),
                });
                // Detailed ring state dump for migration debugging
                let guest_avail = q.read_avail_idx(&self.physmap);
                let guest_used = q.read_used_ring_idx(&self.physmap);
                eprintln!("[virtio-export] q={} k_avail={} k_used={} guest_avail={} guest_used={} export_avail={} export_used={}",
                    idx, k_avail, k_used, guest_avail, guest_used, avail_idx, used_idx);
                if k_used != 0 && k_avail == guest_avail {
                    // Fully drained — dump avail ring entries to see descriptor IDs
                    let qsize = q.size();
                    let mut ids = Vec::new();
                    for slot in 0..qsize.min(16) {
                        let gpa = q.avail_addr() + 4 + u64::from(slot) * 2;
                        if let Some(id) = self
                            .physmap
                            .lookup(gpa, 2)
                            .and_then(|s| s.read::<u16>().ok())
                        {
                            ids.push(id);
                        }
                    }
                    eprintln!(
                        "[virtio-export] q={} DRAINED avail_ring[0..15]={:?}",
                        idx, ids
                    );
                }
            }
        }
        result
    }

    /// Rebuild the transport from an export, in the order the
    /// module doc gives.
    pub(super) fn restore_migrate_queues(
        &self,
        states: &[vmm_devices::lifecycle::VirtioMigrateQueue],
    ) {
        // Group by queue_idx=0 starts, find group matching our queue count
        let my_nq = self.num_queues as usize;
        let matching: Vec<_> = {
            let mut groups: Vec<Vec<_>> = Vec::new();
            let mut cur: Vec<_> = Vec::new();
            for s in states {
                if s.queue_idx == 0 && !cur.is_empty() {
                    groups.push(std::mem::take(&mut cur));
                }
                cur.push(s.clone());
            }
            if !cur.is_empty() {
                groups.push(cur);
            }
            groups
                .into_iter()
                .find(|g| g.len() == my_nq)
                .unwrap_or_default()
        };

        if let Some(pci_state) =
            matching.iter().find_map(|qs| qs.pci_state.as_ref())
        {
            eprintln!(
                "[virtio-restore] restoring PCI: cmd={:#x} bars={:?}",
                pci_state.command, pci_state.bar_addrs
            );
            self.restore_migrate_pci_state(pci_state);
        } else {
            eprintln!(
                "[virtio-restore] NO pci_state in matching queues (nq={})",
                my_nq
            );
        }

        let mut vs = self.virtio_state.lock().expect("virtio lock");
        self.device.reset_all_rings();

        for qs in &matching {
            let idx = qs.queue_idx as usize;
            if idx >= vs.queues.len() {
                continue;
            }

            if vs.guest_features == 0 && qs.features != 0 {
                vs.guest_features = qs.features;
                self.device.set_features(qs.features);
                // Propagate ring feature flags to all queues — the
                // normal guest write path does this in the feature
                // register handler, but restore bypasses it. Without
                // this, indirect_supported stays false and
                // collect_chain() silently drops every indirect
                // descriptor request (all Linux virtio-blk I/O).
                let event_idx =
                    qs.features & bits::VIRTIO_F_RING_EVENT_IDX != 0;
                let indirect =
                    qs.features & bits::VIRTIO_F_RING_INDIRECT_DESC != 0;
                for q in &mut vs.queues {
                    q.set_event_idx(event_idx);
                    q.set_indirect_supported(indirect);
                }
            }

            vs.queues[idx].set_addr_modern(
                qs.desc_addr,
                qs.avail_addr,
                qs.used_addr,
            );
            // Restore ring indices for software-emulated devices.
            // set_addr_modern resets indices to 0, but after migration
            // we need to continue from the source's position.
            vs.queues[idx].set_last_avail_idx(qs.avail_idx);
            vs.queues[idx].set_shadow_used_idx(qs.used_idx);
            eprintln!("[virtio-restore] q={} avail_idx={} used_idx={} addrs={:#x}/{:#x}/{:#x}",
                qs.queue_idx, qs.avail_idx, qs.used_idx,
                qs.desc_addr, qs.avail_addr, qs.used_addr);
            if idx < vs.queue_desc.len() {
                vs.queue_desc[idx] = qs.desc_addr;
                vs.queue_avail[idx] = qs.avail_addr;
                vs.queue_used[idx] = qs.used_addr;
            }
            if idx < vs.queue_enabled.len() {
                vs.queue_enabled[idx] = qs.enabled;
            }

            // Restore MSI-X vector mapping for this queue
            if qs.msix_vector != bits::VIRTIO_MSI_NO_VECTOR {
                if idx < vs.queue_msix_vectors.len() {
                    vs.queue_msix_vectors[idx] = qs.msix_vector;
                }
                if let Some(a) = self.msix_queue_vectors.get(idx) {
                    a.store(
                        qs.msix_vector,
                        std::sync::atomic::Ordering::Release,
                    );
                }
                // Write the MSI-X table entry so fire() can inject the interrupt
                if let Some(ref msix) = self.msix {
                    msix.write_entry(
                        qs.msix_vector,
                        qs.msix_addr,
                        qs.msix_data as u64,
                    );
                }
            }

            if qs.enabled {
                // Log ring state for debugging
                let guest_avail_gpa = qs.avail_addr + 2;
                let guest_used_gpa = qs.used_addr + 2;
                let ga = self
                    .physmap
                    .lookup(guest_avail_gpa, 2)
                    .and_then(|s| s.read::<u16>().ok())
                    .unwrap_or(0);
                let gu = self
                    .physmap
                    .lookup(guest_used_gpa, 2)
                    .and_then(|s| s.read::<u16>().ok())
                    .unwrap_or(0);
                let avail_delta = ga.wrapping_sub(qs.avail_idx);
                eprintln!(
                    "[virtio-restore] q={} kernel(avail={} used={}) guest_mem(avail={} used={}) available={}",
                    qs.queue_idx, qs.avail_idx, qs.used_idx, ga, gu, avail_delta,
                );

                self.device.restore_ring_state(
                    qs.queue_idx,
                    qs.queue_size,
                    qs.desc_addr,
                    qs.avail_addr,
                    qs.used_addr,
                    qs.avail_idx,
                    qs.used_idx,
                    qs.msix_addr,
                    qs.msix_data,
                );
            }
        }

        if !matching.is_empty() {
            vs.status = bits::STATUS_DRIVER_OK
                | bits::STATUS_FEATURES_OK
                | bits::STATUS_DRIVER
                | bits::STATUS_ACKNOWLEDGE;

            // Enable MSI-X so fire() can deliver interrupts
            if let Some(ref msix) = self.msix {
                msix.set_enabled(true);
            }
        }

        // Program notification addresses from current BAR state
        let pci = self.pci_state.lock().expect("pci lock");
        let bar0_port = pci
            .bars()
            .get(BarN::BAR0)
            .and_then(|(def, addr)| {
                if def.is_pio() {
                    Some(addr as u16)
                } else {
                    None
                }
            })
            .unwrap_or(0);
        let bar2_addr = pci
            .bars()
            .get(BarN::BAR2)
            .and_then(
                |(def, addr)| if def.is_mmio() { Some(addr) } else { None },
            )
            .unwrap_or(0);
        drop(pci);
        eprintln!(
            "[virtio-restore] BAR0={:#x} BAR2={:#x}",
            bar0_port, bar2_addr
        );

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

        // Start the deferred interrupt poll thread now that rings,
        // MSI-X vectors, and notification addresses are all programmed.
        self.device.start_poll_deferred();

        // Inject a post-restore interrupt on each queue to wake the
        // guest driver. After migration, the rx ring may have no
        // available buffers (avail_idx == used_idx). The guest driver
        // needs an interrupt to process the "caught up" used ring and
        // post new rx buffers. Without this, incoming packets are
        // dropped with "no_space" in the kernel viona driver.
        drop(vs);
        for qs in &matching {
            if qs.enabled {
                eprintln!(
                    "[virtio-restore] injecting post-restore interrupt q={}",
                    qs.queue_idx
                );
                self.raise_queue_interrupt_current(qs.queue_idx);
            }
        }
    }
}
