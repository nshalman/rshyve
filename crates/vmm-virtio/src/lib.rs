// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VirtIO device framework.
//!
//! Implements the VirtIO specification for guest-facing virtual devices,
//! using the legacy PCI transport mechanism. Currently provides:
//!
//! - [`VirtioDevice`] trait for backend device implementations
//! - [`queue::VirtQueue`] - split virtqueue ring buffer
//! - [`pci::VirtioPciDevice`] - PCI transport wrapping a VirtioDevice
//! - [`block::VirtioBlock`] - VirtIO block device backed by a file
//!
//! # Architecture
//!
//! ```text
//! +-----------------------+
//! |  VirtioPciDevice<D>   |  <-- PciDevice trait (attached to PciBus)
//! |  +-----------------+  |
//! |  | Legacy PIO BAR  |  |  <-- BAR0: virtio config registers
//! |  | VirtQueue(s)    |  |  <-- split virtqueue in guest memory
//! |  +-----------------+  |
//! |  |  VirtioDevice D |  |  <-- backend (e.g., VirtioBlock)
//! |  +-----------------+  |
//! +-----------------------+
//! ```

pub mod attach;
pub mod bits;
pub mod block;
pub mod chain_io;
pub mod console;
pub mod fs;
pub mod packed_queue;
pub mod pci;
pub mod queue;
pub mod rng;
pub mod viona;
pub mod vsock;

pub use block::VirtioBlock;
pub use console::VirtioConsole;
pub use fs::VirtioFs;
pub use pci::VirtioPciDevice;
pub use queue::VirtQueue;
pub use rng::VirtioRng;
pub use viona::VirtioViona;

use vmm_core::mem::PhysMap;

fn queue_drain_should_stop(
    queue: &VirtQueue,
    before: u16,
    hit_request_cap: bool,
    physmap: &PhysMap,
) -> bool {
    queue.last_avail_idx() == before
        || hit_request_cap
        || !queue.has_new_avail(physmap)
}

/// Trait implemented by VirtIO backend devices.
///
/// A VirtioDevice provides the device-specific logic (features, config
/// space, request processing) while the PCI transport layer
/// ([`VirtioPciDevice`]) handles the PCI identity, BAR layout, and
/// virtqueue management.
pub trait VirtioDevice: Send + Sync + 'static {
    /// Return the device feature bits (u64 for spec compliance).
    fn device_features(&self) -> u64;

    /// Called when the guest writes the negotiated features.
    fn set_features(&self, features: u64);

    /// Read from device-specific configuration space.
    ///
    /// `offset` is relative to the start of the device-specific region
    /// (BAR0 offset 0x14 in legacy transport).
    fn cfg_read(&self, offset: u16, len: u8) -> u32;

    /// Write to device-specific configuration space.
    ///
    /// The transport calls this with the transport lock held, so that
    /// a reset in flight cannot let the write land. An implementation
    /// must not take that lock again, and must not call
    /// [`VirtioPciDevice::signal_device_needs_reset`], or it stops
    /// itself.
    ///
    /// [`VirtioPciDevice::signal_device_needs_reset`]:
    ///     crate::pci::VirtioPciDevice::signal_device_needs_reset
    fn cfg_write(&self, offset: u16, val: u32, len: u8);

    /// Process a single request from a virtqueue.
    ///
    /// Called when the guest notifies a queue. The device should walk
    /// the descriptor chain starting at `head`, perform the requested
    /// operation, and return the number of bytes written to
    /// device-writable buffers.
    fn process_queue(
        &self,
        queue_idx: u16,
        queue: &mut VirtQueue,
        head: u16,
        physmap: &PhysMap,
    ) -> u32;

    /// Handle a queue notification from the guest.
    ///
    /// The default implementation pops available descriptors and calls
    /// [`process_queue`](Self::process_queue) for each one, then
    /// pushes used ring entries and returns `true` to signal that an
    /// interrupt should be raised.
    ///
    /// Kernel-accelerated devices (e.g., viona) override this to
    /// forward the notification to the kernel driver instead of
    /// processing descriptors in userspace.
    fn notify_queue(
        &self,
        queue_idx: u16,
        queues: &mut [VirtQueue],
        physmap: &PhysMap,
    ) -> bool {
        let queue = match queues.get_mut(queue_idx as usize) {
            Some(q) => q,
            None => return false,
        };

        // Record used_idx before processing for interrupt suppression.
        let old_used_idx = queue.read_used_idx(physmap);

        let mut did_work = false;
        let max_requests = usize::from(queue.size());
        let mut popped = 0usize;

        // EVENT_IDX re-check loop (VirtIO 1.3 sec 2.7.10):
        // After draining the ring and setting used_event, re-read
        // avail_idx. If the guest added entries during the race
        // window, it may have decided not to kick — so we must
        // process them here. Bound the total work to one queue's
        // worth and stop if pop_avail() cannot make forward progress.
        loop {
            let before = queue.last_avail_idx();
            while popped < max_requests {
                let Some(head) = queue.pop_avail(physmap) else {
                    break;
                };
                popped += 1;
                let written =
                    self.process_queue(queue_idx, queue, head, physmap);
                queue.push_used(physmap, head, written);
                did_work = true;
            }

            let hit_request_cap = popped >= max_requests;
            if !hit_request_cap {
                queue.update_used_event(physmap);
            }
            if queue_drain_should_stop(queue, before, hit_request_cap, physmap)
            {
                break;
            }
        }

        // With EVENT_IDX, suppress the interrupt unless the used
        // index crossed the guest's avail_event threshold.
        did_work && queue.should_notify_guest(physmap, old_used_idx)
    }

    /// Called when the guest sets a queue address (PFN) during setup.
    ///
    /// Kernel-accelerated devices override this to inform the kernel
    /// of the queue ring addresses. The default does nothing.
    fn queue_addr_set(&self, _queue_idx: u16, _queue: &VirtQueue) {
        // Default: nothing to do for software-emulated devices
    }

    /// Get kernel ring indices for migration export. Returns (avail_idx, used_idx).
    /// Software devices return the VirtQueue indices; kernel devices query the kernel.
    fn get_kernel_ring_indices(&self, _queue_idx: u16) -> (u16, u16) {
        (0, 0)
    }

    /// Reset all rings before migration restore. Kernel-accelerated
    /// devices pause and reset all ring processing.
    fn reset_all_rings(&self) {}

    /// Restore ring state after migration with current indices + MSI-X.
    fn restore_ring_state(
        &self,
        _queue_idx: u16,
        _size: u16,
        _desc: u64,
        _avail: u64,
        _used: u64,
        _avail_idx: u16,
        _used_idx: u16,
        _msix_addr: u64,
        _msix_data: u32,
    ) {
    }

    /// Pause kernel ring threads for migration export.
    /// Must be called after vCPU pause to quiesce kernel I/O before
    /// reading ring state. Only kernel-accelerated devices need this.
    fn pause_rings_for_export(&self) {}

    /// Program notification addresses after migration.
    fn set_notify_addrs(&self, _pio_port: u16, _mmio_addr: u64) {}

    /// Start the deferred interrupt poll thread after migration restore.
    /// Kernel-accelerated devices (viona) use this to start the poll
    /// thread that was skipped during device creation.
    fn start_poll_deferred(&self) {}

    /// Reset the device to its initial state.
    ///
    /// The transport publishes `DEVICE_STATUS` 0 as soon as this
    /// returns, which tells the guest it can use the ring memory
    /// again. A device with worker threads must therefore not return
    /// until no worker can write the ring or the guest chains. The
    /// transport cannot check this.
    ///
    /// It may take as long as that needs. The transport calls this on
    /// its own drain thread, never on the vCPU that wrote the status
    /// register, so a wait here holds up the one device and nothing
    /// else. Giving up on an unfinished drain is the wrong trade: the
    /// guest frees the vring on reading that 0, and no check made
    /// afterwards takes back a write that has already landed in it.
    fn reset(&self);
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::queue::VirtqDesc;
    use super::*;

    const DESC_GPA: u64 = 0x1000;
    const AVAIL_GPA: u64 = 0x1100;
    const USED_GPA: u64 = 0x1200;

    struct TestDevice {
        processed: AtomicUsize,
        max_expected: usize,
    }

    impl TestDevice {
        fn new(max_expected: usize) -> Self {
            Self {
                processed: AtomicUsize::new(0),
                max_expected,
            }
        }

        fn processed(&self) -> usize {
            self.processed.load(Ordering::Relaxed)
        }
    }

    impl VirtioDevice for TestDevice {
        fn device_features(&self) -> u64 {
            0
        }

        fn set_features(&self, _features: u64) {}

        fn cfg_read(&self, _offset: u16, _len: u8) -> u32 {
            0
        }

        fn cfg_write(&self, _offset: u16, _val: u32, _len: u8) {}

        fn process_queue(
            &self,
            _queue_idx: u16,
            _queue: &mut VirtQueue,
            _head: u16,
            _physmap: &PhysMap,
        ) -> u32 {
            let processed = self.processed.fetch_add(1, Ordering::Relaxed) + 1;
            assert!(processed <= self.max_expected, "queue drain exceeded cap");
            0
        }

        fn reset(&self) {}
    }

    fn configured_queue(size: u16, avail_idx: u16) -> (PhysMap, VirtQueue) {
        let physmap =
            PhysMap::new_anon(DESC_GPA, 0x3000).expect("create queue memory");
        let mut queue = VirtQueue::new(size);
        queue.set_addr_modern(DESC_GPA, AVAIL_GPA, USED_GPA);
        queue.set_event_idx(true);

        physmap
            .lookup(AVAIL_GPA + 2, 2)
            .expect("mapped avail index")
            .write::<u16>(&avail_idx)
            .expect("write avail index");
        for idx in 0..size {
            physmap
                .lookup(AVAIL_GPA + 4 + u64::from(idx) * 2, 2)
                .expect("mapped avail entry")
                .write::<u16>(&idx)
                .expect("write avail entry");
            physmap
                .lookup(DESC_GPA + u64::from(idx) * 16, 16)
                .expect("mapped descriptor")
                .write(&VirtqDesc {
                    addr: 0,
                    len: 0,
                    flags: 0,
                    next: 0,
                })
                .expect("write descriptor");
        }

        (physmap, queue)
    }

    #[test]
    fn notify_queue_terminates_when_avail_ring_entry_is_unmapped() {
        const REGION_GPA: u64 = 0x4000;
        const REGION_SIZE: usize = 0x1000;
        const REGION_END: u64 = REGION_GPA + REGION_SIZE as u64;

        let physmap = PhysMap::new_anon(REGION_GPA, REGION_SIZE)
            .expect("create guest memory");
        let mut queue = VirtQueue::new(2);
        queue.set_addr_modern(REGION_GPA, REGION_END - 4, REGION_GPA + 0x100);
        queue.set_event_idx(true);
        physmap
            .lookup(REGION_END - 2, 2)
            .expect("mapped avail index")
            .write::<u16>(&1)
            .expect("write avail index");

        let before = queue.last_avail_idx();
        assert_eq!(queue.pop_avail(&physmap), None);
        queue.update_used_event(&physmap);

        assert_eq!(queue.last_avail_idx(), before);
        assert!(queue.has_new_avail(&physmap));
        assert!(
            queue_drain_should_stop(&queue, before, false, &physmap),
            "the EVENT_IDX re-check must stop when pop_avail makes no progress",
        );

        // And end to end: notify_queue itself must return. Run it on a
        // worker so a regression fails this test on a timeout instead of
        // wedging the whole suite, which is what the bug did to a vCPU.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("virtio-drain-timeout".into())
            .spawn(move || {
                let device = TestDevice::new(0);
                let mut queues = [queue];
                device.notify_queue(0, &mut queues, &physmap);
                let _ = tx.send(());
            })
            .expect("spawn drain worker");
        rx.recv_timeout(std::time::Duration::from_secs(10)).expect(
            "notify_queue must return when the avail ring entry is unmapped",
        );
    }

    #[test]
    fn default_notify_queue_is_capped_at_queue_size() {
        let (physmap, queue) = configured_queue(2, 4);
        let device = TestDevice::new(2);
        let mut queues = [queue];

        device.notify_queue(0, &mut queues, &physmap);

        assert_eq!(device.processed(), 2);
        assert_eq!(queues[0].last_avail_idx(), 2);
        assert!(queues[0].has_new_avail(&physmap));
    }

    #[test]
    fn default_notify_queue_drains_ring_and_updates_avail_event() {
        let (physmap, queue) = configured_queue(4, 2);
        let device = TestDevice::new(4);
        let mut queues = [queue];

        device.notify_queue(0, &mut queues, &physmap);

        let avail_event_gpa = USED_GPA + 4 + u64::from(queues[0].size()) * 8;
        let avail_event = physmap
            .lookup(avail_event_gpa, 2)
            .expect("mapped avail event")
            .read::<u16>()
            .expect("read avail event");
        assert_eq!(device.processed(), 2);
        assert_eq!(queues[0].last_avail_idx(), 2);
        assert_eq!(queues[0].read_used_ring_idx(&physmap), 2);
        assert_eq!(avail_event, 2);
    }
}
