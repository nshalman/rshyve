// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! i440FX migration: the 0xCF8 latch, the PIRQ routing and the bridge
//! headers. The kernel holds the i8259 pair, the I/O APIC, the PIT, the
//! RTC and the PM timer, and migrates them itself.

use std::sync::atomic::Ordering;
use std::sync::Mutex;

use crate::lifecycle::{
    ChipsetMigrateState, DeviceMigrateState, DeviceStateError, Lifecycle,
};
use crate::pci::device::{replay_migrate_pci_state, DeviceState};

use super::I440FxChipset;

impl Lifecycle for I440FxChipset {
    fn type_name(&self) -> &'static str {
        "i440fx"
    }

    fn migrates_by_id(&self) -> bool {
        true
    }

    fn export_migrate_state(
        &self,
    ) -> Result<Option<DeviceMigrateState>, DeviceStateError> {
        let header = |state: &Mutex<DeviceState>| {
            state.lock().expect("bridge lock poisoned").migrate_state()
        };
        Ok(Some(DeviceMigrateState::Chipset(ChipsetMigrateState {
            cfg_addr: self.cfg_decoder.addr(),
            pir: *self.lpc_bridge.pir_regs.lock().expect("pir lock"),
            post_code: self.post_code.load(Ordering::Relaxed),
            hostbridge: header(&self.hostbridge.state),
            lpc_bridge: header(&self.lpc_bridge.state),
        })))
    }

    fn restore_migrate_state(
        &self,
        state: &DeviceMigrateState,
    ) -> Result<(), DeviceStateError> {
        let DeviceMigrateState::Chipset(mig) = state else {
            return Err(DeviceStateError::WrongKind {
                want: "chipset",
                got: state.kind(),
            });
        };
        self.cfg_decoder.set_addr(mig.cfg_addr);
        *self.lpc_bridge.pir_regs.lock().expect("pir lock") = mig.pir;
        self.post_code.store(mig.post_code, Ordering::Relaxed);
        // Neither bridge has a BAR, so only the command register and
        // the Interrupt Line come back.
        replay_migrate_pci_state(&*self.hostbridge, &[], &mig.hostbridge);
        replay_migrate_pci_state(&*self.lpc_bridge, &[], &mig.lpc_bridge);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use vmm_core::pio::PioBus;

    use super::*;

    fn chipset() -> (Arc<PioBus>, Arc<I440FxChipset>) {
        let pio = Arc::new(PioBus::new());
        let chipset = I440FxChipset::create(
            &pio,
            None,
            slog::Logger::root(slog::Discard, slog::o!()),
        );
        (pio, chipset)
    }

    const LPC_PIR: u32 = 0x8000_0800 | 0x60;
    const HB_CMD: u32 = 0x8000_0000 | 0x04;

    #[test]
    fn the_latch_routing_and_headers_survive_the_wire() {
        let (pio, src) = chipset();
        // The firmware routes PIRQ A..D and enables the host bridge.
        pio.handle_out(0xCF8, 4, LPC_PIR);
        for (i, irq) in [10u32, 11, 10, 11].iter().enumerate() {
            pio.handle_out(0xCFC + i as u16, 1, *irq);
        }
        pio.handle_out(0xCF8, 4, HB_CMD);
        pio.handle_out(0xCFC, 2, 0x0006);
        pio.handle_out(0x80, 1, 0x42);
        // The guest is paused between the address and the data access.
        pio.handle_out(0xCF8, 4, LPC_PIR);
        let state = src.export_migrate_state().expect("export").expect("x");
        let text = ron::ser::to_string(&state).expect("serialize");
        let state: DeviceMigrateState = ron::de::from_str(&text).expect("de");

        let (dst_pio, dst) = chipset();
        dst.restore_migrate_state(&state).expect("restore");
        assert_eq!(
            dst.export_migrate_state().expect("export").expect("x"),
            state,
        );

        // The latched address still names the PIRQ registers.
        let pir: Vec<u32> =
            (0..4).map(|i| dst_pio.handle_in(0xCFC + i, 1)).collect();
        assert_eq!(pir, [10, 11, 10, 11]);
        dst_pio.handle_out(0xCF8, 4, HB_CMD);
        assert_eq!(dst_pio.handle_in(0xCFC, 2), 0x0006);
        assert_eq!(dst.post_code(), 0x42);
    }
}
