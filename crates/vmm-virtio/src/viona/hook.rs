// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Keeping the kernel notify hook on the addresses the guest decodes.
//!
//! The hook is what lets the kernel take a guest kick without a vCPU
//! exit. It has to follow the BARs: a hook left on an old address
//! takes kicks meant for whatever the guest puts there next.
//!
//! Every change goes through [`viona_api::quiesce`] first, because the
//! kernel takes the ring lock under the VM write lock and a live ring
//! worker can deadlock the host against a hook change. A guest can move
//! its BARs as often as it likes, so the changes are counted against a
//! budget. A guest that spends it loses the hook, and its kicks go
//! through userspace until the budget refills.

use std::time::{Duration, Instant};

use super::{PausedRing, RingState, VionaInner, VirtioViona, NET_NUM_QUEUES};

/// The window the modern notify capability points the guest at.
const NOTIFY_MMIO_SIZE: u32 = 0x1000;

/// Hook changes a guest gets at once. Firmware and an OS boot use a
/// handful.
const BURST: u32 = 16;

/// How often one more change is allowed once the burst is spent.
const REFILL: Duration = Duration::from_secs(1);

/// The addresses a hook sits on. 0 is none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct HookAddrs {
    pub(super) pio: u16,
    pub(super) mmio: u64,
}

impl HookAddrs {
    const NONE: Self = Self { pio: 0, mmio: 0 };
}

/// What the kernel hook is now, and what the guest may still change.
pub(super) struct NotifyHook {
    /// What the guest decodes now. The hook follows it when it can.
    want: HookAddrs,
    /// `None` is a hook the kernel may or may not hold, after a call
    /// it refused. The next sync sends it again.
    pio: Option<u16>,
    mmio: Option<u64>,
    tokens: u32,
    refilled: Option<Instant>,
    /// The one clear allowed past the budget has been used.
    clear_spent: bool,
    throttled: bool,
    throttle_reported: bool,
}

impl Default for NotifyHook {
    fn default() -> Self {
        Self {
            want: HookAddrs::NONE,
            pio: Some(0),
            mmio: Some(0),
            tokens: BURST,
            refilled: None,
            clear_spent: false,
            throttled: false,
            throttle_reported: false,
        }
    }
}

impl NotifyHook {
    fn installed(&self) -> Option<HookAddrs> {
        Some(HookAddrs {
            pio: self.pio?,
            mmio: self.mmio?,
        })
    }

    /// The hook to program for `want` at `now`, or `None` when nothing
    /// should change.
    ///
    /// A change to what is installed already costs nothing. Past the
    /// budget the hook is cleared once, so no kick lands on a stale
    /// address, and stays clear until a change is allowed again.
    pub(super) fn plan(
        &mut self,
        want: HookAddrs,
        now: Instant,
    ) -> Option<HookAddrs> {
        if self.installed() == Some(want) {
            return None;
        }
        self.refill(now);
        if self.tokens > 0 {
            self.tokens -= 1;
            self.clear_spent = false;
            return Some(want);
        }
        if self.installed() == Some(HookAddrs::NONE) || self.clear_spent {
            return None;
        }
        self.clear_spent = true;
        self.throttled = true;
        Some(HookAddrs::NONE)
    }

    fn refill(&mut self, now: Instant) {
        let Some(last) = self.refilled else {
            self.refilled = Some(now);
            return;
        };
        let elapsed = now.saturating_duration_since(last);
        let earned = elapsed.as_secs() / REFILL.as_secs();
        if earned == 0 {
            return;
        }
        let earned = u32::try_from(earned).unwrap_or(u32::MAX);
        self.tokens = self.tokens.saturating_add(earned).min(BURST);
        self.refilled = Some(now);
    }

    /// Whether the last plan was the throttle, the first time only.
    fn first_throttle(&mut self) -> bool {
        std::mem::take(&mut self.throttled)
            && !std::mem::replace(&mut self.throttle_reported, true)
    }
}

impl VirtioViona {
    /// Point the kernel notify hook at the addresses the guest decodes
    /// now. 0 is an address it does not decode.
    pub fn set_notify_addrs(&self, pio_port: u16, mmio_addr: u64) {
        let want = HookAddrs {
            pio: pio_port,
            mmio: mmio_addr,
        };
        self.sync_hook(want, Instant::now());
    }

    pub(super) fn sync_hook(&self, want: HookAddrs, now: Instant) {
        let mut inner = self.inner.lock().expect("viona lock");
        inner.hook.want = want;
        Self::follow_hook(&mut inner, now);
    }

    /// Try again for a hook that lags what the guest decodes.
    ///
    /// A change can be refused, held back by an export, or spent from
    /// the budget, and the guest may not move its BARs again. Its kicks
    /// come through userspace while the hook lags, so each one is a
    /// chance to catch up.
    pub(super) fn retry_hook(inner: &mut VionaInner) {
        if inner.hook.installed() != Some(inner.hook.want) {
            Self::follow_hook(inner, Instant::now());
        }
    }

    fn follow_hook(inner: &mut VionaInner, now: Instant) {
        // The halt destroys the link with this lock dropped, and the
        // delete removes the hook itself.
        if inner.halted {
            return;
        }
        let want = inner.hook.want;
        let Some(target) = inner.hook.plan(want, now) else {
            return;
        };
        if inner.hook.first_throttle() {
            slog::warn!(inner.log(),
                "viona notify hook moved too often, kicks go through \
                 userspace for now";
                "pio" => want.pio, "mmio" => format!("{:#x}", want.mmio));
        }
        Self::change_hook(inner, target);
    }

    /// Stop the rings, change the hook, and start them again.
    ///
    /// The lock is held throughout: a kick or a ring init in between
    /// would start a worker again.
    fn change_hook(inner: &mut VionaInner, target: HookAddrs) {
        // A migration export holds these stopped and owns their state.
        // A restart here would move the indices it is reading.
        if inner.paused_rings.iter().any(|r| *r != PausedRing::Running) {
            slog::warn!(
                inner.log(),
                "viona notify hook not changed while an export holds the rings"
            );
            return;
        }
        let link = inner.link();
        let rings = match viona_api::quiesce(link.as_ref()) {
            Ok(rings) => rings,
            // The old hook stays. A guest network that stops is
            // survivable; a host deadlock is not.
            Err(failed) => {
                slog::warn!(inner.log(),
                    "viona rings did not stop, notify hook unchanged";
                    "error" => %failed);
                Self::restart_rings(inner, failed.resumed);
                return;
            }
        };

        if inner.hook.pio != Some(target.pio) {
            match link.set_notify_iop(&rings, target.pio) {
                Ok(()) => inner.hook.pio = Some(target.pio),
                Err(e) => {
                    inner.hook.pio = None;
                    slog::warn!(inner.log(), "viona set notify port failed";
                        "port" => target.pio, "error" => %e);
                }
            }
        }
        if inner.hook.mmio != Some(target.mmio) {
            let rc =
                link.set_notify_mmio(&rings, target.mmio, NOTIFY_MMIO_SIZE);
            match rc {
                Ok(()) => inner.hook.mmio = Some(target.mmio),
                Err(e) => {
                    inner.hook.mmio = None;
                    slog::warn!(inner.log(), "viona set notify mmio failed";
                        "addr" => format!("{:#x}", target.mmio),
                        "error" => %e);
                }
            }
        }

        let resumed = rings.resume();
        Self::restart_rings(inner, resumed);
    }

    /// Kick the rings a quiesce put back, and quarantine the ones it
    /// could not.
    fn restart_rings(inner: &mut VionaInner, resumed: viona_api::Resumed) {
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

#[cfg(test)]
mod tests {
    use super::*;

    const A: HookAddrs = HookAddrs {
        pio: 0x2010,
        mmio: 0xC000_2000,
    };
    const B: HookAddrs = HookAddrs {
        pio: 0x3010,
        mmio: 0xD000_2000,
    };

    #[test]
    fn a_change_to_the_installed_hook_is_dropped() {
        let mut hook = NotifyHook::default();
        let now = Instant::now();

        assert_eq!(hook.plan(HookAddrs::NONE, now), None);
        assert_eq!(hook.plan(A, now), Some(A));
        hook.pio = Some(A.pio);
        hook.mmio = Some(A.mmio);
        assert_eq!(hook.plan(A, now), None);
    }

    #[test]
    fn a_hook_the_kernel_refused_is_sent_again() {
        let mut hook = NotifyHook {
            pio: None,
            ..Default::default()
        };

        assert_eq!(
            hook.plan(HookAddrs::NONE, Instant::now()),
            Some(HookAddrs::NONE)
        );
    }

    #[test]
    fn a_guest_past_its_budget_gets_one_clear_and_then_nothing() {
        // Each change stops this VM's rings and its vCPUs. Past the
        // budget the cost has to be zero.
        let mut hook = NotifyHook::default();
        let now = Instant::now();
        let mut moves = 0;
        for i in 0..BURST * 4 {
            let want = if i % 2 == 0 { A } else { B };
            if let Some(target) = hook.plan(want, now) {
                hook.pio = Some(target.pio);
                hook.mmio = Some(target.mmio);
                moves += 1;
            }
        }

        assert_eq!(moves, BURST + 1);
        assert_eq!(hook.installed(), Some(HookAddrs::NONE));
    }

    #[test]
    fn the_budget_refills_with_time() {
        let mut hook = NotifyHook::default();
        let start = Instant::now();
        for i in 0..=BURST {
            let want = if i % 2 == 0 { A } else { B };
            if let Some(t) = hook.plan(want, start) {
                hook.pio = Some(t.pio);
                hook.mmio = Some(t.mmio);
            }
        }
        assert_eq!(hook.installed(), Some(HookAddrs::NONE));

        let later = start + REFILL;
        assert_eq!(hook.plan(A, later), Some(A));
        hook.pio = Some(A.pio);
        hook.mmio = Some(A.mmio);
        assert_eq!(hook.plan(B, later), Some(HookAddrs::NONE));
    }
}
