// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One completion thread per queue.
//!
//! Every completion the ring holds goes to the sink in one call, so the
//! controller raises one MSI for the batch. Before it sleeps, the thread
//! must set idle, fence and look again, or it can miss a wake-up.

use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use manta_block_ring::layout::CQE_BYTES;
use manta_block_ring::{BadCompletion, Consumer, Cqe};

use crate::job::Job;
use crate::out::{Lost, Out};
use crate::queue::{CqSide, QueueRing};
use crate::ring::Shared;
use crate::wake::{self, Waited};

/// Bounds how late the thread sees the stop flag.
const NAP: Duration = Duration::from_millis(50);

/// Empty polls between clock reads: a read costs a few polls.
const POLLS_PER_CLOCK: u32 = 16;

pub(crate) fn run<J: Job>(
    shared: &Shared<J>,
    q: u16,
    mut cq: Consumer<CQE_BYTES>,
    wake_me: &OwnedFd,
    spin: Duration,
) {
    let Some(ring) = shared.queue(usize::from(q)) else {
        return;
    };
    let mut side = ring.cq_side();
    let mut cqes = Vec::new();
    // The spin runs from the last completion, so a sleep that ends with
    // nothing to take starts no new spin.
    let mut last = Instant::now();
    let mut polls = 0u32;
    let mut empty = wake::Empty::default();
    let mut woke = false;
    while !shared.stopping() {
        let found = match reap(shared, ring, q, &mut cq, &mut cqes, &mut side) {
            Ok(n) => n > 0,
            Err(why) => return shared.lose(why),
        };
        if std::mem::take(&mut woke) || found {
            if let Some(nap) = empty.after(found) {
                std::thread::sleep(nap);
            }
        }
        if found {
            last = Instant::now();
            polls = 0;
            continue;
        }
        polls += 1;
        if !spin.is_zero()
            && (!polls.is_multiple_of(POLLS_PER_CLOCK) || last.elapsed() < spin)
        {
            std::hint::spin_loop();
            continue;
        }
        polls = 0;
        match cq.prepare_wait() {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => return shared.lose(Lost::Broken(q)),
        }
        let waited = wake::wait(wake_me, NAP);
        cq.woke();
        match waited {
            Waited::Woke => woke = true,
            Waited::Timeout => {}
            Waited::Gone => {
                // Take what the engine finished before it closed its end.
                if let Err(why) =
                    reap(shared, ring, q, &mut cq, &mut cqes, &mut side)
                {
                    return shared.lose(why);
                }
                if !shared.halted() {
                    shared.lose(Lost::Pipe(q));
                }
                return;
            }
        }
    }
}

fn reap<J: Job>(
    shared: &Shared<J>,
    ring: &QueueRing<J>,
    q: u16,
    cq: &mut Consumer<CQE_BYTES>,
    cqes: &mut Vec<Cqe>,
    side: &mut CqSide,
) -> Result<usize, Lost> {
    let bad = take(cq, cqes, usize::from(ring.tags()), q);
    if cqes.is_empty() {
        return bad.map_or(Ok(0), Err);
    }
    // Each entry is copied out, so the slots go back at once.
    cq.release();
    let mut out = Out::default();
    let done = ring.complete(cqes, side, &mut out);
    shared.finish(out);
    done?;
    bad.map_or(Ok(cqes.len()), Err)
}

/// Pops into `cqes` what the ring holds, and returns why the engine is
/// lost if it is. Tags come back only after the pass, so a correct engine
/// cannot have more than `tags` completions on the ring.
pub(crate) fn take(
    cq: &mut Consumer<CQE_BYTES>,
    cqes: &mut Vec<Cqe>,
    tags: usize,
    q: u16,
) -> Option<Lost> {
    cqes.clear();
    loop {
        match cq.pop() {
            Ok(Some(_)) if cqes.len() == tags => return Some(Lost::Overrun(q)),
            Ok(Some(b)) => match Cqe::decode(&b) {
                Ok(c) => cqes.push(c),
                Err(e) => {
                    return Some(Lost::Completion(q, BadCompletion::Entry(e)))
                }
            },
            Ok(None) => return None,
            Err(_) => return Some(Lost::Broken(q)),
        }
    }
}
