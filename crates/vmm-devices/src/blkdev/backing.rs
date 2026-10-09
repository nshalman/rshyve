// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A disk backing file or zvol that a migration destination opens only
//! after the commit.
//!
//! The orchestrator sends the last zfs increment into the destination's
//! zvol at the cutover gate, while the destination VMM already runs. A
//! zvol that is open refuses that receive with "dataset is busy".

use std::fs::File;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use super::{check_not_block, open_disk, DiskSync};

const FIRST_BACKOFF: Duration = Duration::from_millis(20);
const MAX_BACKOFF: Duration = Duration::from_millis(500);

/// A disk backing store, open now or opened by [`Self::attach`].
pub struct BackingDisk {
    path: PathBuf,
    read_only: bool,
    /// In bytes. The guest sees this size before the store is open, so
    /// the store must have it when it opens.
    capacity: u64,
    disk: OnceLock<Arc<DiskSync>>,
    /// Two attaches must not both open the store.
    opening: Mutex<()>,
    log: slog::Logger,
}

impl BackingDisk {
    /// [`Self::deferred`] for a migration destination, else
    /// [`Self::open`].
    pub fn new(
        path: &Path,
        read_only: bool,
        defer_open: bool,
        log: &slog::Logger,
    ) -> io::Result<Self> {
        if defer_open {
            Self::deferred(path, read_only, log)
        } else {
            Self::open(path, read_only, log)
        }
    }

    /// Open the store now, as every VM that is not a migration
    /// destination does.
    pub fn open(
        path: &Path,
        read_only: bool,
        log: &slog::Logger,
    ) -> io::Result<Self> {
        let file = open_disk(path, read_only)?;
        let capacity = file.metadata()?.len();
        let disk = DiskSync::new(Arc::new(file), read_only, log)?;
        Ok(Self::with_disk(path, read_only, capacity, Arc::new(disk), log))
    }

    /// A store that is open already. For a caller that opened it itself.
    pub fn from_disk(
        disk: Arc<DiskSync>,
        read_only: bool,
        log: &slog::Logger,
    ) -> io::Result<Self> {
        let capacity = disk.file().metadata()?.len();
        Ok(Self::with_disk(Path::new(""), read_only, capacity, disk, log))
    }

    fn with_disk(
        path: &Path,
        read_only: bool,
        capacity: u64,
        disk: Arc<DiskSync>,
        log: &slog::Logger,
    ) -> Self {
        let slot = OnceLock::new();
        let _ = slot.set(disk);
        Self {
            path: path.to_path_buf(),
            read_only,
            capacity,
            disk: slot,
            opening: Mutex::new(()),
            log: log.clone(),
        }
    }

    /// Learn the size and close the store again. [`Self::attach`] opens
    /// it for the guest.
    ///
    /// The probe holds the store for one open and one fstat, at VMM
    /// start. The last increment comes later, at the gate.
    pub fn deferred(
        path: &Path,
        read_only: bool,
        log: &slog::Logger,
    ) -> io::Result<Self> {
        let probe = File::open(path)?;
        let meta = probe.metadata()?;
        drop(probe);
        // A writable block node would fail at the attach, after the
        // source has given the guest up.
        check_not_block(meta.mode(), read_only, Some(path))?;
        slog::info!(log, "disk opens after the migration commits";
            "path" => %path.display(), "bytes" => meta.len());
        Ok(Self {
            path: path.to_path_buf(),
            read_only,
            capacity: meta.len(),
            disk: OnceLock::new(),
            opening: Mutex::new(()),
            log: log.clone(),
        })
    }

    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }

    pub fn is_attached(&self) -> bool {
        self.disk.get().is_some()
    }

    /// The open store, or `None` before [`Self::attach`].
    pub fn get(&self) -> Option<&Arc<DiskSync>> {
        self.disk.get()
    }

    /// Open the store, retrying a missing or busy node until `until`.
    /// A store whose size changed is refused at once: the guest already
    /// has the old size.
    pub fn attach(&self, until: Instant) -> io::Result<()> {
        let _one = self.opening.lock().unwrap_or_else(|e| e.into_inner());
        if self.is_attached() {
            return Ok(());
        }
        let mut backoff = FIRST_BACKOFF;
        let mut tries = 0u32;
        loop {
            tries += 1;
            let error = match self.open_checked() {
                Ok(disk) => {
                    let _ = self.disk.set(Arc::new(disk));
                    slog::info!(self.log, "disk opened for the guest";
                        "path" => %self.path.display(), "tries" => tries);
                    return Ok(());
                }
                Err(e) if !transient(&e) => return Err(e),
                Err(e) => e,
            };
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    error.kind(),
                    format!("{error} (after {tries} tries)"),
                ));
            }
            thread::sleep(backoff.min(left));
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    fn open_checked(&self) -> io::Result<DiskSync> {
        let file = open_disk(&self.path, self.read_only).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("open {}: {e}", self.path.display()),
            )
        })?;
        let now = file.metadata()?.len();
        if now != self.capacity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} is {now} bytes; the guest has {} bytes",
                    self.path.display(),
                    self.capacity
                ),
            ));
        }
        DiskSync::new(Arc::new(file), self.read_only, &self.log)
    }
}

/// A zvol node can be missing or busy for a moment after a receive.
fn transient(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ENOENT | libc::EBUSY | libc::EAGAIN | libc::EINTR)
    )
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn log() -> slog::Logger {
        slog::Logger::root(slog::Discard, slog::o!())
    }

    fn image(bytes: usize) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().expect("tempfile");
        f.write_all(&vec![0u8; bytes]).expect("fill");
        f
    }

    #[test]
    fn a_deferred_disk_holds_nothing_until_it_attaches() {
        let img = image(8192);
        let disk = BackingDisk::deferred(img.path(), false, &log())
            .expect("probe");
        assert_eq!(disk.capacity(), 8192);
        assert!(!disk.is_attached());
        assert!(disk.get().is_none());

        disk.attach(Instant::now()).expect("attach");
        assert!(disk.is_attached());
        // A second attach keeps the first open.
        let first = Arc::as_ptr(disk.get().expect("open"));
        disk.attach(Instant::now()).expect("again");
        assert_eq!(Arc::as_ptr(disk.get().expect("open")), first);
    }

    #[test]
    fn a_missing_disk_fails_the_attach_and_a_retry_takes_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("disk");
        std::fs::write(&path, vec![0u8; 4096]).expect("write");
        let disk = BackingDisk::deferred(&path, false, &log()).expect("probe");

        std::fs::remove_file(&path).expect("remove");
        let until = Instant::now() + Duration::from_millis(60);
        let e = disk.attach(until).expect_err("no node");
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
        assert!(Instant::now() >= until, "a missing node is retried");
        assert!(!disk.is_attached());

        std::fs::write(&path, vec![0u8; 4096]).expect("write");
        disk.attach(Instant::now()).expect("the retry opens it");
        assert!(disk.is_attached());
    }

    #[test]
    fn a_disk_that_changed_size_is_refused_at_once() {
        let img = image(8192);
        let disk = BackingDisk::deferred(img.path(), false, &log())
            .expect("probe");
        img.as_file().set_len(4096).expect("shrink");

        let started = Instant::now();
        let e = disk
            .attach(started + Duration::from_secs(30))
            .expect_err("another size");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
        assert!(started.elapsed() < Duration::from_secs(5), "not retried");
        assert!(!disk.is_attached());
    }

    #[test]
    fn an_open_disk_is_attached_from_the_start() {
        let img = image(4096);
        let disk = BackingDisk::open(img.path(), false, &log()).expect("open");
        assert!(disk.is_attached());
        assert_eq!(disk.capacity(), 4096);
    }
}
