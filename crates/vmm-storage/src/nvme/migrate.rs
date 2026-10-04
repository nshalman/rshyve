// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Controller state a live migration carries.

use vmm_devices::lifecycle::{
    DeviceMigrateState, DeviceStateError, NvmeMigrateCq, NvmeMigrateSq,
    NvmeMigrateState,
};
use vmm_devices::pci::device::replay_migrate_pci_state;

use super::bits::*;
use super::queues::{CompQueue, NvmeState, SubQueue};
use super::NvmeController;

impl NvmeController {
    /// The controller's state, once every command the guest submitted
    /// has finished.
    ///
    /// A command in a backend has left its SQ and has no completion in
    /// a CQ, so a payload taken around it loses it for good.
    pub(super) fn export_state(
        &self,
    ) -> Result<Option<DeviceMigrateState>, DeviceStateError> {
        let st = self.state.lock().expect("nvme: state lock");
        let (mut st, drained) = self.drain_all(st, self.fences.reset);
        let state = if drained {
            Ok(self.snapshot(&st))
        } else {
            Err(DeviceStateError::Export(format!(
                "{} commands still in flight",
                st.in_flight()
            )))
        };
        self.reopen(&mut st);
        state
    }

    fn snapshot(&self, st: &NvmeState) -> Option<DeviceMigrateState> {
        // A disabled controller has no queues, so there is nothing for
        // the destination to put back.
        if (st.cc & CC_EN) == 0 {
            return None;
        }

        let wire_sq = |sq: &SubQueue| NvmeMigrateSq {
            base: sq.base,
            size: sq.size,
            head: sq.head,
            tail: sq.tail,
            cq_id: sq.cq_id,
        };
        let wire_cq = |cq: &CompQueue| NvmeMigrateCq {
            base: cq.base,
            size: cq.size,
            head: cq.head,
            tail: cq.tail,
            phase: cq.phase,
            iv: cq.iv,
            ien: cq.ien,
        };

        slog::info!(self.log, "nvme: exporting controller state";
            "cc" => format!("{:#x}", st.cc),
            "csts" => format!("{:#x}", st.csts),
            "aqa" => format!("{:#x}", st.aqa),
            "io_queues" => st.num_io_queues,
            "msix_enabled" => self.msix.is_enabled());

        Some(DeviceMigrateState::Nvme(NvmeMigrateState {
            cc: st.cc,
            csts: st.csts,
            aqa: st.aqa,
            asq_base: st.asq_base,
            acq_base: st.acq_base,
            admin_sq: st.admin_sq.as_ref().map(wire_sq),
            admin_cq: st.admin_cq.as_ref().map(wire_cq),
            io_sqs: st
                .io_sqs
                .iter()
                .map(|sq| sq.as_ref().map(wire_sq))
                .collect(),
            io_cqs: st
                .io_cqs
                .iter()
                .map(|cq| cq.as_ref().map(wire_cq))
                .collect(),
            num_io_queues: st.num_io_queues,
            pci: self.pci_state.lock().expect("pci lock").migrate_state(),
            msix: self.msix.export_state(),
        }))
    }

    pub(super) fn restore_state(
        &self,
        state: &DeviceMigrateState,
    ) -> Result<(), DeviceStateError> {
        let DeviceMigrateState::Nvme(mig) = state else {
            return Err(DeviceStateError::WrongKind {
                want: "nvme",
                got: state.kind(),
            });
        };
        // Every field is checked before any of it is applied. The
        // doorbell handlers divide by a queue size and index a ring by
        // head and tail, so a payload the checks let through would
        // abort the VMM on the guest's first doorbell write.
        check_nvme_state(mig)?;
        {
            let st = self.state.lock().expect("nvme: state lock");
            if mig.num_io_queues > st.max_io_queues {
                return Err(DeviceStateError::invalid(format!(
                    "num_io_queues {} is past the {} this backend serves",
                    mig.num_io_queues, st.max_io_queues,
                )));
            }
            // Their completions would be matched against queues the
            // import is about to replace.
            if st.in_flight() > 0 {
                return Err(DeviceStateError::invalid(
                    "the controller has commands in flight",
                ));
            }
        }

        slog::info!(self.log, "nvme: restoring controller state";
            "cc" => format!("{:#x}", mig.cc),
            "csts" => format!("{:#x}", mig.csts),
            "aqa" => format!("{:#x}", mig.aqa),
            "io_queues" => mig.num_io_queues);

        let bar_writes = self
            .pci_state
            .lock()
            .expect("pci lock")
            .bar_replay_writes(&mig.pci.bar_addrs);
        replay_migrate_pci_state(self, &bar_writes, mig.pci.command);

        let mut st = self.state.lock().expect("nvme: state lock");
        st.cc = mig.cc;
        st.csts = mig.csts;
        st.aqa = mig.aqa;
        st.asq_base = mig.asq_base;
        st.acq_base = mig.acq_base;
        st.num_io_queues = mig.num_io_queues;

        // One fresh instance for every imported queue: nothing from
        // before the import may complete into one.
        let instance = st.next_instance();

        let live_sq = |sq: &NvmeMigrateSq| SubQueue {
            base: sq.base,
            size: sq.size,
            head: sq.head,
            tail: sq.tail,
            cq_id: sq.cq_id,
            instance,
        };
        let live_cq = |cq: &NvmeMigrateCq| CompQueue {
            base: cq.base,
            size: cq.size,
            head: cq.head,
            tail: cq.tail,
            phase: cq.phase,
            iv: cq.iv,
            ien: cq.ien,
            instance,
        };

        st.retire_queues();
        st.admin_sq = mig.admin_sq.as_ref().map(live_sq);
        st.admin_cq = mig.admin_cq.as_ref().map(live_cq);
        for (idx, wire) in mig.io_sqs.iter().enumerate() {
            st.set_sq(idx, wire.as_ref().map(live_sq));
        }
        for (idx, wire) in mig.io_cqs.iter().enumerate() {
            st.set_cq(idx, wire.as_ref().map(live_cq));
        }
        drop(st);

        self.msix.import_state(&mig.msix).map_err(|error| {
            DeviceStateError::invalid(format!("MSI-X table: {error}"))
        })
    }
}

/// Everything a controller payload must satisfy before any of it is
/// applied, matching what the guest-facing create-queue commands
/// enforce.
pub(super) fn check_nvme_state(
    mig: &NvmeMigrateState,
) -> Result<(), DeviceStateError> {
    use super::queues::{io_queue_index, valid_queue_base};

    fn check_size(what: &str, size: u16) -> Result<(), DeviceStateError> {
        // The doorbell handlers take `% size`, so 0 divides by zero and
        // aborts a VMM built with panic = "abort".
        if !(2..=MAX_QUEUE_SIZE).contains(&size) {
            return Err(DeviceStateError::invalid(format!(
                "{what} size {size} is outside 2..={MAX_QUEUE_SIZE}",
            )));
        }
        Ok(())
    }

    fn check_cursors(
        what: &str,
        size: u16,
        head: u16,
        tail: u16,
    ) -> Result<(), DeviceStateError> {
        if head >= size || tail >= size {
            return Err(DeviceStateError::invalid(format!(
                "{what} head {head} / tail {tail} outside a {size}-entry ring",
            )));
        }
        Ok(())
    }

    fn check_base(what: &str, base: u64) -> Result<(), DeviceStateError> {
        if !valid_queue_base(base) {
            return Err(DeviceStateError::invalid(format!(
                "{what} base {base:#x} is zero or not page-aligned",
            )));
        }
        Ok(())
    }

    fn check_vector(what: &str, iv: u16) -> Result<(), DeviceStateError> {
        // `MsixTable::fire` drops a vector the table does not hold, so
        // the queue would never raise and its I/O would hang. Create
        // I/O Completion Queue refuses the same value.
        if iv >= NVME_MSIX_COUNT {
            return Err(DeviceStateError::invalid(format!(
                "{what} interrupt vector {iv} is outside \
                 0..{NVME_MSIX_COUNT}",
            )));
        }
        Ok(())
    }

    let check_cq =
        |what: &str, cq: &NvmeMigrateCq| -> Result<(), DeviceStateError> {
            check_size(what, cq.size)?;
            check_cursors(what, cq.size, cq.head, cq.tail)?;
            check_vector(what, cq.iv)?;
            check_base(what, cq.base)
        };

    if !(1..=MAX_IO_QUEUES as u16).contains(&mig.num_io_queues) {
        return Err(DeviceStateError::invalid(format!(
            "num_io_queues {} is outside 1..={MAX_IO_QUEUES}",
            mig.num_io_queues,
        )));
    }
    if mig.io_sqs.len() > MAX_IO_QUEUES || mig.io_cqs.len() > MAX_IO_QUEUES {
        return Err(DeviceStateError::invalid(format!(
            "payload carries {} SQs and {} CQs, this controller has \
             {MAX_IO_QUEUES} of each",
            mig.io_sqs.len(),
            mig.io_cqs.len(),
        )));
    }

    // Create I/O Queue refuses an id past the count Set Features
    // granted, so no payload from a real source carries one.
    fn used<T>(slots: &[Option<T>]) -> usize {
        slots.iter().rposition(Option::is_some).map_or(0, |i| i + 1)
    }
    let count = usize::from(mig.num_io_queues);
    if used(&mig.io_sqs) > count || used(&mig.io_cqs) > count {
        return Err(DeviceStateError::invalid(format!(
            "a queue id is past num_io_queues {}",
            mig.num_io_queues,
        )));
    }

    if let Some(cq) = &mig.admin_cq {
        check_cq("admin CQ", cq)?;
    }
    for (idx, cq) in mig.io_cqs.iter().enumerate() {
        if let Some(cq) = cq {
            check_cq(&format!("IO CQ {}", idx + 1), cq)?;
        }
    }

    // A submission queue names the completion queue its results go to.
    // An SQ whose CQ does not exist would post completions through a
    // `None` slot, or into another queue's ring.
    let cq_exists = |cq_id: u16| -> bool {
        if cq_id == 0 {
            return mig.admin_cq.is_some();
        }
        io_queue_index(cq_id)
            .and_then(|idx| mig.io_cqs.get(idx))
            .is_some_and(|slot| slot.is_some())
    };
    let check_sq =
        |what: &str, sq: &NvmeMigrateSq| -> Result<(), DeviceStateError> {
            check_size(what, sq.size)?;
            check_cursors(what, sq.size, sq.head, sq.tail)?;
            check_base(what, sq.base)?;
            if !cq_exists(sq.cq_id) {
                return Err(DeviceStateError::invalid(format!(
                    "{what} posts to CQ {}, which the payload does not create",
                    sq.cq_id,
                )));
            }
            Ok(())
        };

    if let Some(sq) = &mig.admin_sq {
        check_sq("admin SQ", sq)?;
    }
    for (idx, sq) in mig.io_sqs.iter().enumerate() {
        if let Some(sq) = sq {
            check_sq(&format!("IO SQ {}", idx + 1), sq)?;
        }
    }
    Ok(())
}
