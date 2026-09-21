// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Interrupt delivery against the reset that ends the driver session.
//!
//! Two properties, and they fail in different ways.
//!
//! The ISR byte and the INTx level are one piece of state. A level
//! left asserted behind a zero ISR is not a lost interrupt, it is a
//! wedged line: the driver reads zero, claims nothing, lowers nothing,
//! and every device sharing that line stops being able to interrupt
//! for the life of the VM.
//!
//! An MSI-X message has no level to leave behind, so its rule is
//! narrower: none may be handed to the kernel for a session the reset
//! has ended, and a reset must not return while one it admitted is
//! still on its way.

use super::*;

use std::sync::atomic::AtomicU64;
use vmm_devices::pci::msix::{MsiSink, MsixTable};
use vmm_devices::pci::INTxPinID;

/// Size of one MSI-X table entry, so a test can reach the control
/// dword of a chosen vector.
const ENTRY_SIZE: usize = 16;

/// An interrupt pin a test can park inside, the way a slow kernel call
/// parks a real one.
struct TestPin {
    level: AtomicBool,
    asserts: AtomicUsize,
    park: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Every assert takes this long while set, so a flood keeps
    /// injections in flight instead of leaving gaps a reset can slip
    /// through by luck.
    slow: AtomicBool,
}

impl TestPin {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            level: AtomicBool::new(false),
            asserts: AtomicUsize::new(0),
            park: Mutex::new(None),
            slow: AtomicBool::new(false),
        })
    }

    /// Park the next assert, before the level moves.
    fn park_in_assert(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.park.lock().expect("pin park poisoned") = Some(hook);
    }

    fn asserts(&self) -> usize {
        self.asserts.load(Ordering::Acquire)
    }

    /// Make every assert take about as long as a contended kernel
    /// call.
    fn make_slow(&self) {
        self.slow.store(true, Ordering::Release);
    }
}

impl IntrPin for TestPin {
    fn assert(&self) {
        let hook = self.park.lock().expect("pin park poisoned").take();
        if let Some(hook) = hook {
            hook();
        }
        if self.slow.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(5));
        }
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

/// A sink that records what reached the kernel, and can be parked
/// inside the send.
struct TestSink {
    sent: Mutex<Vec<(u64, u64)>>,
    /// Messages the sink has finished delivering.
    delivered: AtomicUsize,
    park: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl TestSink {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            sent: Mutex::new(Vec::new()),
            delivered: AtomicUsize::new(0),
            park: Mutex::new(None),
        })
    }

    fn park_in_send(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.park.lock().expect("sink park poisoned") = Some(hook);
    }

    fn delivered(&self) -> usize {
        self.delivered.load(Ordering::Acquire)
    }

    fn sent(&self) -> Vec<(u64, u64)> {
        self.sent.lock().expect("sink lock poisoned").clone()
    }
}

impl MsiSink for TestSink {
    fn send(&self, addr: u64, data: u64) {
        let hook = self.park.lock().expect("sink park poisoned").take();
        if let Some(hook) = hook {
            hook();
        }
        self.sent
            .lock()
            .expect("sink lock poisoned")
            .push((addr, data));
        self.delivered.fetch_add(1, Ordering::AcqRel);
    }
}

type Wired = (Dev, mpsc::Receiver<()>, mpsc::Sender<()>, Arc<TestPin>);

/// Build a device on a real interrupt pin, with the MSI-X table a
/// guest that uses MSI-X would leave behind.
fn wired_device(msix: Option<(Arc<MsixTable>, Option<u16>)>) -> Wired {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let pin = TestPin::new();
    let table = msix.as_ref().map(|(t, _)| Arc::clone(t));
    let dev = VirtioPciDevice::new(
        DrainDevice {
            log: Mutex::default(),
            entered: entered_tx,
            release: Mutex::new(release_rx),
            resets: AtomicUsize::new(0),
            reset_thread: Mutex::new(None),
            off_the_ring: AtomicBool::new(true),
        },
        bits::VIRTIO_DEV_TYPE_BLOCK,
        1,
        256,
        8,
        Some((INTxPinID::IntA, Arc::clone(&pin) as Arc<dyn IntrPin>)),
        Arc::new(PhysMap::new()),
        Arc::new(PioBus::new()),
        Arc::new(MmioBus::new()),
        table,
    );
    if let Some((_, Some(vector))) = msix {
        dev.msix_queue_vectors[0].store(vector, Ordering::Release);
    }
    (dev, entered_rx, release_tx, pin)
}

/// A guest with no MSI-X: every interrupt goes out on the pin.
fn wired_intx() -> Wired {
    wired_device(None)
}

/// A guest that enabled MSI-X, with `vector` mapped to queue 0.
fn wired_msix(vector: Option<u16>) -> (Wired, Arc<MsixTable>, Arc<TestSink>) {
    let sink = TestSink::new();
    let table =
        Arc::new(MsixTable::new(4, Arc::clone(&sink) as Arc<dyn MsiSink>));
    table.set_enabled(true);
    // A vector the driver programmed, so `fire` has somewhere to send.
    table.write_entry(1, 0xFEE0_0000, 0x21);
    let wired = wired_device(Some((Arc::clone(&table), vector)));
    (wired, table, sink)
}

/// Let the backend through its reset as soon as it gets there, so a
/// test can drive a whole reset from one thread.
fn release_on_drain(entered: mpsc::Receiver<()>, release: mpsc::Sender<()>) {
    std::thread::spawn(move || {
        entered.recv_timeout(WEDGED).expect("the backend drained");
        release.send(()).expect("release the drain");
    });
}

/// Write the MSI-X table the way a guest does, through the BAR.
fn write_table(dev: &Dev, offset: usize, val: u32) {
    let wo = WriteOp::from_buf(&val.to_le_bytes());
    dev.bar_rw(BarN::BAR4, offset, RWOp::Write(&wo));
}

/// Poll until `done`, or give up once the deadline passes. Never parks
/// the run: the caller releases whatever it parked either way.
fn wait_for(done: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + WEDGED;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    false
}

// The wedge, in one interleaving. A worker sets the ISR and is caught
// on its way into the pin. A driver's handler reads the ISR, gets the
// bit, and lowers the line. The worker then asserts. If those two are
// not one step, the device ends with ISR zero and the pin held up: the
// next read claims nothing and the level never comes down again.
#[test]
fn an_isr_read_and_a_raise_do_not_split_the_line() {
    let (dev, _entered, _release, pin) = wired_intx();
    let (hook, arrived, go) = park();
    pin.park_in_assert(hook);

    let worker = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || dev.raise_queue_interrupt_current(0))
    };
    arrived
        .recv_timeout(WEDGED)
        .expect("the raise reached the pin");

    let claimed = Arc::new(AtomicU64::new(u64::MAX));
    let reader = {
        let dev = Arc::clone(&dev);
        let claimed = Arc::clone(&claimed);
        std::thread::spawn(move || {
            claimed.store(u64::from(isr(&dev)), Ordering::Release)
        })
    };
    // Long enough for a wrong build to run the whole handler ahead of
    // the parked assert.
    std::thread::sleep(SETTLE);
    go.send(()).expect("release the raise");
    worker.join().expect("the raise finished");
    reader.join().expect("the handler finished");

    assert_ne!(
        claimed.load(Ordering::Acquire),
        0,
        "the driver's handler read a zero ISR for an interrupt it was sent"
    );
    assert!(
        !pin.is_asserted(),
        "the line stayed asserted behind an ISR the driver had cleared"
    );
}

// A reset must not return while an injection it admitted is still on
// its way to the kernel. The message here has no level to leave
// behind, so nothing else can catch it: the only evidence is whether
// the send had finished when the register write returned.
#[test]
fn a_reset_waits_out_the_injection_it_admitted() {
    let ((dev, entered, release, _pin), _table, sink) = wired_msix(Some(1));
    let (hook, arrived, go) = park();
    sink.park_in_send(hook);
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);

    let worker = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || dev.raise_queue_interrupt_current(0))
    };
    arrived
        .recv_timeout(WEDGED)
        .expect("the raise reached the kernel");

    // The reset, on its own vCPU, so the test can release the park.
    let resetter = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || write_status(&dev, 0))
    };
    release_on_drain(entered, release);
    // Give a build with no settle every chance to finish the reset
    // while the message is still in flight.
    std::thread::sleep(SETTLE);
    let finished_early = resetter.is_finished();
    go.send(()).expect("release the injection");
    worker.join().expect("the raise finished");
    resetter.join().expect("the reset finished");

    assert!(
        !finished_early,
        "the reset returned with an injection it admitted still in flight"
    );
    assert_eq!(sink.delivered(), 1, "the injection was never delivered");
}

// The session check itself. A raise that names the session a reset
// ended is refused outright, whether it would have gone out on the pin
// or as a message.
#[test]
fn a_raise_naming_the_old_session_is_refused() {
    // Both routes, because the refusal has to happen before either is
    // chosen.
    let ((msix_dev, entered, release, _pin), _table, sink) =
        wired_msix(Some(1));
    let (dev, intx_entered, intx_release, pin) = wired_intx();
    for d in [&msix_dev, &dev] {
        write_status(d, READY | bits::STATUS_DRIVER_OK);
    }
    let old = dev.intr.session();
    assert_eq!(old, msix_dev.intr.session());

    release_on_drain(entered, release);
    write_status(&msix_dev, 0);
    release_on_drain(intx_entered, intx_release);
    write_status(&dev, 0);

    msix_dev.raise_queue_interrupt_in(old, 0);
    msix_dev.raise_config_interrupt_in(old);
    dev.raise_queue_interrupt_in(old, 0);
    dev.raise_config_interrupt_in(old);

    assert_eq!(sink.sent(), Vec::new(), "the old session sent a message");
    assert!(!pin.is_asserted(), "the old session put the line back up");
    assert_eq!(isr(&dev), 0, "the old session set the ISR");
    assert_eq!(pin.asserts(), 0, "the old session reached the pin at all");
}

// Admission is shut for the whole reset, not just for the session
// bump. A backend worker reaching the transport while the reset is
// between its drain and its cleanup gets nothing: raising there would
// set the ISR the reset is about to clear, and assert the line it is
// about to lower.
#[test]
fn a_raise_during_the_reset_is_refused() {
    let (dev, entered, release, pin) = wired_intx();
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);

    let resetter = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || write_status(&dev, 0))
    };
    entered.recv_timeout(WEDGED).expect("the backend drained");

    // The worker's own epoch check passed before the reset started.
    dev.raise_queue_interrupt_current(0);
    let reached = pin.asserts();

    release.send(()).expect("release the drain");
    resetter.join().expect("the reset finished");

    assert_eq!(reached, 0, "a raise ran while the reset held the device");
    assert!(!pin.is_asserted(), "the reset returned with the line up");
    assert_eq!(isr(&dev), 0, "the reset returned with the ISR set");
}

// VirtIO 1.3 sec 4.1.5.1.2: with MSI-X enabled, a queue mapped to
// NO_VECTOR is told nothing. Falling back to the pin asserts a line an
// MSI-X driver registered no handler for, so it never reads the ISR
// and the level stays up for the life of the VM. A reset leaves every
// vector at NO_VECTOR, which is exactly when this happens.
#[test]
fn an_msix_driver_with_no_vector_is_not_put_on_the_pin() {
    let ((dev, _entered, _release, pin), _table, sink) = wired_msix(None);
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);

    dev.raise_queue_interrupt_current(0);
    // The config route has the same rule, and a reset leaves its
    // vector at NO_VECTOR too.
    dev.signal_device_needs_reset();

    assert_eq!(pin.asserts(), 0, "an MSI-X driver was put on the INTx pin");
    assert!(!pin.is_asserted(), "the line went up with no handler on it");
    assert_eq!(
        isr(&dev),
        0,
        "the ISR was set for a driver that never reads it"
    );
    assert_eq!(sink.sent(), Vec::new(), "a message went out with no vector");
}

// A message the old session left in the pending array leaves on the
// next unmask, which is a guest write to the table BAR that no reset
// gates. It carries the address and data of a driver that is gone.
#[test]
fn a_reset_drops_the_messages_the_old_session_left_pending() {
    let ((dev, entered, release, _pin), _table, sink) = wired_msix(Some(1));
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);

    // The driver masks its vector, then the device raises on it.
    write_table(&dev, ENTRY_SIZE + 12, 1);
    dev.raise_queue_interrupt_current(0);
    assert_eq!(sink.sent(), Vec::new(), "a masked vector sent a message");

    release_on_drain(entered, release);
    write_status(&dev, 0);

    // The next driver unmasks the same vector.
    write_table(&dev, ENTRY_SIZE + 12, 0);
    assert_eq!(
        sink.sent(),
        Vec::new(),
        "an unmask delivered a message the old session raised"
    );
}

// The same message, taken out one step earlier. An unmask is a guest
// write to the table BAR, which no reset latch covers, so the reset
// can be under way when the message leaves. Admission is what stops
// it: taken before the send, it either puts the send inside the set
// the reset waits out or refuses it outright.
#[test]
fn an_unmask_during_a_reset_delivers_nothing() {
    let ((dev, entered, release, _pin), _table, sink) = wired_msix(Some(1));
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);
    write_table(&dev, ENTRY_SIZE + 12, 1);
    dev.raise_queue_interrupt_current(0);

    let resetter = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || write_status(&dev, 0))
    };
    entered.recv_timeout(WEDGED).expect("the backend drained");

    // The guest unmasks while the reset holds the device.
    write_table(&dev, ENTRY_SIZE + 12, 0);
    let sent = sink.sent();

    release.send(()).expect("release the drain");
    resetter.join().expect("the reset finished");

    assert_eq!(
        sent,
        Vec::new(),
        "an unmask during the reset delivered the old session's message"
    );
}

// The hostile case the gate exists for: a guest that keeps every vCPU
// it has raising interrupts while one of them resets the device. Each
// injection is made slow, so the flood keeps one in flight at all
// times and the reset cannot get through on a lucky gap. What stops
// the flood holding the resetting vCPU is that admission shuts before
// the wait: the set the reset waits out is closed the moment it starts
// waiting.
#[test]
fn a_flood_of_raises_does_not_hold_the_resetting_vcpu() {
    let (dev, entered, release, pin) = wired_intx();
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);
    pin.make_slow();

    let stop = Arc::new(AtomicBool::new(false));
    let floods: Vec<_> = (0..8)
        .map(|_| {
            let dev = Arc::clone(&dev);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    dev.raise_queue_interrupt_current(0);
                    isr(&dev);
                }
            })
        })
        .collect();
    assert!(wait_for(|| pin.asserts() > 8), "the flood never got going");

    release_on_drain(entered, release);
    // The reset runs on its own thread under a deadline. One the flood
    // can hold would park the run instead of failing it: only this
    // thread stops the flood.
    let done = Arc::new(AtomicBool::new(false));
    let resetter = {
        let dev = Arc::clone(&dev);
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            write_status(&dev, 0);
            done.store(true, Ordering::Release);
        })
    };
    let start = Instant::now();
    while !done.load(Ordering::Acquire) && start.elapsed() < RESET_BUDGET {
        std::thread::sleep(Duration::from_micros(20));
    }
    let waited = start.elapsed();

    stop.store(true, Ordering::Release);
    resetter.join().expect("the reset finished");
    for t in floods {
        t.join().expect("a flooding thread finished");
    }

    assert!(
        waited < RESET_BUDGET,
        "a raise flood held the resetting vCPU for {waited:?}"
    );
    assert_eq!(
        dev.intr.session().raw(),
        1,
        "the reset never ended the driver session"
    );
}

// The narrower half of the same problem. The unmask can be caught
// between taking the message out of the pending array and sending it,
// and a whole reset can run in that window: admission shuts and
// reopens, so "is admission open now" says yes again. Only the session
// the unmask sampled before it took the message tells the two drivers
// apart.
#[test]
fn an_unmask_overtaken_by_a_whole_reset_delivers_nothing() {
    let ((dev, entered, release, _pin), _table, sink) = wired_msix(Some(1));
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);
    write_table(&dev, ENTRY_SIZE + 12, 1);
    dev.raise_queue_interrupt_current(0);
    assert_eq!(sink.sent(), Vec::new(), "a masked vector sent a message");

    let (hook, arrived, go) = park();
    dev.park_before_released_delivery(hook);
    let unmasker = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || write_table(&dev, ENTRY_SIZE + 12, 0))
    };
    arrived
        .recv_timeout(WEDGED)
        .expect("the unmask took the message");

    // The whole reset, start to finish, while the message waits.
    release_on_drain(entered, release);
    write_status(&dev, 0);
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);

    go.send(()).expect("release the unmask");
    unmasker.join().expect("the unmask finished");

    assert_eq!(
        sink.sent(),
        Vec::new(),
        "an unmask from the old session reached the driver that followed"
    );
}

// The interleaving a device backend cannot avoid. It authorises work
// under one driver session and raises for it on another thread. A
// whole reset can run in between: admission shuts and reopens, so "is
// admission open now" says yes again on the far side of one. Only the
// session the backend held when the work was authorised tells the two
// drivers apart.
#[test]
fn a_backend_raise_overtaken_by_a_whole_reset_delivers_nothing() {
    let (dev, entered, release, pin) = wired_intx();
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);
    let intr = dev.backend_intr();
    // The session a completion authorised now would carry.
    let armed = intr.session();

    // The whole reset, start to finish, and the next driver up.
    release_on_drain(entered, release);
    write_status(&dev, 0);
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);

    intr.raise(armed, 0);
    assert_eq!(
        pin.asserts(),
        0,
        "a completion from the closed session reached the driver that \
         followed"
    );
    assert!(!pin.is_asserted(), "the old session put the line back up");
    assert_eq!(isr(&dev), 0, "the old session set the ISR");

    // The other half, and it is the worse failure of the two: a
    // completion of the session running now has to get through, or the
    // device has traded a spurious interrupt for a guest that waits
    // for ever.
    intr.raise(intr.session(), 0);
    assert_eq!(pin.asserts(), 1, "the driver that is running got nothing");
    assert_ne!(isr(&dev), 0, "the running driver's ISR was left clear");
}

// The same interleaving run concurrently rather than in sequence. A backend authorises work under one session and the thread
// that will raise for it is parked on the far side of that handoff,
// before the transport admits anything. A whole reset then runs to
// completion on the vCPU thread and the next driver comes up.
//
// Two properties, and a wrong fix breaks the second one. Nothing from
// the parked thread may reach the driver that followed; and the reset
// must not wait for that thread, because a raise the gate never
// admitted is not in the set `settle` closes over. A reset that waited
// for it would be bounded by a backend, which is the other half of the
// same finding.
#[test]
fn a_raise_parked_across_a_whole_reset_is_refused_and_never_holds_it() {
    let (dev, entered, release, pin) = wired_intx();
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);
    let intr = dev.backend_intr();
    // The session a completion built now would carry to its raise.
    let armed = intr.session();

    let (hook, arrived, go) = park();
    let raiser = {
        let intr = Arc::clone(&intr);
        std::thread::spawn(move || {
            hook();
            intr.raise(armed, 0);
        })
    };
    arrived
        .recv_timeout(WEDGED)
        .expect("the raise parked before admission");

    release_on_drain(entered, release);
    let started = Instant::now();
    write_status(&dev, 0);
    let reset_took = started.elapsed();
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);
    assert!(
        reset_took < RESET_BUDGET,
        "the reset waited {reset_took:?} on a raise it never admitted",
    );

    go.send(()).expect("release the raise");
    raiser.join().expect("the raise finished");

    assert_eq!(
        pin.asserts(),
        0,
        "a raise held over from the closed session reached the driver that \
         followed"
    );
    assert!(!pin.is_asserted(), "the old session left the line up");
    assert_eq!(isr(&dev), 0, "the old session set the new driver's ISR");

    // The converse, and it is the worse failure of the two: the driver
    // running now must still be interrupted for its own work, or the
    // device has traded a spurious interrupt for a guest that waits
    // for ever.
    let live = intr.session();
    let (hook, arrived, go) = park();
    let raiser = {
        let intr = Arc::clone(&intr);
        std::thread::spawn(move || {
            hook();
            intr.raise(live, 0);
        })
    };
    arrived.recv_timeout(WEDGED).expect("the live raise parked");
    go.send(()).expect("release the live raise");
    raiser.join().expect("the live raise finished");

    assert_eq!(
        pin.asserts(),
        1,
        "the driver that is running got no interrupt for its own work"
    );
    assert_ne!(isr(&dev), 0, "the running driver's ISR was left clear");
}

// The boundary the session check must not cross. A raise the gate
// admitted BEFORE the reset started is already the reset's to wait
// out, and it belongs to the driver that asked for it. Refusing it
// would lose an interrupt the guest is entitled to; letting the reset
// return while it is still on its way leaves a message from the old
// session arriving in the new one.
//
// This goes out as an MSI-X message on purpose. The INTx path holds
// the line guard across the pin, so a reset there is held by that lock
// whether or not the gate counted the delivery, and the test would
// pass for the wrong reason. A message has no line and no level, so
// the only thing that can hold the reset is the admission the raise
// took, and the only evidence is whether the send had finished.
#[test]
fn a_session_raise_admitted_before_the_reset_is_delivered_and_waited_out() {
    let ((dev, entered, release, _pin), _table, sink) = wired_msix(Some(1));
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);
    let intr = dev.backend_intr();
    let armed = intr.session();

    let (hook, arrived, go) = park();
    sink.park_in_send(hook);
    let raiser = {
        let intr = Arc::clone(&intr);
        std::thread::spawn(move || intr.raise(armed, 0))
    };
    arrived
        .recv_timeout(WEDGED)
        .expect("the admitted raise reached the kernel");

    let resetter = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || write_status(&dev, 0))
    };
    release_on_drain(entered, release);
    // Every chance for a build that does not hold the admission across
    // the delivery to finish the reset while the message is in flight.
    std::thread::sleep(SETTLE);
    let finished_early = resetter.is_finished();

    go.send(()).expect("release the admitted raise");
    raiser.join().expect("the admitted raise finished");
    resetter.join().expect("the reset finished");

    assert!(
        !finished_early,
        "the reset returned with a message it admitted still in flight"
    );
    assert_eq!(
        sink.delivered(),
        1,
        "the driver that asked for this interrupt never got it"
    );
}

// The post-restore kick is not on a vCPU. It runs on its own thread
// for about a second after the vCPUs start (bin/rshyve's
// post-migrate-virtio-kick), so a migrated guest that re-probes the
// device in that window resets it underneath the kick. What the kick
// then raises belongs to the queue set the previous driver had
// enabled, and the next driver never armed it: on INTx a level nobody
// lowers, on MSI-X a message to whatever vector it had just
// programmed.
fn a_reset_refuses_the_post_restore_kick_it_overtook(msix: bool) {
    let (wired, _table, sink) = if msix {
        let (w, t, s) = wired_msix(Some(1));
        (w, Some(t), Some(s))
    } else {
        (wired_intx(), None, None)
    };
    let (dev, entered, release, pin) = wired;
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);
    // The queue set a restore leaves behind. How it was enabled is not
    // what this test is about.
    dev.virtio_state.lock().expect("virtio lock").queue_enabled[0] = true;

    let (hook, arrived, go) = park();
    dev.park_before_restore_kick(hook);
    let kicker = {
        let dev = Arc::clone(&dev);
        std::thread::spawn(move || dev.post_restore_kick())
    };
    arrived
        .recv_timeout(WEDGED)
        .expect("the kick dropped the transport lock");

    // A whole reset, and then the next driver, while the kick is held.
    // A reset that waited for this thread would never return, which is
    // why the kick is not admitted before it parks.
    release_on_drain(entered, release);
    write_status(&dev, 0);
    write_status(&dev, READY | bits::STATUS_DRIVER_OK);
    if msix {
        // A reset leaves every vector at NO_VECTOR, so without this the
        // MSI-X silence below would prove nothing.
        dev.msix_queue_vectors[0].store(1, Ordering::Release);
    }

    go.send(()).expect("release the kick");
    kicker.join().expect("the kick finished");

    let delivered =
        pin.asserts() + sink.as_ref().map_or(0, |sink| sink.sent().len());
    assert_eq!(
        delivered, 0,
        "a kick for the old driver's queues reached the next driver",
    );
    assert_eq!(isr(&dev), 0, "the old session set the ISR");
    assert!(!pin.is_asserted(), "the old session left the line asserted",);

    // The converse, on the same device and the same queue. A migrated
    // guest that never gets this kick leaves its rx ring unreplenished.
    dev.virtio_state.lock().expect("virtio lock").queue_enabled[0] = true;
    dev.post_restore_kick();
    let delivered =
        pin.asserts() + sink.as_ref().map_or(0, |sink| sink.sent().len());
    assert_eq!(delivered, 1, "the running session's own kick was dropped");
}

#[test]
fn a_reset_refuses_the_post_restore_kick_it_overtook_on_intx() {
    a_reset_refuses_the_post_restore_kick_it_overtook(false);
}

#[test]
fn a_reset_refuses_the_post_restore_kick_it_overtook_on_msix() {
    a_reset_refuses_the_post_restore_kick_it_overtook(true);
}
