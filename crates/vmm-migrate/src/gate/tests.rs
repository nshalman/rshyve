// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use super::*;

fn discard() -> slog::Logger {
    slog::Logger::root(slog::Discard, slog::o!())
}

fn status() -> Arc<Mutex<MigrationStatus>> {
    Arc::new(Mutex::new(MigrationStatus::default()))
}

fn gate_of(status: &Arc<Mutex<MigrationStatus>>) -> GateState {
    status.lock().expect("status").gate
}

/// A stop the test can fire, which shuts what the wait registered.
#[derive(Default)]
struct Stop {
    fired: AtomicBool,
    shut: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
}

impl Stop {
    fn fire(&self) {
        self.fired.store(true, Ordering::Release);
        for shut in self.shut.lock().expect("shut").drain(..) {
            shut();
        }
    }
}

impl StopGate for Stop {
    fn cancelled(&self) -> bool {
        self.fired.load(Ordering::Acquire)
    }

    fn commit(&self) -> Result<(), MigrateError> {
        Ok(())
    }

    fn on_stop(&self, shut: Box<dyn FnOnce() + Send>) {
        self.shut.lock().expect("shut").push(shut);
    }
}

/// An agent that reads READY, writes `answer` as is, and returns every
/// line it read.
fn agent(
    answer: &'static [u8],
) -> (tempfile::TempDir, PathBuf, thread::JoinHandle<Vec<String>>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("gate.sock");
    let listener = UnixListener::bind(&path).expect("bind");
    let t = thread::spawn(move || {
        let (s, _) = listener.accept().expect("accept");
        let mut w = s.try_clone().expect("clone");
        let mut lines = Vec::new();
        let mut r = BufReader::new(s);
        let mut line = String::new();
        r.read_line(&mut line).expect("READY");
        lines.push(line.trim_end().to_string());
        if answer.is_empty() {
            return lines;
        }
        w.write_all(answer).expect("answer");
        loop {
            let mut line = String::new();
            match r.read_line(&mut line) {
                Ok(0) | Err(_) => return lines,
                Ok(_) => lines.push(line.trim_end().to_string()),
            }
        }
    });
    (dir, path, t)
}

#[test]
fn go_lets_the_migration_go_on_and_is_acknowledged() {
    let (_dir, path, agent) = agent(b"GO\n");
    let status = status();
    wait(&path, DEFAULT_BUDGET, &Stop::default(), &status, &discard())
        .expect("go");
    assert_eq!(gate_of(&status), GateState::Go);
    assert_eq!(
        agent.join().expect("agent"),
        ["READY rshyve-gate/1", "ACK GO"]
    );
}

#[test]
fn abort_fails_the_migration_and_is_acknowledged() {
    let (_dir, path, agent) = agent(b"ABORT\n");
    let status = status();
    let e = wait(&path, DEFAULT_BUDGET, &Stop::default(), &status, &discard())
        .expect_err("abort");
    assert!(e.to_string().contains("abort"), "{e}");
    assert_eq!(gate_of(&status), GateState::Abort);
    assert_eq!(
        agent.join().expect("agent"),
        ["READY rshyve-gate/1", "ACK ABORT"]
    );
}

#[test]
fn anything_but_an_exact_go_is_an_abort() {
    for answer in [&b"go\n"[..], b"GO \n", b" GO\n", b"GOO\n", b"\n", b"GO\r\n"]
    {
        let (_dir, path, agent) = agent(answer);
        let status = status();
        let e =
            wait(&path, DEFAULT_BUDGET, &Stop::default(), &status, &discard())
                .expect_err("not a go");
        assert_eq!(gate_of(&status), GateState::Abort, "{e}");
        agent.join().expect("agent");
    }
}

#[test]
fn a_long_line_is_an_abort() {
    let long: &'static [u8] = Box::leak(vec![b'G'; 200].into_boxed_slice());
    let (_dir, path, agent) = agent(long);
    let status = status();
    let e = wait(&path, DEFAULT_BUDGET, &Stop::default(), &status, &discard())
        .expect_err("too long");
    assert!(e.to_string().contains("too long"), "{e}");
    drop(agent);
}

#[test]
fn a_closed_gate_is_an_abort() {
    let (_dir, path, agent) = agent(b"");
    let status = status();
    let e = wait(&path, DEFAULT_BUDGET, &Stop::default(), &status, &discard())
        .expect_err("closed");
    assert!(e.to_string().contains("no answer"), "{e}");
    assert_eq!(gate_of(&status), GateState::Abort);
    agent.join().expect("agent");
}

#[test]
fn no_agent_is_an_abort() {
    let dir = tempfile::tempdir().expect("tempdir");
    let status = status();
    let e = wait(
        &dir.path().join("none.sock"),
        DEFAULT_BUDGET,
        &Stop::default(),
        &status,
        &discard(),
    )
    .expect_err("nobody listens");
    assert!(e.to_string().contains("gate connect"), "{e}");
    assert_eq!(gate_of(&status), GateState::Abort);
}

#[test]
fn a_silent_agent_is_an_abort_at_the_budget() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("gate.sock");
    let _listener = UnixListener::bind(&path).expect("bind");
    let status = status();
    let t = Instant::now();
    let e = wait(
        &path,
        Duration::from_millis(300),
        &Stop::default(),
        &status,
        &discard(),
    )
    .expect_err("silent");
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    assert!(e.to_string().contains("no answer"), "{e}");
    assert_eq!(gate_of(&status), GateState::Abort);
}

#[test]
fn a_stop_frees_the_wait_as_a_cancel() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("gate.sock");
    let _listener = UnixListener::bind(&path).expect("bind");
    let stop = Arc::new(Stop::default());
    let by_test = Arc::clone(&stop);
    let status = status();
    let waiting = Arc::clone(&status);
    let t = thread::spawn(move || {
        wait(&path, MAX_BUDGET, stop.as_ref(), &waiting, &discard())
    });
    thread::sleep(Duration::from_millis(100));
    let started = Instant::now();
    by_test.fire();
    let e = t.join().expect("wait").expect_err("stopped");
    assert!(crate::is_cancel(&e), "{e}");
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_ne!(gate_of(&status), GateState::Go);
}

#[test]
fn a_cancel_before_the_go_wins() {
    // migrate-cancel checks for a go under the same lock.
    let status = status();
    let stop = Stop::default();
    stop.fire();
    assert!(!take_go(&status, &stop));
    assert_ne!(gate_of(&status), GateState::Go);
    assert!(take_go(&status, &Stop::default()));
    assert_eq!(gate_of(&status), GateState::Go);
}

#[test]
fn the_parse_takes_two_words_only() {
    assert_eq!(parse("GO"), Verdict::Go);
    assert!(matches!(parse("ABORT"), Verdict::Abort(_)));
    let Verdict::Abort(why) = parse("GO\u{1b}[2J") else {
        panic!("a go with control bytes");
    };
    assert!(!why.contains('\u{1b}'), "{why}");
}
