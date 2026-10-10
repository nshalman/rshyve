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

/// A rollback after the device pause started.
fn paused(devices: DeviceResumeFn) -> Rollback {
    Rollback {
        devices,
        devices_paused: true,
        vcpus_paused: true,
    }
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
        GateState::None,
        &discard(),
        paused(resume_fn),
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
        GateState::None,
        &discard(),
        paused(resume_fn),
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
        GateState::None,
        &discard(),
        paused(resume_fn),
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

fn no_hook() -> CommittedFn {
    Box::new(|| {})
}

/// A hook that counts its calls.
fn counting() -> (CommittedFn, Arc<std::sync::atomic::AtomicUsize>) {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let by_hook = Arc::clone(&calls);
    (
        Box::new(move || {
            by_hook.fetch_add(1, Ordering::AcqRel);
        }),
        calls,
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

    let error = hand_over(&mut source, &gate, no_hook(), &status, &discard())
        .expect_err("cancelled");

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

    let error = hand_over(&mut source, &gate, no_hook(), &status, &discard())
        .expect_err("the destination aborted");

    assert_eq!(gate.commits.load(Ordering::Acquire), 0, "committed");
    assert_eq!(phase(&status), MigrationPhase::Finish);
    let rolled_back = Arc::new(AtomicBool::new(false));
    let by_callback = Arc::clone(&rolled_back);
    let _ = finish_source(
        Err(error),
        phase(&status),
        GateState::None,
        &discard(),
        paused(Box::new(move || by_callback.store(true, Ordering::Release))),
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

    let error = hand_over(
        &mut source,
        &Gate::default(),
        no_hook(),
        &status,
        &discard(),
    )
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

    hand_over(&mut source, &gate, no_hook(), &status, &discard())
        .expect("handed over");

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
        GateState::None,
        &discard(),
        paused(resume_fn),
        || Ok::<(), &'static str>(()),
    );
    assert!(result.is_err());
    assert!(!resumed.load(Ordering::Acquire));
}

#[test]
fn a_source_held_by_the_gate_is_never_resumed() {
    // The orchestrator recorded the go before it answered. Only its
    // order may bring the guest back here.
    for phase in [
        MigrationPhase::RamPushPostPause,
        MigrationPhase::DeviceState,
        MigrationPhase::Finish,
    ] {
        let resumed = Arc::new(AtomicBool::new(false));
        let by_callback = Arc::clone(&resumed);
        let result = finish_source(
            Err(MigrateError::WebSocket("closed".into())),
            phase,
            GateState::Go,
            &discard(),
            paused(Box::new(move || {
                by_callback.store(true, Ordering::Release)
            })),
            || -> Result<(), &'static str> {
                panic!("a held source must not resume its vCPUs")
            },
        );
        assert!(result.is_err());
        assert!(!resumed.load(Ordering::Acquire), "{phase}");
    }
}

#[test]
fn an_aborted_gate_rolls_the_source_back() {
    let resumed = Arc::new(AtomicBool::new(false));
    let by_callback = Arc::clone(&resumed);
    let _ = finish_source(
        Err(MigrateError::Phase("cutover gate: abort".into())),
        MigrationPhase::Pause,
        GateState::Abort,
        &discard(),
        paused(Box::new(move || by_callback.store(true, Ordering::Release))),
        || Ok::<(), &'static str>(()),
    );
    assert!(resumed.load(Ordering::Acquire));
}

#[test]
fn the_disks_are_given_up_right_after_the_commit() {
    let (mut source, mut dest) = pair();
    let peer = std::thread::spawn(move || {
        dest.send(&Message::Okay).expect("holds it all");
        assert!(matches!(dest.recv(), Ok(Message::Okay)));
        dest.send(&Message::Okay).expect("started");
        dest
    });
    let (hook, calls) = counting();
    hand_over(&mut source, &Gate::default(), hook, &status(), &discard())
        .expect("handed over");
    assert_eq!(calls.load(Ordering::Acquire), 1);
    peer.join().expect("the destination finished");
}

#[test]
fn a_commit_that_may_be_lost_still_gives_the_disks_up() {
    let (mut source, mut dest) = pair();
    dest.send(&Message::Okay).expect("holds it all");
    drop(dest);
    let (hook, calls) = counting();
    let error =
        hand_over(&mut source, &Gate::default(), hook, &status(), &discard())
            .expect_err("the peer left");
    assert!(matches!(error, MigrateError::Committed(_)), "{error}");
    assert_eq!(calls.load(Ordering::Acquire), 1);
}

#[test]
fn no_commit_keeps_the_disks() {
    let (mut source, mut dest) = pair();
    dest.send(&Message::Error(crate::cancelled()))
        .expect("abort");
    let (hook, calls) = counting();
    hand_over(&mut source, &Gate::default(), hook, &status(), &discard())
        .expect_err("the destination aborted");
    assert_eq!(calls.load(Ordering::Acquire), 0);
}

/// Guest RAM at 0, the firmware scratch RAM in the hole where the lab
/// source failed, and the boot ROM.
fn firmware_layout(ram: usize) -> MemCtx {
    use vmm_core::mem::{PhysMap, RegionKind};
    let map = PhysMap::new();
    map.add_region_anon(0, ram, RegionKind::Ram).expect("RAM");
    map.add_region_anon(0xFCC0_0000, 0x4000, RegionKind::Ram)
        .expect("scratch RAM");
    map.add_region_anon(0xFF00_0000, 0x1000, RegionKind::Rom)
        .expect("ROM");
    MemCtx::new(Arc::new(map))
}

fn no_tracking(_: u64, _: usize, _: &mut [u8]) -> std::io::Result<()> {
    Ok(())
}

#[test]
fn the_full_pass_sends_guest_ram_only() {
    // The fbuf VRAM is kernel device memory and never in the map; the
    // scratch RAM and the ROM are, and must stay out of the stream.
    const RAM: usize = 16 * PAGE_SIZE;
    let memctx = firmware_layout(RAM);
    let (mut source, mut dest) = pair();
    let peer = std::thread::spawn(move || {
        let mut pages = Vec::new();
        loop {
            match dest.recv().expect("message") {
                Message::PageBatch {
                    base_gpa,
                    page_count,
                    ..
                } => pages.extend(
                    (0..u64::from(page_count))
                        .map(|i| base_gpa + i * PAGE_SIZE as u64),
                ),
                Message::MemEnd => break,
                other => panic!("unexpected {}", other.name()),
            }
        }
        dest.send(&Message::MemDone).expect("MemDone");
        pages
    });

    push_ram_all(
        &mut source,
        &mut no_tracking,
        &memctx,
        &Gate::default(),
        &status(),
    )
    .expect("full pass");

    let pages = peer.join().expect("peer");
    assert_eq!(pages.len(), RAM / PAGE_SIZE);
    assert!(pages.iter().all(|&gpa| gpa < RAM as u64), "{pages:x?}");
}

#[test]
fn a_peer_that_stops_reading_fails_the_pre_pause_push() {
    // The lab source sat at ram-push-pre-pause for ten minutes after
    // the destination died behind the agent's bridge.
    const RAM: usize = 2048 * PAGE_SIZE;
    let memctx = firmware_layout(RAM);
    // Bytes zstd cannot shrink, so the socket buffer fills.
    let mut noise = vec![0u8; RAM];
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for b in noise.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = x as u8;
    }
    memctx.write(0, &noise).expect("fill");

    let (ours, _silent_peer) = UnixStream::pair().expect("pair");
    let (stream, watch) =
        crate::stall::watch(ours, Duration::from_millis(300), &discard())
            .expect("watch");
    let mut source = Chan::new(
        stream,
        tungstenite::protocol::Role::Client,
        Deadlines::default(),
    )
    .expect("channel");
    let status = status();
    set_phase(&status, MigrationPhase::RamPushPrePause);

    let started = std::time::Instant::now();
    let error = push_ram_all(
        &mut source,
        &mut no_tracking,
        &memctx,
        &Gate::default(),
        &status,
    )
    .expect_err("nobody reads");
    let error = stalled_or(&watch, error);
    record_failure(&status, &error);

    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(error.to_string().contains("no progress"), "{error}");
    let s = status.lock().expect("status");
    assert_eq!(s.phase, MigrationPhase::Error);
    assert!(s
        .error
        .as_deref()
        .is_some_and(|e| e.contains("no progress")));
}

#[test]
fn a_committed_failure_keeps_the_committed_phase() {
    // The orchestrator reads `committed` to know the guest may be gone.
    let status = status();
    set_phase(&status, MigrationPhase::Committed);
    record_failure(&status, &MigrateError::Committed("lost".into()));
    let s = status.lock().expect("status");
    assert_eq!(s.phase, MigrationPhase::Committed);
    assert!(s.error.is_some());
}

/// The kernel's dirty bitmap, as a test drives it: a guest write marks
/// a page, and a read clears what it reports.
#[derive(Clone, Default)]
struct FakeDirty(Arc<Mutex<std::collections::BTreeSet<u64>>>);

impl FakeDirty {
    fn guest_write(&self, memctx: &MemCtx, gpa: u64, fill: u8) {
        memctx.write(gpa, &[fill; PAGE_SIZE]).expect("guest write");
        self.0.lock().expect("bitmap").insert(gpa);
    }

    fn track(
        &self,
        gpa: u64,
        len: usize,
        bm: &mut [u8],
    ) -> std::io::Result<()> {
        let mut dirty = self.0.lock().expect("bitmap");
        let end = gpa + len as u64;
        for page in dirty.range(gpa..end) {
            let bit = ((page - gpa) / PAGE_SIZE as u64) as usize;
            bm[bit / 8] |= 1 << (bit % 8);
        }
        dirty.retain(|&p| p < gpa || p >= end);
        Ok(())
    }
}

/// One attempt's RAM passes into a fresh destination, which it hands
/// back. `between` runs on the guest between the full and the dirty
/// pass.
fn ram_attempt(
    memctx: &MemCtx,
    dirty: &FakeDirty,
    ram: usize,
    between: impl FnOnce(),
) -> MemCtx {
    use crate::destination::{receive_pass, PassEnd};
    let (mut source, mut dest) = pair();
    let peer = std::thread::spawn(move || {
        let memctx = MemCtx::new(Arc::new(
            vmm_core::mem::PhysMap::new_anon(0, ram).expect("guest memory"),
        ));
        for _ in 0..2 {
            receive_pass(
                &mut dest,
                &memctx,
                &crate::NoStop,
                &status(),
                PassEnd::MemEnd,
            )
            .expect("pass");
        }
        memctx
    });
    let mut track = |gpa, len, bm: &mut [u8]| dirty.track(gpa, len, bm);
    push_ram_all(&mut source, &mut track, memctx, &Gate::default(), &status())
        .expect("full pass");
    between();
    push_ram(&mut source, &mut track, memctx, &Gate::default(), &status())
        .expect("dirty pass");
    peer.join().expect("destination")
}

fn image(memctx: &MemCtx, ram: usize) -> Vec<u8> {
    let mut out = vec![0u8; ram];
    memctx.read(0, &mut out).expect("read");
    out
}

#[test]
fn a_retry_after_a_failed_attempt_carries_its_writes() {
    // Attempt one sends page 3 in its dirty pass, which clears the bit,
    // and then fails. The guest never writes page 3 again. Attempt two
    // goes to a new destination, so only a full first pass carries it.
    const RAM: usize = 16 * PAGE_SIZE;
    let memctx = firmware_layout(RAM);
    let dirty = FakeDirty::default();
    for page in 0..16u64 {
        dirty.guest_write(&memctx, page * PAGE_SIZE as u64, page as u8);
    }
    let third = 3 * PAGE_SIZE as u64;

    let failed = ram_attempt(&memctx, &dirty, RAM, || {
        dirty.guest_write(&memctx, third, 0xB3);
    });
    drop(failed);
    assert!(dirty.0.lock().expect("bitmap").is_empty());

    let retry = ram_attempt(&memctx, &dirty, RAM, || {});
    assert_eq!(image(&retry, RAM), image(&memctx, RAM));
}

#[test]
fn a_page_dirty_at_the_commit_refuses_it() {
    const RAM: usize = 16 * PAGE_SIZE;
    let memctx = firmware_layout(RAM);
    let dirty = FakeDirty::default();
    let owners = Owners::new(&memctx.guest_ram(), &[]);
    let mut track = |gpa, len, bm: &mut [u8]| dirty.track(gpa, len, bm);
    check_clean(&mut track, &memctx, &owners).expect("nothing dirty");

    dirty.guest_write(&memctx, 5 * PAGE_SIZE as u64, 0x55);
    let mut track = |gpa, len, bm: &mut [u8]| dirty.track(gpa, len, bm);
    let error =
        check_clean(&mut track, &memctx, &owners).expect_err("one page left");
    assert!(error.to_string().contains("1 guest pages"), "{error}");
    assert!(
        error.to_string().contains("0x5000 (RAM region 0"),
        "{error}"
    );
}

#[test]
fn a_dirty_source_ends_before_the_destination_says_ready() {
    // The destination hears the failure in place of the MemEnd, so it
    // fails before it commits, not in doubt.
    let (mut source, mut dest) = pair();
    let error = end_ram(
        &mut source,
        &Gate::default(),
        || Err(MigrateError::State("1 guest pages left".into())),
        &discard(),
    )
    .expect_err("dirty");
    assert!(matches!(error, MigrateError::State(_)), "{error}");
    assert!(
        !matches!(dest.recv(), Ok(Message::MemEnd)),
        "the destination heard a MemEnd"
    );

    let (mut source, mut dest) = pair();
    end_ram(&mut source, &Gate::default(), || Ok(()), &discard())
        .expect("clean");
    assert!(matches!(dest.recv(), Ok(Message::MemEnd)));
}

#[test]
fn a_page_the_source_cannot_read_fails_and_sends_nothing() {
    // Propolis sends a zero page here. The guest would resume with that
    // page wiped.
    let memctx = firmware_layout(4 * PAGE_SIZE);
    let (mut source, mut dest) = pair();
    let unmapped = 0x10_0000_0000;
    for gpas in [vec![0, unmapped], vec![unmapped]] {
        let error = send_batch_sparse(
            &mut source,
            &memctx,
            &Gate::default(),
            &gpas,
            &status(),
        )
        .expect_err("unmapped page");
        assert!(error.to_string().contains("guest memory read"), "{error}");
    }
    drop(source);
    assert!(dest.recv().is_err(), "a batch went out");
}

mod final_pass;
