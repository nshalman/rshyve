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
    /// The errno SETWCE answers when turning the cache off.
    off_err: Option<i32>,
    /// DKIOCINFO's drive name, or the errno it answers.
    name: Result<&'static str, i32>,
    sets: Mutex<Vec<bool>>,
}

impl Fake {
    fn new(probe: Result<(), i32>, wce: Result<bool, i32>) -> Self {
        Self {
            flushes: Mutex::new(VecDeque::from([probe])),
            flush_calls: AtomicUsize::new(0),
            wce: Mutex::new(wce),
            set_err: None,
            off_err: None,
            name: Ok("sd"),
            sets: Mutex::new(Vec::new()),
        }
    }

    fn zvol(wce: Result<bool, i32>) -> Self {
        Self {
            name: Ok("zvol"),
            ..Self::new(Ok(()), wce)
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
    fn drive_name(&self) -> io::Result<String> {
        self.name
            .map(str::to_string)
            .map_err(io::Error::from_raw_os_error)
    }

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
        if let Some(errno) = self.set_err.or(self.off_err.filter(|_| !on)) {
            return Err(io::Error::from_raw_os_error(errno));
        }
        *self.wce.lock().unwrap() = Ok(on);
        Ok(())
    }
}

/// Collects the messages logged through it.
#[derive(Clone, Default)]
struct Msgs(Arc<Mutex<Vec<String>>>);

impl slog::Drain for Msgs {
    type Ok = ();
    type Err = slog::Never;

    fn log(
        &self,
        record: &slog::Record<'_>,
        _: &slog::OwnedKVList,
    ) -> Result<(), slog::Never> {
        self.0.lock().unwrap().push(record.msg().to_string());
        Ok(())
    }
}

impl Msgs {
    fn logger(&self) -> slog::Logger {
        slog::Logger::root(self.clone(), slog::o!())
    }

    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

fn discard() -> slog::Logger {
    slog::Logger::root(slog::Discard, slog::o!())
}

fn backing() -> Arc<File> {
    Arc::new(tempfile::tempfile().expect("tempfile"))
}

fn open(st_mode: u32, fake: &Arc<Fake>) -> io::Result<DiskSync> {
    open_logged(st_mode, fake, &discard())
}

fn open_logged(
    st_mode: u32,
    fake: &Arc<Fake>,
    log: &slog::Logger,
) -> io::Result<DiskSync> {
    DiskSync::build(backing(), st_mode, false, Arc::clone(fake) as _, log)
}

fn errno(r: io::Result<()>) -> Option<i32> {
    r.expect_err("expected an error").raw_os_error()
}

#[test]
fn a_regular_file_syncs() {
    let mut file = tempfile::tempfile().expect("tempfile");
    file.write_all(&[0x5A; 512]).expect("write");
    let disk =
        DiskSync::new(Arc::new(file), false, &discard()).expect("classify");
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
    let disk = DiskSync::build(
        backing(),
        BLK,
        true,
        Arc::clone(&fake) as _,
        &discard(),
    )
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
        ("/dev/lofi/3", "use a zvol"),
    ] {
        let err = check_not_block(BLK, false, Some(Path::new(blk)))
            .expect_err("refused");
        assert!(err.to_string().contains(raw), "{err}");
    }
    assert_eq!(raw_node(Path::new("/dev/lofi/3")), None, "rlofi is refused");
    check_not_block(BLK, true, Some(Path::new("/dev/lofi/3")))
        .expect("read-only");
    check_not_block(CHR, false, Some(Path::new("/dev/rlofi/3")))
        .expect("raw node");
}

#[test]
fn the_raw_node_hint_is_only_given_under_dev() {
    for path in [
        "/zones/dsk/vm/disk",
        "/var/tmp/lofi/disk.img",
        "dev/dsk/c1t0d0p0",
        "/devices/dsk/x",
        "/dev/../zones/dsk/vm/disk",
        "/dev/zvol/rdsk/zones/vm0",
    ] {
        assert_eq!(raw_node(Path::new(path)), None, "{path}");
        let err = check_not_block(BLK, false, Some(Path::new(path)))
            .expect_err("refused");
        assert!(err.to_string().contains("raw (character) node"), "{err}");
    }
}

#[test]
fn open_disk_takes_a_regular_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("disk.img");
    std::fs::write(&path, [0u8; 512]).expect("write");
    let file = open_disk(&path, false).expect("open");
    DiskSync::new(Arc::new(file), false, &discard()).expect("classify");
}

#[test]
fn a_read_only_disk_is_left_alone() {
    let fake = Arc::new(Fake::new(Err(libc::EIO), Ok(true)));
    let disk = DiskSync::build(
        backing(),
        CHR,
        true,
        Arc::clone(&fake) as _,
        &discard(),
    )
    .expect("open");
    disk.sync().expect("sync");
    assert_eq!(fake.calls(), 0);
    assert!(fake.sets().is_empty());
}

#[test]
fn a_zvol_runs_writeback_and_restores_it() {
    let fake = Arc::new(Fake::zvol(Ok(false)));
    let disk = open(CHR, &fake).expect("open");
    assert_eq!(fake.wce(), Ok(true));
    disk.sync().expect("sync");
    assert_eq!(fake.calls(), 1, "no probe, one flush");
    drop(disk);
    assert_eq!(fake.sets(), [true, false]);
    assert_eq!(fake.wce(), Ok(false));
}

#[test]
fn a_zvol_already_in_writeback_is_left_on() {
    let fake = Arc::new(Fake::zvol(Ok(true)));
    drop(open(CHR, &fake).expect("open"));
    assert!(fake.sets().is_empty());
    assert_eq!(fake.wce(), Ok(true));
}

#[test]
fn a_zvol_cache_change_is_best_effort_and_logged() {
    let msgs = Msgs::default();
    let fake = Arc::new(Fake {
        set_err: Some(libc::EACCES),
        ..Fake::zvol(Ok(false))
    });
    let disk = open_logged(CHR, &fake, &msgs.logger()).expect("open");
    disk.sync().expect("sync");
    drop(disk);
    assert_eq!(fake.sets(), [true], "nothing to restore");
    assert_eq!(msgs.take(), ["zvol write cache stays off"]);

    let fake = Arc::new(Fake::zvol(Err(libc::EIO)));
    drop(open_logged(CHR, &fake, &msgs.logger()).expect("open"));
    assert!(fake.sets().is_empty());
    assert_eq!(msgs.take(), ["zvol write cache state unknown"]);

    let fake = Arc::new(Fake {
        off_err: Some(libc::EIO),
        ..Fake::zvol(Ok(false))
    });
    let disk = open_logged(CHR, &fake, &msgs.logger()).expect("open");
    assert!(msgs.take().is_empty());
    drop(disk);
    assert_eq!(fake.sets(), [true, false]);
    assert_eq!(msgs.take(), ["zvol write cache left on"]);
}

#[test]
fn a_zvol_returns_every_flush_error() {
    let fake = Arc::new(Fake {
        flushes: Mutex::new(VecDeque::from([
            Err(libc::ENXIO),
            Err(libc::EIO),
            Err(libc::ENOTSUP),
        ])),
        ..Fake::zvol(Ok(false))
    });
    let disk = open(CHR, &fake).expect("open");
    assert_eq!(fake.calls(), 0);
    assert_eq!(errno(disk.sync()), Some(libc::ENXIO));
    assert_eq!(errno(disk.sync()), Some(libc::EIO));
    assert_eq!(errno(disk.sync()), Some(libc::ENOTSUP));
    disk.sync().expect("recovered");
}

#[test]
fn two_opens_of_a_zvol_flush_whatever_the_cache_state() {
    let fake = Arc::new(Fake::zvol(Ok(false)));
    let a = open(CHR, &fake).expect("open a");
    let b = open(CHR, &fake).expect("open b");
    drop(a);
    assert_eq!(fake.wce(), Ok(false));
    b.sync().expect("b still flushes");
    assert_eq!(fake.calls(), 1);
    drop(b);
    assert_eq!(fake.sets(), [true, false]);
}

#[test]
fn only_a_zvol_drive_name_is_a_zvol() {
    for name in [
        Ok("sd"),
        Ok("blkdev"),
        Ok("lofi"),
        Ok("zvol0"),
        Err(libc::ENOTTY),
    ] {
        let fake = Arc::new(Fake {
            name,
            ..Fake::new(Ok(()), Ok(false))
        });
        drop(open(CHR, &fake).expect("open"));
        assert!(fake.sets().is_empty(), "{name:?}");
        assert_eq!(fake.calls(), 1, "probed as a disk: {name:?}");
    }
}

#[test]
fn a_flushing_disk_with_its_cache_off_is_left_alone() {
    let fake = Arc::new(Fake::new(Ok(()), Ok(false)));
    let disk = open(CHR, &fake).expect("open");
    disk.sync().expect("sync");
    assert_eq!(fake.calls(), 2);
    drop(disk);
    assert!(fake.sets().is_empty());
    assert_eq!(fake.wce(), Ok(false));
}

#[test]
fn a_cache_found_on_is_refreshed_then_probed() {
    let fake = Arc::new(Fake::new(Ok(()), Ok(true)));
    let disk = open(CHR, &fake).expect("open");
    assert_eq!(fake.sets(), [true]);
    assert_eq!(fake.calls(), 1);
    drop(disk);
    assert_eq!(fake.sets(), [true], "never turned off");
    assert_eq!(fake.wce(), Ok(true));
}

#[test]
fn a_failed_refresh_is_refused() {
    for set in [libc::EINVAL, libc::EIO, libc::ENOTSUP] {
        let fake = Arc::new(Fake {
            set_err: Some(set),
            ..Fake::new(Ok(()), Ok(true))
        });
        let err = open(CHR, &fake).err().expect("refused");
        assert!(err.to_string().contains("refresh"), "{err}");
        assert!(err.to_string().contains(&format!("os error {set}")));
        assert!(err.to_string().contains("turn the write cache off"));
        assert_eq!(
            err.to_string().contains("sd.conf"),
            set == libc::EINVAL,
            "{err}"
        );
        assert_eq!(fake.calls(), 0, "not probed");
        assert_eq!(fake.sets(), [true]);
    }
}

#[test]
fn a_flushing_disk_returns_every_flush_error() {
    let fake = Arc::new(Fake::new(Ok(()), Ok(true)).then_flush(&[
        Err(libc::EIO),
        Err(libc::ENXIO),
        Err(libc::ENOTSUP),
    ]));
    let disk = open(CHR, &fake).expect("open");
    assert_eq!(errno(disk.sync()), Some(libc::EIO));
    assert_eq!(errno(disk.sync()), Some(libc::ENXIO));
    assert_eq!(errno(disk.sync()), Some(libc::ENOTSUP));
    disk.sync().expect("recovered");
    assert_eq!(fake.calls(), 5);
}

#[test]
fn a_disk_with_no_cache_and_its_cache_off_skips_flushes() {
    let fake = Arc::new(Fake::new(Err(libc::ENOTSUP), Ok(false)));
    let disk = open(CHR, &fake).expect("open");
    for _ in 0..3 {
        disk.sync().expect("no-op flush");
    }
    assert_eq!(fake.calls(), 1, "only the probe");
    drop(disk);
    assert!(fake.sets().is_empty());
}

#[test]
fn a_disk_with_no_cache_and_no_cache_control_skips_flushes() {
    for get in [libc::ENOTSUP, libc::ENOTTY] {
        let fake = Arc::new(Fake::new(Err(libc::ENOTSUP), Err(get)));
        let disk = open(CHR, &fake).expect("open");
        disk.sync().expect("no-op flush");
        assert_eq!(fake.calls(), 1);
        assert!(fake.sets().is_empty());
    }
}

/// Raw lofi answers ENOTTY to both and may wrap a disk with a cache.
#[test]
fn a_disk_that_does_not_know_the_flush_ioctl_is_refused() {
    for wce in [Err(libc::ENOTTY), Err(libc::ENOTSUP), Ok(false), Ok(true)] {
        let fake = Arc::new(Fake::new(Err(libc::ENOTTY), wce));
        let err = open(CHR, &fake).err().expect("refused");
        assert!(err.to_string().contains("cannot report a flush"), "{err}");
        assert!(err.to_string().contains("use a zvol"), "{err}");
        assert_eq!(fake.calls(), 1, "only the probe");
    }
}

#[test]
fn a_flushing_disk_without_cache_control_takes_the_ioctl() {
    let fake = Arc::new(Fake::new(Ok(()), Err(libc::ENOTTY)));
    let disk = open(CHR, &fake).expect("open");
    disk.sync().expect("sync");
    assert_eq!(fake.calls(), 2);
    assert!(fake.sets().is_empty());
}

#[test]
fn a_cache_on_that_cannot_flush_is_refused() {
    let fake = Arc::new(Fake::new(Err(libc::ENOTSUP), Ok(true)));
    let err = open(CHR, &fake).err().expect("refused");
    assert!(err.to_string().contains("cannot flush"), "{err}");
    assert!(
        err.to_string().contains("turn the write cache off"),
        "{err}"
    );
    assert_eq!(fake.sets(), [true], "refresh only");
    assert_eq!(fake.wce(), Ok(true));
}

#[test]
fn any_other_probe_error_is_refused() {
    for wce in [Ok(false), Ok(true), Err(libc::ENOTSUP)] {
        for probe in [libc::ENXIO, libc::EIO, libc::EINVAL, libc::ENODEV] {
            let fake = Arc::new(Fake::new(Err(probe), wce));
            let err = open(CHR, &fake).err().expect("refused");
            assert!(err.to_string().contains(&format!("os error {probe}")));
            assert_eq!(err.raw_os_error(), None);
        }
    }
}

#[test]
fn any_other_getwce_error_is_refused() {
    for get in [libc::EIO, libc::ENXIO, libc::EINVAL] {
        let fake = Arc::new(Fake::new(Ok(()), Err(get)));
        let err = open(CHR, &fake).err().expect("refused");
        assert!(err.to_string().contains(&format!("os error {get}")));
        assert!(err.to_string().contains("attach it read-only"), "{err}");
        assert_eq!(fake.calls(), 0);
        assert!(fake.sets().is_empty());
    }
}

#[test]
fn two_opens_of_a_disk_never_change_its_cache() {
    for (probe, wce) in [
        (Ok(()), Ok(false)),
        (Err(libc::ENOTSUP), Ok(false)),
        (Err(libc::ENOTSUP), Err(libc::ENOTTY)),
    ] {
        let fake = Arc::new(Fake::new(probe, wce).then_flush(&[probe]));
        let a = open(CHR, &fake).expect("open a");
        let b = open(CHR, &fake).expect("open b");
        drop(a);
        b.sync().expect("b flushes");
        drop(b);
        assert!(fake.sets().is_empty());
        assert_eq!(fake.wce(), wce);
    }

    // A cache found on is only refreshed, never turned off.
    let fake = Arc::new(Fake::new(Ok(()), Ok(true)));
    let a = open(CHR, &fake).expect("open a");
    let b = open(CHR, &fake).expect("open b");
    drop(a);
    drop(b);
    assert_eq!(fake.sets(), [true, true]);
    assert_eq!(fake.wce(), Ok(true));
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
    assert_eq!(fake.calls(), 1 + THREADS * EACH);
}

#[test]
fn dname_stops_at_the_first_nul() {
    assert_eq!(dname(b"zvol\0\0\0\0\0\0\0\0\0\0\0\0"), "zvol");
    assert_eq!(dname(b"zvol\0sd\0\0\0\0\0\0\0\0\0"), "zvol");
    assert_eq!(dname(b"abcdefghijklmnop"), "abcdefghijklmnop");
    assert_eq!(dname(&[0; 16]), "");
    assert_eq!(dname(&[]), "");
}

/// /dev/null answers DKIOCINFO and DKIOCGETWCE with ENXIO.
#[cfg(target_os = "illumos")]
#[test]
fn a_char_device_that_fails_getwce_is_refused() {
    let file = File::options()
        .write(true)
        .open("/dev/null")
        .expect("open /dev/null");
    let err = DiskSync::new(Arc::new(file), false, &discard())
        .err()
        .expect("/dev/null accepted as a disk");
    assert!(err
        .to_string()
        .contains(&format!("os error {}", libc::ENXIO)));
}
