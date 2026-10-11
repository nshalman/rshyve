// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Fails a migration whose sends stop moving.
//!
//! A send has no deadline on illumos (see [`Transport`]). When the
//! peer dies behind a bridge that keeps our socket open, the send
//! blocks forever and the source never learns. A watchdog shuts the
//! socket once one write has made no progress for the stall limit,
//! and the blocked send then fails.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use crate::wire::{Shutter, Transport};

/// The most one `write` hands the kernel, so a slow but live peer
/// shows progress well inside the stall limit.
const WRITE_CHUNK: usize = 64 * 1024;

/// The longest the watchdog sleeps between checks.
const MAX_TICK: Duration = Duration::from_secs(1);

#[derive(Debug)]
struct Progress {
    epoch: Instant,
    /// Milliseconds since `epoch` of the last write start or progress.
    last_ms: AtomicU64,
    writing: AtomicBool,
    stalled: AtomicBool,
}

impl Progress {
    fn touch(&self) {
        let ms =
            u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_ms.store(ms, Ordering::Release);
    }

    fn idle(&self) -> Duration {
        let last = Duration::from_millis(self.last_ms.load(Ordering::Acquire));
        self.epoch.elapsed().saturating_sub(last)
    }
}

/// A transport whose writes the watchdog can see.
pub struct Watched<S> {
    inner: S,
    progress: Arc<Progress>,
}

impl<S: Read> Read for Watched<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<S: Write> Write for Watched<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let chunk = &buf[..buf.len().min(WRITE_CHUNK)];
        self.progress.touch();
        self.progress.writing.store(true, Ordering::Release);
        let result = self.inner.write(chunk);
        self.progress.writing.store(false, Ordering::Release);
        self.progress.touch();
        result
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<S: Transport> Transport for Watched<S> {
    fn set_read_deadline(&self, budget: Duration) -> io::Result<()> {
        self.inner.set_read_deadline(budget)
    }

    fn shutter(&self) -> io::Result<Shutter> {
        self.inner.shutter()
    }
}

/// The watchdog. Dropping it stops the thread.
pub struct Watch {
    progress: Arc<Progress>,
    limit: Duration,
    _stop: mpsc::Sender<()>,
}

impl Watch {
    /// Whether the watchdog shut the stream.
    pub fn stalled(&self) -> bool {
        self.progress.stalled.load(Ordering::Acquire)
    }

    /// The error a stalled migration ends with.
    pub fn reason(&self) -> String {
        format!(
            "the migration stream made no progress for {} s; the peer is \
             gone or stuck",
            self.limit.as_secs()
        )
    }
}

/// Wrap `stream` and start its watchdog.
pub fn watch<S: Transport>(
    stream: S,
    limit: Duration,
    log: &slog::Logger,
) -> io::Result<(Watched<S>, Watch)> {
    let shut = stream.shutter()?;
    let progress = Arc::new(Progress {
        epoch: Instant::now(),
        last_ms: AtomicU64::new(0),
        writing: AtomicBool::new(false),
        stalled: AtomicBool::new(false),
    });
    let (stop, stopped) = mpsc::channel::<()>();
    let tick = (limit / 4).clamp(Duration::from_millis(10), MAX_TICK);
    let watched = Arc::clone(&progress);
    let log = log.clone();
    std::thread::Builder::new()
        .name("migrate-stall".into())
        .spawn(move || loop {
            match stopped.recv_timeout(tick) {
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                _ => return,
            }
            if watched.writing.load(Ordering::Acquire)
                && watched.idle() >= limit
            {
                slog::error!(log, "the migration stream stalled; \
                    shutting it down";
                    "limit_s" => limit.as_secs());
                watched.stalled.store(true, Ordering::Release);
                shut();
                return;
            }
        })?;
    Ok((
        Watched {
            inner: stream,
            progress: Arc::clone(&progress),
        },
        Watch {
            progress,
            limit,
            _stop: stop,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    fn discard() -> slog::Logger {
        slog::Logger::root(slog::Discard, slog::o!())
    }

    #[test]
    fn a_send_nobody_reads_fails_within_the_limit() {
        // The lab peer died behind a bridge that kept the socket open.
        let (ours, _peer) = UnixStream::pair().expect("pair");
        let (mut stream, watch) =
            watch(ours, Duration::from_millis(200), &discard()).expect("watch");
        let started = Instant::now();
        let block = vec![0u8; 1 << 20];
        let error = loop {
            if let Err(e) = stream.write_all(&block) {
                break e;
            }
        };
        assert!(watch.stalled(), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_reading_peer_never_stalls() {
        let (ours, mut peer) = UnixStream::pair().expect("pair");
        let reader = std::thread::spawn(move || {
            let mut sink = Vec::new();
            peer.read_to_end(&mut sink).expect("read");
            sink.len()
        });
        let (mut stream, watch) =
            watch(ours, Duration::from_millis(200), &discard()).expect("watch");
        let block = vec![0u8; 4 << 20];
        stream.write_all(&block).expect("a live peer takes it all");
        // Idle without a write in flight is not a stall.
        std::thread::sleep(Duration::from_millis(400));
        assert!(!watch.stalled());
        drop(stream);
        drop(watch);
        assert_eq!(reader.join().expect("reader"), block.len());
    }
}
