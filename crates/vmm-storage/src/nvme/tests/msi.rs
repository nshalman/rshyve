// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Interrupts for a batch of completions.

use super::*;

/// Three completions for queue 1 and two for queue 2, finished by the
/// backend in one call. Returns the vectors raised by that call.
fn one_batch(policy: MsiPolicy) -> (Harness, Vec<u64>) {
    let manual = Manual::new(IO_QDEPTH);
    let config = NvmeConfig {
        msi_policy: policy,
        ..NvmeConfig::default()
    };
    let mut h = Harness::manual_with(&manual, config);
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

/// A reset clears the pending bits once it has drained, so an MSI
/// still on its way would set one after it for a driver that is gone.
#[test]
fn a_reset_waits_for_an_msi_on_its_way() {
    let manual = Manual::new(IO_QDEPTH);
    let mut h = Harness::manual(&manual);
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);
    let mut tail = 0;
    submit_on(&h, 1, SQ1, &mut tail, Sqe::read(0, 1, 0x50000, 0), 0x41);

    let before = h.msi_count();
    h.sink.close_gate();
    let completer = {
        let manual = Arc::clone(&manual);
        std::thread::spawn(move || manual.complete_all())
    };
    h.sink.wait_sent(before + 1);

    let resetter = {
        let ctrl = Arc::clone(&h.ctrl);
        std::thread::spawn(move || {
            let wo = WriteOp::from_buf(&0u32.to_le_bytes());
            ctrl.bar_rw(BarN::BAR0, REG_CC, RWOp::Write(&wo));
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    let early = resetter.is_finished();
    h.sink.open_gate();
    join_within(completer, WAIT);
    join_within(resetter, WAIT);

    assert!(!early, "the reset finished under an MSI on its way");
    assert_eq!(h.csts() & CSTS_RDY, 0);
}

/// The quiesce query reads two counters without the lock. From the
/// moment a command leaves flight until its MSI is out, one of them
/// must be up. The gap is a few instructions wide, so this samples it
/// many times.
#[test]
fn quiet_is_never_reported_before_the_msi_goes_out() {
    let manual = Manual::new(IO_QDEPTH);
    let mut h = Harness::manual(&manual);
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);

    let mut tail = 0;
    for round in 0..2000u32 {
        h.ctrl.resume();
        let cid = (round % u32::from(IO_QDEPTH)) as u16;
        submit_on(&h, 1, SQ1, &mut tail, Sqe::read(0, 1, 0x50000, 0), cid);
        let req = manual.take().pop().expect("one request");
        h.ctrl.pause();
        let sent = h.msi_count();

        let ready = Arc::new(AtomicBool::new(false));
        let poller = {
            let ctrl = Arc::clone(&h.ctrl);
            let sink = Arc::clone(&h.sink);
            let ready = Arc::clone(&ready);
            std::thread::spawn(move || {
                ready.store(true, Ordering::Release);
                while !ctrl.is_quiesced() {
                    std::hint::spin_loop();
                }
                sink.sent.lock().expect("sink lock").len()
            })
        };
        while !ready.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
        manual.sink().complete(&[(req.id(), BlockStatus::Success)]);
        let seen = join_within(poller, WAIT);
        assert_eq!(seen, sent + 1, "quiet before the MSI, round {round}");
        h.cq_doorbell(1, tail);
    }
}
