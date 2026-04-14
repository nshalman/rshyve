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

pub mod bits;
pub mod block;
pub mod packed_queue;
pub mod pci;
pub mod queue;
pub mod rng;
pub mod viona;

pub use block::VirtioBlock;
pub use pci::VirtioPciDevice;
pub use queue::VirtQueue;
pub use rng::VirtioRng;
pub use viona::VirtioViona;

use vmm_core::mem::PhysMap;

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

        // EVENT_IDX re-check loop (VirtIO 1.3 sec 2.7.10):
        // After draining the ring and setting used_event, re-read
        // avail_idx. If the guest added entries during the race
        // window, it may have decided not to kick — so we must
        // process them here.
        loop {
            while let Some(head) = queue.pop_avail(physmap) {
                let written = self.process_queue(queue_idx, queue, head, physmap);
                queue.push_used(physmap, head, written);
                did_work = true;
            }
            queue.update_used_event(physmap);
            if !queue.has_new_avail(physmap) {
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
    fn queue_addr_set(
        &self,
        _queue_idx: u16,
        _queue: &VirtQueue,
    ) {
        // Default: nothing to do for software-emulated devices
    }

    /// Get kernel ring indices for migration export. Returns (avail_idx, used_idx).
    /// Software devices return the VirtQueue indices; kernel devices query the kernel.
    fn get_kernel_ring_indices(&self, _queue_idx: u16) -> (u16, u16) { (0, 0) }

    /// Reset all rings before migration restore. Kernel-accelerated
    /// devices pause and reset all ring processing.
    fn reset_all_rings(&self) {}

    /// Restore ring state after migration with current indices + MSI-X.
    fn restore_ring_state(
        &self,
        _queue_idx: u16, _size: u16,
        _desc: u64, _avail: u64, _used: u64,
        _avail_idx: u16, _used_idx: u16,
        _msix_addr: u64, _msix_data: u32,
    ) {}

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
    fn reset(&self);
}
