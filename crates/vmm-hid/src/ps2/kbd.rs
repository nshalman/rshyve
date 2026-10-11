// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Portions derived from Propolis, Copyright Oxide Computer Company,
// used under MPL-2.0. Upstream keeps `PS2Kbd` in
// lib/propolis/src/hw/ps2/ctrl.rs.

use std::collections::VecDeque;

use vmm_devices::lifecycle::{DeviceStateError, Ps2KbdMigrateState};

use super::ctrl::{
    probe_keyboard_cmd, probe_keyboard_data, probe_keyboard_overflow,
    probe_unknown_keyboard_cmd, PS2ScanCodeSet,
};

const PS2K_CMD_SET_LEDS: u8 = 0xed;

const PS2K_CMD_SCAN_CODE: u8 = 0xf0;
const PS2K_CMD_TYPEMATIC: u8 = 0xf3;

const PS2K_CMD_ECHO: u8 = 0xee;
const PS2K_CMD_IDENT: u8 = 0xf2;
const PS2K_CMD_SCAN_EN: u8 = 0xf4;
const PS2K_CMD_SCAN_DIS: u8 = 0xf5;
const PS2K_CMD_SET_DEFAULT: u8 = 0xf6;
const PS2K_CMD_RESEND: u8 = 0xfe;
const PS2K_CMD_RESET: u8 = 0xff;

const PS2K_R_ACK: u8 = 0xfa;
const PS2K_R_ECHO: u8 = 0xee;
const PS2K_R_SELF_TEST_PASS: u8 = 0xaa;

const PS2K_TYPEMATIC_MASK: u8 = 0x7f;

pub(super) const PS2_KBD_BUFSZ: usize = 16;

// TODO: wire up remote console to enabled/led_status/typematic
#[allow(unused)]
pub(super) struct PS2Kbd {
    buf: VecDeque<u8>,
    cur_cmd: Option<u8>,
    enabled: bool,
    led_status: u8,
    typematic: u8,
    scan_code_set: PS2ScanCodeSet,
}
impl PS2Kbd {
    pub(super) fn new() -> Self {
        Self {
            buf: VecDeque::with_capacity(PS2_KBD_BUFSZ),
            cur_cmd: None,
            enabled: true,
            led_status: 0,
            typematic: 0,
            scan_code_set: PS2ScanCodeSet::Set1,
        }
    }
    pub(super) fn cmd_input(&mut self, v: u8) {
        probe_keyboard_cmd(v);
        if let Some(cmd) = self.cur_cmd {
            self.cur_cmd = None;
            match cmd {
                PS2K_CMD_SET_LEDS => {
                    // low three bits set scroll/num/caps lock
                    self.led_status = v & 0b111;
                    self.resp(PS2K_R_ACK);
                }
                PS2K_CMD_SCAN_CODE => {
                    match v {
                        0 => {
                            // get scan code set
                            self.resp(PS2K_R_ACK);
                            self.resp(self.scan_code_set.as_byte());
                        }
                        1 => {
                            self.resp(PS2K_R_ACK);
                            self.scan_code_set = PS2ScanCodeSet::Set1;
                        }
                        2 => {
                            self.resp(PS2K_R_ACK);
                            self.scan_code_set = PS2ScanCodeSet::Set2;
                        }
                        _ => {}
                    }
                }
                PS2K_CMD_TYPEMATIC => {
                    self.typematic = v & PS2K_TYPEMATIC_MASK;
                    self.resp(PS2K_R_ACK);
                }
                _ => {
                    debug_assert!(false, "bad multi-part ps2 cmd {}", cmd);
                }
            }
        } else {
            match v {
                PS2K_CMD_SET_LEDS | PS2K_CMD_SCAN_CODE | PS2K_CMD_TYPEMATIC => {
                    // multi-part command, wait for next byte
                    self.cur_cmd = Some(v);
                    self.resp(PS2K_R_ACK);
                }
                PS2K_CMD_ECHO => {
                    self.resp(PS2K_R_ECHO);
                }
                PS2K_CMD_IDENT => {
                    self.resp(PS2K_R_ACK);
                    // MF2 keyboard
                    self.resp(0xab);
                    self.resp(0x83);
                }
                PS2K_CMD_SCAN_EN => {
                    self.enabled = true;
                    self.resp(PS2K_R_ACK);
                }
                PS2K_CMD_SCAN_DIS => {
                    self.enabled = false;
                    self.resp(PS2K_R_ACK);
                }
                PS2K_CMD_SET_DEFAULT => {
                    // XXX which things to reset?
                    self.resp(PS2K_R_ACK);
                }
                PS2K_CMD_RESEND => {
                    // XXX we do not track last byte for now
                    self.resp(PS2K_R_ACK);
                }
                PS2K_CMD_RESET => {
                    self.reset();
                    // Even for reset, ack is expected
                    self.resp(PS2K_R_ACK);
                    self.resp(PS2K_R_SELF_TEST_PASS);
                }
                _ => {
                    // ignore unrecognized cmds
                    probe_unknown_keyboard_cmd(v);
                }
            }
        }
    }
    pub(super) fn resp(&mut self, v: u8) {
        let remain = PS2_KBD_BUFSZ - self.buf.len();
        match remain {
            0 => {
                // overrun already in progress, do nothing
                probe_keyboard_overflow(v);
            }
            1 => {
                // indicate overflow instead
                probe_keyboard_overflow(v);
                self.buf.push_back(0xff)
            }
            _ => {
                probe_keyboard_data(v);
                self.buf.push_back(v);
            }
        }
    }
    pub(super) fn reset(&mut self) {
        // XXX  what should the defaults be?
        self.cur_cmd = None;
        self.enabled = true;
        self.led_status = 0;
        self.typematic = 0;
        self.scan_code_set = PS2ScanCodeSet::Set1;
        self.buf.clear();
    }
    pub(super) fn has_output(&self) -> bool {
        !self.buf.is_empty()
    }
    pub(super) fn read_output(&mut self) -> Option<u8> {
        self.buf.pop_front()
    }
    pub(super) fn loopback(&mut self, v: u8) {
        self.resp(v);
    }

    pub(super) fn recv_scancode(&mut self, scan_code: Vec<u8>) {
        for s in scan_code.into_iter() {
            self.resp(s);
        }
    }
}
impl PS2Kbd {
    pub(super) fn export_state(&self) -> Ps2KbdMigrateState {
        Ps2KbdMigrateState {
            buf: self.buf.iter().copied().collect(),
            cur_cmd: self.cur_cmd,
            enabled: self.enabled,
            led_status: self.led_status,
            typematic: self.typematic,
            scan_code_set: self.scan_code_set.as_byte(),
        }
    }

    /// Checked whole before anything is applied, so a refused state
    /// leaves the keyboard as it was.
    pub(super) fn import_state(
        &mut self,
        state: &Ps2KbdMigrateState,
    ) -> Result<(), DeviceStateError> {
        if state.buf.len() > PS2_KBD_BUFSZ {
            return Err(DeviceStateError::invalid(format!(
                "ps2: {} keyboard bytes, the buffer holds {PS2_KBD_BUFSZ}",
                state.buf.len()
            )));
        }
        if let Some(cmd) = state.cur_cmd {
            if !matches!(
                cmd,
                PS2K_CMD_SET_LEDS | PS2K_CMD_SCAN_CODE | PS2K_CMD_TYPEMATIC
            ) {
                return Err(DeviceStateError::invalid(format!(
                    "ps2: keyboard command {cmd:#x} takes no second byte"
                )));
            }
        }
        let scan_code_set = match state.scan_code_set {
            1 => PS2ScanCodeSet::Set1,
            2 => PS2ScanCodeSet::Set2,
            other => {
                return Err(DeviceStateError::invalid(format!(
                    "ps2: scan code set {other}"
                )))
            }
        };
        self.buf = state.buf.iter().copied().collect();
        self.cur_cmd = state.cur_cmd;
        self.enabled = state.enabled;
        self.led_status = state.led_status & 0b111;
        self.typematic = state.typematic & PS2K_TYPEMATIC_MASK;
        self.scan_code_set = scan_code_set;
        Ok(())
    }
}

impl Default for PS2Kbd {
    fn default() -> Self {
        Self::new()
    }
}
