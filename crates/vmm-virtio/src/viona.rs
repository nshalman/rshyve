// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VirtIO network device backed by the viona kernel driver.
//!
//! Unlike virtio-blk where I/O is processed in userspace, viona handles
//! packet processing entirely in-kernel. Userspace is responsible for:
//!
//! 1. Opening `/dev/viona` and associating it with a VNIC and the VMM fd
//! 2. Programming virtqueue ring addresses into the kernel
//! 3. Forwarding queue notifications (kicks) to the kernel
//! 4. Providing the virtio-net device config (MAC address, status)
//!
//! # Queue layout
//!
//! - Queue 0: RX (receive)
//! - Queue 1: TX (transmit)
//!
//! # Where the rest lives
//!
//! - [`link`]: the in-kernel calls, behind one trait
//! - [`halt`]: the teardown order, which gives the VM its hold back
//! - [`poll`]: the interrupt poll thread
//! - [`migrate`]: the ring state a migration moves
//! - [`setup`]: opening the link and pointing the kernel at it

use std::sync::{Arc, Mutex};

use vmm_core::mem::PhysMap;

use super::pci::intr::BackendIntr;
use super::queue::VirtQueue;
use super::VirtioDevice;
use link::LinkOps;

mod halt;
mod link;
mod migrate;
mod poll;
mod setup;

#[cfg(test)]
mod tests;

/// Size of the virtio-net device config space.
///
/// Layout (legacy transport):
/// - `[0..6]`  MAC address (6 bytes)
/// - `[6..8]`  status (u16)
pub const NET_CONFIG_SIZE: u16 = 8;

/// Number of virtqueues: RX + TX.
pub const NET_NUM_QUEUES: usize = 2;

/// Default virtqueue size for viona queues.
pub const NET_QUEUE_SIZE: u16 = 256;

/// Ethernet address length.
const ETHERADDRL: usize = 6;

// VirtIO net feature bits (from VirtIO 1.2 sec 5.1.3).
const VIRTIO_NET_F_MAC: u32 = 1 << 5;
const VIRTIO_NET_F_STATUS: u32 = 1 << 16;

// VirtIO net status bits.
const VIRTIO_NET_S_LINK_UP: u16 = 1;

/// Per-ring initialization state.
///
/// Tracks whether a ring has been programmed with valid addresses
/// via `ring_init`. Kicks to rings in `Init` state are suppressed
/// to prevent kernel NULL pointer dereferences.
#[derive(Clone, Copy, Debug, PartialEq)]
enum RingState {
    /// Created but not yet configured with addresses.
    Init,
    /// `ring_init` succeeded; addresses are programmed in-kernel.
    Ready,
}

/// VirtIO network device backed by the illumos viona kernel driver.
///
/// Only illumos can open a link, so [`VirtioViona::new`] fails
/// elsewhere. Everything above the link is the same code on every
/// platform.
pub struct VirtioViona {
    inner: Mutex<VionaInner>,

    /// The transport interrupt path, filled by the binary once the
    /// transport exists.
    ///
    /// Outside `inner` because the poll thread reads it on every
    /// wakeup and must never wait on the device lock: the halt drops
    /// that lock across an untimed kernel call.
    interrupt: Arc<Mutex<Option<VionaInterrupt>>>,
}

/// The transport interrupt path, installed by the binary after the
/// transport is built.
///
/// The poll thread names the driver session each notification was
/// collected under, so a reset in between refuses it. A bare
/// `Fn(u16)` cannot: it has no session to name.
pub type VionaInterrupt = Arc<BackendIntr>;

struct VionaInner {
    /// The in-kernel link, on every platform.
    ///
    /// On illumos this holds the open `/dev/viona` handle. It is a
    /// trait object there too, so a test can put its own link in the
    /// same slot and run where the product runs.
    ///
    /// Shared, so the halt can keep the link after it drops the device
    /// lock. The calls the halt makes are untimed kernel waits.
    link: Arc<dyn LinkOps>,

    /// MAC address of the VNIC.
    mac_addr: [u8; ETHERADDRL],

    /// Features offered by the kernel driver.
    dev_features: u32,

    /// Features negotiated by the guest.
    negotiated_features: u64,

    /// Per-ring initialization state (RX=0, TX=1).
    ring_state: [RingState; NET_NUM_QUEUES],

    /// Deferred interrupt poll state for migration destinations.
    /// Holds the logger so the poll thread can be started after ring
    /// state is restored.
    deferred_poll: Option<slog::Logger>,

    /// The running interrupt poll thread, if one was started.
    poller: Option<halt::Poller>,

    /// The logger the interrupt wiring carried.
    ///
    /// `VirtioViona::new` is given none, so a halt reports through the
    /// first logger the device was handed.
    log: Option<slog::Logger>,

    /// The link is destroyed. Nothing may program it again.
    halted: bool,
}

impl VionaInner {
    /// The link, by value, so a caller can drop the device lock and
    /// still hold it.
    fn link(&self) -> Arc<dyn LinkOps> {
        Arc::clone(&self.link)
    }

    /// The link, borrowed for one call under the device lock.
    fn link_ref(&self) -> &dyn LinkOps {
        self.link.as_ref()
    }

    /// Where a halt reports, discarding when the device got no logger.
    fn log(&self) -> slog::Logger {
        self.log
            .clone()
            .unwrap_or_else(|| slog::Logger::root(slog::Discard, slog::o!()))
    }
}

impl VirtioViona {
    /// The interrupt slot, for the binary to fill after the transport
    /// is built.
    pub fn interrupt(&self) -> Arc<Mutex<Option<VionaInterrupt>>> {
        Arc::clone(&self.interrupt)
    }

    /// Kick an in-kernel virtqueue ring.
    ///
    /// Only kicks if the queue index is valid (0=RX, 1=TX), features
    /// have been negotiated, and the ring has been initialized via
    /// `ring_init`. Silently ignores kicks to unconfigured or
    /// out-of-range queues to prevent kernel NULL pointer dereferences
    /// in the mac layer.
    fn ring_kick(inner: &VionaInner, queue_idx: u16) {
        let idx = queue_idx as usize;
        // Only kick valid queues (RX=0, TX=1)
        if idx >= NET_NUM_QUEUES {
            return;
        }
        // Only kick if features have been negotiated (device is ready)
        if inner.negotiated_features == 0 {
            return;
        }
        // Only kick if ring_init has programmed addresses into the kernel
        if inner.ring_state[idx] != RingState::Ready {
            return;
        }
        if let Err(e) = inner.link_ref().ring_kick(queue_idx) {
            slog::warn!(inner.log(), "viona ring kick failed";
                "ring" => queue_idx, "error" => %e);
        }
    }

    /// Program the ring addresses for a queue into the kernel.
    ///
    /// On success, marks the ring as `Ready` so that subsequent kicks
    /// are forwarded to the kernel driver.
    fn ring_init(inner: &mut VionaInner, queue_idx: u16, queue: &VirtQueue) {
        if !queue.is_configured() {
            return;
        }

        // Use the actual addresses from VirtQueue: they hold for both
        // legacy (set_addr_legacy) and modern (set_addr_modern).
        let rc = inner.link_ref().ring_init(
            queue_idx,
            queue.size(),
            queue.desc_addr(),
            queue.avail_addr(),
            queue.used_addr(),
        );
        match rc {
            Ok(()) => {
                let idx = queue_idx as usize;
                if idx < NET_NUM_QUEUES {
                    inner.ring_state[idx] = RingState::Ready;
                }
            }
            // The ring stays unready, so no kick reaches a ring the
            // kernel does not have.
            Err(e) => slog::warn!(inner.log(), "viona ring init failed";
                "ring" => queue_idx, "error" => %e),
        }
    }

    /// Set negotiated features in the kernel driver.
    fn set_kernel_features(inner: &VionaInner, features: u64) {
        if let Err(e) = inner.link_ref().set_features(features) {
            slog::warn!(inner.log(), "viona set features failed";
                "features" => features, "error" => %e);
        }
    }
}

impl VirtioDevice for VirtioViona {
    fn device_features(&self) -> u64 {
        let inner = self.inner.lock().expect("viona lock poisoned");
        // Combine kernel features with the features we always advertise.
        let mut features = u64::from(inner.dev_features);
        features |= u64::from(VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS);
        // Strip EVENT_IDX: viona handles rings in-kernel and
        // manages its own notification suppression protocol.
        // Don't advertise it from userspace.
        features &= !super::bits::VIRTIO_F_RING_EVENT_IDX;
        features
    }

    fn set_features(&self, features: u64) {
        let mut inner = self.inner.lock().expect("viona lock poisoned");
        // A halt in flight has dropped this lock and is inside an
        // untimed kernel call. A vCPU that issued an ioctl here would
        // wait for that call inside the kernel while still holding
        // this guard, and every other vCPU would then wait for the
        // guard. That is the hot-unplug stall by another route.
        if inner.halted {
            return;
        }
        inner.negotiated_features = features;
        Self::set_kernel_features(&inner, features);
    }

    fn cfg_read(&self, offset: u16, len: u8) -> u32 {
        let inner = self.inner.lock().expect("viona lock poisoned");

        // Build the full 8-byte config in a buffer, then extract
        // the requested slice. This correctly handles all access
        // widths and alignments.
        let status = VIRTIO_NET_S_LINK_UP;
        let cfg: [u8; 8] = [
            inner.mac_addr[0],
            inner.mac_addr[1],
            inner.mac_addr[2],
            inner.mac_addr[3],
            inner.mac_addr[4],
            inner.mac_addr[5],
            (status & 0xFF) as u8,
            (status >> 8) as u8,
        ];

        let off = offset as usize;
        let nbytes = (len as usize).min(4);
        if off >= cfg.len() {
            return 0;
        }

        let mut val = 0u32;
        for i in 0..nbytes {
            if off + i < cfg.len() {
                val |= u32::from(cfg[off + i]) << (i * 8);
            }
        }
        val
    }

    fn cfg_write(&self, _offset: u16, _val: u32, _len: u8) {
        // Network device config is read-only.
    }

    fn process_queue(
        &self,
        _queue_idx: u16,
        _queue: &mut VirtQueue,
        _head: u16,
        _physmap: &PhysMap,
    ) -> u32 {
        // For viona, individual descriptor processing never happens
        // in userspace. This method should not be called because we
        // override notify_queue.
        0
    }

    fn notify_queue(
        &self,
        queue_idx: u16,
        _queues: &mut [VirtQueue],
        _physmap: &PhysMap,
    ) -> bool {
        // Forward the notification to the kernel driver.
        // The kernel handles popping descriptors, processing packets,
        // and pushing used entries.
        let inner = self.inner.lock().expect("viona lock poisoned");
        Self::ring_kick(&inner, queue_idx);

        // Return false: the kernel handles interrupts directly,
        // so the PCI transport should not raise a legacy interrupt.
        false
    }

    fn get_kernel_ring_indices(&self, queue_idx: u16) -> (u16, u16) {
        self.get_kernel_ring_state(queue_idx)
    }

    fn reset_all_rings(&self) {
        self.reset_all_rings();
    }

    fn restore_ring_state(
        &self,
        queue_idx: u16,
        size: u16,
        desc: u64,
        avail: u64,
        used: u64,
        avail_idx: u16,
        used_idx: u16,
        msix_addr: u64,
        msix_data: u32,
    ) {
        self.ring_set_state(
            queue_idx, size, desc, avail, used, avail_idx, used_idx, msix_addr,
            msix_data,
        );
    }

    fn pause_rings_for_export(&self) {
        self.pause_rings();
    }

    fn set_notify_addrs(&self, pio_port: u16, mmio_addr: u64) {
        self.set_notify_addrs(pio_port, mmio_addr);
    }

    fn start_poll_deferred(&self) {
        self.start_poll_deferred();
    }

    fn queue_addr_set(&self, queue_idx: u16, queue: &VirtQueue) {
        // When the guest programs a queue address, tell the kernel
        // where the ring structures live.
        let mut inner = self.inner.lock().expect("viona lock poisoned");
        // The halt destroys the link with this lock dropped, so a guest
        // write can land in the middle of it. A ring marked ready then
        // would let the next kick reach a link that is gone.
        if inner.halted {
            return;
        }
        Self::ring_init(&mut inner, queue_idx, queue);
    }

    fn reset(&self) {
        let mut inner = self.inner.lock().expect("viona lock poisoned");
        // Same reason as `set_features`: no ioctl may be issued under
        // this guard once the halt has let it go. The halt cleared
        // both fields below already, and nothing can set them again.
        if inner.halted {
            return;
        }
        inner.negotiated_features = 0;
        inner.ring_state = [RingState::Init; NET_NUM_QUEUES];

        // Reset both rings in the kernel, so the worker stops before
        // the guest programs the ring again.
        for ring in 0..NET_NUM_QUEUES as u16 {
            if let Err(e) = inner.link_ref().ring_reset(ring) {
                slog::warn!(inner.log(), "viona ring reset failed";
                    "ring" => ring, "error" => %e);
            }
        }
    }
}
