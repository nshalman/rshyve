// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unit tests for the virtio-fs device.
//!
//! The transport publishes DEVICE_STATUS 0 as soon as `reset` returns,
//! and the driver frees the vring on reading that 0, so "`reset` has
//! not returned" and "the guest has not been told the reset succeeded"
//! are the same statement.

use super::*;

use std::sync::Condvar;
use std::time::Instant;

use crate::queue::VirtqDesc;
use vmm_devices::Lifecycle;

/// Deadline on every wait a parked backend can be caught in. A device
/// that wrongly waits on the backing store fails the test after this
/// instead of hanging the run. Long enough that a loaded runner cannot
/// reach it, because only a real fault should.
const PARK_BUDGET: Duration = Duration::from_secs(10);

/// Longer than the two second budget the old drain gave up after. A
/// reset that returns inside this window has a deadline again.
const PAST_THE_OLD_DRAIN_BUDGET: Duration = Duration::from_millis(2500);

/// Take the shared side of `ring_access` the way the worker holds it
/// across a copy into or out of a chain.
fn hold_ring_access(fs: &VirtioFs) -> std::sync::RwLockReadGuard<'_, ()> {
    fs.ring_access.read().expect("ring access")
}

#[derive(Default)]
struct ParkState {
    /// The worker is inside the stand-in call.
    inside: bool,
    /// The test has let it out.
    released: bool,
    /// The stand-in call has returned.
    left: bool,
}

/// Stands in for a backing-store call that has not returned.
///
/// The device runs this where `FuseServer::run` goes, so it holds no
/// guest access while it waits. Every wait here has a deadline, so a
/// reset that blocks on the backing store fails a test rather than
/// wedging the run.
#[derive(Default)]
struct ParkedBackend {
    state: Mutex<ParkState>,
    signal: Condvar,
}

impl ParkedBackend {
    /// Arm the stand-in on `fs`. It answers the next request only.
    fn install(fs: &VirtioFs) -> Arc<Self> {
        let this = Arc::new(Self::default());
        let hook = Arc::clone(&this);
        *fs.parks.in_backend.lock().expect("park lock") =
            Some(Arc::new(move || hook.park()));
        this
    }

    fn park(&self) {
        let mut state = self.state.lock().expect("park state");
        state.inside = true;
        self.signal.notify_all();
        while !state.released {
            let (next, timed_out) = self
                .signal
                .wait_timeout(state, PARK_BUDGET)
                .expect("park state");
            state = next;
            if timed_out.timed_out() {
                break;
            }
        }
        state.left = true;
        self.signal.notify_all();
    }

    /// Block until the worker is inside the stand-in call.
    fn wait_until_inside(&self) {
        let mut state = self.state.lock().expect("park state");
        while !state.inside {
            let (next, timed_out) = self
                .signal
                .wait_timeout(state, PARK_BUDGET)
                .expect("park state");
            state = next;
            assert!(
                !timed_out.timed_out(),
                "the worker never reached the backing store"
            );
        }
    }

    /// Whether the stand-in call is still running.
    fn still_running(&self) -> bool {
        !self.state.lock().expect("park state").left
    }

    /// Let the stand-in call return, and wait until it has.
    fn release(&self) {
        let mut state = self.state.lock().expect("park state");
        state.released = true;
        self.signal.notify_all();
        while !state.left {
            let (next, timed_out) = self
                .signal
                .wait_timeout(state, PARK_BUDGET)
                .expect("park state");
            state = next;
            assert!(!timed_out.timed_out(), "the backing store never returned");
        }
    }
}

/// Park the worker and block until it is there.
///
/// Past this point the worker cannot observe anything the test does
/// until it is resumed, so an assertion between the two is about the
/// caller alone.
fn park_the_worker(fs: &VirtioFs) {
    fs.pause();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fs.is_quiesced() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        fs.is_quiesced(),
        "the worker did not park within its budget"
    );
}

/// Block until the worker owes the guest nothing.
fn wait_until_idle(fs: &VirtioFs) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while fs.inflight.load(Ordering::Acquire) != 0 && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        fs.inflight.load(Ordering::Acquire),
        0,
        "the worker never finished with the request"
    );
}

/// Block until the reset thread is inside the `ring_access` wait.
///
/// The epoch moves before the reset reaches the barrier and the caller
/// holds the shared side, so past this point the reset is in the wait
/// and can be nowhere else. Sleeping a fixed time instead would let the
/// assertions run after the reset had already finished.
fn wait_until_reset_blocks(
    fs: &VirtioFs,
    epoch_before: u64,
    finished: &AtomicBool,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while fs.epoch.load(Ordering::Acquire) == epoch_before
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_ne!(
        fs.epoch.load(Ordering::Acquire),
        epoch_before,
        "the reset never started"
    );
    assert!(
        !finished.load(Ordering::Acquire),
        "the reset finished while the worker still held ring access"
    );
}

const DESC_GPA: u64 = 0x1000;
const AVAIL_GPA: u64 = 0x1100;
const USED_GPA: u64 = 0x1200;
const REQ_GPA: u64 = 0x2000;
const RESP_GPA: u64 = 0x2800;
const RESP_LEN: usize = 256;
/// Byte the reply window carries before a reply can reach it.
const REPLY_SENTINEL: u8 = 0xA5;

fn test_logger() -> slog::Logger {
    slog::Logger::root(slog::Discard, slog::o!())
}

fn test_fs(physmap: Arc<PhysMap>) -> VirtioFs {
    let opts = VirtioFsOpts {
        tag: "shared".to_string(),
        path: std::env::temp_dir(),
        read_only: true,
        queue_size: FS_QUEUE_SIZE_DEFAULT,
    };
    VirtioFs::new(&opts, physmap, test_logger()).expect("open export")
}

/// Build a two-descriptor chain and an avail ring that publishes it
/// `avail_idx` times, mirroring `block.rs`'s ring fixture.
fn ring(
    queue_size: u16,
    avail_idx: u16,
) -> (Arc<PhysMap>, [VirtQueue; FS_NUM_QUEUES], VirtioFs) {
    let physmap = Arc::new(
        PhysMap::new_anon(DESC_GPA, 0x3000).expect("create queue memory"),
    );
    let mut queue = VirtQueue::new(queue_size);
    queue.set_addr_modern(DESC_GPA, AVAIL_GPA, USED_GPA);
    queue.set_event_idx(true);

    physmap
        .lookup(DESC_GPA, 16)
        .expect("mapped request descriptor")
        .write(&VirtqDesc {
            addr: REQ_GPA,
            len: 64,
            flags: crate::bits::VRING_DESC_F_NEXT,
            next: 1,
        })
        .expect("write request descriptor");
    physmap
        .lookup(DESC_GPA + 16, 16)
        .expect("mapped reply descriptor")
        .write(&VirtqDesc {
            addr: RESP_GPA,
            len: RESP_LEN as u32,
            flags: crate::bits::VRING_DESC_F_WRITE,
            next: 0,
        })
        .expect("write reply descriptor");

    physmap
        .lookup(AVAIL_GPA + 2, 2)
        .expect("mapped avail index")
        .write::<u16>(&avail_idx)
        .expect("write avail index");
    for idx in 0..queue_size {
        physmap
            .lookup(AVAIL_GPA + 4 + u64::from(idx) * 2, 2)
            .expect("mapped avail entry")
            .write::<u16>(&0)
            .expect("write avail entry");
    }

    let fs = test_fs(Arc::clone(&physmap));
    // Queue 0 is hiprio and stays unconfigured; the tests drive the
    // request queue at the index the transport uses.
    (physmap, [VirtQueue::new(queue_size), queue], fs)
}

/// Write a FUSE_INIT into the chain's readable segment, so the reply
/// the worker would publish has a body worth catching.
fn seed_fuse_init(physmap: &PhysMap) {
    let hdr = fuse::FuseInHeader {
        len: (fuse::FUSE_IN_HEADER_SIZE + size_of::<fuse::FuseInitIn>()) as u32,
        opcode: fuse::FUSE_INIT,
        unique: 1,
        nodeid: 0,
        uid: 0,
        gid: 0,
        pid: 0,
        padding: 0,
    };
    let mut buf = fuse::bytes_of(&hdr);
    fuse::push_val(
        &mut buf,
        &fuse::FuseInitIn {
            major: 7,
            minor: 33,
            max_readahead: 0x2_0000,
            flags: 0,
        },
    );
    physmap
        .lookup(REQ_GPA, buf.len())
        .expect("mapped request")
        .write_bytes(&buf)
        .expect("seed the request");
}

/// Fill the chain's writable segment with a sentinel, so a reply that
/// lands there after a reset is visible.
fn seed_reply_window(physmap: &PhysMap) {
    physmap
        .lookup(RESP_GPA, RESP_LEN)
        .expect("mapped reply window")
        .write_bytes(&[REPLY_SENTINEL; RESP_LEN])
        .expect("seed the reply window");
}

fn reply_window(physmap: &PhysMap) -> Vec<u8> {
    let mut out = vec![0u8; RESP_LEN];
    physmap
        .lookup(RESP_GPA, RESP_LEN)
        .expect("mapped reply window")
        .read_bytes(&mut out)
        .expect("read the reply window");
    out
}

fn avail_event(physmap: &PhysMap, queue: &VirtQueue) -> u16 {
    let gpa = queue.used_addr() + 4 + u64::from(queue.size()) * 8;
    physmap
        .lookup(gpa, 2)
        .expect("mapped avail event")
        .read::<u16>()
        .expect("read avail event")
}

#[test]
fn config_requires_a_path() {
    let err = parse_fs_config("").expect_err("empty config");
    assert_eq!(err.to_string(), "virtio-fs requires a host directory path");
}

#[test]
fn tag_defaults_to_the_last_path_component() {
    let opts = parse_fs_config("/export/shared").expect("parse");
    assert_eq!(opts.tag, "shared");
    assert_eq!(opts.path, std::path::PathBuf::from("/export/shared"));
    assert!(!opts.read_only);
    assert_eq!(opts.queue_size, FS_QUEUE_SIZE_DEFAULT);
}

#[test]
fn options_are_parsed() {
    let opts =
        parse_fs_config("/export/x,tag=data,ro,queue-size=256").expect("parse");
    assert_eq!(opts.tag, "data");
    assert!(opts.read_only);
    assert_eq!(opts.queue_size, 256);
}

#[test]
fn oversized_tag_is_rejected() {
    let tag = "t".repeat(FS_TAG_LEN + 1);
    let err =
        parse_fs_config(&format!("/export/x,tag={tag}")).expect_err("long tag");
    assert_eq!(
        err.to_string(),
        format!("virtio-fs tag '{tag}' exceeds {FS_TAG_LEN} bytes")
    );
}

#[test]
fn out_of_range_queue_size_is_rejected() {
    let err = parse_fs_config("/export/x,queue-size=4").expect_err("too small");
    assert_eq!(
        err.to_string(),
        "virtio-fs queue-size must be 8..=1024, got 4"
    );
}

// virtio-fs is new, so there is no forward compatibility to preserve
// and an unknown option is a hard error, unlike parse_blk_config.
#[test]
fn unknown_option_is_rejected() {
    let err = parse_fs_config("/export/x,dax").expect_err("unknown");
    assert_eq!(err.to_string(), "virtio-fs: unknown option 'dax'");
}

// VirtQueue::new panics on a non-power-of-two size, so the catalog must
// pass clamp_queue_size's result to VirtioPciDevice::new.
#[test]
fn queue_size_is_clamped_and_rounded_up() {
    assert_eq!(clamp_queue_size(0), FS_QUEUE_SIZE_MIN);
    assert_eq!(clamp_queue_size(8), 8);
    assert_eq!(clamp_queue_size(100), 128);
    assert_eq!(clamp_queue_size(u16::MAX), FS_QUEUE_SIZE_MAX);
    for n in [0u16, 1, 7, 8, 9, 100, 512, 1023, 1024, 4096, u16::MAX] {
        let q = clamp_queue_size(n);
        assert!(q.is_power_of_two(), "{q} is not a power of two");
        assert!((FS_QUEUE_SIZE_MIN..=FS_QUEUE_SIZE_MAX).contains(&q));
    }
}

// Linux's virtio_cread bounds-checks offset+len against the advertised
// device-config length, so FS_CONFIG_SIZE covers the whole spec struct.
#[test]
fn config_size_covers_tag_and_both_trailing_fields() {
    assert_eq!(FS_CONFIG_SIZE as usize, FS_TAG_LEN + 4 + 4);
    assert_eq!(FS_CONFIG_SIZE, 44);
}

#[test]
fn a_tag_the_config_field_cannot_hold_is_refused_by_new() {
    let opts = VirtioFsOpts {
        tag: "t".repeat(FS_TAG_LEN + 1),
        path: std::env::temp_dir(),
        read_only: true,
        queue_size: FS_QUEUE_SIZE_DEFAULT,
    };
    let Err(err) =
        VirtioFs::new(&opts, Arc::new(PhysMap::new()), test_logger())
    else {
        panic!("an oversized tag must not build a device");
    };
    assert!(
        err.to_string()
            .ends_with(&format!("exceeds {FS_TAG_LEN} bytes")),
        "got: {err}"
    );
}

#[test]
fn queue_count_fits_the_transport_limit() {
    // VirtioPciDevice::new asserts num_queues <= MAX_QUEUES (4).
    assert!(FS_NUM_QUEUES <= 4);
    assert_eq!(FS_HIPRIO_QUEUE, 0);
    assert_eq!(FS_REQUEST_QUEUE, 1);
}

#[test]
fn device_config_serves_the_tag_then_num_request_queues() {
    let fs = test_fs(Arc::new(PhysMap::new()));

    let mut tag = Vec::new();
    for off in 0..FS_TAG_LEN as u16 {
        tag.push(fs.cfg_read(off, 1) as u8);
    }
    assert_eq!(&tag[..6], b"shared");
    assert!(tag[6..].iter().all(|b| *b == 0));

    assert_eq!(fs.cfg_read(36, 4), FS_NUM_REQUEST_QUEUES);
    assert_eq!(fs.cfg_read(40, 4), 0, "notify_buf_size");
    assert_eq!(fs.cfg_read(FS_CONFIG_SIZE, 4), 0, "past the config");

    let feat = fs.device_features();
    assert_ne!(feat & crate::bits::VIRTIO_F_RING_EVENT_IDX, 0);
    assert_ne!(feat & crate::bits::VIRTIO_F_RING_INDIRECT_DESC, 0);
}

// The used ring belongs to VirtioCompletion, so notify_queue must
// drain the avail ring and update avail_event without ever calling
// push_used. shadow_used_idx only moves on push_used, so it is the
// assertion that stays deterministic against the worker thread.
#[test]
fn notify_queue_drains_the_ring_and_never_pushes_used() {
    let (physmap, mut queues, fs) = ring(8, 2);
    let req = usize::from(FS_REQUEST_QUEUE);

    assert!(!fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap));

    assert_eq!(queues[req].last_avail_idx(), 2);
    assert_eq!(avail_event(&physmap, &queues[req]), 2);
    assert_eq!(queues[req].read_used_idx(&physmap), 0);
}

// A guest can keep advancing avail_idx during the EVENT_IDX re-check.
// One notification must retain at most one queue's worth of work.
#[test]
fn notify_queue_caps_collection_at_queue_size() {
    const AVAIL_EVENT_SENTINEL: u16 = 0x55aa;

    // Four published entries keep has_new_avail() true after this
    // two-entry queue has processed one queue's worth of work.
    let (physmap, mut queues, fs) = ring(2, 4);
    let req = usize::from(FS_REQUEST_QUEUE);
    let gpa = queues[req].used_addr() + 4 + u64::from(queues[req].size()) * 8;
    physmap
        .lookup(gpa, 2)
        .expect("mapped avail event")
        .write::<u16>(&AVAIL_EVENT_SENTINEL)
        .expect("write avail event sentinel");

    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);

    assert_eq!(queues[req].last_avail_idx(), queues[req].size());
    assert!(queues[req].has_new_avail(&physmap));
    assert_eq!(avail_event(&physmap, &queues[req]), AVAIL_EVENT_SENTINEL);
    assert_eq!(queues[req].read_used_idx(&physmap), 0);
}

#[test]
fn reset_clears_the_negotiated_features_and_completions() {
    let (physmap, mut queues, fs) = ring(8, 1);
    fs.set_features(crate::bits::VIRTIO_F_RING_EVENT_IDX);
    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);
    assert!(fs.completions.lock().expect("completions")
        [usize::from(FS_REQUEST_QUEUE)]
    .is_some());

    VirtioDevice::reset(&fs);

    assert_eq!(*fs.negotiated_features.lock().expect("features"), 0);
    assert!(fs
        .completions
        .lock()
        .expect("completions")
        .iter()
        .all(Option::is_none));
}

// A guest that writes 0 to DEVICE_STATUS frees the vring right after.
// A reply the worker already dequeued must not land in the used ring
// the guest has recycled.
#[test]
fn reset_drops_a_request_queued_before_it() {
    let (physmap, mut queues, fs) = ring(8, 1);
    let req = usize::from(FS_REQUEST_QUEUE);

    // Park the worker so the dispatch below is observable: the
    // request sits in the channel instead of racing to completion.
    park_the_worker(&fs);

    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);
    assert_eq!(
        queues[req].read_used_ring_idx(&physmap),
        0,
        "the parked worker must not have run yet"
    );

    VirtioDevice::reset(&fs);
    fs.resume();

    // The woken worker drops the stale request and decrements as it
    // goes, so a count back at zero means it has dealt with it. Timing
    // a window instead would pass on a runner that never scheduled it.
    wait_until_idle(&fs);
    assert_eq!(
        queues[req].read_used_ring_idx(&physmap),
        0,
        "the worker published into the pre-reset used ring"
    );
}

// Reset pauses the worker to drain it. It must hand the gate back, or
// the next driver's first request never completes.
#[test]
fn reset_leaves_the_worker_runnable() {
    let (physmap, mut queues, fs) = ring(8, 1);
    let req = usize::from(FS_REQUEST_QUEUE);

    VirtioDevice::reset(&fs);
    assert!(!fs.gate.is_paused(), "reset left the worker parked");

    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);

    let deadline = Instant::now() + Duration::from_secs(5);
    while queues[req].read_used_ring_idx(&physmap) == 0
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        queues[req].read_used_ring_idx(&physmap),
        1,
        "a post-reset request never completed"
    );
}

// A guest resets its virtio devices several times while it boots, and
// the worker takes 50ms to reach its park point. The reset waits on
// `ring_access`, which the worker holds only while it is using the
// ring, so it must never park the worker to find that out.
#[test]
fn reset_never_parks_the_worker() {
    let (physmap, mut queues, fs) = ring(8, 1);

    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);
    wait_until_idle(&fs);

    VirtioDevice::reset(&fs);

    assert_eq!(
        fs.gate.drain_count(),
        0,
        "reset parked the worker to drain it"
    );
    assert!(!fs.gate.is_paused(), "reset left the worker parked");
}

// The fd tables are backing-store state. A reset must not close them,
// because the worker holds raw fds from them across a FUSE syscall and
// a number closed under it goes to the next thread in this process
// that opens anything. The worker reaps them instead, once it sees the
// epoch move.
#[test]
fn the_worker_reaps_the_fd_tables_a_reset_retired() {
    let dir = tempfile::tempdir().expect("export directory");
    // `bind_restricted` moves the process umask while it binds, and
    // the vsock tests bind in parallel with this one, so the mode
    // this directory is created with cannot be trusted.
    std::fs::set_permissions(
        dir.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("export directory mode");
    std::fs::write(dir.path().join("f"), b"hello").expect("seed file");
    let opts = VirtioFsOpts {
        tag: "shared".to_string(),
        path: dir.path().to_path_buf(),
        read_only: true,
        queue_size: FS_QUEUE_SIZE_DEFAULT,
    };
    let fs = VirtioFs::new(&opts, Arc::new(PhysMap::new()), test_logger())
        .expect("open export");
    fs.start().expect("start");

    let name = std::ffi::CString::new("f").expect("file name");
    let entry = fs
        .server
        .passthrough()
        .lookup(fuse::FUSE_ROOT_ID, &name)
        .expect("look the file up");

    // Parked, so the reset provably runs with the worker elsewhere and
    // the mark it leaves has to outlive it.
    park_the_worker(&fs);
    VirtioDevice::reset(&fs);
    fs.server
        .passthrough()
        .getattr(entry.nodeid)
        .expect("the reset closed an fd the worker could still hold");

    fs.resume();
    let deadline = Instant::now() + Duration::from_secs(5);
    while fs.server.passthrough().getattr(entry.nodeid).is_ok()
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        fs.server.passthrough().getattr(entry.nodeid).is_err(),
        "the worker never reaped the retired session's fd tables"
    );
}

// A worker inside a copy has already passed its epoch check, so no
// later check takes that copy back. The reset must not finish while
// the worker holds the shared side, and it needs no deadline to say
// so: the sections under that guard are bounded copies.
#[test]
fn a_reset_waits_out_a_copy_into_the_chain() {
    let fs = test_fs(Arc::new(PhysMap::new()));
    fs.start().expect("start");

    let access = hold_ring_access(&fs);
    let epoch_before = fs.epoch.load(Ordering::Acquire);
    let finished = AtomicBool::new(false);
    let fs_ref = &fs;
    let finished_ref = &finished;

    std::thread::scope(|scope| {
        scope.spawn(move || {
            VirtioDevice::reset(fs_ref);
            finished_ref.store(true, Ordering::Release);
        });

        wait_until_reset_blocks(&fs, epoch_before, &finished);

        let until = Instant::now() + PAST_THE_OLD_DRAIN_BUDGET;
        while Instant::now() < until {
            assert!(
                !finished.load(Ordering::Acquire),
                "the reset told the guest it was done while the worker \
                 could still write into the ring"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        drop(access);
    });

    assert!(
        finished.load(Ordering::Acquire),
        "the reset never finished after the worker let the ring go"
    );
}

// The reset wait is guest driven: any write of 0 to DEVICE_STATUS
// reaches it, in a loop if the guest likes. A migration or an operator
// pause that lands while it waits belongs to the control plane, and the
// reset must not hand the worker back when it ends.
#[test]
fn a_pause_that_lands_inside_a_reset_survives_it() {
    let fs = test_fs(Arc::new(PhysMap::new()));
    fs.start().expect("start");

    let access = hold_ring_access(&fs);
    let epoch_before = fs.epoch.load(Ordering::Acquire);
    let finished = AtomicBool::new(false);
    let fs_ref = &fs;
    let finished_ref = &finished;

    std::thread::scope(|scope| {
        scope.spawn(move || {
            VirtioDevice::reset(fs_ref);
            finished_ref.store(true, Ordering::Release);
        });

        // The handshake is what makes this a test: the pause is taken
        // only once the reset is known to be inside the wait.
        wait_until_reset_blocks(&fs, epoch_before, &finished);
        fs.pause();

        drop(access);
    });

    assert!(
        finished.load(Ordering::Acquire),
        "the reset never finished after the worker let the ring go"
    );
    assert!(
        fs.gate.is_paused(),
        "the reset released a pause the control plane owns"
    );
}

// The invariant, stated as a test: a backing-store call owns only host
// memory, so a reset can end the driver session while one is running
// and nothing it produces reaches the guest. The worker has already
// copied the request out and passed its first epoch check, which is
// the interleaving that matters.
#[test]
fn a_backend_parked_across_a_reset_writes_nothing_into_the_ring() {
    let (physmap, mut queues, fs) = ring(8, 1);
    let req = usize::from(FS_REQUEST_QUEUE);
    seed_fuse_init(&physmap);
    seed_reply_window(&physmap);
    fs.start().expect("start");

    let backend = ParkedBackend::install(&fs);
    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);
    backend.wait_until_inside();

    VirtioDevice::reset(&fs);
    backend.release();
    wait_until_idle(&fs);

    assert_eq!(
        queues[req].read_used_ring_idx(&physmap),
        0,
        "the reply was published into the ring the guest had freed"
    );
    assert!(
        reply_window(&physmap).iter().all(|b| *b == REPLY_SENTINEL),
        "the reply landed in guest pages the reset had released"
    );
}

// The illumos legacy driver writes the reset register once and does not
// poll, so this runs on the vCPU. A host filesystem that never answers
// must therefore not be in the wait: it would hold that vCPU for as
// long as it took.
#[test]
fn a_reset_does_not_wait_for_the_backend() {
    let (physmap, mut queues, fs) = ring(8, 1);
    seed_fuse_init(&physmap);
    fs.start().expect("start");

    let backend = ParkedBackend::install(&fs);
    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);
    backend.wait_until_inside();

    VirtioDevice::reset(&fs);

    assert!(
        backend.still_running(),
        "the reset waited for the backing store"
    );
    backend.release();
}

// A halt abandons the request the worker holds and every one still
// queued behind it. The count has to lose them all, or the next
// reset waits its whole budget for a worker that is already gone.
#[test]
fn halt_clears_the_count_of_every_request_it_abandons() {
    let (physmap, mut queues, fs) = ring(8, 3);
    fs.start().expect("start");

    // Park the worker so the requests queue up behind it instead
    // of completing one by one.
    park_the_worker(&fs);

    fs.notify_queue(FS_REQUEST_QUEUE, &mut queues, &physmap);
    assert_eq!(fs.inflight.load(Ordering::Acquire), 3);

    fs.halt();

    assert_eq!(
        fs.inflight.load(Ordering::Acquire),
        0,
        "halt left abandoned requests on the count"
    );
}

// halt runs only from the paused state, and it must join the worker
// it unparks rather than leaving the thread behind.
#[test]
fn halt_joins_the_worker_after_a_pause() {
    let fs = test_fs(Arc::new(PhysMap::new()));
    fs.start().expect("start");
    park_the_worker(&fs);

    fs.halt();
    assert!(fs.worker.lock().expect("worker lock").is_none());
    assert_eq!(fs.lifecycle_state(), Some(IndicatedState::Halt));
}

// A DAX window would be guest-accessible memory that no session
// covers: the guest keeps the mapping across a device reset, so the
// reset could not take it back the way it takes back the ring. The
// device offers no window, and the config it publishes must keep
// saying so.
#[test]
fn the_device_publishes_no_dax_window() {
    let fs = test_fs(Arc::new(PhysMap::new()));

    // notify_buf_size, the last field of virtio_fs_config. A DAX
    // device would also carry a shared-memory region capability,
    // which this transport never builds.
    assert_eq!(fs.cfg_read(40, 4), 0);
    assert_eq!(FS_CONFIG_SIZE as usize, FS_TAG_LEN + 4 + 4);
}
