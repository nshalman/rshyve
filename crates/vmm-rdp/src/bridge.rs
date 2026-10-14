// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Connections for a runtime with no I/O driver. A bhyve zone has neither
//! /dev/poll nor /dev/eventfd, which tokio's driver needs on illumos, so
//! sockets stay std and each connection reaches IronRDP as an in-memory
//! duplex that two plain threads keep filled.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::runtime::Handle;

use crate::Listen;

const BUF: usize = 64 * 1024;

pub(crate) enum Listener {
    Tcp(TcpListener),
    Unix(UnixListener),
}

impl Listener {
    pub(crate) fn bind(listen: &Listen) -> anyhow::Result<Self> {
        use anyhow::Context;
        let listener = match listen {
            Listen::Tcp(addr) => Self::Tcp(
                TcpListener::bind(addr)
                    .with_context(|| format!("RDP: binding {addr}"))?,
            ),
            Listen::Unix(path) => Self::Unix(
                vmm_core::unixsock::bind_restricted(
                    path,
                    vmm_core::unixsock::SocketPolicy::default(),
                )
                .with_context(|| format!("RDP: binding {}", path.display()))?,
            ),
        };
        match &listener {
            Self::Tcp(l) => l.set_nonblocking(true),
            Self::Unix(l) => l.set_nonblocking(true),
        }
        .context("RDP: making the listener non-blocking")?;
        Ok(listener)
    }

    /// A waiting connection, or `None` when there is none yet.
    pub(crate) fn try_accept(&self) -> io::Result<Option<Conn>> {
        let accepted = match self {
            // RDP's small PDUs (input, frame and audio acknowledgements)
            // must not wait behind each other for Nagle.
            Self::Tcp(l) => l
                .accept()
                .and_then(|(s, _)| s.set_nodelay(true).map(|()| Conn::Tcp(s))),
            Self::Unix(l) => l.accept().map(|(s, _)| Conn::Unix(s)),
        };
        match accepted {
            Ok(conn) => Ok(Some(conn)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }
}

pub(crate) enum Conn {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl Conn {
    fn try_clone(&self) -> io::Result<Self> {
        Ok(match self {
            Self::Tcp(s) => Self::Tcp(s.try_clone()?),
            Self::Unix(s) => Self::Unix(s.try_clone()?),
        })
    }

    fn set_blocking(&self) -> io::Result<()> {
        match self {
            Self::Tcp(s) => s.set_nonblocking(false),
            Self::Unix(s) => s.set_nonblocking(false),
        }
    }

    fn shutdown(&self) -> io::Result<()> {
        match self {
            Self::Tcp(s) => s.shutdown(Shutdown::Both),
            Self::Unix(s) => s.shutdown(Shutdown::Both),
        }
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(s) => s.read(buf),
            Self::Unix(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(s) => s.write(buf),
            Self::Unix(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp(s) => s.flush(),
            Self::Unix(s) => s.flush(),
        }
    }
}

/// When the outbound thread's current write began. illumos ignores
/// SO_SNDTIMEO on a Unix socket, so a stalled write is spotted here and
/// ended by shutting the socket down.
struct WriteClock {
    base: Instant,
    /// Milliseconds past `base` plus one, or 0 between writes.
    since: AtomicU64,
}

impl WriteClock {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            since: AtomicU64::new(0),
        }
    }

    fn start(&self) {
        let at = u64::try_from(self.base.elapsed().as_millis())
            .unwrap_or(u64::MAX - 1);
        self.since.store(at.saturating_add(1), Ordering::Release);
    }

    fn stop(&self) {
        self.since.store(0, Ordering::Release);
    }

    fn waiting(&self) -> Option<Duration> {
        match self.since.load(Ordering::Acquire) {
            0 => None,
            at => Some(
                self.base
                    .elapsed()
                    .saturating_sub(Duration::from_millis(at - 1)),
            ),
        }
    }
}

/// Bytes the client may still send before the server trusts it. IronRDP
/// keeps what arrives before login, so an untrusted client is held to a
/// budget; [`InboundBudget::lift`] removes it.
pub(crate) struct InboundBudget(AtomicU64);

impl InboundBudget {
    pub(crate) fn new(bytes: u64) -> Self {
        Self(AtomicU64::new(bytes))
    }

    pub(crate) fn lift(&self) {
        self.0.store(u64::MAX, Ordering::Release);
    }

    pub(crate) fn lifted(&self) -> bool {
        self.0.load(Ordering::Acquire) == u64::MAX
    }

    // `try_update` replaces this from Rust 1.95, and the MSRV is 1.94.
    #[allow(deprecated)]
    fn spend(&self, bytes: usize) -> bool {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                if left == u64::MAX {
                    Some(left)
                } else {
                    left.checked_sub(bytes)
                }
            })
            .is_ok()
    }
}

/// DTrace probes for what goes out to the client; `tools/audio-rate.d`
/// reads them.
#[usdt::provider(provider = "vmm")]
mod probes {
    /// The bridge wrote `bytes` to the client, which took `write_us`;
    /// `backlog` is what the connection still has unwritten.
    fn rdp_write(bytes: u64, write_us: u64, backlog: u64) {}
}

/// One connection's EGFX frame and audio bytes handed to the server
/// loop, less what the bridge has written to the client. IronRDP queues
/// those without limit: a client that suspends its frame
/// acknowledgements is never held back by them, and waves go out
/// unconfirmed. This is what keeps a client that reads slowly or not
/// at all from growing the queue without end. The classic bitmap path
/// needs no count: the server loop pulls its updates as it writes them.
///
/// The bridge's bytes also carry TLS and PDU framing and the small
/// PDUs (pointer, input acknowledgements), so this is a little under
/// what the client has not read.
pub(crate) struct OutboundBacklog(AtomicU64);

/// The EGFX frame and audio bytes a client may leave unread before more
/// wait (frames) or are dropped (audio): a few seconds of a busy 4K
/// display.
pub(crate) const OUTPUT_BACKLOG_CAP: u64 = 32 << 20;

impl OutboundBacklog {
    pub(crate) fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    pub(crate) fn queued(&self, bytes: usize) {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.0.fetch_add(bytes, Ordering::AcqRel);
    }

    /// The bridge writes every PDU, not only those counted in, so this
    /// stops at nothing.
    // `try_update` replaces this from Rust 1.95, and the MSRV is 1.94.
    #[allow(deprecated)]
    pub(crate) fn written(&self, bytes: usize) {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        let _ =
            self.0
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                    Some(left.saturating_sub(bytes))
                });
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }

    /// Whether the client has more than [`OUTPUT_BACKLOG_CAP`] unread.
    pub(crate) fn over_cap(&self) -> bool {
        self.bytes() > OUTPUT_BACKLOG_CAP
    }
}

/// Shuts the socket down when dropped. That wakes both bridge threads
/// whatever they are blocked on, including a write to a client that has
/// stopped reading.
pub(crate) struct Closer {
    conn: Conn,
    log: slog::Logger,
    writing: Arc<WriteClock>,
}

impl Closer {
    /// How long the write to the client in progress has been waiting, if
    /// one is.
    pub(crate) fn stalled_for(&self) -> Option<Duration> {
        self.writing.waiting()
    }
}

impl Drop for Closer {
    fn drop(&mut self) {
        shutdown(&self.conn, &self.log);
    }
}

fn shutdown(conn: &Conn, log: &slog::Logger) {
    match conn.shutdown() {
        Err(e) if e.kind() != io::ErrorKind::NotConnected => {
            slog::debug!(log, "RDP socket shutdown failed"; "error" => %e);
        }
        _ => {}
    }
}

/// Carry `conn` to the returned stream until either side closes, until
/// the returned [`Closer`] is dropped, or until the client overspends
/// `budget`. What reaches the client comes off `backlog`.
pub(crate) fn bridge(
    conn: Conn,
    rt: &Handle,
    log: &slog::Logger,
    budget: Arc<InboundBudget>,
    backlog: Arc<OutboundBacklog>,
) -> io::Result<(DuplexStream, Closer)> {
    conn.set_blocking()?;
    let writing = Arc::new(WriteClock::new());
    let closer = Closer {
        conn: conn.try_clone()?,
        log: log.clone(),
        writing: Arc::clone(&writing),
    };
    let (ours, theirs) = tokio::io::duplex(BUF);
    let (mut from_peer, mut to_peer) = tokio::io::split(theirs);
    // Dropped when the outbound thread leaves, so an inbound write the
    // server will never read stops waiting.
    let (out_alive, mut out_gone) = tokio::sync::watch::channel(());
    let mut inbound = conn.try_clone()?;
    let mut outbound = conn;

    let rt_in = rt.clone();
    let log_in = log.clone();
    thread::Builder::new()
        .name("rdp-in".into())
        .spawn(move || {
            let mut buf = vec![0; BUF];
            loop {
                let n = match inbound.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) => {
                        slog::debug!(log_in, "RDP client read failed"; "error" => %e);
                        break;
                    }
                };
                if !budget.spend(n) {
                    slog::info!(log_in, "RDP client sent too much before login");
                    break;
                }
                let delivered = rt_in.block_on(async {
                    tokio::select! {
                        r = to_peer.write_all(&buf[..n]) => r.is_ok(),
                        _ = out_gone.changed() => false,
                    }
                });
                if !delivered {
                    break;
                }
            }
            if let Err(e) = rt_in.block_on(to_peer.shutdown()) {
                slog::debug!(log_in, "RDP server side already closed"; "error" => %e);
            }
        })?;

    let rt_out = rt.clone();
    let log_out = log.clone();
    thread::Builder::new()
        .name("rdp-out".into())
        .spawn(move || {
            let _alive = out_alive;
            let mut buf = vec![0; BUF];
            loop {
                match rt_out.block_on(from_peer.read(&mut buf)) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        writing.start();
                        let began = Instant::now();
                        let wrote = outbound.write_all(&buf[..n]);
                        let took = began.elapsed();
                        writing.stop();
                        if let Err(e) = wrote {
                            slog::debug!(log_out, "RDP client write failed"; "error" => %e);
                            break;
                        }
                        backlog.written(n);
                        probes::rdp_write!(|| (
                            n as u64,
                            u64::try_from(took.as_micros()).unwrap_or(u64::MAX),
                            backlog.bytes()
                        ));
                    }
                }
            }
            // Wakes the inbound thread out of its blocking read.
            shutdown(&outbound, &log_out);
        })?;

    Ok((ours, closer))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("a runtime without an I/O driver")
    }

    fn connected(
        rt: &tokio::runtime::Runtime,
    ) -> (tempfile::TempDir, UnixStream, DuplexStream, Closer) {
        connected_with(rt, Arc::new(OutboundBacklog::new()))
    }

    /// As `connected`, with the written bytes taken off `backlog`.
    fn connected_with(
        rt: &tokio::runtime::Runtime,
        backlog: Arc<OutboundBacklog>,
    ) -> (tempfile::TempDir, UnixStream, DuplexStream, Closer) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let listener =
            Listener::bind(&Listen::Unix(dir.path().join("s"))).expect("bind");
        let client =
            UnixStream::connect(dir.path().join("s")).expect("connect");
        let conn = loop {
            if let Some(conn) = listener.try_accept().expect("accept") {
                break conn;
            }
        };
        let log = slog::Logger::root(slog::Discard, slog::o!());
        let budget = Arc::new(InboundBudget::new(0));
        budget.lift();
        let (server, closer) =
            bridge(conn, rt.handle(), &log, budget, backlog).expect("bridge");
        (dir, client, server, closer)
    }

    /// RDP sends small PDUs (input acknowledgements, frame acks, audio
    /// confirms) that Nagle would hold back behind each other.
    #[test]
    fn an_accepted_tcp_connection_sends_without_delay() {
        let listener = Listener::bind(&Listen::Tcp(
            "127.0.0.1:0".parse().expect("an address"),
        ))
        .expect("bind");
        let Listener::Tcp(tcp) = &listener else {
            panic!("a TCP listener");
        };
        let _client =
            TcpStream::connect(tcp.local_addr().expect("its address"))
                .expect("connect");
        let conn = loop {
            if let Some(conn) = listener.try_accept().expect("accept") {
                break conn;
            }
        };
        let Conn::Tcp(stream) = conn else {
            panic!("a TCP connection");
        };
        assert!(stream.nodelay().expect("read TCP_NODELAY"));
    }

    #[test]
    fn a_client_that_overspends_before_login_is_cut_off() {
        let rt = runtime();
        let dir = tempfile::tempdir().expect("a temp dir");
        let listener =
            Listener::bind(&Listen::Unix(dir.path().join("s"))).expect("bind");
        let mut client =
            UnixStream::connect(dir.path().join("s")).expect("connect");
        let conn = loop {
            if let Some(conn) = listener.try_accept().expect("accept") {
                break conn;
            }
        };
        let log = slog::Logger::root(slog::Discard, slog::o!());
        let budget = Arc::new(InboundBudget::new(1024));
        let backlog = Arc::new(OutboundBacklog::new());
        let (mut server, _closer) =
            bridge(conn, rt.handle(), &log, budget, backlog).expect("bridge");

        client.write_all(&[7u8; 512]).expect("within budget");
        let mut got = [0u8; 512];
        rt.block_on(server.read_exact(&mut got))
            .expect("bytes within budget arrive");
        // Overspend by plenty; the bridge may cut the client off mid-write.
        if let Err(e) = client.write_all(&[7u8; 8192]) {
            assert!(
                matches!(
                    e.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                ),
                "{e}"
            );
        }
        let mut rest = Vec::new();
        let read = rt.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(5),
                server.read_to_end(&mut rest),
            )
            .await
        });
        assert!(
            matches!(read, Ok(Ok(_))),
            "the stream did not end: {read:?}"
        );
        assert!(rest.len() <= 512, "{} bytes past the budget", rest.len());
    }

    #[test]
    fn a_lifted_budget_is_never_spent() {
        let budget = InboundBudget::new(10);
        assert!(budget.spend(10));
        assert!(!budget.spend(1));
        budget.lift();
        assert!(budget.lifted());
        assert!(budget.spend(usize::MAX));
        assert!(budget.lifted());
    }

    /// Write until the outbound thread is blocked on a client that is not
    /// reading.
    fn fill(rt: &tokio::runtime::Runtime, server: &mut DuplexStream) {
        let chunk = vec![0u8; BUF];
        // Far more than the duplex and a socket buffer hold.
        for _ in 0..(64 << 20) / BUF {
            let wrote = rt.block_on(async {
                tokio::time::timeout(
                    Duration::from_millis(500),
                    server.write_all(&chunk),
                )
                .await
            });
            match wrote {
                Ok(written) => written.expect("server write"),
                Err(_) => return,
            }
        }
        panic!("the writes never blocked on a full socket");
    }

    #[test]
    fn a_write_to_a_client_that_stopped_reading_shows_as_stalled() {
        let rt = runtime();
        let (_dir, _client, mut server, closer) = connected(&rt);
        assert_eq!(closer.stalled_for(), None);
        fill(&rt, &mut server);
        assert!(closer.stalled_for().is_some(), "a write is waiting");
        // The stall grows while the client does not read.
        wait_for(|| {
            closer
                .stalled_for()
                .is_some_and(|stalled| stalled >= Duration::from_millis(500))
        });
    }

    /// A client that stops reading must not keep the socket open once
    /// the server is done with it.
    #[test]
    fn dropping_the_closer_frees_a_client_that_stopped_reading() {
        let rt = runtime();
        let (_dir, mut client, mut server, closer) = connected(&rt);
        fill(&rt, &mut server);
        drop(server);
        drop(closer);

        client.set_nonblocking(true).expect("nonblocking");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match client.write(b"x") {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => break,
                Ok(_) => {}
            }
            assert!(
                Instant::now() < deadline,
                "the socket stayed open for a client that stopped reading"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// After a client floods input and leaves, the server's writes fail
    /// rather than wait forever on a reader that is gone.
    #[test]
    fn a_client_that_floods_input_and_leaves_fails_the_server_writes() {
        let rt = runtime();
        let (_dir, mut client, mut server, _closer) = connected(&rt);
        client.set_nonblocking(true).expect("nonblocking");
        // Fill the duplex and the socket, so the inbound thread is blocked
        // writing to a server that is not reading.
        let flood = vec![0u8; BUF];
        let start = Instant::now();
        let (mut sent, mut blocked_since) = (0usize, None);
        loop {
            match client.write(&flood) {
                Ok(n) => {
                    sent += n;
                    blocked_since = None;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let since = *blocked_since.get_or_insert_with(Instant::now);
                    if sent > BUF
                        && since.elapsed() > Duration::from_millis(200)
                    {
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("client write: {e}"),
            }
            assert!(start.elapsed() < Duration::from_secs(5), "never filled");
        }
        drop(client);

        let failed = rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if server.write_all(&[0u8; 4096]).await.is_err() {
                        return;
                    }
                }
            })
            .await
        });
        assert!(
            failed.is_ok(),
            "the server's writes hung after the client left"
        );
    }

    #[test]
    fn bytes_cross_the_bridge_both_ways_and_close_follows() {
        let rt = runtime();
        let (_dir, mut client, mut server, _closer) = connected(&rt);

        client.write_all(b"hello").expect("client write");
        let mut got = [0; 5];
        rt.block_on(server.read_exact(&mut got))
            .expect("server read");
        assert_eq!(&got, b"hello");

        rt.block_on(server.write_all(b"world"))
            .expect("server write");
        client.read_exact(&mut got).expect("client read");
        assert_eq!(&got, b"world");

        drop(server);
        let mut rest = Vec::new();
        client
            .read_to_end(&mut rest)
            .expect("the bridge closes the socket");
        assert!(rest.is_empty());
    }

    /// What reaches the client comes off the backlog, which never goes
    /// below nothing: the backlog counts frames and audio, and the
    /// bridge carries every PDU.
    #[test]
    fn bytes_written_to_the_client_come_off_the_backlog() {
        let rt = runtime();
        let backlog = Arc::new(OutboundBacklog::new());
        let (_dir, mut client, mut server, _closer) =
            connected_with(&rt, Arc::clone(&backlog));
        backlog.queued(10);

        rt.block_on(server.write_all(b"world"))
            .expect("server write");
        let mut got = [0; 5];
        client.read_exact(&mut got).expect("client read");
        wait_for(|| backlog.bytes() == 5);

        rt.block_on(server.write_all(b"0123456789"))
            .expect("server write");
        let mut got = [0; 10];
        client.read_exact(&mut got).expect("client read");
        wait_for(|| backlog.bytes() == 0);
    }

    /// Wait up to 5 s for `done`.
    fn wait_for(done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn no_connection_is_not_an_error() {
        let addr = Listen::Tcp(std::net::SocketAddr::from(([127, 0, 0, 1], 0)));
        let listener = Listener::bind(&addr).expect("bind");
        assert!(listener.try_accept().expect("accept").is_none());
    }
}
