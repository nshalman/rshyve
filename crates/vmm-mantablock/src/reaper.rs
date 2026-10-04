// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One completion thread per queue.
//!
//! Every completion the ring holds goes to the sink in one call, so the
//! controller raises one MSI for the batch. Before it sleeps, the thread
//! must set idle, fence and look again, or it can miss a wake-up.

use std::os::fd::OwnedFd;
use std::time::Duration;

use manta_block_ring::layout::CQE_BYTES;
use manta_block_ring::{BadCompletion, Consumer, Cqe};

use crate::job::Job;
use crate::queue::{Lost, Out};
use crate::ring::Shared;
use crate::wake::{self, Waited};

/// Bounds how late the thread sees the stop flag.
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
