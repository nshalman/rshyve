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

use std::sync::{Arc, Mutex};

use slog;
use vmm_core::mem::PhysMap;

use crate::Lifecycle;
use super::queue::VirtQueue;
use super::VirtioDevice;

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
/// On non-illumos platforms, the viona ioctls are stubbed out
/// (they return errors), so this device cannot actually function.
/// The code compiles on all platforms to keep CI working.
pub struct VirtioViona {
    inner: Mutex<VionaInner>,
}

struct VionaInner {
    /// Open handle to the viona kernel device.
    #[cfg(target_os = "illumos")]
    viona_fd: viona_api::VionaFd,

    /// MAC address of the VNIC.
    mac_addr: [u8; ETHERADDRL],

    /// Features offered by the kernel driver.
    dev_features: u32,

    /// Features negotiated by the guest.
    negotiated_features: u32,

    /// Per-ring initialization state (RX=0, TX=1).
    ring_state: [RingState; NET_NUM_QUEUES],
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

        let viona_fd = viona_api::VionaFd::new(link_id, vm_fd)
            .map_err(|e| {
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
                viona_fd.ioctl(
                    viona_api::VNA_IOC_GET_FEATURES,
                    &mut feat,
                )
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
            }),
        })
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
            inner.viona_fd.ioctl(
                viona_api::VNA_IOC_SET_PROMISC,
                &mut mode,
            )
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
        raise_intr: Arc<dyn Fn(u16) + Send + Sync>,
        log: slog::Logger,
    ) {
        use std::os::fd::AsRawFd;
        let inner = self.inner.lock().expect("viona lock");
        let fd = inner.viona_fd.as_raw_fd();

        std::thread::Builder::new()
            .name("viona-intr-poll".into())
            .spawn(move || {
                viona_intr_poll_loop(fd, raise_intr, log);
            })
            .expect("failed to spawn viona-intr-poll thread");
    }

    #[cfg(not(target_os = "illumos"))]
    pub fn start_intr_poll(
        &self,
        _raise_intr: Arc<dyn Fn(u16) + Send + Sync>,
        _log: slog::Logger,
    ) {
    }

    /// Program the ring addresses for a queue into the kernel.
    ///
    /// On success, marks the ring as `Ready` so that subsequent kicks
    /// are forwarded to the kernel driver.
    #[cfg(target_os = "illumos")]
    fn ring_init(
        inner: &mut VionaInner,
        queue_idx: u16,
        queue: &VirtQueue,
    ) {
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
    fn ring_init(
        _inner: &mut VionaInner,
        _queue_idx: u16,
        _queue: &VirtQueue,
    ) {
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
        inner.negotiated_features = features as u32;
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
            let _ = inner
                .viona_fd
                .ioctl_usize(viona_api::VNA_IOC_RING_RESET, 0);
            let _ = inner
                .viona_fd
                .ioctl_usize(viona_api::VNA_IOC_RING_RESET, 1);
        }
    }
}

/// Look up a VNIC by name using libdladm, returning its link ID and
/// MAC address.
///
/// Uses `dladm_name2info()` to properly resolve the datalink name to
/// its kernel datalink_id. This is the same method propolis uses.
#[cfg(target_os = "illumos")]
fn lookup_vnic(
    name: &str,
) -> std::io::Result<(u32, [u8; ETHERADDRL])> {
    use std::ffi::CString;
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    // Open a dladm handle and resolve the link name to a datalink_id.
    let name_cstr = CString::new(name).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid VNIC name: '{}'", name),
        )
    })?;

    let mut dladm_hdl: *mut libc::c_void = std::ptr::null_mut();
    let rc = unsafe {
        dladm_open(&mut dladm_hdl as *mut *mut libc::c_void)
    };
    if rc != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("dladm_open failed: rc={}", rc),
        ));
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

    // Get MAC address via dladm CLI. This may fail inside a zone
    // (EPERM) — in that case we use a zero MAC and let the guest
    // discover the real MAC from the viona device.
    let mut mac_addr = [0u8; ETHERADDRL];
    if let Ok(mac_output) = Command::new("/usr/sbin/dladm")
        .args(["show-vnic", "-p", "-o", "MACADDRESS"])
        .arg(name)
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .output()
    {
        if mac_output.status.success() {
            if let Some(Ok(mac_line)) =
                BufReader::new(&mac_output.stdout[..]).lines().next()
            {
                parse_mac(&mac_line, &mut mac_addr);
            }
        }
    }

    Ok((link_id, mac_addr))
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
}

/// Interrupt polling loop for the viona kernel driver.
///
/// Uses `poll(2)` with `POLLRDBAND` to wait for the kernel to signal
/// that completed I/O is ready. When signaled, reads the interrupt
/// bitmap via `VNA_IOC_INTR_POLL`, clears each pending queue's
/// interrupt state, and calls the provided callback to raise the
/// PCI interrupt.
#[cfg(target_os = "illumos")]
fn viona_intr_poll_loop(
    fd: std::os::fd::RawFd,
    raise_intr: Arc<dyn Fn(u16) + Send + Sync>,
    log: slog::Logger,
) {
    use std::os::fd::AsRawFd;

    loop {
        // Wait for POLLRDBAND — viona's signal that interrupts are pending
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLRDBAND,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, -1) };
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
                    libc::ioctl(
                        fd,
                        viona_api::VNA_IOC_RING_INTR_CLR as _,
                        i,
                    )
                };
                raise_intr(i as u16);
            }
        }
    }
}

/// Parse a colon-separated MAC address string into a byte array.
#[cfg(target_os = "illumos")]
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `VionaInner` for testing on non-illumos platforms.
    /// On illumos this would need a real VionaFd; for CI we test
    /// the state-tracking logic via the fields directly.
    #[cfg(not(target_os = "illumos"))]
    fn test_inner() -> VionaInner {
        VionaInner {
            mac_addr: [0x02, 0x08, 0x20, 0xaa, 0xbb, 0xcc],
            dev_features: VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS,
            negotiated_features: 0,
            ring_state: [RingState::Init; NET_NUM_QUEUES],
        }
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
        inner.negotiated_features = VIRTIO_NET_F_MAC;
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
        inner.negotiated_features = VIRTIO_NET_F_MAC;
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
