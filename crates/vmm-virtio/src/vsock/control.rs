// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The CONTROL verb on the vsock host socket.
//!
//! A host peer that sends `CONNECT <port>` is joined to a guest port. A
//! peer that sends `CONTROL` is answered by the VMM itself and reaches
//! no guest. firehyve has no control socket, so this is how an operator
//! reads the machine.
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
//! `OK <count>` and one field per record; a record is its values
//! joined by commas. No value carries a space or a comma. A response
//! past [`RESPONSE_LINE_MAX`] is one refusal, never a cut list.
//!
//! Who may speak is decided before this module runs: the socket is
//! 0600 in a 0700 directory and the accept loop applies the process
//! peer policy (`vmm_core::unixsock::peercred`). There is no token.
//! The only peer that can reach a path under the zone root is
//! global-zone root, which the policy admits. A request is still
//! hostile input: the line is bounded, nothing parsed from it indexes
//! anything, and a malformed verb changes no state.

use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slog::warn;

use vmm_core::unixsock::write_all_bounded;
use vmm_devices::pci::msix::MsixTable;
use vmm_devices::pci::LintrCfg;

use super::device::{
    VirtioVsock, VSOCK_MSIX_VECTORS, VSOCK_NUM_QUEUES, VSOCK_QUEUE_SIZE,
};
use crate::attach::VirtioAttachCtx;
use crate::bits;
use crate::pci::VirtioPciDevice;

/// Longest request accepted, for the same reason the CONNECT line is
/// capped: a peer that never sends a newline must not make the device
/// read without bound. A request is one verb and at most one number.
const CONTROL_LINE_MAX: usize = 128;

/// Longest response line written back, newline excluded.
///
/// A `cpu-list` on a 256-slot machine is about 5 KiB. An answer above
/// this is a sink bug or a hostile id count, and either way the peer
/// gets one refusal rather than a line with no end.
pub const RESPONSE_LINE_MAX: usize = 16 * 1024;

/// How long a blocked read waits before it looks at the shutdown flag.
/// Without this, halt would wait for the peer instead of the guest.
pub(super) const LINE_POLL: Duration = Duration::from_millis(250);

/// How long a response line has to reach the peer.
///
/// This thread is one of the capped reader slots. `SO_SNDTIMEO` bounds
/// nothing on illumos, so before this the answer to a peer that never
/// read one held that slot for the life of the VM. Enough such peers
/// and no further connection can be made, this session included.
const RESPONSE_BUDGET: Duration = Duration::from_millis(500);

/// How long a peer has to send its opening verb.
///
/// Reader threads are capped, so a peer that connects and says nothing
/// holds a slot the shutdown flag is the only thing that frees. Enough
/// of them and no further connection can be made, the CONTROL session
/// included, for the life of the VM. A peer with nothing to say in
/// this long has nothing to say.
pub(super) const HANDSHAKE_BUDGET: Duration = Duration::from_secs(2);

/// How long an established session may say nothing before it loses its
/// place.
///
/// The session thread is one of the capped reader slots, so a peer that
/// opens CONTROL and then idles holds a slot every later CONNECT needs.
/// Minutes, not seconds: an operator tool may hold a session open
/// between commands, and losing one costs only a reconnect.
pub(super) const CONTROL_IDLE_BUDGET: Duration = Duration::from_secs(300);

/// How many CONTROL sessions may run at once.
///
/// The idle budget alone does not stop starvation: a peer that sends one
/// byte inside every budget keeps its slot for ever. A quota of its own
/// does, and it is small because one operator tool is the expected
/// caller.
pub(super) const MAX_CONTROL_SESSIONS: usize = 4;

/// Longest value written back for one field. An id comes from the `-s`
/// arguments, so an operator can make it long; the response line stays
/// readable either way.
pub const FIELD_MAX: usize = 64;

/// Longest refusal written back. A refusal can quote a value the peer
/// sent, so the line stays bounded.
pub const REASON_MAX: usize = 200;

/// What an empty value renders as, so every record keeps its shape.
const NO_VALUE: &str = "-";

/// One request, as the grammar names it.
///
/// The arguments a verb takes are parsed here, so a sink never sees
/// text it has to split, and a typo is answered as a typo whatever the
/// machine behind the sink can do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlRequest {
    DeviceList,
    CpuList,
    MemList,
    CpuAdd(u32),
    /// Give the guest this many more bytes.
    MemAdd(u64),
    /// Named so the answer can say why the platform cannot, rather
    /// than reading as a typo the operator can fix.
    CpuRemove,
    MemRemove,
    /// The machine as a list of `key,value` records.
    Status,
    /// Counters as `key,value` records.
    Metrics,
    /// Stop the VM now. There is no ACPI button in a microVM; a
    /// graceful stop is the guest agent's job over vsock.
    Halt,
    /// Named so the answer can say that, rather than read as a typo.
    Shutdown,
}

/// Why `device-add` and `device-remove` answer `ERR`.
pub const NO_DEVICE_ADD: &str = "device-add is not supported: the device \
    set is fixed at boot, and after the privilege lock no host file can \
    be opened";
pub const NO_DEVICE_REMOVE: &str = "device-remove is not supported: the \
    device set is fixed at boot";

impl ControlRequest {
    /// Parse one request line. The error is the reason the line is
    /// refused, ready for the wire.
    pub fn parse(line: &str) -> Result<Self, String> {
        let mut fields = line.split_whitespace();
        // Read all three up front: a match guard cannot advance the
        // iterator. A `-s` spec carries no space, so one field is a
        // whole spec and a second means the request is malformed.
        let verb = fields.next().unwrap_or("");
        let argument = fields.next();
        let extra = fields.next();

        match (verb, argument, extra) {
            ("device-list", None, _) => Ok(Self::DeviceList),
            ("cpu-list", None, _) => Ok(Self::CpuList),
            ("mem-list", None, _) => Ok(Self::MemList),
            ("status", None, _) => Ok(Self::Status),
            ("metrics", None, _) => Ok(Self::Metrics),
            ("halt", None, _) => Ok(Self::Halt),
            ("shutdown", None, _) => Ok(Self::Shutdown),
            // Refused at the grammar, so no binary can wire them back
            // without changing the contract every client reads.
            ("device-add", _, _) => Err(NO_DEVICE_ADD.to_string()),
            ("device-remove", _, _) => Err(NO_DEVICE_REMOVE.to_string()),
            // The id is never used as an index: the engine range-checks
            // it as well.
            ("cpu-add", Some(id), None) => id
                .parse()
                .map(Self::CpuAdd)
                .map_err(|_| format!("{} is not a CPU id", value(id))),
            ("mem-add", Some(bytes), None) => {
                bytes.parse().map(Self::MemAdd).map_err(|_| {
                    format!("{} is not a size in bytes", value(bytes))
                })
            }
            ("cpu-remove", _, _) => Ok(Self::CpuRemove),
            ("mem-remove", _, _) => Ok(Self::MemRemove),
            ("", _, _) => Err("empty request".to_string()),
            (
                "device-list" | "cpu-list" | "mem-list" | "status" | "metrics"
                | "halt" | "shutdown",
                Some(_),
                _,
            ) => Err(format!("{} takes no argument", value(verb))),
            ("cpu-add" | "mem-add", None, _) => {
                Err(format!("{} needs one argument", value(verb)))
            }
            ("cpu-add" | "mem-add", Some(_), Some(_)) => {
                Err(format!("{} takes one argument", value(verb)))
            }
            _ => Err(format!("unknown command {}", value(verb))),
        }
    }
}

/// One answer. A record is its values joined by commas, and the reply
/// is `OK` and the records joined by spaces, or `ERR` and a reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlReply {
    Ok(Vec<Vec<String>>),
    Refused(String),
}

impl ControlReply {
    /// `OK` and one record.
    pub fn record(values: impl IntoIterator<Item = String>) -> Self {
        Self::Ok(vec![values.into_iter().collect()])
    }

    /// `OK <count>` and then one record each.
    pub fn list(records: Vec<Vec<String>>) -> Self {
        let mut out = Vec::with_capacity(records.len() + 1);
        out.push(vec![records.len().to_string()]);
        out.extend(records);
        Self::Ok(out)
    }

    pub fn refused(reason: impl Into<String>) -> Self {
        Self::Refused(reason.into())
    }

    /// The response line, with every value and the reason bounded and
    /// stripped down to what a terminal can show.
    ///
    /// A line past [`RESPONSE_LINE_MAX`] is replaced, not cut: a cut
    /// list would read as a shorter, valid list.
    pub fn render(&self) -> String {
        let line = match self {
            Self::Ok(records) => {
                let mut out = String::from("OK");
                for record in records {
                    out.push(' ');
                    let record: Vec<String> =
                        record.iter().map(|v| value(v)).collect();
                    out.push_str(&record.join(","));
                    if out.len() > RESPONSE_LINE_MAX {
                        break;
                    }
                }
                out
            }
            Self::Refused(reason) => format!("ERR {}", clean_reason(reason)),
        };
        if line.len() > RESPONSE_LINE_MAX {
            format!("ERR response exceeds {RESPONSE_LINE_MAX} bytes")
        } else {
            line
        }
    }
}

/// One value, safe to put on the wire.
///
/// The grammar separates records by spaces and values by commas, and an
/// operator reads the answer on a terminal. So a value that came from
/// the command line keeps only plain characters, and the length is
/// capped.
fn value(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .take(FIELD_MAX)
        .map(|c| {
            if c.is_ascii_alphanumeric()
                || matches!(c, '-' | '_' | '.' | '@' | ':')
            {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        NO_VALUE.to_string()
    } else {
        cleaned
    }
}

/// One refusal, safe to put on the wire.
///
/// A refusal can quote a value the peer sent, so the text is capped and
/// stripped down to printable ASCII. Spaces stay: the grammar makes a
/// refusal one reason, not a record.
fn clean_reason(reason: &str) -> String {
    reason
        .chars()
        .take(REASON_MAX)
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Answers one control request.
///
/// Implemented by the VMM binary, which owns the device registry and the
/// CPU and memory configuration. The device knows the framing and
/// nothing else.
pub trait ControlSink: Send + Sync {
    fn handle(&self, request: ControlRequest) -> ControlReply;
}

/// One request line in, one response line out, with no newline.
pub fn answer_line(sink: &dyn ControlSink, line: &str) -> String {
    match ControlRequest::parse(line.trim()) {
        Ok(request) => sink.handle(request).render(),
        Err(reason) => ControlReply::refused(reason).render(),
    }
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
    pci_dev.device().install_interrupt(pci_dev.backend_intr());

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

/// Send one response line, and give up on a peer that will not take it.
///
/// The newline goes in the same write as the line. One response then
/// costs one budget rather than two, and a peer that stalls cannot
/// leave a line on the socket with no end to it.
///
/// [`write_all_bounded`] polls for space and then sends, so a second
/// writer on the same socket would break the bound. There is no second
/// writer here. [`run_control_session`] takes the socket by value, this
/// module never clones it, and the CONTROL verb keeps it out of the
/// device socket table, which is the only place another thread looks
/// one up to write it. The `&mut` is what a later change has to break
/// first.
fn write_line(stream: &mut UnixStream, line: &str) -> io::Result<()> {
    let mut out = Vec::with_capacity(line.len() + 1);
    out.extend_from_slice(line.as_bytes());
    out.push(b'\n');
    write_all_bounded(stream, &out, RESPONSE_BUDGET)
}

/// Serve control requests until the peer closes or the device halts.
pub(super) fn run_control_session(
    slot: &ControlSlot,
    shutdown: &AtomicBool,
    mut stream: UnixStream,
    log: &slog::Logger,
    idle_budget: Duration,
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
        // An established session may idle between requests, but not
        // without end: this thread holds one of the capped reader
        // slots, and a peer that never speaks again must give it back.
        match read_line_bounded(
            &mut stream,
            shutdown,
            CONTROL_LINE_MAX,
            Some(Instant::now() + idle_budget),
        ) {
            LineRead::Line(request) => {
                let answer = answer_line(sink.as_ref(), &request);
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
    use std::io::{BufRead, BufReader, Write};
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

    /// Names the request it was given, so a test sees the exact
    /// request the device passed on.
    struct EchoSink;

    impl ControlSink for EchoSink {
        fn handle(&self, request: ControlRequest) -> ControlReply {
            match request {
                ControlRequest::DeviceList => {
                    ControlReply::record(["device-list".to_string()])
                }
                other => ControlReply::refused(format!("{other:?}")),
            }
        }
    }

    /// Answers with a value holding a newline, which the rendering
    /// must not let out as a second line.
    struct TwoLineSink;

    impl ControlSink for TwoLineSink {
        fn handle(&self, _request: ControlRequest) -> ControlReply {
            ControlReply::record(["first\nsmuggled".to_string()])
        }
    }

    /// Answers with a line at the response cap, so a few pipelined
    /// requests fill any socket buffer.
    struct BulkSink;

    impl ControlSink for BulkSink {
        fn handle(&self, _request: ControlRequest) -> ControlReply {
            ControlReply::list(vec![
                vec!["x".repeat(FIELD_MAX)];
                RESPONSE_LINE_MAX / (FIELD_MAX + 1) - 2
            ])
        }
    }

    fn null_log() -> slog::Logger {
        slog::Logger::root(slog::Discard, slog::o!())
    }

    /// Start a session over one end of a socket pair and give the test
    /// the other end plus the thread to join. The peer has not spoken.
    fn session_with_idle(
        slot: &ControlSlot,
        shutdown: Arc<AtomicBool>,
        idle: Duration,
    ) -> (UnixStream, thread::JoinHandle<()>) {
        let (client, server) = UnixStream::pair().expect("socketpair");
        let slot = slot.clone();
        let handle = thread::spawn(move || {
            run_control_session(&slot, &shutdown, server, &null_log(), idle);
        });
        (client, handle)
    }

    fn session_raw(
        slot: &ControlSlot,
        shutdown: Arc<AtomicBool>,
    ) -> (UnixStream, thread::JoinHandle<()>) {
        session_with_idle(slot, shutdown, CONTROL_IDLE_BUDGET)
    }

    /// One line, read a byte at a time so nothing pipelined behind it
    /// is taken off the socket.
    fn read_line_raw(stream: &mut UnixStream) -> String {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            match stream.read(&mut byte) {
                Ok(1) if byte[0] == b'\n' => break,
                Ok(1) => line.push(byte[0]),
                other => panic!("the line ended early: {other:?}"),
            }
        }
        String::from_utf8(line).expect("text")
    }

    /// A session past the banner.
    fn session(
        slot: &ControlSlot,
        shutdown: Arc<AtomicBool>,
    ) -> (UnixStream, thread::JoinHandle<()>) {
        let (mut client, handle) = session_raw(slot, shutdown);
        assert_eq!(read_line_raw(&mut client), "OK control");
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
        let (client, handle) =
            session_raw(&slot, Arc::new(AtomicBool::new(false)));

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).expect("one line");

        assert_eq!(line, "ERR control interface not available\n");
        // Refused means closed: no second line follows.
        line.clear();
        assert_eq!(reader.read_line(&mut line).expect("eof"), 0);
        handle.join().expect("session thread");
    }

    /// The first request is served straight after the banner: the peer
    /// gate ran in the accept loop, and there is no second proof.
    #[test]
    fn the_first_request_after_the_banner_is_served() {
        let slot = installed(Arc::new(EchoSink));
        let (mut client, handle) =
            session_raw(&slot, Arc::new(AtomicBool::new(false)));

        assert_eq!(read_line_raw(&mut client), "OK control");
        write_line(&mut client, "device-list").expect("request");
        assert_eq!(read_line_raw(&mut client), "OK device-list");
        // `auth` is not a verb any more, so it is a typo like any other.
        write_line(&mut client, "auth whatever").expect("request");
        assert_eq!(read_line_raw(&mut client), "ERR unknown command auth");
        drop(client);
        handle.join().expect("session thread");
    }

    #[test]
    fn a_request_round_trips_through_the_sink() {
        let slot = installed(Arc::new(EchoSink));
        let (mut client, handle) =
            session(&slot, Arc::new(AtomicBool::new(false)));

        write_line(&mut client, "device-list").expect("request");
        let mut reader = BufReader::new(client.try_clone().expect("clone"));
        let mut answer = String::new();
        reader.read_line(&mut answer).expect("answer");

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
        reader.read_line(&mut line).expect("answer");

        assert_eq!(line, "OK first_smuggled\n");
        drop(client);
        drop(reader);
        handle.join().expect("session thread");
    }

    /// A peer that asks and never listens must not keep the session.
    ///
    /// The session thread is one of the capped reader slots, so a write
    /// with no end here closes the socket to every other client for the
    /// life of the VM.
    #[test]
    fn a_peer_that_never_reads_its_answer_loses_the_session() {
        let slot = installed(Arc::new(BulkSink));
        let (mut client, handle) =
            session(&slot, Arc::new(AtomicBool::new(false)));

        // Nothing here reads the socket after the auth: the answers
        // have to fill it. Each is near the response cap, so a few
        // hundred exceed any socket buffer.
        client
            .write_all(&b"device-list\n".repeat(256))
            .expect("request");

        // The session is the thing under test, so it cannot be joined
        // here. One that never ended would park the test run.
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(handle.join().is_ok());
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(true),
            "the session must end on its write budget, not on the peer"
        );

        // The peer stays open until here. One that closed would end
        // the write on a broken pipe, which is not the case under test.
        drop(client);
    }

    #[test]
    fn halt_ends_an_idle_session() {
        // The reader wakes on its own to look at the flag, so a session
        // an operator left open does not hold up teardown.
        let slot = installed(Arc::new(EchoSink));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (client, handle) = session(&slot, Arc::clone(&shutdown));

        shutdown.store(true, Ordering::Release);

        handle.join().expect("session thread ends on the flag");
        drop(client);
    }

    /// An established session used to read with no deadline at all, so
    /// a peer that opened CONTROL and then said nothing held one of the
    /// capped reader slots until the VM shut down. Enough such peers and
    /// every later CONNECT was refused in silence.
    #[test]
    fn an_idle_session_loses_its_slot() {
        let slot = installed(Arc::new(EchoSink));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (mut client, handle) = session_with_idle(
            &slot,
            Arc::clone(&shutdown),
            Duration::from_millis(50),
        );
        assert_eq!(read_line_raw(&mut client), "OK control");

        // Nothing is sent from here, and the shutdown flag stays clear:
        // only the idle budget can end this.
        let mut reader = BufReader::new(client);
        assert_session_closed(&mut reader);
        handle
            .join()
            .expect("session thread ends on its idle budget");
        assert!(!shutdown.load(Ordering::Acquire));
    }

    /// A peer that sends nothing must not park the thread.
    ///
    /// The reader owns its own poll timeout, so a caller that forgets to
    /// arm one still gets a bounded read. A connection that is not in
    /// the socket table yet is out of reach of the halt sweep, so
    /// nothing else would wake it.
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

    /// A list past the cap is one refusal, never a shorter list that
    /// a client would read as complete.
    #[test]
    fn an_answer_past_the_response_cap_is_refused_whole() {
        let big = ControlReply::list(vec![vec!["x".repeat(FIELD_MAX)]; 20_000]);
        let line = big.render();
        assert_eq!(
            line,
            format!("ERR response exceeds {RESPONSE_LINE_MAX} bytes")
        );
        assert!(line.len() <= RESPONSE_LINE_MAX);

        let fits = ControlReply::list(vec![vec!["x".repeat(FIELD_MAX)]; 100]);
        assert!(fits.render().starts_with("OK 100 "));
    }

    #[test]
    fn device_add_and_remove_are_refused_at_the_grammar() {
        for line in [
            "device-add 5,virtio-blk,/etc/shadow",
            "device-add",
            "device-remove virtio-blk@5",
        ] {
            let err = ControlRequest::parse(line).expect_err(line);
            assert!(err.contains("not supported"), "{line}: {err}");
            assert!(err.contains("fixed at boot"), "{line}: {err}");
        }
    }

    #[test]
    fn the_read_only_verbs_parse_and_take_no_argument() {
        assert_eq!(ControlRequest::parse("status"), Ok(ControlRequest::Status));
        assert_eq!(
            ControlRequest::parse("metrics"),
            Ok(ControlRequest::Metrics)
        );
        assert_eq!(ControlRequest::parse("halt"), Ok(ControlRequest::Halt));
        assert_eq!(
            ControlRequest::parse("shutdown"),
            Ok(ControlRequest::Shutdown)
        );
        for verb in ["status", "metrics", "halt", "shutdown"] {
            let err = ControlRequest::parse(&format!("{verb} now"))
                .expect_err("an argument is refused");
            assert_eq!(err, format!("{verb} takes no argument"));
        }
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
