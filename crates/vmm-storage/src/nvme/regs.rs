// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! BAR0 register and doorbell decoding, BAR4 MSI-X window.

use std::sync::Arc;

use vmm_core::common::RWOp;
use vmm_core::mmio::MmioFn;

use vmm_devices::pci::device::PciDevice;
use vmm_devices::pci::BarN;

use super::bits::*;
use super::queues::{io_queue_index, CompQueue, NvmeState, SubQueue};
use super::NvmeController;

// ---------------------------------------------------------------------------
// BAR0 handling: controller registers + doorbells
// ---------------------------------------------------------------------------

impl NvmeController {
    /// Unregister a recorded MMIO base and clear the record.
    pub(super) fn release_mmio(&self, bar: BarN, reg: &mut Option<u64>) {
        if let Some(addr) = reg.take() {
            if let Err(e) = self.bus_mmio.unregister(addr) {
                slog::warn!(self.log, "nvme: MMIO unregister failed";
                    "bar" => ?bar,
                    "addr" => format!("{addr:#x}"),
                    "error" => %e);
            }
        }
    }

    /// Update MMIO bus registration for BAR0 and BAR4.
    pub(super) fn update_mmio_registration(self: &Arc<Self>) {
        let pci = self.pci_state.lock().expect("nvme: pci lock");
        let mmio_enabled = pci
            .command()
            .contains(vmm_devices::pci::bits::RegCmd::MMIO_EN);

        // Register both BAR0 and BAR4 on the MMIO bus
        for (bar, reg_mutex) in [
            (BarN::BAR0, &self.registered_bar0),
            (BarN::BAR4, &self.registered_bar4),
        ] {
            let mut reg = reg_mutex.lock().expect("bar reg lock");
            self.release_mmio(bar, &mut reg);
            if !mmio_enabled {
                continue;
            }
            let Some((def, addr)) = pci.bars().get(bar) else {
                continue;
            };
            if addr == 0 || !def.is_mmio() {
                continue;
            }
            let size = def.size();
            let dev = Arc::clone(self);
            let handler: Arc<MmioFn> =
                Arc::new(move |offset: usize, rwo: RWOp<'_>| {
                    dev.bar_rw(bar, offset, rwo);
                });
            match self.bus_mmio.register(addr, size, handler) {
                Ok(()) => *reg = Some(addr),
                // The controller now decodes nothing on this BAR.
                Err(e) => {
                    slog::error!(self.log,
                        "nvme: MMIO register failed, BAR dark";
                        "bar" => ?bar,
                        "addr" => format!("{addr:#x}"),
                        "size" => size,
                        "error" => %e);
                }
            }
        }
    }

    /// Handle BAR0 MMIO access (controller registers and doorbells).
    pub(super) fn bar0_rw(&self, offset: usize, rwo: RWOp<'_>) {
        if offset < REG_DOORBELL_BASE {
            self.reg_rw(offset, rwo);
        } else {
            self.doorbell_rw(offset, rwo);
        }
    }

    /// Handle controller register reads/writes (offsets 0x00..0x38).
    fn reg_rw(&self, offset: usize, rwo: RWOp<'_>) {
        let mut st = self.state.lock().expect("nvme: state lock");
        match rwo {
            RWOp::Read(ro) => {
                let val = match offset {
                    REG_CAP => {
                        // 8-byte register; handle 4-byte aligned reads
                        if ro.len() == 8 {
                            ro.write_u64(st.cap);
                            return;
                        }
                        st.cap as u32
                    }
                    0x04 => (st.cap >> 32) as u32, // CAP high dword
                    REG_VS => NVME_VS_1_0,
                    REG_INTMS => 0, // unused with MSI-X
                    REG_INTMC => 0,
                    REG_CC => st.cc,
                    REG_CSTS => st.csts,
                    REG_AQA => st.aqa,
                    REG_ASQ => {
                        if ro.len() == 8 {
                            ro.write_u64(st.asq_base);
                            return;
                        }
                        st.asq_base as u32
                    }
                    0x2C => (st.asq_base >> 32) as u32,
                    REG_ACQ => {
                        if ro.len() == 8 {
                            ro.write_u64(st.acq_base);
                            return;
                        }
                        st.acq_base as u32
                    }
                    0x34 => (st.acq_base >> 32) as u32,
                    _ => 0,
                };
                if ro.len() == 8 {
                    ro.write_u64(u64::from(val));
                } else {
                    ro.write_dword_at(val, offset);
                }
            }
            RWOp::Write(wo) => {
                let val32 = wo.read_u32();
                match offset {
                    REG_CC => {
                        let old_cc = st.cc;
                        st.cc = val32;
                        self.handle_cc_write(&mut st, old_cc, val32);
                    }
                    REG_AQA if (st.cc & CC_EN) == 0 => st.aqa = val32,
                    REG_ASQ if (st.cc & CC_EN) == 0 => {
                        if wo.len() == 8 {
                            st.asq_base = wo.read_u64();
                        } else {
                            st.asq_base = (st.asq_base & 0xFFFF_FFFF_0000_0000)
                                | u64::from(val32);
                        }
                    }
                    0x2C if (st.cc & CC_EN) == 0 => {
                        st.asq_base = (st.asq_base & 0x0000_0000_FFFF_FFFF)
                            | (u64::from(val32) << 32);
                    }
                    REG_ACQ if (st.cc & CC_EN) == 0 => {
                        if wo.len() == 8 {
                            st.acq_base = wo.read_u64();
                        } else {
                            st.acq_base = (st.acq_base & 0xFFFF_FFFF_0000_0000)
                                | u64::from(val32);
                        }
                    }
                    0x34 if (st.cc & CC_EN) == 0 => {
                        st.acq_base = (st.acq_base & 0x0000_0000_FFFF_FFFF)
                            | (u64::from(val32) << 32);
                    }
                    // INTMS, INTMC: ignored (MSI-X mode)
                    _ => {}
                }
            }
        }
    }

    /// Handle CC register write transitions (enable/disable/shutdown).
    fn handle_cc_write(&self, st: &mut NvmeState, old_cc: u32, new_cc: u32) {
        let was_enabled = (old_cc & CC_EN) != 0;
        let now_enabled = (new_cc & CC_EN) != 0;

        if !was_enabled && now_enabled {
            // Controller enable: set up admin queues
            let asqs = (st.aqa & AQA_ASQS_MASK) + 1;
            let acqs = ((st.aqa >> AQA_ACQS_SHIFT) & AQA_ACQS_MASK) + 1;

            st.admin_sq = Some(SubQueue {
                base: st.asq_base,
                size: asqs as u16,
                head: 0,
                tail: 0,
                cq_id: 0,
            });
            st.admin_cq = Some(CompQueue {
                base: st.acq_base,
                size: acqs as u16,
                head: 0,
                tail: 0,
                phase: true,
                iv: 0,
            });

            st.csts |= CSTS_RDY;
        } else if was_enabled && !now_enabled {
            // Controller disable: tear down all queues
            st.admin_sq = None;
            st.admin_cq = None;
            for sq in &mut st.io_sqs {
                *sq = None;
            }
            for cq in &mut st.io_cqs {
                *cq = None;
            }
            st.csts &= !CSTS_RDY;
        }

        // Handle shutdown notification
        let shn = (new_cc >> CC_SHN_SHIFT) & CC_SHN_MASK;
        if shn != SHST_NORMAL {
            // Acknowledge shutdown complete
            st.csts = (st.csts & !(CSTS_SHST_MASK << CSTS_SHST_SHIFT))
                | (SHST_COMPLETE << CSTS_SHST_SHIFT);
        }
    }

    /// Handle doorbell writes (offsets 0x1000+).
    ///
    /// Doorbell layout (DSTRD=0, 4-byte stride):
    /// - 0x1000 + (2*qid + 0) * 4 = SQ tail doorbell for queue qid
    /// - 0x1000 + (2*qid + 1) * 4 = CQ head doorbell for queue qid
    #[inline]
    fn doorbell_rw(&self, offset: usize, rwo: RWOp<'_>) {
        // Doorbells are write-only; reads return 0
        let wo = match rwo {
            RWOp::Write(wo) => wo,
            RWOp::Read(ro) => {
                ro.write_u32(0);
                return;
            }
        };

        let db_offset = offset - REG_DOORBELL_BASE;
        let db_index = db_offset / 4;
        let qid = (db_index / 2) as u16;
        let is_sq = db_index.is_multiple_of(2);
        let new_val = wo.read_u32() as u16;

        if is_sq {
            self.sq_doorbell(qid, new_val);
        } else {
            self.cq_doorbell(qid, new_val);
        }
    }

    /// Handle SQ tail doorbell write: process new submissions.
    #[inline]
    fn sq_doorbell(&self, qid: u16, new_tail: u16) {
        if qid == ADMIN_QUEUE_ID {
            self.admin_sq_doorbell(new_tail);
        } else {
            self.io_sq_doorbell(qid, new_tail);
        }
    }

    /// Handle CQ head doorbell write: update head pointer.
    #[inline]
    fn cq_doorbell(&self, qid: u16, new_head: u16) {
        let mut st = self.state.lock().expect("nvme: state lock");
        if qid == ADMIN_QUEUE_ID {
            if let Some(cq) = &mut st.admin_cq {
                if new_head < cq.size {
                    cq.head = new_head;
                }
            }
        } else {
            if let Some(idx) = io_queue_index(qid) {
                if let Some(cq) = &mut st.io_cqs[idx] {
                    if new_head < cq.size {
                        cq.head = new_head;
                    }
                }
            }
        }
    }

    /// Handle BAR4 MMIO access (MSI-X table + PBA).
    pub(super) fn bar4_rw(&self, offset: usize, rwo: RWOp<'_>) {
        let pba_offset = self.msix.pba_offset();
        match rwo {
            RWOp::Read(ro) => {
                let val = if offset >= pba_offset {
                    self.msix.pba_read(offset - pba_offset)
                } else {
                    self.msix.table_read(offset)
                };
                ro.write_dword(val)
            }
            RWOp::Write(wo) => {
                let val = wo.read_dword();
                // The PBA is read-only to the guest.
                if offset < pba_offset {
                    self.msix.table_write(offset, val);
                }
            }
        }
    }
}
