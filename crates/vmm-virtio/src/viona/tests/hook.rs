// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The notify hook: rings stopped before it changes, started after,
//! and a guest that moves its BARs in a loop held to a budget.

use std::sync::Arc;
use std::time::{Duration, Instant};

use viona_api::vioc_ring_state;

use super::device::{device_with, running_device};
use super::{steps_of, Op, Step, Steps, TestLink};
use crate::viona::hook::HookAddrs;
use crate::viona::{RingState, NET_QUEUE_SIZE};
use crate::VirtioDevice;

const PORT: u16 = 0x2010;
const WINDOW: u64 = 0xC000_2000;

fn saved(ring: u16) -> vioc_ring_state {
    vioc_ring_state {
        vrs_index: ring,
        vrs_avail_idx: 0x1111 + ring,
        vrs_used_idx: 0x2222 + ring,
        vrs_qsize: NET_QUEUE_SIZE,
        ..Default::default()
    }
}

fn quiesce_steps() -> [Step; 6] {
    [
        Step::RingPause(0),
        Step::RingGetState(0),
        Step::RingReset(0),
        Step::RingPause(1),
        Step::RingGetState(1),
        Step::RingReset(1),
    ]
}

fn hook_steps(steps: &Steps) -> Vec<Step> {
    steps_of(steps)
        .into_iter()
        .filter(|s| {
            matches!(s, Step::SetNotifyIop(_) | Step::SetNotifyMmio { .. })
        })
        .collect()
}

#[test]
fn a_hook_change_stops_every_ring_first_and_starts_them_after() {
    // The kernel takes the ring lock under the VM write lock, and a
    // hook change takes the write lock. A ring worker alive across it
    // can deadlock the host, so every worker is gone first.
    let steps = Steps::default();
    let link = TestLink::new(&steps).live_rings(NET_QUEUE_SIZE);
    let device = running_device(Arc::new(link));

    VirtioDevice::set_notify_addrs(&device, PORT, WINDOW);

    let mut want = quiesce_steps().to_vec();
    want.extend([
        Step::SetNotifyIop(PORT),
        Step::SetNotifyMmio {
            addr: WINDOW,
            size: 0x1000,
        },
        Step::RingSetState(saved(0)),
        Step::RingSetState(saved(1)),
        Step::RingKick(0),
        Step::RingKick(1),
    ]);
    assert_eq!(steps_of(&steps), want);
}

#[test]
fn a_ring_the_kernel_was_never_given_is_not_restarted() {
    // The guest moves its BARs at boot, before any ring exists.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));

    VirtioDevice::set_notify_addrs(&device, PORT, WINDOW);

    let mut want = quiesce_steps().to_vec();
    want.extend([
        Step::SetNotifyIop(PORT),
        Step::SetNotifyMmio {
            addr: WINDOW,
            size: 0x1000,
        },
    ]);
    assert_eq!(steps_of(&steps), want);
}

#[test]
fn a_ring_that_will_not_pause_keeps_the_old_hook() {
    // A broken guest network is survivable; a host deadlock is not.
    let steps = Steps::default();
    let link = TestLink::new(&steps)
        .live_rings(NET_QUEUE_SIZE)
        .failing(&[Op::RingPause]);
    let device = running_device(Arc::new(link));

    VirtioDevice::set_notify_addrs(&device, PORT, WINDOW);

    assert_eq!(hook_steps(&steps), []);
    // The ring may be paused already, so it is kicked back to work.
    assert_eq!(steps_of(&steps).last(), Some(&Step::RingKick(0)));
}

#[test]
fn a_ring_that_will_not_reset_keeps_the_old_hook_and_is_quarantined() {
    // Its worker may still be alive.
    let steps = Steps::default();
    let link = TestLink::new(&steps)
        .live_rings(NET_QUEUE_SIZE)
        .failing(&[Op::RingReset]);
    let device = running_device(Arc::new(link));

    VirtioDevice::set_notify_addrs(&device, PORT, WINDOW);

    assert_eq!(hook_steps(&steps), []);
    let inner = device.inner.lock().expect("viona lock");
    assert_eq!(inner.ring_state[0], RingState::Error);
}

#[test]
fn a_ring_that_cannot_take_its_state_back_is_quarantined() {
    let steps = Steps::default();
    let link = TestLink::new(&steps)
        .live_rings(NET_QUEUE_SIZE)
        .failing(&[Op::RingSetState]);
    let device = running_device(Arc::new(link));

    VirtioDevice::set_notify_addrs(&device, PORT, WINDOW);

    let inner = device.inner.lock().expect("viona lock");
    assert_eq!(inner.ring_state, [RingState::Error; 2]);
    drop(inner);
    assert!(!steps_of(&steps).contains(&Step::RingKick(0)));
}

#[test]
fn an_unchanged_hook_stops_no_ring() {
    let steps = Steps::default();
    let link = TestLink::new(&steps).live_rings(NET_QUEUE_SIZE);
    let device = running_device(Arc::new(link));
    VirtioDevice::set_notify_addrs(&device, PORT, WINDOW);
    steps.lock().expect("steps lock").clear();

    VirtioDevice::set_notify_addrs(&device, PORT, WINDOW);

    assert_eq!(steps_of(&steps), []);
}

#[test]
fn a_bar_no_longer_decoded_takes_the_hook_away() {
    // A hook left on the old address takes kicks meant for whatever
    // the guest puts there next.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));
    VirtioDevice::set_notify_addrs(&device, PORT, WINDOW);
    steps.lock().expect("steps lock").clear();

    VirtioDevice::set_notify_addrs(&device, 0, 0);

    assert_eq!(
        hook_steps(&steps),
        [
            Step::SetNotifyIop(0),
            Step::SetNotifyMmio {
                addr: 0,
                size: 0x1000
            }
        ],
    );
}

#[test]
fn a_refused_port_does_not_stop_the_window() {
    // A VirtIO 1.0 guest kicks through the window, so it still gets
    // the in-kernel kick.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::SetNotifyIop]);
    let device = device_with(Arc::new(link));

    VirtioDevice::set_notify_addrs(&device, PORT, WINDOW);

    assert_eq!(
        hook_steps(&steps),
        [
            Step::SetNotifyIop(PORT),
            Step::SetNotifyMmio {
                addr: WINDOW,
                size: 0x1000
            }
        ],
    );
}

#[test]
fn a_refused_port_is_sent_again_on_the_next_change() {
    // The kernel removes the old hook before it adds the new one, so
    // after a refusal nothing says which hook it holds.
    let steps = Steps::default();
    let link = TestLink::new(&steps).failing(&[Op::SetNotifyIop]);
    let device = device_with(Arc::new(link));
    VirtioDevice::set_notify_addrs(&device, PORT, WINDOW);
    steps.lock().expect("steps lock").clear();

    VirtioDevice::set_notify_addrs(&device, PORT, WINDOW);

    assert_eq!(hook_steps(&steps), [Step::SetNotifyIop(PORT)]);
}

#[test]
fn a_guest_moving_its_bars_in_a_loop_is_held_to_a_budget() {
    // Each change stops the rings and every vCPU of this VM. A guest
    // that loops gets a bounded number, then one clear, then nothing.
    let steps = Steps::default();
    let link = TestLink::new(&steps).live_rings(NET_QUEUE_SIZE);
    let device = running_device(Arc::new(link));
    let now = Instant::now();

    for i in 0..1000u16 {
        let port = if i % 2 == 0 { PORT } else { PORT + 0x100 };
        device.sync_hook(
            HookAddrs {
                pio: port,
                mmio: WINDOW,
            },
            now,
        );
    }

    let pauses = steps_of(&steps)
        .iter()
        .filter(|s| **s == Step::RingPause(0))
        .count();
    assert_eq!(pauses, 17, "16 changes and one clear");
    assert_eq!(
        hook_steps(&steps).last(),
        Some(&Step::SetNotifyMmio {
            addr: 0,
            size: 0x1000
        })
    );

    // The budget comes back with time, and the hook with it.
    steps.lock().expect("steps lock").clear();
    device.sync_hook(
        HookAddrs {
            pio: PORT,
            mmio: WINDOW,
        },
        now + Duration::from_secs(1),
    );
    assert_eq!(
        hook_steps(&steps),
        [
            Step::SetNotifyIop(PORT),
            Step::SetNotifyMmio {
                addr: WINDOW,
                size: 0x1000
            }
        ],
    );
}

#[test]
fn a_hook_change_waits_while_an_export_holds_the_rings() {
    // The export reads indices from the stopped rings. A restart here
    // would move them under it.
    let steps = Steps::default();
    let link = TestLink::new(&steps).live_rings(NET_QUEUE_SIZE);
    let device = running_device(Arc::new(link));
    VirtioDevice::pause_rings_for_export(&device).expect("the rings pause");
    steps.lock().expect("steps lock").clear();

    VirtioDevice::set_notify_addrs(&device, PORT, WINDOW);

    assert_eq!(steps_of(&steps), []);
}

#[test]
fn a_change_held_back_by_an_export_lands_when_the_export_lets_go() {
    // Otherwise the hook stays on an address the guest gave away.
    let steps = Steps::default();
    let link = TestLink::new(&steps).live_rings(NET_QUEUE_SIZE);
    let device = running_device(Arc::new(link));
    VirtioDevice::pause_rings_for_export(&device).expect("the rings pause");
    VirtioDevice::set_notify_addrs(&device, PORT, WINDOW);
    assert_eq!(hook_steps(&steps), []);

    VirtioDevice::resume_rings_after_migration(&device)
        .expect("the rings resume");

    assert_eq!(
        hook_steps(&steps),
        [
            Step::SetNotifyIop(PORT),
            Step::SetNotifyMmio {
                addr: WINDOW,
                size: 0x1000
            }
        ],
    );
}

#[test]
fn a_throttled_hook_comes_back_on_a_kick_once_the_budget_refills() {
    // A guest that stops moving its BARs does not write them again, so
    // the kicks that now exit to userspace are what bring it back.
    let steps = Steps::default();
    let link = TestLink::new(&steps).live_rings(NET_QUEUE_SIZE);
    let device = running_device(Arc::new(link));
    let past = Instant::now()
        .checked_sub(Duration::from_secs(5))
        .expect("the clock is past five seconds");
    for i in 0..40u16 {
        let port = if i % 2 == 0 { PORT } else { PORT + 0x100 };
        device.sync_hook(
            HookAddrs {
                pio: port,
                mmio: WINDOW,
            },
            past,
        );
    }
    steps.lock().expect("steps lock").clear();

    super::device::notify(&device, 1);

    assert_eq!(
        hook_steps(&steps),
        [
            Step::SetNotifyIop(PORT + 0x100),
            Step::SetNotifyMmio {
                addr: WINDOW,
                size: 0x1000
            }
        ],
    );
}

#[test]
fn a_restarted_ring_that_will_not_start_is_quarantined() {
    // An RX ring may never see another guest kick.
    let steps = Steps::default();
    let link = TestLink::new(&steps)
        .live_rings(NET_QUEUE_SIZE)
        .failing(&[Op::RingKick]);
    let device = running_device(Arc::new(link));

    VirtioDevice::set_notify_addrs(&device, PORT, WINDOW);

    let inner = device.inner.lock().expect("viona lock");
    assert_eq!(inner.ring_state, [RingState::Error; 2]);
}
