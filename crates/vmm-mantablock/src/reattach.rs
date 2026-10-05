// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Reattach after engine loss: connect to the same socket, Attach with
//! the same generation, and move every queue to the new region.
//!
//! The new engine must grant exactly what the old one did: held requests
//! name their queue, tag and buffer pages, and a replay must match the
//! entry it repeats.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::attach::{self, Attached, Grant};
use crate::conn::Conn;
use crate::job::Job;
use crate::out::{Lost, Out};
use crate::queue::Budget;
use crate::ring::Shared;
use crate::Config;

/// The first wait between attempts; it doubles up to `MAX_BACKOFF`.
const FIRST_BACKOFF: Duration = Duration::from_millis(20);
const MAX_BACKOFF: Duration = Duration::from_secs(1);

/// Bounds how late a wait between attempts sees halt.
const NAP: Duration = Duration::from_millis(50);

pub(crate) struct Plan {
    socket: PathBuf,
    volume: [u8; 16],
    allow_volatile: bool,
    grant: Grant,
    pub(crate) limit: Duration,
    deadline: Duration,
    #[cfg(test)]
    replay_stall: Duration,
}

impl Plan {
    pub(crate) fn new(cfg: &Config, grant: Grant) -> Self {
        Self {
            socket: cfg.socket.clone(),
            volume: *cfg.volume.as_bytes(),
            allow_volatile: cfg.allow_volatile,
            grant,
            limit: cfg.reattach,
            deadline: cfg.deadline,
            #[cfg(test)]
            replay_stall: cfg.replay_stall,
        }
    }
}

enum Failed {
    /// This attempt failed; another may work.
    Retry(String),
    /// A reattach cannot help.
    Fatal(Lost),
}

/// Tries until the ring is live on a new attachment, `until` passes, or
/// halt starts. Returns the new control socket; `None` when the ring is
/// not live, its held I/O failed or about to fail by halt.
pub(crate) fn run<J: Job>(
    shared: &Arc<Shared<J>>,
    plan: &Plan,
    old: Conn,
    until: Instant,
) -> Option<Conn> {
    // The engine ends the old attachment when its socket closes.
    drop(old);
    if !shared.quiesce() {
        return None;
    }
    let mut backoff = FIRST_BACKOFF;
    let mut last = String::new();
    let mut tries = 0u32;
    loop {
        if shared.halted() || shared.is_lost() {
            return None;
        }
        let now = Instant::now();
        if now >= until {
            shared.lose(Lost::Internal(format!(
                "no reattach in time after {tries} attempts: {last}"
            )));
            return None;
        }
        tries += 1;
        match once(shared, plan, (now + plan.deadline).min(until), until) {
            Ok(conn) => {
                slog::info!(shared.log, "mantablock: reattached";
                    "generation" => plan.grant.generation,
                    "attempts" => tries);
                return Some(conn);
            }
            Err(Failed::Fatal(why)) => {
                shared.lose(why);
                return None;
            }
            Err(Failed::Retry(why)) => {
                slog::debug!(shared.log, "mantablock: reattach failed";
                    "reason" => &why);
                last = why;
            }
        }
        nap(
            shared,
            backoff.min(until.saturating_duration_since(Instant::now())),
        );
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

fn nap<J: Job>(shared: &Shared<J>, d: Duration) {
    let end = Instant::now() + d;
    while !shared.halted() {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(NAP));
    }
}

fn once<J: Job>(
    shared: &Arc<Shared<J>>,
    plan: &Plan,
    deadline: Instant,
    until: Instant,
) -> Result<Conn, Failed> {
    let retry = |e: std::io::Error| Failed::Retry(e.to_string());
    let mut conn = Conn::connect(&plan.socket, deadline).map_err(retry)?;
    let ask = plan.grant.ask(plan.volume, plan.allow_volatile);
    let att = attach::attach(&mut conn, &ask, deadline).map_err(retry)?;
    if att.grant() != plan.grant {
        return Err(Failed::Retry(format!(
            "the engine changed the grant: {:?}",
            att.grant()
        )));
    }
    if Instant::now() >= until {
        return Err(Failed::Retry("attached past the bound".into()));
    }
    // Anything lost before this point was about the old attachment.
    shared.rehold();
    match switch(shared, plan, att, until) {
        Ok(()) => Ok(conn),
        Err(e) => {
            shared.rehold();
            Err(e)
        }
    }
}

/// Replays on every queue, goes live, starts the completion threads,
/// then sends what waited. Every queue stays held until all have
/// replayed, so no new request reaches the engine before a replay.
fn switch<J: Job>(
    shared: &Arc<Shared<J>>,
    plan: &Plan,
    att: Attached,
    until: Instant,
) -> Result<(), Failed> {
    let budget = Budget {
        until,
        #[cfg(test)]
        stall: plan.replay_stall,
    };
    #[cfg(not(test))]
    let _ = plan;
    let fail = |why: Lost| {
        if why.fatal() {
            Failed::Fatal(why)
        } else {
            Failed::Retry(why.to_string())
        }
    };
    let Attached { region, pipes, .. } = att;
    if pipes.len() != shared.queues.len() {
        return Err(Failed::Retry("queue count".into()));
    }
    let mut reapers = Vec::with_capacity(pipes.len());
    let mut idle = true;
    // On expiry the replays may already be on the new ring; the engine may
    // serve them, but the guest sees them fail, as a failed write allows.
    for ((q, ring), p) in (0u16..).zip(&shared.queues).zip(pipes) {
        budget.spent().map_err(fail)?;
        let cq = region
            .cq_consumer(q)
            .ok_or_else(|| Failed::Retry("no CQ".into()))?;
        let n = ring
            .reattach(Arc::clone(&region), p.wake_engine, &budget)
            .map_err(fail)?;
        idle &= n == 0;
        reapers.push((q, cq, p.wake_me));
    }
    budget.spent().map_err(fail)?;
    if !shared.resume(idle) {
        return Err(Failed::Retry("lost again during the reattach".into()));
    }
    if !shared.spawn_reapers(&region, reapers) {
        return Err(Failed::Fatal(Lost::Internal(
            "no thread for the ring".into(),
        )));
    }
    for ring in &shared.queues {
        let mut out = Out::default();
        let sent = ring.kick(&mut out);
        shared.finish(out);
        if let Err(why) = sent {
            // Live now: an ordinary loss, which the control thread sees.
            shared.lose(why);
            break;
        }
    }
    Ok(())
}
