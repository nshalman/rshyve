// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The poll thread's delivery against the reset that ends the driver
//! session.
//!
//! Viona's reset returns the kernel rings. It does not stop this
//! thread and it does not drain it, so the thread can be holding a
//! notification it collected under one driver while the guest resets
//! the device and the next driver arms its interrupts. Delivering
//! there writes an interrupt for a ring the new driver never armed: on
//! INTx a level nobody lowers, on MSI-X a message to the vector that
//! driver had just programmed.
//!
//! Both routes are covered because they fail differently, and because
//! a reset leaves every MSI-X vector at `NO_VECTOR`: a device that is
//! quiet only for that reason has proved nothing. Each test below ends
//! by delivering on the same ring in the running session, which is
//! also the shape this fix could break. A session check that refuses
//! too much drops live interrupts and hangs the guest.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use vmm_core::common::{RWOp, ReadOp, WriteOp};
use vmm_core::mem::PhysMap;
use vmm_core::mmio::MmioBus;
use vmm_core::pio::PioBus;
use vmm_devices::pci::device::PciDevice;
use vmm_devices::pci::msix::{MsiSink, MsixTable};
use vmm_devices::pci::{BarN, INTxPinID, IntrPin};

use super::device::device_with;
use super::{cleared, null_log, Park, ParkAt, Steps, TestLink};
use crate::bits;
use crate::pci::VirtioPciDevice;
use crate::viona::link::LinkOps;
use crate::viona::poll::deliver_pending;
use crate::viona::{
    VionaInterrupt, VirtioViona, NET_CONFIG_SIZE, NET_NUM_QUEUES,
    NET_QUEUE_SIZE,
};

/// A wait this long means a thread is wedged, not slow.
const WEDGED: Duration = Duration::from_secs(15);

/// Status a driver holds just before it sets DRIVER_OK.
const READY: u8 =
    bits::STATUS_ACKNOWLEDGE | bits::STATUS_DRIVER | bits::STATUS_FEATURES_OK;

/// The vector a driver maps the rx ring to.
const QUEUE_VECTOR: u16 = 1;

/// The rx ring, which is the one the kernel reports pending.
const PENDING_RING: usize = 0;

/// Where the message that vector carries goes.
const MSG_ADDR: u64 = 0xFEE0_0000;
const MSG_DATA: u64 = 0x21;

/// An interrupt pin that counts what the transport put on the line.
struct TestPin {
    level: AtomicBool,
    asserts: AtomicUsize,
}

impl TestPin {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            level: AtomicBool::new(false),
            asserts: AtomicUsize::new(0),
        })
    }

    fn asserts(&self) -> usize {
        self.asserts.load(Ordering::Acquire)
    }
}

impl IntrPin for TestPin {
    fn assert(&self) {
        self.asserts.fetch_add(1, Ordering::AcqRel);
        self.level.store(true, Ordering::Release);
    }

    fn deassert(&self) {
        self.level.store(false, Ordering::Release);
    }

    fn is_asserted(&self) -> bool {
        self.level.load(Ordering::Acquire)
    }
}

/// A sink that records the messages that reached the kernel.
struct TestSink {
    sent: Mutex<Vec<(u64, u64)>>,
}

impl TestSink {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            sent: Mutex::new(Vec::new()),
        })
    }

    fn sent(&self) -> Vec<(u64, u64)> {
        self.sent.lock().expect("sink lock").clone()
    }
}

impl MsiSink for TestSink {
    fn send(&self, addr: u64, data: u64) {
        self.sent.lock().expect("sink lock").push((addr, data));
    }
}

/// Write a legacy BAR0 register the way a guest does.
fn write_legacy_on(dev: &Dev, reg: u16, val: &[u8]) {
    let wo = WriteOp::from_buf(val);
    dev.bar_rw(BarN::BAR0, usize::from(reg), RWOp::Write(&wo));
}

type Dev = Arc<VirtioPciDevice<VirtioViona>>;

/// A viona device on a real pin, its poll link, and whatever the
/// guest's interrupts reach.
struct Fixture {
    dev: Dev,
    link: Arc<TestLink>,
    /// What the device asked of the kernel, the clears included.
    steps: Steps,
    intr: Arc<Mutex<Option<VionaInterrupt>>>,
    pin: Arc<TestPin>,
    /// Set when the driver in this fixture uses MSI-X.
    sink: Option<Arc<TestSink>>,
    arrived: mpsc::Receiver<()>,
    go: mpsc::Sender<()>,
}

impl Fixture {
    /// Build a device whose poll thread parks at `park_at`.
    ///
    /// `msix` decides the route the driver takes, which is the whole
    /// reason for two of every test below.
    fn new(park_at: ParkAt, msix: bool) -> Self {
        let (arrived_tx, arrived_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel();
        let steps = Steps::default();
        let link = Arc::new(
            TestLink::new(&steps).pending(PENDING_RING).parking(Park {
                at: park_at,
                arrived: arrived_tx,
                go: go_rx,
            }),
        );

        let sink = msix.then(TestSink::new);
        let table = sink.as_ref().map(|sink| {
            let table = Arc::new(MsixTable::new(
                NET_NUM_QUEUES as u16 + 1,
                Arc::clone(sink) as Arc<dyn MsiSink>,
            ));
            table.set_enabled(true);
            // A vector the driver programmed, so `fire` has somewhere
            // to send.
            table.write_entry(QUEUE_VECTOR, MSG_ADDR, MSG_DATA);
            table
        });

        let device = device_with(Arc::clone(&link) as Arc<dyn LinkOps>);
        let intr = device.interrupt();
        let pin = TestPin::new();
        let dev = VirtioPciDevice::new(
            device,
            bits::VIRTIO_DEV_TYPE_NET,
            NET_NUM_QUEUES,
            NET_QUEUE_SIZE,
            NET_CONFIG_SIZE,
            Some((INTxPinID::IntA, Arc::clone(&pin) as Arc<dyn IntrPin>)),
            Arc::new(PhysMap::new()),
            Arc::new(PioBus::new()),
            Arc::new(MmioBus::new()),
            table,
        );
        *intr.lock().expect("viona interrupt lock") = Some(dev.backend_intr());

        Self {
            dev,
            link,
            steps,
            intr,
            pin,
            sink,
            arrived: arrived_rx,
            go: go_tx,
        }
    }

    fn write_legacy(&self, reg: u16, val: &[u8]) {
        write_legacy_on(&self.dev, reg, val);
    }

    fn write_status(&self, val: u8) {
        self.write_legacy(bits::LEGACY_REG_DEVICE_STATUS, &[val]);
    }

    /// Read and clear the ISR, the way a legacy driver's handler does.
    fn isr(&self) -> u8 {
        let mut ro = ReadOp::new(1);
        self.dev.bar_rw(
            BarN::BAR0,
            usize::from(bits::LEGACY_REG_ISR_STATUS),
            RWOp::Read(&mut ro),
        );
        ro.buf()[0]
    }

    /// Bring a driver up, the way the guest does.
    ///
    /// The MSI-X vector is part of that: a reset leaves every vector
    /// at `NO_VECTOR`, and a device that says nothing for that reason
    /// would pass every assertion here without a session check.
    fn arm(&self) {
        self.write_status(READY | bits::STATUS_DRIVER_OK);
        if self.sink.is_some() {
            self.write_legacy(
                bits::LEGACY_REG_QUEUE_SELECT,
                &(PENDING_RING as u16).to_le_bytes(),
            );
            self.write_legacy(
                bits::LEGACY_REG_MSIX_QUEUE_VECTOR,
                &QUEUE_VECTOR.to_le_bytes(),
            );
        }
    }

    /// Run one poll wakeup on its own thread, the way the poll thread
    /// does.
    fn spawn_poller(&self) -> std::thread::JoinHandle<()> {
        let link = Arc::clone(&self.link);
        let intr = Arc::clone(&self.intr);
        std::thread::Builder::new()
            .name("viona-intr-poll-stub".into())
            .spawn(move || {
                deliver_pending(link.as_ref(), &intr, &null_log());
            })
            .expect("the test can spawn a thread")
    }

    /// Deliver one wakeup on this thread, with no park left to hit.
    fn deliver_here(&self) {
        deliver_pending(self.link.as_ref(), &self.intr, &null_log());
    }

    fn wait_parked(&self) {
        self.arrived
            .recv_timeout(WEDGED)
            .expect("the poll thread reached its park");
    }

    fn release(&self) {
        self.go.send(()).expect("release the poll thread");
    }

    /// What reached the guest, by either route.
    fn delivered(&self) -> usize {
        let messages = self.sink.as_ref().map_or(0, |sink| sink.sent().len());
        self.pin.asserts() + messages
    }
}

/// A whole device reset lands between the poll thread's session and
/// its raise. Nothing may reach the driver that follows, and the same
/// ring must still reach the driver that is running.
fn a_reset_refuses_the_notification_it_overtook(park_at: ParkAt, msix: bool) {
    let fix = Fixture::new(park_at, msix);
    fix.arm();

    let poller = fix.spawn_poller();
    fix.wait_parked();

    // The guest resets the device and the next driver arms its
    // interrupts, all while the poll thread is held. A reset that
    // waited for this thread would never return.
    fix.write_status(0);
    fix.arm();

    fix.release();
    poller.join().expect("the poll thread finished");

    assert_eq!(
        fix.delivered(),
        0,
        "a notification collected before the reset reached the next driver",
    );
    assert_eq!(fix.isr(), 0, "the old session set the ISR");
    assert!(
        !fix.pin.is_asserted(),
        "the old session left the line asserted",
    );

    // The converse, on the same device, the same ring and the same
    // vector. Only the session differs, so this is what tells a
    // refusal apart from a device that can no longer interrupt.
    fix.deliver_here();
    assert_eq!(
        fix.delivered(),
        1,
        "the running session's own notification was dropped",
    );
    assert_eq!(
        cleared(&fix.steps),
        [PENDING_RING as u16, PENDING_RING as u16],
        "the kernel's pending interrupt was not cleared each wakeup",
    );
}

#[test]
fn a_reset_refuses_a_notification_parked_at_the_poll_on_intx() {
    a_reset_refuses_the_notification_it_overtook(ParkAt::Poll, false);
}

#[test]
fn a_reset_refuses_a_notification_parked_at_the_poll_on_msix() {
    a_reset_refuses_the_notification_it_overtook(ParkAt::Poll, true);
}

#[test]
fn a_reset_refuses_a_notification_parked_at_the_clear_on_intx() {
    a_reset_refuses_the_notification_it_overtook(ParkAt::Clear, false);
}

#[test]
fn a_reset_refuses_a_notification_parked_at_the_clear_on_msix() {
    a_reset_refuses_the_notification_it_overtook(ParkAt::Clear, true);
}

#[test]
fn a_notification_in_the_running_session_reaches_an_intx_driver() {
    // The interrupt the guest is waiting for. A device that lost this
    // stalls the ring instead of wedging the line, which is the other
    // way a session check goes wrong.
    let fix = Fixture::new(ParkAt::Poll, false);
    fix.arm();
    fix.release();

    fix.deliver_here();

    assert_eq!(fix.pin.asserts(), 1, "the interrupt never reached the pin");
    assert!(fix.pin.is_asserted(), "the line was not left asserted");
    assert_eq!(
        fix.isr(),
        bits::ISR_QUEUE_INTR,
        "the driver's handler would read a zero ISR",
    );
}

#[test]
fn a_notification_in_the_running_session_reaches_an_msix_driver() {
    // The same, on the route a modern guest uses. This one has no
    // level to leave behind, so the message itself is the evidence.
    let fix = Fixture::new(ParkAt::Poll, true);
    fix.arm();
    fix.release();

    fix.deliver_here();

    assert_eq!(
        fix.sink.as_ref().expect("this fixture is on MSI-X").sent(),
        [(MSG_ADDR, MSG_DATA)],
        "the interrupt never reached the vector the driver programmed",
    );
    assert_eq!(
        fix.pin.asserts(),
        0,
        "an MSI-X driver was sent an INTx assertion it will never lower",
    );
}

/// How long a reset may take while a poll thread sits at its park.
///
/// Generous: this is not a latency claim, it separates "returned" from
/// "waiting on the parked thread".
const RESET_BOUND: Duration = Duration::from_secs(5);

/// The reset runs on its own thread, and must finish while the poll
/// thread is still parked.
///
/// This is what says the park sits BEFORE admission rather than after.
/// A delivery the gate had already admitted is one `finish_reset` waits
/// out, so a poller parked on the far side of admission would hold the
/// reset here and this fails instead of hanging. It is also the
/// property the whole remedy rests on: the poll thread is never
/// stopped or drained by a reset, so a reset that could wait for it
/// would wait for a kernel poll with no deadline.
fn a_reset_completes_while_the_poll_thread_is_held(
    park_at: ParkAt,
    msix: bool,
) {
    let fix = Fixture::new(park_at, msix);
    fix.arm();

    let poller = fix.spawn_poller();
    fix.wait_parked();

    // Another thread entirely, the way the guest's vCPU is another
    // thread. Nothing releases the park until it has returned.
    let resetter = {
        let dev = Arc::clone(&fix.dev);
        std::thread::Builder::new()
            .name("guest-vcpu-stub".into())
            .spawn(move || {
                write_legacy_on(&dev, bits::LEGACY_REG_DEVICE_STATUS, &[0]);
            })
            .expect("the test can spawn a thread")
    };
    let deadline = std::time::Instant::now() + RESET_BOUND;
    while !resetter.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    let reset_returned = resetter.is_finished();

    // The next driver arms BEFORE the poll thread is let go, vector
    // and all. Arming afterwards makes this test lie on MSI-X: a reset
    // leaves every vector at NO_VECTOR, so a delivery into the new
    // session would be swallowed by the routing and the test would
    // pass with the session check removed. Verified by mutation, not
    // by reading.
    fix.arm();

    // Released either way, so a wrong build fails rather than parking
    // the run.
    fix.release();
    poller.join().expect("the poll thread finished");
    resetter.join().expect("the reset finished");
    assert!(
        reset_returned,
        "the reset waited on a poll thread parked before admission",
    );

    assert_eq!(
        fix.delivered(),
        0,
        "a notification collected before the reset reached the next driver",
    );
    assert_eq!(fix.isr(), 0, "the old session set the ISR");
    assert!(!fix.pin.is_asserted(), "the old session left the line up");

    // Same device, same ring, same vector: only the session differs.
    // Without this, silence is not evidence of a session check.
    fix.deliver_here();
    assert_eq!(
        fix.delivered(),
        1,
        "the running session's own notification was dropped",
    );
    // A refused notification still has to leave the kernel's pending
    // bit clear, or viona re-signals it and this thread spins.
    assert_eq!(
        cleared(&fix.steps),
        [PENDING_RING as u16, PENDING_RING as u16],
        "the kernel's pending interrupt was not cleared each wakeup",
    );
}

#[test]
fn a_reset_on_another_thread_finishes_over_a_parked_poller_on_intx() {
    a_reset_completes_while_the_poll_thread_is_held(ParkAt::Poll, false);
}

#[test]
fn a_reset_on_another_thread_finishes_over_a_parked_poller_on_msix() {
    a_reset_completes_while_the_poll_thread_is_held(ParkAt::Poll, true);
}

#[test]
fn a_reset_on_another_thread_finishes_over_a_poller_at_the_clear_on_intx() {
    a_reset_completes_while_the_poll_thread_is_held(ParkAt::Clear, false);
}

#[test]
fn a_reset_on_another_thread_finishes_over_a_poller_at_the_clear_on_msix() {
    a_reset_completes_while_the_poll_thread_is_held(ParkAt::Clear, true);
}
