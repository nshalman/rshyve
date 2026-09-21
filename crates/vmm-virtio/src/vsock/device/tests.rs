// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unit tests for the virtio-vsock device.

use super::listener::{
    classify_host_line, handle_host_connect, reader_loop,
    start_host_connection, HostVerb, CONNECT_LINE_MAX,
};
use super::shared::HOST_WRITE_TIMEOUT;
use super::*;
use crate::pci::intr::{BackendIntr, IntrGate};
use crate::socket_accept::accepted_blocking;
use crate::socket_halt::{HALT_BUDGET, HALT_JOIN_ROUNDS};
use crate::vsock::control::{read_line_bounded, LineRead, HANDSHAKE_BUDGET};
use crate::vsock::mux::MAX_CONNS;
use crate::vsock::packet::ConnKey;
use std::io::{self, BufRead as _, Read};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use vmm_core::unixsock::write_all_bounded;
use vmm_devices::Lifecycle;

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

    let shutdown = AtomicBool::new(false);
    let LineRead::Line(line) =
        read_line_bounded(&mut b, &shutdown, CONNECT_LINE_MAX, None)
    else {
        panic!("the CONNECT line was not read");
    };
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
    let shutdown = AtomicBool::new(false);
    assert!(matches!(
        read_line_bounded(&mut b, &shutdown, CONNECT_LINE_MAX, None),
        LineRead::TooLong,
    ));
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

// -- Receive ring fixtures --

const DESC_GPA: u64 = 0x1000;
const AVAIL_GPA: u64 = 0x1100;
const USED_GPA: u64 = 0x1200;
const BUF_GPA: u64 = 0x1800;
const RING_SIZE: u16 = 4;

fn test_logger() -> slog::Logger {
    slog::Logger::root(slog::Discard, slog::o!())
}

/// A device whose socket sits in a directory `bind_restricted`
/// makes itself. Making it here instead would race the umask that
/// `bind_restricted` sets around its own bind.
fn test_vsock(physmap: Arc<PhysMap>) -> (PathBuf, VirtioVsock) {
    static SEQ: std::sync::atomic::AtomicU32 =
        std::sync::atomic::AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "rust-bhyve-vsock-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let vsock =
        VirtioVsock::new(&dir.join("v.sock"), 3, physmap, test_logger())
            .expect("create vsock device");
    (dir, vsock)
}

fn write_desc(
    physmap: &PhysMap,
    idx: u64,
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
) {
    physmap
        .lookup(DESC_GPA + idx * 16, 16)
        .expect("mapped descriptor")
        .write(&crate::queue::VirtqDesc {
            addr,
            len,
            flags,
            next,
        })
        .expect("write descriptor");
}

/// A receive queue with one chain posted in the available ring.
fn rx_ring() -> (Arc<PhysMap>, Vec<VirtQueue>) {
    let physmap = Arc::new(
        PhysMap::new_anon(DESC_GPA, 0x2000).expect("create queue memory"),
    );
    let mut queue = VirtQueue::new(RING_SIZE);
    queue.set_addr_modern(DESC_GPA, AVAIL_GPA, USED_GPA);
    physmap
        .lookup(AVAIL_GPA + 2, 2)
        .expect("mapped avail index")
        .write::<u16>(&1)
        .expect("write avail index");
    physmap
        .lookup(AVAIL_GPA + 4, 2)
        .expect("mapped avail entry")
        .write::<u16>(&0)
        .expect("write avail entry");
    (physmap, vec![queue])
}

/// A receive queue with EVENT_IDX on and every slot filled, which is
/// what Linux's virtio_vsock posts as soon as it probes.
fn rx_ring_full() -> (Arc<PhysMap>, Vec<VirtQueue>) {
    let physmap = Arc::new(
        PhysMap::new_anon(DESC_GPA, 0x2000).expect("create queue memory"),
    );
    let mut queue = VirtQueue::new(RING_SIZE);
    queue.set_addr_modern(DESC_GPA, AVAIL_GPA, USED_GPA);
    queue.set_event_idx(true);
    for i in 0..RING_SIZE {
        write_desc(
            &physmap,
            u64::from(i),
            BUF_GPA + u64::from(i) * 64,
            64,
            crate::bits::VRING_DESC_F_WRITE,
            0,
        );
        physmap
            .lookup(AVAIL_GPA + 4 + u64::from(i) * 2, 2)
            .expect("mapped avail entry")
            .write::<u16>(&i)
            .expect("write avail entry");
    }
    physmap
        .lookup(AVAIL_GPA + 2, 2)
        .expect("mapped avail index")
        .write::<u16>(&RING_SIZE)
        .expect("write avail index");
    (physmap, vec![queue])
}

/// A transmit queue with EVENT_IDX on and every slot filled. The
/// chains are device-readable, as a guest's outgoing packets are.
fn tx_ring_full() -> (Arc<PhysMap>, Vec<VirtQueue>) {
    let physmap = Arc::new(
        PhysMap::new_anon(DESC_GPA, 0x2000).expect("create queue memory"),
    );
    let mut queue = VirtQueue::new(RING_SIZE);
    queue.set_addr_modern(DESC_GPA, AVAIL_GPA, USED_GPA);
    queue.set_event_idx(true);
    for i in 0..RING_SIZE {
        write_desc(
            &physmap,
            u64::from(i),
            BUF_GPA + u64::from(i) * 64,
            64,
            0,
            0,
        );
        physmap
            .lookup(AVAIL_GPA + 4 + u64::from(i) * 2, 2)
            .expect("mapped avail entry")
            .write::<u16>(&i)
            .expect("write avail entry");
    }
    physmap
        .lookup(AVAIL_GPA + 2, 2)
        .expect("mapped avail index")
        .write::<u16>(&RING_SIZE)
        .expect("write avail index");
    // The transport indexes by queue number, so the transmit ring has
    // to sit at its own index.
    (physmap, vec![VirtQueue::new(RING_SIZE), queue])
}

/// The device's own "kick me past here" threshold, which lives at the
/// end of the used ring (VIRTIO 1.3 sec 2.7.7).
fn avail_event(physmap: &PhysMap) -> u16 {
    physmap
        .lookup(USED_GPA + 4 + u64::from(RING_SIZE) * 8, 2)
        .expect("mapped avail event")
        .read::<u16>()
        .expect("read avail event")
}

fn used_idx(physmap: &PhysMap) -> u16 {
    physmap
        .lookup(USED_GPA + 2, 2)
        .expect("mapped used index")
        .read::<u16>()
        .expect("read used index")
}

fn used_entry(physmap: &PhysMap) -> (u32, u32) {
    let sub = physmap.lookup(USED_GPA + 4, 8).expect("mapped used entry");
    let id = sub.read::<u32>().expect("read used id");
    let len = sub
        .subregion(4, 4)
        .expect("used length")
        .read::<u32>()
        .expect("read used length");
    (id, len)
}

/// A chain the walker refuses must still go back on the used ring.
/// `pop_avail` already took its head out of the available ring, so
/// dropping it burns one descriptor for the life of the device.
/// Enough of them and the ring runs dry and the guest stalls.
#[test]
fn a_refused_rx_chain_returns_its_descriptor() {
    let (physmap, mut queues) = rx_ring();
    // NEXT past the end of the table: the walker refuses this.
    write_desc(
        &physmap,
        0,
        BUF_GPA,
        64,
        crate::bits::VRING_DESC_F_NEXT | crate::bits::VRING_DESC_F_WRITE,
        RING_SIZE + 3,
    );
    let (dir, vsock) = test_vsock(Arc::clone(&physmap));

    vsock.notify_queue(VSOCK_RX_QUEUE, &mut queues, &physmap);

    assert_eq!(
        used_idx(&physmap),
        1,
        "the refused head never went back to the guest",
    );
    assert_eq!(used_entry(&physmap), (0, 0));
    assert!(vsock.shared.pending_rx.lock().expect("pending").is_empty());

    vsock.halt();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The opposite case: a chain that walks is held for a packet, not
/// completed. The guest gets it back once there is data in it.
#[test]
fn a_good_rx_chain_is_held_for_a_packet() {
    let (physmap, mut queues) = rx_ring();
    write_desc(&physmap, 0, BUF_GPA, 64, crate::bits::VRING_DESC_F_WRITE, 0);
    let (dir, vsock) = test_vsock(Arc::clone(&physmap));

    vsock.notify_queue(VSOCK_RX_QUEUE, &mut queues, &physmap);

    assert_eq!(used_idx(&physmap), 0, "an empty chain was completed");
    assert_eq!(vsock.shared.pending_rx.lock().expect("pending").len(), 1);

    vsock.halt();
    let _ = std::fs::remove_dir_all(&dir);
}

// -- Host peer bounds --

/// A peer that connects and sends nothing must not park a thread for
/// the life of the VM. The reader arms its own poll and reads the
/// shutdown flag on every pass; this connection is not in `sockets`
/// yet, so halt's socket sweep cannot wake it.
#[test]
fn a_silent_peer_does_not_park_the_connect_thread() {
    let (dir, vsock) = test_vsock(Arc::new(PhysMap::new()));
    let shared = Arc::clone(&vsock.shared);
    let (_peer, device_end) = UnixStream::pair().expect("socketpair");

    let reader =
        std::thread::spawn(move || handle_host_connect(shared, device_end));
    std::thread::sleep(Duration::from_millis(50));
    assert!(!reader.is_finished(), "the reader must still be waiting");

    vsock.shared.shutdown.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !reader.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        reader.is_finished(),
        "the reader never saw the shutdown flag"
    );
    reader.join().expect("join the reader");

    vsock.halt();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The reader table is only drained by halt, so without reaping a peer
/// that connects and disconnects in a loop grows it, and every handle
/// in it, for the life of the VM.
#[test]
fn a_finished_reader_is_reaped_when_the_next_one_starts() {
    let (dir, vsock) = test_vsock(Arc::new(PhysMap::new()));

    for _ in 0..64 {
        let handle = std::thread::spawn(|| {});
        vsock.shared.track_reader(handle);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        vsock.shared.track_reader(std::thread::spawn(|| {}));
        let live = vsock.shared.reader_count();
        if live <= 2 || Instant::now() >= deadline {
            assert!(live <= 2, "{live} handles were kept");
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    vsock.halt();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fill the `stream` -> peer direction, so the next write has nowhere
/// to put its bytes and `POLLOUT` stays clear.
///
/// The fill is itself bounded, so it ends on its own budget once the
/// socket stops taking bytes.
fn fill_send_buffer(stream: &UnixStream) {
    // Larger than any default socket buffer, so the write cannot
    // finish and leave room behind it.
    let oversized = vec![0u8; 8 * 1024 * 1024];
    write_all_bounded(stream, &oversized, HOST_WRITE_TIMEOUT)
        .expect_err("the peer reads nothing, so the socket must fill");
}

/// The OK line and the REQUEST cannot be reordered.
///
/// Once the REQUEST is queued, any thread's `deliver_rx` can hand it to
/// the guest, the guest can answer and send payload, and a vCPU can
/// then be inside `write_host` on this same socket. The handshake write
/// has to be finished by then, or the two writers interleave and break
/// the framing.
///
/// A peer that cannot take the OK line is how that order is read from
/// outside: if the write comes first, the guest is never told at all.
#[test]
fn a_host_peer_hears_ok_before_the_guest_hears_the_request() {
    use std::sync::mpsc;

    let (dir, vsock) = test_vsock(Arc::new(PhysMap::new()));
    // Held open to the end: a closed peer would end the write on a
    // broken pipe, which is not the case under test.
    let (peer, device_end) = UnixStream::pair().expect("socketpair");
    fill_send_buffer(&device_end);

    let shared = Arc::clone(&vsock.shared);
    let (done_tx, done_rx) = mpsc::channel();
    let serving = std::thread::spawn(move || {
        start_host_connection(shared, device_end, 9999);
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the handshake write must end on its budget");

    assert!(
        vsock.shared.mux.rx_is_empty(),
        "the guest was told about a connection whose peer never got its OK"
    );
    assert_eq!(
        vsock.shared.mux.conn_count(),
        0,
        "the slot was not released"
    );
    assert!(vsock
        .shared
        .sockets
        .lock()
        .expect("sockets lock")
        .is_empty());

    drop(peer);
    vsock.halt();
    serving.join().expect("join the connect thread");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The mux refuses a connection over the cap and registers nothing, so
/// the device has to close the peer rather than leave it holding a
/// socket and a thread for an answer that is never coming.
#[test]
fn a_host_connect_over_the_cap_is_closed() {
    use std::io::BufRead as _;

    let (dir, vsock) = test_vsock(Arc::new(PhysMap::new()));
    for port in 0..MAX_CONNS as u32 {
        assert!(vsock.shared.mux.start_host_connect(ConnKey {
            guest_port: port,
            host_port: 40_000 + port,
        }));
    }

    let (peer, device_end) = UnixStream::pair().expect("socketpair");
    // Armed before the call: macOS refuses the option once the far end
    // has closed. The call runs on its own thread, because a device
    // that ignored the refusal goes on to stream and never returns.
    peer.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("arm the read timeout");
    let shared = Arc::clone(&vsock.shared);
    let serving = std::thread::spawn(move || {
        start_host_connection(shared, device_end, 9999)
    });

    let mut answer = String::new();
    io::BufReader::new(peer)
        .read_line(&mut answer)
        .expect("read the refusal");
    assert!(answer.starts_with("ERROR"), "answered {answer:?}");
    assert_eq!(vsock.shared.mux.conn_count(), MAX_CONNS);
    assert!(vsock
        .shared
        .sockets
        .lock()
        .expect("sockets lock")
        .is_empty());

    vsock.halt();
    serving.join().expect("join the connect thread");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The halt budget has to bound the join itself. A deadline tested
/// before each `join()` only decides whether the next one starts, so
/// one wedged reader holds teardown open for as long as it stays
/// wedged.
#[test]
fn a_wedged_reader_does_not_hold_halt_open() {
    let (dir, vsock) = test_vsock(Arc::new(PhysMap::new()));
    let release = Arc::new(AtomicBool::new(false));
    let wedged = Arc::clone(&release);
    vsock.shared.track_reader(std::thread::spawn(move || {
        while !wedged.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }));

    // halt runs on its own thread: a budget that does not bound the
    // join blocks for ever, and this must report that rather than hang.
    let (done, ended) = std::sync::mpsc::channel();
    let halting = std::thread::spawn(move || {
        vsock.halt();
        let _ = done.send(());
        vsock
    });
    let ended = ended.recv_timeout(HALT_BUDGET * 3).is_ok();

    release.store(true, Ordering::Release);
    halting.join().expect("join the halt thread");
    assert!(ended, "halt did not return within its budget");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A host peer that stops reading must not hold the vCPU that is
/// writing to it. `write_host` runs with the transport lock held, so
/// an unbounded write there wedges every other vCPU that touches this
/// device, and halt behind them.
#[test]
fn a_host_peer_that_never_reads_does_not_hold_the_writer() {
    let (dir, vsock) = test_vsock(Arc::new(PhysMap::new()));
    let key = ConnKey {
        guest_port: 1024,
        host_port: 1,
    };
    // The peer end is never read from, so the socket buffer fills and
    // stays full for as long as this test holds it.
    let (_peer, device_end) = UnixStream::pair().expect("socketpair");
    vsock.shared.register_socket(key, device_end);

    let writer = {
        let shared = Arc::clone(&vsock.shared);
        std::thread::spawn(move || {
            shared.write_host(key, &vec![0u8; 8 * 1024 * 1024]);
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !writer.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        writer.is_finished(),
        "the write never gave up on a peer that stopped reading"
    );
    writer.join().expect("join the writer");
    assert!(
        !vsock
            .shared
            .sockets
            .lock()
            .expect("sockets lock")
            .contains_key(&key),
        "the wedged connection stayed in the table"
    );

    vsock.halt();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A peer that reads, but too slowly to take the payload inside the
/// budget, must lose its connection like one that reads nothing. The
/// write makes progress here, so a bound that only watched the first
/// blocked write would never fire.
#[test]
fn a_host_peer_that_reads_too_slowly_loses_its_connection() {
    let (dir, vsock) = test_vsock(Arc::new(PhysMap::new()));
    let key = ConnKey {
        guest_port: 1024,
        host_port: 2,
    };
    let (peer, device_end) = UnixStream::pair().expect("socketpair");
    vsock.shared.register_socket(key, device_end);

    // Small reads, spaced out, so the socket buffer frees a little at
    // a time and the write can never catch up with the payload.
    let taken = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&taken);
    let slow = std::thread::spawn(move || {
        let mut peer = peer;
        let mut buf = [0u8; 512];
        loop {
            std::thread::sleep(HOST_WRITE_TIMEOUT / 4);
            match peer.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => counted.fetch_add(n, Ordering::Release),
            };
        }
    });

    let writer = {
        let shared = Arc::clone(&vsock.shared);
        std::thread::spawn(move || {
            shared.write_host(key, &vec![0u8; 8 * 1024 * 1024]);
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !writer.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        writer.is_finished(),
        "the write never gave up on a peer that could not keep up"
    );
    writer.join().expect("join the writer");
    assert!(
        taken.load(Ordering::Acquire) > 0,
        "the peer took nothing, so this did not test a partial write"
    );
    assert!(
        !vsock
            .shared
            .sockets
            .lock()
            .expect("sockets lock")
            .contains_key(&key),
        "the connection the write gave up on stayed in the table"
    );
    slow.join().expect("join the slow reader");

    vsock.halt();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The reader slots are capped, so a peer that connects and never
/// sends its verb must give the slot back on its own. Waiting on the
/// shutdown flag only frees them when the VM is already going away.
#[test]
fn a_silent_peer_gives_its_connect_thread_back() {
    let (dir, vsock) = test_vsock(Arc::new(PhysMap::new()));
    let shared = Arc::clone(&vsock.shared);
    let (_peer, device_end) = UnixStream::pair().expect("socketpair");

    let reader =
        std::thread::spawn(move || handle_host_connect(shared, device_end));
    let deadline = Instant::now() + HANDSHAKE_BUDGET * 4;
    while !reader.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        reader.is_finished(),
        "a peer that sent nothing held its thread past the handshake budget"
    );
    reader.join().expect("join the reader");

    vsock.halt();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Payload for a connection the mux knows but the table has no socket
/// for must not vanish. The guest still believes the connection is
/// live, so it would read a byte stream with a hole in it and nothing
/// would say so.
#[test]
fn payload_with_no_host_socket_tells_the_guest() {
    use super::super::packet::OP_SHUTDOWN;

    let (dir, vsock) = test_vsock(Arc::new(PhysMap::new()));
    let key = ConnKey {
        guest_port: 1024,
        host_port: 1,
    };
    assert!(vsock.shared.mux.start_host_connect(key));
    // Drop the REQUEST the mux queued, so only what the write below
    // queues is left to read.
    while vsock.shared.mux.pop_rx().is_some() {}

    vsock.shared.write_host(key, b"lost");

    let packet = vsock
        .shared
        .mux
        .pop_rx()
        .expect("the guest was told nothing about the lost payload");
    assert_eq!(packet.hdr.op, OP_SHUTDOWN);

    vsock.halt();
    let _ = std::fs::remove_dir_all(&dir);
}

/// What the guest's receive buffer holds now.
fn rx_buf(physmap: &PhysMap) -> Vec<u8> {
    let mut out = vec![0u8; 64];
    physmap
        .lookup(BUF_GPA, out.len())
        .expect("mapped rx buffer")
        .read_bytes(&mut out)
        .expect("read rx buffer");
    out
}

/// Delivery runs on reader and accept threads, outside the transport's
/// register lock, so a reset can land between the pop of a chain and
/// the write into it. The illumos legacy driver frees the vring on the
/// status write, so a late write lands in whatever the guest has put
/// in those pages since. A delivery therefore needs the reset's
/// permission, exactly as virtio-fs's worker does.
#[test]
fn a_delivery_without_the_resets_permission_writes_nothing() {
    const SENTINEL: u8 = 0x5a;

    let (physmap, mut queues) = rx_ring();
    write_desc(&physmap, 0, BUF_GPA, 64, crate::bits::VRING_DESC_F_WRITE, 0);
    let (dir, vsock) = test_vsock(Arc::clone(&physmap));
    physmap
        .lookup(BUF_GPA, 64)
        .expect("mapped rx buffer")
        .write_bytes(&[SENTINEL; 64])
        .expect("seed the rx buffer");

    // The chain is posted under the transport lock, before the reset.
    vsock.notify_queue(VSOCK_RX_QUEUE, &mut queues, &physmap);
    assert_eq!(vsock.shared.pending_rx.lock().expect("pending").len(), 1);

    let key = ConnKey {
        guest_port: 1024,
        host_port: 1,
    };
    assert!(vsock.shared.mux.start_host_connect(key));

    // Admission shut, the way a reset shuts it before its drain.
    vsock.shared.access.close_all();
    vsock.shared.deliver_rx();

    assert_eq!(
        used_idx(&physmap),
        0,
        "a delivery published into the ring without permission",
    );
    assert_eq!(
        rx_buf(&physmap),
        vec![SENTINEL; 64],
        "a delivery wrote into the chain without permission",
    );
    assert!(!vsock.shared.mux.rx_is_empty(), "the packet was consumed");

    // The other half: the driver that follows must still be served, or
    // the device has traded a stale write for a permanent stall.
    vsock
        .shared
        .access
        .reopen(vsock.shared.access.intr_session());
    vsock.shared.deliver_rx();
    assert_eq!(used_idx(&physmap), 1, "the running driver got nothing");

    vsock.halt();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The other side of the guard: the reset has to wait for a delivery
/// that was already inside, or it returns while a write into the ring
/// is still running and the driver frees the pages under it.
#[test]
fn a_reset_waits_out_a_delivery_already_inside() {
    let (physmap, _queues) = rx_ring();
    let (dir, vsock) = test_vsock(Arc::clone(&physmap));

    let session = vsock
        .shared
        .access
        .enter_current(VSOCK_RX_QUEUE)
        .expect("a session is open");

    let finished = Arc::new(AtomicBool::new(false));
    let done = Arc::clone(&finished);
    let device = Arc::clone(&vsock.shared);
    let resetter = std::thread::spawn(move || {
        device.access.close_all();
        device.access.drain();
        done.store(true, Ordering::Release);
        device.access.reopen(device.access.intr_session());
    });

    // Admission shuts first, so this is the reset inside its drain and
    // nowhere else.
    let deadline = Instant::now() + Duration::from_secs(5);
    while vsock.shared.access.is_open() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!vsock.shared.access.is_open(), "the reset never started");
    assert!(
        !finished.load(Ordering::Acquire),
        "the reset returned while a delivery still held the ring",
    );

    drop(session);
    resetter.join().expect("reset thread");
    assert!(finished.load(Ordering::Acquire));

    vsock.halt();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A driver may program the receive ring again without resetting the
/// device, and it frees the old ring as soon as it has. A delivery
/// already inside `pair_rx` holds the chain it took off the stash and
/// the writer that names the old ring, so dropping the cached ones
/// does not reach it: the reprogram has to wait it out.
#[test]
fn a_reprogrammed_rx_ring_waits_out_a_delivery_already_inside() {
    const SENTINEL: u8 = 0x5a;
    // Free, mapped ring memory past the fixture's own.
    const NEW_DESC_GPA: u64 = 0x1900;
    const NEW_AVAIL_GPA: u64 = 0x1A00;
    const NEW_USED_GPA: u64 = 0x1B00;

    let (physmap, mut queues) = rx_ring();
    write_desc(&physmap, 0, BUF_GPA, 64, crate::bits::VRING_DESC_F_WRITE, 0);
    let (dir, vsock) = test_vsock(Arc::clone(&physmap));
    physmap
        .lookup(BUF_GPA, 64)
        .expect("mapped rx buffer")
        .write_bytes(&[SENTINEL; 64])
        .expect("seed the rx buffer");

    // A chain on the stash and a packet waiting for it: what a
    // delivery inside `pair_rx` holds.
    vsock.notify_queue(VSOCK_RX_QUEUE, &mut queues, &physmap);
    assert_eq!(vsock.shared.pending_rx.lock().expect("pending").len(), 1);
    let key = ConnKey {
        guest_port: 1024,
        host_port: 1,
    };
    assert!(vsock.shared.mux.start_host_connect(key));

    let mut reprogrammed = VirtQueue::new(RING_SIZE);
    reprogrammed.set_addr_modern(NEW_DESC_GPA, NEW_AVAIL_GPA, NEW_USED_GPA);

    let held = vsock
        .shared
        .access
        .enter_current(VSOCK_RX_QUEUE)
        .expect("a delivery is admitted");
    let finished = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            vsock.queue_addr_set(VSOCK_RX_QUEUE, &reprogrammed);
            finished.store(true, Ordering::Release);
        });

        // Admission shuts first, so past this the reprogram is inside
        // its drain and nowhere else.
        let deadline = Instant::now() + Duration::from_secs(5);
        while vsock.shared.access.is_open() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            !vsock.shared.access.is_open(),
            "the reprogram never took the ring back"
        );
        assert!(
            !finished.load(Ordering::Acquire),
            "the reprogram returned while a delivery still held the old ring",
        );
        drop(held);
    });
    assert!(finished.load(Ordering::Acquire));

    // The packet is still owed, and a delivery runs again now that
    // admission is back. Nothing of it may reach the old ring.
    vsock.shared.deliver_rx();
    assert_eq!(
        used_idx(&physmap),
        0,
        "a delivery published into the ring the driver gave up",
    );
    assert_eq!(
        rx_buf(&physmap),
        vec![SENTINEL; 64],
        "a delivery wrote into the chain the driver gave up",
    );

    vsock.halt();
    let _ = std::fs::remove_dir_all(&dir);
}

/// CONTROL shares the reader table with every CONNECT, and an
/// established session is allowed to idle, so a few idle CONTROL peers
/// could take every slot and leave the operator unable to connect at
/// all. The verb has a quota of its own.
#[test]
fn control_sessions_are_held_to_their_own_quota() {
    use crate::vsock::control::MAX_CONTROL_SESSIONS;
    use std::io::Write as _;

    let (dir, vsock) = test_vsock(Arc::new(PhysMap::new()));
    let held: Vec<_> = (0..MAX_CONTROL_SESSIONS)
        .map(|_| {
            vsock
                .shared
                .reserve_control_session()
                .expect("a free control slot")
        })
        .collect();

    let (mut peer, device_end) = UnixStream::pair().expect("socketpair");
    peer.write_all(b"CONTROL\n").expect("send the verb");
    handle_host_connect(Arc::clone(&vsock.shared), device_end);

    let mut answer = String::new();
    io::BufReader::new(&peer)
        .read_line(&mut answer)
        .expect("read the refusal");
    assert_eq!(answer.trim_end(), "ERR too many control sessions");

    // A slot given back admits the next session.
    drop(held);
    assert!(vsock.shared.reserve_control_session().is_some());

    vsock.halt();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Host accepts are held to the reader table, and so are the guest's
/// own connections: they cost the same thread and the same 64 KiB
/// buffer. A guest allowed past it doubles the ceiling the device
/// claims, and the refusal has to come before the dial, so nothing is
/// opened on the host that cannot be served.
#[test]
fn a_guest_connect_is_held_to_the_reader_table() {
    use crate::vsock::packet::{
        VsockHdr, HDR_SIZE, OP_REQUEST, OP_RST, TYPE_STREAM,
    };
    use std::os::unix::net::UnixListener;

    const GUEST_PORT: u32 = 1024;
    const HOST_PORT: u32 = 5000;

    let hdr = VsockHdr {
        src_cid: 3,
        dst_cid: crate::vsock::packet::CID_HOST,
        src_port: GUEST_PORT,
        dst_port: HOST_PORT,
        len: 0,
        type_: TYPE_STREAM,
        op: OP_REQUEST,
        flags: 0,
        buf_alloc: 64 * 1024,
        fwd_cnt: 0,
    };

    let physmap = Arc::new(
        PhysMap::new_anon(DESC_GPA, 0x2000).expect("create queue memory"),
    );
    let mut queue = VirtQueue::new(RING_SIZE);
    queue.set_addr_modern(DESC_GPA, AVAIL_GPA, USED_GPA);
    write_desc(&physmap, 0, BUF_GPA, HDR_SIZE as u32, 0, 0);
    physmap
        .lookup(BUF_GPA, HDR_SIZE)
        .expect("mapped packet")
        .write_bytes(&hdr.to_bytes())
        .expect("seed the packet");
    physmap
        .lookup(AVAIL_GPA + 4, 2)
        .expect("mapped avail entry")
        .write::<u16>(&0)
        .expect("write avail entry");
    physmap
        .lookup(AVAIL_GPA + 2, 2)
        .expect("mapped avail index")
        .write::<u16>(&1)
        .expect("write avail index");
    let mut queues = vec![VirtQueue::new(RING_SIZE), queue];

    let (dir, vsock) = test_vsock(Arc::clone(&physmap));
    // A host listener the dial would reach, so a refusal can only come
    // from the cap.
    let port_path = PathBuf::from(format!(
        "{}_{}",
        vsock.socket_path().display(),
        HOST_PORT
    ));
    let host = UnixListener::bind(&port_path).expect("bind the host port");
    host.set_nonblocking(true).expect("non-blocking listener");

    // Every slot promised to a thread, as a full table is.
    let slots: Vec<_> = (0..MAX_CONNS)
        .map(|_| vsock.shared.reserve_reader().expect("a free slot"))
        .collect();

    vsock.notify_queue(VSOCK_TX_QUEUE, &mut queues, &physmap);

    let key = ConnKey {
        guest_port: GUEST_PORT,
        host_port: HOST_PORT,
    };
    assert_eq!(
        vsock.shared.mux.conn_count(),
        0,
        "a connection was kept with no reader to serve it",
    );
    assert!(
        !vsock
            .shared
            .sockets
            .lock()
            .expect("sockets")
            .contains_key(&key),
        "a host socket was registered past the reader cap",
    );
    let refusal = vsock
        .shared
        .mux
        .pop_rx()
        .expect("the guest was told nothing");
    assert_eq!(refusal.hdr.op, OP_RST);
    assert!(
        matches!(host.accept(), Err(e) if e.kind() == io::ErrorKind::WouldBlock),
        "the host port was dialled before the cap was tested",
    );

    drop(slots);
    vsock.halt();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A reader wakes when its own socket closes, and the guest may by then
/// have closed that connection and opened another under the same key:
/// it picks both ports, so a program that re-dials from a fixed local
/// port reuses the key at once. A reader that tore the key down would
/// kill the connection the guest had just opened.
#[test]
fn a_readers_exit_leaves_a_connection_that_reused_its_key_alone() {
    let (dir, vsock) = test_vsock(Arc::new(PhysMap::new()));
    let key = ConnKey {
        guest_port: 1024,
        host_port: 1,
    };

    // The connection this reader serves.
    let (peer, device_end) = UnixStream::pair().expect("socketpair");
    assert!(vsock.shared.mux.start_host_connect(key));
    let served = vsock.shared.register_socket(key, device_end);

    // The guest closes it, then opens the same key again.
    vsock.shared.mux.forget(key);
    vsock.shared.drop_socket(key);
    let (_next_peer, next_end) = UnixStream::pair().expect("socketpair");
    assert!(vsock.shared.mux.start_host_connect(key));
    vsock.shared.register_socket(key, next_end);
    while vsock.shared.mux.pop_rx().is_some() {}

    // Only now does the old reader wake and run its exit.
    drop(peer);
    reader_loop(Arc::clone(&vsock.shared), key, served);

    assert_eq!(
        vsock.shared.mux.conn_count(),
        1,
        "the old reader forgot the connection that took its key",
    );
    assert!(
        vsock.shared.mux.pop_rx().is_none(),
        "the guest was told the connection it just opened had gone",
    );
    assert!(
        vsock
            .shared
            .sockets
            .lock()
            .expect("sockets")
            .contains_key(&key),
        "the old reader dropped the new connection's socket",
    );

    vsock.halt();
    let _ = std::fs::remove_dir_all(&dir);
}

// The RX handler and the transport session are two pieces of state a
// reset changes one after the other, and a socket thread can be
// between them. A packet delivered on the handler the reset dropped
// must not interrupt the driver that followed.
#[test]
fn an_rx_completion_from_the_closed_session_raises_nothing() {
    let (physmap, queues) = rx_ring();
    let (dir, vsock) = test_vsock(Arc::clone(&physmap));
    let gate = Arc::new(IntrGate::new());
    let seen = Arc::new(Mutex::new(Vec::new()));
    vsock
        .shared
        .interrupt
        .install(BackendIntr::recording(Arc::clone(&gate), Arc::clone(&seen)));

    let stale = vsock.ensure_rx_completion(&queues[0]);

    // The whole reset, in the order the transport runs it: the session
    // ends first, then the backend resets, then admission reopens.
    gate.end_session();
    VirtioDevice::reset(&vsock);
    gate.reopen();

    stale.signal();
    assert!(
        seen.lock().expect("record lock").is_empty(),
        "a packet of the closed session interrupted the driver that followed"
    );

    // The other half, and it is the worse failure of the two: a packet
    // of the session running now has to get through, or the device has
    // traded a spurious interrupt for a guest that waits for ever.
    vsock.ensure_rx_completion(&queues[0]).signal();
    assert_eq!(
        seen.lock().expect("record lock").len(),
        1,
        "the driver that is running got no interrupt for its packet"
    );

    vsock.halt();
    let _ = std::fs::remove_dir_all(&dir);
}

// The halt joins the accept thread and then the readers, one after
// the other, so the deadline it holds is the product and not the
// constant. Teardown derives its own bound from what this reports; a
// device that under-reports is cut off inside its own wait.
#[test]
fn the_declared_halt_budget_covers_both_joins_the_halt_runs() {
    let (dir, vsock) = test_vsock(Arc::new(PhysMap::new()));
    assert_eq!(
        Lifecycle::halt_budget(&vsock),
        HALT_BUDGET * HALT_JOIN_ROUNDS,
    );
    assert!(
        Lifecycle::halt_budget(&vsock) > HALT_BUDGET,
        "one join's budget does not cover two joins run in sequence",
    );
    vsock.halt();
    let _ = std::fs::remove_dir_all(&dir);
}

// -- The accept loop and the teardown it allows --

/// The accept thread must end on the shutdown flag alone.
///
/// It used to be woken by halt connecting to the device's own socket.
/// That connect has no bound of its own: a same-uid peer can fill the
/// listen backlog and make it wait for as long as it likes, and the
/// teardown sweep budget was the only cap on it. Taking the socket
/// away first is how that is read from outside. Nothing can connect to
/// this path any more, so a halt that still needed a connection to
/// wake the thread pays its whole join budget and leaves it running.
#[test]
fn halt_ends_the_accept_thread_with_no_connection_to_the_socket() {
    let (dir, vsock) = test_vsock(Arc::new(PhysMap::new()));
    std::fs::remove_file(vsock.socket_path()).expect("remove the socket");

    let started = Instant::now();
    vsock.halt();
    let took = started.elapsed();

    // `join_bounded` returns early only when the thread has ended, so
    // the time it took is what says whether it did.
    assert!(
        took < HALT_BUDGET,
        "halt spent {took:?} of its {HALT_BUDGET:?} join budget waiting \
         for an accept thread nothing could wake",
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// An accepted socket must not keep the listener's non-blocking flag.
///
/// The listener carries the flag so the accept loop can poll, and on
/// BSD and illumos an accepted socket inherits it. Every reader here
/// arms a read timeout and reads `WouldBlock` as an idle poll, so a
/// socket that kept the flag would spin a thread at full speed instead
/// of waiting for its peer.
#[test]
fn an_accepted_socket_does_not_keep_the_non_blocking_flag() {
    let (_peer, inherited) = UnixStream::pair().expect("socketpair");
    inherited.set_nonblocking(true).expect("arm the flag");

    let served = accepted_blocking(inherited).expect("the socket is usable");

    let flags = unsafe { libc::fcntl(served.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0, "F_GETFL failed");
    assert_eq!(
        flags & libc::O_NONBLOCK,
        0,
        "a reader on this socket would spin instead of waiting",
    );
}

/// The polling accept loop still serves a client.
///
/// A poll that never reaches `accept`, or an accepted socket the
/// handshake cannot read, leaves every host client unanswered with
/// nothing else in the device failing.
#[test]
fn the_accept_loop_answers_a_host_connect() {
    use std::io::{BufRead as _, Write as _};

    let (dir, vsock) = test_vsock(Arc::new(PhysMap::new()));
    let mut client = UnixStream::connect(vsock.socket_path()).expect("connect");
    // Armed so a loop that never accepts fails here rather than hangs.
    client
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("arm the read timeout");
    client.write_all(b"CONNECT 1024\n").expect("send the verb");

    let mut answer = String::new();
    io::BufReader::new(client)
        .read_line(&mut answer)
        .expect("read the answer");
    assert!(answer.starts_with("OK "), "answered {answer:?}");

    vsock.halt();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The reader thread is the only production caller of
/// `queue_host_data`, and the contract it has to keep is to offer one
/// buffer until the mux has emptied it. This drives the real thread
/// with a window far smaller than the data, so the offer takes many
/// passes, and compares the guest's stream with the source byte for
/// byte.
///
/// The mux tests state the same property against a loop written in the
/// test. This states it against the loop that ships. Kills the
/// mutation of leaving the loop after one pass, which truncates, and
/// would kill a caller that rebuilt its buffer between passes, which
/// duplicates.
#[test]
fn the_reader_offers_every_byte_once_through_a_small_window() {
    use crate::vsock::packet::{
        VsockHdr, CID_HOST, OP_CREDIT_UPDATE, OP_RESPONSE, OP_RW, TYPE_STREAM,
    };
    use std::io::Write as _;
    use std::net::Shutdown;

    const GUEST_CID: u64 = 3;
    const WINDOW: u32 = 8 * 1024;

    let (dir, vsock) = test_vsock(Arc::new(PhysMap::new()));
    let key = ConnKey {
        guest_port: 1024,
        host_port: 50000,
    };
    let guest_hdr = |op: u16, fwd_cnt: u32| VsockHdr {
        src_cid: GUEST_CID,
        dst_cid: CID_HOST,
        src_port: key.guest_port,
        dst_port: key.host_port,
        len: 0,
        type_: TYPE_STREAM,
        op,
        flags: 0,
        buf_alloc: WINDOW,
        fwd_cnt,
    };

    // A connection the guest has answered, naming a window smaller
    // than one read from the host socket.
    assert!(vsock.shared.mux.start_host_connect(key));
    vsock
        .shared
        .mux
        .on_guest_packet(&guest_hdr(OP_RESPONSE, 0), vec![]);
    while vsock.shared.mux.pop_rx().is_some() {}

    let source: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
    let (peer, device_end) = UnixStream::pair().expect("socketpair");

    let reader = {
        let shared = Arc::clone(&vsock.shared);
        std::thread::spawn(move || {
            reader_loop(shared, key, Arc::new(device_end))
        })
    };
    // The host peer writes on its own thread: the socket buffer fills
    // long before the guest's window has taken the whole stream.
    let writer = {
        let source = source.clone();
        std::thread::spawn(move || {
            let mut peer = peer;
            peer.write_all(&source).expect("write the source");
            peer.shutdown(Shutdown::Write).expect("half close");
            peer
        })
    };

    // The guest: take the packets and say what has been read, which is
    // the only thing that reopens the window.
    let mut got: Vec<u8> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    while got.len() < source.len() && Instant::now() < deadline {
        let mut moved = false;
        while let Some(pkt) = vsock.shared.mux.pop_rx() {
            if pkt.hdr.op == OP_RW {
                assert_eq!(
                    pkt.hdr.len as usize,
                    pkt.payload.len(),
                    "len is a lie"
                );
                assert!(
                    pkt.payload.len() <= WINDOW as usize,
                    "sent past the guest's window"
                );
                got.extend_from_slice(&pkt.payload);
                moved = true;
            }
        }
        if moved {
            vsock.shared.mux.on_guest_packet(
                &guest_hdr(OP_CREDIT_UPDATE, got.len() as u32),
                vec![],
            );
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    assert_eq!(got.len(), source.len(), "the guest saw a different count");
    assert_eq!(got, source, "the guest read a different stream");

    let _peer = writer.join().expect("join the host peer");
    reader.join().expect("join the reader");
    vsock.halt();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Taking the guest's whole ring in one pass must still arm the kick.
///
/// `avail_event` is how the device asks to be told about new chains.
/// A guest with EVENT_IDX reads it and stays quiet while it is behind
/// the index the guest is adding at, so a device that leaves it
/// unarmed is never kicked again. It then spends the chains it already
/// holds and goes deaf: packets queue for a guest that cannot be
/// given them, and every later connection hangs, because a RESPONSE
/// needs a chain like any other packet.
///
/// Linux fills the whole ring the moment it probes, so this is the
/// first pass, not a corner. Fails if the kick is armed only when the
/// pass stopped short of the request cap.
#[test]
fn a_ring_taken_whole_still_arms_the_next_kick() {
    let (physmap, mut queues) = rx_ring_full();
    let (dir, vsock) = test_vsock(Arc::clone(&physmap));

    vsock.notify_queue(VSOCK_RX_QUEUE, &mut queues, &physmap);

    assert_eq!(
        vsock.shared.pending_rx.lock().expect("pending").len(),
        usize::from(RING_SIZE),
        "the whole ring should be stashed",
    );
    assert_eq!(
        avail_event(&physmap),
        RING_SIZE,
        "avail_event left behind the ring: the guest will never kick again",
    );

    vsock.halt();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The transmit ring has the same trap as the receive ring: nothing
/// returns to it without a guest kick, so a pass that stops on the
/// request cap must still arm `avail_event` or the guest goes quiet
/// and the device never sees the rest of the ring.
#[test]
fn a_transmit_ring_taken_whole_still_arms_the_next_kick() {
    let (physmap, mut queues) = tx_ring_full();
    let (dir, vsock) = test_vsock(Arc::clone(&physmap));

    vsock.notify_queue(VSOCK_TX_QUEUE, &mut queues, &physmap);

    assert_eq!(
        used_idx(&physmap),
        RING_SIZE,
        "the whole ring should have been consumed",
    );
    assert_eq!(
        avail_event(&physmap),
        RING_SIZE,
        "avail_event left behind the ring: the guest will never kick again",
    );

    vsock.halt();
    let _ = std::fs::remove_dir_all(&dir);
}
