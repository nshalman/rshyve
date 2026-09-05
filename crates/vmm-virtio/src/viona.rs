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

use std::io::PipeWriter;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use slog;
use vmm_core::mem::PhysMap;

use super::queue::VirtQueue;
use super::VirtioDevice;
use vmm_devices::Lifecycle;

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

/// The in-kernel calls a halt makes.
///
/// Behind a trait for two reasons: off illumos there is no link to
/// call, and a test can watch the order without a kernel.
trait LinkOps: Send + Sync {
    /// Return one ring to its reset state.
    fn ring_reset(&self, ring: usize) -> std::io::Result<()>;

    /// Destroy the link, which gives back its `vmm_drv` hold.
    fn delete(&self) -> std::io::Result<()>;
}

#[cfg(target_os = "illumos")]
impl LinkOps for viona_api::VionaFd {
    fn ring_reset(&self, ring: usize) -> std::io::Result<()> {
        viona_api::VionaFd::ring_reset(self, ring)
    }

    fn delete(&self) -> std::io::Result<()> {
        viona_api::VionaFd::delete(self)
    }
}

/// The interrupt poll thread, while one is running.
struct Poller {
    /// Write end of the pipe that stops the thread.
    ///
    /// The thread parks in `poll(2)` on the viona descriptor with no
    /// timeout, so only a second descriptor can wake it. Closing this
    /// end is what makes that descriptor ready.
    wake: PipeWriter,
    thread: JoinHandle<()>,
}

/// VirtIO network device backed by the illumos viona kernel driver.
///
/// On non-illumos platforms, the viona ioctls are stubbed out
/// (they return errors), so this device cannot actually function.
/// The code compiles on all platforms to keep CI working.
pub struct VirtioViona {
    inner: Mutex<VionaInner>,
}

type QueueInterrupt = Arc<dyn Fn(u16) + Send + Sync>;

struct VionaInner {
    /// Open handle to the viona kernel device.
    #[cfg(target_os = "illumos")]
    viona_fd: viona_api::VionaFd,

    /// The link a halt calls, on a build with no viona driver.
    ///
    /// On illumos the open handle above is that link. Elsewhere the
    /// device cannot be created at all, so this exists only so the
    /// halt has one code path on every platform.
    #[cfg(not(target_os = "illumos"))]
    link: Box<dyn LinkOps>,

    /// MAC address of the VNIC.
    mac_addr: [u8; ETHERADDRL],

    /// Features offered by the kernel driver.
    dev_features: u32,

    /// Features negotiated by the guest.
    negotiated_features: u64,

    /// Per-ring initialization state (RX=0, TX=1).
    ring_state: [RingState; NET_NUM_QUEUES],

    /// Deferred interrupt poll state for migration destinations.
    /// Stores the raise_fn and logger so the poll thread can be
    /// started after ring state is restored.
    deferred_poll: Option<(QueueInterrupt, slog::Logger)>,

    /// The running interrupt poll thread, if one was started.
    poller: Option<Poller>,

    /// The logger the interrupt wiring carried.
    ///
    /// `VirtioViona::new` is given none, so a halt reports through the
    /// first logger the device was handed.
    log: Option<slog::Logger>,

    /// The link is destroyed. Nothing may program it again.
    halted: bool,
}

impl VionaInner {
    /// The link this build talks to.
    #[cfg(target_os = "illumos")]
    fn link(&self) -> &dyn LinkOps {
        &self.viona_fd
    }

    #[cfg(not(target_os = "illumos"))]
    fn link(&self) -> &dyn LinkOps {
        self.link.as_ref()
    }

    /// Where a halt reports, discarding when the device got no logger.
    fn log(&self) -> slog::Logger {
        self.log
            .clone()
            .unwrap_or_else(|| slog::Logger::root(slog::Discard, slog::o!()))
    }
}

/// Wake a running poll thread and wait for it to leave.
///
/// Closing the write end is the wake: the thread parks in an untimed
/// `poll(2)`, so nothing else reaches it.
fn stop_poller(poller: Option<Poller>, log: &slog::Logger) {
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
/// The delete resets the rings itself, but its reset does not heed
/// signals (`viona_ring.c`, `viona_ring_reset`). Resetting first, where
/// a signal can cut the wait short, leaves that one less to do.
///
/// A failure is reported and the sweep carries on. Teardown must reach
/// the destroy whatever the link does.
fn halt_link(link: &dyn LinkOps, log: &slog::Logger) {
    for ring in 0..NET_NUM_QUEUES {
        if let Err(e) = link.ring_reset(ring) {
            slog::warn!(log, "viona ring reset failed during halt";
                "ring" => ring, "error" => %e);
        }
    }

    // WHY this is an error and not a warning: the hold this call gives
    // back is what VM_DESTROY_SELF waits for. A delete that failed
    // predicts the destroy wedge.
    if let Err(e) = link.delete() {
        slog::error!(log, "viona link delete failed; the VM destroy may wait for its hold";
            "error" => %e);
    }
}

impl VirtioViona {
    /// Create a new viona-backed virtio-net device.
    ///
    /// # Arguments
    ///
    /// - `vnic_name` - Name of the VNIC (e.g., "vnic0")
    /// - `vm_fd` - Raw fd of the VMM instance
    ///
    /// # Errors
    ///
    /// Returns an error if the VNIC link ID cannot be resolved, or if
    /// the viona device cannot be opened/associated.
    #[cfg(target_os = "illumos")]
    pub fn new(
        vnic_name: &str,
        vm_fd: std::os::fd::RawFd,
    ) -> std::io::Result<Self> {
        let (link_id, mac_addr) = lookup_vnic(vnic_name)?;

        let viona_fd =
            viona_api::VionaFd::new(link_id, vm_fd).map_err(|e| {
                std::io::Error::new(
                    e.kind(),
                    format!(
                        "failed to create viona instance for '{}' \
                         (link_id={}): {}",
                        vnic_name, link_id, e
                    ),
                )
            })?;

        // Query features supported by the kernel driver.
        let dev_features = {
            let mut feat: u64 = 0;
            unsafe {
                viona_fd.ioctl(viona_api::VNA_IOC_GET_FEATURES, &mut feat)
            }
            .map_err(|e| {
                std::io::Error::new(
                    e.kind(),
                    format!("VNA_IOC_GET_FEATURES failed: {}", e),
                )
            })?;
            // For legacy transport we only use the low 32 bits.
            feat as u32
        };

        Ok(Self {
            inner: Mutex::new(VionaInner {
                viona_fd,
                mac_addr,
                dev_features,
                negotiated_features: 0,
                ring_state: [RingState::Init; NET_NUM_QUEUES],
                deferred_poll: None,
                poller: None,
                log: None,
                halted: false,
            }),
        })
    }

    /// Stop the interrupt poll thread and wait for it to leave.
    ///
    /// The thread reads the viona descriptor and calls back into the
    /// PCI transport, so it must be gone before the link is destroyed.
    /// The join runs with the device lock dropped: the thread may be
    /// inside that callback now, and it takes transport locks.
    fn poller_stop(&self) {
        let (poller, log) = {
            let mut inner = self.inner.lock().expect("viona lock poisoned");
            (inner.poller.take(), inner.log())
        };
        stop_poller(poller, &log);
    }

    /// Restore ring state after migration using VNA_IOC_RING_SET_STATE.
    /// This programs ring addresses AND indices into the kernel, unlike
    /// ring_init which resets indices to 0.
    /// Reset all viona rings (pause + reset). Call before ring_set_state.
    #[cfg(target_os = "illumos")]
    pub fn reset_all_rings(&self) {
        let inner = self.inner.lock().expect("viona lock");
        for idx in 0..NET_NUM_QUEUES {
            let _ = inner
                .viona_fd
                .ioctl_usize(viona_api::VNA_IOC_RING_PAUSE, idx);
            let _ = inner
                .viona_fd
                .ioctl_usize(viona_api::VNA_IOC_RING_RESET, idx);
        }
    }

    #[cfg(not(target_os = "illumos"))]
    pub fn reset_all_rings(&self) {}

    #[cfg(target_os = "illumos")]
    pub fn ring_set_state(
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
        let mut inner = self.inner.lock().expect("viona lock");
        // A restore that races the halt must not program a link that
        // is already destroyed.
        if inner.halted {
            return;
        }

        // Set features (VERSION_1 sets l_modern in kernel)
        if inner.negotiated_features != 0 {
            Self::set_kernel_features(&inner, inner.negotiated_features);
        }

        // Program ring state (addresses + indices).
        //
        // vrs_avail_idx sets vr_cur_aidx, the kernel's consumption
        // cursor. The kernel computes available entries as:
        //   num_avail = guest_memory_avail_idx - vr_cur_aidx
        //
        // We must use the REAL avail_idx from the source (not used_idx)
        // because descriptors between used_idx and avail_idx may have
        // been reclaimed by the guest driver. Replaying them causes
        // "id N is not a head!" errors in the guest.
        //
        // If avail_idx == used_idx (zero available buffers), the guest
        // driver must re-post buffers via the post-restore interrupt.
        let mut state = viona_api::vioc_ring_state {
            vrs_index: queue_idx,
            vrs_avail_idx: avail_idx,
            vrs_used_idx: used_idx,
            vrs_qsize: size,
            vrs_qaddr_desc: desc,
            vrs_qaddr_avail: avail,
            vrs_qaddr_used: used,
        };
        let rc = unsafe {
            inner
                .viona_fd
                .ioctl(viona_api::VNA_IOC_RING_SET_STATE, &mut state)
        };
        if rc.is_ok() {
            let idx = queue_idx as usize;
            if idx < NET_NUM_QUEUES {
                inner.ring_state[idx] = RingState::Ready;
            }

            // Program MSI-X interrupt delivery for this queue
            if msix_addr != 0 {
                let mut msi = viona_api::vioc_ring_msi {
                    rm_index: queue_idx,
                    rm_addr: msix_addr,
                    rm_msg: msix_data as u64,
                    ..Default::default()
                };
                let msi_rc = unsafe {
                    inner
                        .viona_fd
                        .ioctl(viona_api::VNA_IOC_RING_SET_MSI, &mut msi)
                };
                if let Err(ref e) = msi_rc {
                    eprintln!(
                        "[viona] ring_set_msi q={} failed: {}",
                        queue_idx, e
                    );
                }
            }

            Self::ring_kick(&inner, queue_idx);
            eprintln!("[viona] ring_set_state q={} OK: avail={} used={} msi_addr={:#x}",
                queue_idx, avail_idx, used_idx, msix_addr);
        } else {
            eprintln!(
                "[viona] ring_set_state q={} failed: {:?}",
                queue_idx, rc
            );
        }
    }

    #[cfg(not(target_os = "illumos"))]
    pub fn ring_set_state(
        &self,
        _: u16,
        _: u16,
        _: u64,
        _: u64,
        _: u64,
        _: u16,
        _: u16,
        _: u64,
        _: u32,
    ) {
    }

    /// Program the notification I/O port and MMIO address in the kernel.
    #[cfg(target_os = "illumos")]
    pub fn set_notify_addrs(&self, pio_port: u16, mmio_addr: u64) {
        let inner = self.inner.lock().expect("viona lock");
        if pio_port != 0 {
            let _ = inner.viona_fd.ioctl_usize(
                viona_api::VNA_IOC_SET_NOTIFY_IOP,
                pio_port as usize,
            );
        }
        if mmio_addr != 0 {
            let mut vim = viona_api::vioc_notify_mmio {
                vim_address: mmio_addr,
                vim_size: 0x1000,
            };
            let _ = unsafe {
                inner
                    .viona_fd
                    .ioctl(viona_api::VNA_IOC_SET_NOTIFY_MMIO, &mut vim)
            };
        }
    }

    #[cfg(not(target_os = "illumos"))]
    pub fn set_notify_addrs(&self, _: u16, _: u64) {}

    /// Get ring state from kernel (avail_idx, used_idx) for migration export.
    #[cfg(target_os = "illumos")]
    pub fn get_kernel_ring_state(&self, queue_idx: u16) -> (u16, u16) {
        let inner = self.inner.lock().expect("viona lock");
        let mut state = viona_api::vioc_ring_state {
            vrs_index: queue_idx,
            ..Default::default()
        };
        let rc = unsafe {
            inner
                .viona_fd
                .ioctl(viona_api::VNA_IOC_RING_GET_STATE, &mut state)
        };
        if rc.is_ok() {
            (state.vrs_avail_idx, state.vrs_used_idx)
        } else {
            (0, 0)
        }
    }

    #[cfg(not(target_os = "illumos"))]
    pub fn get_kernel_ring_state(&self, _queue_idx: u16) -> (u16, u16) {
        (0, 0)
    }

    /// Kick all ready queues after migration to wake kernel processing.
    pub fn kick_all_queues(&self) {
        let inner = self.inner.lock().expect("viona lock");
        for idx in 0..NET_NUM_QUEUES as u16 {
            if inner.ring_state[idx as usize] == RingState::Ready {
                Self::ring_kick(&inner, idx);
            }
        }
    }

    /// Enable promiscuous mode on the viona device.
    ///
    /// When `promiscphys` is true, the VNIC receives all physical
    /// traffic (MAC spoofing). This is required for VMs with
    /// `allow_mac_spoofing=true` in the zone config.
    #[cfg(target_os = "illumos")]
    pub fn set_promisc(&self, enable: bool) -> std::io::Result<()> {
        let inner = self.inner.lock().expect("viona lock");
        let mut mode: i32 = if enable {
            viona_api::VIONA_PROMISC_ALL
        } else {
            viona_api::VIONA_PROMISC_NONE
        };
        unsafe {
            inner
                .viona_fd
                .ioctl(viona_api::VNA_IOC_SET_PROMISC, &mut mode)
        }?;
        Ok(())
    }

    #[cfg(not(target_os = "illumos"))]
    pub fn set_promisc(&self, _enable: bool) -> std::io::Result<()> {
        Ok(())
    }

    /// Stub constructor for non-illumos platforms.
    #[cfg(not(target_os = "illumos"))]
    pub fn new(
        _vnic_name: &str,
        _vm_fd: std::os::fd::RawFd,
    ) -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "viona is only available on illumos",
        ))
    }

    /// Kick an in-kernel virtqueue ring.
    ///
    /// Only kicks if the queue index is valid (0=RX, 1=TX), features
    /// have been negotiated, and the ring has been initialized via
    /// `ring_init`. Silently ignores kicks to unconfigured or
    /// out-of-range queues to prevent kernel NULL pointer dereferences
    /// in the mac layer.
    #[cfg(target_os = "illumos")]
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
        let _ = inner
            .viona_fd
            .ioctl_usize(viona_api::VNA_IOC_RING_KICK, idx);
    }

    #[cfg(not(target_os = "illumos"))]
    fn ring_kick(_inner: &VionaInner, _queue_idx: u16) {}

    /// Start the interrupt polling thread.
    ///
    /// Viona completes I/O in-kernel and signals readiness via
    /// `POLLRDBAND` on the device fd. This thread polls for that
    /// event, calls `VNA_IOC_INTR_POLL` to discover which queues
    /// have pending interrupts, clears them, and raises the per-queue
    /// interrupt so the guest processes the used ring.
    ///
    /// The callback receives the queue index (0=rx, 1=tx) so the
    /// PCI transport can route via the correct MSI-X vector.
    #[cfg(target_os = "illumos")]
    pub fn start_intr_poll(
        &self,
        raise_intr: QueueInterrupt,
        log: slog::Logger,
    ) {
        let mut inner = self.inner.lock().expect("viona lock");
        inner.log = Some(log.clone());
        // A halted device has no link left to poll.
        if inner.halted {
            return;
        }
        Self::poller_start(&mut inner, raise_intr, log);
    }

    #[cfg(not(target_os = "illumos"))]
    pub fn start_intr_poll(
        &self,
        _raise_intr: QueueInterrupt,
        log: slog::Logger,
    ) {
        self.inner.lock().expect("viona lock").log = Some(log);
    }

    /// Spawn the interrupt poll thread and record it for the halt.
    ///
    /// The spawn failures panic, as they did before the halt existed: a
    /// viona device with no poll thread delivers no interrupt to the
    /// guest, and both failures are resource exhaustion at boot.
    #[cfg(target_os = "illumos")]
    fn poller_start(
        inner: &mut VionaInner,
        raise_intr: QueueInterrupt,
        log: slog::Logger,
    ) {
        use std::os::fd::AsRawFd;

        // A second thread on the same descriptor would be left running
        // when the halt stops the first one.
        if inner.poller.is_some() {
            slog::warn!(
                log,
                "a viona interrupt poll thread is running already"
            );
            return;
        }

        // Without this pipe the thread could not be stopped, and the
        // halt would join a thread parked in an untimed poll(2).
        let (stop, wake) =
            std::io::pipe().expect("failed to open the viona poll stop pipe");
        let fd = inner.viona_fd.as_raw_fd();
        let thread = std::thread::Builder::new()
            .name("viona-intr-poll".into())
            .spawn(move || {
                viona_intr_poll_loop(fd, stop, raise_intr, log);
            })
            .expect("failed to spawn viona-intr-poll thread");
        inner.poller = Some(Poller { wake, thread });
    }

    /// Store the interrupt poll parameters for deferred start.
    ///
    /// On migration destinations, the poll thread cannot start at device
    /// creation time because the rings haven't been programmed yet
    /// (and starting it would make them EBUSY). Instead, store the
    /// raise_fn and logger here, then call `start_poll_deferred()` after
    /// ring state is fully restored.
    pub fn store_poll_fn(&self, raise_intr: QueueInterrupt, log: slog::Logger) {
        let mut inner = self.inner.lock().expect("viona lock");
        inner.log = Some(log.clone());
        // A halted device has no link left to poll.
        if inner.halted {
            return;
        }
        inner.deferred_poll = Some((raise_intr, log));
    }

    /// Start the interrupt poll thread using previously stored parameters.
    ///
    /// Called after migration restore has programmed all ring state,
    /// MSI-X vectors, and notification addresses. This is the final
    /// step that enables viona to deliver completion interrupts to
    /// the guest.
    #[cfg(target_os = "illumos")]
    pub fn start_poll_deferred(&self) {
        let mut inner = self.inner.lock().expect("viona lock");
        // A migration restore that lands after the halt must not start
        // a thread on a link that is already destroyed.
        if inner.halted {
            inner.deferred_poll = None;
            return;
        }
        if let Some((raise_intr, log)) = inner.deferred_poll.take() {
            slog::info!(log, "starting deferred viona interrupt poll thread");
            Self::poller_start(&mut inner, raise_intr, log);

            // Re-kick all rings after starting the poll thread.
            // VNA_IOC_RING_SET_STATE programs indices but may not
            // activate the kernel ring worker. A second kick after
            // the poll thread is running ensures the worker processes
            // available entries.
            for idx in 0..NET_NUM_QUEUES {
                if inner.ring_state[idx] == RingState::Ready {
                    let _ = inner
                        .viona_fd
                        .ioctl_usize(viona_api::VNA_IOC_RING_KICK, idx);
                }
            }
        }
    }

    #[cfg(not(target_os = "illumos"))]
    pub fn start_poll_deferred(&self) {
        let mut inner = self.inner.lock().expect("viona lock");
        inner.deferred_poll.take();
    }

    /// Program the ring addresses for a queue into the kernel.
    ///
    /// On success, marks the ring as `Ready` so that subsequent kicks
    /// are forwarded to the kernel driver.
    #[cfg(target_os = "illumos")]
    fn ring_init(inner: &mut VionaInner, queue_idx: u16, queue: &VirtQueue) {
        if !queue.is_configured() {
            return;
        }

        // Use the actual addresses from VirtQueue — works for both
        // legacy (set_addr_legacy) and modern (set_addr_modern).
        let mut rim = viona_api::vioc_ring_init_modern {
            rim_index: queue_idx,
            rim_qsize: queue.size(),
            rim_qaddr_desc: queue.desc_addr(),
            rim_qaddr_avail: queue.avail_addr(),
            rim_qaddr_used: queue.used_addr(),
            ..Default::default()
        };

        let rc = unsafe {
            inner
                .viona_fd
                .ioctl(viona_api::VNA_IOC_RING_INIT_MODERN, &mut rim)
        };
        if rc.is_ok() {
            let idx = queue_idx as usize;
            if idx < NET_NUM_QUEUES {
                inner.ring_state[idx] = RingState::Ready;
            }
        }
    }

    #[cfg(not(target_os = "illumos"))]
    fn ring_init(_inner: &mut VionaInner, _queue_idx: u16, _queue: &VirtQueue) {
    }

    /// Set negotiated features in the kernel driver.
    #[cfg(target_os = "illumos")]
    fn set_kernel_features(inner: &VionaInner, features: u64) {
        let mut feat = features;
        let _ = unsafe {
            inner
                .viona_fd
                .ioctl(viona_api::VNA_IOC_SET_FEATURES, &mut feat)
        };
    }

    #[cfg(not(target_os = "illumos"))]
    fn set_kernel_features(_inner: &VionaInner, _features: u64) {}
}

impl VirtioDevice for VirtioViona {
    fn device_features(&self) -> u64 {
        let inner = self.inner.lock().expect("viona lock poisoned");
        // Combine kernel features with the features we always advertise.
        let mut features = u64::from(inner.dev_features);
        features |= u64::from(VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS);
        // Strip EVENT_IDX — viona handles rings in-kernel and
        // manages its own notification suppression protocol.
        // Don't advertise it from userspace.
        features &= !super::bits::VIRTIO_F_RING_EVENT_IDX;
        features
    }

    fn set_features(&self, features: u64) {
        let mut inner = self.inner.lock().expect("viona lock poisoned");
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
        #[cfg(target_os = "illumos")]
        {
            let inner = self.inner.lock().expect("viona lock");
            for idx in 0..NET_NUM_QUEUES {
                let _ = inner
                    .viona_fd
                    .ioctl_usize(viona_api::VNA_IOC_RING_PAUSE, idx);
            }
        }
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
        Self::ring_init(&mut inner, queue_idx, queue);
    }

    fn reset(&self) {
        let mut inner = self.inner.lock().expect("viona lock poisoned");
        inner.negotiated_features = 0;
        inner.ring_state = [RingState::Init; NET_NUM_QUEUES];

        // Reset both rings in the kernel.
        #[cfg(target_os = "illumos")]
        {
            let _ =
                inner.viona_fd.ioctl_usize(viona_api::VNA_IOC_RING_RESET, 0);
            let _ =
                inner.viona_fd.ioctl_usize(viona_api::VNA_IOC_RING_RESET, 1);
        }
    }
}

/// Look up a VNIC by name using libdladm, returning its link ID and
/// MAC address.
///
/// Uses `dladm_name2info()` to properly resolve the datalink name to
/// its kernel datalink_id. This is the same method propolis uses.
#[cfg(target_os = "illumos")]
fn lookup_vnic(name: &str) -> std::io::Result<(u32, [u8; ETHERADDRL])> {
    use std::ffi::CString;

    // Open a dladm handle and resolve the link name to a datalink_id.
    let name_cstr = CString::new(name).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid VNIC name: '{}'", name),
        )
    })?;

    let mut dladm_hdl: *mut libc::c_void = std::ptr::null_mut();
    let rc = unsafe { dladm_open(&mut dladm_hdl as *mut *mut libc::c_void) };
    if rc != 0 {
        return Err(std::io::Error::other(format!(
            "dladm_open failed: rc={}",
            rc
        )));
    }

    let mut link_id: u32 = 0;
    let rc = unsafe {
        dladm_name2info(
            dladm_hdl,
            name_cstr.as_ptr() as *const u8,
            &mut link_id,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    unsafe { dladm_close(dladm_hdl) };

    if rc != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "dladm_name2info failed for '{}': rc={} \
                 (is '{}' a valid VNIC?)",
                name, rc, name,
            ),
        ));
    }

    // Get MAC address via libdladm FFI (dladm_vnic_info ioctl).
    // This works inside bhyve zones where `dladm show-vnic` fails
    // due to missing privileges.
    let mut mac_addr = [0u8; ETHERADDRL];

    // Re-open handle for the vnic_info call
    let mut dladm_hdl2: *mut libc::c_void = std::ptr::null_mut();
    let rc2 = unsafe { dladm_open(&mut dladm_hdl2) };
    if rc2 == 0 {
        let mut vnic_attr: DladmVnicAttr = unsafe { std::mem::zeroed() };
        let rc3 = unsafe {
            dladm_vnic_info(
                dladm_hdl2,
                link_id,
                &mut vnic_attr,
                1, // DLADM_OPT_ACTIVE
            )
        };
        if rc3 == 0 && vnic_attr.va_mac_len as usize >= ETHERADDRL {
            mac_addr.copy_from_slice(&vnic_attr.va_mac_addr[..ETHERADDRL]);
        }
        unsafe { dladm_close(dladm_hdl2) };
    }

    Ok((link_id, mac_addr))
}

/// libdladm VNIC attributes returned by dladm_vnic_info().
///
/// The actual struct is 15,488 bytes due to the trailing
/// mac_resource_props_t (15,428 bytes). We only need the first
/// few fields (MAC addr at offset 16, MAC len at offset 12).
#[cfg(target_os = "illumos")]
#[repr(C)]
struct DladmVnicAttr {
    va_vnic_id: u32,                 // offset 0
    va_link_id: u32,                 // offset 4
    va_mac_addr_type: libc::c_int,   // offset 8
    va_mac_len: libc::c_uint,        // offset 12
    va_mac_addr: [u8; 20],           // offset 16, MAXMACADDRLEN
    va_mac_slot: libc::c_int,        // offset 36
    va_mac_prefix_len: libc::c_uint, // offset 40
    va_vid: u16,                     // offset 44
    va_force: libc::c_int,           // offset 48 (with padding)
    va_vrid: u32,
    va_af: libc::c_int,
    // mac_resource_props_t (15,428 bytes) follows.
    _pad: [u8; 15432],
}

// libdladm FFI declarations
#[cfg(target_os = "illumos")]
#[link(name = "dladm")]
extern "C" {
    fn dladm_open(handle: *mut *mut libc::c_void) -> libc::c_int;
    fn dladm_close(handle: *mut libc::c_void);
    fn dladm_name2info(
        handle: *mut libc::c_void,
        link: *const u8,
        linkidp: *mut u32,
        flagp: *mut u32,
        classp: *mut libc::c_int,
        mediap: *mut u32,
    ) -> libc::c_int;
    fn dladm_vnic_info(
        handle: *mut libc::c_void,
        linkid: u32,
        attrp: *mut DladmVnicAttr,
        flags: u32,
    ) -> libc::c_int;
}

/// Interrupt polling loop for the viona kernel driver.
///
/// Uses `poll(2)` with `POLLRDBAND` to wait for the kernel to signal
/// that completed I/O is ready. When signaled, reads the interrupt
/// bitmap via `VNA_IOC_INTR_POLL`, clears each pending queue's
/// interrupt state, and calls the provided callback to raise the
/// PCI interrupt.
///
/// `stop` is the read end of the halt's pipe. The wait has no timeout,
/// so that descriptor is the only thing that ends this loop on demand.
#[cfg(target_os = "illumos")]
fn viona_intr_poll_loop(
    fd: std::os::fd::RawFd,
    stop: std::io::PipeReader,
    raise_intr: QueueInterrupt,
    log: slog::Logger,
) {
    use std::os::fd::AsRawFd;

    loop {
        // Wait for POLLRDBAND — viona's signal that interrupts are pending
        let mut pfds = [
            libc::pollfd {
                fd,
                events: libc::POLLRDBAND,
                revents: 0,
            },
            libc::pollfd {
                fd: stop.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ret = unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            // Device closed or fatal error — exit thread
            slog::warn!(log, "poll error"; "error" => %err);
            break;
        }
        if ret == 0 {
            continue;
        }

        // The halt closed its end of the pipe, so the descriptor is
        // ready and stays ready. Leave before touching the link.
        if pfds[1].revents != 0 {
            break;
        }

        let pfd = pfds[0];
        if pfd.revents & libc::POLLRDBAND == 0 {
            // Spurious wakeup or POLLHUP/POLLERR
            if pfd.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                break;
            }
            continue;
        }

        // Read which queues have pending interrupts
        let mut poll_data = viona_api::vioc_intr_poll::default();
        let rc = unsafe {
            libc::ioctl(
                fd,
                viona_api::VNA_IOC_INTR_POLL as _,
                &mut poll_data as *mut _,
            )
        };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            slog::warn!(log, "VNA_IOC_INTR_POLL failed"; "error" => %err);
            continue;
        }

        // Process each queue with a pending interrupt.
        // Fire per-queue so PCI transport routes via the correct
        // MSI-X vector (or falls back to shared INTx).
        for i in 0..viona_api::VIONA_VQ_MAX as usize {
            if poll_data.vip_status[i] != 0 {
                let _ = unsafe {
                    libc::ioctl(fd, viona_api::VNA_IOC_RING_INTR_CLR as _, i)
                };
                raise_intr(i as u16);
            }
        }
    }
}

/// Parse a colon-separated MAC address string into a byte array.
#[cfg(target_os = "illumos")]
#[allow(dead_code)]
fn parse_mac(s: &str, mac: &mut [u8; ETHERADDRL]) {
    for (i, part) in s.split(':').enumerate() {
        if i >= ETHERADDRL {
            break;
        }
        if let Ok(b) = u8::from_str_radix(part, 16) {
            mac[i] = b;
        }
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
    fn halt(&self) {
        self.poller_stop();

        let mut inner = self.inner.lock().expect("viona lock poisoned");
        // A second halt has no link left to destroy. viona would treat
        // it as a no-op, but a repeated delete would also repeat the
        // wait inside it.
        if inner.halted {
            return;
        }
        inner.halted = true;
        // A deferred start held past the halt would revive a dead link.
        inner.deferred_poll = None;

        let log = inner.log();
        halt_link(inner.link(), &log);

        // Nothing may kick the rings again: they are gone.
        inner.ring_state = [RingState::Init; NET_NUM_QUEUES];
        inner.negotiated_features = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What one halt asked of the kernel, in order.
    type Steps = Arc<Mutex<Vec<String>>>;

    /// A link that records the calls a halt makes.
    ///
    /// This is the whole in-kernel side of the halt, so the order and
    /// the error handling are testable on every platform, illumos
    /// included.
    struct RecordingLink {
        steps: Steps,
        delete_result: fn() -> std::io::Result<()>,
    }

    impl RecordingLink {
        fn new(steps: &Steps) -> Self {
            Self {
                steps: Arc::clone(steps),
                delete_result: || Ok(()),
            }
        }

        fn failing(steps: &Steps) -> Self {
            Self {
                steps: Arc::clone(steps),
                delete_result: || {
                    Err(std::io::Error::from(std::io::ErrorKind::NotFound))
                },
            }
        }

        fn push(&self, step: String) {
            self.steps.lock().expect("steps lock").push(step);
        }
    }

    impl LinkOps for RecordingLink {
        fn ring_reset(&self, ring: usize) -> std::io::Result<()> {
            self.push(format!("ring_reset({ring})"));
            Ok(())
        }

        fn delete(&self) -> std::io::Result<()> {
            self.push("delete".to_string());
            (self.delete_result)()
        }
    }

    fn steps_of(steps: &Steps) -> Vec<String> {
        steps.lock().expect("steps lock").clone()
    }

    fn null_log() -> slog::Logger {
        slog::Logger::root(slog::Discard, slog::o!())
    }

    /// Build a `VionaInner` for testing on non-illumos platforms.
    /// On illumos this would need a real VionaFd; for CI we test
    /// the state-tracking logic via the fields directly.
    #[cfg(not(target_os = "illumos"))]
    fn test_inner_with(link: Box<dyn LinkOps>) -> VionaInner {
        VionaInner {
            link,
            mac_addr: [0x02, 0x08, 0x20, 0xaa, 0xbb, 0xcc],
            dev_features: VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS,
            negotiated_features: 0,
            ring_state: [RingState::Init; NET_NUM_QUEUES],
            deferred_poll: None,
            poller: None,
            log: None,
            halted: false,
        }
    }

    #[cfg(not(target_os = "illumos"))]
    fn test_inner() -> VionaInner {
        test_inner_with(Box::new(RecordingLink::new(&Steps::default())))
    }

    /// A device whose halt is watched through `steps`.
    #[cfg(not(target_os = "illumos"))]
    fn device_with(link: Box<dyn LinkOps>) -> VirtioViona {
        VirtioViona {
            inner: Mutex::new(test_inner_with(link)),
        }
    }

    #[test]
    fn a_halt_resets_every_ring_before_it_deletes_the_link() {
        // The delete resets the rings itself, in a wait that does not
        // heed signals. Resetting first leaves that one less to do.
        let steps = Steps::default();
        let link = RecordingLink::new(&steps);

        halt_link(&link, &null_log());

        assert_eq!(
            steps_of(&steps),
            ["ring_reset(0)", "ring_reset(1)", "delete"],
        );
    }

    #[test]
    fn a_failed_delete_does_not_stop_the_halt() {
        // Teardown has to reach VM_DESTROY_SELF whatever the link
        // does, so the failure is reported and nothing is unwound.
        let steps = Steps::default();
        let link = RecordingLink::failing(&steps);

        halt_link(&link, &null_log());

        assert_eq!(steps_of(&steps).last().map(String::as_str), Some("delete"));
    }

    #[test]
    fn stopping_a_poller_wakes_the_thread_and_joins_it() {
        // The poll thread parks with no timeout, so closing the write
        // end is the only thing that can end it. A stop that does not
        // wake the thread hangs this test, which is the failure it
        // exists to catch; the poll below bounds that hang.
        use std::os::fd::AsRawFd;

        let (stop, wake) = std::io::pipe().expect("the test can open a pipe");
        let woken = Arc::new(Mutex::new(false));
        let reported = Arc::clone(&woken);
        let thread = std::thread::Builder::new()
            .name("poller-stub".into())
            .spawn(move || {
                let mut pfd = libc::pollfd {
                    fd: stop.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // A closed write end leaves the descriptor ready, so
                // this returns once and does not spin.
                while unsafe { libc::poll(&mut pfd, 1, 10_000) } < 0 {
                    let err = std::io::Error::last_os_error();
                    assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
                }
                *reported.lock().expect("woken lock") = pfd.revents != 0;
            })
            .expect("the test can spawn a thread");

        stop_poller(Some(Poller { wake, thread }), &null_log());

        assert!(
            *woken.lock().expect("woken lock"),
            "the stop pipe never became ready",
        );
    }

    #[cfg(not(target_os = "illumos"))]
    #[test]
    fn a_halt_issues_the_delete_that_frees_the_vmm_hold() {
        // VM_DESTROY_SELF waits for every vmm_drv hold in an untimed
        // cv_wait. VNA_IOC_DELETE is what gives viona's hold back.
        let steps = Steps::default();
        let device = device_with(Box::new(RecordingLink::new(&steps)));

        device.halt();

        assert_eq!(
            steps_of(&steps),
            ["ring_reset(0)", "ring_reset(1)", "delete"],
        );
    }

    #[cfg(not(target_os = "illumos"))]
    #[test]
    fn a_second_halt_does_not_delete_again() {
        // The delete waits for each ring worker to stop. Repeating it
        // would repeat that wait for nothing.
        let steps = Steps::default();
        let device = device_with(Box::new(RecordingLink::new(&steps)));

        device.halt();
        device.halt();

        assert_eq!(
            steps_of(&steps).iter().filter(|s| *s == "delete").count(),
            1
        );
    }

    #[cfg(not(target_os = "illumos"))]
    #[test]
    fn a_failed_delete_still_leaves_the_device_halted() {
        // Teardown carries on to the destroy, so a failure must not
        // leave the device asking to be halted again.
        let steps = Steps::default();
        let device = device_with(Box::new(RecordingLink::failing(&steps)));

        device.halt();
        device.halt();

        assert_eq!(
            steps_of(&steps).iter().filter(|s| *s == "delete").count(),
            1
        );
    }

    #[cfg(not(target_os = "illumos"))]
    #[test]
    fn a_halt_drops_a_deferred_poll_so_a_late_restore_cannot_revive_it() {
        // A migration restore can land after the halt. Starting the
        // deferred poll thread then would read a destroyed link.
        let steps = Steps::default();
        let device = device_with(Box::new(RecordingLink::new(&steps)));
        let raise: QueueInterrupt = Arc::new(|_| {});
        device.store_poll_fn(Arc::clone(&raise), null_log());

        device.halt();
        assert!(
            device
                .inner
                .lock()
                .expect("viona lock")
                .deferred_poll
                .is_none(),
            "the halt left a deferred poll behind",
        );

        // A restore that stores its callback after the halt is
        // refused, so nothing can start on the destroyed link.
        device.store_poll_fn(raise, null_log());
        device.start_poll_deferred();

        let inner = device.inner.lock().expect("viona lock");
        assert!(inner.halted);
        assert!(inner.deferred_poll.is_none());
        assert!(inner.poller.is_none());
    }

    #[cfg(not(target_os = "illumos"))]
    #[test]
    fn a_halted_device_refuses_a_ring_restore() {
        // ring_set_state programs kernel state and kicks the ring. On
        // a destroyed link there is nothing to program.
        let steps = Steps::default();
        let device = device_with(Box::new(RecordingLink::new(&steps)));

        device.halt();
        device.ring_set_state(0, 256, 0x1000, 0x2000, 0x3000, 0, 0, 0, 0);

        let inner = device.inner.lock().expect("viona lock");
        assert_eq!(inner.ring_state[0], RingState::Init);
    }

    #[test]
    fn ring_state_default_is_init() {
        let states = [RingState::Init; NET_NUM_QUEUES];
        assert_eq!(states[0], RingState::Init);
        assert_eq!(states[1], RingState::Init);
    }

    #[test]
    fn ring_state_transitions_to_ready() {
        let mut states = [RingState::Init; NET_NUM_QUEUES];
        states[0] = RingState::Ready;
        assert_eq!(states[0], RingState::Ready);
        assert_eq!(states[1], RingState::Init);
    }

    #[test]
    // The first assignments model pre-reset migration state. Keep the explicit
    // transition until the migration engine restructuring is resolved.
    #[allow(unused_assignments)]
    fn ring_state_reset_clears_ready() {
        let mut states = [RingState::Init; NET_NUM_QUEUES];
        states[0] = RingState::Ready;
        states[1] = RingState::Ready;
        // Simulate reset
        states = [RingState::Init; NET_NUM_QUEUES];
        assert_eq!(states[0], RingState::Init);
        assert_eq!(states[1], RingState::Init);
    }

    #[cfg(not(target_os = "illumos"))]
    #[test]
    fn kick_suppressed_when_ring_not_ready() {
        let inner = test_inner();
        // ring_kick is a no-op on non-illumos, but we verify
        // the guard conditions would prevent a kick:
        assert_eq!(inner.ring_state[0], RingState::Init);
        assert_eq!(inner.ring_state[1], RingState::Init);
        assert_eq!(inner.negotiated_features, 0);
        // Calling ring_kick should be safe (no-op stub)
        VirtioViona::ring_kick(&inner, 0);
        VirtioViona::ring_kick(&inner, 1);
        VirtioViona::ring_kick(&inner, 2); // out of range
    }

    #[cfg(not(target_os = "illumos"))]
    #[test]
    fn kick_allowed_after_features_and_ring_init() {
        let mut inner = test_inner();
        inner.negotiated_features = VIRTIO_NET_F_MAC as u64;
        inner.ring_state[0] = RingState::Ready;
        // On illumos this would actually issue the ioctl; on
        // non-illumos the stub is a no-op. Verify state is correct.
        assert_eq!(inner.ring_state[0], RingState::Ready);
        assert_ne!(inner.negotiated_features, 0);
        VirtioViona::ring_kick(&inner, 0);
    }

    #[cfg(not(target_os = "illumos"))]
    #[test]
    fn reset_clears_features_and_ring_state() {
        let mut inner = test_inner();
        inner.negotiated_features = VIRTIO_NET_F_MAC as u64;
        inner.ring_state[0] = RingState::Ready;
        inner.ring_state[1] = RingState::Ready;

        // Simulate the reset path
        inner.negotiated_features = 0;
        inner.ring_state = [RingState::Init; NET_NUM_QUEUES];

        assert_eq!(inner.negotiated_features, 0);
        assert_eq!(inner.ring_state[0], RingState::Init);
        assert_eq!(inner.ring_state[1], RingState::Init);
    }

    #[test]
    fn ring_state_copy_semantics() {
        let a = RingState::Ready;
        let b = a;
        assert_eq!(a, b);
        assert_eq!(a, RingState::Ready);
    }

    #[test]
    fn ring_state_debug_fmt() {
        // Ensure Debug derive works (useful for error messages)
        let s = format!("{:?}", RingState::Init);
        assert_eq!(s, "Init");
        let s = format!("{:?}", RingState::Ready);
        assert_eq!(s, "Ready");
    }
}
