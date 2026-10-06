// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! How a request ends, and how the ring ends.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use manta_block_ring::BadCompletion;
use vmm_storage::nvme::backend::BlockStatus;

use crate::job::Job;

/// Why the engine counts as lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Lost {
    /// An index moved in a way no correct engine moves it.
    Broken(u16),
    /// The SQ had no room for a request within the depth.
    Full(u16),
    /// More completions in one pass than there are tags.
    Overrun(u16),
    /// A completion that matches no request in flight.
    Completion(u16, BadCompletion),
    /// A wake-up pipe closed or failed.
    Pipe(u16),
    /// The queue ran out of sequence numbers.
    Exhausted(u16),
    /// The control socket closed, failed or carried an Error.
    Control(String),
    /// No Pong within the heartbeat limit.
    Heartbeat,
    /// A request stayed on the ring past the I/O limit.
    Stalled,
    /// The ring itself failed: a poisoned lock or no thread.
    Internal(String),
    /// The engine let go of the generation and never takes it again.
    Refused(String),
}

impl Lost {
    /// True when a reattach cannot help: the generation has no sequence
    /// left or was refused for good, or the ring's own state is not to be
    /// trusted.
    pub(crate) fn fatal(&self) -> bool {
        matches!(
            self,
            Self::Exhausted(_) | Self::Internal(_) | Self::Refused(_)
        )
    }
}

impl std::fmt::Display for Lost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

/// A request the ring makes for itself, and the thread that waits on it.
#[derive(Debug, Default)]
pub(crate) struct Waiter {
    status: Mutex<Option<BlockStatus>>,
    cv: Condvar,
}

impl Waiter {
    fn set(&self, s: BlockStatus) {
        if let Ok(mut g) = self.status.lock() {
            *g = Some(s);
            self.cv.notify_all();
        }
    }

    /// Waits until the request ends or `until` passes.
    pub(crate) fn wait(&self, until: Instant) -> Option<BlockStatus> {
        let mut g = self.status.lock().ok()?;
        loop {
            if let Some(s) = *g {
                return Some(s);
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            g = self.cv.wait_timeout(g, left).ok()?.0;
        }
    }
}

pub(crate) enum Owner<J> {
    Guest(J),
    Internal(Arc<Waiter>),
}

/// What finished under the lock, to report once it is released.
pub(crate) struct Out<Id> {
    pub(crate) done: Vec<(Id, BlockStatus)>,
    woken: Vec<(Arc<Waiter>, BlockStatus)>,
}

impl<Id> Default for Out<Id> {
    fn default() -> Self {
        Self {
            done: Vec::new(),
            woken: Vec::new(),
        }
    }
}

impl<Id> Out<Id> {
    pub(crate) fn finish<J: Job<Id = Id>>(
        &mut self,
        owner: Owner<J>,
        s: BlockStatus,
    ) {
        match owner {
            Owner::Guest(job) => self.done.push((job.id(), s)),
            Owner::Internal(w) => self.woken.push((w, s)),
        }
    }

    /// Wakes the ring's own waiters. The guest's go to the sink.
    pub(crate) fn wake_internal(&mut self) {
        for (w, s) in self.woken.drain(..) {
            w.set(s);
        }
    }
}
