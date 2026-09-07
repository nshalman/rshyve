// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What these tests reach, and what they do not.
//!
//! Every call the device makes into the kernel goes through
//! [`LinkOps`](viona_api::LinkOps), on every platform. A recording
//! link stands in for the driver, so a test reads the command and the
//! ring the device asked for, and the same test runs on illumos, where
//! the product runs. What the illumos build then puts on the wire is
//! pinned one layer down, in `viona_api`.
//!
//! Nothing here reaches the ioctl itself. That needs an open
//! `/dev/viona`, which is a lab test.

use std::io::{PipeReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use viona_api::{vioc_ring_state, IntrStatus, IntrWait, LinkOps, PromiscMode};

use super::halt::{halt_link, stop_poller, Poller};
use super::VirtioViona;

mod device;
mod errors;
mod intr;

/// One kind of call the device makes into the kernel.
///
/// A test names the calls whose kernel refuses them. Per call, because
/// one refused call says nothing about the branch that follows
/// another: of the thirteen error branches in this module, only the
/// delete had a test before this existed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    RingInit,
    RingKick,
    RingReset,
    RingPause,
    RingSetState,
    RingGetState,
    RingSetMsi,
    SetFeatures,
    IntrStatus,
    RingIntrClr,
    SetNotifyIop,
    SetNotifyMmio,
    SetPromisc,
    WaitIntr,
    Delete,
}

/// One call the device made into the kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    RingInit {
        ring: u16,
        size: u16,
        desc: u64,
        avail: u64,
        used: u64,
    },
    RingKick(u16),
    RingReset(u16),
    RingPause(u16),
    RingSetState(vioc_ring_state),
    RingGetState(u16),
    RingSetMsi {
        ring: u16,
        addr: u64,
        msg: u64,
    },
    RingIntrClr(u16),
    SetFeatures(u64),
    SetNotifyIop(u16),
    SetNotifyMmio {
        addr: u64,
        size: u32,
    },
    SetPromisc(PromiscMode),
    Delete,
    /// The poll thread left.
    PollerStopped,
}

/// What the device asked of the kernel, in order.
type Steps = Arc<Mutex<Vec<Step>>>;

/// Where the poll thread is held while a test acts.
///
/// Two points, because they pin different mistakes. `Poll` catches a
/// session read after the kernel is asked what is pending; `Clear`
/// catches a session read at the raise. Only a sample taken before
/// both survives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ParkAt {
    /// Inside `VNA_IOC_INTR_POLL`, before the pending rings are known.
    Poll,
    /// After the ring's pending interrupt is cleared, which is the
    /// step that makes the notification this thread's to deliver.
    Clear,
}

/// A one-shot park the test drives from the other side.
struct Park {
    at: ParkAt,
    arrived: mpsc::Sender<()>,
    go: mpsc::Receiver<()>,
}

/// A delete that waits until the test lets it go.
///
/// On illumos the delete is an untimed kernel wait. This stands in for
/// it, so a test can act while the halt is inside one.
struct Hold {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}

/// A wait this long means a thread is wedged, not slow.
const WEDGED: Duration = Duration::from_secs(15);

/// A link that records the calls the device makes.
///
/// This is the whole in-kernel side of the device, so the order and
/// the error handling are testable on every platform, illumos
/// included. Each test switches on only what it reads.
struct TestLink {
    steps: Steps,
    /// The calls whose kernel refuses them.
    failing: Vec<Op>,
    /// Holds `delete` until the test releases it.
    delete_hold: Mutex<Option<Hold>>,
    /// The device to check for a held lock, once it exists.
    device: OnceLock<Weak<VirtioViona>>,
    /// The calls that ran with the device lock held.
    locked: Mutex<Vec<Step>>,
    /// What `intr_status` reports pending, one entry per ring.
    status: IntrStatus,
    /// Where the poll thread is held, once.
    park: Mutex<Option<Park>>,
}

impl TestLink {
    fn new(steps: &Steps) -> Self {
        Self {
            steps: Arc::clone(steps),
            failing: Vec::new(),
            delete_hold: Mutex::new(None),
            device: OnceLock::new(),
            locked: Mutex::new(Vec::new()),
            status: IntrStatus::default(),
            park: Mutex::new(None),
        }
    }

    /// A link whose kernel refuses `ops` and answers the rest.
    fn failing(mut self, ops: &[Op]) -> Self {
        self.failing = ops.to_vec();
        self
    }

    /// Record one call, then answer it the way the test asked.
    ///
    /// The step is recorded either way: a refused call is one the
    /// device still made, and the test reads both.
    fn answer(&self, op: Op, step: Step) -> std::io::Result<()> {
        self.push(step);
        self.result(op)
    }

    /// What one call answers.
    fn result(&self, op: Op) -> std::io::Result<()> {
        if self.failing.contains(&op) {
            return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        }
        Ok(())
    }

    /// A link whose delete waits for `hold`.
    fn holding(self, hold: Hold) -> Self {
        *self.delete_hold.lock().expect("hold lock") = Some(hold);
        self
    }

    /// A link that reports `ring` pending on every poll.
    fn pending(mut self, ring: usize) -> Self {
        self.status[ring] = 1;
        self
    }

    /// A link that holds the poll thread once, at `park.at`.
    fn parking(self, park: Park) -> Self {
        *self.park.lock().expect("park lock") = Some(park);
        self
    }

    fn push(&self, step: Step) {
        if let Some(device) = self.device.get().and_then(Weak::upgrade) {
            // A guard the halt still holds refuses this, which is
            // the regression the watch exists to catch.
            if device.inner.try_lock().is_err() {
                self.locked.lock().expect("locked lock").push(step);
            }
        }
        self.steps.lock().expect("steps lock").push(step);
    }

    /// Hold the caller here, if this is the step the test named.
    ///
    /// One shot: the park is taken, so the same link delivers
    /// normally afterwards.
    fn maybe_park(&self, at: ParkAt) {
        let park = {
            let mut slot = self.park.lock().expect("park lock");
            match slot.as_ref() {
                Some(park) if park.at == at => slot.take(),
                _ => None,
            }
        };
        let Some(park) = park else {
            return;
        };
        park.arrived.send(()).expect("the test watches this park");
        park.go
            .recv_timeout(WEDGED)
            .expect("the test never released this park");
    }
}

impl LinkOps for TestLink {
    fn ring_init(
        &self,
        ring: u16,
        size: u16,
        desc: u64,
        avail: u64,
        used: u64,
    ) -> std::io::Result<()> {
        self.answer(
            Op::RingInit,
            Step::RingInit {
                ring,
                size,
                desc,
                avail,
                used,
            },
        )
    }

    fn ring_kick(&self, ring: u16) -> std::io::Result<()> {
        self.answer(Op::RingKick, Step::RingKick(ring))
    }

    fn ring_reset(&self, ring: u16) -> std::io::Result<()> {
        self.answer(Op::RingReset, Step::RingReset(ring))
    }

    fn ring_pause(&self, ring: u16) -> std::io::Result<()> {
        self.answer(Op::RingPause, Step::RingPause(ring))
    }

    fn ring_set_state(&self, state: &vioc_ring_state) -> std::io::Result<()> {
        self.answer(Op::RingSetState, Step::RingSetState(*state))
    }

    fn ring_get_state(&self, ring: u16) -> std::io::Result<vioc_ring_state> {
        self.push(Step::RingGetState(ring));
        self.result(Op::RingGetState)?;
        // The indices a migration source reads out. Distinct, and
        // neither is the ring, so a caller that returns the wrong
        // field or swaps the pair is visible.
        Ok(vioc_ring_state {
            vrs_index: ring,
            vrs_avail_idx: 0x1111 + ring,
            vrs_used_idx: 0x2222 + ring,
            ..Default::default()
        })
    }

    fn ring_set_msi(
        &self,
        ring: u16,
        addr: u64,
        msg: u64,
    ) -> std::io::Result<()> {
        self.answer(Op::RingSetMsi, Step::RingSetMsi { ring, addr, msg })
    }

    fn set_features(&self, features: u64) -> std::io::Result<()> {
        self.answer(Op::SetFeatures, Step::SetFeatures(features))
    }

    fn intr_status(&self) -> std::io::Result<IntrStatus> {
        self.maybe_park(ParkAt::Poll);
        self.result(Op::IntrStatus)?;
        Ok(self.status)
    }

    fn ring_intr_clr(&self, ring: u16) -> std::io::Result<()> {
        let refused = self.answer(Op::RingIntrClr, Step::RingIntrClr(ring));
        self.maybe_park(ParkAt::Clear);
        refused
    }

    fn wait_intr(&self, stop: &PipeReader) -> std::io::Result<IntrWait> {
        self.result(Op::WaitIntr)?;
        // The halt closes the write end, so this read ends with 0
        // bytes. No test link signals its own work.
        let mut byte = [0u8; 1];
        let mut stop = stop;
        match stop.read(&mut byte) {
            Ok(0) => Ok(IntrWait::Stopped),
            other => panic!("the stop pipe answered {other:?}"),
        }
    }

    fn set_notify_iop(&self, port: u16) -> std::io::Result<()> {
        self.answer(Op::SetNotifyIop, Step::SetNotifyIop(port))
    }

    fn set_notify_mmio(&self, addr: u64, size: u32) -> std::io::Result<()> {
        self.answer(Op::SetNotifyMmio, Step::SetNotifyMmio { addr, size })
    }

    fn set_promisc(&self, mode: PromiscMode) -> std::io::Result<()> {
        self.answer(Op::SetPromisc, Step::SetPromisc(mode))
    }

    fn delete(&self) -> std::io::Result<()> {
        self.push(Step::Delete);
        if let Some(hold) = self.delete_hold.lock().expect("hold lock").take() {
            hold.entered
                .send(())
                .expect("the test waits for the delete");
            hold.release
                .recv_timeout(WEDGED)
                .expect("the test released the delete");
        }
        self.result(Op::Delete)
    }
}

fn steps_of(steps: &Steps) -> Vec<Step> {
    steps.lock().expect("steps lock").clone()
}

/// The rings whose pending interrupt the device cleared, in order.
fn cleared(steps: &Steps) -> Vec<u16> {
    steps_of(steps)
        .iter()
        .filter_map(|step| match step {
            Step::RingIntrClr(ring) => Some(*ring),
            _ => None,
        })
        .collect()
}

fn null_log() -> slog::Logger {
    slog::Logger::root(slog::Discard, slog::o!())
}

#[test]
fn a_halt_resets_every_ring_before_it_deletes_the_link() {
    // A ring that is reset already leaves the delete's own wait at
    // once, and that wait ignores signals.
    let steps = Steps::default();
    let link = TestLink::new(&steps);

    halt_link(&link, &null_log());

    assert_eq!(
        steps_of(&steps),
        [Step::RingReset(0), Step::RingReset(1), Step::Delete],
    );
}

#[test]
fn a_failed_delete_does_not_stop_the_halt() {
    // Teardown has to reach VM_DESTROY_SELF whatever the link
    // does, so the failure is reported and nothing is unwound.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::Delete]);

    halt_link(&link, &null_log());

    assert_eq!(steps_of(&steps).last(), Some(&Step::Delete));
}

#[test]
fn stopping_a_poller_wakes_the_thread_and_waits_for_it() {
    // Two properties the halt needs. The poll thread parks with no
    // timeout, so closing the write end is the only thing that can
    // end it. And the stop must not return before the thread is
    // gone: the link is deleted next, and the thread reads it.
    use std::os::fd::AsRawFd;

    let (stop, wake) = std::io::pipe().expect("the test can open a pipe");
    let woken = Arc::new(AtomicBool::new(false));
    let gone = Arc::new(AtomicBool::new(false));
    let thread = {
        let woken = Arc::clone(&woken);
        let gone = Arc::clone(&gone);
        std::thread::Builder::new()
            .name("poller-stub".into())
            .spawn(move || {
                let mut pfd = libc::pollfd {
                    fd: stop.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // A closed write end leaves the descriptor ready,
                // so this returns once and does not spin. The
                // timeout only stops a broken wake from hanging.
                while unsafe { libc::poll(&mut pfd, 1, 10_000) } < 0 {
                    let err = std::io::Error::last_os_error();
                    assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
                }
                woken.store(pfd.revents != 0, Ordering::SeqCst);
                // A stop that drops the handle instead of joining
                // it returns inside this window.
                std::thread::sleep(Duration::from_millis(200));
                gone.store(true, Ordering::SeqCst);
            })
            .expect("the test can spawn a thread")
    };

    stop_poller(Some(Poller { wake, thread }), &null_log());

    assert!(
        woken.load(Ordering::SeqCst),
        "the stop pipe never became ready",
    );
    assert!(
        gone.load(Ordering::SeqCst),
        "the stop returned before the thread left",
    );
}
