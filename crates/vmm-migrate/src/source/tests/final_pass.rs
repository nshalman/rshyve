// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The pause, the drain and the final pass, with fakes for the guest.

use super::*;

const RAM: usize = 16 * PAGE_SIZE;

fn page(n: u64) -> u64 {
    n * PAGE_SIZE as u64
}

/// A destination that takes exactly one pass, the final one.
fn final_destination(
    mut dest: Chan<UnixStream>,
) -> std::thread::JoinHandle<MemCtx> {
    use crate::destination::{receive_pass, PassEnd};
    std::thread::spawn(move || {
        let memctx = MemCtx::new(Arc::new(
            vmm_core::mem::PhysMap::new_anon(0, RAM).expect("guest memory"),
        ));
        receive_pass(
            &mut dest,
            &memctx,
            &crate::NoStop,
            &status(),
            PassEnd::MemEnd,
        )
        .expect("final pass");
        memctx
    })
}

fn no_owners(_: &()) -> Owners {
    Owners::new(&[], &[])
}

/// The pause, the final pass and the check, as `run_inner` runs them.
fn pause_and_pass(
    memctx: &MemCtx,
    track: &mut TrackFn<'_>,
    pause_vm: &VmPauseFn,
    drain: DevicePrePauseFn,
    export: impl FnOnce() -> Result<(), MigrateError>,
) -> (Result<(), MigrateError>, MemCtx) {
    let (mut source, dest) = pair();
    let peer = final_destination(dest);
    let gate = Gate::default();
    let paused = AtomicBool::new(false);
    pause_guest(pause_vm, drain, &paused, &gate).expect("pause");
    assert!(paused.load(Ordering::Acquire));
    final_pass(
        &mut source,
        track,
        memctx,
        &gate,
        &status(),
        export,
        no_owners,
        &discard(),
    )
    .expect("final pass");
    let clean = check_clean(track, memctx, &Owners::new(&[], &[]));
    (clean, peer.join().expect("destination"))
}

#[test]
fn the_vcpus_pause_before_the_devices_drain() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let by_vm = Arc::clone(&order);
    let by_devices = Arc::clone(&order);
    let pause_vm: VmPauseFn = Box::new(move || {
        by_vm.lock().expect("order").push("vcpus");
        Ok(())
    });
    let drain: DevicePrePauseFn = Box::new(move || {
        by_devices.lock().expect("order").push("devices");
        Ok(())
    });
    let paused = AtomicBool::new(false);
    pause_guest(&pause_vm, drain, &paused, &Gate::default()).expect("pause");
    assert_eq!(*order.lock().expect("order"), ["vcpus", "devices"]);
}

#[test]
fn a_write_during_the_drain_goes_out_in_the_final_pass() {
    // A request in flight at the pause completes in the drain and
    // writes its CQ entry. The guest must find it on the destination.
    let map = Arc::new(
        vmm_core::mem::PhysMap::new_anon(0, RAM).expect("guest memory"),
    );
    let memctx = MemCtx::new(Arc::clone(&map));
    let dirty = FakeDirty::default();
    let pause_vm: VmPauseFn = Box::new(|| Ok(()));
    let by_drain = dirty.clone();
    let drain: DevicePrePauseFn = Box::new(move || {
        by_drain.guest_write(&MemCtx::new(map), page(7), 0xC7);
        Ok(())
    });
    let mut track = |gpa, len, bm: &mut [u8]| dirty.track(gpa, len, bm);

    let (clean, dest) =
        pause_and_pass(&memctx, &mut track, &pause_vm, drain, || Ok(()));

    clean.expect("nothing written after the pass");
    assert_eq!(image(&dest, RAM), image(&memctx, RAM));
}

#[test]
fn a_write_by_the_export_goes_out_in_the_second_round() {
    // A read through the tracked view marks the page dirty after the
    // first round. The second round sends it while the guest is paused.
    let memctx = firmware_layout(RAM);
    let dirty = FakeDirty::default();
    dirty.guest_write(&memctx, page(2), 0x22);
    let pause_vm: VmPauseFn = Box::new(|| Ok(()));
    let drain: DevicePrePauseFn = Box::new(|| Ok(()));
    let mut track = |gpa, len, bm: &mut [u8]| dirty.track(gpa, len, bm);

    let (clean, dest) =
        pause_and_pass(&memctx, &mut track, &pause_vm, drain, || {
            dirty.guest_write(&memctx, page(9), 0x99);
            Ok(())
        });

    clean.expect("the second round carried the export's write");
    assert_eq!(image(&dest, RAM), image(&memctx, RAM));
}

#[test]
fn a_writer_that_keeps_going_fails_the_commit() {
    // Something still writes while the guest is paused. One more round
    // cannot catch up with it, so the commit is refused.
    let memctx = firmware_layout(RAM);
    let dirty = FakeDirty::default();
    let pause_vm: VmPauseFn = Box::new(|| Ok(()));
    let drain: DevicePrePauseFn = Box::new(|| Ok(()));
    let mut track = |gpa, len, bm: &mut [u8]| {
        let found = dirty.track(gpa, len, bm);
        dirty.guest_write(&memctx, page(3), 0x33);
        found
    };

    let (clean, _) =
        pause_and_pass(&memctx, &mut track, &pause_vm, drain, || Ok(()));

    let error = clean.expect_err("a writer is still running");
    assert!(error.to_string().contains("1 guest pages"), "{error}");
    assert!(error.to_string().contains("0x3000"), "{error}");
}

#[test]
fn a_failed_vcpu_pause_leaves_the_devices_alone() {
    let drained = Arc::new(AtomicBool::new(false));
    let by_drain = Arc::clone(&drained);
    let pause_vm: VmPauseFn =
        Box::new(|| Err(std::io::Error::other("refused")));
    let drain: DevicePrePauseFn = Box::new(move || {
        by_drain.store(true, Ordering::Release);
        Ok(())
    });
    let paused = AtomicBool::new(false);

    pause_guest(&pause_vm, drain, &paused, &Gate::default())
        .expect_err("the vCPUs would not pause");
    assert!(!drained.load(Ordering::Acquire));

    let resumed = Arc::new(AtomicBool::new(false));
    let by_resume = Arc::clone(&resumed);
    let _ = finish_source(
        Err(MigrateError::Io("pause: refused".into())),
        MigrationPhase::Pause,
        GateState::None,
        &discard(),
        Rollback {
            devices: Box::new(move || by_resume.store(true, Ordering::Release)),
            devices_paused: paused.load(Ordering::Acquire),
        },
        || Ok::<(), &'static str>(()),
    );
    assert!(!resumed.load(Ordering::Acquire), "resumed a running device");
}

#[test]
fn a_failed_drain_resumes_the_devices() {
    // Some devices paused before the one that failed. They have to run
    // again before the vCPUs do.
    let pause_vm: VmPauseFn = Box::new(|| Ok(()));
    let drain: DevicePrePauseFn = Box::new(|| {
        Err(DeviceStateError::Export("devices did not quiesce".into()))
    });
    let paused = AtomicBool::new(false);
    pause_guest(&pause_vm, drain, &paused, &Gate::default())
        .expect_err("drain failed");
    assert!(paused.load(Ordering::Acquire));
}
