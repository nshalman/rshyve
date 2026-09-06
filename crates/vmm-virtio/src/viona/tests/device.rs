// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The device around the link: the halt, and the guards that keep a
//! destroyed link out of the kernel.
//!
//! A device here holds a recording link in place of the kernel handle,
//! which the illumos build does not have room for. Every guard below
//! is the same code on both platforms.

use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use vmm_core::mem::PhysMap;
use vmm_devices::Lifecycle;

use super::{null_log, steps_of, RecordingLink, Step, Steps};
use crate::queue::VirtQueue;
use crate::viona::halt::Poller;
use crate::viona::link::LinkOps;
use crate::viona::{
    QueueInterrupt, RingState, VionaInner, VirtioViona, NET_NUM_QUEUES,
    NET_QUEUE_SIZE, VIRTIO_NET_F_MAC,
};
use crate::VirtioDevice;

/// How long a test waits before it calls a thread stuck.
///
/// A wait that runs out is a failure, never a pass. It is long
/// enough that a loaded machine does not reach it.
const STUCK: Duration = Duration::from_secs(15);

/// How long a blocked delete waits to be released.
///
/// Longer than [`STUCK`], so a test that fails reports the thread
/// that is really stuck.
const DELETE_STUCK: Duration = Duration::from_secs(60);

impl RecordingLink {
    /// Watch `device` for a lock held across the calls it makes.
    fn watch(&self, device: &Arc<VirtioViona>) {
        self.device
            .set(Arc::downgrade(device))
            .expect("the link watches one device");
    }

    fn locked_calls(&self) -> Vec<Step> {
        self.locked.lock().expect("locked lock").clone()
    }
}

/// A link whose delete waits until the test releases it.
///
/// On illumos the delete is an untimed kernel wait. This stands in
/// for it, so a test can act while the halt is inside one.
struct BlockingLink {
    entered: Mutex<mpsc::Sender<()>>,
    release: Mutex<mpsc::Receiver<()>>,
}

impl LinkOps for BlockingLink {
    fn ring_init(
        &self,
        _ring: u16,
        _size: u16,
        _desc: u64,
        _avail: u64,
        _used: u64,
    ) -> std::io::Result<()> {
        Ok(())
    }

    fn ring_kick(&self, _ring: u16) -> std::io::Result<()> {
        Ok(())
    }

    fn ring_reset(&self, _ring: u16) -> std::io::Result<()> {
        Ok(())
    }

    fn set_features(&self, _features: u64) -> std::io::Result<()> {
        Ok(())
    }

    fn delete(&self) -> std::io::Result<()> {
        self.entered
            .lock()
            .expect("entered lock")
            .send(())
            .expect("the test waits for the delete");
        self.release
            .lock()
            .expect("release lock")
            .recv_timeout(DELETE_STUCK)
            .expect("the test released the delete");
        Ok(())
    }
}

/// Build a `VionaInner` with a test link in place of the kernel
/// handle.
fn test_inner_with(link: Arc<dyn LinkOps>) -> VionaInner {
    VionaInner {
        link,
        mac_addr: [0x02, 0x08, 0x20, 0xaa, 0xbb, 0xcc],
        dev_features: VIRTIO_NET_F_MAC,
        negotiated_features: 0,
        ring_state: [RingState::Init; NET_NUM_QUEUES],
        deferred_poll: None,
        poller: None,
        log: None,
        halted: false,
    }
}

/// A device whose calls into the kernel are watched through its link.
fn device_with(link: Arc<dyn LinkOps>) -> VirtioViona {
    VirtioViona {
        inner: Mutex::new(test_inner_with(link)),
    }
}

/// A device the guest has finished programming.
fn running_device(link: Arc<dyn LinkOps>) -> VirtioViona {
    let device = device_with(link);
    {
        let mut inner = device.inner.lock().expect("viona lock");
        inner.negotiated_features = u64::from(VIRTIO_NET_F_MAC);
        inner.ring_state = [RingState::Ready; NET_NUM_QUEUES];
    }
    device
}

/// A queue the guest has given addresses to.
fn addressed_queue() -> VirtQueue {
    let mut queue = VirtQueue::new(NET_QUEUE_SIZE);
    queue.set_addr_modern(0x1_0000, 0x2_0000, 0x3_0000);
    queue
}

fn notify(device: &VirtioViona, queue_idx: u16) {
    let mut queues = [
        VirtQueue::new(NET_QUEUE_SIZE),
        VirtQueue::new(NET_QUEUE_SIZE),
    ];
    let physmap = PhysMap::new();
    VirtioDevice::notify_queue(device, queue_idx, &mut queues, &physmap);
}

/// A poll thread that records the moment it was stopped.
///
/// The wake is the pipe closing, the same as the real one. The
/// pause before the step is written is what makes the order
/// readable: a halt that drops the poller instead of joining it
/// reaches the assertion before this line runs.
fn recording_poller(steps: &Steps) -> Poller {
    use std::os::fd::AsRawFd;

    let (stop, wake) = std::io::pipe().expect("the test can open a pipe");
    let steps = Arc::clone(steps);
    let thread = std::thread::Builder::new()
        .name("poller-stub".into())
        .spawn(move || {
            let mut pfd = libc::pollfd {
                fd: stop.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            while unsafe { libc::poll(&mut pfd, 1, 10_000) } < 0 {
                let err = std::io::Error::last_os_error();
                assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
            }
            std::thread::sleep(Duration::from_millis(200));
            steps.lock().expect("steps lock").push(Step::PollerStopped);
        })
        .expect("the test can spawn a thread");
    Poller { wake, thread }
}

#[test]
fn a_halt_issues_the_delete_that_frees_the_vmm_hold() {
    // VM_DESTROY_SELF waits for every vmm_drv hold in an untimed
    // cv_wait. VNA_IOC_DELETE is what gives viona's hold back.
    let steps = Steps::default();
    let device = device_with(Arc::new(RecordingLink::new(&steps)));

    device.halt();

    assert_eq!(
        steps_of(&steps),
        [Step::RingReset(0), Step::RingReset(1), Step::Delete],
    );
}

#[test]
fn a_halt_stops_the_poll_thread_before_it_deletes_the_link() {
    // The poll thread reads the viona descriptor and calls back
    // into the PCI transport, so a delete that runs beside it hands
    // it a link that is gone. This is Propolis's order, and it
    // needs the whole halt to hold it, not `stop_poller` alone.
    let steps = Steps::default();
    let device = device_with(Arc::new(RecordingLink::new(&steps)));
    device.inner.lock().expect("viona lock").poller =
        Some(recording_poller(&steps));

    device.halt();

    assert_eq!(
        steps_of(&steps),
        [
            Step::PollerStopped,
            Step::RingReset(0),
            Step::RingReset(1),
            Step::Delete,
        ],
    );
}

#[test]
fn a_halt_holds_no_device_lock_while_it_stops_the_link() {
    // A hot unplug halts a device the guest is still using
    // (`vmm_devices::lifecycle::prepare_unplug`). Every call the
    // halt makes is an untimed kernel wait, and cfg_read,
    // notify_queue, queue_addr_set and reset all take this lock. A
    // vCPU must not wait behind the teardown. This fails the moment
    // somebody holds the guard across halt_link again.
    let steps = Steps::default();
    let link = Arc::new(RecordingLink::new(&steps));
    let device = Arc::new(device_with(link.clone()));
    link.watch(&device);

    device.halt();

    assert_eq!(
        steps_of(&steps),
        [Step::RingReset(0), Step::RingReset(1), Step::Delete],
    );
    assert!(
        link.locked_calls().is_empty(),
        "the halt held the device lock through {:?}",
        link.locked_calls(),
    );
}

#[test]
fn a_config_read_does_not_wait_behind_the_link_delete() {
    // The same property from the guest's side: a vCPU reads the
    // device config while the halt sits in the delete.
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let device = Arc::new(device_with(Arc::new(BlockingLink {
        entered: Mutex::new(entered_tx),
        release: Mutex::new(release_rx),
    })));

    let halting = {
        let device = Arc::clone(&device);
        std::thread::Builder::new()
            .name("halt".into())
            .spawn(move || device.halt())
            .expect("the test can spawn a thread")
    };
    entered_rx
        .recv_timeout(STUCK)
        .expect("the halt reached the delete");

    // The read runs on its own thread. A lock the halt held would
    // park it for as long as the delete runs, and this test has to
    // report that rather than hang with it.
    let (read_tx, read_rx) = mpsc::channel();
    let reader = {
        let device = Arc::clone(&device);
        std::thread::Builder::new()
            .name("cfg-read".into())
            .spawn(move || {
                let mac = device.cfg_read(0, 4);
                read_tx.send(mac).expect("the test waits for the read");
            })
            .expect("the test can spawn a thread")
    };
    let observed = read_rx.recv_timeout(STUCK);

    release_tx.send(()).expect("the delete waits for this");
    halting.join().expect("the halting thread left");
    reader.join().expect("the reading thread left");

    assert_eq!(
        observed.expect("a config read waited out the link delete"),
        u32::from_le_bytes([0x02, 0x08, 0x20, 0xaa]),
    );
}

#[test]
fn a_running_device_kicks_the_ring_the_guest_named() {
    // The kick carries the ring. One that always sends ring 0 leaves
    // the guest's transmits in the ring and stalls the link.
    let steps = Steps::default();
    let device = running_device(Arc::new(RecordingLink::new(&steps)));

    notify(&device, 1);

    assert_eq!(steps_of(&steps), [Step::RingKick(1)]);
}

#[test]
fn a_halt_leaves_no_ring_a_kick_can_reach() {
    // The halt drops the device lock before it destroys the link,
    // so a vCPU can reach notify_queue while the delete runs. It
    // must find no ring ready to kick.
    let steps = Steps::default();
    let device = running_device(Arc::new(RecordingLink::new(&steps)));

    device.halt();
    steps.lock().expect("steps lock").clear();
    notify(&device, 1);

    let inner = device.inner.lock().expect("viona lock");
    assert_eq!(inner.ring_state, [RingState::Init; NET_NUM_QUEUES]);
    assert_eq!(inner.negotiated_features, 0);
    drop(inner);
    assert_eq!(
        steps_of(&steps),
        [],
        "a kick reached the link after the delete",
    );
}

#[test]
fn a_queue_address_write_programs_that_ring() {
    // The addresses the guest wrote are what the kernel worker
    // reads. A ring or an address that drifts here points the
    // in-kernel worker at guest memory the driver never offered.
    let steps = Steps::default();
    let device = device_with(Arc::new(RecordingLink::new(&steps)));

    VirtioDevice::queue_addr_set(&device, 1, &addressed_queue());

    assert_eq!(
        steps_of(&steps),
        [Step::RingInit {
            ring: 1,
            size: NET_QUEUE_SIZE,
            desc: 0x1_0000,
            avail: 0x2_0000,
            used: 0x3_0000,
        }],
    );
    assert_eq!(
        device.inner.lock().expect("viona lock").ring_state[1],
        RingState::Ready,
    );
}

#[test]
fn a_halted_device_programs_no_ring() {
    // The halt destroys the link with the device lock dropped, so a
    // guest write can land in the middle of it. A ring init that got
    // through would start a kernel worker on a link that is gone,
    // and mark the ring ready for the next kick.
    let steps = Steps::default();
    let device = device_with(Arc::new(RecordingLink::new(&steps)));
    device.halt();
    steps.lock().expect("steps lock").clear();

    VirtioDevice::queue_addr_set(&device, 1, &addressed_queue());

    assert_eq!(
        steps_of(&steps),
        [],
        "a halted device programmed a ring in the kernel",
    );
    assert_eq!(
        device.inner.lock().expect("viona lock").ring_state,
        [RingState::Init; NET_NUM_QUEUES],
    );
}

#[test]
fn a_running_device_takes_a_feature_write() {
    // The kernel needs the features the guest accepted: VERSION_1
    // is what puts viona's rings in the modern layout.
    let steps = Steps::default();
    let device = device_with(Arc::new(RecordingLink::new(&steps)));

    VirtioDevice::set_features(&device, u64::from(VIRTIO_NET_F_MAC));

    assert_eq!(
        steps_of(&steps),
        [Step::SetFeatures(u64::from(VIRTIO_NET_F_MAC))],
    );
}

#[test]
fn a_halted_device_takes_no_feature_write() {
    // On illumos `set_features` issues an ioctl under the device
    // guard. A halt has dropped that guard and is inside an untimed
    // kernel call, so a vCPU that got in would block in the kernel
    // still holding the guard, and the next vCPU would block on the
    // guard: the hot-unplug stall by another route.
    let steps = Steps::default();
    let device = device_with(Arc::new(RecordingLink::new(&steps)));
    device.halt();
    steps.lock().expect("steps lock").clear();

    VirtioDevice::set_features(&device, u64::from(VIRTIO_NET_F_MAC));

    assert_eq!(
        steps_of(&steps),
        [],
        "a halted device sent a feature write to the link",
    );
    assert_eq!(
        device.inner.lock().expect("viona lock").negotiated_features,
        0,
        "a halted device took a feature write",
    );
}

#[test]
fn a_second_halt_does_not_delete_again() {
    // The delete waits for each ring worker to stop. Repeating it
    // would repeat that wait for nothing.
    let steps = Steps::default();
    let device = device_with(Arc::new(RecordingLink::new(&steps)));

    device.halt();
    device.halt();

    assert_eq!(
        steps_of(&steps)
            .iter()
            .filter(|s| **s == Step::Delete)
            .count(),
        1
    );
}

#[test]
fn a_failed_delete_still_leaves_the_device_halted() {
    // Teardown carries on to the destroy, so a failure must not
    // leave the device asking to be halted again.
    let steps = Steps::default();
    let device = device_with(Arc::new(RecordingLink::failing(&steps)));

    device.halt();
    device.halt();

    assert_eq!(
        steps_of(&steps)
            .iter()
            .filter(|s| **s == Step::Delete)
            .count(),
        1
    );
}

#[test]
fn a_halt_drops_a_deferred_poll() {
    // A migration restore can land after the halt. Starting the
    // deferred poll thread then would read a destroyed link.
    let steps = Steps::default();
    let device = device_with(Arc::new(RecordingLink::new(&steps)));
    let raise: QueueInterrupt = Arc::new(|_| {});
    device.store_poll_fn(raise, null_log());

    device.halt();

    let inner = device.inner.lock().expect("viona lock");
    assert!(inner.halted);
    assert!(
        inner.deferred_poll.is_none(),
        "the halt left a deferred poll behind",
    );
}

#[test]
fn a_halted_device_refuses_a_deferred_poll() {
    // A restore that stores its callback after the halt must leave
    // nothing for start_poll_deferred to find.
    let steps = Steps::default();
    let device = device_with(Arc::new(RecordingLink::new(&steps)));
    device.halt();

    let raise: QueueInterrupt = Arc::new(|_| {});
    device.store_poll_fn(raise, null_log());

    assert!(
        device
            .inner
            .lock()
            .expect("viona lock")
            .deferred_poll
            .is_none(),
        "a halted device took a deferred poll",
    );
}

#[test]
fn a_reset_returns_every_ring_to_the_kernel() {
    // The guest writes 0 to the device status. Nothing may kick a
    // ring until the driver programs it again, and the kernel worker
    // has to stop before the guest reuses that memory.
    let steps = Steps::default();
    let device = running_device(Arc::new(RecordingLink::new(&steps)));

    VirtioDevice::reset(&device);

    assert_eq!(steps_of(&steps), [Step::RingReset(0), Step::RingReset(1)]);
    let inner = device.inner.lock().expect("viona lock");
    assert_eq!(inner.negotiated_features, 0);
    assert_eq!(inner.ring_state, [RingState::Init; NET_NUM_QUEUES]);
}

#[test]
fn a_halted_device_resets_no_ring() {
    // Same reason as the feature write: the halt holds no device
    // lock while it sits in an untimed kernel call, so a vCPU can
    // reach this. The halt reset the rings already.
    let steps = Steps::default();
    let device = running_device(Arc::new(RecordingLink::new(&steps)));
    device.halt();
    steps.lock().expect("steps lock").clear();

    VirtioDevice::reset(&device);

    assert_eq!(
        steps_of(&steps),
        [],
        "a halted device sent a ring reset to the link",
    );
}

// No test calls `start_poll_deferred` here. Off illumos its body only
// clears the field the halt cleared already, so a test of it would
// exercise a stub and pin nothing. The guard it holds on illumos is
// the same one `store_poll_fn` holds, which the two tests above pin.
