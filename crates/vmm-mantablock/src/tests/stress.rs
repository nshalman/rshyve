// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Submission and completion at full rate on both threads at once.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread;
use std::time::{Duration, Instant};

use manta_block_ring::layout::PAGE;
use vmm_storage::nvme::backend::{BlockOp, BlockStatus};

use super::{pattern, setup_with, Engine, TJob};
use crate::{Ring, Sink};

const TAGS: u16 = 8;
const TOTAL: u32 = 20_000;
/// Blocks each guest tag owns on the volume.
const REGION: u64 = 8;

/// The guest: each tag writes its region, then reads it back and checks
/// it. Half the next requests go from inside the sink, on the completion
/// thread, as the controller does.
struct Guest {
    ring: OnceLock<Weak<Ring<TJob>>>,
    next_id: AtomicU32,
    free: Mutex<VecDeque<u16>>,
    state: Mutex<State>,
    done: AtomicU32,
}

#[derive(Default)]
struct State {
    /// Times each id completed.
    seen: HashMap<u32, u32>,
    /// What each tag last wrote, by seed and blocks.
    written: HashMap<u16, (u32, usize)>,
    /// Reads in flight: tag, guest memory, what it must hold.
    reads: HashMap<u32, (u16, Arc<Mutex<Vec<u8>>>, Vec<u8>)>,
    tag_of: HashMap<u32, u16>,
    errors: Vec<String>,
}

impl Guest {
    /// The next request for `tag`, or None once all are sent.
    fn next(&self, tag: u16) -> Option<TJob> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if id >= TOTAL {
            return None;
        }
        let mut st = self.state.lock().unwrap();
        st.tag_of.insert(id, tag);
        let lba = u64::from(tag) * REGION;
        let mem;
        let op = match (id % 3, st.written.get(&tag).copied()) {
            (0, Some((seed, blocks))) => {
                mem = Arc::new(Mutex::new(vec![0; blocks * PAGE]));
                let want = pattern(blocks, seed as u8);
                st.reads.insert(id, (tag, Arc::clone(&mem), want));
                BlockOp::Read
            }
            _ if id.is_multiple_of(11) => {
                mem = Arc::new(Mutex::new(Vec::new()));
                BlockOp::Flush
            }
            _ => {
                // Splits into up to three parts of two pages.
                let blocks = [1, 2, 3, 5][(id % 4) as usize];
                mem = Arc::new(Mutex::new(pattern(blocks, id as u8)));
                st.written.insert(tag, (id, blocks));
                BlockOp::Write {
                    fua: id.is_multiple_of(2),
                }
            }
        };
        Some(TJob {
            id,
            tag,
            op,
            off: if op == BlockOp::Flush {
                0
            } else {
                lba * PAGE as u64
            },
            mem,
        })
    }

    fn submit(&self, job: TJob) {
        if let Some(ring) = self.ring.get().and_then(Weak::upgrade) {
            ring.submit(job);
        }
    }
}

impl Sink<u32> for Guest {
    fn complete(&self, done: &[(u32, BlockStatus)]) {
        let mut again = Vec::new();
        {
            let mut st = self.state.lock().unwrap();
            for &(id, s) in done {
                *st.seen.entry(id).or_default() += 1;
                if s != BlockStatus::Success {
                    st.errors.push(format!("{id}: {s:?}"));
                }
                if let Some((tag, mem, want)) = st.reads.remove(&id) {
                    if *mem.lock().unwrap() != want {
                        st.errors.push(format!("{id}: tag {tag} read back"));
                    }
                }
                let Some(tag) = st.tag_of.remove(&id) else {
                    st.errors.push(format!("{id}: not in flight"));
                    continue;
                };
                if id.is_multiple_of(2) {
                    again.push(tag);
                } else {
                    self.free.lock().unwrap().push_back(tag);
                }
            }
        }
        self.done.fetch_add(done.len() as u32, Ordering::Relaxed);
        // No lock of ours is held: this may complete inline and call
        // back in.
        for tag in again {
            match self.next(tag) {
                Some(job) => self.submit(job),
                None => self.free.lock().unwrap().push_back(tag),
            }
        }
    }
}

/// Serves everything on the ring until told to stop, in batches of
/// varying size and order.
fn serve(mut eng: Engine, stop: &AtomicBool) {
    let mut rng = 0x2545_f491_4f6c_dd1du64;
    while !stop.load(Ordering::Acquire) {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let mut reqs = eng.take(1 + (rng % 8) as usize);
        if reqs.is_empty() {
            // Never idle, so the ring never wakes this thread.
            thread::yield_now();
            continue;
        }
        if rng & 1 == 1 {
            reqs.reverse();
        }
        let cqes: Vec<_> = reqs.iter().map(|e| eng.serve(e).encode()).collect();
        eng.post(&cqes);
    }
}

#[test]
fn submit_and_completion_at_full_rate_lose_and_repeat_nothing() {
    // Eight tags of two pages, and a volatile engine, so requests wait
    // for tags and FUA writes queue their flush from the completion
    // thread.
    let guest = Arc::new(Guest {
        ring: OnceLock::new(),
        next_id: AtomicU32::new(0),
        free: Mutex::new((0..TAGS).collect()),
        state: Mutex::new(State::default()),
        done: AtomicU32::new(0),
    });
    let (ring, eng) =
        setup_with::<TJob>(8, 16, 2, false, Arc::clone(&guest) as _);
    guest.ring.set(Arc::downgrade(&ring)).ok();
    let stop = Arc::new(AtomicBool::new(false));
    let server = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || serve(eng, &stop))
    };

    let until = Instant::now() + Duration::from_secs(30);
    while guest.done.load(Ordering::Relaxed) < TOTAL {
        assert!(
            Instant::now() < until,
            "only {} of {TOTAL} done",
            guest.done.load(Ordering::Relaxed)
        );
        let tag = guest.free.lock().unwrap().pop_front();
        match tag.map(|t| (t, guest.next(t))) {
            Some((_, Some(job))) => guest.submit(job),
            Some((t, None)) => guest.free.lock().unwrap().push_back(t),
            None => std::hint::spin_loop(),
        }
    }
    assert!(!ring.is_lost());
    ring.halt();
    stop.store(true, Ordering::Release);
    server.join().unwrap();

    let st = guest.state.lock().unwrap();
    assert!(
        st.errors.is_empty(),
        "{:?}",
        &st.errors[..st.errors.len().min(10)]
    );
    assert_eq!(st.seen.len(), TOTAL as usize);
    assert!(st.seen.values().all(|&n| n == 1));
}
