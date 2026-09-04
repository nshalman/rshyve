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
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context;
use slog::warn;
use vmm_core::mem::PhysMap;
use vmm_core::unixsock::{bind_restricted, SocketPolicy};

use super::control::{run_control_session, ControlSlot};
use super::mux::{GuestAction, RxPacket, VsockMux};
use super::packet::{ConnKey, VsockHdr, CID_GUEST_MIN, HDR_SIZE};
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
/// Budget for a reader thread to notice shutdown during halt.
const HALT_BUDGET: Duration = Duration::from_secs(2);

type InterruptFn = Arc<dyn Fn() + Send + Sync>;

struct VsockShared {
    mux: VsockMux,
    physmap: Arc<PhysMap>,
    /// RX chains the guest has posted, waiting for a packet.
    pending_rx: Mutex<VecDeque<(u16, Vec<ChainBuf>)>>,
    rx_completion: Mutex<Option<Arc<VirtioCompletion>>>,
    interrupt: Arc<Mutex<Option<InterruptFn>>>,
    /// Serialises RX delivery. Held for a whole pairing pass.
    delivery: Mutex<()>,
    /// Host socket per live connection.
    sockets: Mutex<HashMap<ConnKey, UnixStream>>,
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
        let mut sockets = self.sockets.lock().expect("sockets lock");
        let Some(stream) = sockets.get_mut(&key) else {
            return;
        };
        if stream.write_all(data).is_err() {
            // Shutting the socket down wakes this connection's reader,
            // which tells the guest.
            if let Some(stream) = sockets.remove(&key) {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
    }

    fn drop_socket(&self, key: ConnKey) {
        if let Some(stream) =
            self.sockets.lock().expect("sockets lock").remove(&key)
        {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

/// Read from a connection's host socket until it closes.
fn reader_loop(shared: Arc<VsockShared>, key: ConnKey, mut stream: UnixStream) {
    let mut buf = vec![0u8; HOST_READ_CHUNK];
    loop {
        if shared.shutdown.load(Ordering::Acquire) {
            break;
        }
        let n = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
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
    for stream in listener.incoming() {
        if shared.shutdown.load(Ordering::Acquire) {
            break;
        }
        let Ok(stream) = stream else { continue };
        let conn_shared = Arc::clone(&shared);
        // One thread per pending connection: the CONNECT line is read
        // with a blocking read, and a client that never sends one must
        // not block every other client behind it.
        if let Ok(handle) = std::thread::Builder::new()
            .name("vsock-connect".into())
            .spawn(move || handle_host_connect(conn_shared, stream))
        {
            shared.readers.lock().expect("reader lock").push(handle);
        }
    }
}

/// Longest CONNECT line accepted, so a client that never sends a newline
/// cannot make the device read without bound.
const CONNECT_LINE_MAX: usize = 64;

/// Read the CONNECT line one byte at a time.
///
/// Deliberately not a BufReader: that reads a whole block, and anything
/// the client pipelined behind the newline would sit in a buffer this
/// function is about to drop. A client is entitled to send its first
/// request in the same write as the CONNECT line, and losing it would
/// look like the guest never answered.
fn read_connect_line(stream: &mut UnixStream) -> Option<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.len() < CONNECT_LINE_MAX {
        match stream.read(&mut byte) {
            Ok(0) => return None,
            Ok(_) => {
                if byte[0] == b'\n' {
                    return String::from_utf8(line).ok();
                }
                line.push(byte[0]);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
    None
}

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
    let Some(line) = read_connect_line(&mut stream) else {
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
            // connection ends either way.
            let mut s = stream;
            let _ = s.write_all(b"ERROR: expected CONNECT <port>\n");
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
    let mut s = stream;
    if writeln!(s, "OK {}", key.host_port).is_err() {
        return;
    }
    shared.sockets.lock().expect("sockets lock").insert(
        key,
        match s.try_clone() {
            Ok(c) => c,
            Err(_) => return,
        },
    );
    shared.mux.start_host_connect(key);
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

    fn ensure_rx_completion(&self, queue: &VirtQueue) {
        let mut slot = self
            .shared
            .rx_completion
            .lock()
            .expect("rx completion lock");
        if slot.is_some() {
            return;
        }
        let intr = Arc::clone(&self.shared.interrupt);
        let completion = VirtioCompletion::new(
            queue,
            Arc::clone(&self.shared.physmap),
            move || {
                if let Ok(guard) = intr.lock() {
                    if let Some(ref raise) = *guard {
                        raise();
                    }
                }
            },
        );
        // Force the first completions through EVENT_IDX suppression so
        // an idle guest always wakes on the first packet.
        completion.set_force_interrupt(32);
        *slot = Some(completion);
    }

    /// Pop RX chains and hold them until a packet needs one.
    fn stash_rx(&self, queue: &mut VirtQueue, physmap: &PhysMap) {
        self.ensure_rx_completion(queue);

        // A stashed chain is only completed once a packet lands in it,
        // so the stash needs a bound of its own: a guest can otherwise
        // publish one descriptor repeatedly and grow VMM memory.
        let max_requests = usize::from(queue.size());
        let mut popped = 0usize;
        loop {
            let before = queue.last_avail_idx();
            {
                let mut pending =
                    self.shared.pending_rx.lock().expect("pending rx lock");
                while popped < max_requests && pending.len() < max_requests {
                    let Some(head) = queue.pop_avail(physmap) else {
                        break;
                    };
                    popped += 1;
                    let Some(chain) = queue.collect_chain(physmap, head) else {
                        continue;
                    };
                    pending.push_back((head, chain));
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
                warn!(self.shared.log, "virtio-vsock guest connect refused";
                    "path" => &path, "error" => %error);
                self.shared.mux.refuse_guest_connect(key);
                return;
            }
        };
        let Ok(reader_stream) = stream.try_clone() else {
            self.shared.mux.refuse_guest_connect(key);
            return;
        };
        self.shared
            .sockets
            .lock()
            .expect("sockets lock")
            .insert(key, stream);
        self.shared.mux.accept_guest_connect(key);

        let shared = Arc::clone(&self.shared);
        if let Ok(handle) = std::thread::Builder::new()
            .name("vsock-reader".into())
            .spawn(move || reader_loop(shared, key, reader_stream))
        {
            self.shared
                .readers
                .lock()
                .expect("reader lock")
                .push(handle);
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
        if let Some(handle) = self.accept.lock().expect("accept lock").take() {
            let _ = handle.join();
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
        let deadline = std::time::Instant::now() + HALT_BUDGET;
        for handle in readers {
            // A panicked or wedged reader must not abort teardown.
            if std::time::Instant::now() < deadline {
                let _ = handle.join();
            }
        }

        // The socket may already be gone if the path was reused.
        let _ = std::fs::remove_file(&self.shared.socket_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_line_is_parsed() {
        assert_eq!(parse_connect_line("CONNECT 1024\n"), Some(1024));
        assert_eq!(parse_connect_line("CONNECT 1024\r\n"), Some(1024));
        assert_eq!(parse_connect_line("CONNECT 1024"), Some(1024));
        assert_eq!(parse_connect_line("CONNECT  52 \n"), Some(52));
    }

    #[test]
    fn a_malformed_connect_line_is_refused() {
        for line in [
            "connect 1024\n", // Firecracker's verb is upper case
            "CONNECT\n",
            "CONNECT abc\n",
            "CONNECT -1\n",
            "\n",
            "",
            "HELLO 1024\n",
        ] {
            assert_eq!(parse_connect_line(line), None, "accepted {line:?}");
        }
    }

    #[test]
    fn config_needs_a_path_and_a_cid() {
        let (path, cid) = parse_vsock_config("/tmp/v.sock,cid=3").expect("ok");
        assert_eq!(path, PathBuf::from("/tmp/v.sock"));
        assert_eq!(cid, 3);
    }

    #[test]
    fn config_without_a_cid_is_an_error() {
        // No default: two guests sharing a CID is a routing bug that
        // only shows up under load.
        assert!(parse_vsock_config("/tmp/v.sock").is_err());
    }

    /// 0, 1 and 2 are the hypervisor, loopback and host. A guest given
    /// one of them would collide with the host's own address.
    #[test]
    fn config_rejects_a_reserved_cid() {
        for cid in [0, 1, 2] {
            assert!(
                parse_vsock_config(&format!("/tmp/v.sock,cid={cid}")).is_err(),
                "accepted reserved cid {cid}"
            );
        }
        assert!(parse_vsock_config("/tmp/v.sock,cid=3").is_ok());
    }

    #[test]
    fn config_rejects_junk() {
        assert!(parse_vsock_config("").is_err());
        assert!(parse_vsock_config("/tmp/v.sock,cid=x").is_err());
        assert!(parse_vsock_config("/tmp/v.sock,cid=3,bogus").is_err());
        assert!(parse_vsock_config("/tmp/v.sock,tag=nope,cid=3").is_err());
    }

    /// The line reader must stop at the newline and leave whatever
    /// follows on the socket. Reading a block instead would swallow a
    /// request a client pipelined behind the CONNECT line.
    #[test]
    fn the_connect_line_reader_stops_at_the_newline() {
        use std::io::Write as _;
        use std::os::unix::net::UnixStream as Uds;

        let (mut a, mut b) = Uds::pair().expect("socketpair");
        a.write_all(b"CONNECT 1234\nPIPELINED-REQUEST")
            .expect("write");

        let line = read_connect_line(&mut b).expect("line");
        assert_eq!(line, "CONNECT 1234");

        // What follows must still be readable by the next stage.
        let mut rest = [0u8; 32];
        let n = b.read(&mut rest).expect("read rest");
        assert_eq!(&rest[..n], b"PIPELINED-REQUEST");
    }

    #[test]
    fn a_connect_line_without_a_newline_is_bounded() {
        use std::io::Write as _;
        use std::os::unix::net::UnixStream as Uds;

        let (mut a, mut b) = Uds::pair().expect("socketpair");
        a.write_all(&[b'X'; CONNECT_LINE_MAX * 2]).expect("write");
        assert_eq!(read_connect_line(&mut b), None);
    }

    #[test]
    fn the_connect_verb_is_unchanged_by_the_control_verb() {
        assert!(matches!(
            classify_host_line("CONNECT 1024\n"),
            HostVerb::Connect(1024)
        ));
        assert!(matches!(
            classify_host_line("CONNECT 1024"),
            HostVerb::Connect(1024)
        ));
    }

    #[test]
    fn the_control_verb_is_recognised() {
        for line in ["CONTROL", "CONTROL\n", "CONTROL\r\n", " CONTROL "] {
            assert!(
                matches!(classify_host_line(line), HostVerb::Control),
                "refused {line:?}"
            );
        }
    }

    #[test]
    fn every_other_opening_line_is_unknown() {
        for line in [
            "control",  // the verb is upper case
            "CONTROLX", // no prefix match
            "CONTROL 4",
            "CONNECT abc",
            "",
        ] {
            assert!(
                matches!(classify_host_line(line), HostVerb::Unknown),
                "accepted {line:?}"
            );
        }
    }

    #[test]
    fn queue_count_fits_the_transport() {
        assert_eq!(VSOCK_NUM_QUEUES, 3);
        assert!(VSOCK_QUEUE_SIZE.is_power_of_two());
        assert_eq!(VSOCK_MSIX_VECTORS, 4);
    }

    #[test]
    fn the_config_space_is_one_le64_cid() {
        assert_eq!(VSOCK_CONFIG_SIZE, 8);
    }
}
