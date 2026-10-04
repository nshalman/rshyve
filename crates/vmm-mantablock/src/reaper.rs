// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One completion thread per queue (spec 37 §4).
//!
//! It takes every completion the ring holds in one pass and reports them
//! to the sink in one call, so the controller writes the batch and
//! raises one MSI for it. When the ring stays empty for the spin budget
//! it sleeps on its wake-up pipe, following the state machine: set idle,
//! fence, look again, then wait.

use std::os::fd::OwnedFd;
use std::time::Duration;

use manta_block_ring::layout::CQE_BYTES;
use manta_block_ring::{BadCompletion, Consumer, Cqe};

use crate::job::Job;
use crate::queue::{Lost, Out};
use crate::ring::Shared;
use crate::wake::{self, Waited};

/// Longest sleep before the thread looks at the stop flag again. A
/// wake-up the engine owes arrives well before it.
const NAP: Duration = Duration::from_millis(50);

pub(crate) fn run<J: Job>(
    shared: &Shared<J>,
    q: u16,
    mut cq: Consumer<CQE_BYTES>,
    wake_me: &OwnedFd,
    spin: u32,
) {
    let mut empty = 0u32;
    while !shared.stopping() {
        match reap(shared, q, &mut cq) {
            Ok(0) => {}
            Ok(_) => {
                empty = 0;
                continue;
            }
            Err(why) => return shared.lose(why),
        }
        empty += 1;
        if empty < spin {
            std::hint::spin_loop();
            continue;
        }
        empty = 0;
        match cq.prepare_wait() {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => return shared.lose(Lost::Broken(q)),
        }
        let waited = wake::wait(wake_me, NAP);
        cq.woke();
        if waited == Waited::Gone {
            // Take what the engine finished before it closed its end.
            if let Err(why) = reap(shared, q, &mut cq) {
                return shared.lose(why);
            }
            if !shared.halted() {
                shared.lose(Lost::Pipe(q));
            }
            return;
        }
    }
}

/// Takes every completion on the ring and reports them in one batch.
/// Returns how many there were.
fn reap<J: Job>(
    shared: &Shared<J>,
    q: u16,
    cq: &mut Consumer<CQE_BYTES>,
) -> Result<usize, Lost> {
    let mut cqes = Vec::new();
    let mut bad = None;
    loop {
        match cq.pop() {
            Ok(Some(b)) => match Cqe::decode(&b) {
                Ok(c) => cqes.push(c),
                Err(e) => {
                    bad = Some(Lost::Completion(q, BadCompletion::Entry(e)));
                    break;
                }
            },
            Ok(None) => break,
            Err(_) => {
                bad = Some(Lost::Broken(q));
                break;
            }
        }
    }
    if cqes.is_empty() {
        return bad.map_or(Ok(0), Err);
    }
    // Each entry is copied out, so the slots go back at once.
    cq.release();
    let mut out = Out::default();
    let done = match shared.queue(usize::from(q)) {
        Some(mut g) => g.complete(&cqes, &mut out),
        None => Err(Lost::Control("queue lock poisoned".into())),
    };
    shared.finish(out);
    done?;
    bad.map_or(Ok(cqes.len()), Err)
}
