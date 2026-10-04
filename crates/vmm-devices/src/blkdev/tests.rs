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
        if !self.set_ignored {
            *self.wce.lock().unwrap() = Ok(on);
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
fn block_and_char_devices_take_the_ioctl() {
    for st_mode in [BLK, CHR] {
        let fake = Arc::new(Fake::new(Ok(()), Ok(true)));
        let disk = open(st_mode, &fake).expect("open");
        assert_eq!(fake.calls(), 1, "probe");
        disk.sync().expect("sync");
        assert_eq!(fake.calls(), 2);
    }
}

#[test]
fn a_read_only_disk_is_left_alone() {
    let fake = Arc::new(Fake::new(Err(libc::EIO), Ok(true)));
    let disk = DiskSync::build(backing(), BLK, true, Arc::clone(&fake) as _)
        .expect("open");
    disk.sync().expect("sync");
    assert_eq!(fake.calls(), 0);
    assert!(fake.sets().is_empty());
}

#[test]
fn a_flushing_disk_returns_every_flush_error() {
    let fake = Arc::new(Fake::new(Ok(()), Ok(true)).then_flush(&[
        Err(libc::EIO),
        Err(libc::ENXIO),
        Err(libc::ENOTSUP),
    ]));
    let disk = open(BLK, &fake).expect("open");
    assert_eq!(errno(disk.sync()), Some(libc::EIO));
    assert_eq!(errno(disk.sync()), Some(libc::ENXIO));
    assert_eq!(errno(disk.sync()), Some(libc::ENOTSUP));
    disk.sync().expect("recovered");
    assert_eq!(fake.calls(), 5);
}

#[test]
fn a_flushing_disk_runs_writeback_and_restores_it() {
    let fake = Arc::new(Fake::new(Ok(()), Ok(false)));
    let disk = open(CHR, &fake).expect("open");
    assert_eq!(fake.wce(), Ok(true));
    drop(disk);
    assert_eq!(fake.sets(), [true, false]);
    assert_eq!(fake.wce(), Ok(false));
}

#[test]
fn a_flushing_disk_already_in_writeback_is_not_touched() {
    let fake = Arc::new(Fake::new(Ok(()), Ok(true)));
    drop(open(CHR, &fake).expect("open"));
    assert!(fake.sets().is_empty());
}

#[test]
fn a_disk_that_cannot_flush_with_its_cache_off_skips_flushes() {
    for probe in [libc::ENOTSUP, libc::ENOTTY] {
        let fake = Arc::new(Fake::new(Err(probe), Ok(false)));
        let disk = open(BLK, &fake).expect("open");
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
    let disk = open(BLK, &fake).expect("open");
    assert_eq!(fake.wce(), Ok(false));
    disk.sync().expect("no-op flush");
    assert_eq!(fake.calls(), 1);
    assert_eq!(fake.sets(), [false]);
    drop(disk);
    assert_eq!(fake.sets(), [false, true], "restored on drop");
}

#[test]
fn a_disk_that_cannot_flush_or_turn_its_cache_off_is_refused() {
    let fake = Arc::new(Fake {
        set_err: Some(libc::EACCES),
        ..Fake::new(Err(libc::ENOTSUP), Ok(true))
    });
    let err = open(BLK, &fake).err().expect("refused");
    assert!(err.to_string().contains("could not be confirmed off"));
    assert_eq!(fake.sets(), [false]);
    assert_eq!(fake.wce(), Ok(true));
}

#[test]
fn a_cache_that_stays_on_is_refused_and_restored() {
    let fake = Arc::new(Fake {
        set_ignored: true,
        ..Fake::new(Err(libc::ENOTTY), Ok(true))
    });
    assert!(open(CHR, &fake).is_err());
    assert_eq!(fake.sets(), [false, true]);
}

#[test]
fn an_unreadable_cache_state_is_refused() {
    let fake = Arc::new(Fake::new(Err(libc::ENOTSUP), Err(libc::EIO)));
    assert!(open(BLK, &fake).is_err());
    assert!(fake.sets().is_empty());
}

#[test]
fn any_other_probe_error_is_refused() {
    for probe in [libc::ENXIO, libc::EIO, libc::EINVAL, libc::ENODEV] {
        let fake = Arc::new(Fake::new(Err(probe), Ok(false)));
        let err = open(BLK, &fake).err().expect("refused");
        assert!(err.to_string().contains(&format!("os error {probe}")));
        assert!(fake.sets().is_empty());
    }
}

#[test]
fn concurrent_flushes_each_reach_the_disk() {
    const THREADS: usize = 8;
    const EACH: usize = 100;
    let fake = Arc::new(Fake::new(Ok(()), Ok(true)));
    let disk = Arc::new(open(BLK, &fake).expect("open"));
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
    assert_eq!(fake.calls(), 1 + THREADS * EACH);
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
