// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Giving the in-kernel link back before the VM is destroyed.

use std::io::PipeWriter;
use std::thread::JoinHandle;
use std::time::Duration;

use vmm_devices::Lifecycle;

use viona_api::LinkOps;
use vmm_core::rings::GateError;

use super::{RingState, VirtioViona, NET_NUM_QUEUES};

/// How long a halt waits for the gate to stop every ring before it
/// leaves a link that may hold a hook in the kernel.
///
/// The usual refusal is another link in the middle of its own delete:
/// its ring calls answer `ENXIO` until the delete ends.
const GATE_WAIT: Duration = Duration::from_secs(2);
const GATE_STEP: Duration = Duration::from_millis(20);

/// Links left in the kernel. Held for the life of the process: the
/// last close of the descriptor runs the same delete (`viona_main.c`,
/// `viona_close`), with no gate around it.
static LEAKED: std::sync::Mutex<Vec<std::sync::Arc<dyn LinkOps>>> =
    std::sync::Mutex::new(Vec::new());

/// The interrupt poll thread, while one is running.
pub(super) struct Poller {
    /// Write end of the pipe that stops the thread.
    ///
    /// The thread parks in `poll(2)` on the viona descriptor with no
    /// timeout, so only a second descriptor can wake it. Closing this
    /// end is what makes that descriptor ready.
    pub(super) wake: PipeWriter,
    pub(super) thread: JoinHandle<()>,
}

/// Wake a running poll thread and wait for it to leave.
///
/// Closing the write end is the wake: the thread parks in an untimed
/// `poll(2)`, so nothing else reaches it.
pub(super) fn stop_poller(poller: Option<Poller>, log: &slog::Logger) {
    let Some(poller) = poller else {
        return;
    };
    drop(poller.wake);
    if poller.thread.join().is_err() {
        slog::error!(log, "the viona interrupt poll thread panicked");
    }
}

/// Reset every ring, then destroy the link.
///
/// The delete resets the rings itself, in a wait that ignores signals
/// (`viona_main.c`, `viona_ioc_delete`). A ring that is in its reset
/// state already leaves that wait at once (`viona_ring.c`,
/// `viona_ring_reset`), so each reset that succeeds here is one wait
/// the delete does not make. Nothing in this process signals the
/// teardown thread, so the order does not make the halt interruptible.
///
/// A ring reset failure is reported and the sweep carries on. Teardown
/// must reach the destroy whatever the link does. The answer is the
/// delete's: it gives the `vmm_drv` hold back, and it is the one call
/// that proves every ring stopped.
pub(super) fn halt_link(
    link: &dyn LinkOps,
    log: &slog::Logger,
) -> std::io::Result<()> {
    for ring in 0..NET_NUM_QUEUES as u16 {
        if let Err(e) = link.ring_reset(ring) {
            slog::warn!(log, "viona ring reset failed during halt";
                "ring" => ring, "error" => %e);
        }
    }

    link.delete()
}

impl VirtioViona {
    /// Stop the poll thread and destroy the link, once.
    ///
    /// The answer is the delete's. `VNA_IOC_DELETE` resets every ring
    /// in a wait that ignores signals and cannot fail once it has
    /// started (`viona_main.c`, `viona_ioc_delete`), so an `Ok` here
    /// proves no kernel thread reads or writes the guest ring pages any
    /// more.
    ///
    /// The delete removes a notify hook it finds before it resets any
    /// ring, and that removal takes the VM write lock. So a link that
    /// may hold a hook is deleted inside the ring gate, after the hook
    /// is taken off with the proof the gate leaves: no worker of any
    /// link on the VM is alive across it. A link with no hook takes no
    /// write lock to delete, and once it is halted nothing can give it
    /// one.
    ///
    /// If the gate cannot stop the rings within [`GATE_WAIT`], or a
    /// worker escaped it, the link is left in the kernel. A delete then
    /// takes the write lock beside a live worker, which can hang the
    /// host until a reboot. The leak is counted in the gate, so the VM
    /// status shows it, and the link stays one the gate stops.
    pub(super) fn destroy_link(&self) -> std::io::Result<()> {
        let (link, log, poller, hooked) = {
            let mut inner = self.inner.lock().expect("viona lock poisoned");
            // A second destroy has no link left. viona would treat it
            // as a no-op, but a repeated delete would also repeat the
            // wait inside it.
            if inner.halted {
                return Ok(());
            }
            inner.halted = true;
            // A deferred start held past the destroy would revive a
            // dead link.
            inner.deferred_poll = None;
            // Nothing may kick the rings again: they are gone. This is
            // set before the lock is dropped, so a kick that races the
            // delete finds no ring to send.
            inner.ring_state = [RingState::Init; NET_NUM_QUEUES];
            inner.negotiated_features = 0;
            (
                inner.link(),
                inner.log(),
                inner.poller.take(),
                inner.hook.maybe_installed(),
            )
        };

        // The poll thread reads the viona descriptor and calls back
        // into the PCI transport, so it must be gone before the link
        // is destroyed. The join needs the device lock dropped as well:
        // the thread may be inside that callback now, and it takes
        // transport locks. It is joined outside the gate too: a vCPU
        // that holds a transport lock may be waiting for the gate.
        stop_poller(poller, &log);

        let rc = if hooked {
            let rc = self.delete_in_gate(link.as_ref(), &log);
            if rc
                .as_ref()
                .is_err_and(|e| e.kind() == std::io::ErrorKind::ResourceBusy)
            {
                LEAKED
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(std::sync::Arc::clone(&link));
            }
            rc
        } else {
            halt_link(link.as_ref(), &log)
        };
        // Only a delete that worked ended every worker. A link the
        // delete refused, or one left in the kernel, keeps its place in
        // the gate, which still has to stop it.
        if rc.is_ok() {
            self.inner.lock().expect("viona lock").deleted = true;
        }
        rc
    }

    /// Take the hook off and delete the link with every ring of the VM
    /// stopped.
    fn delete_in_gate(
        &self,
        link: &dyn LinkOps,
        log: &slog::Logger,
    ) -> std::io::Result<()> {
        let mut waited = Duration::ZERO;
        loop {
            let ran = self.rings.run(|_| {
                Self::clear_hook(&mut self.inner.lock().expect("viona lock"));
                halt_link(link, log)
            });
            match ran {
                Ok(rc) => return rc,
                Err(GateError::Busy(_)) if waited < GATE_WAIT => {
                    std::thread::sleep(GATE_STEP);
                    waited += GATE_STEP;
                }
                Err(e) => {
                    self.rings.note_leak();
                    slog::error!(log, "viona rings did not stop, so the \
                        link is LEFT IN THE KERNEL: a delete now could \
                        hang the host"; "error" => %e);
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::ResourceBusy,
                        format!("viona link left in the kernel: {e}"),
                    ));
                }
            }
        }
    }

    /// Stop the kernel the one way that cannot fail, after a ring
    /// refused to reset.
    ///
    /// `VNA_IOC_RING_RESET` answers only 0 or `EINTR` for a ring viona
    /// holds (`viona_ring.c`, `viona_ring_reset`), and `EINTR` is
    /// retried. Anything else says the link is on its way out: `ENXIO`
    /// for a destroyed link or a VM hold marked for release
    /// (`viona_main.c`, `viona_ioctl`). `VNA_IOC_DELETE` is exempt from
    /// that check and stops every ring regardless, so it is the only
    /// call left that proves the guest can have its ring pages back.
    ///
    /// The caller waits out that delete. That is the point: the guest
    /// must not be told the ring is free while the kernel still holds
    /// it. The NIC is dead afterwards, which the guest can see and
    /// survive.
    pub(super) fn destroy_stopped_ring(&self, ring: u16, err: &std::io::Error) {
        let log = self.inner.lock().expect("viona lock poisoned").log();
        slog::error!(log, "viona ring reset refused; destroying the link";
            "ring" => ring, "error" => %err);
        let Err(e) = self.destroy_link() else {
            return;
        };
        // Nothing is left that can stop the ring, and the guest
        // reclaims the pages as soon as this returns. Leaving the
        // process is what stops the kernel: the driver's close path
        // resets every ring in a wait that ignores signals
        // (`viona_main.c`, `viona_close`).
        slog::error!(log, "viona link delete refused; the kernel may still \
            reach the guest ring pages, so this process cannot continue";
            "error" => %e);
        std::process::abort();
    }
}

impl Lifecycle for VirtioViona {
    fn type_name(&self) -> &'static str {
        "virtio-viona"
    }

    /// Give the in-kernel link back before the VM is destroyed.
    ///
    /// WHY teardown cannot skip this: viona takes a `vmm_drv` hold for
    /// the life of its link. `VM_DESTROY_SELF` marks every hold for
    /// release and then waits for the leases to break in an untimed
    /// `cv_wait` (`vmm_sol_dev.c`, `vmm_drv_purge`). Releasing the last
    /// hold first clears `VMM_HELD`, and the purge skips that wait
    /// altogether. The order below is Propolis's: stop the poller, kill
    /// the rings, then delete the link.
    ///
    /// This call is not bounded. The delete waits for each ring worker
    /// to stop, and does not heed signals, so it must run on the
    /// teardown thread the shutdown watchdog keeps a clock on.
    ///
    /// A hot unplug halts too, with the guest running
    /// (`vmm_devices::lifecycle::prepare_unplug`). So the device lock is
    /// dropped before any of that: `cfg_read`, `notify_queue`,
    /// `queue_addr_set` and `reset` take the same lock, and a vCPU in
    /// one of them must not wait out an untimed kernel call.
    fn halt(&self) {
        let Err(e) = self.destroy_link() else {
            return;
        };
        // WHY this is an error and not a warning: the hold this call
        // gives back is what VM_DESTROY_SELF waits for. A delete that
        // failed predicts the destroy wedge. Teardown carries on
        // regardless, because it has to reach VM_DESTROY_SELF.
        let log = self.inner.lock().expect("viona lock poisoned").log();
        slog::error!(log, "viona link delete failed; the VM destroy may wait \
            for its hold";
            "error" => %e);
    }
}
