// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The device around the link: the halt, the guards that keep a
//! destroyed link out of the kernel, and what a migration asks of it.
//!
//! A device here holds a recording link in place of the kernel handle.
//! That is the same slot the illumos build fills, so every test below
//! runs on the target too.

use std::sync::{mpsc, Arc};
use std::time::Duration;

use vmm_core::mem::PhysMap;
use vmm_devices::Lifecycle;

use super::{null_log, steps_of, Hold, Op, Step, Steps, TestLink};
use crate::queue::VirtQueue;
use crate::viona::halt::Poller;
use crate::viona::{
    RingState, VirtioViona, ETHERADDRL, NET_NUM_QUEUES, NET_QUEUE_SIZE,
    VIRTIO_NET_F_MAC,
};
use crate::VirtioDevice;
use viona_api::LinkOps;

/// How long a test waits before it calls a thread stuck.
///
/// A wait that runs out is a failure, never a pass. It is long
/// enough that a loaded machine does not reach it.
const STUCK: Duration = Duration::from_secs(15);

/// The MAC the test VNIC answers for.
const TEST_MAC: [u8; ETHERADDRL] = [0x02, 0x08, 0x20, 0xaa, 0xbb, 0xcc];

impl TestLink {
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

/// A device whose calls into the kernel are watched through its link.
///
/// The same constructor the illumos build uses once it has opened a
/// link, so nothing here builds a device shape the product does not.
pub(super) fn device_with(link: Arc<dyn LinkOps>) -> VirtioViona {
    VirtioViona::with_link(link, TEST_MAC, VIRTIO_NET_F_MAC)
}

/// A device the guest has finished programming.
pub(super) fn running_device(link: Arc<dyn LinkOps>) -> VirtioViona {
    let device = device_with(link);
    {
        let mut inner = device.inner.lock().expect("viona lock");
        inner.negotiated_features = u64::from(VIRTIO_NET_F_MAC);
        inner.ring_state = [RingState::Ready; NET_NUM_QUEUES];
    }
    device
}

/// A queue the guest has given addresses to.
pub(super) fn addressed_queue() -> VirtQueue {
    let mut queue = VirtQueue::new(NET_QUEUE_SIZE);
    queue.set_addr_modern(0x1_0000, 0x2_0000, 0x3_0000);
    queue
}

pub(super) fn notify(device: &VirtioViona, queue_idx: u16) {
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
    let device = device_with(Arc::new(TestLink::new(&steps)));

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
    let device = device_with(Arc::new(TestLink::new(&steps)));
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
    let link = Arc::new(TestLink::new(&steps));
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
    let steps = Steps::default();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let device =
        Arc::new(device_with(Arc::new(TestLink::new(&steps).holding(Hold {
            entered: entered_tx,
            release: release_rx,
        }))));

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
    let device = running_device(Arc::new(TestLink::new(&steps)));

    notify(&device, 1);

    assert_eq!(steps_of(&steps), [Step::RingKick(1)]);
}

/// The guest picks this index. `pci/legacy.rs` and `pci/modern.rs` both
/// take it from the notify register with `val as u16` and pass it on
/// unchecked, so the bound in `ring_kick` is the only one there is.
/// Indexing `ring_state` past its end panics the vCPU thread while it
/// holds the device mutex, and the seven other `expect` sites on that
/// mutex then panic in turn.
#[test]
fn a_kick_naming_a_queue_that_does_not_exist_is_refused() {
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));

    for idx in [NET_NUM_QUEUES as u16, 2, 3, 255, u16::MAX] {
        notify(&device, idx);
    }

    assert_eq!(
        steps_of(&steps),
        [],
        "a queue index the device does not have reached the link",
    );
}

/// The harm a missing bound does is not one panic. It is the poisoned
/// mutex every later caller finds.
#[test]
fn a_kick_from_a_hostile_guest_leaves_the_device_usable() {
    let steps = Steps::default();
    let device = Arc::new(running_device(Arc::new(TestLink::new(&steps))));

    // A vCPU thread, the way the transport calls in.
    let vcpu = Arc::clone(&device);
    std::thread::spawn(move || notify(&vcpu, u16::MAX))
        .join()
        .expect("a guest queue index must not panic the vCPU thread");

    // What every other thread does next.
    assert!(
        device.inner.lock().is_ok(),
        "the device mutex was poisoned, so every other caller panics",
    );
    notify(&device, 1);
    assert_eq!(
        steps_of(&steps),
        [Step::RingKick(1)],
        "a real kick stopped working after the hostile one",
    );
}

/// The guest reaches this by accepting features and then kicking,
/// before it has written a single ring address. viona starts a ring
/// worker in `ring_init`, so a ring the guest never programmed has
/// none, and the kick walks into the mac layer with a NULL there.
///
/// The features guard above does not cover it: the guest sets
/// FEATURES_OK first, which is the order the specification gives it.
#[test]
fn a_kick_before_the_guest_programmed_a_ring_is_refused() {
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));

    VirtioDevice::set_features(&device, u64::from(VIRTIO_NET_F_MAC));
    for ring in 0..NET_NUM_QUEUES as u16 {
        notify(&device, ring);
    }

    assert_eq!(
        steps_of(&steps),
        [Step::SetFeatures(u64::from(VIRTIO_NET_F_MAC))],
        "a kick reached a ring the kernel has no worker for",
    );
}

/// The guard is per ring, not per device. A guest that programs one
/// ring and kicks the other reaches the same NULL as the one that
/// programs nothing.
#[test]
fn a_kick_reaches_only_the_ring_the_guest_programmed() {
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));
    VirtioDevice::set_features(&device, u64::from(VIRTIO_NET_F_MAC));
    VirtioDevice::queue_addr_set(&device, 0, &addressed_queue());
    steps.lock().expect("steps lock").clear();

    notify(&device, 0);
    notify(&device, 1);

    assert_eq!(steps_of(&steps), [Step::RingKick(0)]);
}

#[test]
fn a_halt_leaves_no_ring_a_kick_can_reach() {
    // The halt drops the device lock before it destroys the link,
    // so a vCPU can reach notify_queue while the delete runs. It
    // must find no ring ready to kick.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));

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
    let device = device_with(Arc::new(TestLink::new(&steps)));

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

/// A legacy driver retires a ring by writing `QUEUE_PFN = 0` and then
/// frees the pages. A kernel ring left running reads and writes memory
/// the guest has already put to another use.
#[test]
fn retiring_a_ring_resets_it_in_the_kernel() {
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));

    // What `set_addr_legacy(0)` leaves behind: no addresses.
    VirtioDevice::queue_addr_set(&device, 1, &VirtQueue::new(NET_QUEUE_SIZE));

    assert_eq!(
        steps_of(&steps),
        [Step::RingReset(1)],
        "the kernel kept the ring the driver retired",
    );
    assert_eq!(
        device.inner.lock().expect("viona lock").ring_state[1],
        RingState::Init,
        "a retired ring stayed ready, so the next kick would reach it",
    );
}

/// The kernel refuses `RING_INIT` unless the ring is reset, so a ring
/// reprogrammed without a device reset has to be retired first or the
/// new addresses never reach the kernel at all.
#[test]
fn reprogramming_a_ring_resets_it_before_programming_again() {
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));

    VirtioDevice::queue_addr_set(&device, 1, &addressed_queue());

    assert_eq!(
        steps_of(&steps),
        [
            Step::RingReset(1),
            Step::RingInit {
                ring: 1,
                size: NET_QUEUE_SIZE,
                desc: 0x1_0000,
                avail: 0x2_0000,
                used: 0x3_0000,
            },
        ],
        "the ring was programmed over while the kernel still ran it",
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
    let device = device_with(Arc::new(TestLink::new(&steps)));
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
    let device = device_with(Arc::new(TestLink::new(&steps)));

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
    let device = device_with(Arc::new(TestLink::new(&steps)));
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
    let device = device_with(Arc::new(TestLink::new(&steps)));

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
    let link = TestLink::new(&steps).failing(&[Op::Delete]);
    let device = device_with(Arc::new(link));

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
    let device = device_with(Arc::new(TestLink::new(&steps)));
    device.defer_intr_poll(null_log());

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
    let device = device_with(Arc::new(TestLink::new(&steps)));
    device.halt();

    device.defer_intr_poll(null_log());

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
    let device = running_device(Arc::new(TestLink::new(&steps)));

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
    let device = running_device(Arc::new(TestLink::new(&steps)));
    device.halt();
    steps.lock().expect("steps lock").clear();

    VirtioDevice::reset(&device);

    assert_eq!(
        steps_of(&steps),
        [],
        "a halted device sent a ring reset to the link",
    );
}

#[test]
fn a_deferred_start_kicks_the_rings_a_restore_programmed() {
    // Programming the ring state sets the indices but does not always
    // wake the kernel worker. Without this second kick a migrated
    // guest gets no traffic until it kicks a ring itself, which on an
    // idle rx ring never happens.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));
    device.defer_intr_poll(null_log());

    device.start_poll_deferred();
    // The halt joins the poll thread this started, so the test never
    // leaves one behind.
    device.halt();

    assert_eq!(
        steps_of(&steps)
            .iter()
            .take_while(|step| **step != Step::RingReset(0))
            .copied()
            .collect::<Vec<_>>(),
        [Step::RingKick(0), Step::RingKick(1)],
        "a deferred start left the restored rings unkicked",
    );
}

#[test]
fn a_deferred_start_kicks_no_ring_the_guest_has_not_programmed() {
    // A kick to a ring the kernel has no worker for is refused, and on
    // an older viona it dereferences a NULL in the mac layer.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));
    device.defer_intr_poll(null_log());

    device.start_poll_deferred();
    device.halt();

    assert_eq!(
        steps_of(&steps),
        [Step::RingReset(0), Step::RingReset(1), Step::Delete],
        "a deferred start kicked a ring that was never programmed",
    );
}

#[test]
fn a_halted_device_starts_no_deferred_poll() {
    // Two guards stand between a migration restore and a destroyed
    // link: the halt drops the deferred start, and this refuses one
    // that is left behind. The one below is planted by hand, because
    // the first guard closed every other route to it.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));
    device.halt();
    steps.lock().expect("steps lock").clear();
    device.inner.lock().expect("viona lock").deferred_poll = Some(null_log());

    device.start_poll_deferred();

    let inner = device.inner.lock().expect("viona lock");
    assert!(inner.poller.is_none(), "a halted device started a poller");
    assert!(
        inner.deferred_poll.is_none(),
        "the refusal left the deferred start in place",
    );
    assert_eq!(
        steps_of(&steps),
        [],
        "a halted device kicked a ring after the delete",
    );
}

#[test]
fn a_migration_source_reads_the_indices_the_kernel_reports() {
    // These two go into the payload the destination resumes from. A
    // pair that swaps replays descriptors the guest already reclaimed,
    // and the guest driver reports "id N is not a head!".
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));

    assert_eq!(
        VirtioDevice::get_kernel_ring_indices(&device, 1),
        (0x1112, 0x2223),
    );
}

#[test]
fn a_halted_device_reads_no_ring_state() {
    // An export can run while the halt is inside the delete, which
    // holds no device lock. An ioctl that got in would wait out that
    // untimed kernel call holding the lock every vCPU needs, on a
    // link that is destroyed.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));
    device.halt();
    steps.lock().expect("steps lock").clear();

    assert_eq!(device.get_kernel_ring_state(1), None);

    assert_eq!(
        steps_of(&steps),
        [],
        "a ring state read reached the link after the delete",
    );
}

#[test]
fn a_migration_source_reads_the_state_of_the_ring_it_names() {
    // The read carries the ring, and the answer goes into that ring's
    // payload. One that always read ring 0 would give every ring ring
    // 0's indices.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));

    assert_eq!(device.get_kernel_ring_state(1), Some((0x1112, 0x2223)));

    assert_eq!(steps_of(&steps), [Step::RingGetState(1)]);
}

#[test]
fn a_restore_refuses_indices_no_running_ring_can_have_left() {
    // The export answers with a pair of numbers and cannot say that
    // the kernel refused the read, so a ring it could not read for
    // arrives with a consumption cursor of 0 against a used index the
    // export took from guest memory. Programmed as is, viona treats
    // the whole available ring as unconsumed and hands the driver
    // descriptors it has already reclaimed.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));

    VirtioDevice::restore_ring_state(
        &device,
        1,
        NET_QUEUE_SIZE,
        0x1_0000,
        0x2_0000,
        0x3_0000,
        0,
        900,
        0,
        0,
    );

    assert_eq!(
        steps_of(&steps),
        [],
        "a ring with more chains in flight than descriptors was programmed",
    );
}

#[test]
fn a_restore_takes_indices_that_wrapped() {
    // Both cursors count in a u16 and wrap with it. A check that read
    // the pair as plain numbers would refuse every ring the source
    // paused across a wrap, and a migrated guest would lose the queue.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));

    VirtioDevice::restore_ring_state(
        &device,
        1,
        NET_QUEUE_SIZE,
        0x1_0000,
        0x2_0000,
        0x3_0000,
        3,
        u16::MAX - 5,
        0,
        0,
    );

    assert!(
        steps_of(&steps)
            .iter()
            .any(|step| matches!(step, Step::RingSetState(_))),
        "a ring whose indices wrapped was refused",
    );
}

#[test]
fn a_restore_programs_the_ring_and_its_interrupt() {
    // The addresses and the indices go in together, because ring_init
    // would put the indices back to 0. The features come first, since
    // VERSION_1 is what puts the kernel rings in the modern layout the
    // addresses are read in. The kick that follows is what makes the
    // kernel worker look at what the source left available.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));

    VirtioDevice::restore_ring_state(
        &device,
        1,
        NET_QUEUE_SIZE,
        0x1_0000,
        0x2_0000,
        0x3_0000,
        7,
        5,
        0xFEE0_0000,
        0x21,
    );

    assert_eq!(
        steps_of(&steps),
        [
            Step::SetFeatures(u64::from(VIRTIO_NET_F_MAC)),
            Step::RingSetState(viona_api::vioc_ring_state {
                vrs_index: 1,
                vrs_avail_idx: 7,
                vrs_used_idx: 5,
                vrs_qsize: NET_QUEUE_SIZE,
                vrs_qaddr_desc: 0x1_0000,
                vrs_qaddr_avail: 0x2_0000,
                vrs_qaddr_used: 0x3_0000,
            }),
            Step::RingSetMsi {
                ring: 1,
                addr: 0xFEE0_0000,
                msg: 0x21,
            },
            Step::RingKick(1),
        ],
    );
    assert_eq!(
        device.inner.lock().expect("viona lock").ring_state[1],
        RingState::Ready,
    );
}

#[test]
fn a_halted_device_restores_no_ring() {
    // A restore that races the halt would program a link that is
    // already destroyed, and mark the ring ready for the next kick.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));
    device.halt();
    steps.lock().expect("steps lock").clear();

    VirtioDevice::restore_ring_state(
        &device,
        1,
        NET_QUEUE_SIZE,
        0x1_0000,
        0x2_0000,
        0x3_0000,
        7,
        5,
        0,
        0,
    );

    assert_eq!(
        steps_of(&steps),
        [],
        "a halted device programmed a ring in the kernel",
    );
}

#[test]
fn an_export_pauses_every_ring_before_it_reads_them() {
    // A ring the kernel still works on moves its indices while the
    // source reads them, and the destination resumes from a cursor
    // that was never true. The state is read before each pause,
    // because that is what a rollback has to put back.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));

    VirtioDevice::pause_rings_for_export(&device).expect("pauses");

    assert_eq!(
        steps_of(&steps),
        [
            Step::RingGetState(0),
            Step::RingPause(0),
            Step::RingGetState(1),
            Step::RingPause(1),
        ],
    );
}

#[test]
fn a_rollback_puts_every_paused_ring_back_to_work() {
    // A paused ring is VRS_STOP, and viona answers a guest kick on one
    // with EBUSY, so only a reset plus a state write revives it. The
    // source guest would otherwise resume with a dead NIC.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));

    VirtioDevice::pause_rings_for_export(&device).expect("pauses");
    steps.lock().expect("steps").clear();

    VirtioDevice::resume_rings_after_migration(&device).expect("resumes");

    let seen = steps_of(&steps);
    assert!(
        seen.contains(&Step::RingReset(0))
            && seen.contains(&Step::RingReset(1)),
        "{seen:?}",
    );
    assert_eq!(
        seen.iter()
            .filter(|s| matches!(s, Step::RingSetState { .. }))
            .count(),
        2,
        "both rings get their saved state back: {seen:?}",
    );
}

#[test]
fn a_ring_that_will_not_pause_fails_the_migration() {
    // The export would otherwise read indices that are still moving.
    let steps = Steps::default();
    let device = running_device(Arc::new(
        TestLink::new(&steps).failing(&[Op::RingPause]),
    ));

    VirtioDevice::pause_rings_for_export(&device)
        .expect_err("a ring that stays running must fail the migration");
}

#[test]
fn a_restore_stops_every_ring_before_it_programs_them() {
    // The destination pauses and resets each ring before it programs
    // the state the source left. A worker still running would read the
    // ring while its addresses change under it.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));

    VirtioDevice::reset_all_rings(&device);

    assert_eq!(
        steps_of(&steps),
        [
            Step::RingPause(0),
            Step::RingReset(0),
            Step::RingPause(1),
            Step::RingReset(1),
        ],
    );
}

#[test]
fn a_halted_device_pauses_no_ring() {
    // An export can run while the halt is inside the delete: the halt
    // holds no device lock there. A pause that got in could wait out
    // that untimed kernel call holding the lock every vCPU needs.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));
    device.halt();
    steps.lock().expect("steps lock").clear();

    VirtioDevice::pause_rings_for_export(&device)
        .expect("a halted device has nothing to pause");

    assert_eq!(
        steps_of(&steps),
        [],
        "a halted device paused a ring in the kernel",
    );
}

#[test]
fn a_halted_device_resets_no_ring_for_a_restore() {
    // Same window, from the destination side. The halt reset every
    // ring already and destroyed the link.
    let steps = Steps::default();
    let device = running_device(Arc::new(TestLink::new(&steps)));
    device.halt();
    steps.lock().expect("steps lock").clear();

    VirtioDevice::reset_all_rings(&device);

    assert_eq!(
        steps_of(&steps),
        [],
        "a halted device reset a ring in the kernel",
    );
}

#[test]
fn a_halted_device_sends_no_notification_address() {
    // The restore programs these one step before it starts the poll
    // thread, and that start is guarded already. This is the same
    // window: the link is gone and the ioctl would go under the
    // device lock.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));
    device.halt();
    steps.lock().expect("steps lock").clear();

    VirtioDevice::set_notify_addrs(&device, 0x2000, 0xC000_0000);

    assert_eq!(
        steps_of(&steps),
        [],
        "a halted device gave the kernel a notification address",
    );
}

#[test]
fn a_halted_device_refuses_a_promiscuous_mode_change() {
    // The one link call that reports to its caller. Silence here would
    // tell a zone with allow_mac_spoofing that its guest can see every
    // frame, on a link that no longer exists.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));
    device.halt();
    steps.lock().expect("steps lock").clear();

    device
        .set_promisc(true)
        .expect_err("a halted device has no link to set");

    assert_eq!(
        steps_of(&steps),
        [],
        "a halted device sent a promiscuous mode to the link",
    );
}

#[test]
fn a_notification_address_reaches_the_kernel() {
    // With these the kernel takes the guest's kick itself and the vCPU
    // does not leave to userspace. A port or an address that drifts
    // sends the kick nowhere and stalls the ring.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));

    VirtioDevice::set_notify_addrs(&device, 0x2000, 0xC000_0000);

    assert_eq!(
        steps_of(&steps),
        [
            Step::SetNotifyIop(0x2000),
            Step::SetNotifyMmio {
                addr: 0xC000_0000,
                size: 0x1000,
            },
        ],
    );
}

#[test]
fn an_unprogrammed_notification_address_is_not_sent() {
    // A transport with no legacy BAR passes port 0, and one with no
    // modern BAR passes address 0. Neither is an address the guest
    // writes, and the kernel would take them as one.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));

    VirtioDevice::set_notify_addrs(&device, 0, 0);

    assert_eq!(steps_of(&steps), []);
}

#[test]
fn promiscuous_mode_follows_the_zone_config() {
    // A VM with allow_mac_spoofing needs every frame the physical link
    // carries. One without must not see its neighbours' traffic.
    let steps = Steps::default();
    let device = device_with(Arc::new(TestLink::new(&steps)));

    device.set_promisc(true).expect("the test link answers");
    device.set_promisc(false).expect("the test link answers");

    assert_eq!(
        steps_of(&steps),
        [
            Step::SetPromisc(viona_api::PromiscMode::All),
            Step::SetPromisc(viona_api::PromiscMode::None),
        ],
    );
}
