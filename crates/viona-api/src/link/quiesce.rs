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

use std::io::{Error, ErrorKind, Result};

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
pub struct Quiesced<'a> {
    link: &'a dyn LinkOps,
    saved: [Option<vioc_ring_state>; RINGS],
    resumed: bool,
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
    link: &dyn LinkOps,
) -> std::result::Result<Quiesced<'_>, QuiesceFailed> {
    let mut q = Quiesced {
        link,
        saved: [None; RINGS],
        resumed: false,
    };
    for ring in 0..VIONA_VQ_MAX {
        let fail = |q: &mut Quiesced<'_>, step, error, stuck: bool| {
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
        let state = match link.ring_get_state(ring) {
            Ok(state) => state,
            Err(e) => return Err(fail(&mut q, "get state", e, false)),
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

impl Quiesced<'_> {
    /// Whether this proof covers `link`.
    ///
    /// A proof made for one link says nothing about another.
    pub fn covers(&self, link: &dyn LinkOps) -> bool {
        std::ptr::addr_eq(
            self.link as *const dyn LinkOps,
            link as *const dyn LinkOps,
        )
    }

    /// Write back the state of every ring that held one.
    pub fn resume(mut self) -> Resumed {
        self.put_back()
    }

    fn put_back(&mut self) -> Resumed {
        let mut out = Resumed::default();
        if std::mem::replace(&mut self.resumed, true) {
            return out;
        }
        for (ring, state) in (0u16..).zip(self.saved.iter_mut()) {
            let Some(state) = state.take() else {
                continue;
            };
            match self.link.ring_set_state(&state) {
                Ok(()) => out.kick.push(ring),
                Err(e) => out.lost.push((ring, e)),
            }
        }
        out
    }
}

impl Drop for Quiesced<'_> {
    fn drop(&mut self) {
        // A caller that never resumed still gets its rings back. Nobody
        // is left to kick them, so a guest kick has to.
        drop(self.put_back());
    }
}

#[cfg(test)]
impl<'a> Quiesced<'a> {
    /// A proof for a recorder, which runs no ring worker.
    pub(crate) fn for_recorder(link: &'a dyn LinkOps) -> Self {
        Self {
            link,
            saved: [None; RINGS],
            resumed: true,
        }
    }
}

#[cfg(test)]
mod test;
