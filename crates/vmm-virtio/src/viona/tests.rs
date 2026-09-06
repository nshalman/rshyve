// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What these tests reach, and what they do not.
//!
//! The halt, the guards and the ring bookkeeping are the same code on
//! every platform, and they all reach the kernel through
//! [`LinkOps`](super::link::LinkOps). A recording link stands in for
//! the driver, so a test reads the command and the ring the device
//! asked for. What the illumos build then puts on the wire is pinned
//! one layer down, in `viona_api`.
//!
//! The calls a migration or a boot makes ([`super::migrate`],
//! [`super::setup`]) go straight to the kernel handle and have nothing
//! to stand in for them off illumos. No test here covers those. Do not
//! add one that calls their empty stub: it reads as coverage and pins
//! nothing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use super::halt::{halt_link, stop_poller, Poller};
use super::link::LinkOps;
use super::VirtioViona;

/// The device tests need a `VionaInner` with no kernel handle, and the
/// illumos build has no such shape. What they pin is the shared halt
/// and guard logic, which is the same code on both platforms.
#[cfg(not(target_os = "illumos"))]
mod device;

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
    SetFeatures(u64),
    Delete,
    /// The poll thread left.
    ///
    /// Only a device test starts a poller, and that needs the shape
    /// the illumos build does not have.
    #[cfg(not(target_os = "illumos"))]
    PollerStopped,
}

/// What one halt asked of the kernel, in order.
type Steps = Arc<Mutex<Vec<Step>>>;

/// A link that records the calls the device makes.
///
/// This is the whole in-kernel side of the shared logic, so the order
/// and the error handling are testable on every platform, illumos
/// included.
struct RecordingLink {
    steps: Steps,
    delete_result: fn() -> std::io::Result<()>,
    /// The device to check for a held lock, once it exists.
    device: OnceLock<Weak<VirtioViona>>,
    /// The calls that ran with the device lock held.
    locked: Mutex<Vec<Step>>,
}

impl RecordingLink {
    fn new(steps: &Steps) -> Self {
        Self {
            steps: Arc::clone(steps),
            delete_result: || Ok(()),
            device: OnceLock::new(),
            locked: Mutex::new(Vec::new()),
        }
    }

    fn failing(steps: &Steps) -> Self {
        Self {
            delete_result: || {
                Err(std::io::Error::from(std::io::ErrorKind::NotFound))
            },
            ..Self::new(steps)
        }
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
}

impl LinkOps for RecordingLink {
    fn ring_init(
        &self,
        ring: u16,
        size: u16,
        desc: u64,
        avail: u64,
        used: u64,
    ) -> std::io::Result<()> {
        self.push(Step::RingInit {
            ring,
            size,
            desc,
            avail,
            used,
        });
        Ok(())
    }

    fn ring_kick(&self, ring: u16) -> std::io::Result<()> {
        self.push(Step::RingKick(ring));
        Ok(())
    }

    fn ring_reset(&self, ring: u16) -> std::io::Result<()> {
        self.push(Step::RingReset(ring));
        Ok(())
    }

    fn set_features(&self, features: u64) -> std::io::Result<()> {
        self.push(Step::SetFeatures(features));
        Ok(())
    }

    fn delete(&self) -> std::io::Result<()> {
        self.push(Step::Delete);
        (self.delete_result)()
    }
}

fn steps_of(steps: &Steps) -> Vec<Step> {
    steps.lock().expect("steps lock").clone()
}

fn null_log() -> slog::Logger {
    slog::Logger::root(slog::Discard, slog::o!())
}

#[test]
fn a_halt_resets_every_ring_before_it_deletes_the_link() {
    // A ring that is reset already leaves the delete's own wait at
    // once, and that wait ignores signals.
    let steps = Steps::default();
    let link = RecordingLink::new(&steps);

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
    let link = RecordingLink::failing(&steps);

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
