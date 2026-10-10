// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The order a quiesce stops and restarts rings in.

use std::io::{ErrorKind, PipeReader, Result};
use std::sync::{Arc, Mutex};

use super::*;
use crate::{IntrStatus, IntrWait, PromiscMode};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Call {
    Pause(u16),
    GetState(u16),
    Reset(u16),
    SetState(u16),
}

/// A link that logs ring calls and refuses the ones a test names.
struct Fake {
    calls: Mutex<Vec<Call>>,
    /// The ring size each read reports. 0 is a ring never programmed.
    size: u16,
    /// The call refused, and with what.
    refuse: Option<(Call, ErrorKind)>,
    /// How many times the refusal is given before the call succeeds.
    refusals: Mutex<usize>,
}

impl Fake {
    fn new(size: u16) -> Self {
        Self {
            calls: Mutex::default(),
            size,
            refuse: None,
            refusals: Mutex::new(usize::MAX),
        }
    }

    fn refusing(mut self, call: Call, kind: ErrorKind, times: usize) -> Self {
        self.refuse = Some((call, kind));
        self.refusals = Mutex::new(times);
        self
    }

    fn answer(&self, call: Call) -> Result<()> {
        self.calls.lock().unwrap().push(call);
        match self.refuse {
            Some((c, kind)) if c == call => {
                let mut left = self.refusals.lock().unwrap();
                if *left == 0 {
                    return Ok(());
                }
                *left -= 1;
                Err(Error::from(kind))
            }
            _ => Ok(()),
        }
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

impl LinkOps for Fake {
    fn ring_init(&self, _: u16, _: u16, _: u64, _: u64, _: u64) -> Result<()> {
        unreachable!("a quiesce programs no new ring")
    }
    fn ring_kick(&self, _: u16) -> Result<()> {
        unreachable!("the caller kicks, not the quiesce")
    }
    fn ring_reset(&self, ring: u16) -> Result<()> {
        self.answer(Reset(ring))
    }
    fn ring_pause(&self, ring: u16) -> Result<()> {
        self.answer(Pause(ring))
    }
    fn ring_set_state(&self, state: &vioc_ring_state) -> Result<()> {
        self.answer(SetState(state.vrs_index))
    }
    fn ring_get_state(&self, ring: u16) -> Result<vioc_ring_state> {
        self.answer(GetState(ring))?;
        Ok(vioc_ring_state {
            vrs_index: ring,
            vrs_qsize: self.size,
            vrs_avail_idx: 7,
            vrs_used_idx: 5,
            ..Default::default()
        })
    }
    fn ring_set_msi(&self, _: u16, _: u64, _: u64) -> Result<()> {
        unreachable!()
    }
    fn set_features(&self, _: u64) -> Result<()> {
        unreachable!()
    }
    fn intr_status(&self) -> Result<IntrStatus> {
        unreachable!()
    }
    fn ring_intr_clr(&self, _: u16) -> Result<()> {
        unreachable!()
    }
    fn wait_intr(&self, _: &PipeReader) -> Result<IntrWait> {
        unreachable!()
    }
    fn set_notify_iop(&self, _: &Quiesced, _: u16) -> Result<()> {
        unreachable!()
    }
    fn set_notify_mmio(&self, _: &Quiesced, _: u64, _: u32) -> Result<()> {
        unreachable!()
    }
    fn set_promisc(&self, _: PromiscMode) -> Result<()> {
        unreachable!()
    }
    fn delete(&self) -> Result<()> {
        unreachable!()
    }
}

use Call::*;

#[test]
fn every_ring_is_paused_read_and_reset_before_the_proof_exists() {
    // The read comes after the pause so the indices hold still, and
    // the reset comes last so the worker is gone.
    let link = Arc::new(Fake::new(256));

    let q = quiesce(link.clone()).expect("nothing refused");
    assert_eq!(
        link.calls(),
        [
            Pause(0),
            GetState(0),
            Reset(0),
            Pause(1),
            GetState(1),
            Reset(1)
        ],
    );

    let resumed = q.resume();
    assert_eq!(link.calls()[6..], [SetState(0), SetState(1)]);
    assert_eq!(resumed.kick, [0, 1]);
    assert!(resumed.lost.is_empty());
}

#[test]
fn the_state_written_back_is_the_state_read_out() {
    // Indices that drift lose or replay packets.
    let link = Arc::new(Fake::new(256));
    let q = quiesce(link.clone()).expect("nothing refused");
    assert_eq!(
        q.saved[0].map(|s| (s.vrs_avail_idx, s.vrs_used_idx)),
        Some((7, 5))
    );
    drop(q.resume());
}

#[test]
fn a_ring_never_programmed_is_not_written_back() {
    // A state write with size 0 is refused by the kernel, and the ring
    // had no worker to stop.
    let link = Arc::new(Fake::new(0));

    let resumed = quiesce(link.clone()).expect("nothing refused").resume();

    assert!(!link.calls().iter().any(|c| matches!(c, SetState(_))));
    assert!(resumed.kick.is_empty());
}

#[test]
fn a_refused_pause_puts_back_the_rings_already_stopped() {
    // Nothing may change the hook now. Ring 0 is reset already and
    // needs its state back; ring 1 is at most paused and needs a kick.
    let link = Arc::new(Fake::new(256).refusing(
        Pause(1),
        ErrorKind::ResourceBusy,
        usize::MAX,
    ));

    let failed = quiesce(link.clone()).err().expect("the pause was refused");

    assert_eq!((failed.ring, failed.step), (1, "pause"));
    assert_eq!(link.calls().last(), Some(&SetState(0)));
    assert_eq!(failed.resumed.kick, [0, 1]);
}

#[test]
fn a_refused_state_read_kicks_the_paused_ring() {
    let link =
        Arc::new(Fake::new(256).refusing(GetState(0), ErrorKind::Other, 1));

    let failed = quiesce(link.clone()).err().expect("the read was refused");

    assert_eq!(failed.step, "get state");
    assert!(!link.calls().contains(&Reset(0)));
    assert_eq!(failed.resumed.kick, [0]);
}

#[test]
fn a_refused_reset_loses_that_ring() {
    // Its worker may still be alive, so it cannot be restarted from a
    // state write, and the hook must not change.
    let link = Arc::new(Fake::new(256).refusing(
        Reset(1),
        ErrorKind::Other,
        usize::MAX,
    ));

    let failed = quiesce(link.clone()).err().expect("the reset was refused");

    assert_eq!(failed.step, "reset");
    assert_eq!(failed.resumed.kick, [0]);
    assert_eq!(failed.resumed.lost.len(), 1);
    assert_eq!(failed.resumed.lost[0].0, 1);
}

#[test]
fn an_interrupted_reset_is_tried_again() {
    let link =
        Arc::new(Fake::new(256).refusing(Reset(0), ErrorKind::Interrupted, 2));

    let q = quiesce(link.clone()).expect("the reset got through");

    assert_eq!(link.calls().iter().filter(|c| **c == Reset(0)).count(), 3);
    drop(q.resume());
}

#[test]
fn a_dropped_proof_still_writes_the_rings_back() {
    let link = Arc::new(Fake::new(256));

    drop(quiesce(link.clone()).expect("nothing refused"));

    assert_eq!(link.calls()[6..], [SetState(0), SetState(1)]);
}

#[test]
fn a_proof_covers_only_its_own_link() {
    let mine = Arc::new(Fake::new(0));
    let other = Fake::new(0);
    let q = quiesce(mine.clone()).expect("nothing refused");

    assert!(q.covers(mine.as_ref()));
    assert!(!q.covers(&other));
    drop(q.resume());
}

#[test]
fn a_ring_the_guest_programs_while_stopped_is_written_back_as_programmed() {
    let link = Arc::new(Fake::new(256));
    let mut q = quiesce(link.clone()).expect("nothing refused");
    let fresh = vioc_ring_state {
        vrs_index: 1,
        vrs_qsize: 128,
        ..Default::default()
    };

    q.set_saved(0, None);
    q.set_saved(1, Some(fresh));
    assert_eq!(q.saved(0), None);
    assert_eq!(q.saved(1).map(|s| s.vrs_qsize), Some(128));

    let resumed = q.resume();
    assert_eq!(link.calls()[6..], [SetState(1)]);
    assert_eq!(resumed.kick, [1]);
}

#[test]
fn a_discarded_proof_writes_nothing_back() {
    // The link is about to be destroyed; a state write would start a
    // worker on it.
    let link = Arc::new(Fake::new(256));

    quiesce(link.clone()).expect("nothing refused").discard();

    assert_eq!(link.calls().len(), 6);
}
