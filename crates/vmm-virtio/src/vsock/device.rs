// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The virtio-vsock device and its host sockets.
//!
//! The host interface is Firecracker's, so tooling written against that
//! works unchanged:
//!
//! - host to guest: connect to the device socket, send
//!   `CONNECT <port>\n`, read back `OK <host_port>\n`, then stream.
//! - guest to host: the guest connects to port N and the device
//!   connects to `<socket>_N`.
//!
//! Threads rather than an async runtime: `deny-firehyve.toml` keeps
//! tokio out of firehyve's dependency graph on purpose, and the console
//! already solves the same shape this way.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context;
use slog::debug;
use vmm_core::mem::PhysMap;
use vmm_core::unixsock::{bind_restricted, write_all_bounded, SocketPolicy};

use super::control::{
    read_line_bounded, run_control_session, ControlSlot, LineRead,
    HANDSHAKE_BUDGET,
};
use super::mux::{GuestAction, RxPacket, VsockMux, MAX_CONNS};
use super::packet::{ConnKey, VsockHdr, CID_GUEST_MIN, HDR_SIZE};
use crate::pci::intr::{next_intr_session, BackendIntr, IntrSession};
use crate::queue::{ChainBuf, VirtQueue, VirtioCompletion};
use crate::{queue_drain_should_stop, VirtioDevice};
use vmm_devices::lifecycle::{IndicatedState, Indicator};
use vmm_devices::{Lifecycle, QuiesceGate};

/// rx, tx and event. Under the transport's four-queue cap.
pub const VSOCK_NUM_QUEUES: usize = 3;
/// Host to guest.
pub const VSOCK_RX_QUEUE: u16 = 0;
/// Guest to host.
pub const VSOCK_TX_QUEUE: u16 = 1;
/// Transport reset events. Buffers are accepted and never used.
pub const VSOCK_EVENT_QUEUE: u16 = 2;
/// Must be a power of two: `VirtQueue::new` panics otherwise.
pub const VSOCK_QUEUE_SIZE: u16 = 256;
/// `struct virtio_vsock_config` is one le64 guest_cid.
pub const VSOCK_CONFIG_SIZE: u16 = 8;
/// One vector per queue plus one for config change.
pub const VSOCK_MSIX_VECTORS: u16 = VSOCK_NUM_QUEUES as u16 + 1;
/// Bytes read from a host socket in one go.
const HOST_READ_CHUNK: usize = 64 * 1024;
/// Largest payload accepted from one guest TX chain, so a length the
/// guest picked never becomes an unbounded allocation.
const MAX_TX_PAYLOAD: usize = 64 * 1024;
/// Pause before retrying a host read when the guest is not draining.
const BACKPRESSURE_PAUSE: Duration = Duration::from_millis(2);
/// Budget for the reader threads to end during halt.
const HALT_BUDGET: Duration = Duration::from_secs(2);
/// How long a blocked host read waits before it looks at the shutdown
/// flag. A reader that only wakes on the socket being shut down cannot
/// end on its own, and halt then pays the whole budget for it.
const HOST_READ_POLL: Duration = Duration::from_millis(250);

/// How long a write to a host peer may take before the device gives
/// up on it.
///
/// `write_host` runs on a vCPU thread with the transport lock held, so
/// an unbounded write there stalls every other vCPU that touches this
/// device. `write_all_bounded` supplies the end: `SO_SNDTIMEO` reports
/// success on illumos and bounds nothing.
const HOST_WRITE_TIMEOUT: Duration = Duration::from_millis(500);
/// Pause after a failed `accept`, so a persistent error such as EMFILE
/// cannot spin the accept thread at full speed.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

type InterruptFn = Arc<BackendIntr>;

struct VsockShared {
    mux: VsockMux,
    physmap: Arc<PhysMap>,
    /// RX chains the guest has posted, waiting for a packet.
    pending_rx: Mutex<VecDeque<(u16, Vec<ChainBuf>)>>,
    rx_completion: Mutex<Option<Arc<VirtioCompletion>>>,
    interrupt: Arc<Mutex<Option<InterruptFn>>>,
    /// Serialises RX delivery. Held for a whole pairing pass.
    delivery: Mutex<()>,
    /// Host socket per live connection. Held behind an `Arc` so a
    /// write does not need the table locked while it runs.
    sockets: Mutex<HashMap<ConnKey, Arc<UnixStream>>>,
    /// Answers CONTROL requests. Empty until the binary installs a sink.
    control: ControlSlot,
    socket_path: PathBuf,
    /// Threads serving one host connection each.
    readers: Mutex<Vec<JoinHandle<()>>>,
    shutdown: Arc<AtomicBool>,
    gate: Arc<QuiesceGate>,
    log: slog::Logger,
}

impl VsockShared {
    /// Move queued packets into posted RX chains and publish them.
    fn deliver_rx(&self) {
        // A paused device must not touch guest memory. Packets stay
        // queued and go out on resume.
        if self.gate.is_paused() {
            return;
        }
        let completion = {
            let guard = self.rx_completion.lock().expect("rx completion lock");
            match guard.as_ref() {
                Some(c) => Arc::clone(c),
                None => return,
            }
        };

        // Delivery is serialised for the whole pass. Two threads pairing
        // packets with chains independently -- a reader thread and a
        // vCPU in notify_queue -- can complete them in either order, and
        // the guest consumes the used ring in order, so the stream would
        // be reassembled wrong. A split packet makes it worse: the tail
        // requeued at the head can be overtaken by the packet behind it.
        let _delivery = self.delivery.lock().expect("delivery lock");

        loop {
            if self.mux.rx_is_empty() {
                return;
            }
            let next = {
                let mut pending =
                    self.pending_rx.lock().expect("pending rx lock");
                pending.pop_front()
            };
            let Some((head, bufs)) = next else {
                return;
            };
            let Some(pkt) = self.mux.pop_rx() else {
                // Nothing to send after all: give the chain back rather
                // than completing it empty.
                self.pending_rx
                    .lock()
                    .expect("pending rx lock")
                    .push_front((head, bufs));
                return;
            };

            let written = self.write_packet(&bufs, pkt);
            completion.complete(head, written);
        }
    }

    /// Write one packet into a chain, splitting it when the chain is too
    /// small. RW carries a byte stream, so a split is legitimate; the
    /// remainder goes back to the front of the queue in order.
    fn write_packet(&self, bufs: &[ChainBuf], mut pkt: RxPacket) -> u32 {
        let capacity: usize = bufs
            .iter()
            .filter_map(|b| match b {
                ChainBuf::Writable { len, .. } => Some(*len as usize),
                _ => None,
            })
            .sum();
        if capacity < HDR_SIZE {
            // Cannot even carry a header. Drop the chain, keep the
            // packet for a chain that can.
            self.mux.requeue_rx_front(pkt);
            return 0;
        }

        let room = capacity - HDR_SIZE;
        if pkt.payload.len() > room {
            let rest = pkt.payload.split_off(room);
            let mut tail = pkt.clone();
            tail.payload = rest;
            tail.hdr.len = tail.payload.len() as u32;
            self.mux.requeue_rx_front(tail);
        }
        pkt.hdr.len = pkt.payload.len() as u32;

        let mut blob = Vec::with_capacity(HDR_SIZE + pkt.payload.len());
        blob.extend_from_slice(&pkt.hdr.to_bytes());
        blob.extend_from_slice(&pkt.payload);

        let mut written = 0usize;
        for buf in bufs {
            let ChainBuf::Writable { addr, len } = buf else {
                continue;
            };
            let remaining = blob.len() - written;
            if remaining == 0 {
                break;
            }
            let want = (*len as usize).min(remaining);
            let Some(sub) = self.physmap.lookup(*addr, want) else {
                break;
            };
            if sub.write_bytes(&blob[written..written + want]).is_err() {
                break;
            }
            written += want;
        }
        written as u32
    }

    /// Write payload to a connection's host socket.
    fn write_host(&self, key: ConnKey, data: &[u8]) {
        let socket = self
            .sockets
            .lock()
            .expect("sockets lock")
            .get(&key)
            .cloned();
        let Some(stream) = socket else {
            // The mux still holds the connection, so the guest thinks
            // it is live and would read a stream with a hole in it.
            // Dropping the payload in silence is what leaves it there.
            self.mux.queue_host_close(key);
            self.deliver_rx();
            return;
        };
        // The table stays unlocked for the write. A slow peer must not
        // hold the other vCPUs, or the halt sweep, behind it.
        if let Err(error) = write_all_bounded(&stream, data, HOST_WRITE_TIMEOUT)
        {
            debug!(self.log, "virtio-vsock host write gave up";
                "error" => %error);
            // A dead peer and one that stopped reading for
            // HOST_WRITE_TIMEOUT are the same to the guest.
            self.drop_socket_if_current(key, &stream);
        }
    }

    /// Take `key` out of the table and shut its socket down, so that
    /// this connection's reader wakes and tells the guest it has gone.
    ///
    /// The entry goes only while `stream` still holds it. The guest
    /// picks the key for a connection it opens, so it can close one
    /// and reopen the same key while an unlocked write is running.
    fn drop_socket_if_current(&self, key: ConnKey, stream: &Arc<UnixStream>) {
        let mut sockets = self.sockets.lock().expect("sockets lock");
        if sockets
            .get(&key)
            .is_some_and(|held| Arc::ptr_eq(held, stream))
        {
            sockets.remove(&key);
        }
        drop(sockets);
        // The peer may already be gone; there is nothing to do.
        let _ = stream.shutdown(Shutdown::Both);
    }

    /// Put a host socket in the table, ready to be written to.
    fn register_socket(&self, key: ConnKey, stream: UnixStream) {
        // Belt and braces only. This bounds a write on the platforms
        // where it works, and reports success and does nothing on
        // illumos, which is why `write_all_bounded` polls instead.
        if let Err(error) = stream.set_write_timeout(Some(HOST_WRITE_TIMEOUT)) {
            debug!(self.log, "virtio-vsock write timeout not set";
                "error" => %error);
        }
        self.sockets
            .lock()
            .expect("sockets lock")
            .insert(key, Arc::new(stream));
    }

    fn drop_socket(&self, key: ConnKey) {
        if let Some(stream) =
            self.sockets.lock().expect("sockets lock").remove(&key)
        {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    /// Register a reader thread, dropping the handles whose threads
    /// have ended.
    ///
    /// The vector is otherwise only drained by halt, so a peer that
    /// connects and disconnects in a loop grows it, and every handle in
    /// it, for the life of the VM.
    fn track_reader(&self, handle: JoinHandle<()>) {
        let mut readers = self.readers.lock().expect("reader lock");
        readers.retain(|h| !h.is_finished());
        readers.push(handle);
    }

    /// Whether another reader thread may start.
    ///
    /// This is what bounds the thread count. The mux connection cap
    /// cannot: a peer that connects and sends no CONNECT line holds a
    /// thread the mux has never heard of.
    fn reader_slot_free(&self) -> bool {
        let mut readers = self.readers.lock().expect("reader lock");
        readers.retain(|h| !h.is_finished());
        readers.len() < MAX_CONNS
    }
}

/// Read from a connection's host socket until it closes.
fn reader_loop(shared: Arc<VsockShared>, key: ConnKey, mut stream: UnixStream) {
    // Arm the poll here rather than inherit whatever the CONNECT
    // handshake left on the socket: a timeout the loop did not ask for
    // would read as a dead peer, and no timeout at all leaves the
    // shutdown check below unreachable.
    let polled = stream.set_read_timeout(Some(HOST_READ_POLL)).is_ok();
    let mut buf = vec![0u8; HOST_READ_CHUNK];
    loop {
        if shared.shutdown.load(Ordering::Acquire) {
            break;
        }
        let n = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            // An idle poll, not a dead peer. Round the loop so the
            // shutdown flag above is read again.
            Err(e)
                if polled
                    && matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
            {
                continue
            }
            Err(_) => break,
        };

        // The backlog is bounded, so a guest that stops reading must
        // stop this thread rather than grow VMM memory.
        let mut data = buf[..n].to_vec();
        loop {
            if shared.mux.queue_host_data(key, std::mem::take(&mut data)) {
                break;
            }
            data = buf[..n].to_vec();
            if shared.shutdown.load(Ordering::Acquire) {
                return;
            }
            shared.deliver_rx();
            std::thread::sleep(BACKPRESSURE_PAUSE);
        }
        shared.deliver_rx();
    }

    shared.mux.queue_host_close(key);
    shared.drop_socket(key);
    shared.deliver_rx();
}

/// Accept host-initiated connections and parse their CONNECT line.
fn accept_loop(shared: Arc<VsockShared>, listener: UnixListener) {
    // One report per run of failures. `incoming()` never ends, so a
    // persistent error is seen again on every pass, and a peer that can
    // make one happen must not be able to drive the log with it.
    let mut reported = false;
    for stream in listener.incoming() {
        if shared.shutdown.load(Ordering::Acquire) {
            break;
        }
        let stream = match stream {
            Ok(stream) => {
                reported = false;
                stream
            }
            Err(error) => {
                // A transient error is skipped and a persistent one,
                // such as EMFILE once the fd table fills, would spin
                // this thread at full speed without the pause.
                if !reported {
                    debug!(shared.log, "virtio-vsock accept failed";
                        "error" => %error);
                    reported = true;
                }
                std::thread::sleep(ACCEPT_BACKOFF);
                continue;
            }
        };
        // Every accepted connection costs a thread until its peer sends
        // a CONNECT line, which it need never do.
        if !shared.reader_slot_free() {
            let _ = stream.shutdown(Shutdown::Both);
            continue;
        }
        let conn_shared = Arc::clone(&shared);
        // One thread per pending connection: the CONNECT line is read
        // with a blocking read, and a client that never sends one must
        // not block every other client behind it.
        if let Ok(handle) = std::thread::Builder::new()
            .name("vsock-connect".into())
            .spawn(move || handle_host_connect(conn_shared, stream))
        {
            shared.track_reader(handle);
        }
    }
}

/// Longest CONNECT line accepted, so a client that never sends a newline
/// cannot make the device read without bound.
const CONNECT_LINE_MAX: usize = 64;

/// The verb a host peer sent on its first line.
enum HostVerb {
    /// Firecracker's `CONNECT <port>`: stream to a guest port.
    Connect(u32),
    /// `CONTROL`: the VMM answers this connection itself and the guest
    /// mux never sees it.
    Control,
    Unknown,
}

fn classify_host_line(line: &str) -> HostVerb {
    if let Some(port) = parse_connect_line(line) {
        return HostVerb::Connect(port);
    }
    if line.trim_end_matches(['\r', '\n']).trim() == "CONTROL" {
        return HostVerb::Control;
    }
    HostVerb::Unknown
}

/// Read the opening line and act on the verb it names.
fn handle_host_connect(shared: Arc<VsockShared>, mut stream: UnixStream) {
    // The reader arms its own poll and checks the shutdown flag on
    // every pass. A peer that opens a connection and sends nothing
    // would otherwise park this thread for the life of the VM, and it
    // is not in `sockets` yet, so halt's sweep cannot wake it either.
    let LineRead::Line(line) = read_line_bounded(
        &mut stream,
        &shared.shutdown,
        CONNECT_LINE_MAX,
        Some(Instant::now() + HANDSHAKE_BUDGET),
    ) else {
        return;
    };
    match classify_host_line(&line) {
        HostVerb::Connect(port) => start_host_connection(shared, stream, port),
        HostVerb::Control => run_control_session(
            &shared.control,
            &shared.shutdown,
            stream,
            &shared.log,
        ),
        HostVerb::Unknown => {
            // Best effort: the peer may already be gone, and the
            // connection ends either way. Bounded, because a peer that
            // never reads would otherwise hold this thread, and one of
            // the reader slots, for the life of the VM.
            let _ = write_all_bounded(
                &stream,
                b"ERROR: expected CONNECT <port>\n",
                HOST_WRITE_TIMEOUT,
            );
        }
    }
}

/// Join a host peer to a guest port and stream until it closes.
fn start_host_connection(
    shared: Arc<VsockShared>,
    stream: UnixStream,
    port: u32,
) {
    let key = ConnKey {
        guest_port: port,
        host_port: shared.mux.alloc_host_port(),
    };
    let s = stream;
    let Ok(peer) = s.try_clone() else {
        return;
    };
    // The table is capped, and a peer told OK for a connection the mux
    // refused would wait on a guest that is never going to hear about
    // it. Reserving the slot answers that without letting the guest
    // reach the connection yet.
    if !shared.mux.reserve_host_connect(key) {
        // Best effort: the peer may already be gone.
        let _ = write_all_bounded(
            &s,
            b"ERROR: too many connections\n",
            HOST_WRITE_TIMEOUT,
        );
        return;
    }
    // This thread owns the socket until the REQUEST is queued. Once it
    // is, any thread's deliver_rx may hand it to the guest, the guest
    // may answer and send payload, and a vCPU may then be inside
    // write_host on this same socket. Two writers would interleave the
    // OK line with guest payload and break the handshake framing, and
    // each would take the space the other's poll reported. So the OK
    // line goes out first, and the socket goes in the table before the
    // REQUEST, because payload for a key with no socket has nowhere to
    // go.
    if write_all_bounded(
        &s,
        format!("OK {}\n", key.host_port).as_bytes(),
        HOST_WRITE_TIMEOUT,
    )
    .is_err()
    {
        shared.mux.forget(key);
        return;
    }
    shared.register_socket(key, peer);
    shared.mux.announce_host_connect(key);
    shared.deliver_rx();
    reader_loop(shared, key, s);
}

/// Parse Firecracker's `CONNECT <port>` line.
pub fn parse_connect_line(line: &str) -> Option<u32> {
    let rest = line
        .trim_end_matches(['\r', '\n'])
        .strip_prefix("CONNECT ")?;
    rest.trim().parse().ok()
}

/// Parse the config half of `-s <slot>,virtio-vsock,<config>`.
///
/// Grammar: `<uds-path>,cid=<n>`. The CID is required: there is no
/// sensible default, and two guests sharing one would be a routing bug
/// that only shows up under load.
pub fn parse_vsock_config(config: &str) -> anyhow::Result<(PathBuf, u64)> {
    let mut parts = config.split(',');
    let path = parts.next().unwrap_or("");
    anyhow::ensure!(!path.is_empty(), "virtio-vsock requires a socket path");

    let mut cid: Option<u64> = None;
    for opt in parts.filter(|o| !o.is_empty()) {
        if let Some(v) = opt.strip_prefix("cid=") {
            cid =
                Some(v.parse().map_err(|_| {
                    anyhow::anyhow!("virtio-vsock: bad cid '{v}'")
                })?);
        } else {
            anyhow::bail!("virtio-vsock: unknown option '{opt}'");
        }
    }
    let cid =
        cid.ok_or_else(|| anyhow::anyhow!("virtio-vsock requires cid=<n>"))?;
    anyhow::ensure!(
        cid >= CID_GUEST_MIN,
        "virtio-vsock cid must be {CID_GUEST_MIN} or above, got {cid}"
    );
    Ok((PathBuf::from(path), cid))
}

pub struct VirtioVsock {
    shared: Arc<VsockShared>,
    accept: Mutex<Option<JoinHandle<()>>>,
    negotiated_features: Mutex<u64>,
    indicator: Indicator,
}

impl VirtioVsock {
    /// Bind the host socket and start accepting clients.
    ///
    /// Returns by value: `VirtioPciDevice::new` takes the backend by
    /// value, so clone [`interrupt`](Self::interrupt) before the move.
    pub fn new(
        socket_path: &Path,
        guest_cid: u64,
        physmap: Arc<PhysMap>,
        log: slog::Logger,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            guest_cid >= CID_GUEST_MIN,
            "virtio-vsock cid must be {CID_GUEST_MIN} or above, got {guest_cid}"
        );
        // 0600 socket in a 0700 directory, same policy as the console
        // and the control socket. Not a bare UnixListener::bind.
        let listener = bind_restricted(socket_path, SocketPolicy::default())
            .with_context(|| {
                format!("bind virtio-vsock socket {}", socket_path.display())
            })?;

        // Zero registered workers: the accept and reader threads block
        // on sockets, outside the gate. The gate's job here is
        // `is_paused`, so a paused device stops touching guest memory.
        let shared = Arc::new(VsockShared {
            mux: VsockMux::new(guest_cid),
            physmap,
            pending_rx: Mutex::new(VecDeque::new()),
            rx_completion: Mutex::new(None),
            delivery: Mutex::new(()),
            interrupt: Arc::new(Mutex::new(None)),
            sockets: Mutex::new(HashMap::new()),
            control: ControlSlot::default(),
            socket_path: socket_path.to_path_buf(),
            readers: Mutex::new(Vec::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            gate: Arc::new(QuiesceGate::new(0)),
            log,
        });

        let accept_shared = Arc::clone(&shared);
        let accept = std::thread::Builder::new()
            .name("virtio-vsock-accept".into())
            .spawn(move || accept_loop(accept_shared, listener))
            .context("spawn virtio-vsock accept thread")?;

        Ok(Self {
            shared,
            accept: Mutex::new(Some(accept)),
            negotiated_features: Mutex::new(0),
            indicator: Indicator::new(),
        })
    }

    /// The interrupt slot, for the binary to fill after the transport
    /// exists. Clone this before moving the device into the transport.
    pub fn interrupt(&self) -> Arc<Mutex<Option<InterruptFn>>> {
        Arc::clone(&self.shared.interrupt)
    }

    /// The control slot, for the binary to fill once the device registry
    /// exists. Clone this before moving the device into the transport.
    pub fn control_slot(&self) -> ControlSlot {
        self.shared.control.clone()
    }

    pub const fn config_size(&self) -> u16 {
        VSOCK_CONFIG_SIZE
    }

    pub fn socket_path(&self) -> &Path {
        &self.shared.socket_path
    }

    /// Get or create the completion handler for the RX ring.
    ///
    /// The session is read and the handler is stored under one lock,
    /// and [`VirtioDevice::reset`] drops the handler under the same
    /// lock after the transport has ended its session. So a handler
    /// this leaves behind names either the session that is ending, and
    /// is then dropped, or the one that follows.
    fn ensure_rx_completion(&self, queue: &VirtQueue) -> Arc<VirtioCompletion> {
        let mut slot = self
            .shared
            .rx_completion
            .lock()
            .expect("rx completion lock");
        if let Some(completion) = slot.as_ref() {
            return Arc::clone(completion);
        }
        let intr = Arc::clone(&self.shared.interrupt);
        let session =
            next_intr_session(&self.shared.interrupt, IntrSession::INITIAL);
        let completion = VirtioCompletion::new(
            queue,
            Arc::clone(&self.shared.physmap),
            session,
            move |session| {
                // The slot is unlocked for the raise. Delivery is a
                // call into the kernel, and the reset reads this
                // slot, so a raise that held the lock would put that
                // call in front of a vCPU.
                let path = intr.lock().ok().and_then(|slot| slot.clone());
                if let Some(path) = path {
                    path.raise(session, VSOCK_RX_QUEUE);
                }
            },
        );
        // Force the first completions through EVENT_IDX suppression so
        // an idle guest always wakes on the first packet.
        completion.set_force_interrupt(32);
        *slot = Some(Arc::clone(&completion));
        completion
    }

    /// Pop RX chains and hold them until a packet needs one.
    fn stash_rx(&self, queue: &mut VirtQueue, physmap: &PhysMap) {
        let completion = self.ensure_rx_completion(queue);

        // A stashed chain is only completed once a packet lands in it,
        // so the stash needs a bound of its own: a guest can otherwise
        // publish one descriptor repeatedly and grow VMM memory.
        let max_requests = usize::from(queue.size());
        let mut popped = 0usize;
        loop {
            let before = queue.last_avail_idx();
            let mut refused: Vec<u16> = Vec::new();
            {
                let mut pending =
                    self.shared.pending_rx.lock().expect("pending rx lock");
                while popped < max_requests && pending.len() < max_requests {
                    let Some(head) = queue.pop_avail(physmap) else {
                        break;
                    };
                    popped += 1;
                    let Some(chain) = queue.collect_chain(physmap, head) else {
                        refused.push(head);
                        continue;
                    };
                    pending.push_back((head, chain));
                }
            }
            // `pop_avail` already took these heads out of the available
            // ring, so each one has to go back on the used ring. Losing
            // one costs the guest a descriptor for the life of the
            // device, and there is no abort path to recover it.
            if !refused.is_empty() {
                debug!(self.shared.log, "virtio-vsock refused rx chains";
                    "count" => refused.len());
                for head in refused {
                    completion.complete(head, 0);
                }
            }
            let hit_request_cap = popped >= max_requests;
            if !hit_request_cap {
                queue.update_used_event(physmap);
            }
            if queue_drain_should_stop(queue, before, hit_request_cap, physmap)
            {
                break;
            }
        }
        self.shared.deliver_rx();
    }

    /// Consume guest packets from the transmit queue.
    fn drain_tx(&self, queue: &mut VirtQueue, physmap: &PhysMap) -> bool {
        let max_requests = usize::from(queue.size());
        let mut popped = 0usize;
        let mut used = false;
        loop {
            let before = queue.last_avail_idx();
            while popped < max_requests {
                let Some(head) = queue.pop_avail(physmap) else {
                    break;
                };
                popped += 1;
                if let Some(chain) = queue.collect_chain(physmap, head) {
                    self.handle_tx_chain(&chain, physmap);
                }
                // The chain is device-readable only, so nothing was
                // written back into it.
                queue.push_used(physmap, head, 0);
                used = true;
            }
            let hit_request_cap = popped >= max_requests;
            if !hit_request_cap {
                queue.update_used_event(physmap);
            }
            if queue_drain_should_stop(queue, before, hit_request_cap, physmap)
            {
                break;
            }
        }
        if used {
            self.shared.deliver_rx();
        }
        used
    }

    /// Read one guest packet out of a chain and act on it.
    fn handle_tx_chain(&self, bufs: &[ChainBuf], physmap: &PhysMap) {
        let mut blob = Vec::new();
        for buf in bufs {
            let ChainBuf::Readable { addr, len } = buf else {
                continue;
            };
            // A length the guest picked must never become an unbounded
            // allocation, so the whole packet is capped.
            const CAP: usize = HDR_SIZE + MAX_TX_PAYLOAD;
            let want = (*len as usize).min(CAP.saturating_sub(blob.len()));
            if want == 0 {
                break;
            }
            let Some(sub) = physmap.lookup(*addr, want) else {
                return;
            };
            let mut chunk = vec![0u8; want];
            if sub.read_bytes(&mut chunk).is_err() {
                return;
            }
            blob.extend_from_slice(&chunk);
        }

        let Some(hdr) = VsockHdr::from_bytes(&blob) else {
            return;
        };
        // Trust the header's length only as far as the bytes actually
        // present: a guest may claim more than it supplied.
        let available = blob.len() - HDR_SIZE;
        let take = (hdr.len as usize).min(available).min(MAX_TX_PAYLOAD);
        let payload = blob[HDR_SIZE..HDR_SIZE + take].to_vec();

        match self.shared.mux.on_guest_packet(&hdr, payload) {
            GuestAction::None => {}
            GuestAction::Deliver(key, data) => {
                self.shared.write_host(key, &data)
            }
            GuestAction::Close(key) => self.shared.drop_socket(key),
            GuestAction::Connect(key, port) => self.connect_out(key, port),
        }
    }

    /// A guest-initiated connection: dial `<socket>_<port>` on the host.
    fn connect_out(&self, key: ConnKey, port: u32) {
        let path = format!("{}_{}", self.shared.socket_path.display(), port);
        let stream = match UnixStream::connect(&path) {
            Ok(s) => s,
            Err(error) => {
                // Guest reachable and unbounded: a guest dialling a
                // closed port in a loop reaches this once per packet.
                debug!(self.shared.log, "virtio-vsock guest connect refused";
                    "path" => &path, "error" => %error);
                self.shared.mux.refuse_guest_connect(key);
                return;
            }
        };
        let Ok(reader_stream) = stream.try_clone() else {
            self.shared.mux.refuse_guest_connect(key);
            return;
        };
        self.shared.register_socket(key, stream);
        self.shared.mux.accept_guest_connect(key);

        let shared = Arc::clone(&self.shared);
        if let Ok(handle) = std::thread::Builder::new()
            .name("vsock-reader".into())
            .spawn(move || reader_loop(shared, key, reader_stream))
        {
            self.shared.track_reader(handle);
        }
    }
}

impl VirtioDevice for VirtioVsock {
    fn device_features(&self) -> u64 {
        // Ring features only. The transport adds VERSION_1 and strips
        // these two on the legacy path.
        crate::bits::VIRTIO_F_RING_EVENT_IDX
            | crate::bits::VIRTIO_F_RING_INDIRECT_DESC
    }

    fn set_features(&self, features: u64) {
        // Store only: the modern transport calls this twice, once per
        // 32-bit half, with a partial value on the first call.
        *self.negotiated_features.lock().expect("features lock") = features;
    }

    fn cfg_read(&self, offset: u16, _len: u8) -> u32 {
        // The whole config is one le64 guest_cid.
        let cid = self.shared.mux.guest_cid();
        match offset {
            0 => cid as u32,
            4 => (cid >> 32) as u32,
            _ => 0,
        }
    }

    fn cfg_write(&self, _offset: u16, _val: u32, _len: u8) {}

    fn process_queue(
        &self,
        _queue_idx: u16,
        _queue: &mut VirtQueue,
        _head: u16,
        _physmap: &PhysMap,
    ) -> u32 {
        0 // not called: notify_queue is overridden
    }

    fn notify_queue(
        &self,
        queue_idx: u16,
        queues: &mut [VirtQueue],
        physmap: &PhysMap,
    ) -> bool {
        let Some(queue) = queues.get_mut(queue_idx as usize) else {
            return false;
        };
        match queue_idx {
            VSOCK_RX_QUEUE => {
                self.stash_rx(queue, physmap);
                // The completion handler raises the interrupt later.
                false
            }
            VSOCK_TX_QUEUE => self.drain_tx(queue, physmap),
            VSOCK_EVENT_QUEUE => {
                // Event buffers are accepted and never used: the only
                // event defined is transport reset, which needs a
                // migration this binary does not have.
                false
            }
            _ => false,
        }
    }

    fn reset(&self) {
        *self.negotiated_features.lock().expect("features lock") = 0;
        *self
            .shared
            .rx_completion
            .lock()
            .expect("rx completion lock") = None;
        // Chains posted before the reset are dropped, not completed:
        // the guest abandons its rings across a reset and reposts.
        self.shared
            .pending_rx
            .lock()
            .expect("pending rx lock")
            .clear();
    }
}

impl Lifecycle for VirtioVsock {
    fn type_name(&self) -> &'static str {
        "virtio-vsock"
    }

    fn lifecycle_state(&self) -> Option<IndicatedState> {
        Some(self.indicator.state())
    }

    fn start(&self) -> anyhow::Result<()> {
        self.indicator.start();
        Ok(())
    }

    fn pause(&self) {
        self.indicator.pause();
        self.shared.gate.pause();
    }

    fn is_quiesced(&self) -> bool {
        self.shared.gate.is_quiesced()
    }

    fn resume(&self) {
        self.shared.gate.resume();
        self.indicator.resume();
        // Packets queued during the pause are owed to the guest.
        self.shared.deliver_rx();
    }

    fn halt(&self) {
        self.indicator.halt();
        self.shared.shutdown.store(true, Ordering::Release);

        // The accept thread is blocked in accept(). Connecting once
        // wakes it so it sees the flag; without this the join below
        // waits for a client that may never come.
        let _ = UnixStream::connect(&self.shared.socket_path);
        // Bounded for the same reason the readers are: if that connect
        // did not land, the accept thread is still blocked in accept()
        // and joining it has no end.
        if let Some(handle) = self.accept.lock().expect("accept lock").take() {
            if !vmm_core::thread::join_bounded(vec![handle], HALT_BUDGET) {
                debug!(
                    self.shared.log,
                    "virtio-vsock halt left the accept thread running"
                );
            }
        }

        // Shutting every socket down wakes the readers blocked on
        // read(). The accept thread has returned, so no reader is added
        // after this point.
        let keys: Vec<ConnKey> = self
            .shared
            .sockets
            .lock()
            .expect("sockets lock")
            .keys()
            .copied()
            .collect();
        for key in keys {
            self.shared.drop_socket(key);
        }
        let readers: Vec<_> = self
            .shared
            .readers
            .lock()
            .expect("reader lock")
            .drain(..)
            .collect();
        // The budget has to bound the join itself. Testing a deadline
        // before each `join()` only decides whether the next one
        // starts, so one wedged reader holds teardown open for as long
        // as it stays wedged.
        if !vmm_core::thread::join_bounded(readers, HALT_BUDGET) {
            debug!(self.shared.log, "virtio-vsock halt left a reader running");
        }

        // The socket may already be gone if the path was reused.
        let _ = std::fs::remove_file(&self.shared.socket_path);
    }
}

#[cfg(test)]
mod tests;
