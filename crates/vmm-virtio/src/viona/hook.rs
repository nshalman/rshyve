// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Keeping the kernel notify hook on the addresses the guest decodes.
//!
//! The hook is what lets the kernel take a guest kick without a vCPU
//! exit. It has to follow the BARs: a hook left on an old address
//! takes kicks meant for whatever the guest puts there next.
//!
//! A hook change takes the VM write lock, so it runs inside the VM's
//! ring gate, with every ring of every link stopped. A guest can move
//! its BARs as often as it likes, so the changes are counted against a
//! budget. A guest that spends it loses the hook, and its kicks go
//! through userspace until the budget refills.

use std::time::{Duration, Instant};

use vmm_core::ratelimit::{Pace, Pacer};

use super::{VionaInner, VirtioViona};

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
    pub(super) pio: Option<u16>,
    pub(super) mmio: Option<u64>,
    pacer: Pacer,
}

impl Default for NotifyHook {
    fn default() -> Self {
        Self {
            want: HookAddrs::NONE,
            pio: Some(0),
            mmio: Some(0),
            pacer: Pacer::new(BURST, REFILL),
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

    /// Whether the kernel may hold a hook that nothing has removed.
    pub(super) fn maybe_installed(&self) -> bool {
        self.installed() != Some(HookAddrs::NONE)
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
        let cleared = self.installed() == Some(HookAddrs::NONE);
        match self.pacer.admit(now, cleared) {
            Pace::Change => Some(want),
            Pace::Clear => Some(HookAddrs::NONE),
            Pace::Wait => None,
        }
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
        let target = {
            let mut inner = self.inner.lock().expect("viona lock");
            inner.hook.want = want;
            Self::plan_hook(&mut inner, now)
        };
        if let Some(target) = target {
            self.change_hook(target);
        }
    }

    /// Try again for a hook that lags what the guest decodes.
    ///
    /// A change can be refused, held back by the gate, or spent from
    /// the budget, and the guest may not move its BARs again. Its kicks
    /// come through userspace while the hook lags, so each one is a
    /// chance to catch up.
    pub(super) fn retry_hook(&self) {
        let target = {
            let mut inner = self.inner.lock().expect("viona lock");
            if inner.hook.installed() == Some(inner.hook.want) {
                return;
            }
            Self::plan_hook(&mut inner, Instant::now())
        };
        if let Some(target) = target {
            self.change_hook(target);
        }
    }

    fn plan_hook(inner: &mut VionaInner, now: Instant) -> Option<HookAddrs> {
        // The halt destroys the link with this lock dropped, and the
        // delete removes the hook itself.
        if inner.halted {
            return None;
        }
        let want = inner.hook.want;
        let target = inner.hook.plan(want, now)?;
        if inner.hook.pacer.first_throttle() {
            slog::warn!(inner.log(),
                "viona notify hook moved too often, kicks go through \
                 userspace for now";
                "pio" => want.pio, "mmio" => format!("{:#x}", want.mmio));
        }
        Some(target)
    }

    /// Change the hook inside the ring gate.
    ///
    /// Called with no device lock held: the gate takes it to stop this
    /// link.
    fn change_hook(&self, target: HookAddrs) {
        let ran = self.rings.run(|_| {
            let mut inner = self.inner.lock().expect("viona lock");
            Self::write_hook(&mut inner, target);
        });
        // The old hook stays. A guest network that stops is
        // survivable; a host deadlock is not.
        if let Err(e) = ran {
            let inner = self.inner.lock().expect("viona lock");
            slog::warn!(inner.log(),
                "viona rings did not stop, notify hook unchanged";
                "error" => %e);
        }
    }

    /// Send the hook calls, with the proof the gate left this link.
    ///
    /// Only a gate run calls this.
    fn write_hook(inner: &mut VionaInner, target: HookAddrs) {
        if inner.halted {
            return;
        }
        // The guest moved its BARs again while the gate stopped the
        // rings. The planned address is stale, so the hook comes off
        // instead; a later sync puts it on what the guest decodes now.
        let target = if target != HookAddrs::NONE && target != inner.hook.want {
            HookAddrs::NONE
        } else {
            target
        };
        Self::send_hook(inner, target);
    }

    /// Take the hook off a link the halt is about to delete. Only a
    /// gate run calls this.
    pub(super) fn clear_hook(inner: &mut VionaInner) {
        Self::send_hook(inner, HookAddrs::NONE);
    }

    fn send_hook(inner: &mut VionaInner, target: HookAddrs) {
        let log = inner.log();
        let VionaInner {
            link,
            stopped,
            hook,
            ..
        } = inner;
        let Some(rings) = stopped.as_ref() else {
            // The gate stops every member before it runs anything, so
            // this device is not in the gate it was given.
            slog::error!(log, "viona rings not stopped, notify hook unchanged");
            return;
        };
        if hook.pio != Some(target.pio) {
            match link.set_notify_iop(rings, target.pio) {
                Ok(()) => hook.pio = Some(target.pio),
                Err(e) => {
                    hook.pio = None;
                    slog::warn!(log, "viona set notify port failed";
                        "port" => target.pio, "error" => %e);
                }
            }
        }
        if hook.mmio != Some(target.mmio) {
            let rc = link.set_notify_mmio(rings, target.mmio, NOTIFY_MMIO_SIZE);
            match rc {
                Ok(()) => hook.mmio = Some(target.mmio),
                Err(e) => {
                    hook.mmio = None;
                    slog::warn!(log, "viona set notify mmio failed";
                        "addr" => format!("{:#x}", target.mmio),
                        "error" => %e);
                }
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
