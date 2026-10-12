// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A file disk that a migration destination opens after the commit.

use std::path::{Path, PathBuf};

use vmm_devices::blkdev::BackingDisk;
use vmm_devices::lifecycle::BackingAttach;

use super::super::late::LateFile;
use super::*;

const DISK_BYTES: usize = 1 << 20;

fn attach_by(until: Instant) -> BackingAttach {
    BackingAttach {
        until,
        allow_unproven: false,
    }
}

/// This process's descriptors open on `path`.
fn fds_on(path: &Path) -> usize {
    #[cfg(target_os = "illumos")]
    let dir = "/proc/self/path";
    #[cfg(not(target_os = "illumos"))]
    let dir = "/proc/self/fd";
    let want = path.canonicalize().expect("canonical path");
    std::fs::read_dir(dir)
        .expect("list descriptors")
        .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .filter(|target| *target == want)
        .count()
}

fn image(dir: &Path) -> PathBuf {
    let path = dir.join("disk1");
    let mut bytes = vec![0u8; DISK_BYTES];
    bytes[..512].fill(0xAB);
    std::fs::write(&path, bytes).expect("write the image");
    path
}

fn destination(path: &Path) -> Harness {
    let log = slog::Logger::root(slog::Discard, slog::o!());
    let disk = BackingDisk::deferred(path, false, &log).expect("probe");
    Harness::from_factory(
        LateFile::factory(disk, log),
        false,
        FENCE_BUDGETS,
        NvmeConfig::default(),
    )
    .expect("controller")
}

#[test]
fn a_destination_opens_its_file_disk_only_at_the_attach() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = image(dir.path());
    let mut h = destination(&path);
    assert!(!h.ctrl.backing_attached());
    assert_eq!(fds_on(&path), 0, "the disk is open before the commit");

    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);
    // A restore leaves the controller paused.
    h.ctrl.pause();
    let mut tail = 0;
    submit_on(&h, 1, SQ1, &mut tail, Sqe::read(0, 1, 0x50000, 0), 9);
    h.ctrl.resume();
    assert_eq!(fds_on(&path), 0, "a resume opened the disk");

    h.ctrl
        .attach_backing(&attach_by(Instant::now()))
        .expect("attach");
    assert!(h.ctrl.backing_attached());
    assert!(fds_on(&path) > 0, "the attach did not open the disk");

    h.ctrl.resume();
    let cqe = h.wait_cqe(CQ1, 0, true);
    assert_eq!((cqe.cid, cqe.status()), (9, Status::SUCCESS));
    assert_eq!(h.read_guest(0x50000, 512), vec![0xAB; 512]);
}

#[test]
fn a_failed_open_keeps_the_disk_detached_and_a_retry_attaches() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = image(dir.path());
    let h = destination(&path);
    h.ctrl.pause();

    std::fs::remove_file(&path).expect("remove");
    let error = h
        .ctrl
        .attach_backing(&attach_by(Instant::now()))
        .expect_err("no disk");
    assert!(error.to_string().contains("disk1"), "{error}");
    assert!(!h.ctrl.backing_attached());

    image(dir.path());
    h.ctrl
        .attach_backing(&attach_by(Instant::now()))
        .expect("the retry");
    assert!(h.ctrl.backing_attached());
}

#[test]
fn a_halted_destination_never_opens_its_disk() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = image(dir.path());
    let h = destination(&path);
    h.ctrl.pause();
    h.ctrl.halt();
    h.ctrl
        .attach_backing(&attach_by(Instant::now()))
        .expect_err("halted");
    assert!(!h.ctrl.backing_attached());
}

#[test]
fn a_worker_that_will_not_start_fails_the_attach_and_a_retry_takes_it() {
    // After the commit this process holds the only copy of the guest,
    // so the attach fails and stays retryable instead of aborting.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = image(dir.path());
    let mut h = destination(&path);
    h.enable();
    h.create_io_queues(1, IO_QDEPTH, SQ1, CQ1);
    h.ctrl.pause();

    super::super::pool::FAIL_SPAWN.with(|f| f.set(Some(3)));
    let error = h
        .ctrl
        .attach_backing(&attach_by(Instant::now()))
        .expect_err("a worker did not start");
    super::super::pool::FAIL_SPAWN.with(|f| f.set(None));
    assert!(error.to_string().contains("did not start"), "{error}");
    assert!(!h.ctrl.backing_attached());

    h.ctrl
        .attach_backing(&attach_by(Instant::now()))
        .expect("the retry");
    assert!(h.ctrl.backing_attached());
    let mut tail = 0;
    submit_on(&h, 1, SQ1, &mut tail, Sqe::read(0, 1, 0x50000, 0), 4);
    h.ctrl.resume();
    let cqe = h.wait_cqe(CQ1, 0, true);
    assert_eq!((cqe.cid, cqe.status()), (4, Status::SUCCESS));
}
