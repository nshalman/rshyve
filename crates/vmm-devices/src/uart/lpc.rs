// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! LPC-attached UART.
//!
//! Bridges a [`Uart`] to the PIO bus and an interrupt pin, providing the
//! standard PC serial port interface. The standard configurations are:
//!
//! | Port  | Base   | IRQ |
//! |-------|--------|-----|
//! | COM1  | 0x3F8  |  4  |
//! | COM2  | 0x2F8  |  3  |
//!
//! # Thread safety
//!
//! All UART state is protected by a single `Mutex`. The interrupt pin
//! is synchronised after every register access to ensure the physical
//! IRQ line matches the logical state.
//!
//! # External data path
//!
//! Use [`LpcUart::input_byte`] to feed data from an external source
//! (e.g., a terminal emulator) into the guest's RX FIFO, and
//! [`LpcUart::output_byte`] to drain data the guest has transmitted.

use std::sync::{Arc, Mutex};

use vmm_core::common::RWOp;
use vmm_core::intr_pins::IntrPin;
use vmm_core::pio::{PioBus, PioFn};

use crate::lifecycle::{DeviceMigrateState, DeviceStateError};
use crate::Lifecycle;

use super::uart16550::Uart;

/// Number of I/O ports occupied by a 16550 UART (offsets 0-7).
pub const REGISTER_LEN: u16 = 8;

/// Standard I/O base address for COM1.
pub const COM1_BASE: u16 = 0x3F8;

/// Standard IRQ for COM1.
pub const COM1_IRQ: u8 = 4;

/// Standard I/O base address for COM2.
pub const COM2_BASE: u16 = 0x2F8;

/// Standard IRQ for COM2.
pub const COM2_IRQ: u8 = 3;

// ---------------------------------------------------------------------------
// Inner state behind the mutex
// ---------------------------------------------------------------------------

struct UartState {
    uart: Uart,
    irq_pin: Arc<dyn IntrPin>,
    /// Host input is held back while set. A byte that lands after the
    /// kernel interrupt state is exported raises a line the destination
    /// kernel never sees, and the port then stalls for good.
    paused: bool,
    /// Input that came while paused, up to [`HELD_CAP`]. It reaches the
    /// FIFO at the resume. A migration does not carry it.
    held: std::collections::VecDeque<u8>,
}

/// The RX FIFO's size: the resume can place no more than that.
const HELD_CAP: usize = 256;

impl UartState {
    /// Drive the physical interrupt pin to match the UART's logical
    /// interrupt state.
    #[inline]
    fn sync_intr_pin(&self) {
        self.irq_pin.set_state(self.uart.intr_state());
    }
}

// ---------------------------------------------------------------------------
// LpcUart -- public API
// ---------------------------------------------------------------------------

/// An LPC-attached 16550 UART.
///
/// Wraps [`Uart`] with PIO bus registration and interrupt pin
/// synchronisation.
/// Callback for synchronous TX output (called in PIO handler context).
pub type TxSink = Box<dyn Fn(u8) + Send + Sync>;

pub struct LpcUart {
    state: Mutex<UartState>,
    /// Optional synchronous sink for TX bytes. Called on every THR write
    /// in PIO handler context. If set, bytes go here instead of the TX FIFO.
    tx_sink: Mutex<Option<TxSink>>,
}

impl LpcUart {
    /// Create a new LPC UART wired to the given interrupt pin.
    pub fn new(irq_pin: Arc<dyn IntrPin>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(UartState {
                uart: Uart::new(),
                irq_pin,
                paused: false,
                held: std::collections::VecDeque::new(),
            }),
            tx_sink: Mutex::new(None),
        })
    }

    /// Set a synchronous TX sink that receives bytes immediately when
    /// the guest writes to THR. This bypasses the TX FIFO buffering.
    ///
    /// Used for `com1,stdio` style output where bytes must go directly
    /// to the host without buffering delays.
    pub fn set_tx_sink(&self, sink: TxSink) {
        *self.tx_sink.lock().expect("tx_sink lock") = Some(sink);
    }

    /// Register PIO handlers on `bus` at the given `base` port address.
    ///
    /// This covers ports `base` through `base + 7`.
    ///
    /// # Panics
    ///
    /// Panics if the port range conflicts with an existing registration
    /// on `bus`.
    pub fn attach(self: &Arc<Self>, bus: &PioBus, base: u16) {
        let this = Arc::clone(self);
        let handler: Arc<PioFn> =
            Arc::new(move |offset: u16, rwo: RWOp<'_>| {
                this.pio_rw(offset, rwo);
            });
        bus.register(base, REGISTER_LEN, handler)
            .expect("LpcUart: port range conflict during attach");
    }

    /// Feed a byte from an external source into the guest's RX path.
    ///
    /// Returns `true` if the byte was accepted, `false` if the RX FIFO
    /// was full, or the port is paused and its hold is full (byte
    /// dropped).
    pub fn input_byte(&self, data: u8) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.paused {
            if state.held.len() >= HELD_CAP {
                return false;
            }
            state.held.push_back(data);
            return true;
        }
        let accepted = state.uart.data_write(data);
        state.sync_intr_pin();
        accepted
    }

    /// Drain a byte from the guest's TX path.
    ///
    /// Returns `None` when there is no pending output.
    pub fn output_byte(&self) -> Option<u8> {
        let mut state = self.state.lock().unwrap();
        let byte = state.uart.data_read();
        state.sync_intr_pin();
        byte
    }

    /// Reset the UART to its power-on state.
    pub fn reset(&self) {
        let mut state = self.state.lock().unwrap();
        state.uart.reset();
        state.sync_intr_pin();
    }

    // -- private -----------------------------------------------------------

    /// Handle a PIO read or write from a vCPU thread.
    fn pio_rw(&self, offset: u16, rwo: RWOp<'_>) {
        let reg_offset = (offset & 0x07) as u8;

        let mut state = self.state.lock().unwrap();

        match rwo {
            RWOp::Read(ro) => {
                ro.write_u8(state.uart.reg_read(reg_offset));
            }
            RWOp::Write(wo) => {
                state.uart.reg_write(reg_offset, wo.read_u8());
            }
        }

        self.drain_to_sink(&mut state);
        state.sync_intr_pin();
    }

    /// Hand every pending TX byte to the sink, if one is set. This
    /// provides C bhyve-style immediate serial output without buffering
    /// delays.
    fn drain_to_sink(&self, state: &mut UartState) {
        if let Ok(sink) = self.tx_sink.lock() {
            if let Some(ref sink_fn) = *sink {
                while let Some(b) = state.uart.data_read() {
                    sink_fn(b);
                }
            }
        }
    }
}

impl Lifecycle for LpcUart {
    fn type_name(&self) -> &'static str {
        "lpc-uart"
    }

    fn pause(&self) {
        self.state.lock().unwrap().paused = true;
    }

    /// Bytes a source carried in its TX FIFO go out here, and the THRE
    /// interrupt that follows lets an interrupt-driven guest send on.
    fn resume(&self) {
        let mut state = self.state.lock().unwrap();
        state.paused = false;
        self.drain_to_sink(&mut state);
        // What the FIFO cannot take is dropped, as live input is.
        while let Some(byte) = state.held.pop_front() {
            if !state.uart.data_write(byte) {
                state.held.clear();
            }
        }
        state.sync_intr_pin();
    }

    fn migrates_by_id(&self) -> bool {
        true
    }

    fn export_migrate_state(
        &self,
    ) -> Result<Option<DeviceMigrateState>, DeviceStateError> {
        let state = self.state.lock().unwrap();
        Ok(Some(DeviceMigrateState::Uart(state.uart.export_state())))
    }

    /// Runs before the kernel interrupt state is imported, which then
    /// sets the line level the source had. The pin is driven here only
    /// so this side's own count of it matches that level.
    ///
    /// The port stays paused until [`Lifecycle::resume`], so no host
    /// byte lands between this and that import.
    fn restore_migrate_state(
        &self,
        state: &DeviceMigrateState,
    ) -> Result<(), DeviceStateError> {
        let DeviceMigrateState::Uart(mig) = state else {
            return Err(DeviceStateError::WrongKind {
                want: "uart",
                got: state.kind(),
            });
        };
        let mut state = self.state.lock().unwrap();
        state.uart.import_state(mig)?;
        state.paused = true;
        state.sync_intr_pin();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A test interrupt pin that tracks assertion state.
    struct TestPin {
        asserted: AtomicBool,
    }

    impl TestPin {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                asserted: AtomicBool::new(false),
            })
        }
    }

    impl IntrPin for TestPin {
        fn assert(&self) {
            self.asserted.store(true, Ordering::SeqCst);
        }
        fn deassert(&self) {
            self.asserted.store(false, Ordering::SeqCst);
        }
        fn is_asserted(&self) -> bool {
            self.asserted.load(Ordering::SeqCst)
        }
    }

    #[test]
    fn attach_and_read_lsr() {
        let pin = TestPin::new();
        let bus = PioBus::new();
        let uart = LpcUart::new(pin.clone());
        uart.attach(&bus, COM1_BASE);

        // Read LSR (offset 5 from base) -- should show THRE|TEMT.
        let val = bus.handle_in(COM1_BASE + 5, 1);
        assert_eq!(val, 0x60, "LSR should be THRE|TEMT at reset");
    }

    #[test]
    fn input_output_byte() {
        let pin = TestPin::new();
        let uart = LpcUart::new(pin);

        // Feed bytes in until FIFO is full (256 bytes).
        for i in 0..256u16 {
            assert!(uart.input_byte(i as u8), "input_byte {i} should succeed");
        }
        // FIFO full — next byte rejected.
        assert!(!uart.input_byte(0xFF));

        // Nothing in TX yet.
        assert_eq!(uart.output_byte(), None);
    }

    #[test]
    fn guest_tx_and_output_byte() {
        let pin = TestPin::new();
        let bus = PioBus::new();
        let uart = LpcUart::new(pin.clone());
        uart.attach(&bus, COM1_BASE);

        // Guest writes to THR (offset 0).
        bus.handle_out(COM1_BASE, 1, 0x61);

        // External consumer drains it.
        assert_eq!(uart.output_byte(), Some(0x61));
        assert_eq!(uart.output_byte(), None);
    }

    #[test]
    fn guest_rx_and_input_byte() {
        let pin = TestPin::new();
        let bus = PioBus::new();
        let uart = LpcUart::new(pin);
        uart.attach(&bus, COM1_BASE);

        // External source feeds data.
        assert!(uart.input_byte(0x62));

        // Guest reads RBR (offset 0).
        let val = bus.handle_in(COM1_BASE, 1);
        assert_eq!(val, 0x62);
    }

    #[test]
    fn interrupt_pin_synced() {
        let pin = TestPin::new();
        let bus = PioBus::new();
        let uart = LpcUart::new(pin.clone());
        uart.attach(&bus, COM1_BASE);

        // Enable ERBFI (data available interrupt).
        bus.handle_out(COM1_BASE + 1, 1, 0x01);
        assert!(!pin.is_asserted());

        // Feed data -- pin should assert.
        assert!(uart.input_byte(0x42));
        assert!(pin.is_asserted());

        // Guest reads the byte -- pin should deassert.
        let _ = bus.handle_in(COM1_BASE, 1);
        assert!(!pin.is_asserted());
    }

    /// A source whose guest wrote with IER set and no backend took the
    /// bytes yet: they are in the TX FIFO and THRE has been taken.
    fn source_with_pending_tx() -> DeviceMigrateState {
        let bus = PioBus::new();
        let src = LpcUart::new(TestPin::new());
        src.attach(&bus, COM2_BASE);
        bus.handle_out(COM2_BASE + 1, 1, 0x03); // IER = ERBFI | ETBEI
        for b in b"V2 21 " {
            bus.handle_out(COM2_BASE, 1, u32::from(*b));
        }
        let _ = bus.handle_in(COM2_BASE + 2, 1); // the guest takes THRE
        assert!(src.input_byte(b'!'));
        src.pause();
        assert!(src.input_byte(b'?'), "a paused port holds input");
        src.export_migrate_state().expect("export").expect("state")
    }

    #[test]
    fn pending_tx_with_ier_set_resumes_sending_on_the_target() {
        let state = source_with_pending_tx();
        let DeviceMigrateState::Uart(mig) = &state else {
            panic!("not uart state");
        };
        assert_eq!(mig.tx, b"V2 21 ");
        assert_eq!(mig.rx, b"!");

        let pin = TestPin::new();
        let bus = PioBus::new();
        let dst = LpcUart::new(pin.clone());
        dst.attach(&bus, COM2_BASE);
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&sent);
        dst.set_tx_sink(Box::new(move |b| sink.lock().unwrap().push(b)));

        dst.restore_migrate_state(&state).expect("restore");
        assert!(pin.is_asserted(), "the RX byte is pending");
        assert!(sent.lock().unwrap().is_empty());

        dst.resume();
        assert_eq!(*sent.lock().unwrap(), b"V2 21 ");
        // The guest drains RX, and THRE is what is left: the cue for an
        // interrupt-driven driver to write the rest.
        assert_eq!(bus.handle_in(COM2_BASE, 1), u32::from(b'!'));
        assert!(pin.is_asserted(), "THRE asks for more");
        assert_eq!(bus.handle_in(COM2_BASE + 2, 1) & 0x0F, 0b0010);
        bus.handle_out(COM2_BASE, 1, u32::from(b'\n'));
        assert_eq!(sent.lock().unwrap().last(), Some(&b'\n'));
        assert!(dst.input_byte(b'V'), "host input flows again");
    }

    #[test]
    fn a_restore_of_another_kind_is_refused() {
        let dst = LpcUart::new(TestPin::new());
        let other = DeviceMigrateState::AcpiGpe(Default::default());
        assert!(matches!(
            dst.restore_migrate_state(&other),
            Err(DeviceStateError::WrongKind { .. })
        ));
    }

    #[test]
    fn reset_deasserts_pin() {
        let pin = TestPin::new();
        let uart = LpcUart::new(pin.clone());

        // Enable ETBEI via internal state to get THRE interrupt.
        {
            let mut state = uart.state.lock().unwrap();
            state.uart.reg_write(1, 0x02); // IER = ETBEI
            state.sync_intr_pin();
        }
        assert!(pin.is_asserted());

        uart.reset();
        assert!(!pin.is_asserted());
    }

    #[test]
    fn input_while_paused_reaches_the_guest_at_the_resume() {
        let bus = PioBus::new();
        let uart = LpcUart::new(TestPin::new());
        uart.attach(&bus, COM2_BASE);
        uart.pause();
        assert!(uart.input_byte(b'a'));
        assert!(uart.input_byte(b'b'));
        // Held, not in the FIFO, so an export cannot carry a byte that
        // raised no interrupt.
        assert_eq!(bus.handle_in(COM2_BASE + 5, 1) & 1, 0, "data ready");
        uart.resume();
        assert_eq!(bus.handle_in(COM2_BASE, 1), u32::from(b'a'));
        assert_eq!(bus.handle_in(COM2_BASE, 1), u32::from(b'b'));
    }

    #[test]
    fn the_hold_is_bounded() {
        let uart = LpcUart::new(TestPin::new());
        uart.pause();
        for _ in 0..HELD_CAP {
            assert!(uart.input_byte(b'x'));
        }
        assert!(!uart.input_byte(b'x'), "past the bound");
    }
}
