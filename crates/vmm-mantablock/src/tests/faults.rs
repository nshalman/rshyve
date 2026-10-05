// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Parts that fail, engines that fail between parts, and requests the
//! guest should not have sent.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use manta_block_ring::layout::{CQE_BYTES, PAGE};
use manta_block_ring::{Cqe, Geometry, Status};
use vmm_storage::nvme::backend::{BlockOp, BlockStatus};

use super::{job, lie, map, pattern, setup, TJob};
use crate::out::Lost;

fn status(c: Cqe, s: Status) -> [u8; CQE_BYTES] {
    Cqe { status: s, ..c }.encode()
}

#[test]
fn a_failed_part_fails_its_request_once_with_the_first_error() {
    // Volatile, so a FUA write would send a flush after its parts.
    let (ring, calls, mut eng) = setup(8, 16, 2, false);
    ring.submit(job(1, 0, BlockOp::Write { fua: true }, 0, pattern(5, 1)));
    let parts = eng.take_n(3);
    let c: Vec<_> = parts.iter().map(|e| eng.serve(e)).collect();
    eng.post(&[
        status(c[0], Status::MediaError),
        status(c[1], Status::Internal),
        c[2].encode(),
    ]);
    assert_eq!(calls.wait_for(1), vec![(1, BlockStatus::MediaError)]);
    std::thread::sleep(Duration::from_millis(50));
    assert!(eng.take(8).is_empty(), "a flush after a failed write");
    assert_eq!(calls.all().len(), 1);
}

#[test]
fn an_engine_lost_between_parts_fails_the_request_once() {
    let (ring, calls, mut eng) = setup(8, 16, 2, true);
    ring.submit(job(1, 0, BlockOp::Write { fua: false }, 0, pattern(5, 1)));
    let parts = eng.take_n(3);
    let first = eng.serve(&parts[0]).encode();
    eng.post(&[first]);
    let mut bad = eng.serve(&parts[1]);
    bad.op_seq += 1;
    eng.post(&[bad.encode()]);
    assert_eq!(calls.wait_for(1), vec![(1, BlockStatus::Internal)]);
    let last = eng.serve(&parts[2]).encode();
    eng.post(&[last]);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(calls.all().len(), 1);
    assert!(ring.is_lost());
}

#[test]
fn a_failed_read_part_writes_no_guest_memory() {
    let (ring, calls, mut eng) = setup(8, 16, 2, true);
    eng.disk[..4 * PAGE].copy_from_slice(&pattern(4, 7));
    let mem = Arc::new(Mutex::new(vec![0xee; 4 * PAGE]));
    ring.submit(TJob {
        id: 1,
        queue: 1,
        tag: 0,
        op: BlockOp::Read,
        off: 0,
        mem: Arc::clone(&mem),
    });
    let parts = eng.take_n(2);
    // `serve` fills the slot, so a copy would be seen.
    let c: Vec<_> = parts.iter().map(|e| eng.serve(e)).collect();
    eng.post(&[status(c[0], Status::MediaError), c[1].encode()]);
    assert_eq!(calls.wait_for(1), vec![(1, BlockStatus::MediaError)]);
    assert_eq!(parts[0].lba, 0);
    assert!(mem.lock().unwrap()[..2 * PAGE].iter().all(|&b| b == 0xee));
}

#[test]
fn a_completion_with_reserved_bytes_set_loses_the_engine() {
    for at in [4, 24] {
        lie(|e| {
            let mut c = Cqe::decode(&[0; CQE_BYTES]).unwrap();
            c.tag = e.tag;
            c.op_seq = e.op_id as u64;
            let mut b = c.encode();
            b[at] = 1;
            b
        });
    }
}

#[test]
fn a_second_request_on_a_guest_tag_in_flight_fails_alone() {
    let (ring, calls, mut eng) = setup(8, 8, 1, true);
    ring.submit(job(1, 0, BlockOp::Read, 0, pattern(1, 0)));
    ring.submit(job(2, 0, BlockOp::Read, 1, pattern(1, 0)));
    assert_eq!(calls.wait_for(1), vec![(2, BlockStatus::Internal)]);
    eng.serve_all(1);
    assert_eq!(calls.wait_for(2)[1], (1, BlockStatus::Success));
    assert_eq!(eng.seen.len(), 1);
    assert!(!ring.is_lost());
}

/// Takes one pass of `pushed` completions with 4 tags.
fn take_pass(pushed: usize) -> (Option<Lost>, usize) {
    let g = Geometry::new(1, 8, 4).unwrap();
    let file = tempfile::tempfile().unwrap();
    file.set_len(g.region_len() as u64).unwrap();
    let region = map(&file, g);
    let mut cq = region.cq_consumer(0).unwrap();
    let mut eng = region.cq_producer(0).unwrap();
    let c = Cqe {
        tag: 0,
        status: Status::Success,
        op_seq: 1,
        engine_ns: 0,
        durable_ns: 0,
    }
    .encode();
    for _ in 0..pushed {
        assert!(eng.try_push(&c).unwrap());
    }
    eng.publish();
    let mut cqes = Vec::new();
    let lost = crate::reaper::take(&mut cq, &mut cqes, 4, 0);
    (lost, cqes.len())
}

#[test]
fn more_completions_in_a_pass_than_tags_is_an_overrun() {
    assert_eq!(take_pass(5), (Some(Lost::Overrun(0)), 4));
}

#[test]
fn as_many_completions_in_a_pass_as_tags_is_fine() {
    assert_eq!(take_pass(4), (None, 4));
}
