// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Stopping every ring worker before the notify hook changes.
//!
//! The order matters because the kernel takes the ring lock under the
//! VM write lock. A hook change takes the VM write lock, which expires
//! every lease and runs the expiry callback, and that callback takes
//! the ring lock. A ring worker renews its lease with its ring lock
//! held, and the renewal waits for the write lock to clear. If both
//! happen at once, neither side moves, the process cannot be killed,
//! and only a host reboot clears it.
//!
//! A pause is not enough: a paused worker stays alive and still renews
//! its lease. A reset ends the worker, and that drops its lease. So the
//! sequence is pause (the indices stop moving), read the state, reset
//! (the worker exits), and later write the state back.
//!
//! [`Quiesced`] is the proof that the sequence ran. The two hook calls
//! on [`LinkOps`] take it, and only [`quiesce`] makes one.
//!
//! One link's proof is not enough for the VM write lock: every link on
//! the VM has to be stopped. The caller that owns the VM decides that;
//! this proof is what each link holds while it is stopped.

use std::io::{Error, ErrorKind, Result};
use std::sync::Arc;

use super::LinkOps;
use crate::{vioc_ring_state, VIONA_VQ_MAX};

const RINGS: usize = VIONA_VQ_MAX as usize;

/// How many times an interrupted pause or reset is tried again.
///
/// Both kernel waits heed signals, and `EINTR` says only that this
/// thread took one.
const ATTEMPTS: usize = 8;

/// Every ring on one link, stopped, with no ring worker alive.
///
/// The rings that held a programmed ring are written back by
/// [`Self::resume`], or by the drop if the caller does not call it.
#[must_use = "the rings stay stopped until this is resumed"]
pub struct Quiesced {
    /// `None` for a recorder's proof, which has nothing to write back.
    link: Option<Arc<dyn LinkOps>>,
    /// The link's address, which is all [`Self::covers`] compares.
    addr: usize,
    saved: [Option<vioc_ring_state>; RINGS],
    resumed: bool,
}

fn addr_of(link: &dyn LinkOps) -> usize {
    std::ptr::from_ref(link).cast::<()>() as usize
}

/// What putting the rings back did.
#[derive(Debug, Default)]
pub struct Resumed {
    /// Rings that hold their state again. Each needs a kick, because
    /// the kernel does not start a ring on its own.
    pub kick: Vec<u16>,
    /// Rings that could not be put back. They stay stopped.
    pub lost: Vec<(u16, Error)>,
}

/// A quiesce that stopped short. Nothing was changed on the hook.
#[derive(Debug)]
pub struct QuiesceFailed {
    pub ring: u16,
    pub step: &'static str,
    pub error: Error,
    /// The rings this call had already stopped, put back.
    pub resumed: Resumed,
}

impl std::fmt::Display for QuiesceFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ring {} {}: {}", self.ring, self.step, self.error)
    }
}

/// Stop every ring on `link` so that its notify hook can change.
///
/// The caller must keep every other caller off this link's rings until
/// the answer is resumed: a ring init or a kick in between starts a
/// worker again.
pub fn quiesce(
    link: Arc<dyn LinkOps>,
) -> std::result::Result<Quiesced, QuiesceFailed> {
    let mut q = Quiesced {
        addr: addr_of(link.as_ref()),
        link: Some(Arc::clone(&link)),
        saved: [None; RINGS],
        resumed: false,
    };
    for ring in 0..VIONA_VQ_MAX {
        let fail = |q: &mut Quiesced, step, error, stuck: bool| {
            let mut resumed = q.put_back();
            if stuck {
                resumed.lost.push((ring, Error::other(step)));
            } else {
                // Paused at most, with its state in the kernel still.
                // A kick starts it again.
                resumed.kick.push(ring);
            }
            QuiesceFailed {
                ring,
                step,
                error,
                resumed,
            }
        };
        if let Err(e) = retry(|| link.ring_pause(ring)) {
            return Err(fail(&mut q, "pause", e, false));
        }
        // A signal must not end the quiesce with the ring paused: the
        // read is tried again like the pause and the reset are.
        let mut state = None;
        let read = retry(|| {
            state = Some(link.ring_get_state(ring)?);
            Ok(())
        });
        let state = match (read, state) {
            (Ok(()), Some(state)) => state,
            (Err(e), _) => return Err(fail(&mut q, "get state", e, false)),
            (Ok(()), None) => {
                let e = Error::other("no state read");
                return Err(fail(&mut q, "get state", e, false));
            }
        };
        if let Err(e) = retry(|| link.ring_reset(ring)) {
            return Err(fail(&mut q, "reset", e, true));
        }
        // A ring with no size was never programmed, so there is
        // nothing to write back.
        q.saved[usize::from(ring)] = (state.vrs_qsize != 0).then_some(state);
    }
    Ok(q)
}

fn retry(mut call: impl FnMut() -> Result<()>) -> Result<()> {
    for _ in 1..ATTEMPTS {
        match call() {
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            rc => return rc,
        }
    }
    call()
}

impl Quiesced {
    /// Whether this proof covers `link`.
    ///
    /// A proof made for one link says nothing about another.
    pub fn covers(&self, link: &dyn LinkOps) -> bool {
        self.addr == addr_of(link)
    }

    /// The state a ring will be written back with, or `None` for a ring
    /// that will stay reset.
    pub fn saved(&self, ring: u16) -> Option<vioc_ring_state> {
        self.saved.get(usize::from(ring)).copied().flatten()
    }

    /// Change what a ring is written back with, for a guest that
    /// programs or retires the ring while it is stopped.
    ///
    /// A state with no size is a ring left reset.
    pub fn set_saved(&mut self, ring: u16, state: Option<vioc_ring_state>) {
        if let Some(slot) = self.saved.get_mut(usize::from(ring)) {
            *slot = state.filter(|s| s.vrs_qsize != 0);
        }
    }

    /// Write back the state of every ring that held one.
    pub fn resume(mut self) -> Resumed {
        self.put_back()
    }

    /// Give the proof up and write nothing back, for a link that is
    /// about to be destroyed.
    pub fn discard(mut self) {
        self.resumed = true;
    }

    fn put_back(&mut self) -> Resumed {
        let mut out = Resumed::default();
        if std::mem::replace(&mut self.resumed, true) {
            return out;
        }
        let Some(link) = self.link.as_ref() else {
            return out;
        };
        for (ring, state) in (0u16..).zip(self.saved.iter_mut()) {
            let Some(state) = state.take() else {
                continue;
            };
            match link.ring_set_state(&state) {
                Ok(()) => out.kick.push(ring),
                Err(e) => out.lost.push((ring, e)),
            }
        }
        out
    }
}

impl Drop for Quiesced {
    fn drop(&mut self) {
        // A caller that never resumed still gets its rings back. Nobody
        // is left to kick them, so a guest kick has to.
        drop(self.put_back());
    }
}

#[cfg(test)]
impl Quiesced {
    /// A proof for a recorder, which runs no ring worker.
    pub(crate) fn for_recorder(link: &dyn LinkOps) -> Self {
        Self {
            link: None,
            addr: addr_of(link),
            saved: [None; RINGS],
            resumed: true,
        }
    }
}

#[cfg(test)]
mod test;
