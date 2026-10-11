// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A ring disk that a migration detaches and attaches, against an engine
//! socket played by the test.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use manta_block_ring::control::{AttachOk, Message, ATTACH_DURABLE};
use manta_block_ring::layout::LAYOUT_VERSION;
use manta_block_ring::Geometry;
use vmm_devices::lifecycle::BackingAttach;
use vmm_storage::nvme::backend::{BlockOp, BlockStatus};

use super::reattach::handshake_with;
use super::{job, pattern, Calls, Engine, TJob, BLOCKS};
use crate::{Config, Handover, RingShape, MIGRATE_V1};

const DEPTH: u32 = 8;
const BUF_PAGES: u32 = 16;
const MAX_BLOCKS: u32 = 2;
const GENERATION: u64 = 0x77;

fn geometry() -> Geometry {
    Geometry::new(1, DEPTH, BUF_PAGES).unwrap()
}

fn grant() -> AttachOk {
    let g = geometry();
    AttachOk {
        generation: GENERATION,
        queues: g.queues(),
        depth: g.depth(),
        buf_pages: g.buf_pages(),
        max_blocks: MAX_BLOCKS,
        volume_blocks: BLOCKS,
        region_len: g.region_len() as u64,
        layout_version: LAYOUT_VERSION,
        flags: ATTACH_DURABLE,
        sector_bytes: 4096,
    }
}

/// The shape [`grant`] gives.
fn shape() -> RingShape {
    RingShape {
        blocks: BLOCKS,
        sector: 4096,
        queues: 1,
        depth: DEPTH,
        buf_pages: BUF_PAGES,
        tags: DEPTH as u16,
        part_blocks: MAX_BLOCKS,
        durable: true,
        zeroing: false,
    }
}

fn config(socket: PathBuf) -> Config {
    let log = slog::Logger::root(slog::Discard, slog::o!());
    let mut cfg = Config::new(socket, uuid::Uuid::from_u128(9), log);
    cfg.spin = Duration::from_micros(5);
    cfg.reattach = Duration::ZERO;
    cfg.deadline = Duration::from_secs(2);
    cfg
}

fn within(secs: u64, allow_unproven: bool) -> BackingAttach {
    BackingAttach {
        until: Instant::now() + Duration::from_secs(secs),
        allow_unproven,
    }
}

/// An engine for one attach at `path`: it binds the socket after
/// `late`, answers Hello and Attach with `features`, then Pings and a
/// Detach until the socket closes. Gives back the Attach and the ring
/// side.
fn serve_once(
    path: PathBuf,
    late: Duration,
    features: u64,
    ok: AttachOk,
) -> thread::JoinHandle<(manta_block_ring::control::Attach, Engine, bool)> {
    thread::spawn(move || {
        thread::sleep(late);
        let listener = UnixListener::bind(&path).unwrap();
        let (mut s, _) = listener.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let (mut engines, fds) = Engine::all(geometry(), ok.generation);
        let ask = handshake_with(&mut s, ok, &fds, 2, features);
        let detached = answer_until_detach(&mut s);
        (ask, engines.remove(0), detached)
    })
}

/// Answers Pings until a Detach, which it confirms. False when the
/// socket closed first.
fn answer_until_detach(s: &mut UnixStream) -> bool {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        while let Ok(Some((msg, _, used))) = Message::decode_as(&buf, 2) {
            buf.drain(..used);
            match msg {
                Message::Ping
                    if s.write_all(&Message::Pong.encode_as(0, 2)).is_err() =>
                {
                    return false;
                }
                Message::Detach => {
                    let reply = Message::Detached.encode_as(0, 2);
                    return s.write_all(&reply).is_ok();
                }
                _ => {}
            }
        }
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => return false,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

fn deferred(dir: &Path) -> (Arc<Handover<TJob>>, Arc<Calls>) {
    let calls = Arc::new(Calls::default());
    let h = Handover::<TJob>::deferred(
        config(dir.join("engine.sock")),
        shape(),
        Arc::clone(&calls) as _,
    );
    (h, calls)
}

#[test]
fn a_shape_reads_back_as_written_and_refuses_anything_else() {
    let s = shape();
    assert_eq!(s.to_string(), "1:256:4096:1:8:16:8:2:d");
    assert_eq!(s.to_string().parse::<RingShape>(), Ok(s));
    let both = RingShape {
        durable: true,
        zeroing: true,
        sector: 512,
        ..s
    };
    assert_eq!(both.to_string().parse::<RingShape>(), Ok(both));
    for bad in [
        "",
        "2:256:4096:1:8:16:8:2:d",
        "1:256:4096:1:8:16:8:2",
        "1:256:4096:1:8:16:8:2:d:x",
        "1:0:4096:1:8:16:8:2:d",
        "1:256:1024:1:8:16:8:2:d",
        "1:256:4096:0:8:16:8:2:d",
        "1:256:4096:16:8:16:8:2:d",
        "1:256:4096:1:6:16:8:2:d",
        "1:256:4096:1:8:0:8:2:d",
        "1:256:4096:1:8:16:0:2:d",
        "1:256:4096:1:8:16:8:0:d",
        "1:256:4096:1:8:16:8:2:zd",
        "1:+256:4096:1:8:16:8:2:d",
        "1:18446744073709551615:4096:1:8:16:8:2:d",
    ] {
        assert!(bad.parse::<RingShape>().is_err(), "took '{bad}'");
    }
}

#[test]
fn a_deferred_disk_fails_a_request_and_says_it_is_detached() {
    let dir = tempfile::tempdir().unwrap();
    let (h, calls) = deferred(dir.path());
    assert!(!h.is_attached());
    assert_eq!(h.waivable(), None);
    h.submit(job(1, 0, BlockOp::Read, 0, pattern(1, 0)));
    assert_eq!(calls.wait_for(1), vec![(1, BlockStatus::Internal)]);
}

#[test]
fn a_destination_attaches_once_the_engine_opens_the_socket() {
    let dir = tempfile::tempdir().unwrap();
    let (h, calls) = deferred(dir.path());
    let engine = serve_once(
        dir.path().join("engine.sock"),
        Duration::from_millis(300),
        MIGRATE_V1,
        grant(),
    );
    h.attach(&within(10, false)).expect("attach");
    assert!(h.is_attached());
    assert_eq!(h.waivable(), None);

    h.submit(job(1, 0, BlockOp::Write { fua: false }, 3, pattern(1, 1)));
    h.detach().expect("a confirmed Detach");
    let (ask, mut eng, detached) = engine.join().unwrap();
    assert!(detached);
    // A fresh Attach, never a reattach of the source's generation, for
    // what the source was granted.
    assert_eq!(ask.generation, 0);
    assert_eq!(
        (ask.queues, ask.depth, ask.buf_pages),
        (1, DEPTH, BUF_PAGES)
    );
    assert_eq!(ask.max_blocks, MAX_BLOCKS);
    let got = eng.take(8);
    assert_eq!(got.len(), 1, "{got:?}");
    let _ = calls;
}

#[test]
fn an_engine_without_migrate_v1_is_refused_unless_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let (h, _calls) = deferred(dir.path());
    let path = dir.path().join("engine.sock");
    let engine = serve_once(path.clone(), Duration::ZERO, 0, grant());
    let why = h.attach(&within(5, false)).expect_err("no migrate-v1");
    assert!(why.contains("migrate-v1"), "{why}");
    assert!(!h.is_attached());
    let (_, _, detached) = engine.join().unwrap();
    assert!(!detached, "a refused attachment was used");

    std::fs::remove_file(&path).unwrap();
    let engine = serve_once(path, Duration::ZERO, 0, grant());
    h.attach(&within(5, true)).expect("allowed by the operator");
    assert!(h.waivable().is_some_and(|w| w.contains("migrate-v1")));
    h.halt();
    engine.join().unwrap();
}

#[test]
fn a_grant_of_another_shape_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (h, _calls) = deferred(dir.path());
    // Smaller buffer slots: the guest was told another optimal write.
    let small = AttachOk {
        max_blocks: 1,
        ..grant()
    };
    let engine = serve_once(
        dir.path().join("engine.sock"),
        Duration::ZERO,
        MIGRATE_V1,
        small,
    );
    let why = h.attach(&within(5, false)).expect_err("another shape");
    assert!(why.contains("shape"), "{why}");
    assert!(!h.is_attached());
    engine.join().unwrap();
}

#[test]
fn no_engine_by_the_deadline_fails_the_attach() {
    let dir = tempfile::tempdir().unwrap();
    let (h, _calls) = deferred(dir.path());
    let t = Instant::now();
    let why = h
        .attach(&BackingAttach {
            until: Instant::now() + Duration::from_millis(300),
            allow_unproven: false,
        })
        .expect_err("no engine");
    assert!(why.contains("no attach in time"), "{why}");
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
}

#[test]
fn a_halt_ends_an_attach_that_waits() {
    let dir = tempfile::tempdir().unwrap();
    let (h, _calls) = deferred(dir.path());
    let waiting = Arc::clone(&h);
    let t = thread::spawn(move || waiting.attach(&within(30, false)));
    thread::sleep(Duration::from_millis(100));
    h.halt();
    let why = t.join().unwrap().expect_err("halted");
    assert!(why.contains("halted"), "{why}");
}

#[test]
fn a_source_detaches_after_the_commit_and_attaches_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("engine.sock");
    let (h, calls) = deferred(dir.path());
    let first = serve_once(path.clone(), Duration::ZERO, MIGRATE_V1, grant());
    h.attach(&within(5, false)).expect("first attach");
    h.detach().expect("detach");
    assert!(!h.is_attached());
    let (_, _, detached) = first.join().unwrap();
    assert!(detached, "no Detach reached the engine");

    // The guest is taken back: a fresh Attach, then I/O goes on.
    std::fs::remove_file(&path).unwrap();
    let again = serve_once(path, Duration::ZERO, MIGRATE_V1, grant());
    h.attach(&within(5, false)).expect("attach again");
    h.submit(job(2, 0, BlockOp::Read, 1, pattern(1, 0)));
    h.halt();
    let (ask, mut eng, _) = again.join().unwrap();
    assert_eq!(ask.generation, 0);
    // The read went to the new ring, or failed in the halt.
    let sent = eng.take(8).len();
    assert!(sent == 1 || calls.status(2) == Some(BlockStatus::Internal));
}
