// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The UART's migration state.

use crate::lifecycle::{DeviceStateError, UartMigrateState};

use super::{
    Fifo, IntrEnaReg, LineCtrlReg, LineStatusReg, ModemCtrlReg, Uart, FIFO_LEN,
};

impl Uart {
    pub fn export_state(&self) -> UartMigrateState {
        UartMigrateState {
            ier: self.reg_intr_enable.bits(),
            lcr: self.reg_line_ctrl.bits(),
            mcr: self.reg_modem_ctrl.bits(),
            lsr: self.reg_line_status.bits(),
            msr: self.reg_modem_status,
            scr: self.reg_scratch,
            dll: self.reg_div_low,
            dlh: self.reg_div_high,
            thre_pending: self.thre_intr,
            rx: self.rx_fifo.buf.iter().copied().collect(),
            tx: self.tx_fifo.buf.iter().copied().collect(),
        }
    }

    /// Take a source's state. Nothing changes unless all of it fits.
    pub fn import_state(
        &mut self,
        state: &UartMigrateState,
    ) -> Result<(), DeviceStateError> {
        for (name, fifo) in [("RX", &state.rx), ("TX", &state.tx)] {
            if fifo.len() > FIFO_LEN {
                return Err(DeviceStateError::invalid(format!(
                    "uart: {} {name} bytes, the FIFO holds {FIFO_LEN}",
                    fifo.len()
                )));
            }
        }
        self.reg_intr_enable = IntrEnaReg::from_bits_truncate(state.ier);
        self.reg_line_ctrl = LineCtrlReg::from_bits_retain(state.lcr);
        self.reg_modem_ctrl = ModemCtrlReg::from_bits_truncate(state.mcr);
        self.reg_line_status = LineStatusReg::from_bits_truncate(state.lsr);
        self.reg_modem_status = state.msr;
        self.reg_scratch = state.scr;
        self.reg_div_low = state.dll;
        self.reg_div_high = state.dlh;
        self.thre_intr = state.thre_pending;
        self.rx_fifo = Fifo::filled(&state.rx);
        self.tx_fifo = Fifo::filled(&state.tx);
        self.update_dr();
        self.update_isr();
        Ok(())
    }
}

impl Fifo {
    fn filled(bytes: &[u8]) -> Self {
        let mut fifo = Self::new(FIFO_LEN);
        fifo.buf.extend(bytes);
        fifo
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REG_IER: u8 = 1;
    const REG_IIR: u8 = 2;
    const REG_LCR: u8 = 3;
    const REG_LSR: u8 = 5;
    const REG_SCR: u8 = 7;
    const IER_ERBFI: u8 = 1 << 0;
    const IER_ETBEI: u8 = 1 << 1;
    const LCR_DLAB: u8 = 1 << 7;
    const LCR_8N1: u8 = 0b11;

    fn ron_round_trip(state: &UartMigrateState) -> UartMigrateState {
        let text = ron::ser::to_string(state).expect("serialize");
        ron::de::from_str(&text).expect("deserialize")
    }

    /// The registers a Linux 8250 driver sets up at 115200 8N1.
    fn programmed() -> Uart {
        let mut uart = Uart::new();
        uart.reg_write(REG_LCR, LCR_DLAB);
        uart.reg_write(0, 0x01);
        uart.reg_write(1, 0x00);
        uart.reg_write(REG_LCR, LCR_8N1);
        uart.reg_write(REG_SCR, 0x5A);
        uart.reg_write(REG_IER, IER_ERBFI | IER_ETBEI);
        uart
    }

    #[test]
    fn every_register_and_both_fifos_survive_the_wire() {
        let mut src = programmed();
        for b in b"mdata" {
            assert!(src.data_write(*b));
        }
        let exported = src.export_state();

        let mut dst = Uart::new();
        dst.import_state(&ron_round_trip(&exported))
            .expect("import");

        assert_eq!(dst.export_state(), exported);
        assert_eq!(dst.reg_read(REG_LCR), LCR_8N1);
        assert_eq!(dst.reg_read(REG_IER), IER_ERBFI | IER_ETBEI);
        assert_eq!(dst.reg_read(REG_SCR), 0x5A);
        dst.reg_write(REG_LCR, LCR_DLAB | LCR_8N1);
        assert_eq!((dst.reg_read(0), dst.reg_read(1)), (0x01, 0x00));
        dst.reg_write(REG_LCR, LCR_8N1);
        // Data Ready outranks THRE, and both the line and IIR say so.
        assert!(dst.intr_state());
        assert_eq!(dst.reg_read(REG_IIR) & 0x0F, 0b0100);
        let read: Vec<u8> = (0..5).map(|_| dst.reg_read(0)).collect();
        assert_eq!(read, b"mdata");
    }

    #[test]
    fn a_thre_interrupt_the_guest_has_not_taken_is_still_pending() {
        let mut src = programmed();
        src.reg_write(0, b'x');
        assert_eq!(src.data_read(), Some(b'x'));
        assert!(src.intr_state(), "THRE is pending on the source");

        let mut dst = Uart::new();
        dst.import_state(&src.export_state()).expect("import");

        assert!(dst.intr_state(), "THRE is pending on the destination");
        assert_eq!(dst.reg_read(REG_IIR) & 0x0F, 0b0010);
        // Reading IIR is how the guest takes it.
        assert!(!dst.intr_state());
    }

    #[test]
    fn pending_tx_drains_on_the_destination() {
        let mut src = programmed();
        src.reg_write(REG_IER, IER_ETBEI);
        for b in b"login: " {
            src.reg_write(0, *b);
        }
        // Take the THRE interrupt, as the guest's handler does.
        src.reg_read(REG_IIR);

        let mut dst = Uart::new();
        dst.import_state(&src.export_state()).expect("import");
        let mut out = Vec::new();
        while let Some(b) = dst.data_read() {
            out.push(b);
        }
        assert_eq!(out, b"login: ");
        assert!(dst.intr_state(), "an empty FIFO asks for more");
        assert_eq!(dst.reg_read(REG_LSR) & 0x60, 0x60);
    }

    #[test]
    fn an_oversized_fifo_is_refused_and_nothing_changes() {
        let mut dst = programmed();
        let before = dst.export_state();
        let state = UartMigrateState {
            rx: vec![0; FIFO_LEN + 1],
            ..UartMigrateState::default()
        };
        assert!(dst.import_state(&state).is_err());
        let state = UartMigrateState {
            tx: vec![0; FIFO_LEN + 1],
            ..UartMigrateState::default()
        };
        assert!(dst.import_state(&state).is_err());
        assert_eq!(dst.export_state(), before);
    }

    #[test]
    fn data_ready_follows_the_fifo_not_the_wire() {
        let mut dst = Uart::new();
        let state = UartMigrateState {
            ier: IER_ERBFI,
            lsr: 0x61,
            ..UartMigrateState::default()
        };
        dst.import_state(&state).expect("import");
        assert_eq!(dst.reg_read(REG_LSR) & 0x01, 0);
        assert!(!dst.intr_state());
    }
}
