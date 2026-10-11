// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Controller state a live migration carries.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;

use vmm_devices::lifecycle::{
    DeviceMigrateState, DeviceStateError, NvmeMigrateBackend, NvmeMigrateCq,
    NvmeMigrateCqe, NvmeMigrateNamespace, NvmeMigrateQueueCounts,
    NvmeMigrateSq, NvmeMigrateState,
};
use vmm_devices::pci::device::replay_migrate_pci_state;
use vmm_devices::pci::msix::MsixImportError;
use vmm_devices::Lifecycle;

use super::bits::*;
use super::cq::{Cqe, Held};
use super::queues::{io_queue_index, CompQueue, NvmeState, SubQueue};
use super::NvmeController;

impl NvmeController {
    /// Drains first: a command in a backend has left its SQ and has no
    /// CQE yet, so a payload taken around it would lose it.
    pub(super) fn export_state(
        &self,
    ) -> Result<Option<DeviceMigrateState>, DeviceStateError> {
        // Before the enabled check: a disabled controller exports
        // nothing, and the source would still hold the attachment.
        if let Some(why) = self.migration_blocker() {
            return Err(DeviceStateError::Export(why));
        }
        let st = self.lock();
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
            held: cq
                .held
                .iter()
                .map(|held| NvmeMigrateCqe {
                    sqid: held.cqe.sqid,
                    cid: held.cqe.cid,
                    status: held.cqe.status,
                    cdw0: held.cqe.cdw0,
                })
                .collect(),
        };

        slog::info!(self.log, "nvme: exporting controller state";
            "cc" => format!("{:#x}", st.cc),
            "csts" => format!("{:#x}", st.csts),
            "aqa" => format!("{:#x}", st.aqa),
            "io_sqs" => st.num_io_sqs,
            "io_cqs" => st.num_io_cqs,
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
            num_io_queues: st.num_io_sqs.max(st.num_io_cqs),
            io_queue_counts: Some(NvmeMigrateQueueCounts {
                sqs: st.num_io_sqs,
                cqs: st.num_io_cqs,
            }),
            pci: self.pci_state.lock().expect("pci lock").migrate_state(),
            msix: self.msix.export_state(),
            namespace: Some(self.namespace()),
            backend: Some(self.backend.identity()),
            aers_held: st.aers_held,
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
        self.msix
            .check_import_state(&mig.msix)
            .map_err(msix_error)?;
        if let Some(why) = self.migration_blocker() {
            return Err(DeviceStateError::Invalid(why));
        }
        let source = mig.backend.clone().unwrap_or(NvmeMigrateBackend::File);
        let local = self.backend.identity();
        if source != local {
            return Err(DeviceStateError::invalid(format!(
                "the source disk is on {source}, this one on {local}",
            )));
        }
        let local = self.namespace();
        // A v2 source carried no capacity or write protection, so only
        // its one shape can be checked.
        let source = mig.namespace.unwrap_or(NvmeMigrateNamespace {
            block_size: 512,
            volatile_write_cache: true,
            ..local
        });
        if source != local {
            return Err(DeviceStateError::invalid(format!(
                "the source namespace is {source:?}, this one {local:?}",
            )));
        }
        // A held completion keeps its command's tag, and this backend
        // has only so many per queue.
        let held = held_per_sq(mig);
        if let Some(sqid) = held
            .iter()
            .skip(1)
            .position(|&n| n > usize::from(self.depth))
        {
            return Err(DeviceStateError::invalid(format!(
                "SQ {} holds more completions than its depth {}",
                sqid + 1,
                self.depth,
            )));
        }

        // Checked and applied under one hold, so no command is admitted
        // between the two.
        let mut st = self.lock();
        let (sqs, cqs) = granted_counts(mig);
        if sqs.max(cqs) > st.max_io_queues {
            return Err(DeviceStateError::invalid(format!(
                "{sqs} SQs and {cqs} CQs are past the {} this backend serves",
                st.max_io_queues,
            )));
        }
        // Their completions would be matched against queues the import
        // is about to replace.
        if st.in_flight() > 0 {
            return Err(DeviceStateError::invalid(
                "the controller has commands in flight",
            ));
        }

        slog::info!(self.log, "nvme: restoring controller state";
            "cc" => format!("{:#x}", mig.cc),
            "csts" => format!("{:#x}", mig.csts),
            "aqa" => format!("{:#x}", mig.aqa),
            "io_sqs" => sqs,
            "io_cqs" => cqs);

        st.cc = mig.cc;
        st.csts = mig.csts;
        st.aqa = mig.aqa;
        st.asq_base = mig.asq_base;
        st.acq_base = mig.acq_base;
        st.num_io_sqs = sqs;
        st.num_io_cqs = cqs;

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
            held: VecDeque::new(),
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
        self.restore_held(&mut st, mig);
        st.aers_held = mig.aers_held;
        // Nothing runs before the source has committed to never running
        // the guest again: the two would share the disk. Resume opens
        // admission and takes the commands the guest left queued.
        st.admission.paused = true;
        self.counts.paused.store(true, Ordering::Release);
        drop(st);

        // After the checks, so a refused payload changes nothing.
        let bar_writes = self
            .pci_state
            .lock()
            .expect("pci lock")
            .bar_replay_writes(&mig.pci.bar_addrs);
        replay_migrate_pci_state(self, &bar_writes, mig.pci.command);

        self.msix.import_state(&mig.msix).map_err(msix_error)
    }
}

fn msix_error(error: MsixImportError) -> DeviceStateError {
    DeviceStateError::invalid(format!("MSI-X table: {error}"))
}

impl NvmeController {
    fn namespace(&self) -> NvmeMigrateNamespace {
        NvmeMigrateNamespace {
            block_size: self.geometry.block_size,
            volatile_write_cache: self.config.volatile_write_cache,
            total_blocks: self.geometry.total_blocks,
            read_only: self.read_only,
        }
    }

    /// Put back the completions the source was holding, each with a free
    /// tag in its SQ. The payload passed its checks.
    fn restore_held(&self, st: &mut NvmeState, mig: &NvmeMigrateState) {
        for (cq_id, wire) in wire_cqs(mig) {
            for held in &wire.held {
                let tag = io_queue_index(held.sqid)
                    .and_then(|idx| st.inflight[idx].reserve_held(self.depth));
                let cq = match io_queue_index(cq_id) {
                    None => st.admin_cq.as_mut(),
                    Some(idx) => st.io_cqs[idx].as_mut(),
                };
                if let Some(cq) = cq {
                    cq.held.push_back(Held {
                        cqe: Cqe {
                            cdw0: held.cdw0,
                            sqid: held.sqid,
                            cid: held.cid,
                            status: held.status,
                        },
                        tag,
                    });
                }
            }
        }
    }
}

/// Every CQ in a payload with its queue id, the admin CQ first.
fn wire_cqs(
    mig: &NvmeMigrateState,
) -> impl Iterator<Item = (u16, &NvmeMigrateCq)> {
    let admin = mig.admin_cq.as_ref().map(|cq| (ADMIN_QUEUE_ID, cq));
    let io = mig
        .io_cqs
        .iter()
        .enumerate()
        .filter_map(|(idx, cq)| cq.as_ref().map(|cq| (idx as u16 + 1, cq)));
    admin.into_iter().chain(io)
}

/// Held completions per SQ id, the admin SQ at 0. An id past the last
/// I/O queue is counted nowhere; [`check_held`] refuses it.
fn held_per_sq(mig: &NvmeMigrateState) -> [usize; MAX_IO_QUEUES + 1] {
    let mut count = [0usize; MAX_IO_QUEUES + 1];
    for (_, cq) in wire_cqs(mig) {
        for held in &cq.held {
            if let Some(n) = count.get_mut(usize::from(held.sqid)) {
                *n += 1;
            }
        }
    }
    count
}

/// Each held completion names an SQ that posts to its CQ, and has a
/// status a CQE can carry beside the phase bit.
fn check_held(mig: &NvmeMigrateState) -> Result<(), DeviceStateError> {
    let posts_to = |sqid: u16, cq_id: u16| -> bool {
        if sqid == ADMIN_QUEUE_ID {
            return cq_id == ADMIN_QUEUE_ID && mig.admin_sq.is_some();
        }
        io_queue_index(sqid)
            .and_then(|idx| mig.io_sqs.get(idx))
            .and_then(Option::as_ref)
            .is_some_and(|sq| sq.cq_id == cq_id)
    };
    for (cq_id, cq) in wire_cqs(mig) {
        for held in &cq.held {
            if !posts_to(held.sqid, cq_id) {
                return Err(DeviceStateError::invalid(format!(
                    "CQ {cq_id} holds a completion for SQ {}, which does \
                     not post to it",
                    held.sqid,
                )));
            }
            if held.status > CQE_STATUS_MAX {
                return Err(DeviceStateError::invalid(format!(
                    "held status {:#x} does not fit a CQE",
                    held.status,
                )));
            }
        }
    }
    if held_per_sq(mig)
        .iter()
        .any(|&n| n > usize::from(MAX_QUEUE_SIZE))
    {
        return Err(DeviceStateError::invalid(format!(
            "an SQ holds more than {MAX_QUEUE_SIZE} completions",
        )));
    }
    Ok(())
}

/// Everything a controller payload must satisfy before any of it is
/// applied, matching what the guest-facing create-queue commands
/// enforce.
pub(super) fn check_nvme_state(
    mig: &NvmeMigrateState,
) -> Result<(), DeviceStateError> {
    use super::queues::valid_queue_base;

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
    let (sqs, cqs) = granted_counts(mig);
    for (what, count) in [("SQ", sqs), ("CQ", cqs)] {
        if !(1..=MAX_IO_QUEUES as u16).contains(&count) {
            return Err(DeviceStateError::invalid(format!(
                "the {what} count {count} is outside 1..={MAX_IO_QUEUES}",
            )));
        }
    }
    if used(&mig.io_sqs) > usize::from(sqs)
        || used(&mig.io_cqs) > usize::from(cqs)
    {
        return Err(DeviceStateError::invalid(format!(
            "a queue id is past the {sqs} SQs and {cqs} CQs granted",
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
    // The admin SQ posts to the admin CQ and an I/O SQ to an I/O CQ:
    // Create I/O SQ refuses CQ 0, and an I/O completion in the admin
    // ring would be read as an admin one.
    let check_sq = |what: &str,
                    sq: &NvmeMigrateSq,
                    admin: bool|
     -> Result<(), DeviceStateError> {
        check_size(what, sq.size)?;
        check_cursors(what, sq.size, sq.head, sq.tail)?;
        check_base(what, sq.base)?;
        if (sq.cq_id == ADMIN_QUEUE_ID) != admin {
            return Err(DeviceStateError::invalid(format!(
                "{what} posts to CQ {}, a queue of the other kind",
                sq.cq_id,
            )));
        }
        if !cq_exists(sq.cq_id) {
            return Err(DeviceStateError::invalid(format!(
                "{what} posts to CQ {}, which the payload does not create",
                sq.cq_id,
            )));
        }
        Ok(())
    };

    if let Some(sq) = &mig.admin_sq {
        check_sq("admin SQ", sq, true)?;
    }
    for (idx, sq) in mig.io_sqs.iter().enumerate() {
        if let Some(sq) = sq {
            check_sq(&format!("IO SQ {}", idx + 1), sq, false)?;
        }
    }
    check_held(mig)?;
    check_aers(mig)
}

/// No more held AERs than AERL lets a driver send, and none without an
/// admin queue to have sent them on.
fn check_aers(mig: &NvmeMigrateState) -> Result<(), DeviceStateError> {
    let limit = AER_LIMIT + 1;
    if mig.aers_held > limit {
        return Err(DeviceStateError::invalid(format!(
            "{} held AERs, the limit is {limit}",
            mig.aers_held,
        )));
    }
    if mig.aers_held > 0 && mig.admin_sq.is_none() {
        return Err(DeviceStateError::invalid(
            "held AERs with no admin SQ to have sent them",
        ));
    }
    Ok(())
}

/// The granted (SQ, CQ) counts. A source without separate counts kept
/// one for both.
fn granted_counts(mig: &NvmeMigrateState) -> (u16, u16) {
    mig.io_queue_counts
        .map_or((mig.num_io_queues, mig.num_io_queues), |c| (c.sqs, c.cqs))
}
