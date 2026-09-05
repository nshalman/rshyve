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

use std::time::Instant;

use crate::queue::VirtqDesc;
use vmm_devices::Lifecycle;

/// Longer than the two second budget the drain used to give up after.
/// A reset that returns inside this window has a deadline again.
const PAST_THE_OLD_DRAIN_BUDGET: Duration = Duration::from_millis(2500);

/// Take the shared side of `ring_access` the way the worker holds it
/// across a passthrough syscall, and put a request on the count so the
/// reset cannot take its idle short cut.
fn hold_ring_access(fs: &VirtioFs) -> std::sync::RwLockReadGuard<'_, ()> {
    fs.inflight.fetch_add(1, Ordering::AcqRel);
    fs.ring_access.read().expect("ring access")
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
            len: 256,
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
    let opts = parse_fs_config("/export/x,tag=data,ro,queue-size=256")
        .expect("parse");
    assert_eq!(opts.tag, "data");
    assert!(opts.read_only);
    assert_eq!(opts.queue_size, 256);
}

#[test]
fn oversized_tag_is_rejected() {
    let tag = "t".repeat(FS_TAG_LEN + 1);
    let err = parse_fs_config(&format!("/export/x,tag={tag}"))
        .expect_err("long tag");
    assert_eq!(
        err.to_string(),
        format!("virtio-fs tag '{tag}' exceeds {FS_TAG_LEN} bytes")
    );
}

#[test]
fn out_of_range_queue_size_is_rejected() {
    let err =
        parse_fs_config("/export/x,queue-size=4").expect_err("too small");
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
    let gpa =
        queues[req].used_addr() + 4 + u64::from(queues[req].size()) * 8;
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
    fs.pause();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fs.is_quiesced() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(fs.is_quiesced(), "worker did not park within the budget");

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
    let deadline = Instant::now() + Duration::from_secs(5);
    while fs.inflight.load(Ordering::Acquire) != 0 && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        fs.inflight.load(Ordering::Acquire),
        0,
        "the worker never finished with the pre-reset request"
    );
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
    let deadline = Instant::now() + Duration::from_secs(5);
    while fs.inflight.load(Ordering::Acquire) != 0
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        fs.inflight.load(Ordering::Acquire),
        0,
        "the queued request never finished"
    );

    VirtioDevice::reset(&fs);

    assert_eq!(
        fs.gate.drain_count(),
        0,
        "reset parked the worker to drain it"
    );
    assert!(!fs.gate.is_paused(), "reset left the worker parked");
}

// Clearing the fd tables is safe only because the reset waited: the
// worker holds a raw fd from them across a FUSE syscall, and closing
// it hands that number to the next thread in this process that opens
// anything. The reset must not get past `ring_access` to reach the
// clear, and must reach it once the worker lets go.
#[test]
fn a_reset_keeps_the_fd_tables_while_the_worker_holds_them() {
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

    let name = std::ffi::CString::new("f").expect("file name");
    let entry = fs
        .server
        .passthrough()
        .lookup(fuse::FUSE_ROOT_ID, &name)
        .expect("look the file up");

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
        fs.server
            .passthrough()
            .getattr(entry.nodeid)
            .expect("the reset closed an fd the worker still held");

        drop(access);
    });

    assert!(
        fs.server.passthrough().getattr(entry.nodeid).is_err(),
        "the reset left the guest's fd tables behind"
    );
}

// The window codex found in the budgeted drain: the worker passes the
// epoch check, the passthrough syscall runs long, the wait gives up,
// and the reset returns. The guest reads DEVICE_STATUS as 0, frees the
// vring, and `FuseServer::handle` writes the reply into the recycled
// pages. Checking the epoch afterwards cannot take that write back, so
// the reset must not finish at all while the worker holds the shared
// side of `ring_access`.
#[test]
fn a_reset_waits_out_a_worker_that_never_finishes_in_time() {
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

// A halt abandons the request the worker holds and every one still
// queued behind it. The count has to lose them all, or the next
// reset waits its whole budget for a worker that is already gone.
#[test]
fn halt_clears_the_count_of_every_request_it_abandons() {
    let (physmap, mut queues, fs) = ring(8, 3);
    fs.start().expect("start");

    // Park the worker so the requests queue up behind it instead
    // of completing one by one.
    fs.pause();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fs.is_quiesced() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(fs.is_quiesced(), "worker did not park within its budget");

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
    fs.pause();

    let deadline = Instant::now() + Duration::from_secs(5);
    while !fs.is_quiesced() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(fs.is_quiesced(), "worker did not park within its budget");

    fs.halt();
    assert!(fs.worker.lock().expect("worker lock").is_none());
    assert_eq!(fs.lifecycle_state(), Some(IndicatedState::Halt));
}
