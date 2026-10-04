// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::*;
use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

fn blocking(fd: impl std::os::fd::AsFd) -> bool {
    !rustix::fs::fcntl_getfl(fd)
        .expect("flags")
        .contains(rustix::fs::OFlags::NONBLOCK)
}

/// A listener whose backlog a few connects fill.
fn short_listener(path: &Path) -> OwnedFd {
    let fd = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None)
        .expect("socket");
    rustix::net::bind(&fd, &SocketAddrUnix::new(path).expect("addr"))
        .expect("bind");
    rustix::net::listen(&fd, 1).expect("listen");
    fd
}

/// Connections that fill `path`'s backlog, kept open so it stays full.
/// `None` where a full backlog refuses a connect instead, as illumos
/// does: there no connect waits.
fn fill_backlog(path: &Path) -> Option<Vec<OwnedFd>> {
    let addr = SocketAddrUnix::new(path).expect("addr");
    let mut held = Vec::new();
    for _ in 0..64 {
        let fd = nonblocking_socket(AddressFamily::UNIX).expect("socket");
        match rustix::net::connect(&fd, &addr) {
            Ok(()) => held.push(fd),
            Err(Errno::AGAIN) => return Some(held),
            Err(Errno::INPROGRESS) => {
                held.push(fd);
                return Some(held);
            }
            Err(Errno::CONNREFUSED) => return None,
            Err(e) => panic!("filling the backlog: {e}"),
        }
    }
    panic!("the backlog never filled");
}

#[test]
fn a_unix_connect_hands_back_a_blocking_stream() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("peer.sock");
    let listener = UnixListener::bind(&path).expect("bind");

    let mut stream = unix(&path, || false)
        .expect("not cancelled")
        .expect("connected");
    let (mut peer, _) = listener.accept().expect("accept");

    assert!(blocking(&stream));
    peer.write_all(&[7]).expect("write");
    let mut byte = [0u8; 1];
    stream.read_exact(&mut byte).expect("read");
    assert_eq!(byte, [7]);
}

#[test]
fn a_missing_unix_socket_is_a_connect_error_not_a_cancel() {
    let dir = tempfile::tempdir().expect("tempdir");
    let error = unix(&dir.path().join("absent.sock"), || false)
        .expect("not cancelled")
        .expect_err("nothing listens");
    assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
}

#[test]
fn a_stop_frees_a_unix_connect_to_a_full_backlog() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("full.sock");
    let _listener = short_listener(&path);
    let Some(_held) = fill_backlog(&path) else {
        let error = unix(&path, || false)
            .expect("not cancelled")
            .expect_err("refused");
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused, "{error}");
        return;
    };

    let stop = Arc::new(AtomicBool::new(false));
    let checks = Arc::new(AtomicUsize::new(0));
    let stopper = {
        let stop = Arc::clone(&stop);
        let checks = Arc::clone(&checks);
        thread::spawn(move || {
            // Several checks mean the connect is waiting, not starting.
            while checks.load(Ordering::Acquire) < 3 {
                thread::sleep(Duration::from_millis(10));
            }
            stop.store(true, Ordering::Release);
        })
    };

    let started = Instant::now();
    let error = unix(&path, || {
        checks.fetch_add(1, Ordering::AcqRel);
        stop.load(Ordering::Acquire)
    })
    .expect_err("cancelled");

    assert!(crate::is_cancel(&error), "{error}");
    assert!(started.elapsed() < Duration::from_secs(10));
    stopper.join().expect("stopper");
}

#[test]
fn a_tcp_connect_hands_back_a_blocking_stream() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let stream = tcp(addr, Duration::from_secs(10), || false)
        .expect("not cancelled")
        .expect("connected");

    assert!(blocking(&stream));
    let (_peer, from) = listener.accept().expect("accept");
    assert_eq!(from, stream.local_addr().expect("local"));
}

#[test]
fn a_refused_tcp_connect_is_a_connect_error() {
    let addr = TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr");

    let error = tcp(addr, Duration::from_secs(10), || false)
        .expect("not cancelled")
        .expect_err("nothing listens");
    assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused, "{error}");
}
