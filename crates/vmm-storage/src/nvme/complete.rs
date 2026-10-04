// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Finishing commands, and the drains that wait for them.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use vmm_core::mem::MemCtx;

use super::backend::{BlockStatus, CompletionSink, RequestId};
use super::bits::*;
use super::cq::{post_or_hold, Cqe, Posted};
use super::inflight::Admitted;
use super::locked::Locked;
use super::queues::{io_queue_index, NvmeState};
use super::NvmeController;

/// The sink a backend completes into.
///
/// Weak, so a backend's threads never keep the controller, and with it
/// the backend, alive.
pub(super) struct Completions(pub(super) Weak<NvmeController>);

impl CompletionSink for Completions {
    fn complete(&self, done: &[(RequestId, BlockStatus)]) {
        if let Some(ctrl) = self.0.upgrade() {
            ctrl.complete(done);
        }
    }
}

/// Whether the queues a command was admitted on are still the ones
/// the driver has. A completion for a queue it deleted or reset would
/// land in a ring it has given to something else.
fn current(st: &NvmeState, idx: usize, cmd: &Admitted) -> bool {
    let sq_live = st.io_sqs[idx]
        .as_ref()
        .is_some_and(|sq| sq.instance == cmd.sq_instance);
    let cq_live = io_queue_index(cmd.cq_id)
        .and_then(|cq_idx| st.io_cqs[cq_idx].as_ref())
        .is_some_and(|cq| cq.instance == cmd.cq_instance);
    sq_live && cq_live
}

impl NvmeController {
    fn complete(&self, done: &[(RequestId, BlockStatus)]) {
        let mem = MemCtx::new(Arc::clone(&self.physmap));
        let mut st = self.lock();
        for &(id, status) in done {
            let Some(cmd) = io_queue_index(id.queue)
                .and_then(|idx| st.inflight[idx].finish(id.tag, id.serial))
            else {
                // A backend fault, not the guest's: the command it
                // meant to finish is still waiting.
                slog::warn!(self.log, "nvme: completion for no command";
                    "queue" => id.queue, "tag" => id.tag);
                continue;
            };
            self.post(&mut st, &mem, id, cmd, status.nvme(cmd.opcode));
        }
        self.after_finish(&mut st);
    }

    /// Finish a command the controller ends itself, before a backend
    /// sees it.
    pub(super) fn finish_now(
        &self,
        st: &mut NvmeState,
        mem: &MemCtx,
        id: RequestId,
        status: u16,
    ) {
        if let Some(cmd) = io_queue_index(id.queue)
            .and_then(|idx| st.inflight[idx].finish(id.tag, id.serial))
        {
            self.post(st, mem, id, cmd, status);
        }
    }

    /// Take a finished command out of flight and write its completion,
    /// unless its queues are gone.
    fn post(
        &self,
        st: &mut NvmeState,
        mem: &MemCtx,
        id: RequestId,
        cmd: Admitted,
        status: u16,
    ) {
        let Some(idx) = io_queue_index(id.queue) else {
            return;
        };
        let cq_idx = io_queue_index(cmd.cq_id);
        // The lock-free count drops when the lock is released, once
        // this command's CQE is written.
        st.retiring += 1;
        if st.drain_waiters > 0 {
            self.drained.notify_all();
        }

        let posted = match cq_idx {
            Some(cq_idx) if current(st, idx, &cmd) => {
                let sq_head = st.sq_head(id.queue);
                let cqe = Cqe {
                    cdw0: 0,
                    sqid: id.queue,
                    cid: cmd.cid,
                    status,
                };
                st.io_cqs[cq_idx].as_mut().map(|cq| {
                    post_or_hold(
                        mem,
                        &mut st.irqs,
                        cmd.cq_id,
                        cq,
                        cqe,
                        sq_head,
                        Some(id.tag),
                    )
                })
            }
            _ => None,
        };
        match posted {
            // The tag goes back when the guest gets the completion.
            Some(Ok(Posted::Held)) => {}
            Some(Err(error)) => {
                // The driver will never see this command finish.
                slog::error!(self.log, "nvme: I/O CQE write failed";
                    "error" => %error);
                st.csts |= CSTS_CFS;
                st.inflight[idx].release(id.tag);
            }
            Some(Ok(Posted::Written)) | None => {
                st.inflight[idx].release(id.tag);
            }
        }
    }

    /// The CQ head doorbell: the guest has consumed completions up to
    /// `new_head`, so what was held for want of space goes in now, and
    /// the commands that waited for it are taken.
    pub(super) fn cq_head_doorbell(&self, qid: u16, new_head: u16) {
        let mem = MemCtx::new(Arc::clone(&self.physmap));
        let mut st = self.lock();
        let idx = io_queue_index(qid);
        let cq = match idx {
            None if qid == ADMIN_QUEUE_ID => st.admin_cq.as_mut(),
            None => None,
            Some(idx) => st.io_cqs[idx].as_mut(),
        };
        let Some(cq) = cq else {
            return;
        };
        if new_head >= cq.size {
            return;
        }
        cq.head = new_head;

        if let Err(error) = st.flush_held(&mem, idx) {
            slog::error!(self.log, "nvme: held CQE write failed";
                "error" => %error);
            st.csts |= CSTS_CFS;
        }
        if idx.is_some() {
            self.after_finish(&mut st);
        } else if st.admin_sq.as_ref().is_some_and(|sq| sq.head != sq.tail) {
            drop(self.run_admin(st));
        }
    }

    /// Write whatever every CQ holds that now has space for it.
    pub(super) fn flush_all_held(&self, st: &mut NvmeState) {
        let mem = MemCtx::new(Arc::clone(&self.physmap));
        let cqs = std::iter::once(None).chain((0..MAX_IO_QUEUES).map(Some));
        for idx in cqs {
            if let Err(error) = st.flush_held(&mem, idx) {
                slog::error!(self.log, "nvme: held CQE write failed";
                    "error" => %error);
                st.csts |= CSTS_CFS;
            }
        }
    }

    /// Admit again on queues that stopped at their depth and now have
    /// room.
    fn after_finish(&self, st: &mut NvmeState) {
        let throttled = st.throttled;
        for idx in 0..MAX_IO_QUEUES {
            if throttled & (1u16 << idx) != 0
                && st.inflight[idx].has_room(self.depth)
            {
                self.admit(st, idx);
            }
        }
    }

    /// Wait, with the state lock released, until `busy` turns false or
    /// `budget` passes. Returns the lock and whether `busy` turned
    /// false.
    pub(super) fn drain<'a>(
        &'a self,
        mut st: Locked<'a>,
        budget: Duration,
        busy: impl Fn(&NvmeState) -> bool,
    ) -> (Locked<'a>, bool) {
        self.counts.drains.fetch_add(1, Ordering::Relaxed);
        let deadline = Instant::now() + budget;
        st.drain_waiters += 1;
        let done = loop {
            // An MSI still on its way counts too: a reset that cleared
            // the pending bits under it would leave one set for a
            // driver that is gone.
            let raising = self.counts.raising.load(Ordering::Acquire) > 0;
            if !busy(&st) && !raising {
                break true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break false;
            }
            st.wait(left);
        };
        st.drain_waiters -= 1;
        (st, done)
    }

    /// Close admission and wait for every command in flight.
    ///
    /// Admission stays closed until [`Self::reopen`], whether or not the
    /// drain finished.
    pub(super) fn drain_all<'a>(
        &'a self,
        mut st: Locked<'a>,
        budget: Duration,
    ) -> (Locked<'a>, bool) {
        st.admission.draining += 1;
        self.drain(st, budget, |st| st.in_flight() > 0)
    }

    /// End a [`Self::drain_all`], and admit what the guest queued
    /// while it ran.
    pub(super) fn reopen(&self, st: &mut NvmeState) {
        st.admission.draining -= 1;
        self.admit_all(st);
    }
}
