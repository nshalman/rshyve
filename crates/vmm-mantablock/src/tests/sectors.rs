// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A 512e volume: sector requests go as 4 KiB blocks with a byte range,
//! reads copy out only the guest's bytes, and deallocate and write
//! zeroes go as zeroing ops, edges apart.

use super::*;

const SECTOR: usize = 512;

/// A ring on a 512e volume that takes zeroing, `max_blocks` per part.
fn setup_512e(max_blocks: u32) -> (Arc<Ring<TJob>>, Arc<Calls>, Engine) {
    let calls = Arc::new(Calls::default());
    let (ring, mut engines) = setup_volume(
        1,
        8,
        16,
        max_blocks,
        true,
        Arc::clone(&calls) as _,
        None,
        |_| {},
        (512, true),
    );
    (ring, calls, engines.remove(0))
}

fn at(id: u32, tag: u16, op: BlockOp, off: usize, data: Vec<u8>) -> TJob {
    TJob {
        id,
        queue: 1,
        tag,
        op,
        off: off as u64,
        mem: Arc::new(Mutex::new(data)),
    }
}

fn serve_until_done(eng: &mut Engine, calls: &Calls, n: usize) {
    let until = Instant::now() + Duration::from_secs(5);
    while calls.all().len() < n {
        assert!(Instant::now() < until, "only {} done", calls.all().len());
        let got = eng.take(16);
        let cqes: Vec<_> = got.iter().map(|e| eng.serve(e).encode()).collect();
        if !cqes.is_empty() {
            eng.post(&cqes);
        }
        let _ = crate::wake::wait(&eng.wake_me, Duration::from_millis(2));
    }
}

#[test]
fn an_unaligned_write_goes_in_parts_with_ranges_and_reads_back() {
    // Two blocks per part: sectors 1 to 40 touch six blocks, three parts.
    let (ring, calls, mut eng) = setup_512e(2);
    eng.disk.fill(0xaa);
    let data: Vec<u8> = (0..40 * SECTOR).map(|i| (i / SECTOR) as u8).collect();
    ring.submit(at(
        1,
        0,
        BlockOp::Write { fua: false },
        SECTOR,
        data.clone(),
    ));
    serve_until_done(&mut eng, &calls, 1);
    assert_eq!(calls.status(1), Some(BlockStatus::Success));
    let parts: Vec<(u64, u32, u16, u32)> = eng
        .seen
        .iter()
        .map(|e| (e.lba, e.blocks, e.byte_off, e.byte_len))
        .collect();
    assert_eq!(
        parts,
        vec![(0, 2, 512, 7680), (2, 2, 0, 0), (4, 2, 0, 4608)]
    );
    assert!(eng.disk[..SECTOR].iter().all(|&b| b == 0xaa));
    assert_eq!(&eng.disk[SECTOR..41 * SECTOR], &data[..]);
    assert!(eng.disk[41 * SECTOR..48 * SECTOR]
        .iter()
        .all(|&b| b == 0xaa));

    // A read of three sectors inside block 1 moves the block and copies
    // out only them.
    let back = Arc::new(Mutex::new(vec![0; 3 * SECTOR]));
    ring.submit(TJob {
        id: 2,
        queue: 1,
        tag: 1,
        op: BlockOp::Read,
        off: (9 * SECTOR) as u64,
        mem: Arc::clone(&back),
    });
    serve_until_done(&mut eng, &calls, 2);
    let last = eng.seen.last().copied().unwrap();
    assert_eq!((last.lba, last.blocks, last.byte_len), (1, 1, 0));
    assert_eq!(*back.lock().unwrap(), data[8 * SECTOR..11 * SECTOR]);
}

#[test]
fn aligned_requests_on_a_512e_volume_carry_no_range() {
    let (ring, calls, mut eng) = setup_512e(8);
    ring.submit(at(1, 0, BlockOp::Write { fua: true }, 4096, pattern(2, 1)));
    serve_until_done(&mut eng, &calls, 1);
    let e = eng.seen[0];
    assert_eq!((e.lba, e.blocks, e.byte_off, e.byte_len), (1, 2, 0, 0));
}

#[test]
fn deallocate_and_write_zeroes_go_as_zeroing_ops() {
    let (ring, calls, mut eng) = setup_512e(2);
    eng.disk.fill(0xaa);
    // Sector 3 to the end of block 9: an edge, then whole blocks.
    ring.submit(at(
        1,
        0,
        BlockOp::Deallocate,
        3 * SECTOR,
        vec![0; 10 * 4096 - 3 * SECTOR],
    ));
    ring.submit(at(
        2,
        1,
        BlockOp::WriteZeroes {
            deallocate: false,
            fua: false,
        },
        20 * 4096,
        vec![0; 2 * SECTOR],
    ));
    serve_until_done(&mut eng, &calls, 2);
    assert!(calls.all().iter().all(|(_, s)| *s == BlockStatus::Success));
    let ops: Vec<(u8, u64, u32, u16, u32, u32)> = eng
        .seen
        .iter()
        .map(|e| (e.op, e.lba, e.blocks, e.byte_off, e.byte_len, e.buf_page))
        .collect();
    assert_eq!(
        ops,
        vec![
            (4, 0, 1, 1536, 2560, 0),
            (4, 1, 9, 0, 0, 0),
            (5, 20, 1, 0, 1024, 0)
        ]
    );
    assert!(eng.disk[..3 * SECTOR].iter().all(|&b| b == 0xaa));
    assert!(eng.disk[3 * SECTOR..10 * 4096].iter().all(|&b| b == 0));
    assert!(eng.disk[20 * 4096..20 * 4096 + 1024]
        .iter()
        .all(|&b| b == 0));
    assert!(eng.disk[20 * 4096 + 1024..21 * 4096]
        .iter()
        .all(|&b| b == 0xaa));
}

#[test]
fn write_zeroes_with_fua_flushes_a_volatile_engine() {
    let calls = Arc::new(Calls::default());
    let (ring, mut engines) = setup_volume::<TJob>(
        1,
        8,
        16,
        2,
        false,
        Arc::clone(&calls) as _,
        None,
        |_| {},
        (512, true),
    );
    let mut eng = engines.remove(0);
    ring.submit(at(
        1,
        0,
        BlockOp::WriteZeroes {
            deallocate: false,
            fua: true,
        },
        0,
        vec![0; 4096],
    ));
    serve_until_done(&mut eng, &calls, 1);
    let ops: Vec<u8> = eng.seen.iter().map(|e| e.op).collect();
    assert_eq!(ops, vec![5, 3]);
}

#[test]
fn zeroing_on_a_volume_that_does_not_take_it_is_unsupported() {
    let (ring, calls, eng) = setup(8, 8, 1, true);
    ring.submit(job(1, 0, BlockOp::Deallocate, 0, vec![0; 4096]));
    assert_eq!(calls.wait_for(1), vec![(1, BlockStatus::Unsupported)]);
    assert!(eng.seen.is_empty());
}

#[test]
fn a_sub_block_request_on_a_4k_volume_never_reaches_the_ring() {
    let (ring, calls, eng) = setup(8, 8, 1, true);
    ring.submit(at(
        1,
        0,
        BlockOp::Write { fua: false },
        SECTOR,
        vec![1; SECTOR],
    ));
    assert_eq!(calls.wait_for(1), vec![(1, BlockStatus::Internal)]);
    assert!(eng.seen.is_empty());
}
