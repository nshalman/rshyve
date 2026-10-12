// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;

use vmm_core::mem::PhysMap;
use vmm_core::mmio::MmioBus;
use vmm_devices::lifecycle::XhciMigrateTablet;
use vmm_devices::pci::bits::RegCmd;
use vmm_devices::pci::device::PciDevice;
use vmm_devices::{Lifecycle, PointerSink};

use super::super::bits::*;
use super::*;

const RAM_LEN: usize = 0x10_000;
const BAR0: u64 = 0xE000_0000;
const ERST: u64 = 0x1000;
const EVENT_RING: u64 = 0x2000;
const EVENT_TRBS: u32 = 16;
const CMD_RING: u64 = 0x3000;
const DCBAA: u64 = 0x4000;
const EP1_RING: u64 = 0x5000;
const REPORT_BUF: u64 = 0x6000;

fn controller(ram: &Arc<PhysMap>) -> Arc<XhciController> {
    XhciController::new(Arc::clone(ram), Arc::new(MmioBus::new()), None)
}

fn write_ram(ram: &PhysMap, gpa: u64, bytes: &[u8]) {
    ram.lookup(gpa, bytes.len())
        .expect("guest memory")
        .write_bytes(bytes)
        .expect("write guest memory");
}

fn read_ram(ram: &PhysMap, gpa: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    ram.lookup(gpa, len)
        .expect("guest memory")
        .read_bytes(&mut buf)
        .expect("read guest memory");
    buf
}

fn mmio_write(ctrl: &XhciController, off: usize, len: u8, val: u64) {
    ctrl.bus_mmio.handle_write(BAR0 + off as u64, len, val);
}

fn mmio_read(ctrl: &XhciController, off: usize) -> u64 {
    ctrl.bus_mmio.handle_read(BAR0 + off as u64, 4)
}

/// What a guest driver leaves behind: BAR0 decoding, the event ring
/// up, the controller running, and the tablet's interrupt endpoint
/// armed with one TRB.
fn guest_setup(ctrl: &XhciController, ram: &PhysMap) {
    ctrl.cfg_write(0x10, 4, BAR0 as u32);
    ctrl.cfg_write(0x3C, 1, 11);
    ctrl.cfg_write(0x04, 2, u32::from(RegCmd::MMIO_EN.bits()));

    let mut entry = [0u8; 16];
    entry[..8].copy_from_slice(&EVENT_RING.to_le_bytes());
    entry[8..12].copy_from_slice(&EVENT_TRBS.to_le_bytes());
    write_ram(ram, ERST, &entry);
    let ir0 = RTSOFF as usize + IR0_BASE;
    mmio_write(ctrl, ir0 + IR_ERSTSZ, 4, 1);
    mmio_write(ctrl, ir0 + IR_ERSTBA, 8, ERST);
    mmio_write(ctrl, ir0 + IR_ERDP, 8, EVENT_RING);
    mmio_write(ctrl, ir0 + IR_IMAN, 4, u64::from(IMAN_IE));
    mmio_write(ctrl, OP_BASE + OP_CRCR, 8, CMD_RING | CRCR_RCS);
    mmio_write(ctrl, OP_BASE + OP_DCBAAP, 8, DCBAA);
    mmio_write(ctrl, OP_BASE + OP_CONFIG, 4, 1);
    mmio_write(
        ctrl,
        OP_BASE + OP_USBCMD,
        4,
        u64::from(USBCMD_RS | USBCMD_INTE),
    );

    // A Normal TRB for one 6-byte report, cycle bit set, asking for a
    // Transfer Event on completion as a driver's interrupt-IN TRB does.
    let mut trb = [0u8; 16];
    trb[..8].copy_from_slice(&REPORT_BUF.to_le_bytes());
    trb[8..12].copy_from_slice(&6u32.to_le_bytes());
    trb[12..16].copy_from_slice(
        &((TRB_TYPE_NORMAL << TRB_TYPE_SHIFT) | TRB_IOC | 1).to_le_bytes(),
    );
    write_ram(ram, EP1_RING, &trb);
    let mut st = ctrl.state.lock().expect("state");
    st.slot_state = SlotState::Configured;
    st.ep1_in_ring = TransferRing {
        base: EP1_RING,
        dequeue: EP1_RING,
        ccs: true,
    };
    st.ep1_in_has_trbs = true;
}

fn ron_round_trip(state: &DeviceMigrateState) -> DeviceMigrateState {
    let text = ron::ser::to_string(state).expect("serialize");
    ron::de::from_str(&text).expect("deserialize")
}

fn enqueue(state: &DeviceMigrateState) -> u32 {
    let DeviceMigrateState::Xhci(mig) = state else {
        panic!("not xhci state");
    };
    mig.event_ring.enqueue
}

#[test]
fn the_state_round_trips_and_bar0_decodes_again() {
    let ram = Arc::new(PhysMap::new_anon(0, RAM_LEN).expect("guest memory"));
    let src = controller(&ram);
    guest_setup(&src, &ram);

    // A pointer move during the pause stays in the tablet: guest
    // memory is not written after the last RAM pass.
    src.pause();
    src.pointer_event(0b001, 0x1234, 0x0567);
    assert_eq!(read_ram(&ram, REPORT_BUF, 6), [0; 6]);
    let exported = src.export_state();

    // The RAM passes carry the guest's memory; the destination shares
    // it here.
    let dst = controller(&ram);
    assert_eq!(
        dst.bus_mmio.handle_read(BAR0, 4),
        0xFFFF_FFFF,
        "BAR0 dark at reset"
    );
    dst.restore_state(&ron_round_trip(&exported))
        .expect("restore");
    assert_eq!(dst.export_state(), exported);

    // BAR0 decodes again, and reads the source's registers.
    assert_eq!(dst.cfg_read(0x10, 4), BAR0 as u32);
    assert_eq!(dst.cfg_read(0x3C, 1), 11);
    assert_eq!(
        mmio_read(&dst, 0) & 0xFF,
        u64::from(CAPLENGTH),
        "capability registers"
    );
    assert_eq!(
        mmio_read(&dst, OP_BASE + OP_USBCMD),
        u64::from(USBCMD_RS | USBCMD_INTE)
    );
    assert_eq!(mmio_read(&dst, OP_BASE + OP_DCBAAP), DCBAA);
    assert_eq!(mmio_read(&dst, OP_BASE + OP_CONFIG), 1);

    // Nothing reaches guest memory until the resume, and then the
    // report the source held fills the TRB the guest left armed.
    assert_eq!(read_ram(&ram, REPORT_BUF, 6), [0; 6]);
    let at = enqueue(&exported);
    dst.resume();
    assert_eq!(
        read_ram(&ram, REPORT_BUF, 6),
        [1, 0x34, 0x12, 0x67, 0x05, 0]
    );
    let event = read_ram(&ram, EVENT_RING + u64::from(at) * 16, 16);
    let dword3 = u32::from_le_bytes(event[12..16].try_into().expect("4"));
    assert_eq!(
        (dword3 >> TRB_TYPE_SHIFT) & TRB_TYPE_MASK,
        TRB_TYPE_TRANSFER_EVENT
    );
    assert_eq!(&event[..8], &EP1_RING.to_le_bytes());
    assert_eq!(enqueue(&dst.export_state()), at + 1);
}

#[test]
fn a_paused_source_delivers_on_its_resume() {
    let ram = Arc::new(PhysMap::new_anon(0, RAM_LEN).expect("guest memory"));
    let src = controller(&ram);
    guest_setup(&src, &ram);
    src.pause();
    src.pointer_event(0, 0x10, 0x20);
    assert_eq!(read_ram(&ram, REPORT_BUF, 6), [0; 6]);

    src.resume();
    assert_eq!(read_ram(&ram, REPORT_BUF, 6), [0, 0x10, 0, 0x20, 0, 0]);
}

#[test]
fn a_hostile_state_is_refused() {
    let ram = Arc::new(PhysMap::new_anon(0, RAM_LEN).expect("guest memory"));
    let src = controller(&ram);
    guest_setup(&src, &ram);
    let DeviceMigrateState::Xhci(good) = src.export_state() else {
        panic!("not xhci state");
    };
    let dst = controller(&ram);
    let before = dst.export_state();

    let mut bad = Vec::new();
    bad.push(XhciMigrateState {
        slot_state: 4,
        ..good.clone()
    });
    bad.push(XhciMigrateState {
        erstsz: 2,
        ..good.clone()
    });
    bad.push(XhciMigrateState {
        config: u32::from(MAX_SLOTS) + 1,
        ..good.clone()
    });
    for event_ring in [
        XhciMigrateEventRing {
            size: EVENT_TRBS,
            enqueue: EVENT_TRBS,
            ..good.event_ring.clone()
        },
        XhciMigrateEventRing {
            size: MAX_EVENT_RING_SIZE + 1,
            enqueue: 0,
            ..good.event_ring.clone()
        },
        XhciMigrateEventRing {
            size: 0,
            enqueue: 1,
            ..good.event_ring.clone()
        },
    ] {
        bad.push(XhciMigrateState {
            event_ring,
            ..good.clone()
        });
    }
    for state in bad {
        assert!(dst.restore_state(&DeviceMigrateState::Xhci(state)).is_err());
    }
    assert!(dst
        .restore_state(&DeviceMigrateState::Uart(Default::default()))
        .is_err());
    assert_eq!(dst.export_state(), before, "nothing of a refusal applied");
}

#[test]
fn the_tablet_clamps_what_the_wire_says() {
    let ram = Arc::new(PhysMap::new_anon(0, RAM_LEN).expect("guest memory"));
    let dst = controller(&ram);
    let state = XhciMigrateState {
        tablet: XhciMigrateTablet {
            buttons: 0xFF,
            x: 0xFFFF,
            y: 0x8000,
            wheel_buttons: 0xFF,
            address: 0xFF,
            ..Default::default()
        },
        ..Default::default()
    };
    dst.restore_state(&DeviceMigrateState::Xhci(state))
        .expect("restore");
    let DeviceMigrateState::Xhci(back) = dst.export_state() else {
        panic!("not xhci state");
    };
    assert_eq!(
        (
            back.tablet.buttons,
            back.tablet.x,
            back.tablet.y,
            back.tablet.wheel_buttons,
            back.tablet.address
        ),
        (0x07, 0x7FFF, 0x7FFF, 0x18, 0x7F)
    );
}

#[test]
fn a_bar0_the_bus_refuses_fails_the_restore() {
    let ram = Arc::new(PhysMap::new_anon(0, RAM_LEN).expect("guest memory"));
    let src = controller(&ram);
    guest_setup(&src, &ram);
    let exported = src.export_state();

    let dst = controller(&ram);
    // Something else already holds the source's BAR0 range here.
    dst.bus_mmio
        .register(BAR0, 0x1000, Arc::new(|_, _| {}))
        .expect("squatter");
    let error = dst.restore_state(&exported).expect_err("dark BAR0");
    assert!(error.to_string().contains("BAR0"), "{error}");
}

/// The line is held up from an event until the driver clears EINT. The
/// kernel's interrupt state puts the level back on the destination, so
/// a restore with EINT set holds the line there too: otherwise the
/// driver's clear would lower a line this side never raised.
#[test]
fn a_pending_interrupt_holds_the_line_on_the_destination() {
    use vmm_devices::pci::IntrPin;
    let ram = Arc::new(PhysMap::new_anon(0, RAM_LEN).expect("guest memory"));
    let src = controller(&ram);
    guest_setup(&src, &ram);
    let DeviceMigrateState::Xhci(mut pending) = src.export_state() else {
        panic!("not xhci state");
    };
    pending.usbsts |= USBSTS_EINT;

    for (usbsts, raised) in [(pending.usbsts, true), (0, false)] {
        let pin = Arc::new(crate::xhci::tests::LevelPin::default());
        let dst = XhciController::new(
            Arc::clone(&ram),
            Arc::new(MmioBus::new()),
            Some(pin.clone() as Arc<dyn IntrPin>),
        );
        let state = XhciMigrateState {
            usbsts,
            ..pending.clone()
        };
        dst.restore_state(&DeviceMigrateState::Xhci(state))
            .expect("restore");
        assert_eq!(pin.is_asserted(), raised, "USBSTS {usbsts:#x}");
    }
}
