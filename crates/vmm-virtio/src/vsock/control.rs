// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The CONTROL verb on the vsock host socket.
//!
//! A host peer that sends `CONNECT <port>` is joined to a guest port. A
//! peer that sends `CONTROL` is answered by the VMM itself, and nothing
//! it sends ever reaches the guest mux. firehyve has no control socket
//! of its own, so this is how an operator reads the machine.
//!
//! # Grammar
//!
//! ```text
//! open     = "CONTROL" LF
//! banner   = "OK control" LF
//! request  = verb *( SP argument ) LF
//! response = ( "OK" *( SP field ) | "ERR" SP reason ) LF
//! field    = value *( "," value )
//! ```
//!
//! One request per line, one response line back. A list answer is
//! `OK <count>` followed by one field per record, and a record is its
//! values joined by commas, so a client splits on whitespace and then on
//! commas. No value carries a space or a comma.
//!
//! ```text
//! device-list -> OK 2 virtio-blk@4,0.4.0,present varstore,-,present
//! cpu-list    -> OK 2 0,present 1,present
//! mem-list    -> OK 1 boot,1073741824,present
//! ```
//!
//! Wave 2 adds `device-add <spec>` and `device-remove <id>` at the same
//! shape: `OK <id>` on success, `ERR <reason>` on refusal.
//!
//! # Trust
//!
//! The socket is 0600 in a 0700 directory, so the peer is already the
//! VMM's own user. It is still parsed as hostile input: the request line
//! is bounded, nothing parsed from it indexes anything, and an
//! unknown or malformed verb closes the connection with one error line
//! and changes no state.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slog::warn;

use vmm_devices::pci::msix::MsixTable;
use vmm_devices::pci::LintrCfg;

use super::device::{
    VirtioVsock, VSOCK_MSIX_VECTORS, VSOCK_NUM_QUEUES, VSOCK_QUEUE_SIZE,
    VSOCK_RX_QUEUE,
};
use crate::attach::VirtioAttachCtx;
use crate::bits;
use crate::pci::VirtioPciDevice;

/// Longest request accepted, for the same reason the CONNECT line is
/// capped: a peer that never sends a newline must not make the device
/// read without bound. Wider than the CONNECT line because Wave 2's
/// `device-add` argument is a whole `-s` spec.
const CONTROL_LINE_MAX: usize = 512;

/// How long a blocked read waits before it looks at the shutdown flag.
/// Without this, halt would wait for the peer instead of the guest.
pub(super) const LINE_POLL: Duration = Duration::from_millis(250);

/// How long a peer has to send its opening verb.
///
/// Reader threads are capped, so a peer that connects and says nothing
/// holds a slot the shutdown flag is the only thing that frees. Enough
/// of them and no further connection can be made, the CONTROL session
/// included, for the life of the VM. A peer with nothing to say in
/// this long has nothing to say.
pub(super) const HANDSHAKE_BUDGET: Duration = Duration::from_secs(2);

/// Answers one control request.
///
/// Implemented by the VMM binary, which owns the device registry and the
/// CPU and memory configuration. The device knows the framing and
/// nothing else.
pub trait ControlSink: Send + Sync {
    /// Answer one request line. The return value is one response line,
    /// with no trailing newline.
    fn handle_line(&self, line: &str) -> String;
}

/// The device's sink, filled in after the device exists.
///
/// The sink reads the device registry, and the registry is not complete
/// until every device is attached, so the binary installs it once the
/// attach pass returns. Until then a CONTROL request is refused.
#[derive(Clone, Default)]
pub struct ControlSlot {
    sink: Arc<Mutex<Option<Arc<dyn ControlSink>>>>,
}

impl ControlSlot {
    /// Install the sink. Called once, at startup.
    pub fn install(&self, sink: Arc<dyn ControlSink>) {
        // No panic on a host-facing path, for the same reason [`Self::sink`]
        // takes the poison tolerantly. The slot holds one `Option`, which a
        // panic elsewhere cannot leave half written, so the install goes
        // through the poison rather than dropping the sink in silence.
        let mut slot = self.sink.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some(sink);
    }

    /// The installed sink, or `None` while the slot is empty.
    pub fn sink(&self) -> Option<Arc<dyn ControlSink>> {
        // No panic on a host-facing path: a poisoned lock reads as an
        // empty slot, which the caller refuses cleanly.
        self.sink.lock().ok().and_then(|guard| guard.clone())
    }
}

/// Build a virtio-vsock device and its PCI transport, and hand back the
/// control slot.
///
/// The same wiring as [`crate::attach::virtio_vsock`], plus the slot.
/// The slot lives inside the device and the device moves into the
/// transport, so a caller that needs the slot has to take it here.
pub fn attach_with_control(
    socket_path: &Path,
    guest_cid: u64,
    lintr: Option<LintrCfg>,
    ctx: &VirtioAttachCtx<'_>,
    log: &slog::Logger,
) -> anyhow::Result<(Arc<VirtioPciDevice<VirtioVsock>>, ControlSlot)> {
    let vsock = VirtioVsock::new(
        socket_path,
        guest_cid,
        Arc::clone(ctx.physmap),
        log.clone(),
    )?;
    let cfg_size = vsock.config_size();
    let vsock_intr = vsock.interrupt();
    let control = vsock.control_slot();

    let msix = Arc::new(MsixTable::new(VSOCK_MSIX_VECTORS, ctx.msi_sink()));

    let pci_dev = VirtioPciDevice::new(
        vsock,
        bits::VIRTIO_DEV_TYPE_VSOCK,
        VSOCK_NUM_QUEUES,
        VSOCK_QUEUE_SIZE,
        cfg_size,
        lintr,
        Arc::clone(ctx.physmap),
        Arc::clone(ctx.bus_pio),
        Arc::clone(ctx.bus_mmio),
        Some(msix),
    );

    // Only the receive queue completes out of band; the transport
    // raises for the transmit queue on its own.
    let pci_weak = Arc::downgrade(&pci_dev);
    *vsock_intr.lock().expect("virtio-vsock interrupt lock") =
        Some(Arc::new(move || {
            if let Some(dev) = pci_weak.upgrade() {
                dev.raise_queue_interrupt_for(VSOCK_RX_QUEUE);
            }
        }));

    Ok((pci_dev, control))
}

/// What one read of a peer's line produced.
pub(super) enum LineRead {
    Line(String),
    TooLong,
    NotUtf8,
    Closed,
}

/// Read one line from a host peer, bounded in length and in time.
///
/// The reader arms [`LINE_POLL`] on the stream itself rather than
/// trusting the caller to do it. A peer that opens a connection and
/// sends nothing would otherwise park the thread for the life of the
/// VM, and a connection the device has not registered yet is out of
/// reach of the halt sweep, so nothing would ever wake it.
///
/// `deadline` bounds the read itself, which the poll does not: without
/// one, a silent peer holds the thread until the VM shuts down. Pass
/// it for a handshake, and `None` for an established session that may
/// idle between requests.
///
/// Read one byte at a time, and deliberately not through a `BufReader`:
/// that reads a whole block, and anything the peer pipelined behind the
/// newline would sit in a buffer this function is about to drop.
pub(super) fn read_line_bounded(
    stream: &mut UnixStream,
    shutdown: &AtomicBool,
    max: usize,
    deadline: Option<Instant>,
) -> LineRead {
    if stream.set_read_timeout(Some(LINE_POLL)).is_err() {
        return LineRead::Closed;
    }
    let mut line: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if shutdown.load(Ordering::Acquire) {
            return LineRead::Closed;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return LineRead::Closed;
        }
        match stream.read(&mut byte) {
            Ok(0) => return LineRead::Closed,
            Ok(_) => {
                if byte[0] == b'\n' {
                    return match String::from_utf8(line) {
                        Ok(text) => LineRead::Line(text),
                        Err(_) => LineRead::NotUtf8,
                    };
                }
                if line.len() >= max {
                    return LineRead::TooLong;
                }
                line.push(byte[0]);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(_) => return LineRead::Closed,
        }
    }
}

/// One line of a sink's answer.
///
/// A sink that returned an embedded newline would put two lines on the
/// wire for one request and desynchronise every later exchange, so only
/// the first line goes out.
fn framed(answer: &str) -> &str {
    answer.split(['\r', '\n']).next().unwrap_or("")
}

fn write_line(stream: &mut UnixStream, line: &str) -> io::Result<()> {
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

/// Serve control requests until the peer closes or the device halts.
pub(super) fn run_control_session(
    slot: &ControlSlot,
    shutdown: &AtomicBool,
    mut stream: UnixStream,
    log: &slog::Logger,
) {
    // Each refusal below writes one line and closes. The write is best
    // effort: the peer may already be gone, and there is nothing else to
    // report to.
    let Some(sink) = slot.sink() else {
        warn!(log, "vsock CONTROL refused; no control sink installed");
        let _ = write_line(&mut stream, "ERR control interface not available");
        return;
    };
    if write_line(&mut stream, "OK control").is_err() {
        return;
    }

    loop {
        // No deadline: an established session may idle between
        // requests, and shutdown is what ends it.
        match read_line_bounded(&mut stream, shutdown, CONTROL_LINE_MAX, None) {
            LineRead::Line(request) => {
                let request = request.trim();
                let answer = if request.is_empty() {
                    "ERR empty request".to_string()
                } else {
                    sink.handle_line(request)
                };
                if write_line(&mut stream, framed(&answer)).is_err() {
                    return;
                }
            }
            // A truncated request cannot be resynchronised, so the
            // connection ends rather than acting on half a line.
            LineRead::TooLong => {
                let _ = write_line(&mut stream, "ERR request line too long");
                return;
            }
            LineRead::NotUtf8 => {
                let _ = write_line(&mut stream, "ERR request is not text");
                return;
            }
            LineRead::Closed => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::thread;

    use super::*;

    /// Assert the server ended the session.
    ///
    /// A close that leaves bytes unread in the receive queue makes the
    /// kernel send RST instead of FIN, so the peer reads
    /// ConnectionReset rather than a clean end of file. Linux does
    /// this; illumos and macOS report the clean end. The server will
    /// not drain a hostile peer's unbounded input before closing, so
    /// both answers mean the same thing.
    fn assert_session_closed(reader: &mut impl BufRead) {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            other => panic!("expected a closed session, got {other:?}"),
        }
    }

    /// Echoes what it was given, so a test sees the exact request the
    /// device passed on.
    struct EchoSink;

    impl ControlSink for EchoSink {
        fn handle_line(&self, line: &str) -> String {
            format!("OK {line}")
        }
    }

    /// Answers with two lines, which the framing must not let out.
    struct TwoLineSink;

    impl ControlSink for TwoLineSink {
        fn handle_line(&self, _line: &str) -> String {
            "OK first\nERR smuggled".to_string()
        }
    }

    fn null_log() -> slog::Logger {
        slog::Logger::root(slog::Discard, slog::o!())
    }

    /// Run a session against one end of a socket pair, and give the test
    /// the other end plus the thread to join.
    fn session(
        slot: &ControlSlot,
        shutdown: Arc<AtomicBool>,
    ) -> (UnixStream, thread::JoinHandle<()>) {
        let (client, server) = UnixStream::pair().expect("socketpair");
        let slot = slot.clone();
        let handle = thread::spawn(move || {
            run_control_session(&slot, &shutdown, server, &null_log());
        });
        (client, handle)
    }

    fn installed(sink: Arc<dyn ControlSink>) -> ControlSlot {
        let slot = ControlSlot::default();
        slot.install(sink);
        slot
    }

    #[test]
    fn control_with_no_sink_is_refused_and_closed() {
        let slot = ControlSlot::default();
        let (client, handle) = session(&slot, Arc::new(AtomicBool::new(false)));

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).expect("one line");

        assert_eq!(line, "ERR control interface not available\n");
        // Refused means closed: no second line follows.
        line.clear();
        assert_eq!(reader.read_line(&mut line).expect("eof"), 0);
        handle.join().expect("session thread");
    }

    #[test]
    fn a_request_round_trips_through_the_sink() {
        let slot = installed(Arc::new(EchoSink));
        let (mut client, handle) =
            session(&slot, Arc::new(AtomicBool::new(false)));

        write_line(&mut client, "device-list").expect("request");
        let mut reader = BufReader::new(client.try_clone().expect("clone"));
        let mut banner = String::new();
        reader.read_line(&mut banner).expect("banner");
        let mut answer = String::new();
        reader.read_line(&mut answer).expect("answer");

        assert_eq!(banner, "OK control\n");
        assert_eq!(answer, "OK device-list\n");
        drop(client);
        drop(reader);
        handle.join().expect("session thread");
    }

    #[test]
    fn an_empty_request_still_gets_one_answer() {
        // One response per request, so a bare newline is answered rather
        // than leaving the client waiting.
        let slot = installed(Arc::new(EchoSink));
        let (mut client, handle) =
            session(&slot, Arc::new(AtomicBool::new(false)));

        write_line(&mut client, "   ").expect("request");
        let mut reader = BufReader::new(client.try_clone().expect("clone"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("banner");
        line.clear();
        reader.read_line(&mut line).expect("answer");

        assert_eq!(line, "ERR empty request\n");
        drop(client);
        drop(reader);
        handle.join().expect("session thread");
    }

    #[test]
    fn an_over_long_request_is_refused_and_closed() {
        let slot = installed(Arc::new(EchoSink));
        let (mut client, handle) =
            session(&slot, Arc::new(AtomicBool::new(false)));

        // No newline anywhere in it: the cap is the only thing that ends
        // the read.
        client
            .write_all(&vec![b'x'; CONTROL_LINE_MAX * 2])
            .expect("request");
        let mut reader = BufReader::new(client.try_clone().expect("clone"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("banner");
        line.clear();
        reader.read_line(&mut line).expect("answer");

        assert_eq!(line, "ERR request line too long\n");
        // The capped read left the rest of the request unread, so this
        // close is the RST case assert_session_closed documents.
        assert_session_closed(&mut reader);
        handle.join().expect("session thread");
    }

    #[test]
    fn a_request_that_is_not_text_is_refused() {
        let slot = installed(Arc::new(EchoSink));
        let (mut client, handle) =
            session(&slot, Arc::new(AtomicBool::new(false)));

        client.write_all(&[0xff, 0xfe, b'\n']).expect("request");
        let mut reader = BufReader::new(client.try_clone().expect("clone"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("banner");
        line.clear();
        reader.read_line(&mut line).expect("answer");

        assert_eq!(line, "ERR request is not text\n");
        handle.join().expect("session thread");
    }

    #[test]
    fn a_two_line_answer_cannot_desynchronise_the_stream() {
        let slot = installed(Arc::new(TwoLineSink));
        let (mut client, handle) =
            session(&slot, Arc::new(AtomicBool::new(false)));

        write_line(&mut client, "device-list").expect("request");
        let mut reader = BufReader::new(client.try_clone().expect("clone"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("banner");
        line.clear();
        reader.read_line(&mut line).expect("answer");

        assert_eq!(line, "OK first\n");
        drop(client);
        drop(reader);
        handle.join().expect("session thread");
    }

    #[test]
    fn halt_ends_an_idle_session() {
        // The reader wakes on its own to look at the flag, so a session
        // an operator left open does not hold up teardown.
        let slot = installed(Arc::new(EchoSink));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (client, handle) = session(&slot, Arc::clone(&shutdown));

        let mut reader = BufReader::new(client);
        let mut banner = String::new();
        reader.read_line(&mut banner).expect("banner");
        shutdown.store(true, Ordering::Release);

        handle.join().expect("session thread ends on the flag");
    }

    /// A peer that sends nothing must not park the thread.
    ///
    /// The reader owns its own poll timeout, so a caller that forgets to
    /// arm one still gets a bounded read. The CONNECT path in
    /// `device.rs` reads its opening line with no timeout at all, and a
    /// connection that is not in the socket table yet is out of reach of
    /// the halt sweep.
    #[test]
    fn a_silent_peer_does_not_park_the_reader() {
        let (_client, mut server) = UnixStream::pair().expect("socketpair");
        let shutdown = Arc::new(AtomicBool::new(false));

        let flag = Arc::clone(&shutdown);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            flag.store(true, Ordering::Release);
        });

        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let got = read_line_bounded(
                &mut server,
                &shutdown,
                CONTROL_LINE_MAX,
                None,
            );
            let _ = tx.send(matches!(got, LineRead::Closed));
        });

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(true),
            "the read must end on the flag, not on the peer"
        );
    }

    /// The CONNECT path caps its opening line far shorter than a
    /// control request, so the bound has to be the caller's to pick.
    #[test]
    fn the_line_cap_is_the_callers_to_pick() {
        let (mut client, mut server) = UnixStream::pair().expect("socketpair");
        let shutdown = Arc::new(AtomicBool::new(false));
        client.write_all(b"12345678\n").expect("write");

        assert!(matches!(
            read_line_bounded(&mut server, &shutdown, 4, None),
            LineRead::TooLong
        ));
    }

    #[test]
    fn only_the_first_line_of_an_answer_is_framed() {
        assert_eq!(framed("OK 1 a,b,c"), "OK 1 a,b,c");
        assert_eq!(framed("OK first\nERR smuggled"), "OK first");
        assert_eq!(framed("OK first\r\nERR smuggled"), "OK first");
        assert_eq!(framed("\nERR smuggled"), "");
    }

    #[test]
    fn a_poisoned_slot_still_takes_the_sink() {
        let slot = ControlSlot::default();
        let poisoner = slot.clone();
        thread::spawn(move || {
            let _guard = poisoner.sink.lock().expect("lock");
            panic!("poison the slot");
        })
        .join()
        .expect_err("the worker panics on purpose");

        slot.install(Arc::new(EchoSink));

        // A poisoned lock must not kill the VMM, and must not swallow
        // the sink either.
        let held = slot.sink.lock().unwrap_or_else(|e| e.into_inner());
        assert!(held.is_some());
    }

    #[test]
    fn an_empty_slot_hands_back_no_sink() {
        let slot = ControlSlot::default();
        assert!(slot.sink().is_none());
        slot.install(Arc::new(EchoSink));
        assert!(slot.sink().is_some());
        // The clone shares the slot: the binary installs into a clone.
        assert!(slot.clone().sink().is_some());
    }
}
