// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Opening the link and pointing the kernel at it.

use std::sync::{Arc, Mutex};

use viona_api::PromiscMode;

use super::link::LinkOps;
use super::{RingState, VionaInner, VirtioViona, ETHERADDRL, NET_NUM_QUEUES};

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
    /// Returns an error if the platform has no viona driver, if the
    /// VNIC link ID cannot be resolved, or if the viona device cannot
    /// be opened and associated with the VM.
    pub fn new(
        vnic_name: &str,
        vm_fd: std::os::fd::RawFd,
    ) -> std::io::Result<Self> {
        let (link, mac_addr, dev_features) = open_link(vnic_name, vm_fd)?;
        Ok(Self::with_link(link, mac_addr, dev_features))
    }

    /// A device around one link, before the guest has programmed it.
    pub(super) fn with_link(
        link: Arc<dyn LinkOps>,
        mac_addr: [u8; ETHERADDRL],
        dev_features: u32,
    ) -> Self {
        Self {
            inner: Mutex::new(VionaInner {
                link,
                mac_addr,
                dev_features,
                negotiated_features: 0,
                ring_state: [RingState::Init; NET_NUM_QUEUES],
                deferred_poll: None,
                poller: None,
                log: None,
                halted: false,
            }),
            interrupt: Arc::new(Mutex::new(None)),
        }
    }

    /// Program the notification I/O port and MMIO address in the kernel.
    ///
    /// The kernel then takes the guest's ring kick itself, and the vCPU
    /// does not leave to userspace for it.
    pub fn set_notify_addrs(&self, pio_port: u16, mmio_addr: u64) {
        let inner = self.inner.lock().expect("viona lock");
        if pio_port != 0 {
            if let Err(e) = inner.link_ref().set_notify_iop(pio_port) {
                slog::warn!(inner.log(), "viona set notify port failed";
                    "port" => pio_port, "error" => %e);
            }
        }
        if mmio_addr != 0 {
            let rc = inner.link_ref().set_notify_mmio(mmio_addr, 0x1000);
            if let Err(e) = rc {
                slog::warn!(inner.log(), "viona set notify mmio failed";
                    "addr" => mmio_addr, "error" => %e);
            }
        }
    }

    /// Enable promiscuous mode on the viona device.
    ///
    /// When `promiscphys` is true, the VNIC receives all physical
    /// traffic (MAC spoofing). This is required for VMs with
    /// `allow_mac_spoofing=true` in the zone config.
    pub fn set_promisc(&self, enable: bool) -> std::io::Result<()> {
        let mode = if enable {
            PromiscMode::All
        } else {
            PromiscMode::None
        };
        self.inner
            .lock()
            .expect("viona lock")
            .link_ref()
            .set_promisc(mode)
    }
}

/// Open the kernel link for `vnic_name` and read what it offers.
///
/// The features come back with the link because this is the one call
/// the device makes on a handle it does not hold yet.
#[cfg(target_os = "illumos")]
fn open_link(
    vnic_name: &str,
    vm_fd: std::os::fd::RawFd,
) -> std::io::Result<(Arc<dyn LinkOps>, [u8; ETHERADDRL], u32)> {
    let (link_id, mac_addr) = lookup_vnic(vnic_name)?;

    let viona_fd = viona_api::VionaFd::new(link_id, vm_fd).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!(
                "failed to create viona instance for '{}' (link_id={}): {}",
                vnic_name, link_id, e
            ),
        )
    })?;

    let dev_features = viona_fd.get_features().map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("VNA_IOC_GET_FEATURES failed: {}", e),
        )
    })?;

    // The legacy transport carries only the low 32 bits.
    Ok((Arc::new(viona_fd), mac_addr, dev_features as u32))
}

#[cfg(not(target_os = "illumos"))]
fn open_link(
    _vnic_name: &str,
    _vm_fd: std::os::fd::RawFd,
) -> std::io::Result<(Arc<dyn LinkOps>, [u8; ETHERADDRL], u32)> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "viona is only available on illumos",
    ))
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
