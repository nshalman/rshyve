// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Opening the link and pointing the kernel at it.

use std::os::fd::BorrowedFd;
use std::sync::{Arc, Mutex};

use viona_api::{LinkOps, PromiscMode};

use super::{RingState, VionaInner, VirtioViona, ETHERADDRL, NET_NUM_QUEUES};

impl VirtioViona {
    /// Create a new viona-backed virtio-net device.
    ///
    /// # Arguments
    ///
    /// - `vnic_name` - Name of the VNIC (e.g., "vnic0")
    /// - `vm` - The VMM instance the link serves
    ///
    /// # Errors
    ///
    /// Returns an error if the platform has no viona driver, if the
    /// VNIC link ID cannot be resolved, or if the viona device cannot
    /// be opened and associated with the VM.
    pub fn new(
        vnic_name: &str,
        vm: BorrowedFd<'_>,
    ) -> std::io::Result<Self> {
        let (link, mac_addr, dev_features) = open_link(vnic_name, vm)?;
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
        // A migration restore can land after the halt. The link is
        // gone, and the halt holds no device lock while it destroys
        // it, so an ioctl issued here could wait out an untimed
        // kernel call with the lock held.
        if inner.halted {
            return;
        }
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
        let inner = self.inner.lock().expect("viona lock");
        // The same guard the other link calls carry. This one reports
        // to its caller, so the refusal is an error and not silence:
        // a guest that was promised every frame must not be told it
        // has them.
        if inner.halted {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "the viona link is destroyed",
            ));
        }
        inner.link_ref().set_promisc(mode)
    }
}

/// Open the kernel link for `vnic_name` and read what it offers.
///
/// The features come back with the link because this is the one call
/// the device makes on a handle it does not hold yet.
fn open_link(
    vnic_name: &str,
    vm: BorrowedFd<'_>,
) -> std::io::Result<(Arc<dyn LinkOps>, [u8; ETHERADDRL], u32)> {
    let vnic = dladm::Handle::open()?.vnic(vnic_name)?;

    let viona_fd = viona_api::VionaFd::new(vnic.link_id, vm).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!(
                "failed to create viona instance for '{}' (link_id={}): {}",
                vnic_name, vnic.link_id, e
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
    Ok((Arc::new(viona_fd), vnic.mac_addr, dev_features as u32))
}
