// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Every interrupt this transport delivers, and the reset that ends
//! the driver it was raised for.
//!
//! A device backend authorises work on one thread and completes it on
//! another. Between those two points a guest can reset the device, so
//! the completion must not be admitted by asking "is a session open
//! now". It names the session it was authorised under, and
//! [`IntrGate::begin`] tests that name in the same step that admits
//! the delivery.

use std::sync::{Arc, Condvar, Mutex, PoisonError};

use super::bits;
use super::{VirtioDevice, VirtioPciDevice};

use vmm_devices::pci::msix::MsixTable;

/// One driver session, as a device backend names it.
///
/// The transport mints it and a backend keeps the value it held when
/// its work was authorised. Handing that value back at completion is
/// what lets [`IntrGate::begin`] tell "a session is open" apart from
/// "the session this work belongs to is still open". The two differ
/// for exactly as long as one whole reset, which is all a parked
/// completion thread needs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IntrSession(u64);

impl IntrSession {
    /// The session a device runs in before any reset.
    pub const INITIAL: Self = Self(0);

    /// The raw counter, for a backend that stores it in an atomic.
    pub(crate) const fn raw(self) -> u64 {
        self.0
    }

    /// The session `raw` names. The counter is transport-private, so
    /// only a value that came from [`Self::raw`] belongs here.
    pub(crate) const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

/// The transport interrupt path, as a device backend uses it.
///
/// A backend gets one after the transport is built and keeps it for
/// the life of the device. [`Self::session`] names the session work
/// authorised now belongs to; [`Self::raise`] hands that name back and
/// is refused if the session has since ended.
pub struct BackendIntr {
    gate: Arc<IntrGate>,
    raise: Box<dyn Fn(IntrSession, u16) + Send + Sync>,
}

impl BackendIntr {
    pub(crate) fn new(
        gate: Arc<IntrGate>,
        raise: impl Fn(IntrSession, u16) + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            gate,
            raise: Box::new(raise),
        })
    }

    /// The session a backend must name for work it authorises now.
    ///
    /// A reset ends the transport session before it calls the backend
    /// reset, so a backend that reads this from inside its own reset
    /// gets the session the next driver runs under.
    pub fn session(&self) -> IntrSession {
        self.gate.session()
    }

    /// Raise for `queue_idx`, unless `session` has ended.
    pub fn raise(&self, session: IntrSession, queue_idx: u16) {
        (self.raise)(session, queue_idx);
    }

    /// A path with a session counter of its own, for a test that has
    /// no transport to raise through.
    #[cfg(test)]
    pub(crate) fn detached(
        raise: impl Fn(IntrSession, u16) + Send + Sync + 'static,
    ) -> Arc<Self> {
        Self::new(Arc::new(IntrGate::new()), raise)
    }

    /// A path onto `gate` that records what the gate admits.
    ///
    /// The record sits behind admission, where the transport's own
    /// delivery sits, so a test sees what would have reached the
    /// guest and not what a backend asked for.
    #[cfg(test)]
    pub(crate) fn recording(
        gate: Arc<IntrGate>,
        seen: Arc<Mutex<Vec<(IntrSession, u16)>>>,
    ) -> Arc<Self> {
        let admit = Arc::clone(&gate);
        Self::new(gate, move |session, queue_idx| {
            let Some(_admitted) = admit.begin(session) else {
                return;
            };
            seen.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((session, queue_idx));
        })
    }
}

/// The transport session the next driver runs under.
///
/// A backend reads this from inside its own reset, or while it builds
/// the completion handler for a ring. The transport ends its session
/// before it calls the backend reset, so the value read there is the
/// new session and not the one that just ended.
///
/// `fallback` stands until the binary wires the transport up. Nothing
/// can be raised before that, so the value only has to be stable.
pub(crate) fn next_intr_session(
    slot: &Mutex<Option<Arc<BackendIntr>>>,
    fallback: IntrSession,
) -> IntrSession {
    slot.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map_or(fallback, |intr| intr.session())
}

/// Orders every interrupt this device delivers against its own reset.
///
/// A device reset runs on the vCPU that wrote DEVICE_STATUS and must
/// return with the ISR clear and this device off the interrupt line.
/// It cannot recall an injection another thread has already handed to
/// the kernel, so it does the one thing it can: it shuts the door,
/// waits out the injections already through it, and only then clears
/// what it owns.
///
/// Two mechanisms, because the two halves need different shapes.
///
/// `admission` decides which deliveries start. A raise names the
/// driver session it was raised for, and a reset ends that session and
/// shuts admission before anything else. No holder of this lock calls
/// the kernel, so a resetting vCPU never queues behind an injection to
/// take it. [`Self::settle`] then waits for the deliveries already
/// admitted. That set is closed and small: one injection per thread
/// that was already inside, doing nothing else, and a guest cannot add
/// threads. It is the "one write, no poll" contract that makes the
/// wait right rather than merely tolerable, and VirtIO 1.3 sec 2.4.1
/// asks for the same order.
///
/// `line` keeps the ISR byte and the INTx level from disagreeing. They
/// are one piece of state: a driver that reads a zero ISR claims
/// nothing and lowers nothing, so a level left asserted behind a zero
/// ISR wedges the line for every device sharing it. This one IS held
/// across the pin ioctl, and it has to be, but every holder does
/// exactly one injection and no more.
pub(crate) struct IntrGate {
    admission: Mutex<Admission>,
    settled: Condvar,
    /// A cheap read of `Admission::session` for a caller that only
    /// samples it. Staleness is safe: [`Self::begin`] rechecks under
    /// the lock, and a stale sample can only be refused.
    session: std::sync::atomic::AtomicU64,
    line: Mutex<()>,
}

struct Admission {
    /// Whether deliveries are being admitted. Shut for the whole
    /// reset, not just for the session bump: one admitted after the
    /// settle would still be racing the cleanup behind it.
    open: bool,
    /// Counts driver sessions, one more each time a reset takes the
    /// device down.
    session: u64,
    /// Deliveries admitted and not yet finished.
    in_flight: usize,
}

/// One admitted delivery, counted until it is dropped.
struct Admitted<'a> {
    gate: &'a IntrGate,
}

impl Drop for Admitted<'_> {
    fn drop(&mut self) {
        let mut a = self
            .gate
            .admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        a.in_flight -= 1;
        if a.in_flight == 0 {
            self.gate.settled.notify_all();
        }
    }
}

impl IntrGate {
    pub(crate) fn new() -> Self {
        Self {
            admission: Mutex::new(Admission {
                open: true,
                session: IntrSession::INITIAL.0,
                in_flight: 0,
            }),
            settled: Condvar::new(),
            session: std::sync::atomic::AtomicU64::new(IntrSession::INITIAL.0),
            line: Mutex::new(()),
        }
    }

    /// The session a caller must name to be admitted.
    pub(crate) fn session(&self) -> IntrSession {
        IntrSession(self.session.load(std::sync::atomic::Ordering::Acquire))
    }

    /// Admit one delivery for `session`.
    ///
    /// The session test and the count are one step under this lock, so
    /// a caller parked between naming its session and arriving here
    /// cannot be admitted into the session that replaced it.
    fn begin(&self, session: IntrSession) -> Option<Admitted<'_>> {
        let mut a = self
            .admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !a.open || a.session != session.0 {
            return None;
        }
        a.in_flight += 1;
        Some(Admitted { gate: self })
    }

    /// Admit one delivery for whichever session is current.
    fn begin_current(&self) -> Option<Admitted<'_>> {
        let mut a = self
            .admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !a.open {
            return None;
        }
        a.in_flight += 1;
        Some(Admitted { gate: self })
    }

    /// End the driver session and shut admission.
    pub(crate) fn end_session(&self) {
        let mut a = self
            .admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        a.open = false;
        a.session += 1;
        self.session
            .store(a.session, std::sync::atomic::Ordering::Release);
    }

    /// Wait for the deliveries admission let through before it shut.
    pub(crate) fn settle(&self) {
        let mut a = self
            .admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while a.in_flight != 0 {
            a = self.settled.wait(a).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Admit the next driver.
    pub(crate) fn reopen(&self) {
        let mut a = self
            .admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        a.open = true;
    }

    /// Hold the ISR byte and the INTx level together.
    pub(crate) fn line(&self) -> std::sync::MutexGuard<'_, ()> {
        self.line.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<D: VirtioDevice> VirtioPciDevice<D> {
    /// The interrupt path a backend of this device completes through.
    ///
    /// The reference back to the transport is weak: the backend lives
    /// inside the transport, so an owning one would keep the pair
    /// alive for as long as the process.
    pub fn backend_intr(self: &Arc<Self>) -> Arc<BackendIntr> {
        let weak = Arc::downgrade(self);
        BackendIntr::new(Arc::clone(&self.intr), move |session, queue_idx| {
            if let Some(dev) = weak.upgrade() {
                dev.raise_queue_interrupt_in(session, queue_idx);
            }
        })
    }

    /// Assert the PCI interrupt pin.
    ///
    /// PCI INTx is level-triggered: assert while work is pending, and
    /// deassert when the guest reads the ISR. There is no deassert
    /// here. A deassert plus an assert makes a second edge, and the
    /// guest sees that edge with ISR=0, reports "nobody cared" and
    /// disables the IRQ.
    ///
    /// Private to this module. It raises the line with no session
    /// test, so a backend that reached it would deliver into the
    /// driver that followed a reset. Backends go through
    /// [`BackendIntr::raise`], which names the session it was
    /// authorised under.
    fn raise_interrupt(&self) {
        if let Some(pin) = &self.intr_pin {
            pin.assert();
        }
    }

    /// Set the queue bit in the ISR and assert the pin.
    ///
    /// The ISR is an AtomicU8, so a worker and a vCPU can set it at
    /// the same time without taking the transport lock on the hot
    /// path.
    ///
    /// Private for the reason [`Self::raise_interrupt`] is.
    fn raise_queue_interrupt(&self) {
        let _line = self.intr.line();
        self.isr_status.fetch_or(
            bits::ISR_QUEUE_INTR,
            std::sync::atomic::Ordering::Release,
        );
        self.raise_interrupt();
    }

    /// Read and clear the ISR the way a legacy driver's handler does.
    ///
    /// The read, the clear and the pin are one step under the line
    /// guard. Split apart they disagree: a raise landing between the
    /// swap and the deassert leaves the ISR at zero with the pin held
    /// up, and a level-triggered driver that reads zero claims
    /// nothing and never lowers it again.
    pub(super) fn read_isr(&self) -> u8 {
        let _line = self.intr.line();
        let val = self.isr_status.swap(0, std::sync::atomic::Ordering::AcqRel);
        if val != 0 {
            self.lower_interrupt();
        }
        val
    }

    /// Deassert the interrupt after ISR is read or on device reset.
    pub(super) fn lower_interrupt(&self) {
        if let Some(pin) = &self.intr_pin {
            pin.deassert();
        }
    }

    /// Raise an interrupt for a specific queue, under whichever
    /// session is current.
    ///
    /// One caller: the migration restore, which raises on the session
    /// it has just built, on the restore thread, before any vCPU
    /// exists to write DEVICE_STATUS. Every other caller can be parked
    /// between naming its work and arriving here, and "is a session
    /// open now" says yes again on the far side of a whole reset, so
    /// it must use [`Self::raise_queue_interrupt_in`] instead.
    ///
    /// Private to the transport for that reason. A backend reaches the
    /// interrupt path through [`BackendIntr::raise`], which carries
    /// the session its work was authorised under.
    pub(super) fn raise_queue_interrupt_current(&self, queue_idx: u16) {
        let Some(_admitted) = self.intr.begin_current() else {
            return;
        };
        self.deliver_queue_interrupt(queue_idx);
    }

    /// Raise an interrupt for a specific queue, for the driver
    /// session that asked for it.
    ///
    /// A reset ends the session and shuts admission before it clears
    /// the ISR and lowers the line, so a raise is either whole and
    /// before that cleanup or refused outright. `session` is the one
    /// the caller was authorised under, not the one running now, so a
    /// whole reset in between refuses this rather than writing it onto
    /// the driver that followed.
    pub fn raise_queue_interrupt_in(
        &self,
        session: IntrSession,
        queue_idx: u16,
    ) {
        let Some(_admitted) = self.intr.begin(session) else {
            return;
        };
        self.deliver_queue_interrupt(queue_idx);
    }

    /// Route a queue interrupt the way the driver armed it.
    fn deliver_queue_interrupt(&self, queue_idx: u16) {
        // Read vector from lock-free atomic cache (no virtio_state lock
        // needed, so this is safe to call from inside notify_queue
        // callbacks).
        let vector = self
            .msix_queue_vectors
            .get(queue_idx as usize)
            .map(|a| a.load(std::sync::atomic::Ordering::Acquire))
            .unwrap_or(bits::VIRTIO_MSI_NO_VECTOR);
        if self.route_msix(vector) {
            return;
        }
        // MSI-X absent or not enabled, so the driver is on INTx. This
        // is normal operation, not an error: a guest built without
        // CONFIG_PCI_MSI never assigns a vector.
        self.raise_queue_interrupt();
    }

    /// Deliver a configuration interrupt the way the driver armed it.
    ///
    /// WHY one helper for both callers: a modern guest with MSI-X never
    /// reads the ISR, because it registers no INTx handler. Falling
    /// through to the pin there tells that driver nothing and leaves
    /// the level-triggered line asserted for the life of the VM, which
    /// takes down every other device sharing it.
    fn raise_config_interrupt(&self) {
        let vector = self
            .msix_config_vector
            .load(std::sync::atomic::Ordering::Acquire);
        if self.route_msix(vector) {
            return;
        }
        let _line = self.intr.line();
        self.isr_status.fetch_or(
            bits::ISR_CFG_CHANGE,
            std::sync::atomic::Ordering::Release,
        );
        self.raise_interrupt();
    }

    /// Deliver the messages an unmask released, under the same
    /// admission the device's own raises take.
    ///
    /// These leave on a guest write to the MSI-X table or capability,
    /// which no reset latch covers, and they carry the address and
    /// data of whichever driver recorded them. `session` is sampled
    /// before the write takes them out of the pending array, so a
    /// reset that lands anywhere from that point on refuses them; and
    /// one admitted before the reset started is inside the set the
    /// reset waits out.
    pub(super) fn deliver_released(
        &self,
        session: IntrSession,
        msix: &MsixTable,
        released: impl IntoIterator<Item = (u64, u64)>,
    ) {
        let mut released = released.into_iter().peekable();
        if released.peek().is_none() {
            return;
        }
        #[cfg(test)]
        super::run_park(&self.parks.released_delivery_pending);
        let Some(_admitted) = self.intr.begin(session) else {
            return;
        };
        for (addr, data) in released {
            msix.send_message(addr, data);
        }
    }

    /// Send `vector` as a message, if the driver is on MSI-X.
    ///
    /// Returns whether MSI-X answered for this interrupt. It answers
    /// even for `VIRTIO_MSI_NO_VECTOR`, where the answer is silence:
    /// VirtIO 1.3 sec 4.1.5.1.2 says a vector set to NO_VECTOR gets no
    /// interrupt, and an MSI-X driver registers no INTx handler, so
    /// falling through to the pin raises a line nobody will ever lower
    /// and wedges it for every device sharing it. A reset leaves every
    /// vector at NO_VECTOR, which is when this matters most.
    fn route_msix(&self, vector: u16) -> bool {
        let Some(ref msix) = self.msix else {
            return false;
        };
        if !msix.is_enabled() {
            return false;
        }
        if vector != bits::VIRTIO_MSI_NO_VECTOR {
            msix.fire(vector);
        }
        true
    }

    /// Deliver a config interrupt, unless the session that asked for
    /// it has since been reset away.
    ///
    /// The caller reports a change in the device status, and cannot
    /// hold the transport lock across the delivery: an MSI-X write
    /// under that lock stalls every other register access behind it.
    /// So a reset can start in the gap, and the reset both wipes the
    /// status this interrupt reports and lowers the line. Raising
    /// after that leaves an assertion standing in the next driver's
    /// session, for a change that session never saw. Admission and
    /// the session check are one step under a lock that holds no
    /// kernel call, and the reset waits out the deliveries it
    /// admitted, so this either goes out to the driver it was raised
    /// for or not at all.
    pub(super) fn raise_config_interrupt_in(&self, session: IntrSession) {
        #[cfg(test)]
        super::run_park(&self.parks.config_interrupt_pending);
        let Some(_admitted) = self.intr.begin(session) else {
            return;
        };
        self.raise_config_interrupt();
    }
}
