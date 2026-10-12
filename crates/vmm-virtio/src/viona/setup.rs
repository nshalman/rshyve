// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Opening the link and pointing the kernel at it.

use std::os::fd::BorrowedFd;
use std::sync::{Arc, Mutex};

use viona_api::{LinkOps, PromiscMode};
use vmm_core::rings::{KernelRings, RingGate};

use super::gate::GateMember;
use super::{RingState, VionaInner, VirtioViona, ETHERADDRL, NET_NUM_QUEUES};
use crate::pci::intr::IntrSlot;

impl VirtioViona {
    /// Create a new viona-backed virtio-net device.
    ///
    /// # Arguments
    ///
    /// - `vnic_name` - Name of the VNIC (e.g., "vnic0")
    /// - `vm` - The VMM instance the link serves
    /// - `rings` - That VM's ring gate
    ///
    /// # Errors
    ///
    /// Returns an error if the platform has no viona driver, if the
    /// VNIC link ID cannot be resolved, or if the viona device cannot
    /// be opened and associated with the VM.
    pub fn new(
        vnic_name: &str,
        vm: BorrowedFd<'_>,
        rings: Arc<RingGate>,
    ) -> std::io::Result<Self> {
        let (link, mac_addr, dev_features) = open_link(vnic_name, vm)?;
        Self::with_link(link, mac_addr, dev_features, rings)
    }

    /// A device around one link, before the guest has programmed it,
    /// with its rings in the VM's gate.
    pub(super) fn with_link(
        link: Arc<dyn LinkOps>,
        mac_addr: [u8; ETHERADDRL],
        dev_features: u32,
        rings: Arc<RingGate>,
    ) -> std::io::Result<Self> {
        let inner = Arc::new(Mutex::new(VionaInner {
            link,
            mac_addr,
            dev_features,
            negotiated_features: 0,
            ring_state: [RingState::Init; NET_NUM_QUEUES],
            kick_refused: [false; NET_NUM_QUEUES],
            stopped: None,
            msi: [None; NET_NUM_QUEUES],
            deferred_poll: None,
            poller: None,
            log: None,
            hook: Default::default(),
            halted: false,
            deleted: false,
        }));
        let member: Arc<dyn KernelRings> =
            Arc::new(GateMember(Arc::downgrade(&inner)));
        rings.register(Arc::downgrade(&member))?;
        Ok(Self {
            inner,
            rings,
            _member: member,
            interrupt: Arc::new(IntrSlot::new()),
        })
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
