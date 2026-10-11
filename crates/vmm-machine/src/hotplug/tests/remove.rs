// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Removal: what the guest is asked, and what an eject tears down.

use super::*;

#[test]
fn a_removal_request_asks_the_guest_and_tears_nothing_down() {
    let fixture = Fixture::new();
    let id = fixture.add("5,virtio-blk,/disk").expect("added");

    fixture
        .core
        .request_remove(&id)
        .expect("a hot-added device");

    assert_eq!(
        fixture.registry.get_by_id(&id).expect("still there").state,
        SlotState::RemovePending,
    );
    assert!(!fixture.slot_is_empty(5), "the device left before _EJ0");
    assert_eq!(fixture.slot_down_bits(), 1 << 5);
}

#[test]
fn a_boot_device_cannot_be_removed() {
    // Only a hot-added slot was ever advertised as ejectable, so a boot
    // device has no _EJ0 for the guest to run.
    let fixture = Fixture::new();
    fixture
        .registry
        .insert(RegisteredDevice::new(
            "virtio-blk@4",
            Bdf::new(0, 4, 0),
            None,
            None,
            Some("4,virtio-blk,/disk".to_string()),
        ))
        .expect("a boot device");

    let err = fixture
        .core
        .request_remove("virtio-blk@4")
        .expect_err("argv devices stay");

    assert_eq!(
        err,
        HotplugError::NotHotpluggable("virtio-blk@4".to_string())
    );
}

#[test]
fn an_unknown_id_is_refused() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.core.request_remove("ghost").expect_err("no device"),
        HotplugError::NoSuchDevice("ghost".to_string()),
    );
}

#[test]
fn a_second_request_asks_the_guest_again() {
    // The guest may not have had a driver bound the first time. A
    // repeat must re-raise the event, not fail.
    let fixture = Fixture::new();
    let id = fixture.add("5,virtio-blk,/disk").expect("added");
    fixture.core.request_remove(&id).expect("first request");
    assert_eq!(fixture.slot_down_bits(), 1 << 5);

    fixture.core.request_remove(&id).expect("second request");

    assert_eq!(fixture.slot_down_bits(), 1 << 5, "the guest was not asked");
    assert_eq!(
        fixture.registry.get_by_id(&id).expect("still there").state,
        SlotState::RemovePending,
    );
}

#[test]
fn a_guest_eject_no_operator_asked_for_is_refused() {
    // A guest that ejects on its own must not take a device out of the
    // machine.
    let fixture = Fixture::new();
    let id = fixture.add("5,virtio-blk,/disk").expect("added");

    fixture.guest_ejects(5);
    fixture.core.drain_ejects();

    let device = fixture.registry.get_by_id(&id).expect("still registered");
    assert_eq!(device.state, SlotState::Present);
    assert!(!fixture.slot_is_empty(5));
    // The slot is offered back, so the guest re-binds its driver and
    // the slot is ejectable again.
    assert_eq!(fixture.slot_up_bits(), 1 << 5);
}

#[test]
fn an_eject_changes_the_bus_under_the_topology_lock() {
    // A pause, resume or migration claim reads the device set under
    // this lock, so an eject must not change the set while one runs.
    let topology = TopologyLock::default();
    let mut fixture = Fixture::with_pause(RecordingPause {
        topology: Some(topology.clone()),
        ..RecordingPause::default()
    });
    fixture.core.topology = topology;
    let id = fixture.add("5,virtio-blk,/disk").expect("added");
    fixture.core.request_remove(&id).expect("requested");

    fixture.guest_ejects(5);
    fixture.core.drain_ejects();

    assert!(fixture.registry.get_by_id(&id).is_none(), "not ejected");
    assert!(fixture.vm.paused_under_topology.load(Ordering::Acquire));
}

#[test]
fn a_held_eject_waits_until_the_hold_ends() {
    // A migration import restores state into the device set it was
    // admitted with. An eject during the import would take one away.
    let fixture = Fixture::new();
    let id = fixture.add("5,virtio-blk,/disk").expect("added");
    fixture.core.request_remove(&id).expect("requested");
    let topology = fixture.core.topology.clone();
    topology.hold_ejects(&topology.lock(), true);

    fixture.guest_ejects(5);
    fixture.core.drain_ejects();
    assert!(
        fixture.registry.get_by_id(&id).is_some(),
        "ejected while held"
    );

    topology.hold_ejects(&topology.lock(), false);
    fixture.core.drain_ejects();
    assert!(
        fixture.registry.get_by_id(&id).is_none(),
        "the eject was lost"
    );
}

#[test]
fn an_idle_drain_does_not_take_the_topology_lock() {
    // The drain runs every tick, and a pause or migration claim holds
    // this lock across slow device work.
    let fixture = Fixture::new();
    let topology = fixture.core.topology.clone();
    let let_go = AtomicBool::new(false);
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let (topology, let_go_ref) = (&topology, &let_go);
    std::thread::scope(|s| {
        s.spawn(move || {
            let held = topology.lock();
            held_tx.send(()).expect("held");
            // Held until the drain is done; the timeout only keeps a
            // drain that waits from hanging the test.
            let _ = done_rx.recv_timeout(Duration::from_secs(60));
            let_go_ref.store(true, Ordering::Release);
            drop(held);
        });
        held_rx.recv().expect("the lock is held");
        fixture.core.drain_ejects();
        let waited = let_go.load(Ordering::Acquire);
        let _ = done_tx.send(());
        assert!(!waited, "an empty drain waited on the topology lock");
    });
}

#[test]
fn a_poisoned_free_topology_lock_is_not_held() {
    let topology = TopologyLock::default();
    let poisoner = topology.clone();
    let _ = std::thread::spawn(move || {
        let _guard = poisoner.lock();
        panic!("poison the lock");
    })
    .join();

    assert!(!topology.is_held());
    let _guard = topology.lock();
    let other = topology.clone();
    assert!(std::thread::spawn(move || other.is_held())
        .join()
        .expect("join"));
}

#[test]
fn an_eject_the_operator_asked_for_tears_the_device_down() {
    let fixture = Fixture::new();
    let lifecycle = StubLifecycle::new(true);
    let (id, device, _calls) =
        fixture.add_with("5,virtio-blk,/disk", Arc::clone(&lifecycle));
    let id = id.expect("added");
    fixture.core.request_remove(&id).expect("requested");

    fixture.guest_ejects(5);
    fixture.core.drain_ejects();

    assert!(
        fixture.registry.get_by_id(&id).is_none(),
        "still registered"
    );
    assert!(fixture.slot_is_empty(5), "still on the bus");
    assert_eq!(device.regions_detached.load(Ordering::Acquire), 1);
    assert!(lifecycle.paused.load(Ordering::Acquire));
    assert!(lifecycle.halted.load(Ordering::Acquire));
    assert_eq!(fixture.vm.pauses.load(Ordering::Acquire), 1);
    assert_eq!(fixture.vm.resumes.load(Ordering::Acquire), 1);
}

#[test]
fn a_device_with_in_flight_work_keeps_its_slot() {
    // WHY: tearing a device out while it still holds DMA would corrupt
    // guest memory. The eject is abandoned, never forced.
    let fixture = Fixture::new();
    let lifecycle = StubLifecycle::new(false);
    let (id, device, _calls) =
        fixture.add_with("5,virtio-blk,/disk", Arc::clone(&lifecycle));
    let id = id.expect("added");
    fixture.core.request_remove(&id).expect("requested");

    fixture.guest_ejects(5);
    fixture.core.drain_ejects();

    let record = fixture.registry.get_by_id(&id).expect("still registered");
    // The removal did not happen, so the slot must not keep reporting
    // one. The operator asks again if they still want it.
    assert_eq!(record.state, SlotState::Present);
    assert!(!fixture.slot_is_empty(5));
    assert!(!lifecycle.halted.load(Ordering::Acquire), "halted anyway");
    assert!(lifecycle.is_running(), "left paused");
    assert_eq!(device.regions_detached.load(Ordering::Acquire), 0);
    assert_eq!(fixture.vm.pauses.load(Ordering::Acquire), 0);
    // The slot is ejectable again, so a later _EJ0 can retry.
    assert_eq!(fixture.slot_up_bits(), 1 << 5);
}

#[test]
fn an_aborted_eject_leaves_the_device_running() {
    // WHY: prepare_unplug pauses before it waits. An abort that walks
    // away leaves a device that never serves the guest again: not
    // halted, backing file still open, and silently dead.
    let fixture = Fixture::new();
    let lifecycle = StubLifecycle::new(false);
    let (id, _device, _calls) =
        fixture.add_with("5,virtio-blk,/disk", Arc::clone(&lifecycle));
    let id = id.expect("added");
    fixture.core.request_remove(&id).expect("requested");

    fixture.guest_ejects(5);
    fixture.core.drain_ejects();

    assert!(lifecycle.is_running(), "the device was left paused");
    assert_eq!(lifecycle.resumes.load(Ordering::Acquire), 1);
    assert!(!lifecycle.halted.load(Ordering::Acquire));
}

#[test]
fn an_aborted_eject_puts_the_slot_back_to_present() {
    // The operator's request did not take, so the slot holds a working
    // device again. Leaving it RemovePending would report a removal
    // that is not happening, and would keep it out of a reboot replay.
    let fixture = Fixture::new();
    let lifecycle = StubLifecycle::new(false);
    let (id, _device, _calls) =
        fixture.add_with("5,virtio-blk,/disk", Arc::clone(&lifecycle));
    let id = id.expect("added");
    fixture.core.request_remove(&id).expect("requested");

    fixture.guest_ejects(5);
    fixture.core.drain_ejects();

    let record = fixture.registry.get_by_id(&id).expect("still registered");
    assert_eq!(record.state, SlotState::Present);
    assert_eq!(hotplug_specs(&fixture.registry), ["5,virtio-blk,/disk"]);
    // A second request must still be accepted, which needs the Present
    // edge the first one took.
    fixture.core.request_remove(&id).expect("a second request");
    assert_eq!(
        fixture.registry.get_by_id(&id).expect("still there").state,
        SlotState::RemovePending,
    );
}

#[test]
fn an_aborted_eject_does_not_resume_a_device_the_operator_paused() {
    // Indicator::resume panics on anything but Pause, and a VM the
    // operator stopped must not be started from here.
    let fixture = Fixture::new();
    let lifecycle = StubLifecycle::already_paused();
    let (id, _device, _calls) =
        fixture.add_with("5,virtio-blk,/disk", Arc::clone(&lifecycle));
    let id = id.expect("added");
    fixture.core.request_remove(&id).expect("requested");

    fixture.guest_ejects(5);
    fixture.core.drain_ejects();

    assert_eq!(
        lifecycle.resumes.load(Ordering::Acquire),
        0,
        "the engine started a device it did not pause",
    );
    assert!(!lifecycle.halted.load(Ordering::Acquire));
}

#[test]
fn one_slot_is_torn_down_once_however_often_the_eject_runs() {
    // The CAS out of RemovePending is the only guard against two
    // teardowns of one device. A second pass must reach nothing.
    let fixture = Fixture::new();
    let lifecycle = StubLifecycle::new(true);
    let (id, device, _calls) =
        fixture.add_with("5,virtio-blk,/disk", Arc::clone(&lifecycle));
    let id = id.expect("added");
    fixture.core.request_remove(&id).expect("requested");

    fixture.core.eject(5);
    fixture.core.eject(5);
    fixture.core.eject(5);

    assert_eq!(device.regions_detached.load(Ordering::Acquire), 1);
    assert_eq!(fixture.vm.pauses.load(Ordering::Acquire), 1);
    assert_eq!(fixture.vm.resumes.load(Ordering::Acquire), 1);
    assert!(fixture.registry.get_by_id(&id).is_none());
}

#[test]
fn a_backend_with_no_pci_face_is_stopped_when_the_add_is_refused() {
    // WHY: dropping the handle does not join a worker. A backend the
    // catalog built and the engine refused would hold its backing file
    // open for the life of the VM.
    let fixture = Fixture::new();
    let lifecycle = StubLifecycle::new(true);
    let mut build = |_spec: &str| {
        Ok((None, Some(Arc::clone(&lifecycle) as Arc<dyn Lifecycle>)))
    };

    let err = fixture
        .core
        .add("5,virtio-blk,/disk", &mut build)
        .expect_err("no PCI device was built");

    assert!(matches!(err, HotplugError::NothingBuilt(_)));
    assert!(fixture.registry.is_empty());
    assert!(
        lifecycle.halted.load(Ordering::Acquire),
        "the backend was left running",
    );
}

#[test]
fn a_device_that_cannot_be_quiesced_keeps_its_slot() {
    // passthru carries no Lifecycle, so nothing reports that its DMA
    // has stopped.
    let fixture = Fixture::new();
    let device = Arc::new(StubPci::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let mut build =
        fixture.builder(Arc::clone(&device), None, Arc::clone(&calls));
    let id = fixture
        .core
        .add("5,passthru,/dev/ppt0", &mut build)
        .expect("added");
    fixture.core.request_remove(&id).expect("requested");

    fixture.guest_ejects(5);
    fixture.core.drain_ejects();

    assert!(fixture.registry.get_by_id(&id).is_some());
    assert!(!fixture.slot_is_empty(5));
}

#[test]
fn an_eject_of_an_empty_slot_does_nothing() {
    let fixture = Fixture::new();

    fixture.core.eject(9);

    assert!(fixture.registry.is_empty());
    assert_eq!(fixture.vm.pauses.load(Ordering::Acquire), 0);
}

#[test]
fn a_slot_can_be_reused_after_its_device_leaves() {
    let fixture = Fixture::new();
    let id = fixture.add("5,virtio-blk,/first").expect("added");
    fixture.core.request_remove(&id).expect("requested");
    fixture.guest_ejects(5);
    fixture.core.drain_ejects();

    let second = fixture.add("5,virtio-blk,/second").expect("slot 5 is free");

    assert_eq!(second, id);
    assert_eq!(
        fixture
            .registry
            .get_by_id(&second)
            .expect("registered")
            .spec
            .as_deref(),
        Some("5,virtio-blk,/second"),
    );
}
