// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! This link's part in the VM's ring gate.
//!
//! The gate stops every link's rings before any ioctl that takes the
//! VM write lock (see [`vmm_core::rings`]). Stopping here is
//! [`viona_api::quiesce`]: each worker exits and its state is kept.
//! Starting writes the state back, routes the interrupt again, and
//! kicks the ring.

use std::sync::{Mutex, Weak};

use viona_api::LinkOps;
use vmm_core::rings::KernelRings;

use super::{RingState, VionaInner, VirtioViona, NET_NUM_QUEUES};

/// How many times an interrupted pause or reset is tried again.
const ATTEMPTS: usize = 8;

/// The gate's handle on one device. Weak, so the gate does not keep a
/// removed NIC alive.
pub(super) struct GateMember(pub(super) Weak<Mutex<VionaInner>>);

impl KernelRings for GateMember {
    fn stop(&self) -> Result<(), String> {
        let Some(inner) = self.0.upgrade() else {
            return Ok(());
        };
        let mut inner = inner.lock().expect("viona lock");
        // A deleted link has no worker left. A halted one is stopped
        // still: the halt needs the proof to take its hook off.
        if inner.deleted || inner.stopped.is_some() {
            return Ok(());
        }
        match viona_api::quiesce(inner.link()) {
            Ok(rings) => {
                inner.stopped = Some(rings);
                Ok(())
            }
            Err(failed) => {
                let why = failed.to_string();
                VirtioViona::restart_rings(&mut inner, failed.resumed);
                Err(format!("viona {why}"))
            }
        }
    }

    fn start(&self) {
        let Some(inner) = self.0.upgrade() else {
            return;
        };
        let mut inner = inner.lock().expect("viona lock");
        let Some(rings) = inner.stopped.take() else {
            return;
        };
        if inner.halted {
            rings.discard();
            return;
        }
        let resumed = rings.resume();
        for &ring in &resumed.kick {
            VirtioViona::route_again(&mut inner, ring);
        }
        VirtioViona::restart_rings(&mut inner, resumed);
    }
}

impl VirtioViona {
    /// Give a ring the gate put back the MSI-X route a restore gave it.
    fn route_again(inner: &mut VionaInner, ring: u16) {
        let Some(&Some((addr, msg))) = inner.msi.get(usize::from(ring)) else {
            return;
        };
        if let Err(e) = inner.link_ref().ring_set_msi(ring, addr, msg) {
            // The poll thread delivers the interrupt instead.
            slog::warn!(inner.log(), "viona ring lost its MSI-X route";
                "ring" => ring, "error" => %e);
            inner.msi[usize::from(ring)] = None;
        }
    }

    /// Kick the rings a quiesce put back, and quarantine the ones it
    /// could not.
    pub(super) fn restart_rings(
        inner: &mut VionaInner,
        resumed: viona_api::Resumed,
    ) {
        for (ring, e) in resumed.lost {
            slog::warn!(inner.log(), "viona ring lost its state, quarantining";
                "ring" => ring, "error" => %e);
            if let Some(state) = inner.ring_state.get_mut(usize::from(ring)) {
                *state = RingState::Error;
            }
        }
        for ring in resumed.kick {
            let idx = usize::from(ring);
            if idx >= NET_NUM_QUEUES {
                continue;
            }
            Self::ring_kick(inner, ring);
            // A ring that will not start may never see another guest
            // kick, so the guest is told through the quarantine.
            if inner.kick_refused[idx] {
                inner.ring_state[idx] = RingState::Error;
            }
        }
    }
}

/// What stopping one ring left behind.
pub(super) enum StopOne {
    /// The worker is gone and the state is kept.
    Kept(viona_api::vioc_ring_state),
    /// The worker is gone, and the state with it.
    Lost(std::io::Error),
    /// The reset was refused, so the worker may still be alive.
    Escaped(std::io::Error),
}

/// Stop one ring the way a quiesce does, and read back what it holds.
///
/// For a restore under a hold: the kernel checks the ring as it is
/// programmed, and this takes the worker it started away again. The
/// reset is tried whatever the pause did, because only the reset ends
/// the worker.
pub(super) fn stop_one(link: &dyn LinkOps, ring: u16) -> StopOne {
    let read = retry(|| link.ring_pause(ring))
        .and_then(|()| link.ring_get_state(ring));
    if let Err(e) = retry(|| link.ring_reset(ring)) {
        return StopOne::Escaped(e);
    }
    match read {
        Ok(state) => StopOne::Kept(state),
        Err(e) => StopOne::Lost(e),
    }
}

fn retry(mut call: impl FnMut() -> std::io::Result<()>) -> std::io::Result<()> {
    for _ in 1..ATTEMPTS {
        match call() {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            rc => return rc,
        }
    }
    call()
}
