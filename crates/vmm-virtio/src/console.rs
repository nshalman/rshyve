// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Single-port VirtIO console (device type 3).
//!
//! The host side is a Unix domain socket, one client at a time. The
//! guest sees one port and gets `/dev/hvc0`.
//!
//! No `VIRTIO_CONSOLE_F_MULTIPORT` and no `VIRTIO_CONSOLE_F_SIZE`: with
//! neither feature the control queues do not exist and the device needs
//! exactly two queues, which fits under the transport's four-queue cap.
//! The device config region is still advertised at its full spec size,
//! because the modern Linux driver checks `offset + len` against the
//! advertised length before it reads.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context;
use slog::{debug, warn};
use vmm_core::mem::PhysMap;
use vmm_core::unixsock::{bind_restricted, write_all_bounded, SocketPolicy};

use super::queue::{ChainBuf, VirtQueue, VirtioCompletion};
use super::{queue_drain_should_stop, VirtioDevice};
use crate::pci::intr::{next_intr_session, BackendIntr, IntrSession};
use vmm_devices::lifecycle::{IndicatedState, Indicator};
use vmm_devices::{Lifecycle, QuiesceGate};

/// receiveq and transmitq. Under the transport's four-queue cap.
pub const CONSOLE_NUM_QUEUES: usize = 2;
/// receiveq: host to guest.
pub const CONSOLE_RX_QUEUE: u16 = 0;
/// transmitq: guest to host.
pub const CONSOLE_TX_QUEUE: u16 = 1;
/// Must be a power of two: `VirtQueue::new` panics otherwise.
pub const CONSOLE_QUEUE_SIZE: u16 = 128;
/// Full `virtio_console_config`: cols, rows, max_nr_ports, emerg_wr.
///
/// Every field reads zero, but the whole struct is advertised because
/// the modern Linux driver bounds-checks against this length.
pub const CONSOLE_CONFIG_SIZE: u16 = 12;
/// One vector per queue plus one for config change.
pub const CONSOLE_MSIX_VECTORS: u16 = 3;
/// Host bytes buffered while the guest has posted no RX chain.
///
/// A cap is required: without one a fast host writer grows VMM memory
/// without limit while the guest is not reading.
pub const CONSOLE_RX_BACKLOG_MAX: usize = 64 * 1024;

/// Guest output is copied through a fixed buffer, so a descriptor
/// length the guest picked never becomes an allocation size.
const TX_CHUNK: usize = 4096;
/// How long a client may stall before it loses its console.
///
/// The transmit queue drains on the vCPU thread, so an unbounded write
/// would let an idle terminal hold a guest CPU. `write_all_bounded`
/// supplies the end: `SO_SNDTIMEO` reports success on illumos and
/// bounds nothing.
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_millis(250);
/// How long the accept thread waits before it re-reads the shutdown
/// flag. Bounds how long `halt` waits for that thread to return.
const ACCEPT_POLL_MS: i32 = 50;
/// Pause after an accept that failed for a reason retrying cannot fix.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);
/// How long `halt` waits for one set of threads.
///
/// The teardown sweep halts every device before `VM_DESTROY_SELF`, and
/// gives each halt a budget of its own. Staying inside that budget
/// keeps the sweep from abandoning this halt part-way. Same value as
/// the vsock device uses, for the same reason.
const HALT_BUDGET: Duration = Duration::from_secs(2);

/// How many of those joins the halt runs one after the other: the
/// accept thread, then the readers. The deadline teardown must allow
/// is the product, not the constant.
const HALT_JOIN_ROUNDS: u32 = 2;

/// The transport interrupt path, installed by the binary after the
/// transport exists. Clone the slot before the device is moved into
/// the transport.
pub type ConsoleInterrupt = Arc<BackendIntr>;

/// State shared with the accept and reader threads.
struct ConsoleShared {
    socket_path: PathBuf,
    physmap: Arc<PhysMap>,
    /// The attached client, if any. TX writes go here. Held behind an
    /// `Arc` so a write does not need the slot locked while it runs.
    client: Mutex<Option<Arc<UnixStream>>>,
    /// Host bytes with no guest RX chain to land in yet.
    backlog: Mutex<VecDeque<u8>>,
    /// Guest RX chains with no host bytes to fill them yet.
    pending_rx: Mutex<VecDeque<(u16, Vec<ChainBuf>)>>,
    rx_completion: Mutex<Option<Arc<VirtioCompletion>>>,
    interrupt: Arc<Mutex<Option<ConsoleInterrupt>>>,
    /// Reader threads, one per client that has been attached.
    readers: Mutex<Vec<JoinHandle<()>>>,
    shutdown: AtomicBool,
    gate: Arc<QuiesceGate>,
    /// Set while the backlog is over its cap, so the warning is emitted
    /// once per overflow burst rather than once per read.
    overflowed: AtomicBool,
    log: slog::Logger,
}

impl ConsoleShared {
    /// Move host bytes into posted guest RX chains and publish them.
    fn deliver_rx(&self) {
        // A paused device must not touch guest memory. Bytes stay in
        // the backlog and are delivered on resume.
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

        loop {
            let mut backlog = self.backlog.lock().expect("backlog lock");
            if backlog.is_empty() {
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

            let mut written = 0u32;
            for buf in &bufs {
                let ChainBuf::Writable { addr, len } = buf else {
                    continue;
                };
                let want = (*len as usize).min(backlog.len());
                if want == 0 {
                    break;
                }
                // Look the buffer up before the bytes leave the
                // backlog, so an unmapped chain does not eat them.
                let Some(sub) = self.physmap.lookup(*addr, want) else {
                    break;
                };
                let chunk: Vec<u8> = backlog.drain(..want).collect();
                if sub.write_bytes(&chunk).is_err() {
                    break;
                }
                written = written.saturating_add(want as u32);
            }
            drop(backlog);
            completion.complete(head, written);
        }
    }

    /// Write guest output to the attached client, if there is one.
    fn write_host(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let client = self.client.lock().expect("client lock").clone();
        let Some(stream) = client else {
            // No client attached. Console output is best-effort.
            return;
        };
        // The slot stays unlocked for the write. A terminal that
        // stopped reading must not hold the vCPU draining the
        // transmit queue, or every thread that needs the slot.
        if let Err(error) =
            write_all_bounded(&stream, data, CLIENT_WRITE_TIMEOUT)
        {
            warn!(self.log, "virtio-console client write failed";
                "error" => %error);
            self.detach_client(&stream);
        }
    }

    /// Drop `stream` as the client and shut it down, so that its
    /// reader thread wakes and returns.
    ///
    /// The slot is cleared only while `stream` still holds it: a newer
    /// client may have taken it over during the write that failed.
    fn detach_client(&self, stream: &Arc<UnixStream>) {
        let mut guard = self.client.lock().expect("client lock");
        if guard.as_ref().is_some_and(|held| Arc::ptr_eq(held, stream)) {
            guard.take();
        }
        drop(guard);
        // The peer may already be gone; there is nothing to do.
        let _ = stream.shutdown(Shutdown::Both);
    }

    /// Install `stream` as the client and shut down the one it
    /// replaces, so that its reader thread wakes and returns.
    fn replace_client(&self, stream: Option<UnixStream>) {
        let previous = std::mem::replace(
            &mut *self.client.lock().expect("client lock"),
            stream.map(Arc::new),
        );
        if let Some(old) = previous {
            // The peer may already be gone; there is nothing to do.
            let _ = old.shutdown(Shutdown::Both);
        }
    }

    /// Join the readers whose clients have gone, so a long-lived
    /// console does not accumulate thread handles.
    fn reap_readers(&self) {
        let done = {
            let mut readers = self.readers.lock().expect("reader lock");
            let (done, alive): (Vec<_>, Vec<_>) =
                readers.drain(..).partition(JoinHandle::is_finished);
            *readers = alive;
            done
        };
        for handle in done {
            // A panicked reader must not abort the accept loop.
            let _ = handle.join();
        }
    }

    /// Take over as the single client and start reading from it.
    fn attach_client(self: &Arc<Self>, stream: UnixStream) {
        // An accepted socket inherits the listener's non-blocking flag
        // on some platforms, and the reader needs a blocking read.
        if let Err(error) = stream.set_nonblocking(false) {
            warn!(self.log, "virtio-console client setup failed";
                "error" => %error);
            return;
        }
        // Belt and braces only. This bounds a write on the platforms
        // where it works, and reports success and does nothing on
        // illumos, which is why `write_all_bounded` polls instead.
        if let Err(error) = stream.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT))
        {
            warn!(self.log, "virtio-console write timeout not set";
                "error" => %error);
        }
        let reader = match stream.try_clone() {
            Ok(r) => r,
            Err(error) => {
                warn!(self.log, "virtio-console client clone failed";
                    "error" => %error);
                return;
            }
        };

        self.replace_client(Some(stream));
        self.reap_readers();

        let worker = Arc::clone(self);
        match std::thread::Builder::new()
            .name("virtio-console-rx".into())
            .spawn(move || reader_loop(worker, reader))
        {
            Ok(handle) => {
                self.readers.lock().expect("reader lock").push(handle)
            }
            Err(error) => {
                warn!(self.log, "virtio-console reader spawn failed";
                    "error" => %error);
                self.replace_client(None);
            }
        }
    }
}

/// Wait for events on `fd`, or for `timeout_ms` to pass.
///
/// A blocking `accept` can only be broken by racing a connect against
/// the listener, and a connect that fails leaves the thread blocked
/// forever, which would hang VM teardown.
fn poll_events(fd: RawFd, timeout_ms: i32) -> libc::c_short {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // A timeout or EINTR reports nothing, and the caller then re-reads
    // the shutdown flag.
    if unsafe { libc::poll(&mut pfd, 1, timeout_ms) } <= 0 {
        return 0;
    }
    pfd.revents
}

fn accept_loop(shared: Arc<ConsoleShared>, listener: UnixListener) {
    if let Err(error) = listener.set_nonblocking(true) {
        warn!(shared.log, "virtio-console listener setup failed";
            "error" => %error);
        return;
    }
    let fd = listener.as_raw_fd();

    while !shared.shutdown.load(Ordering::Acquire) {
        let revents = poll_events(fd, ACCEPT_POLL_MS);
        // A dead listener reports its condition on every call, so the
        // loop must leave rather than spin on it.
        if revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            warn!(shared.log, "virtio-console listener is unusable";
                "revents" => i32::from(revents));
            return;
        }
        if revents & libc::POLLIN == 0 {
            continue;
        }
        match listener.accept() {
            Ok((stream, _)) => shared.attach_client(stream),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => {
                warn!(shared.log, "virtio-console accept failed";
                    "error" => %error);
                // Out of file descriptors leaves the connection
                // pending, so back off instead of spinning on it.
                std::thread::sleep(ACCEPT_BACKOFF);
            }
        }
    }
}

fn reader_loop(shared: Arc<ConsoleShared>, mut stream: UnixStream) {
    let mut buf = [0u8; 4096];
    loop {
        if shared.shutdown.load(Ordering::Acquire) {
            break;
        }
        let n = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                continue
            }
            Err(_) => break,
        };
        {
            let mut backlog = shared.backlog.lock().expect("backlog lock");
            let room = CONSOLE_RX_BACKLOG_MAX.saturating_sub(backlog.len());
            let take = n.min(room);
            if take < n {
                if !shared.overflowed.swap(true, Ordering::Relaxed) {
                    warn!(shared.log,
                        "virtio-console RX backlog full, dropping input";
                        "dropped" => n.saturating_sub(take));
                }
            } else {
                shared.overflowed.store(false, Ordering::Relaxed);
            }
            backlog.extend(&buf[..take]);
        }
        shared.deliver_rx();
    }
    // The client slot is not cleared here: a newer client may already
    // own it, and the write path drops a dead one on its own.
}

/// Parse the config half of `-s <slot>,virtio-console,<socket-path>`.
pub fn parse_console_config(config: &str) -> anyhow::Result<&str> {
    if config.is_empty() {
        anyhow::bail!("virtio-console requires a socket path");
    }
    let mut parts = config.split(',');
    let path = parts.next().unwrap_or(config);
    if let Some(opt) = parts.next() {
        // Rejecting unknown options stops a misspelled flag from
        // looking as though it were honored.
        anyhow::bail!("virtio-console takes no options, got '{}'", opt);
    }
    Ok(path)
}

/// Single-port VirtIO console backed by a Unix domain socket.
pub struct VirtioConsole {
    shared: Arc<ConsoleShared>,
    accept: Mutex<Option<JoinHandle<()>>>,
    negotiated_features: Mutex<u64>,
    indicator: Indicator,
}

impl VirtioConsole {
    /// Bind the host socket and start accepting clients.
    ///
    /// Returns by value: `VirtioPciDevice::new` takes the backend by
    /// value, so clone [`interrupt`](Self::interrupt) and read
    /// [`config_size`](Self::config_size) before the move.
    pub fn new(
        socket_path: &Path,
        physmap: Arc<PhysMap>,
        log: slog::Logger,
    ) -> anyhow::Result<Self> {
        // 0600 socket in a 0700 directory, same policy as the control
        // socket. Do not use a bare UnixListener::bind here.
        let listener = bind_restricted(socket_path, SocketPolicy::default())
            .with_context(|| {
                format!("bind virtio-console socket {}", socket_path.display())
            })?;

        // Zero registered workers: the accept and reader threads block
        // on the socket, outside the gate, and a console holds no
        // durable in-flight work. The gate's job here is `is_paused`.
        let shared = Arc::new(ConsoleShared {
            socket_path: socket_path.to_path_buf(),
            physmap,
            client: Mutex::new(None),
            backlog: Mutex::new(VecDeque::new()),
            pending_rx: Mutex::new(VecDeque::new()),
            rx_completion: Mutex::new(None),
            interrupt: Arc::new(Mutex::new(None)),
            readers: Mutex::new(Vec::new()),
            shutdown: AtomicBool::new(false),
            gate: Arc::new(QuiesceGate::new(0)),
            overflowed: AtomicBool::new(false),
            log,
        });

        let accept_shared = Arc::clone(&shared);
        let accept = std::thread::Builder::new()
            .name("virtio-console-accept".into())
            .spawn(move || accept_loop(accept_shared, listener))
            .context("spawn virtio-console accept thread")?;

        Ok(Self {
            shared,
            accept: Mutex::new(Some(accept)),
            negotiated_features: Mutex::new(0),
            indicator: Indicator::new(),
        })
    }

    /// The interrupt slot, for the binary to fill after the transport
    /// exists. Clone this before moving the device into the transport.
    pub fn interrupt(&self) -> Arc<Mutex<Option<ConsoleInterrupt>>> {
        Arc::clone(&self.shared.interrupt)
    }

    pub const fn config_size(&self) -> u16 {
        CONSOLE_CONFIG_SIZE
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
                    path.raise(session, CONSOLE_RX_QUEUE);
                }
            },
        );
        // Force the first completions through EVENT_IDX suppression so
        // an idle guest console always wakes on its first input.
        completion.set_force_interrupt(32);
        *slot = Some(Arc::clone(&completion));
        completion
    }

    /// Pop RX chains and hold them until host bytes arrive.
    fn stash_rx(&self, queue: &mut VirtQueue, physmap: &PhysMap) {
        let completion = self.ensure_rx_completion(queue);

        // A stashed chain is only completed once host bytes land in
        // it, so the stash needs a bound of its own: a guest can
        // otherwise publish one descriptor over and over and grow VMM
        // memory without limit.
        let max_requests = usize::from(queue.size());
        let max_pending = max_requests;
        let mut popped = 0usize;
        loop {
            let before = queue.last_avail_idx();
            let mut refused: Vec<u16> = Vec::new();
            {
                let mut pending =
                    self.shared.pending_rx.lock().expect("pending rx lock");
                while popped < max_requests && pending.len() < max_pending {
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
                debug!(self.shared.log, "virtio-console refused rx chains";
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

    /// Drain guest output inline on the vCPU thread.
    fn drain_tx(&self, queue: &mut VirtQueue, physmap: &PhysMap) -> bool {
        let old_used_idx = queue.read_used_idx(physmap);
        let max_requests = usize::from(queue.size());
        let mut popped = 0usize;
        let mut did_work = false;
        let mut chunk = [0u8; TX_CHUNK];

        loop {
            let before = queue.last_avail_idx();
            while popped < max_requests {
                let Some(head) = queue.pop_avail(physmap) else {
                    break;
                };
                popped += 1;
                if let Some(chain) = queue.collect_chain(physmap, head) {
                    for buf in &chain {
                        let ChainBuf::Readable { addr, len } = buf else {
                            continue;
                        };
                        self.write_chain_buf(physmap, *addr, *len, &mut chunk);
                    }
                }
                // The chain is entirely device-readable, so the used
                // length is zero.
                queue.push_used(physmap, head, 0);
                did_work = true;
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

        did_work && queue.should_notify_guest(physmap, old_used_idx)
    }

    /// Copy one device-readable buffer out to the client, in chunks.
    fn write_chain_buf(
        &self,
        physmap: &PhysMap,
        addr: u64,
        len: u32,
        chunk: &mut [u8; TX_CHUNK],
    ) {
        let mut remaining = len as usize;
        let mut offset = 0u64;
        while remaining > 0 {
            let n = remaining.min(TX_CHUNK);
            let Some(gpa) = addr.checked_add(offset) else {
                return;
            };
            let Some(sub) = physmap.lookup(gpa, n) else {
                return;
            };
            if sub.read_bytes(&mut chunk[..n]).is_err() {
                return;
            }
            self.shared.write_host(&chunk[..n]);
            offset = offset.saturating_add(n as u64);
            remaining = remaining.saturating_sub(n);
        }
    }
}

impl VirtioDevice for VirtioConsole {
    fn device_features(&self) -> u64 {
        // Ring features only. The transport adds VERSION_1 and strips
        // these two on the legacy path.
        super::bits::VIRTIO_F_RING_EVENT_IDX
            | super::bits::VIRTIO_F_RING_INDIRECT_DESC
    }

    fn set_features(&self, features: u64) {
        // Store only: the modern transport calls this twice, once per
        // 32-bit half, with a partial value on the first call.
        *self.negotiated_features.lock().expect("features lock") = features;
    }

    fn cfg_read(&self, _offset: u16, _len: u8) -> u32 {
        // cols, rows, max_nr_ports and emerg_wr all read zero.
        0
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
            CONSOLE_RX_QUEUE => {
                self.stash_rx(queue, physmap);
                // The completion handler raises the interrupt later.
                false
            }
            CONSOLE_TX_QUEUE => self.drain_tx(queue, physmap),
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
        // The host-side backlog survives a guest reset: those bytes
        // were accepted from the client and are still owed to the guest.
    }
}

impl Lifecycle for VirtioConsole {
    fn type_name(&self) -> &'static str {
        "virtio-console"
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
        // Non-blocking atomic load. Joining happens in halt().
        self.shared.gate.is_quiesced()
    }

    fn resume(&self) {
        self.shared.gate.resume();
        self.indicator.resume();
        // Bytes buffered during the pause are owed to the guest.
        self.shared.deliver_rx();
    }

    /// The halt joins the accept thread for one budget and then the
    /// reader threads for another, one after the other, so the deadline
    /// it really holds is twice the constant.
    fn halt_budget(&self) -> Duration {
        HALT_BUDGET.saturating_mul(HALT_JOIN_ROUNDS)
    }

    fn halt(&self) {
        self.indicator.halt();
        self.shared.shutdown.store(true, Ordering::Release);

        // The accept thread polls the flag, so it returns on its own.
        // The bound is for the case where it cannot: this halt runs on
        // the teardown sweep now, and no device may hold that thread
        // short of the VM destroy.
        if let Some(handle) = self.accept.lock().expect("accept lock").take() {
            if !vmm_core::thread::join_bounded(vec![handle], HALT_BUDGET) {
                debug!(
                    self.shared.log,
                    "virtio-console halt left the accept thread running"
                );
            }
        }

        // Shutting the socket down wakes the reader blocked on read().
        // The accept thread has returned, so no reader is added after
        // this point.
        self.shared.replace_client(None);
        let readers: Vec<_> = self
            .shared
            .readers
            .lock()
            .expect("reader lock")
            .drain(..)
            .collect();
        // The budget has to bound the join itself, not decide whether
        // the next one starts. A reader the shutdown did not reach sits
        // in a `read(2)` nothing here can revoke, so it is left running
        // and teardown goes on.
        if !vmm_core::thread::join_bounded(readers, HALT_BUDGET) {
            debug!(
                self.shared.log,
                "virtio-console halt left a reader running"
            );
        }

        // The socket may already be gone if the path was reused.
        let _ = std::fs::remove_file(&self.shared.socket_path);
    }
}

#[cfg(test)]
mod tests;
