// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::*;
use std::io::Write as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};

fn discard() -> slog::Logger {
    slog::Logger::root(slog::Discard, slog::o!())
}

/// A barrier socket that answers with `reply`, or with nothing when
/// `reply` is `None`.
fn barrier(reply: Option<u8>) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("barrier.sock");
    let listener = UnixListener::bind(&path).expect("bind");
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            if let Some(byte) = reply {
                let _ = stream.write_all(&[byte]);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    });
    (dir, path)
}

#[test]
fn a_reported_zfs_failure_fails_the_migration() {
    // The guest would resume on the destination having already
    // acknowledged writes the destination disk never received.
    let (_dir, path) = barrier(Some(1));
    let error = zfs_barrier_wait(
        &path,
        Duration::from_secs(5),
        &crate::NoStop,
        &discard(),
    )
    .expect_err("a non-zero status must fail");
    assert!(error.to_string().contains("reported failure"), "{error}");
}

#[test]
fn a_silent_zfs_barrier_fails_the_migration() {
    let (_dir, path) = barrier(None);
    let error = zfs_barrier_wait(
        &path,
        Duration::from_millis(200),
        &crate::NoStop,
        &discard(),
    )
    .expect_err("a timeout must fail");
    assert!(error.to_string().contains("reported nothing"), "{error}");
}

#[test]
fn a_missing_zfs_barrier_fails_the_migration() {
    let dir = tempfile::tempdir().expect("tempdir");
    let error = zfs_barrier_wait(
        &dir.path().join("absent.sock"),
        Duration::from_secs(1),
        &crate::NoStop,
        &discard(),
    )
    .expect_err("a missing socket must fail");
    assert!(error.to_string().contains("connect failed"), "{error}");
}

#[test]
fn an_agreeing_zfs_barrier_lets_the_migration_go_on() {
    let (_dir, path) = barrier(Some(ZFS_BARRIER_OK));
    zfs_barrier_wait(&path, Duration::from_secs(5), &crate::NoStop, &discard())
        .expect("status 0 is the go-ahead");
}

/// A gate a stop can cancel from another thread, shutting what was
/// registered.
#[derive(Default)]
struct Stoppable {
    stopped: AtomicBool,
    shut: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
}

impl Stoppable {
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        for shut in std::mem::take(&mut *self.shut.lock().expect("shut")) {
            shut();
        }
    }
}

impl StopGate for Stoppable {
    fn cancelled(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    fn commit(&self) -> Result<(), MigrateError> {
        Ok(())
    }

    fn on_stop(&self, shut: Box<dyn FnOnce() + Send>) {
        if self.cancelled() {
            shut();
            return;
        }
        self.shut.lock().expect("shut").push(shut);
    }
}

#[test]
fn a_stop_frees_a_wait_on_a_silent_zfs_barrier() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("barrier.sock");
    let listener = UnixListener::bind(&path).expect("bind");
    let (accepted_tx, accepted) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let _ = accepted_tx.send(());
            std::thread::sleep(Duration::from_secs(20));
            drop(stream);
        }
    });
    let gate = Arc::new(Stoppable::default());
    let stopper = Arc::clone(&gate);
    std::thread::spawn(move || {
        // Stopped only once connected, so the stop lands on the read.
        if accepted.recv_timeout(Duration::from_secs(10)).is_ok() {
            std::thread::sleep(Duration::from_millis(100));
        }
        stopper.stop();
    });

    let started = std::time::Instant::now();
    let error = zfs_barrier_wait(
        &path,
        Duration::from_secs(60),
        gate.as_ref(),
        &discard(),
    )
    .expect_err("stopped");

    assert!(crate::is_cancel(&error), "{error}");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn flush_failure_resumes_devices_and_vcpus() {
    let result: Result<(), MigrateError> =
        Err(MigrateError::Io("device flush failed".into()));
    let devices_resumed = Arc::new(AtomicBool::new(false));
    let vcpus_resumed = Arc::new(AtomicBool::new(false));
    let devices_resumed_by_callback = Arc::clone(&devices_resumed);
    let vcpus_resumed_by_callback = Arc::clone(&vcpus_resumed);
    let devices_resumed_before_vcpus = Arc::clone(&devices_resumed);
    let resume_fn: DeviceResumeFn = Box::new(move || {
        devices_resumed_by_callback.store(true, Ordering::Release);
    });

    let result = finish_source(
        result,
        MigrationPhase::Pause,
        &discard(),
        resume_fn,
        move || {
            assert!(devices_resumed_before_vcpus.load(Ordering::Acquire));
            vcpus_resumed_by_callback.store(true, Ordering::Release);
            Ok::<(), &'static str>(())
        },
    );

    assert!(result.is_err());
    assert!(devices_resumed.load(Ordering::Acquire));
    assert!(vcpus_resumed.load(Ordering::Acquire));
}

#[test]
fn a_committed_source_is_never_resumed() {
    // The destination may be running the guest. Two copies against
    // one disk is worse than a guest that needs an operator.
    let resumed = Arc::new(AtomicBool::new(false));
    let by_callback = Arc::clone(&resumed);
    let resume_fn: DeviceResumeFn = Box::new(move || {
        by_callback.store(true, Ordering::Release);
    });
    let result = finish_source(
        Err(MigrateError::Committed("no confirmation".into())),
        MigrationPhase::Committed,
        &discard(),
        resume_fn,
        || -> Result<(), &'static str> {
            panic!("a committed source must not resume its vCPUs")
        },
    );
    assert!(result.is_err());
    assert!(!resumed.load(Ordering::Acquire));
}

#[test]
fn a_failure_in_finish_still_rolls_back() {
    // Nothing was promised yet: the destination waits for the
    // commit before it starts the guest.
    let resumed = Arc::new(AtomicBool::new(false));
    let by_callback = Arc::clone(&resumed);
    let resume_fn: DeviceResumeFn = Box::new(move || {
        by_callback.store(true, Ordering::Release);
    });
    let result = finish_source(
        Err(MigrateError::WebSocket("closed".into())),
        MigrationPhase::Finish,
        &discard(),
        resume_fn,
        || Ok::<(), &'static str>(()),
    );
    assert!(result.is_err());
    assert!(resumed.load(Ordering::Acquire));
}

/// A gate a test can cancel, counting the commits it let through.
#[derive(Default)]
struct Gate {
    cancel: bool,
    commits: std::sync::atomic::AtomicUsize,
}

impl StopGate for Gate {
    fn cancelled(&self) -> bool {
        self.cancel
    }

    fn commit(&self) -> Result<(), MigrateError> {
        if self.cancel {
            return Err(crate::cancelled());
        }
        self.commits.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn on_stop(&self, _shut: Box<dyn FnOnce() + Send>) {}
}

/// A connected pair of channels, source end first.
fn pair() -> (Chan<UnixStream>, Chan<UnixStream>) {
    let (a, b) = UnixStream::pair().expect("socket pair");
    let deadlines = Deadlines {
        post_pause: Duration::from_secs(5),
        ..Deadlines::default()
    };
    (
        Chan::new(a, tungstenite::protocol::Role::Client, deadlines)
            .expect("source channel"),
        Chan::new(b, tungstenite::protocol::Role::Server, deadlines)
            .expect("destination channel"),
    )
}

fn status() -> Arc<Mutex<MigrationStatus>> {
    Arc::new(Mutex::new(MigrationStatus::default()))
}

fn phase(status: &Arc<Mutex<MigrationStatus>>) -> MigrationPhase {
    status.lock().expect("status").phase
}

#[test]
fn a_cancelled_source_never_commits() {
    let (mut source, mut dest) = pair();
    dest.send(&Message::Okay)
        .expect("the destination holds it all");
    let gate = Gate {
        cancel: true,
        ..Gate::default()
    };
    let status = status();

    let error = hand_over(&mut source, &gate, &status).expect_err("cancelled");

    assert!(crate::is_cancel(&error), "{error}");
    assert_eq!(phase(&status), MigrationPhase::Finish, "not rolled back");
    drop(source);
    assert!(dest.recv().is_err(), "the destination heard a commit");
}

#[test]
fn a_destination_abort_rolls_the_source_back() {
    let (mut source, mut dest) = pair();
    dest.send(&Message::Error(crate::cancelled()))
        .expect("abort");
    let gate = Gate::default();
    let status = status();

    let error = hand_over(&mut source, &gate, &status)
        .expect_err("the destination aborted");

    assert_eq!(gate.commits.load(Ordering::Acquire), 0, "committed");
    assert_eq!(phase(&status), MigrationPhase::Finish);
    let rolled_back = Arc::new(AtomicBool::new(false));
    let by_callback = Arc::clone(&rolled_back);
    let _ = finish_source(
        Err(error),
        phase(&status),
        &discard(),
        Box::new(move || by_callback.store(true, Ordering::Release)),
        || Ok::<(), &'static str>(()),
    );
    assert!(rolled_back.load(Ordering::Acquire));
}

#[test]
fn a_commit_the_peer_may_have_missed_is_never_rolled_back() {
    // The destination may have the commit and be running the guest.
    let (mut source, mut dest) = pair();
    dest.send(&Message::Okay).expect("holds it all");
    drop(dest);
    let status = status();

    let error = hand_over(&mut source, &Gate::default(), &status)
        .expect_err("the peer left");

    assert!(matches!(error, MigrateError::Committed(_)), "{error}");
    assert_eq!(phase(&status), MigrationPhase::Committed);
}

#[test]
fn a_commit_goes_through_the_gate() {
    let (mut source, mut dest) = pair();
    let peer = std::thread::spawn(move || {
        dest.send(&Message::Okay).expect("holds it all");
        assert!(matches!(dest.recv(), Ok(Message::Okay)));
        dest.send(&Message::Okay).expect("started");
        dest
    });
    let gate = Gate::default();
    let status = status();

    hand_over(&mut source, &gate, &status).expect("handed over");

    assert_eq!(gate.commits.load(Ordering::Acquire), 1);
    assert_eq!(phase(&status), MigrationPhase::Committed);
    peer.join().expect("the destination finished");
}

#[test]
fn a_cancel_stops_the_last_partial_batch() {
    let gate = Gate {
        cancel: true,
        ..Gate::default()
    };
    let mut sent = 0;
    let error = send_dirty((0..10).map(|p| p * 4096), &gate, |_| {
        sent += 1;
        Ok(())
    })
    .expect_err("cancelled");
    assert!(crate::is_cancel(&error), "{error}");
    assert_eq!(sent, 0);
}

#[test]
fn every_dirty_page_is_sent_once() {
    let mut sizes = Vec::new();
    let total = send_dirty(
        (0..(BATCH_SIZE as u64 + 6)).map(|p| p * 4096),
        &Gate::default(),
        |batch| {
            sizes.push(batch.len());
            Ok(())
        },
    )
    .expect("sent");
    assert_eq!(total, BATCH_SIZE as u64 + 6);
    assert_eq!(sizes, [BATCH_SIZE, 6]);
}

#[test]
fn a_cancelled_pass_sends_no_mem_end() {
    let (mut source, mut dest) = pair();
    let gate = Gate {
        cancel: true,
        ..Gate::default()
    };
    let error = end_pass(&mut source, &gate).expect_err("cancelled");
    assert!(crate::is_cancel(&error), "{error}");
    drop(source);
    assert!(dest.recv().is_err(), "the destination heard a MemEnd");
}

#[test]
fn a_pause_a_stop_refused_is_a_cancel() {
    let gate = Gate {
        cancel: true,
        ..Gate::default()
    };
    let error = cancel_or(&gate, MigrateError::Io("pause: refused".into()));
    assert!(crate::is_cancel(&error), "{error}");
    let error =
        cancel_or(&Gate::default(), MigrateError::Io("pause: EIO".into()));
    assert!(!crate::is_cancel(&error), "{error}");
}

#[test]
fn a_pre_pause_failure_does_not_resume_a_running_guest() {
    // Nothing was paused yet, so a resume here would be a state
    // change on a VM that never stopped.
    let resumed = Arc::new(AtomicBool::new(false));
    let by_callback = Arc::clone(&resumed);
    let resume_fn: DeviceResumeFn = Box::new(move || {
        by_callback.store(true, Ordering::Release);
    });
    let result = finish_source(
        Err(MigrateError::Io("connect failed".into())),
        MigrationPhase::Sync,
        &discard(),
        resume_fn,
        || Ok::<(), &'static str>(()),
    );
    assert!(result.is_err());
    assert!(!resumed.load(Ordering::Acquire));
}
