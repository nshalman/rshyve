// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The control thread against an engine socket played by the test.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use manta_block_ring::control::Message;
use vmm_storage::nvme::backend::{BlockOp, BlockStatus};

use super::{job, pattern, setup_full, Calls, Engine, TJob};
use crate::conn::Conn;
use crate::{Config, Ring};

/// A ring whose control socket's other end is returned to the test.
fn setup_ctl(
    tune: impl FnOnce(&mut Config),
) -> (Arc<Ring<TJob>>, Arc<Calls>, Engine, UnixStream) {
    let (ours, theirs) = UnixStream::pair().unwrap();
    let calls = Arc::new(Calls::default());
    let (ring, eng) = setup_full(
        8,
        8,
        1,
        true,
        Arc::clone(&calls) as _,
        Some(Conn::new(ours).unwrap()),
        tune,
    );
    (ring, calls, eng, theirs)
}

/// Runs `halt` and returns how long it took, or None past `limit`.
fn time_halt(ring: Arc<Ring<TJob>>, limit: Duration) -> Option<Duration> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let t = Instant::now();
        ring.halt();
        let _ = tx.send(t.elapsed());
    });
    rx.recv_timeout(limit).ok()
}

#[test]
fn an_engine_that_floods_pongs_cannot_hold_off_halt() {
    let (ring, _calls, _eng, mut theirs) = setup_ctl(|c| {
        c.deadline = Duration::from_millis(200);
        c.heartbeat_limit = Duration::from_secs(30);
    });
    let stop = Arc::new(AtomicBool::new(false));
    let flooding = Arc::clone(&stop);
    let burst: Vec<u8> =
        (0..1000).flat_map(|_| Message::Pong.encode(0)).collect();
    let flood = thread::spawn(move || {
        while !flooding.load(Ordering::Relaxed) {
            if theirs.write_all(&burst).is_err() {
                break;
            }
        }
    });
    thread::sleep(Duration::from_millis(100));
    let took = time_halt(ring, Duration::from_secs(10));
    stop.store(true, Ordering::Relaxed);
    let took = took.expect("halt did not return under a Pong flood");
    assert!(took < Duration::from_secs(3), "halt took {took:?}");
    flood.join().unwrap();
}

#[test]
fn a_hung_engine_holds_halt_for_the_deadline_not_the_io_limit() {
    let (ring, _calls, _eng, _theirs) = setup_ctl(|c| {
        c.deadline = Duration::from_millis(300);
        c.heartbeat_limit = Duration::from_secs(30);
        c.io_limit = Duration::from_secs(30);
    });
    let took = time_halt(ring, Duration::from_secs(10))
        .expect("halt waited for the I/O limit");
    assert!(took < Duration::from_secs(3), "halt took {took:?}");
}

/// Answers every Ping, and nothing else, until the socket closes.
fn answer_pings(mut s: UnixStream) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        while let Ok(Some((msg, _, used))) = Message::decode(&buf) {
            buf.drain(..used);
            if msg == Message::Ping
                && s.write_all(&Message::Pong.encode(0)).is_err()
            {
                return;
            }
        }
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

#[test]
fn a_request_past_the_io_limit_loses_the_engine() {
    let (ring, calls, mut eng, theirs) = setup_ctl(|c| {
        c.io_limit = Duration::from_millis(200);
    });
    thread::spawn(move || answer_pings(theirs));
    ring.submit(job(1, 0, BlockOp::Read, 0, pattern(1, 0)));
    eng.take_n(1);
    assert_eq!(calls.wait_for(1), vec![(1, BlockStatus::Internal)]);
    assert!(ring.is_lost());
}

#[test]
fn a_frame_already_read_is_returned_past_the_deadline() {
    let (ours, mut theirs) = UnixStream::pair().unwrap();
    let mut conn = Conn::new(ours).unwrap();
    let two: Vec<u8> = [Message::Pong, Message::Detached]
        .iter()
        .flat_map(|m| m.encode(0))
        .collect();
    theirs.write_all(&two).unwrap();
    let soon = Instant::now() + Duration::from_secs(5);
    assert_eq!(conn.recv(soon).unwrap().unwrap().msg, Message::Pong);
    assert!(conn.frame_buffered());
    let got = conn.recv(Instant::now()).unwrap().unwrap();
    assert_eq!(got.msg, Message::Detached);
    assert!(!conn.frame_buffered());
}
