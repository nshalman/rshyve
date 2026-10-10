// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::{Arc, Mutex};

use super::*;

type Log = Arc<Mutex<Vec<String>>>;

struct Member {
    name: &'static str,
    log: Log,
    refuse: bool,
}

impl KernelRings for Member {
    fn stop(&self) -> Result<(), String> {
        self.log.lock().unwrap().push(format!("stop {}", self.name));
        if self.refuse {
            return Err(format!("{} refused", self.name));
        }
        Ok(())
    }

    fn start(&self) {
        self.log
            .lock()
            .unwrap()
            .push(format!("start {}", self.name));
    }
}

fn member(gate: &RingGate, log: &Log, name: &'static str) -> Arc<Member> {
    let m = Arc::new(Member {
        name,
        log: Arc::clone(log),
        refuse: false,
    });
    gate.register(Arc::downgrade(&m) as Weak<dyn KernelRings>)
        .expect("register");
    m
}

fn take(log: &Log) -> Vec<String> {
    std::mem::take(&mut *log.lock().unwrap())
}

#[test]
fn a_run_stops_every_member_before_the_call_and_starts_them_after() {
    let gate = RingGate::new();
    let log = Log::default();
    let _a = member(&gate, &log, "a");
    let _b = member(&gate, &log, "b");

    let l = Arc::clone(&log);
    gate.run(|_| l.lock().unwrap().push("ioctl".into()))
        .expect("run");

    assert_eq!(
        take(&log),
        ["stop a", "stop b", "ioctl", "start a", "start b"]
    );
}

#[test]
fn a_refusal_runs_nothing_and_starts_the_members_already_stopped() {
    let gate = RingGate::new();
    let log = Log::default();
    let _a = member(&gate, &log, "a");
    let b = Arc::new(Member {
        name: "b",
        log: Arc::clone(&log),
        refuse: true,
    });
    gate.register(Arc::downgrade(&b) as Weak<dyn KernelRings>)
        .expect("register");

    let ran = gate.run(|_| panic!("the ioctl ran with a ring alive"));

    assert!(matches!(ran, Err(GateError::Busy(_))));
    assert_eq!(take(&log), ["stop a", "stop b", "start a"]);
}

#[test]
fn a_run_under_a_hold_stops_nothing_again() {
    let gate = RingGate::new();
    let log = Log::default();
    let _a = member(&gate, &log, "a");

    let hold = gate.hold().expect("hold");
    assert!(gate.is_held());
    gate.run(|_| ()).expect("run");
    gate.run(|_| ()).expect("run");
    assert_eq!(take(&log), ["stop a"]);

    drop(hold);
    assert_eq!(take(&log), ["start a"]);
    assert!(!gate.is_held());
}

#[test]
fn the_last_hold_starts_the_rings() {
    let gate = RingGate::new();
    let log = Log::default();
    let _a = member(&gate, &log, "a");

    let first = gate.hold().expect("hold");
    let second = gate.hold().expect("hold");
    drop(first);
    assert_eq!(take(&log), ["stop a"]);
    drop(second);
    assert_eq!(take(&log), ["start a"]);
}

#[test]
fn a_refused_hold_holds_nothing() {
    let gate = RingGate::new();
    let log = Log::default();
    let b = Arc::new(Member {
        name: "b",
        log: Arc::clone(&log),
        refuse: true,
    });
    gate.register(Arc::downgrade(&b) as Weak<dyn KernelRings>)
        .expect("register");

    assert!(gate.hold().is_err());
    assert!(!gate.is_held());
}

#[test]
fn a_member_added_under_a_hold_is_stopped_at_once() {
    let gate = RingGate::new();
    let log = Log::default();
    let hold = gate.hold().expect("hold");

    let _a = member(&gate, &log, "a");
    assert_eq!(take(&log), ["stop a"]);
    drop(hold);
    assert_eq!(take(&log), ["start a"]);
}

#[test]
fn a_gone_member_is_skipped() {
    let gate = RingGate::new();
    let log = Log::default();
    drop(member(&gate, &log, "a"));
    let _b = member(&gate, &log, "b");

    gate.run(|_| ()).expect("run");

    assert_eq!(take(&log), ["stop b", "start b"]);
}

#[test]
fn the_gate_refuses_itself_rather_than_deadlock() {
    let gate = RingGate::new();
    let inner = gate.run(|_| gate.run(|_| ()));
    assert_eq!(inner, Ok(Err(GateError::Reentered)));
    let inner = gate.run(|_| gate.hold().err());
    assert_eq!(inner, Ok(Some(GateError::Reentered)));
}

#[test]
fn a_proof_names_its_gate() {
    let mine = RingGate::new();
    let other = RingGate::new();
    mine.run(|rings| {
        assert!(mine.proves(rings));
        assert!(!other.proves(rings));
    })
    .expect("run");
}

#[test]
fn a_vm_pause_stops_the_rings_first_and_starts_them_after_the_resume() {
    // VM_PAUSE and VM_RESUME take the write lock, and every ioctl in
    // between does too. A guest kick must not start a ring meanwhile.
    let gate = RingGate::new();
    let log = Log::default();
    let _a = member(&gate, &log, "a");
    let paused = PausedRings::default();

    let l = Arc::clone(&log);
    paused
        .pause(&gate, |_| {
            l.lock().unwrap().push("VM_PAUSE".into());
            Ok(())
        })
        .expect("pause");
    gate.run(|_| log.lock().unwrap().push("export".into()))
        .expect("run");
    let l = Arc::clone(&log);
    paused
        .resume(&gate, |_| {
            l.lock().unwrap().push("VM_RESUME".into());
            Ok(())
        })
        .expect("resume");

    assert_eq!(
        take(&log),
        ["stop a", "VM_PAUSE", "export", "VM_RESUME", "start a"]
    );
}

#[test]
fn a_refused_pause_leaves_the_rings_running() {
    let gate = RingGate::new();
    let log = Log::default();
    let _a = member(&gate, &log, "a");
    let paused = PausedRings::default();

    paused
        .pause(&gate, |_| Err(std::io::Error::other("EBUSY")))
        .expect_err("refused");

    assert_eq!(take(&log), ["stop a", "start a"]);
    assert!(!gate.is_held());
}

#[test]
fn a_pause_whose_rings_will_not_stop_runs_no_ioctl() {
    let gate = RingGate::new();
    let log = Log::default();
    let b = Arc::new(Member {
        name: "b",
        log: Arc::clone(&log),
        refuse: true,
    });
    gate.register(Arc::downgrade(&b) as Weak<dyn KernelRings>)
        .expect("register");
    let paused = PausedRings::default();

    paused
        .pause(&gate, |_| panic!("VM_PAUSE with a ring alive"))
        .expect_err("the rings did not stop");
}

#[test]
fn a_refused_resume_keeps_the_rings_stopped_with_the_vm() {
    let gate = RingGate::new();
    let log = Log::default();
    let _a = member(&gate, &log, "a");
    let paused = PausedRings::default();
    paused.pause(&gate, |_| Ok(())).expect("pause");
    take(&log);

    paused
        .resume(&gate, |_| Err(std::io::Error::other("EIO")))
        .expect_err("refused");

    assert_eq!(take(&log), Vec::<String>::new());
    assert!(gate.is_held());
}
