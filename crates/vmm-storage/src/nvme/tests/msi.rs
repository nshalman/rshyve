// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Interrupts for a batch of completions.

use super::*;

/// Three completions for queue 1 and two for queue 2, finished by the
/// backend in one call. Returns the vectors raised by that call.
fn one_batch(policy: MsiPolicy) -> (Harness, Vec<u64>) {
    let manual = Manual::new(IO_QDEPTH);
    let mut h = Harness::manual_with(&manual, policy);
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);
    h.create_io_queues(2, IO_QDEPTH, SQ2, CQ2);
    let (mut t1, mut t2) = (0, 0);
    for cid in 0..3 {
        submit_on(&h, 1, SQ1, &mut t1, Sqe::read(0, 1, 0x50000, 0), cid);
    }
    for cid in 0..2 {
        submit_on(&h, 2, SQ2, &mut t2, Sqe::read(0, 1, 0x60000, 0), cid);
    }

    let before = h.msi_count();
    manual.complete_all();
    let raised = h.msi_vectors().split_off(before);
    (h, raised)
}

#[test]
fn a_batch_raises_one_msi_per_queue() {
    let (h, mut raised) = one_batch(MsiPolicy::PerBatch);
    raised.sort_unstable();
    assert_eq!(raised, vec![1, 2]);
    for slot in 0..3 {
        assert_eq!(h.wait_cqe(CQ1, slot, true).status(), Status::SUCCESS);
    }
}

#[test]
fn the_per_completion_switch_raises_one_msi_per_completion() {
    let (_h, mut raised) = one_batch(MsiPolicy::PerCompletion);
    raised.sort_unstable();
    assert_eq!(raised, vec![1, 1, 1, 2, 2]);
}

/// The guest's handler runs on the MSI and reads the CQ at once; a
/// raise under the lock makes the doorbell it rings wait for the lock.
#[test]
fn msis_go_out_with_the_controller_lock_released() {
    let (h, raised) = one_batch(MsiPolicy::PerCompletion);
    assert!(!raised.is_empty());
    assert_eq!(h.sink.under_lock.load(Ordering::Relaxed), 0);

    // The probe does see a send made under the lock.
    let held = h.ctrl.state.lock().expect("state lock");
    h.ctrl.msix.fire(1);
    drop(held);
    assert_eq!(h.sink.under_lock.load(Ordering::Relaxed), 1);
}

#[test]
fn admin_completions_raise_outside_the_lock_too() {
    let mut h = Harness::new();
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);
    let mut tail = 0;
    io_submit(&mut h, &mut tail, Sqe::read(0, 1, 0x50000, 0), 0x21);
    h.wait_cqe(CQ1, 0, true);

    assert_eq!(h.msi_count(), 3, "two admin completions and one I/O");
    assert_eq!(h.sink.under_lock.load(Ordering::Relaxed), 0);
}
