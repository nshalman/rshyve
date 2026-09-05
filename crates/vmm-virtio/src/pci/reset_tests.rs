// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A device reset against the register writes and interrupts that race
//! it.
//!
//! The guest write that asks for the reset returns at once and the
//! backend drain runs on the device's own thread, so the device stays
//! reachable while it is half reset. These tests pin the edges of that
//! window: a write must decide whether to run while it holds the
//! transport lock, the vCPU must not wait on the backend, and status 0
//! and the open device must reach the guest in one step.

use super::*;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use vmm_core::common::{ReadOp, WriteOp};

/// A wait this long means a thread is wedged, not slow.
const WEDGED: Duration = Duration::from_secs(10);

/// Time given to a wrong build to run ahead of a parked thread.
const SETTLE: Duration = Duration::from_millis(200);

/// Interval between samples of a value another thread publishes.
const POLL: Duration = Duration::from_millis(2);

/// Status a driver holds just before it sets DRIVER_OK.
const READY: u8 =
    bits::STATUS_ACKNOWLEDGE | bits::STATUS_DRIVER | bits::STATUS_FEATURES_OK;

/// What the transport asked of the backend while the drain ran.
#[derive(Default)]
struct BackendLog {
    draining: bool,
    during_drain: Vec<&'static str>,
}

/// A backend whose reset blocks until the test releases it, standing
/// in for one that must drain its I/O workers.
struct DrainDevice {
    log: Mutex<BackendLog>,
    entered: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
    /// Resets the transport has asked the backend for.
    resets: AtomicUsize,
    /// False from the moment a reset starts until it returns, which is
    /// the window a worker could still be writing the ring in.
    off_the_ring: AtomicBool,
}

impl DrainDevice {
    fn note(&self, call: &'static str) {
        let mut log = self.log.lock().expect("backend log poisoned");
        if log.draining {
            log.during_drain.push(call);
        }
    }

    fn resets(&self) -> usize {
        self.resets.load(Ordering::Acquire)
    }

    fn off_the_ring(&self) -> bool {
        self.off_the_ring.load(Ordering::Acquire)
    }
}

impl VirtioDevice for DrainDevice {
    fn device_features(&self) -> u64 {
        0xF | bits::VIRTIO_F_VERSION_1
    }

    fn set_features(&self, _features: u64) {
        self.note("set_features");
    }

    fn cfg_read(&self, _offset: u16, _len: u8) -> u32 {
        0
    }

    fn cfg_write(&self, _offset: u16, _val: u32, _len: u8) {
        self.note("cfg_write");
    }

    fn queue_addr_set(&self, _queue_idx: u16, _queue: &VirtQueue) {
        self.note("queue_addr_set");
    }

    fn process_queue(
        &self,
        _queue_idx: u16,
        _queue: &mut VirtQueue,
        _head: u16,
        _physmap: &PhysMap,
    ) -> u32 {
        self.note("process_queue");
        0
    }

    fn reset(&self) {
        self.resets.fetch_add(1, Ordering::AcqRel);
        self.off_the_ring.store(false, Ordering::Release);
        self.log.lock().expect("backend log poisoned").draining = true;
        self.entered.send(()).expect("the test watches the drain");
        self.release
            .lock()
            .expect("drain release poisoned")
            .recv_timeout(WEDGED)
            .expect("the test never released the drain");
        self.log.lock().expect("backend log poisoned").draining = false;
        self.off_the_ring.store(true, Ordering::Release);
    }
}

impl Lifecycle for DrainDevice {
    fn type_name(&self) -> &'static str {
        "drain-device"
    }
}

type Dev = Arc<VirtioPciDevice<DrainDevice>>;

/// Build a device whose backend drain the test drives.
fn make_device() -> (Dev, mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let dev = VirtioPciDevice::new(
        DrainDevice {
            log: Mutex::default(),
            entered: entered_tx,
            release: Mutex::new(release_rx),
            resets: AtomicUsize::new(0),
            off_the_ring: AtomicBool::new(true),
        },
        bits::VIRTIO_DEV_TYPE_BLOCK,
        1,
        256,
        8,
        None,
        Arc::new(PhysMap::new()),
        Arc::new(PioBus::new()),
        Arc::new(MmioBus::new()),
        None,
    );
    (dev, entered_rx, release_tx)
}

/// Build a one-shot park: the parked thread reports that it arrived,
/// then waits for the test to let it go.
fn park() -> (
    Arc<dyn Fn() + Send + Sync>,
    mpsc::Receiver<()>,
    mpsc::Sender<()>,
) {
    let (arrived_tx, arrived_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel();
    let go_rx = Mutex::new(go_rx);
    let hook: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        arrived_tx.send(()).expect("the test watches this park");
        go_rx
            .lock()
            .expect("park release poisoned")
            .recv_timeout(WEDGED)
            .expect("the test never released this park");
    });
    (hook, arrived_rx, go_tx)
}

fn status(dev: &Dev) -> u8 {
    let mut ro = ReadOp::new(1);
    dev.bar_rw(
        BarN::BAR0,
        usize::from(bits::LEGACY_REG_DEVICE_STATUS),
        RWOp::Read(&mut ro),
    );
    ro.buf()[0]
}

fn write_status(dev: &Dev, val: u8) {
    let wo = WriteOp::from_buf(&[val]);
    dev.bar_rw(
        BarN::BAR0,
        usize::from(bits::LEGACY_REG_DEVICE_STATUS),
        RWOp::Write(&wo),
    );
}

fn modern_status(dev: &Dev) -> u8 {
    let mut ro = ReadOp::new(1);
    dev.bar_rw(
        BarN::BAR2,
        usize::from(bits::COMMON_CFG_DEVICE_STATUS),
        RWOp::Read(&mut ro),
    );
    ro.buf()[0]
}

fn write_legacy(dev: &Dev, reg: u16, val: u32) {
    let wo = WriteOp::from_buf(&val.to_le_bytes());
    dev.bar_rw(BarN::BAR0, usize::from(reg), RWOp::Write(&wo));
}

fn write_modern(dev: &Dev, offset: u16, val: u32) {
    let wo = WriteOp::from_buf(&val.to_le_bytes());
    dev.bar_rw(BarN::BAR2, usize::from(offset), RWOp::Write(&wo));
}

fn during_drain(dev: &Dev) -> Vec<&'static str> {
    dev.device
        .log
        .lock()
        .expect("backend log poisoned")
        .during_drain
        .clone()
}

/// Poll status the way a driver does, until the reset publishes 0.
fn wait_for_reset(dev: &Dev) {
    let deadline = Instant::now() + WEDGED;
    while status(dev) != 0 {
        assert!(
            Instant::now() < deadline,
            "the reset never published status 0"
        );
        std::thread::sleep(POLL);
    }
}

// The write that asks for the reset must not wait for the backend.
// VirtIO 1.3 sec 4.1.4.3.1 has the driver poll DEVICE_STATUS for 0
// instead, and that is what lets the drain run elsewhere. A backend
// stuck on host storage otherwise holds the vCPU that wrote the
// register, and a one-vCPU guest never reaches the poll that ends it.
#[test]
fn a_status_write_returns_while_the_backend_still_drains() {
    let (dev, entered, release) = make_device();
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);

    let writer = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || write_status(&dev, 0))
    };
    entered.recv_timeout(WEDGED).expect("the backend drained");

    // The drain is held until below, so a write that has returned by
    // now returned while the backend could still write the ring.
    let deadline = Instant::now() + WEDGED;
    while !writer.is_finished() && Instant::now() < deadline {
        std::thread::sleep(POLL);
    }
    assert!(
        writer.is_finished(),
        "the vCPU that asked for the reset waited out the backend"
    );
    writer.join().expect("the write finished");
    assert!(
        !dev.device.off_the_ring(),
        "the backend left the ring on its own, so the wait proved nothing"
    );
    assert_ne!(status(&dev), 0, "the guest was told the reset had finished");

    release.send(()).expect("release the drain");
    wait_for_reset(&dev);
}

// The driver frees the vring the moment it reads status back as 0
// (VirtIO 1.3 sec 4.1.4.3.1). Sampling the backend at that read, and
// not after it, is what makes this a statement about the guest's
// memory rather than about the order of two threads.
#[test]
fn the_backend_leaves_the_ring_before_status_reads_zero() {
    let (dev, entered, release) = make_device();
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);

    write_status(&dev, 0);
    entered.recv_timeout(WEDGED).expect("the backend drained");
    assert!(
        !dev.device.off_the_ring(),
        "the test needs the backend inside its drain"
    );
    assert_ne!(
        status(&dev),
        0,
        "the guest could free the vring while the backend held it"
    );

    release.send(()).expect("release the drain");

    let deadline = Instant::now() + WEDGED;
    loop {
        if status(&dev) == 0 {
            assert!(
                dev.device.off_the_ring(),
                "the guest read status 0 while the backend could still \
                 write the ring"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the reset never published status 0"
        );
        std::thread::sleep(POLL);
    }
}

// A write must test the reset latch while it holds the transport
// lock. If it can test the latch first and take the lock afterwards,
// a reset starts in between and the write then lands in the middle of
// the backend drain.
#[test]
fn a_write_tests_the_reset_latch_under_the_transport_lock() {
    let (dev, _entered, _release) = make_device();
    let (hook, arrived, go) = park();
    dev.park_write_after_latch_check(hook);

    let writer = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || write_status(&dev, READY))
    };
    arrived
        .recv_timeout(WEDGED)
        .expect("the write reached its latch check");

    // A reset needs this lock to close the latch. Held here, no reset
    // can start between the check and the write it guards.
    let locked_out = dev.virtio_state.try_lock().is_err();

    go.send(()).expect("release the write");
    writer.join().expect("the write finished");
    assert!(locked_out, "the latch check ran outside the transport lock");
    assert_eq!(status(&dev), READY);
}

// The interleaving a check outside the lock allows: a write passes the
// check, a reset starts, and the write then runs while the backend is
// draining.
#[test]
fn a_write_parked_at_the_latch_check_cannot_land_during_a_drain() {
    let (dev, entered, release) = make_device();
    write_status(&dev, READY);

    let (hook, arrived, go) = park();
    dev.park_write_after_latch_check(hook);

    let writer = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || {
            write_legacy(&dev, bits::LEGACY_REG_GUEST_FEATURES, 0xF)
        })
    };
    arrived
        .recv_timeout(WEDGED)
        .expect("the write reached its latch check");

    let resetter = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || write_status(&dev, 0))
    };
    // A build that splits the check from the write gets here. A
    // correct one cannot: the reset waits for the lock the parked
    // write holds.
    let drained_early = entered.recv_timeout(SETTLE).is_ok();

    go.send(()).expect("release the write");
    writer.join().expect("the write finished");
    resetter.join().expect("the reset was asked for");
    entered.recv_timeout(WEDGED).expect("the backend drained");
    release.send(()).expect("release the drain");
    wait_for_reset(&dev);

    assert_eq!(
        during_drain(&dev),
        Vec::<&str>::new(),
        "a register write reached the backend during the drain"
    );
    assert!(
        !drained_early,
        "a reset drained the backend while a write held the transport lock"
    );
}

// Every write path is shut while the backend drains, on both
// transports. A write that lands here is handed a ring the drain is
// still writing to.
#[test]
fn every_write_path_is_shut_while_the_backend_drains() {
    let (dev, entered, release) = make_device();
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);

    write_status(&dev, 0);
    entered.recv_timeout(WEDGED).expect("the backend drained");

    write_status(&dev, READY | bits::STATUS_DRIVER_OK);
    write_legacy(&dev, bits::LEGACY_REG_GUEST_FEATURES, 0xF);
    write_legacy(&dev, bits::LEGACY_REG_QUEUE_PFN, 1);
    write_modern(
        &dev,
        bits::COMMON_CFG_DEVICE_STATUS,
        u32::from(READY | bits::STATUS_DRIVER_OK),
    );
    write_modern(&dev, bits::COMMON_CFG_DRIVER_FEATURE, 0xF);
    // Device-specific config, then the notify register.
    write_modern(&dev, 0x1000, 0xFF);
    write_modern(&dev, 0x2000, 0);

    assert_ne!(status(&dev), 0, "the guest saw the reset finish mid-drain");
    assert_eq!(
        status(&dev) & bits::STATUS_DRIVER_OK,
        0,
        "a re-init landed while the backend was still draining"
    );

    release.send(()).expect("release the drain");
    wait_for_reset(&dev);

    assert_eq!(
        during_drain(&dev),
        Vec::<&str>::new(),
        "a register write reached the backend during the drain"
    );
}

// A second reset while one is draining has nothing to do. Running it
// behind the first would drain a device the guest has already been
// told nothing about, and the guest polls for one 0, not two.
#[test]
fn a_second_reset_during_a_drain_is_dropped() {
    let (dev, entered, release) = make_device();
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);

    write_status(&dev, 0);
    entered.recv_timeout(WEDGED).expect("the backend drained");
    write_status(&dev, 0);

    release.send(()).expect("release the drain");
    wait_for_reset(&dev);
    assert!(
        entered.recv_timeout(SETTLE).is_err(),
        "a second drain ran behind the first"
    );
    assert_eq!(dev.device.resets(), 1);

    // The next session resets too, so the drain thread is reusable and
    // the latch was left open behind the first one.
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);
    write_status(&dev, 0);
    entered
        .recv_timeout(WEDGED)
        .expect("the next session's reset reached the backend");
    release.send(()).expect("release the second drain");
    wait_for_reset(&dev);
    assert_eq!(dev.device.resets(), 2);
}

// The driver frees the vring as soon as it reads status back as 0
// (VirtIO 1.3 sec 4.1.4.3.1), then starts the next session with
// ACKNOWLEDGE. Publishing that 0 before the latch opens throws the
// first write of the new session away.
#[test]
fn a_reset_opens_the_latch_before_status_zero_can_be_read() {
    let (dev, entered, release) = make_device();
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);

    let (hook, arrived, go) = park();
    dev.park_reset_after_publish(hook);

    write_status(&dev, 0);
    entered.recv_timeout(WEDGED).expect("the backend drained");
    release.send(()).expect("release the drain");
    arrived
        .recv_timeout(WEDGED)
        .expect("the reset published its result");

    // Status 0 and the open latch are both staged. Neither is
    // readable yet, so no driver can act on half of the pair.
    let locked_out = dev.virtio_state.try_lock().is_err();

    go.send(()).expect("release the reset");
    wait_for_reset(&dev);
    assert!(locked_out, "status 0 was readable before the latch opened");

    write_status(&dev, bits::STATUS_ACKNOWLEDGE);
    assert_eq!(status(&dev), bits::STATUS_ACKNOWLEDGE);
}

// A queue refusal writes DEVICE_NEEDS_RESET into the status. Applied
// after the guard drops, that write lands in the middle of a reset
// drain, or on the session that follows the reset.
#[test]
fn a_queue_refusal_writes_its_status_under_the_refusing_guard() {
    let (dev, _entered, _release) = make_device();
    let (hook, arrived, go) = park();
    dev.park_refusal_after_status(hook);

    let writer = {
        let dev = Arc::clone(&dev);
        // No guest memory is mapped, so the ring is refused.
        std::thread::spawn(move || {
            write_legacy(&dev, bits::LEGACY_REG_QUEUE_PFN, 1)
        })
    };
    arrived.recv_timeout(WEDGED).expect("the queue was refused");

    let locked_out = dev.virtio_state.try_lock().is_err();

    go.send(()).expect("release the refusal");
    writer.join().expect("the refusal finished");
    assert!(
        locked_out,
        "the refusal wrote status outside the transport lock"
    );
    assert_ne!(
        status(&dev) & bits::STATUS_DEVICE_NEEDS_RESET,
        0,
        "the driver was never told the queue had stopped"
    );
}

// Linux resets a device at probe: it writes 0 to a status that is
// already 0, then polls for 0 before it writes ACKNOWLEDGE. A drain
// that reports 0 to that poll throws the ACKNOWLEDGE away.
#[test]
fn a_reset_that_starts_from_status_zero_is_not_reported_as_finished() {
    let (dev, entered, release) = make_device();
    assert_eq!(status(&dev), 0);

    write_status(&dev, 0);
    entered.recv_timeout(WEDGED).expect("the backend drained");

    assert_ne!(
        status(&dev),
        0,
        "a polling driver saw the reset finish while the backend drained"
    );
    assert_ne!(
        modern_status(&dev),
        0,
        "the modern status read reported the reset finished"
    );

    release.send(()).expect("release the drain");
    wait_for_reset(&dev);
    write_status(&dev, bits::STATUS_ACKNOWLEDGE);
    assert_eq!(status(&dev), bits::STATUS_ACKNOWLEDGE);
}

// Teardown must not wait on a drain that never ends. The drain keeps
// its own reference to the device, so leaving it to run is safe, and
// the VM going away is not a reason to stop tearing the rest down.
#[test]
fn a_halt_gives_up_on_a_drain_that_will_not_finish() {
    let (dev, entered, release) = make_device();
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);

    write_status(&dev, 0);
    entered.recv_timeout(WEDGED).expect("the backend drained");

    let started = Instant::now();
    dev.halt();
    let waited = started.elapsed();

    assert!(
        !dev.device.off_the_ring(),
        "the backend finished on its own, so the halt proved nothing"
    );
    assert!(
        waited < WEDGED,
        "the teardown waited on a drain that never finished: {waited:?}"
    );

    release.send(()).expect("release the drain");
}
