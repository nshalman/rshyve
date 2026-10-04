// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::*;
use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

const BLK: u32 = libc::S_IFBLK | 0o600;
const CHR: u32 = libc::S_IFCHR | 0o600;
const REG: u32 = libc::S_IFREG | 0o600;

/// Scripted ioctl results. Errnos are raw so a test reads like the
/// driver's answer.
struct Fake {
    /// Results of successive flushes; `Ok` once it runs dry.
    flushes: Mutex<VecDeque<Result<(), i32>>>,
    flush_calls: AtomicUsize,
    /// Current write cache state, or the errno GETWCE answers.
    wce: Mutex<Result<bool, i32>>,
    /// The errno SETWCE answers, or `None` to accept.
    set_err: Option<i32>,
    /// SETWCE accepts but the state does not change.
    set_ignored: bool,
    /// After a SETWCE, GETWCE answers this errno.
    get_err_after_set: Option<i32>,
    sets: Mutex<Vec<bool>>,
}

impl Fake {
    fn new(probe: Result<(), i32>, wce: Result<bool, i32>) -> Self {
        Self {
            flushes: Mutex::new(VecDeque::from([probe])),
            flush_calls: AtomicUsize::new(0),
            wce: Mutex::new(wce),
            set_err: None,
            set_ignored: false,
            get_err_after_set: None,
            sets: Mutex::new(Vec::new()),
        }
    }

    fn then_flush(self, results: &[Result<(), i32>]) -> Self {
        self.flushes.lock().unwrap().extend(results);
        self
    }

    fn calls(&self) -> usize {
        self.flush_calls.load(Ordering::SeqCst)
    }

    fn sets(&self) -> Vec<bool> {
        self.sets.lock().unwrap().clone()
    }

    fn wce(&self) -> Result<bool, i32> {
        *self.wce.lock().unwrap()
    }
}

impl DiskOps for Fake {
    fn flush(&self) -> io::Result<()> {
        self.flush_calls.fetch_add(1, Ordering::SeqCst);
        match self.flushes.lock().unwrap().pop_front() {
            Some(Err(errno)) => Err(io::Error::from_raw_os_error(errno)),
            _ => Ok(()),
        }
    }

    fn get_wce(&self) -> io::Result<bool> {
        self.wce().map_err(io::Error::from_raw_os_error)
    }

    fn set_wce(&self, on: bool) -> io::Result<()> {
        self.sets.lock().unwrap().push(on);
        if let Some(errno) = self.set_err {
            return Err(io::Error::from_raw_os_error(errno));
        }
        let mut wce = self.wce.lock().unwrap();
        if let Some(errno) = self.get_err_after_set {
            *wce = Err(errno);
        } else if !self.set_ignored {
            *wce = Ok(on);
        }
        Ok(())
    }
}

fn backing() -> Arc<File> {
    Arc::new(tempfile::tempfile().expect("tempfile"))
}

fn open(st_mode: u32, fake: &Arc<Fake>) -> io::Result<DiskSync> {
    DiskSync::build(backing(), st_mode, false, Arc::clone(fake) as _)
}

fn errno(r: io::Result<()>) -> Option<i32> {
    r.expect_err("expected an error").raw_os_error()
}

#[test]
fn a_regular_file_syncs() {
    let mut file = tempfile::tempfile().expect("tempfile");
    file.write_all(&[0x5A; 512]).expect("write");
    let disk = DiskSync::new(Arc::new(file), false).expect("classify");
    disk.sync().expect("sync a regular file");
}

#[test]
fn a_regular_file_never_takes_the_ioctl() {
    let fake = Arc::new(Fake::new(Ok(()), Ok(false)));
    let disk = open(REG, &fake).expect("open");
    disk.sync().expect("sync");
    assert_eq!(fake.calls(), 0);
    assert!(fake.sets().is_empty());
}

#[test]
fn a_writable_block_node_is_refused() {
    let fake = Arc::new(Fake::new(Ok(()), Ok(true)));
    let err = open(BLK, &fake).err().expect("refused");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert!(err.to_string().contains("raw (character) node"), "{err}");
    assert_eq!(fake.calls(), 0);
    assert!(fake.sets().is_empty());
}

#[test]
fn a_read_only_block_node_is_allowed() {
    let fake = Arc::new(Fake::new(Ok(()), Ok(true)));
    let disk = DiskSync::build(backing(), BLK, true, Arc::clone(&fake) as _)
        .expect("open");
    disk.sync().expect("sync");
    assert_eq!(fake.calls(), 0);
    assert!(fake.sets().is_empty());
}

#[test]
fn a_block_node_refusal_names_the_raw_node() {
    for (blk, raw) in [
        (
            "/dev/zvol/dsk/zones/vm0/disk0",
            "/dev/zvol/rdsk/zones/vm0/disk0",
        ),
        ("/dev/dsk/c1t0d0p0", "/dev/rdsk/c1t0d0p0"),
        ("/dev/lofi/3", "/dev/rlofi/3"),
    ] {
        let err = check_not_block(BLK, false, Some(Path::new(blk)))
            .expect_err("refused");
        assert!(err.to_string().contains(raw), "{err}");
    }
    assert_eq!(raw_node(Path::new("/var/tmp/disk.img")), None);
    check_not_block(BLK, true, Some(Path::new("/dev/lofi/3")))
        .expect("read-only");
    check_not_block(CHR, false, Some(Path::new("/dev/rlofi/3")))
        .expect("raw node");
}

#[test]
fn open_disk_takes_a_regular_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("disk.img");
    std::fs::write(&path, [0u8; 512]).expect("write");
    let file = open_disk(&path, false).expect("open");
    DiskSync::new(Arc::new(file), false).expect("classify");
}

#[test]
fn a_char_device_takes_the_ioctl() {
    let fake = Arc::new(Fake::new(Ok(()), Ok(true)));
    let disk = open(CHR, &fake).expect("open");
    assert_eq!(fake.calls(), 2, "both probes");
    disk.sync().expect("sync");
    assert_eq!(fake.calls(), 3);
}

#[test]
fn a_read_only_disk_is_left_alone() {
    let fake = Arc::new(Fake::new(Err(libc::EIO), Ok(true)));
    let disk = DiskSync::build(backing(), CHR, true, Arc::clone(&fake) as _)
        .expect("open");
    disk.sync().expect("sync");
    assert_eq!(fake.calls(), 0);
    assert!(fake.sets().is_empty());
}

#[test]
fn a_flushing_disk_returns_every_flush_error() {
    let fake = Arc::new(Fake::new(Ok(()), Ok(true)).then_flush(&[
        Ok(()),
        Err(libc::EIO),
        Err(libc::ENXIO),
        Err(libc::ENOTSUP),
    ]));
    let disk = open(CHR, &fake).expect("open");
    assert_eq!(errno(disk.sync()), Some(libc::EIO));
    assert_eq!(errno(disk.sync()), Some(libc::ENXIO));
    assert_eq!(errno(disk.sync()), Some(libc::ENOTSUP));
    disk.sync().expect("recovered");
    assert_eq!(fake.calls(), 6);
}

#[test]
fn a_flushing_disk_runs_writeback_and_restores_it() {
    let fake = Arc::new(Fake::new(Ok(()), Ok(false)));
    let disk = open(CHR, &fake).expect("open");
    assert_eq!(fake.wce(), Ok(true));
    assert_eq!(fake.calls(), 2, "probed again with the cache on");
    drop(disk);
    assert_eq!(fake.sets(), [true, false]);
    assert_eq!(fake.wce(), Ok(false));
}

#[test]
fn a_disk_that_stops_flushing_with_its_cache_on_runs_without_it() {
    for errno in [libc::ENOTSUP, libc::ENOTTY] {
        let fake =
            Arc::new(Fake::new(Ok(()), Ok(false)).then_flush(&[Err(errno)]));
        let disk = open(CHR, &fake).expect("open");
        assert_eq!(fake.wce(), Ok(false));
        disk.sync().expect("no-op flush");
        assert_eq!(fake.calls(), 2);
        drop(disk);
        assert_eq!(fake.sets(), [true, false]);
        assert_eq!(fake.wce(), Ok(false));
    }
}

#[test]
fn a_disk_whose_cache_stays_unreadable_after_writeback_is_refused() {
    for get in [libc::EIO, libc::ENOTSUP, libc::ENOTTY] {
        let fake = Arc::new(Fake {
            get_err_after_set: Some(get),
            ..Fake::new(Ok(()), Ok(false)).then_flush(&[Err(libc::ENOTSUP)])
        });
        let err = open(CHR, &fake).err().expect("refused");
        assert!(err.to_string().contains("could not be confirmed off"));
        assert!(err.to_string().contains(&format!("os error {get}")));
        assert_eq!(fake.sets(), [true, false, false], "Drop turns it off");
    }
}

#[test]
fn a_cache_found_on_is_refreshed_and_probed_again() {
    for errno in [libc::ENOTSUP, libc::ENOTTY] {
        let fake =
            Arc::new(Fake::new(Ok(()), Ok(true)).then_flush(&[Err(errno)]));
        let disk = open(CHR, &fake).expect("open");
        assert_eq!(fake.calls(), 2);
        assert_eq!(fake.wce(), Ok(false));
        disk.sync().expect("no-op flush");
        assert_eq!(fake.calls(), 2);
        drop(disk);
        assert_eq!(fake.sets(), [true, false]);
    }
}

#[test]
fn a_cache_found_on_that_refuses_setwce_is_still_probed() {
    let fake = Arc::new(Fake {
        set_err: Some(libc::EINVAL),
        ..Fake::new(Ok(()), Ok(true))
    });
    drop(open(CHR, &fake).expect("open"));
    assert_eq!(fake.calls(), 2);
    assert_eq!(fake.sets(), [true]);
}

#[test]
fn a_disk_that_fails_a_flush_with_its_cache_on_is_refused() {
    let fake =
        Arc::new(Fake::new(Ok(()), Ok(false)).then_flush(&[Err(libc::EIO)]));
    let err = open(CHR, &fake).err().expect("refused");
    assert_eq!(err.raw_os_error(), None);
    assert!(err.to_string().contains(&format!("os error {}", libc::EIO)));
    assert_eq!(fake.sets(), [true, false]);
    assert_eq!(fake.wce(), Ok(false));

    // A cache this open found on is not this open's to turn off.
    let fake =
        Arc::new(Fake::new(Ok(()), Ok(true)).then_flush(&[Err(libc::EIO)]));
    assert!(open(CHR, &fake).is_err());
    assert_eq!(fake.sets(), [true]);
    assert_eq!(fake.wce(), Ok(true));
}

#[test]
fn a_flushing_disk_without_cache_control_is_accepted() {
    let fake = Arc::new(Fake::new(Ok(()), Err(libc::EIO)));
    let disk = open(CHR, &fake).expect("open");
    disk.sync().expect("sync");
    drop(disk);
    assert!(fake.sets().is_empty());
    assert_eq!(fake.calls(), 2);
}

#[test]
fn a_flushing_disk_that_refuses_writeback_is_accepted() {
    let fake = Arc::new(Fake {
        set_err: Some(libc::EACCES),
        ..Fake::new(Ok(()), Ok(false))
    });
    let disk = open(CHR, &fake).expect("open");
    disk.sync().expect("sync");
    drop(disk);
    assert_eq!(fake.sets(), [true], "nothing to restore");
    assert_eq!(fake.calls(), 2);
}

#[test]
fn two_opens_only_move_the_cache_toward_off() {
    // Can flush: B sees the cache A turned on and does not own it.
    let fake = Arc::new(Fake::new(Ok(()), Ok(false)));
    let a = open(CHR, &fake).expect("open a");
    let b = open(CHR, &fake).expect("open b");
    drop(a);
    assert_eq!(fake.wce(), Ok(false));
    b.sync().expect("b still flushes");
    drop(b);
    assert_eq!(fake.sets(), [true, true, false]);

    // Cannot flush: A turns the cache off and never back on under B.
    let fake = Arc::new(
        Fake::new(Err(libc::ENOTSUP), Ok(true))
            .then_flush(&[Err(libc::ENOTSUP)]),
    );
    let a = open(CHR, &fake).expect("open a");
    let b = open(CHR, &fake).expect("open b");
    drop(a);
    assert_eq!(fake.wce(), Ok(false));
    drop(b);
    assert_eq!(fake.sets(), [false]);
    assert_eq!(fake.wce(), Ok(false));
}

#[test]
fn a_flushing_disk_already_in_writeback_is_left_on() {
    let fake = Arc::new(Fake::new(Ok(()), Ok(true)));
    drop(open(CHR, &fake).expect("open"));
    assert_eq!(fake.calls(), 2, "probed again after the refresh");
    assert_eq!(fake.sets(), [true], "refresh only, nothing to restore");
    assert_eq!(fake.wce(), Ok(true));
}

#[test]
fn a_disk_that_cannot_flush_with_its_cache_off_skips_flushes() {
    for probe in [libc::ENOTSUP, libc::ENOTTY] {
        let fake = Arc::new(Fake::new(Err(probe), Ok(false)));
        let disk = open(CHR, &fake).expect("open");
        for _ in 0..3 {
            disk.sync().expect("no-op flush");
        }
        assert_eq!(fake.calls(), 1, "only the probe");
        drop(disk);
        assert!(fake.sets().is_empty(), "never enables the cache");
    }
}

#[test]
fn a_disk_without_cache_control_skips_flushes() {
    for get in [libc::ENOTSUP, libc::ENOTTY] {
        let fake = Arc::new(Fake::new(Err(libc::ENOTSUP), Err(get)));
        let disk = open(CHR, &fake).expect("open");
        disk.sync().expect("no-op flush");
        assert_eq!(fake.calls(), 1);
        assert!(fake.sets().is_empty());
    }
}

#[test]
fn a_disk_that_cannot_flush_has_its_cache_turned_off() {
    let fake = Arc::new(Fake::new(Err(libc::ENOTSUP), Ok(true)));
    let disk = open(CHR, &fake).expect("open");
    assert_eq!(fake.wce(), Ok(false));
    disk.sync().expect("no-op flush");
    assert_eq!(fake.calls(), 1);
    drop(disk);
    assert_eq!(fake.sets(), [false], "never turned back on");
    assert_eq!(fake.wce(), Ok(false));
}

#[test]
fn a_disk_that_cannot_flush_or_turn_its_cache_off_is_refused() {
    let fake = Arc::new(Fake {
        set_err: Some(libc::EACCES),
        ..Fake::new(Err(libc::ENOTSUP), Ok(true))
    });
    let err = open(CHR, &fake).err().expect("refused");
    assert!(err.to_string().contains("could not be confirmed off"));
    assert_eq!(fake.sets(), [false]);
    assert_eq!(fake.wce(), Ok(true));
}

#[test]
fn a_cache_that_stays_on_is_refused() {
    let fake = Arc::new(Fake {
        set_ignored: true,
        ..Fake::new(Err(libc::ENOTTY), Ok(true))
    });
    assert!(open(CHR, &fake).is_err());
    assert_eq!(fake.sets(), [false]);
}

#[test]
fn an_unreadable_cache_after_turning_it_off_is_refused() {
    for get in [libc::EIO, libc::ENOTSUP, libc::ENOTTY] {
        let fake = Arc::new(Fake {
            get_err_after_set: Some(get),
            ..Fake::new(Err(libc::ENOTSUP), Ok(true))
        });
        let err = open(CHR, &fake).err().expect("refused");
        assert!(err.to_string().contains("could not be confirmed off"));
        assert!(err.to_string().contains(&format!("os error {get}")));
        assert_eq!(fake.sets(), [false]);
    }
}

#[test]
fn an_unreadable_cache_state_is_refused() {
    let fake = Arc::new(Fake::new(Err(libc::ENOTSUP), Err(libc::EIO)));
    assert!(open(CHR, &fake).is_err());
    assert!(fake.sets().is_empty());
}

#[test]
fn any_other_probe_error_is_refused() {
    for probe in [libc::ENXIO, libc::EIO, libc::EINVAL, libc::ENODEV] {
        let fake = Arc::new(Fake::new(Err(probe), Ok(false)));
        let err = open(CHR, &fake).err().expect("refused");
        assert!(err.to_string().contains(&format!("os error {probe}")));
        assert!(fake.sets().is_empty());
    }
}

#[test]
fn concurrent_flushes_each_reach_the_disk() {
    const THREADS: usize = 8;
    const EACH: usize = 100;
    let fake = Arc::new(Fake::new(Ok(()), Ok(true)));
    let disk = Arc::new(open(CHR, &fake).expect("open"));
    std::thread::scope(|s| {
        for _ in 0..THREADS {
            let disk = Arc::clone(&disk);
            s.spawn(move || {
                for _ in 0..EACH {
                    disk.sync().expect("flush");
                }
            });
        }
    });
    assert_eq!(fake.calls(), 2 + THREADS * EACH);
}

/// /dev/null refuses the flush with ENXIO, the errno a zvol whose
/// minor is gone also returns.
#[cfg(target_os = "illumos")]
#[test]
fn a_char_device_that_fails_the_probe_is_refused() {
    let file = File::options()
        .write(true)
        .open("/dev/null")
        .expect("open /dev/null");
    let err = DiskSync::new(Arc::new(file), false)
        .err()
        .expect("probe of /dev/null succeeded");
    assert!(err
        .to_string()
        .contains(&format!("os error {}", libc::ENXIO)));
}
