// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! i8042 migration: the controller, the keyboard and the mouse.
//!
//! No interrupt is raised on restore. The edge for a byte already in a
//! buffer was sent on the source, and the kernel i8259 and I/O APIC
//! state that the migration imports after this holds it.

use vmm_devices::lifecycle::{
    DeviceMigrateState, DeviceStateError, Ps2MigrateState,
};

use super::{
    CtrlCfg, CtrlOutPort, PS2Ctrl, PS2C_CMD_WRITE_AUX_IN,
    PS2C_CMD_WRITE_AUX_OUT, PS2C_CMD_WRITE_CTLR_OUT, PS2C_CMD_WRITE_CTRL_CFG,
    PS2C_CMD_WRITE_PRI_OUT, PS2C_CMD_WRITE_RAM_END, PS2C_CMD_WRITE_RAM_START,
    PS2C_RAM_LEN,
};

impl PS2Ctrl {
    pub(super) fn export_state(&self) -> DeviceMigrateState {
        let state = self.state.lock().unwrap();
        DeviceMigrateState::Ps2(Ps2MigrateState {
            resp: state.resp,
            cmd_prefix: state.cmd_prefix,
            ctrl_cfg: state.ctrl_cfg.bits(),
            ctrl_out_port: state.ctrl_out_port.bits(),
            ram: state.ram.to_vec(),
            kbd: state.pri_port.export_state(),
            mouse: state.aux_port.export_state(),
        })
    }

    /// The controller stays paused until its resume, so no remote
    /// console key lands before the kernel interrupt state does.
    pub(super) fn restore_state(
        &self,
        state: &DeviceMigrateState,
    ) -> Result<(), DeviceStateError> {
        let DeviceMigrateState::Ps2(mig) = state else {
            return Err(DeviceStateError::WrongKind {
                want: "ps2",
                got: state.kind(),
            });
        };
        let ram: [u8; PS2C_RAM_LEN] =
            mig.ram.as_slice().try_into().map_err(|_| {
                DeviceStateError::invalid(format!(
                    "ps2: {} RAM bytes, the controller has {PS2C_RAM_LEN}",
                    mig.ram.len()
                ))
            })?;
        if let Some(prefix) = mig.cmd_prefix {
            if !takes_data_byte(prefix) {
                return Err(DeviceStateError::invalid(format!(
                    "ps2: controller command {prefix:#x} takes no data byte"
                )));
            }
        }

        let mut st = self.state.lock().unwrap();
        // Both ports check before either changes, so a refusal leaves
        // the controller as it was.
        let mut kbd = super::PS2Kbd::new();
        kbd.import_state(&mig.kbd)?;
        let mut mouse = super::PS2Mouse::new();
        mouse.import_state(&mig.mouse)?;
        st.pri_port = kbd;
        st.aux_port = mouse;
        st.resp = mig.resp;
        st.cmd_prefix = mig.cmd_prefix;
        st.ctrl_cfg = CtrlCfg::from_bits_truncate(mig.ctrl_cfg);
        let mut out = CtrlOutPort::from_bits_truncate(mig.ctrl_out_port);
        out.remove(CtrlOutPort::DYN_FLAGS);
        st.ctrl_out_port = out;
        st.ram = ram;
        st.paused = true;
        Ok(())
    }
}

/// The controller commands whose second byte comes on the data port.
fn takes_data_byte(prefix: u8) -> bool {
    matches!(
        prefix,
        PS2C_CMD_WRITE_CTRL_CFG
            | PS2C_CMD_WRITE_CTLR_OUT
            | PS2C_CMD_WRITE_PRI_OUT
            | PS2C_CMD_WRITE_AUX_OUT
            | PS2C_CMD_WRITE_AUX_IN
            | PS2C_CMD_WRITE_RAM_START..=PS2C_CMD_WRITE_RAM_END
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use vmm_core::intr_pins::{IntrPin, NoOpPin};
    use vmm_core::pio::PioBus;
    use vmm_devices::Lifecycle;

    use super::*;
    use crate::ps2::{KeyEvent, PORT_PS2_CMD_STATUS, PORT_PS2_DATA};

    const XK_A: u32 = 0x61;

    fn attached() -> (PioBus, Arc<PS2Ctrl>) {
        let bus = PioBus::new();
        let ctrl = PS2Ctrl::create();
        let pin: Arc<dyn IntrPin> = Arc::new(NoOpPin);
        ctrl.attach(&bus, Arc::clone(&pin), pin, None);
        (bus, ctrl)
    }

    fn ron_round_trip(state: &DeviceMigrateState) -> DeviceMigrateState {
        let text = ron::ser::to_string(state).expect("serialize");
        ron::de::from_str(&text).expect("deserialize")
    }

    fn key(down: bool) -> KeyEvent {
        KeyEvent {
            keysym_raw: XK_A,
            is_pressed: down,
        }
    }

    /// What a Linux i8042 driver leaves behind: interrupts on, the
    /// translated set, and the keyboard scanning.
    fn guest_setup(bus: &PioBus) {
        bus.handle_out(PORT_PS2_CMD_STATUS, 1, PS2C_CMD_WRITE_CTRL_CFG.into());
        let cfg = CtrlCfg::PRI_INTR_EN
            | CtrlCfg::AUX_INTR_EN
            | CtrlCfg::SYS_FLAG
            | CtrlCfg::PRI_XLATE_EN;
        bus.handle_out(PORT_PS2_DATA, 1, cfg.bits().into());
        bus.handle_out(PORT_PS2_DATA, 1, 0xED); // keyboard LEDs
        let _ = bus.handle_in(PORT_PS2_DATA, 1);
        bus.handle_out(PORT_PS2_DATA, 1, 0b010);
        let _ = bus.handle_in(PORT_PS2_DATA, 1);
    }

    #[test]
    fn the_controller_and_both_ports_survive_the_wire() {
        let (bus, src) = attached();
        guest_setup(&bus);
        bus.handle_out(PORT_PS2_CMD_STATUS, 1, 0x65);
        bus.handle_out(PORT_PS2_DATA, 1, 0x42); // RAM byte 5
        src.key_event(key(true));
        let exported = src.export_state();

        let (dst_bus, dst) = attached();
        dst.restore_state(&ron_round_trip(&exported))
            .expect("restore");
        assert_eq!(dst.export_state(), exported);

        // The key pressed on the source is still there to read.
        assert_eq!(dst_bus.handle_in(PORT_PS2_DATA, 1), 0x1E);
        dst_bus.handle_out(PORT_PS2_CMD_STATUS, 1, 0x20);
        assert_eq!(
            dst_bus.handle_in(PORT_PS2_DATA, 1) & 0x43,
            u32::from(
                (CtrlCfg::PRI_INTR_EN
                    | CtrlCfg::AUX_INTR_EN
                    | CtrlCfg::PRI_XLATE_EN)
                    .bits()
            )
        );
        dst_bus.handle_out(PORT_PS2_CMD_STATUS, 1, 0x25);
        assert_eq!(dst_bus.handle_in(PORT_PS2_DATA, 1), 0x42);
    }

    #[test]
    fn a_restored_controller_takes_keys_only_after_its_resume() {
        let (bus, src) = attached();
        guest_setup(&bus);
        let (dst_bus, dst) = attached();
        dst.restore_state(&src.export_state()).expect("restore");

        dst.key_event(key(true));
        assert_eq!(dst_bus.handle_in(PORT_PS2_CMD_STATUS, 1) & 0x01, 0);

        dst.resume();
        dst.key_event(key(true));
        assert_eq!(dst_bus.handle_in(PORT_PS2_DATA, 1), 0x1E);
    }

    #[test]
    fn a_hostile_state_is_refused_and_nothing_changes() {
        let (bus, dst) = attached();
        guest_setup(&bus);
        let before = dst.export_state();
        let DeviceMigrateState::Ps2(good) = before.clone() else {
            panic!("not ps2 state");
        };
        let mut bad = Vec::new();
        bad.push(Ps2MigrateState {
            ram: vec![0; 3],
            ..good.clone()
        });
        bad.push(Ps2MigrateState {
            cmd_prefix: Some(0xAA),
            ..good.clone()
        });
        let mut kbd = good.clone();
        kbd.kbd.buf = vec![0; 64];
        bad.push(kbd);
        let mut kbd = good.clone();
        kbd.kbd.cur_cmd = Some(0xF4);
        bad.push(kbd);
        let mut kbd = good.clone();
        kbd.kbd.scan_code_set = 3;
        bad.push(kbd);
        let mut mouse = good.clone();
        mouse.mouse.cur_cmd = Some(0xFF);
        bad.push(mouse);
        for state in bad {
            assert!(dst
                .restore_state(&DeviceMigrateState::Ps2(state))
                .is_err());
        }
        assert_eq!(dst.export_state(), before);
        assert!(dst
            .restore_state(&DeviceMigrateState::AcpiGpe(Default::default()))
            .is_err());
    }
}
