// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! xHCI migration: the PCI header, the register file, the cursors into
//! the guest's rings, and the tablet.
//!
//! The model writes no output device context, and every ring is walked
//! inline on the thread that rang the doorbell, so there is no transfer
//! in flight to drain. The one asynchronous writer is a pointer event
//! from the remote console, which a pause holds back.

use vmm_devices::lifecycle::{
    DeviceMigrateState, DeviceStateError, XhciMigrateEventRing,
    XhciMigrateRing, XhciMigrateState,
};
use vmm_devices::pci::bits::RegCmd;
use vmm_devices::pci::device::replay_migrate_pci_state;

use super::bits::{
    CRCR_CRR, ERDP_PTR_MASK, ERSTBA_PTR_MASK, IMAN_IE, IMAN_IP, MAX_SLOTS,
};
use super::{
    SlotState, TransferRing, XhciController, XhciState, MAX_EVENT_RING_SIZE,
};

impl XhciController {
    pub(super) fn export_state(&self) -> DeviceMigrateState {
        let pci = self
            .pci_state
            .lock()
            .expect("xhci: pci_state lock")
            .migrate_state();
        let st = self.state.lock().expect("xhci: state lock");
        DeviceMigrateState::Xhci(XhciMigrateState {
            pci,
            usbcmd: st.usbcmd,
            usbsts: st.usbsts,
            dnctrl: st.dnctrl,
            crcr: st.crcr,
            dcbaap: st.dcbaap,
            config: st.config,
            portsc: [st.port1_sc, st.port2_sc],
            iman: st.iman,
            imod: st.imod,
            erstsz: st.erstsz,
            erstba: st.erstba,
            erdp: st.erdp,
            event_ring: XhciMigrateEventRing {
                base: st.event_ring_base,
                size: st.event_ring_size,
                enqueue: st.event_ring_enqueue,
                pcs: st.event_ring_pcs,
            },
            cmd_ring: XhciMigrateRing {
                base: st.cmd_ring_base,
                dequeue: st.cmd_ring_dequeue,
                ccs: st.cmd_ring_ccs,
            },
            slot_state: slot_state_wire(st.slot_state),
            ep0: ring_wire(&st.ep0_ring),
            ep1_in: ring_wire(&st.ep1_in_ring),
            ep1_in_armed: st.ep1_in_has_trbs,
            tablet: self.tablet.export_state(),
        })
    }

    /// Called on a paused destination. The controller stays paused, so
    /// no pointer report reaches guest memory before its resume.
    pub(super) fn restore_state(
        &self,
        state: &DeviceMigrateState,
    ) -> Result<(), DeviceStateError> {
        let DeviceMigrateState::Xhci(mig) = state else {
            return Err(DeviceStateError::WrongKind {
                want: "xhci",
                got: state.kind(),
            });
        };
        let slot_state = slot_state_from_wire(mig.slot_state)?;
        check_event_ring(&mig.event_ring)?;
        if mig.erstsz > 1 {
            return Err(DeviceStateError::invalid(format!(
                "xhci: ERSTSZ {} past the one segment ERST Max allows",
                mig.erstsz
            )));
        }
        if mig.config > u32::from(MAX_SLOTS) {
            return Err(DeviceStateError::invalid(format!(
                "xhci: CONFIG {} past {MAX_SLOTS} slots",
                mig.config
            )));
        }

        {
            let mut st = self.state.lock().expect("xhci: state lock");
            *st = XhciState {
                usbcmd: mig.usbcmd,
                usbsts: mig.usbsts,
                dnctrl: mig.dnctrl,
                crcr: mig.crcr & !CRCR_CRR,
                dcbaap: mig.dcbaap,
                config: mig.config,
                port1_sc: mig.portsc[0],
                port2_sc: mig.portsc[1],
                iman: mig.iman & (IMAN_IP | IMAN_IE),
                imod: mig.imod,
                erstsz: mig.erstsz,
                erstba: mig.erstba & ERSTBA_PTR_MASK,
                erdp: mig.erdp & ERDP_PTR_MASK,
                event_ring_base: mig.event_ring.base,
                event_ring_size: mig.event_ring.size,
                event_ring_enqueue: mig.event_ring.enqueue,
                event_ring_pcs: mig.event_ring.pcs,
                cmd_ring_base: mig.cmd_ring.base,
                cmd_ring_dequeue: mig.cmd_ring.dequeue,
                cmd_ring_ccs: mig.cmd_ring.ccs,
                slot_state,
                ep0_ring: ring_from_wire(&mig.ep0),
                ep1_in_ring: ring_from_wire(&mig.ep1_in),
                ep1_in_has_trbs: mig.ep1_in_armed,
                paused: true,
            };
        }
        self.tablet.import_state(&mig.tablet);

        // Last, so BAR0 decodes only once the registers behind it hold
        // the source's values.
        let bar_writes = self
            .pci_state
            .lock()
            .expect("xhci: pci_state lock")
            .bar_replay_writes(&mig.pci.bar_addrs);
        replay_migrate_pci_state(self, &bar_writes, &mig.pci);

        // A BAR the bus refused leaves the controller dark, which is the
        // dead mouse this restore exists to prevent.
        let decodes = mig.pci.command & RegCmd::MMIO_EN.bits() != 0
            && mig.pci.bar_addrs[0] != 0;
        let registered = *self.registered_bar0.lock().expect("xhci: bar0");
        if decodes && registered.is_none() {
            return Err(DeviceStateError::invalid(format!(
                "xhci: BAR0 at {:#x} does not decode",
                mig.pci.bar_addrs[0]
            )));
        }
        Ok(())
    }
}

fn check_event_ring(
    ring: &XhciMigrateEventRing,
) -> Result<(), DeviceStateError> {
    let fits = if ring.size == 0 {
        ring.enqueue == 0
    } else {
        ring.size <= MAX_EVENT_RING_SIZE && ring.enqueue < ring.size
    };
    if fits {
        Ok(())
    } else {
        Err(DeviceStateError::invalid(format!(
            "xhci: event ring enqueue {} of {} TRBs",
            ring.enqueue, ring.size
        )))
    }
}

fn slot_state_wire(state: SlotState) -> u8 {
    match state {
        SlotState::Disabled => 0,
        SlotState::Enabled => 1,
        SlotState::Addressed => 2,
        SlotState::Configured => 3,
    }
}

fn slot_state_from_wire(state: u8) -> Result<SlotState, DeviceStateError> {
    Ok(match state {
        0 => SlotState::Disabled,
        1 => SlotState::Enabled,
        2 => SlotState::Addressed,
        3 => SlotState::Configured,
        other => {
            return Err(DeviceStateError::invalid(format!(
                "xhci: slot state {other}"
            )))
        }
    })
}

fn ring_wire(ring: &TransferRing) -> XhciMigrateRing {
    XhciMigrateRing {
        base: ring.base,
        dequeue: ring.dequeue,
        ccs: ring.ccs,
    }
}

fn ring_from_wire(ring: &XhciMigrateRing) -> TransferRing {
    TransferRing {
        base: ring.base,
        dequeue: ring.dequeue,
        ccs: ring.ccs,
    }
}

#[cfg(test)]
mod tests;
