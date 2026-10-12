// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! xHCI USB 3.0 host controller with built-in HID tablet device.
//!
//! Implements the minimal xHCI controller needed to present a single
//! USB HID absolute-pointer tablet to the guest.  This is the device
//! behind `-s 30:1,xhci,tablet` and pairs with the framebuffer for
//! VNC pointer input.
//!
//! # Architecture
//!
//! - BAR0: 32-bit MMIO (8 KiB) containing capability, operational,
//!   port, doorbell, runtime, and extended capability registers.
//! - INTx interrupt delivery via PCI PIRQ routing.
//! - Single device slot (slot 1) hosting the tablet.
//! - 2 ports (1 USB2 + 1 USB3) for extended capability compliance.
//! - 1 interrupter (interrupter 0).
//! - Command ring, event ring, and per-endpoint transfer rings.

pub mod bits;
mod migrate;
pub mod tablet;

use std::sync::{Arc, Mutex, Weak};

use vmm_core::common::RWOp;
use vmm_core::mem::PhysMap;
use vmm_core::mmio::{MmioBus, MmioFn};
use vmm_core::ratelimit::TokenBucket;

use vmm_devices::lifecycle::DeviceStateError;
use vmm_devices::pci::bar::BarDefine;
use vmm_devices::pci::device::{DeviceIdent, DeviceState, PciDevice};
use vmm_devices::pci::{BarN, IntrPin};
use vmm_devices::Lifecycle;

use bits::*;
use tablet::TabletDevice;

/// Cap on the event ring size, so a malicious guest cannot set an
/// enormous size that lets the enqueue pointer sweep across a huge
/// range of guest physical addresses before wrapping. xHCI allows
/// larger ERST entries, but 4096 entries (64 KiB of TRBs) is more than
/// sufficient.
const MAX_EVENT_RING_SIZE: u32 = 4096;

/// A command's completion code: Parameter Error when a context it reads
/// or writes is not guest memory.
fn context_written(written: bool) -> u32 {
    if written {
        TRB_CC_SUCCESS
    } else {
        TRB_CC_PARAMETER_ERROR
    }
}

/// Failed commands logged: the guest chooses how often it makes them.
const FAILURE_LOG_BURST: u32 = 4;
const FAILURE_LOG_PERIOD: std::time::Duration =
    std::time::Duration::from_secs(15);

/// A Port Status Change Event TRB (xHCI 1.2 section 6.4.2.3): the Port
/// ID is in dword 0 bits 31:24.
fn port_status_change_trb(port_id: u8) -> RawTrb {
    RawTrb {
        dword0: u32::from(port_id) << 24,
        dword1: 0,
        dword2: TRB_CC_SUCCESS << 24,
        dword3: TRB_TYPE_PORT_STATUS_CHANGE << TRB_TYPE_SHIFT,
    }
}

/// Whether a transfer TRB that completed with `cc` gets a Transfer
/// Event (xHCI 1.2 section 4.10.4): when it asked with IOC, when it
/// asked for short packets with ISP and was short, and always on an
/// error.
fn transfer_event_due(dword3: u32, cc: u32) -> bool {
    match cc {
        TRB_CC_SUCCESS => dword3 & TRB_IOC != 0,
        TRB_CC_SHORT_PACKET => dword3 & (TRB_IOC | TRB_ISP) != 0,
        _ => true,
    }
}

/// The Endpoint ID (DCI) a command TRB names, dword 3 bits 20:16.
fn trb_ep_id(trb: &RawTrb) -> u8 {
    ((trb.dword3 >> 16) & 0x1F) as u8
}

/// Replace the low dword of a 64-bit register.
fn merge_low_dword(reg: u64, val: u32) -> u64 {
    (reg & 0xFFFF_FFFF_0000_0000) | u64::from(val)
}

/// Replace the high dword of a 64-bit register.
fn merge_high_dword(reg: u64, val: u32) -> u64 {
    (reg & 0x0000_0000_FFFF_FFFF) | (u64::from(val) << 32)
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

/// Device slot states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Disabled,
    Enabled,
    Addressed,
    Configured,
}

/// Per-endpoint transfer ring tracking.
#[derive(Debug)]
struct TransferRing {
    /// Guest physical address of the ring base.
    base: u64,
    /// Current dequeue pointer.
    dequeue: u64,
    /// Ring cycle state (expected cycle bit).
    ccs: bool,
}

impl TransferRing {
    fn new() -> Self {
        Self {
            base: 0,
            dequeue: 0,
            ccs: true,
        }
    }
}

/// xHCI controller mutable state (under lock).
struct XhciState {
    // ---- Operational registers ----
    usbcmd: u32,
    usbsts: u32,
    dnctrl: u32,
    crcr: u64,   // Command Ring Control Register
    dcbaap: u64, // Device Context Base Address Array Pointer
    config: u32,

    // ---- Port state ----
    /// Port 1 (USB2) status/control.
    port1_sc: u32,
    /// Port 2 (USB3) status/control — device is connected here.
    port2_sc: u32,

    // ---- Runtime / interrupter 0 ----
    iman: u32,
    imod: u32,
    erstsz: u32,
    erstba: u64,
    erdp: u64,

    // ---- Event ring state ----
    /// Guest physical base of event ring segment 0.
    event_ring_base: u64,
    /// Size of event ring segment 0 (in TRBs).
    event_ring_size: u32,
    /// Next write index into the event ring.
    event_ring_enqueue: u32,
    /// Event ring producer cycle state.
    event_ring_pcs: bool,

    // ---- Command ring state ----
    cmd_ring_base: u64,
    cmd_ring_dequeue: u64,
    cmd_ring_ccs: bool,

    // ---- Device slot 1 ----
    slot_state: SlotState,
    /// EP0 (control) transfer ring.
    ep0_ring: TransferRing,
    /// EP1 IN (interrupt) transfer ring for tablet reports.
    ep1_in_ring: TransferRing,
    /// Whether EP1 IN has pending TRBs queued by the guest.
    ep1_in_has_trbs: bool,

    /// Pointer reports wait in the tablet while set. The remote console
    /// thread is the one writer of guest memory that runs when no vCPU
    /// does, and a write after the last RAM pass never reaches the
    /// destination.
    paused: bool,
}

impl XhciState {
    fn new() -> Self {
        Self {
            usbcmd: 0,
            usbsts: USBSTS_HCH, // Halted on reset
            dnctrl: 0,
            crcr: 0,
            dcbaap: 0,
            config: 0,

            // Port 1 (USB2): not connected, powered
            port1_sc: PORTSC_PP | (PLS_RXDETECT << PORTSC_PLS_SHIFT),
            // Port 2 (USB3): connected, powered, SuperSpeed
            port2_sc: PORTSC_CCS
                | PORTSC_PP
                | (PLS_U0 << PORTSC_PLS_SHIFT)
                | (SPEED_SUPER << PORTSC_SPEED_SHIFT),

            iman: 0,
            imod: 0,
            erstsz: 0,
            erstba: 0,
            erdp: 0,

            event_ring_base: 0,
            event_ring_size: 0,
            event_ring_enqueue: 0,
            event_ring_pcs: true,

            cmd_ring_base: 0,
            cmd_ring_dequeue: 0,
            cmd_ring_ccs: true,

            slot_state: SlotState::Disabled,
            ep0_ring: TransferRing::new(),
            ep1_in_ring: TransferRing::new(),
            ep1_in_has_trbs: false,
            paused: false,
        }
    }

    fn enable_slot(&mut self) -> (u32, u8) {
        if self.slot_state != SlotState::Disabled {
            return (TRB_CC_NO_SLOTS_AVAILABLE, 0);
        }

        self.slot_state = SlotState::Enabled;
        (TRB_CC_SUCCESS, 1)
    }
}

// ---------------------------------------------------------------------------
// xHCI Controller
// ---------------------------------------------------------------------------

/// xHCI USB 3.0 host controller.
///
/// Presents a PCI device with a single BAR0 (MMIO) and INTx interrupt.
/// Hosts a built-in USB HID tablet device for absolute pointer input.
pub struct XhciController {
    pci_state: Mutex<DeviceState>,
    state: Mutex<XhciState>,
    /// The built-in tablet device.
    tablet: TabletDevice,
    /// Guest physical memory map.
    physmap: Arc<PhysMap>,
    /// MMIO bus for BAR registration.
    bus_mmio: Arc<MmioBus>,
    /// Currently registered BAR0 address.
    registered_bar0: Mutex<Option<u64>>,
    /// Interrupt pin for INTx delivery.
    intr_pin: Option<Arc<dyn IntrPin>>,
    /// Weak self-reference for MMIO handler closures.
    self_ref: Mutex<Option<Weak<Self>>>,
    /// Structured logger for bus registration and failed commands.
    log: slog::Logger,
    /// Caps the failed commands logged.
    failures: TokenBucket,
}

impl XhciController {
    /// Create a new xHCI controller with a built-in tablet device.
    ///
    /// Bus registration failures go to a discarding logger. Use
    /// [`new_with_logger`](Self::new_with_logger) to see them.
    pub fn new(
        physmap: Arc<PhysMap>,
        bus_mmio: Arc<MmioBus>,
        intr_pin: Option<Arc<dyn IntrPin>>,
    ) -> Arc<Self> {
        Self::new_with_logger(
            physmap,
            bus_mmio,
            intr_pin,
            slog::Logger::root(slog::Discard, slog::o!()),
        )
    }

    /// Create a new xHCI controller with a logger for bus registration
    /// diagnostics.
    pub fn new_with_logger(
        physmap: Arc<PhysMap>,
        bus_mmio: Arc<MmioBus>,
        intr_pin: Option<Arc<dyn IntrPin>>,
        log: slog::Logger,
    ) -> Arc<Self> {
        let ident = DeviceIdent {
            vendor_id: PCI_VENDOR_INTEL,
            device_id: PCI_DEVICE_XHCI_PPT,
            class: PCI_CLASS_SERIAL,
            subclass: PCI_SUBCLASS_USB,
            prog_if: PCI_PROGIF_XHCI,
            revision: 0x04,
            sub_vendor_id: PCI_VENDOR_INTEL,
            sub_device_id: PCI_DEVICE_XHCI_PPT,
        };

        let mut pci_state = DeviceState::new(ident);
        pci_state.define_bar(BarN::BAR0, BarDefine::Mmio(BAR0_SIZE));
        pci_state.set_intr_pin(1); // INTA

        let ctrl = Arc::new(Self {
            pci_state: Mutex::new(pci_state),
            state: Mutex::new(XhciState::new()),
            tablet: TabletDevice::new(),
            physmap,
            bus_mmio,
            registered_bar0: Mutex::new(None),
            intr_pin,
            self_ref: Mutex::new(None),
            log,
            failures: TokenBucket::new(FAILURE_LOG_BURST, FAILURE_LOG_PERIOD),
        });

        // Store weak self-reference for MMIO closures
        *ctrl.self_ref.lock().expect("xhci: self_ref lock") =
            Some(Arc::downgrade(&ctrl));

        ctrl
    }

    /// Get a reference to the built-in tablet device.
    ///
    /// The VNC server uses this to call `pointer_event()`.
    pub fn tablet(&self) -> &TabletDevice {
        &self.tablet
    }

    // ---- MMIO BAR registration (follows NVMe pattern) --------------------

    /// Unregister the recorded BAR0 base and clear the record.
    fn release_mmio(&self, reg: &mut Option<u64>) {
        if let Some(addr) = reg.take() {
            if let Err(e) = self.bus_mmio.unregister(addr) {
                slog::warn!(self.log, "xhci: MMIO unregister failed";
                    "bar" => ?BarN::BAR0,
                    "addr" => format!("{addr:#x}"),
                    "error" => %e);
            }
        }
    }

    /// Update MMIO bus registration after BAR or command register changes.
    fn update_mmio_registration(self: &Arc<Self>) {
        let pci = self.pci_state.lock().expect("xhci: pci lock");
        let mmio_enabled = pci
            .command()
            .contains(vmm_devices::pci::bits::RegCmd::MMIO_EN);

        let mut reg = self.registered_bar0.lock().expect("xhci: bar0 reg lock");
        self.release_mmio(&mut reg);

        if mmio_enabled {
            if let Some((def, addr)) = pci.bars().get(BarN::BAR0) {
                if addr != 0 && def.is_mmio() {
                    let size = def.size();
                    let dev = Arc::clone(self);
                    let handler: Arc<MmioFn> =
                        Arc::new(move |offset: usize, rwo: RWOp<'_>| {
                            dev.bar_rw(BarN::BAR0, offset, rwo);
                        });
                    match self.bus_mmio.register(addr, size, handler) {
                        Ok(()) => *reg = Some(addr),
                        // The controller now decodes nothing on BAR0.
                        Err(e) => {
                            slog::error!(self.log,
                                "xhci: MMIO register failed, BAR0 dark";
                                "bar" => ?BarN::BAR0,
                                "addr" => format!("{addr:#x}"),
                                "size" => size,
                                "error" => %e);
                        }
                    }
                }
            }
        }
    }

    // ---- Interrupt delivery ----------------------------------------------

    /// Assert the INTx interrupt to the guest.
    ///
    /// INTx is level-triggered, so the line stays up until the driver
    /// clears USBSTS.EINT. A pulse is lost whenever it arrives while the
    /// interrupt controller still waits for the previous EOI, and the
    /// event it announced then goes unseen.
    fn assert_interrupt(&self, st: &mut XhciState) {
        st.usbsts |= USBSTS_EINT;
        st.iman |= IMAN_IP;
        if let Some(pin) = &self.intr_pin {
            pin.assert();
        }
    }

    // ---- Event ring management -------------------------------------------

    /// Write an event TRB to the event ring and optionally assert interrupt.
    fn post_event(&self, st: &mut XhciState, event: RawTrb) {
        if st.event_ring_base == 0 || st.event_ring_size == 0 {
            return;
        }

        let idx = st.event_ring_enqueue;
        if idx >= st.event_ring_size {
            // Enqueue pointer out of bounds — reset to avoid
            // writing to arbitrary guest memory.
            st.event_ring_enqueue = 0;
            return;
        }
        let offset = u64::from(idx) * TRB_SIZE as u64;
        let gpa = match st.event_ring_base.checked_add(offset) {
            Some(a) => a,
            None => return,
        };

        // Build the TRB with the producer cycle state
        let mut trb = event;
        if st.event_ring_pcs {
            trb.dword3 |= TRB_CYCLE;
        } else {
            trb.dword3 &= !TRB_CYCLE;
        }

        // Write TRB to guest memory
        let trb_bytes: [u8; TRB_SIZE] = unsafe { std::mem::transmute(trb) };
        // The guest has no way to hear of this one, so the log does.
        if !self.write_guest(gpa, &trb_bytes) && self.failures.take() {
            slog::warn!(self.log, "xhci: the event ring is not guest memory";
                "gpa" => format!("{gpa:#x}"));
        }

        // Advance enqueue pointer
        st.event_ring_enqueue = idx + 1;
        if st.event_ring_enqueue >= st.event_ring_size {
            st.event_ring_enqueue = 0;
            st.event_ring_pcs = !st.event_ring_pcs;
        }

        // Assert interrupt if interrupter is enabled
        if (st.iman & IMAN_IE) != 0 && (st.usbcmd & USBCMD_INTE) != 0 {
            self.assert_interrupt(st);
        }
    }

    /// Post a Command Completion Event.
    fn post_cmd_completion(
        &self,
        st: &mut XhciState,
        cmd_trb_addr: u64,
        cc: u32,
        slot_id: u8,
    ) {
        // A tablet the guest never sees then has a cause in the log.
        if cc != TRB_CC_SUCCESS && self.failures.take() {
            slog::warn!(self.log, "xhci: a command failed";
                "completion_code" => cc, "slot" => slot_id);
        }
        let event = RawTrb {
            dword0: cmd_trb_addr as u32,
            dword1: (cmd_trb_addr >> 32) as u32,
            dword2: (cc << 24),
            dword3: (TRB_TYPE_CMD_COMPLETION << TRB_TYPE_SHIFT)
                | (u32::from(slot_id) << 24),
        };
        self.post_event(st, event);
    }

    /// Post a Port Status Change Event.
    fn post_port_status_change(&self, st: &mut XhciState, port_id: u8) {
        self.post_event(st, port_status_change_trb(port_id));
    }

    /// Post a Transfer Event.
    fn post_transfer_event(
        &self,
        st: &mut XhciState,
        trb_addr: u64,
        cc: u32,
        transfer_len: u32,
        slot_id: u8,
        ep_id: u8,
    ) {
        let event = RawTrb {
            dword0: trb_addr as u32,
            dword1: (trb_addr >> 32) as u32,
            dword2: (cc << 24) | (transfer_len & 0xFFFFFF),
            dword3: (TRB_TYPE_TRANSFER_EVENT << TRB_TYPE_SHIFT)
                | (u32::from(slot_id) << 24)
                | (u32::from(ep_id) << 16),
        };
        self.post_event(st, event);
    }

    /// Apply a complete CRCR value.
    ///
    /// The command ring never runs between doorbells in this model, so
    /// CRR is never set and every write lands. A driver that writes the
    /// two halves separately reaches here twice; the last one wins.
    fn write_crcr(&self, st: &mut XhciState, crcr: u64) {
        let ptr = crcr & CRCR_PTR_MASK;
        st.crcr = crcr & !CRCR_CRR;
        st.cmd_ring_base = ptr;
        st.cmd_ring_dequeue = ptr;
        st.cmd_ring_ccs = (crcr & CRCR_RCS) != 0;
    }

    // ---- Event Ring Segment Table parsing ---------------------------------

    /// Read the ERST and configure the event ring.
    ///
    /// ERST Max advertises one segment, and ERSTSZ is clamped to match,
    /// so entry 0 describes the whole ring.
    fn setup_event_ring(&self, st: &mut XhciState) {
        if st.erstba == 0 || st.erstsz == 0 {
            return;
        }

        let mut buf = [0u8; 16];
        if let Some(sub) = self.physmap.lookup(st.erstba, 16) {
            if sub.read_bytes(&mut buf).is_err() {
                return;
            }
        } else {
            return;
        }

        let base_lo = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let base_hi = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let size = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);

        if size == 0 || size > MAX_EVENT_RING_SIZE {
            return;
        }

        st.event_ring_base = u64::from(base_lo) | (u64::from(base_hi) << 32);
        st.event_ring_size = size;
        st.event_ring_enqueue = 0;
        st.event_ring_pcs = true;
    }

    // ---- Command ring processing -----------------------------------------

    /// Read a TRB from guest memory at the given GPA.
    fn read_trb(&self, gpa: u64) -> Option<RawTrb> {
        let mut buf = [0u8; TRB_SIZE];
        let sub = self.physmap.lookup(gpa, TRB_SIZE)?;
        sub.read_bytes(&mut buf).ok()?;

        Some(RawTrb {
            dword0: u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
            dword1: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            dword2: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            dword3: u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
        })
    }

    /// Process all pending TRBs on the command ring.
    fn process_command_ring(&self, st: &mut XhciState) {
        if st.cmd_ring_base == 0 {
            return;
        }

        // Process up to 16 commands per doorbell ring to avoid infinite loops
        for _ in 0..16 {
            let trb_addr = st.cmd_ring_dequeue;
            let trb = match self.read_trb(trb_addr) {
                Some(t) => t,
                None => break,
            };

            // Check cycle bit matches expected
            if trb.cycle_bit() != st.cmd_ring_ccs {
                break;
            }

            let trb_type = trb.trb_type();

            match trb_type {
                TRB_TYPE_LINK => {
                    // Follow the link to the new segment
                    st.cmd_ring_dequeue = trb.parameter() & CRCR_PTR_MASK;
                    // Toggle cycle if the Toggle Cycle bit is set
                    if (trb.dword3 & (1 << 1)) != 0 {
                        st.cmd_ring_ccs = !st.cmd_ring_ccs;
                    }
                    continue;
                }

                TRB_TYPE_ENABLE_SLOT => {
                    let (cc, slot_id) = st.enable_slot();
                    self.post_cmd_completion(st, trb_addr, cc, slot_id);
                }

                TRB_TYPE_ADDRESS_DEVICE => {
                    if st.slot_state == SlotState::Disabled {
                        self.post_cmd_completion(
                            st,
                            trb_addr,
                            TRB_CC_SLOT_NOT_ENABLED,
                            trb.slot_id(),
                        );
                    } else {
                        let input_ctx_addr = trb.parameter() & !0xF;
                        let cc = self.address_device(
                            st,
                            trb.slot_id(),
                            input_ctx_addr,
                        );
                        self.post_cmd_completion(
                            st,
                            trb_addr,
                            cc,
                            trb.slot_id(),
                        );
                    }
                }

                TRB_TYPE_CONFIGURE_EP => {
                    if st.slot_state == SlotState::Disabled {
                        self.post_cmd_completion(
                            st,
                            trb_addr,
                            TRB_CC_SLOT_NOT_ENABLED,
                            trb.slot_id(),
                        );
                    } else {
                        let cc = self.configure_endpoint(st, &trb);
                        self.post_cmd_completion(
                            st,
                            trb_addr,
                            cc,
                            trb.slot_id(),
                        );
                    }
                }

                TRB_TYPE_EVALUATE_CTX => {
                    let input_ctx_addr = trb.parameter() & !0xF;
                    let cc = context_written(self.evaluate_output_ctx(
                        st,
                        trb.slot_id(),
                        input_ctx_addr,
                    ));
                    self.post_cmd_completion(st, trb_addr, cc, trb.slot_id());
                }

                TRB_TYPE_DISABLE_SLOT => {
                    if st.slot_state == SlotState::Disabled {
                        self.post_cmd_completion(
                            st,
                            trb_addr,
                            TRB_CC_SLOT_NOT_ENABLED,
                            trb.slot_id(),
                        );
                    } else {
                        st.slot_state = SlotState::Disabled;
                        st.ep0_ring = TransferRing::new();
                        st.ep1_in_ring = TransferRing::new();
                        st.ep1_in_has_trbs = false;
                        self.post_cmd_completion(
                            st,
                            trb_addr,
                            TRB_CC_SUCCESS,
                            trb.slot_id(),
                        );
                    }
                }

                TRB_TYPE_NOOP_CMD => {
                    self.post_cmd_completion(
                        st,
                        trb_addr,
                        TRB_CC_SUCCESS,
                        trb.slot_id(),
                    );
                }

                // Nothing here ever halts an endpoint, so a reset or a
                // stop only has to leave it Stopped until the next
                // doorbell.
                TRB_TYPE_RESET_EP | TRB_TYPE_STOP_EP => {
                    let cc = context_written(self.set_ep_state(
                        st,
                        trb.slot_id(),
                        trb_ep_id(&trb),
                        EP_STATE_STOPPED,
                    ));
                    self.post_cmd_completion(st, trb_addr, cc, trb.slot_id());
                }

                TRB_TYPE_SET_TR_DEQUEUE => {
                    let cc = self.set_tr_dequeue(st, &trb);
                    self.post_cmd_completion(st, trb_addr, cc, trb.slot_id());
                }

                _ => {
                    // Unknown command: complete with error
                    self.post_cmd_completion(
                        st,
                        trb_addr,
                        TRB_CC_TRB_ERROR,
                        trb.slot_id(),
                    );
                }
            }

            // Advance to next TRB
            st.cmd_ring_dequeue += TRB_SIZE as u64;
        }
    }

    // ---- Output Device Context ---------------------------------------------
    //
    // The controller owns the output Device Context the DCBAA names for
    // each slot (xHCI 1.2 section 4.6): the slot state and USB address,
    // and every endpoint's state and dequeue pointer. Linux reads it back
    // and will not queue a transfer to an endpoint it finds Disabled.

    /// Slot `slot_id`'s output Device Context. The DCBAA and its entries
    /// are guest-written, so an address that is not guest memory yields
    /// `None` and the context is simply not written.
    fn output_ctx(&self, st: &XhciState, slot_id: u8) -> Option<u64> {
        let entry = st.dcbaap.checked_add(u64::from(slot_id) * 8)?;
        let mut buf = [0u8; 8];
        self.physmap.lookup(entry, 8)?.read_bytes(&mut buf).ok()?;
        let addr = u64::from_le_bytes(buf) & !0x3F;
        (addr != 0).then_some(addr)
    }

    /// Context `index` of the context array at `base`, 32 bytes each.
    fn read_ctx(&self, base: u64, index: u64) -> Option<[u32; 8]> {
        let gpa = base.checked_add(index * CTX_SIZE as u64)?;
        let mut buf = [0u8; CTX_SIZE];
        self.physmap
            .lookup(gpa, CTX_SIZE)?
            .read_bytes(&mut buf)
            .ok()?;
        let mut ctx = [0u32; 8];
        for (i, d) in ctx.iter_mut().enumerate() {
            *d = u32::from_le_bytes([
                buf[i * 4],
                buf[i * 4 + 1],
                buf[i * 4 + 2],
                buf[i * 4 + 3],
            ]);
        }
        Some(ctx)
    }

    /// Copy what fits of `data` to the driver's buffer of `len` bytes at
    /// `gpa`: the transfer's completion code and residual length.
    fn transfer_in(&self, gpa: u64, data: &[u8], len: u32) -> (u32, u32) {
        let copy_len = data.len().min(len as usize);
        if !self.write_guest(gpa, &data[..copy_len]) {
            (TRB_CC_DATA_BUFFER_ERROR, len)
        } else if copy_len < len as usize {
            (TRB_CC_SHORT_PACKET, len - copy_len as u32)
        } else {
            (TRB_CC_SUCCESS, 0)
        }
    }

    /// Write `bytes` to the driver's buffer at `gpa`. False when that is
    /// not guest memory; nothing to write always succeeds.
    fn write_guest(&self, gpa: u64, bytes: &[u8]) -> bool {
        bytes.is_empty()
            || self
                .physmap
                .lookup(gpa, bytes.len())
                .is_some_and(|sub| sub.write_bytes(bytes).is_ok())
    }

    /// Write context `index` of the context array at `base`. False when
    /// that is not guest memory.
    fn write_ctx(&self, base: u64, index: u64, ctx: &[u32; 8]) -> bool {
        let Some(gpa) = base.checked_add(index * CTX_SIZE as u64) else {
            return false;
        };
        let mut buf = [0u8; CTX_SIZE];
        for (i, d) in ctx.iter().enumerate() {
            buf[i * 4..][..4].copy_from_slice(&d.to_le_bytes());
        }
        self.physmap
            .lookup(gpa, CTX_SIZE)
            .is_some_and(|sub| sub.write_bytes(&buf).is_ok())
    }

    /// Change one context of slot `slot_id`'s output context in place.
    /// Index 0 is the slot context; index n is DCI n.
    fn update_output_ctx(
        &self,
        st: &XhciState,
        slot_id: u8,
        index: u64,
        f: impl FnOnce(&mut [u32; 8]),
    ) -> bool {
        let Some(out) = self.output_ctx(st, slot_id) else {
            return false;
        };
        let Some(mut ctx) = self.read_ctx(out, index) else {
            return false;
        };
        f(&mut ctx);
        self.write_ctx(out, index, &ctx)
    }

    /// Change endpoint context `dci` of slot `slot_id`'s output context.
    /// False when it could not be written; a DCI that names no endpoint
    /// has nothing to write.
    fn update_ep_ctx(
        &self,
        st: &XhciState,
        slot_id: u8,
        dci: u8,
        f: impl FnOnce(&mut [u32; 8]),
    ) -> bool {
        !(1..=31).contains(&dci)
            || self.update_output_ctx(st, slot_id, u64::from(dci), f)
    }

    fn set_ep_state(
        &self,
        st: &XhciState,
        slot_id: u8,
        dci: u8,
        state: u32,
    ) -> bool {
        self.update_ep_ctx(st, slot_id, dci, |ep| {
            ep[0] = (ep[0] & !EP_STATE_MASK) | state;
        })
    }

    fn set_ep_dequeue(
        &self,
        st: &XhciState,
        slot_id: u8,
        dci: u8,
        deq: u64,
    ) -> bool {
        self.update_ep_ctx(st, slot_id, dci, |ep| {
            ep[2] = deq as u32;
            ep[3] = (deq >> 32) as u32;
        })
    }

    /// Record where the controller is on DCI `dci`'s ring (1 = EP0,
    /// 3 = EP1 IN) in the output context. Linux reads it to learn where
    /// a stopped or halted endpoint stopped.
    fn sync_ep_dequeue(&self, st: &XhciState, dci: u8) {
        let ring = match dci {
            1 => &st.ep0_ring,
            3 => &st.ep1_in_ring,
            _ => return,
        };
        let deq = ring.dequeue | u64::from(ring.ccs);
        self.set_ep_dequeue(st, 1, dci, deq);
    }

    fn set_slot_state(&self, st: &XhciState, slot_id: u8, state: u32) -> bool {
        self.update_output_ctx(st, slot_id, 0, |slot| {
            slot[3] = (slot[3] & !(0x1F << SLOT_STATE_SHIFT))
                | (state << SLOT_STATE_SHIFT);
        })
    }

    /// Set TR Dequeue Pointer; its completion code. The ring moves only
    /// once the output context records it.
    fn set_tr_dequeue(&self, st: &mut XhciState, trb: &RawTrb) -> u32 {
        let ep_id = trb_ep_id(trb);
        let new_deq = trb.parameter() & !0xF;
        let new_ccs = (trb.parameter() & 1) != 0;
        if !self.set_ep_dequeue(
            st,
            trb.slot_id(),
            ep_id,
            trb.parameter() & !0xE,
        ) {
            return TRB_CC_PARAMETER_ERROR;
        }
        match ep_id {
            1 => {
                st.ep0_ring.dequeue = new_deq;
                st.ep0_ring.ccs = new_ccs;
            }
            3 => {
                st.ep1_in_ring.dequeue = new_deq;
                st.ep1_in_ring.ccs = new_ccs;
            }
            _ => {}
        }
        TRB_CC_SUCCESS
    }

    /// Address Device on an enabled slot; its completion code.
    fn address_device(
        &self,
        st: &mut XhciState,
        slot_id: u8,
        input_ctx_addr: u64,
    ) -> u32 {
        if !self.address_output_ctx(st, slot_id, input_ctx_addr) {
            return TRB_CC_PARAMETER_ERROR;
        }
        // Read the input context to get EP0 transfer ring
        self.parse_input_context_for_ep0(st, input_ctx_addr);
        st.slot_state = SlotState::Addressed;
        TRB_CC_SUCCESS
    }

    /// Configure Endpoint on an enabled slot; its completion code.
    fn configure_endpoint(&self, st: &mut XhciState, trb: &RawTrb) -> u32 {
        let input_ctx_addr = trb.parameter() & !0xF;
        if (trb.dword3 & CONFIGURE_EP_DC) != 0 {
            // Deconfigure: the input context pointer is ignored.
            if !self.deconfigure_output_ctx(st, trb.slot_id()) {
                return TRB_CC_PARAMETER_ERROR;
            }
            st.slot_state = SlotState::Addressed;
            return TRB_CC_SUCCESS;
        }
        if !self.configure_output_ctx(st, trb.slot_id(), input_ctx_addr) {
            return TRB_CC_PARAMETER_ERROR;
        }
        // Parse input context for EP1 IN configuration
        self.parse_input_context_for_ep1(st, input_ctx_addr);
        st.slot_state = SlotState::Configured;
        TRB_CC_SUCCESS
    }

    /// Address Device: the input slot and EP0 contexts become the output
    /// ones, with the slot Addressed at USB address `slot_id` and EP0
    /// Running. False when either context is not guest memory.
    fn address_output_ctx(
        &self,
        st: &XhciState,
        slot_id: u8,
        input: u64,
    ) -> bool {
        let Some(out) = self.output_ctx(st, slot_id) else {
            return false;
        };
        let (Some(mut slot), Some(mut ep0)) =
            (self.read_ctx(input, 1), self.read_ctx(input, 2))
        else {
            return false;
        };
        slot[3] =
            (SLOT_STATE_ADDRESSED << SLOT_STATE_SHIFT) | u32::from(slot_id);
        ep0[0] = (ep0[0] & !EP_STATE_MASK) | EP_STATE_RUNNING;
        self.write_ctx(out, 0, &slot) && self.write_ctx(out, 1, &ep0)
    }

    /// Configure Endpoint: every endpoint the input control context drops
    /// is Disabled, every one it adds is copied in and Running, and the
    /// slot is Configured. False when a context it needs is not guest
    /// memory; the endpoints before that one may have changed.
    fn configure_output_ctx(
        &self,
        st: &XhciState,
        slot_id: u8,
        input: u64,
    ) -> bool {
        let Some(out) = self.output_ctx(st, slot_id) else {
            return false;
        };
        let Some(icc) = self.read_ctx(input, 0) else {
            return false;
        };
        let (drop, add) = (icc[0], icc[1]);
        for dci in 2..=31u64 {
            if add & (1 << dci) != 0 {
                let Some(mut ep) = self.read_ctx(input, dci + 1) else {
                    return false;
                };
                ep[0] = (ep[0] & !EP_STATE_MASK) | EP_STATE_RUNNING;
                if !self.write_ctx(out, dci, &ep) {
                    return false;
                }
            } else if drop & (1 << dci) != 0 {
                self.set_ep_state(st, slot_id, dci as u8, EP_STATE_DISABLED);
            }
        }
        let Some(mut slot) = self.read_ctx(out, 0) else {
            return false;
        };
        if add & 1 != 0 {
            // The driver's slot fields, the controller's state and
            // address.
            let Some(input_slot) = self.read_ctx(input, 1) else {
                return false;
            };
            slot[..3].copy_from_slice(&input_slot[..3]);
        }
        slot[3] = (slot[3] & !(0x1F << SLOT_STATE_SHIFT))
            | (SLOT_STATE_CONFIGURED << SLOT_STATE_SHIFT);
        self.write_ctx(out, 0, &slot)
    }

    /// Configure Endpoint with DC: every endpoint but EP0 is Disabled and
    /// the slot goes back to Addressed. False when the output context
    /// could not be written.
    fn deconfigure_output_ctx(&self, st: &XhciState, slot_id: u8) -> bool {
        (2..=31u8)
            .all(|dci| self.set_ep_state(st, slot_id, dci, EP_STATE_DISABLED))
            && self.set_slot_state(st, slot_id, SLOT_STATE_ADDRESSED)
    }

    /// Evaluate Context: EP0's max packet size, and the slot's max exit
    /// latency and interrupter target, when the input control context
    /// names them.
    /// False when a context it needs is not guest memory.
    fn evaluate_output_ctx(
        &self,
        st: &XhciState,
        slot_id: u8,
        input: u64,
    ) -> bool {
        let Some(icc) = self.read_ctx(input, 0) else {
            return false;
        };
        let add = icc[1];
        if add & 1 != 0 {
            let Some(input_slot) = self.read_ctx(input, 1) else {
                return false;
            };
            let written = self.update_output_ctx(st, slot_id, 0, |slot| {
                slot[1] = (slot[1] & !0xFFFF) | (input_slot[1] & 0xFFFF);
                slot[2] = (slot[2] & !(0x3FF << 22))
                    | (input_slot[2] & (0x3FF << 22));
            });
            if !written {
                return false;
            }
        }
        if add & (1 << 1) != 0 {
            let Some(input_ep0) = self.read_ctx(input, 2) else {
                return false;
            };
            return self.update_output_ctx(st, slot_id, 1, |ep0| {
                ep0[1] = (ep0[1] & 0xFFFF) | (input_ep0[1] & 0xFFFF_0000);
            });
        }
        true
    }

    /// Parse the Input Context to extract the EP0 transfer ring dequeue pointer.
    fn parse_input_context_for_ep0(
        &self,
        st: &mut XhciState,
        input_ctx_addr: u64,
    ) {
        // Input Context layout (32-byte contexts):
        //   [0x00..0x1F] Input Control Context
        //   [0x20..0x3F] Slot Context
        //   [0x40..0x5F] EP0 Context (DCI 1)
        let Some(ep0_ctx_addr) = input_ctx_addr
            .checked_add(INPUT_CTRL_CTX_SIZE as u64 + SLOT_CTX_SIZE as u64)
        else {
            return;
        };

        // EP Context dword 2 (offset 0x08): TR Dequeue Pointer low
        // EP Context dword 3 (offset 0x0C): TR Dequeue Pointer high
        let mut buf = [0u8; EP_CTX_SIZE];
        if let Some(sub) = self.physmap.lookup(ep0_ctx_addr, EP_CTX_SIZE) {
            if sub.read_bytes(&mut buf).is_ok() {
                let deq_lo =
                    u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
                let deq_hi =
                    u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
                let deq = u64::from(deq_lo) | (u64::from(deq_hi) << 32);
                st.ep0_ring.dequeue = deq & !0xF;
                st.ep0_ring.ccs = (deq & 1) != 0;
                st.ep0_ring.base = st.ep0_ring.dequeue;
            }
        }
    }

    /// Parse the Input Context to extract the EP1 IN transfer ring dequeue pointer.
    fn parse_input_context_for_ep1(
        &self,
        st: &mut XhciState,
        input_ctx_addr: u64,
    ) {
        // EP1 IN = DCI 3, so it's the 4th context entry (after input ctrl, slot, EP0)
        // Offset = InputCtrl(32) + Slot(32) + EP0(32) + EP1OUT(32) = 128
        // But for our device there is no EP1 OUT, so the guest should
        // configure DCI 3 at offset: InputCtrl + Slot + EP0 + EP1OUT = 0x80
        // Actually: DCI = 2*EP + direction, EP1 IN = 2*1+1 = 3
        // Context index = DCI, so offset = InputCtrl + DCI * 32
        let Some(ep1_in_ctx_addr) = input_ctx_addr
            .checked_add(INPUT_CTRL_CTX_SIZE as u64 + 3 * EP_CTX_SIZE as u64)
        else {
            return;
        };

        let mut buf = [0u8; EP_CTX_SIZE];
        if let Some(sub) = self.physmap.lookup(ep1_in_ctx_addr, EP_CTX_SIZE) {
            if sub.read_bytes(&mut buf).is_ok() {
                let deq_lo =
                    u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
                let deq_hi =
                    u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
                let deq = u64::from(deq_lo) | (u64::from(deq_hi) << 32);
                st.ep1_in_ring.dequeue = deq & !0xF;
                st.ep1_in_ring.ccs = (deq & 1) != 0;
                st.ep1_in_ring.base = st.ep1_in_ring.dequeue;
            }
        }
    }

    // ---- Transfer ring processing ----------------------------------------

    /// Process the EP0 (control) transfer ring.
    ///
    /// Handles SETUP/DATA/STATUS stage TRBs for USB control transfers.
    fn process_ep0_transfer_ring(&self, st: &mut XhciState) {
        self.walk_ep0_transfer_ring(st);
        self.sync_ep_dequeue(st, 1);
    }

    /// Walk EP0's ring from its dequeue pointer.
    fn walk_ep0_transfer_ring(&self, st: &mut XhciState) {
        for _ in 0..32 {
            let trb_addr = st.ep0_ring.dequeue;
            let trb = match self.read_trb(trb_addr) {
                Some(t) => t,
                None => break,
            };

            if trb.cycle_bit() != st.ep0_ring.ccs {
                break;
            }

            let trb_type = trb.trb_type();

            match trb_type {
                TRB_TYPE_LINK => {
                    st.ep0_ring.dequeue = trb.parameter() & CRCR_PTR_MASK;
                    if (trb.dword3 & (1 << 1)) != 0 {
                        st.ep0_ring.ccs = !st.ep0_ring.ccs;
                    }
                    continue;
                }

                TRB_TYPE_SETUP_STAGE => {
                    // Parse the 8-byte SETUP packet from the TRB
                    let bm_request_type = trb.dword0 as u8;
                    let b_request = (trb.dword0 >> 8) as u8;
                    let w_value = (trb.dword0 >> 16) as u16;
                    let w_index = trb.dword1 as u16;
                    let w_length = (trb.dword1 >> 16) as u16;

                    // Process the control request
                    let response = self.tablet.handle_control(
                        bm_request_type,
                        b_request,
                        w_value,
                        w_index,
                        w_length,
                    );

                    // Complete the SETUP stage. All 8 bytes moved, so
                    // the residual is 0.
                    if transfer_event_due(trb.dword3, TRB_CC_SUCCESS) {
                        self.post_transfer_event(
                            st,
                            trb_addr,
                            TRB_CC_SUCCESS,
                            0,
                            1, // slot 1
                            1, // EP0 = DCI 1
                        );
                    }

                    // Advance past SETUP
                    st.ep0_ring.dequeue += TRB_SIZE as u64;

                    // Now handle DATA + STATUS stages
                    self.process_ep0_data_status(st, response);
                    continue;
                }

                _ => {
                    // Skip unexpected TRB types
                    st.ep0_ring.dequeue += TRB_SIZE as u64;
                    continue;
                }
            }
        }
    }

    /// Process DATA and STATUS stage TRBs following a SETUP stage.
    fn process_ep0_data_status(
        &self,
        st: &mut XhciState,
        response: Option<Vec<u8>>,
    ) {
        // Process remaining TRBs (DATA_STAGE and STATUS_STAGE)
        for _ in 0..8 {
            let trb_addr = st.ep0_ring.dequeue;
            let trb = match self.read_trb(trb_addr) {
                Some(t) => t,
                None => break,
            };

            if trb.cycle_bit() != st.ep0_ring.ccs {
                break;
            }

            let trb_type = trb.trb_type();
            match trb_type {
                TRB_TYPE_LINK => {
                    st.ep0_ring.dequeue = trb.parameter() & CRCR_PTR_MASK;
                    if (trb.dword3 & (1 << 1)) != 0 {
                        st.ep0_ring.ccs = !st.ep0_ring.ccs;
                    }
                    continue;
                }

                TRB_TYPE_DATA_STAGE => {
                    let data_buf_addr = trb.parameter();
                    let trb_transfer_len = trb.transfer_length();
                    let dir_in = (trb.dword3 & TRB_DATA_DIR_IN) != 0;

                    let cc;
                    let residual;

                    if dir_in {
                        // IN data stage: copy response data to guest buffer
                        if let Some(ref data) = response {
                            (cc, residual) = self.transfer_in(
                                data_buf_addr,
                                data,
                                trb_transfer_len,
                            );
                        } else {
                            // STALL
                            cc = TRB_CC_STALL;
                            residual = trb_transfer_len;
                        }
                    } else {
                        // OUT data stage: guest sends data to device.
                        // We accept but ignore the data.
                        cc = TRB_CC_SUCCESS;
                        residual = 0;
                    }

                    if transfer_event_due(trb.dword3, cc) {
                        self.post_transfer_event(
                            st, trb_addr, cc, residual, 1, 1,
                        );
                    }
                    st.ep0_ring.dequeue += TRB_SIZE as u64;
                }

                TRB_TYPE_STATUS_STAGE => {
                    let cc = if response.is_some() {
                        TRB_CC_SUCCESS
                    } else {
                        TRB_CC_STALL
                    };
                    if transfer_event_due(trb.dword3, cc) {
                        self.post_transfer_event(st, trb_addr, cc, 0, 1, 1);
                    }
                    st.ep0_ring.dequeue += TRB_SIZE as u64;
                    return;
                }

                _ => {
                    st.ep0_ring.dequeue += TRB_SIZE as u64;
                }
            }
        }
    }

    /// Process the EP1 IN (interrupt) transfer ring.
    ///
    /// If the tablet has a pending report and the guest has queued TRBs,
    /// copy the report into the guest buffer and post a transfer event.
    fn process_ep1_in_transfer_ring(&self, st: &mut XhciState) {
        self.walk_ep1_in_transfer_ring(st);
        self.sync_ep_dequeue(st, 3);
    }

    /// Walk EP1 IN's ring from its dequeue pointer.
    fn walk_ep1_in_transfer_ring(&self, st: &mut XhciState) {
        if st.slot_state != SlotState::Configured {
            return;
        }

        // Check if tablet has a pending report
        if !self.tablet.has_pending_report() {
            // No data to send. Mark that TRBs are available for later.
            st.ep1_in_has_trbs = true;
            return;
        }

        for _ in 0..4 {
            let trb_addr = st.ep1_in_ring.dequeue;
            let trb = match self.read_trb(trb_addr) {
                Some(t) => t,
                None => {
                    st.ep1_in_has_trbs = false;
                    break;
                }
            };

            if trb.cycle_bit() != st.ep1_in_ring.ccs {
                st.ep1_in_has_trbs = false;
                break;
            }

            let trb_type = trb.trb_type();

            match trb_type {
                TRB_TYPE_LINK => {
                    st.ep1_in_ring.dequeue = trb.parameter() & CRCR_PTR_MASK;
                    if (trb.dword3 & (1 << 1)) != 0 {
                        st.ep1_in_ring.ccs = !st.ep1_in_ring.ccs;
                    }
                    continue;
                }

                TRB_TYPE_NORMAL => {
                    let data_buf_addr = trb.parameter();
                    let trb_transfer_len = trb.transfer_length();
                    let report = self.tablet.get_report();
                    let (cc, residual) = self.transfer_in(
                        data_buf_addr,
                        &report,
                        trb_transfer_len,
                    );

                    // EP1 IN = DCI 3
                    if transfer_event_due(trb.dword3, cc) {
                        self.post_transfer_event(
                            st, trb_addr, cc, residual, 1, 3,
                        );
                    }
                    st.ep1_in_ring.dequeue += TRB_SIZE as u64;
                    st.ep1_in_has_trbs = false;
                    return;
                }

                _ => {
                    st.ep1_in_ring.dequeue += TRB_SIZE as u64;
                }
            }
        }
    }

    /// Called when the VNC server has new pointer data and the device
    /// should attempt to deliver it if TRBs are available.
    ///
    /// This is typically called after `tablet().pointer_event()`.
    pub fn notify_pointer_event(&self) {
        let mut st = self.state.lock().expect("xhci: state lock");
        if st.ep1_in_has_trbs && !st.paused {
            self.process_ep1_in_transfer_ring(&mut st);
        }
    }

    // ---- BAR0 register access -------------------------------------------

    /// Handle a BAR0 MMIO read or write.
    fn bar0_rw(&self, offset: usize, rwo: RWOp<'_>) {
        // Determine which register region this falls into
        if offset < OP_BASE {
            self.cap_reg_rw(offset, rwo);
        } else if offset < (DBOFF as usize) {
            // Operational + port registers
            let op_off = offset - OP_BASE;
            self.op_reg_rw(op_off, rwo);
        } else if offset < (RTSOFF as usize) {
            // Doorbell registers
            let db_off = offset - DBOFF as usize;
            self.doorbell_rw(db_off, rwo);
        } else if offset < (XCAP_BASE) {
            // Runtime registers
            let rt_off = offset - RTSOFF as usize;
            self.runtime_reg_rw(rt_off, rwo);
        } else if offset < (BAR0_SIZE as usize) {
            // Extended capabilities
            let xcap_off = offset - XCAP_BASE;
            self.xcap_rw(xcap_off, rwo);
        } else {
            // Out of range
            if let RWOp::Read(ro) = rwo {
                ro.write_u32(0);
            }
        }
    }

    // ---- Capability registers (read-only) --------------------------------

    fn cap_reg_rw(&self, offset: usize, rwo: RWOp<'_>) {
        match rwo {
            RWOp::Read(ro) => {
                let val = match offset & !0x3 {
                    // CAPLENGTH (8) | reserved (8) | HCIVERSION (16)
                    0x00 => {
                        u32::from(CAPLENGTH) | (u32::from(HCIVERSION) << 16)
                    }
                    0x04 => hcsparams1(),
                    0x08 => hcsparams2(),
                    0x0C => HCSPARAMS3,
                    0x10 => hccparams1(),
                    0x14 => DBOFF,
                    0x18 => RTSOFF,
                    0x1C => HCCPARAMS2,
                    _ => 0,
                };
                ro.write_dword_at(val, offset)
            }
            RWOp::Write(_) => {
                // Capability registers are read-only
            }
        }
    }

    // ---- Operational registers -------------------------------------------

    fn op_reg_rw(&self, op_off: usize, rwo: RWOp<'_>) {
        // Check if this is a port register
        if op_off >= PORTSC_BASE {
            self.port_reg_rw(op_off, rwo);
            return;
        }

        let mut st = self.state.lock().expect("xhci: state lock");
        match rwo {
            RWOp::Read(ro) => {
                // AC64 is advertised, so a guest may read a 64-bit
                // register in one access.
                if ro.len() == 8 {
                    match op_off {
                        OP_CRCR => return ro.write_u64(st.crcr),
                        OP_DCBAAP => return ro.write_u64(st.dcbaap),
                        _ => {}
                    }
                }
                let val = match op_off & !0x3 {
                    OP_USBCMD => st.usbcmd,
                    OP_USBSTS => st.usbsts,
                    OP_PAGESIZE => 1, // 4K pages
                    OP_DNCTRL => st.dnctrl,
                    OP_CRCR => st.crcr as u32,
                    0x1C => (st.crcr >> 32) as u32, // CRCR high
                    OP_DCBAAP => st.dcbaap as u32,
                    0x34 => (st.dcbaap >> 32) as u32, // DCBAAP high
                    OP_CONFIG => st.config,
                    _ => 0,
                };
                ro.write_dword_at(val, op_off)
            }
            RWOp::Write(wo) => {
                if wo.len() == 8 {
                    match op_off {
                        OP_CRCR => self.write_crcr(&mut st, wo.read_u64()),
                        OP_DCBAAP => st.dcbaap = wo.read_u64(),
                        _ => {}
                    }
                    return;
                }
                let val = wo.read_u32();
                match op_off & !0x3 {
                    OP_USBCMD => {
                        let old = st.usbcmd;
                        st.usbcmd = val;

                        // Handle Run/Stop transition
                        if (val & USBCMD_RS) != 0 && (old & USBCMD_RS) == 0 {
                            // Starting: clear HCHalted
                            st.usbsts &= !USBSTS_HCH;

                            // Post port status change for port 2 (USB3, connected)
                            st.port2_sc |= PORTSC_CSC;
                            self.post_port_status_change(&mut st, 2);
                        } else if (val & USBCMD_RS) == 0
                            && (old & USBCMD_RS) != 0
                        {
                            // Stopping: set HCHalted
                            st.usbsts |= USBSTS_HCH;
                        }

                        // Handle Host Controller Reset
                        if (val & USBCMD_HCRST) != 0 {
                            st.usbcmd = 0;
                            st.usbsts = USBSTS_HCH;
                            // EINT is gone, so the line it held goes too.
                            if let Some(pin) = &self.intr_pin {
                                pin.deassert();
                            }
                            st.crcr = 0;
                            st.dcbaap = 0;
                            st.config = 0;
                            st.slot_state = SlotState::Disabled;
                            st.event_ring_base = 0;
                            st.event_ring_size = 0;
                            st.event_ring_enqueue = 0;
                            st.event_ring_pcs = true;
                            st.cmd_ring_base = 0;
                            st.cmd_ring_dequeue = 0;
                            st.cmd_ring_ccs = true;
                            st.ep0_ring = TransferRing::new();
                            st.ep1_in_ring = TransferRing::new();
                            st.ep1_in_has_trbs = false;
                            st.iman = 0;
                            st.imod = 0;
                            st.erstsz = 0;
                            st.erstba = 0;
                            st.erdp = 0;
                        }
                    }
                    OP_USBSTS => {
                        // Write-1-to-clear for status bits
                        st.usbsts &= !val;
                        // Deassert interrupt if EINT was cleared
                        if (val & USBSTS_EINT) != 0 {
                            if let Some(pin) = &self.intr_pin {
                                pin.deassert();
                            }
                        }
                    }
                    OP_DNCTRL => {
                        st.dnctrl = val;
                    }
                    OP_CRCR => {
                        let crcr = merge_low_dword(st.crcr, val);
                        self.write_crcr(&mut st, crcr);
                    }
                    0x1C => {
                        let crcr = merge_high_dword(st.crcr, val);
                        self.write_crcr(&mut st, crcr);
                    }
                    OP_DCBAAP => {
                        st.dcbaap = merge_low_dword(st.dcbaap, val);
                    }
                    0x34 => {
                        st.dcbaap = merge_high_dword(st.dcbaap, val);
                    }
                    OP_CONFIG => {
                        st.config = (val & 0xFF).min(u32::from(MAX_SLOTS));
                    }
                    _ => {}
                }
            }
        }
    }

    // ---- Port registers --------------------------------------------------

    fn port_reg_rw(&self, op_off: usize, rwo: RWOp<'_>) {
        let port_off = op_off - PORTSC_BASE;
        let port_idx = port_off / PORT_REG_SIZE;
        let reg_off = port_off % PORT_REG_SIZE;

        let mut st = self.state.lock().expect("xhci: state lock");

        match rwo {
            RWOp::Read(ro) => {
                let val = match (port_idx, reg_off) {
                    (0, PORTSC_PORTSC) => st.port1_sc,
                    (1, PORTSC_PORTSC) => st.port2_sc,
                    _ => 0,
                };
                ro.write_dword_at(val, op_off)
            }
            RWOp::Write(wo) => {
                let val = wo.read_u32();
                if reg_off == PORTSC_PORTSC {
                    let portsc = match port_idx {
                        0 => &mut st.port1_sc,
                        1 => &mut st.port2_sc,
                        _ => return,
                    };

                    // Write-1-to-clear for change bits
                    let w1c_bits = PORTSC_CSC | PORTSC_PRC | PORTSC_WRC;
                    *portsc &= !(val & w1c_bits);

                    // Handle port reset
                    if (val & PORTSC_PR) != 0 && (*portsc & PORTSC_CCS) != 0 {
                        // Complete reset immediately
                        *portsc |= PORTSC_PED | PORTSC_PRC;
                        *portsc &= !PORTSC_PR;
                        // Set link state to U0
                        *portsc &= !(PORTSC_PLS_MASK << PORTSC_PLS_SHIFT);
                        *portsc |= PLS_U0 << PORTSC_PLS_SHIFT;

                        let port_id = (port_idx + 1) as u8;
                        self.post_port_status_change(&mut st, port_id);
                    }

                    // Handle port link state write
                    if (val & PORTSC_LWS) != 0 {
                        let new_pls =
                            (val >> PORTSC_PLS_SHIFT) & PORTSC_PLS_MASK;
                        let portsc = match port_idx {
                            0 => &mut st.port1_sc,
                            1 => &mut st.port2_sc,
                            _ => return,
                        };
                        *portsc &= !(PORTSC_PLS_MASK << PORTSC_PLS_SHIFT);
                        *portsc |= new_pls << PORTSC_PLS_SHIFT;
                    }
                }
            }
        }
    }

    // ---- Doorbell registers ----------------------------------------------

    fn doorbell_rw(&self, db_off: usize, rwo: RWOp<'_>) {
        match rwo {
            RWOp::Read(ro) => {
                // Doorbell registers read as 0
                ro.write_dword(0)
            }
            RWOp::Write(wo) => {
                let db_idx = db_off / DB_REG_SIZE;
                if db_idx > usize::from(MAX_SLOTS) {
                    return;
                }
                let val = wo.read_u32();
                let mut st = self.state.lock().expect("xhci: state lock");

                match db_idx {
                    // Doorbell 0: Command ring
                    0 => {
                        self.process_command_ring(&mut st);
                    }
                    // Doorbell 1: Device slot 1
                    1 => {
                        let ep_target = val & 0xFF;
                        if matches!(ep_target, 1 | 3) {
                            // A doorbell restarts a stopped endpoint.
                            self.set_ep_state(
                                &st,
                                1,
                                ep_target as u8,
                                EP_STATE_RUNNING,
                            );
                        }
                        match ep_target {
                            // DCI 1 = EP0 (control, bidirectional)
                            1 => {
                                self.process_ep0_transfer_ring(&mut st);
                            }
                            // DCI 3 = EP1 IN (interrupt)
                            3 => {
                                self.process_ep1_in_transfer_ring(&mut st);
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // ---- Runtime registers -----------------------------------------------

    fn runtime_reg_rw(&self, rt_off: usize, rwo: RWOp<'_>) {
        let mut st = self.state.lock().expect("xhci: state lock");

        match rwo {
            RWOp::Read(ro) => {
                // AC64 is advertised, so a guest may read a 64-bit
                // register in one access.
                if ro.len() == 8 && rt_off >= IR0_BASE {
                    match rt_off - IR0_BASE {
                        IR_ERSTBA => return ro.write_u64(st.erstba),
                        IR_ERDP => return ro.write_u64(st.erdp),
                        _ => {}
                    }
                }
                let val = if rt_off < IR0_BASE {
                    // Microframes are not tracked, so MFINDEX stays 0.
                    0
                } else {
                    let ir_off = rt_off - IR0_BASE;
                    match ir_off & !0x3 {
                        IR_IMAN => st.iman,
                        IR_IMOD => st.imod,
                        IR_ERSTSZ => st.erstsz,
                        IR_ERSTBA => st.erstba as u32,
                        0x14 => (st.erstba >> 32) as u32,
                        IR_ERDP => st.erdp as u32,
                        0x1C => (st.erdp >> 32) as u32,
                        _ => 0,
                    }
                };
                ro.write_dword_at(val, rt_off)
            }
            RWOp::Write(wo) => {
                if rt_off < IR0_BASE {
                    return; // MFINDEX is read-only
                }
                let ir_off = rt_off - IR0_BASE;
                if wo.len() == 8 {
                    match ir_off {
                        IR_ERSTBA => {
                            st.erstba = wo.read_u64() & ERSTBA_PTR_MASK;
                            self.setup_event_ring(&mut st);
                        }
                        IR_ERDP => st.erdp = wo.read_u64() & ERDP_PTR_MASK,
                        _ => {}
                    }
                    return;
                }
                let val = wo.read_u32();
                match ir_off & !0x3 {
                    IR_IMAN => {
                        // Write-1-to-clear IP, IE is read/write
                        if (val & IMAN_IP) != 0 {
                            st.iman &= !IMAN_IP;
                        }
                        if (val & IMAN_IE) != 0 {
                            st.iman |= IMAN_IE;
                        } else {
                            st.iman &= !IMAN_IE;
                        }
                    }
                    IR_IMOD => {
                        st.imod = val;
                    }
                    IR_ERSTSZ => {
                        // ERST Max advertises one segment, so only the
                        // first entry can ever be valid.
                        st.erstsz = (val & 0xFFFF).min(1);
                    }
                    IR_ERSTBA => {
                        st.erstba =
                            merge_low_dword(st.erstba, val) & ERSTBA_PTR_MASK;
                        self.setup_event_ring(&mut st);
                    }
                    0x14 => {
                        st.erstba =
                            merge_high_dword(st.erstba, val) & ERSTBA_PTR_MASK;
                        self.setup_event_ring(&mut st);
                    }
                    IR_ERDP => {
                        st.erdp = merge_low_dword(st.erdp, val) & ERDP_PTR_MASK;
                    }
                    0x1C => {
                        st.erdp =
                            merge_high_dword(st.erdp, val) & ERDP_PTR_MASK;
                    }
                    _ => {}
                }
            }
        }
    }

    // ---- Extended capabilities -------------------------------------------

    fn xcap_rw(&self, xcap_off: usize, rwo: RWOp<'_>) {
        match rwo {
            RWOp::Read(ro) => {
                let val = if xcap_off < XCAP_PROTO_SIZE {
                    // USB2 Supported Protocol
                    let dw_idx = xcap_off / 4;
                    let usb2 = xcap_usb2();
                    if dw_idx < usb2.len() {
                        usb2[dw_idx]
                    } else {
                        0
                    }
                } else if xcap_off < 2 * XCAP_PROTO_SIZE {
                    // USB3 Supported Protocol
                    let dw_idx = (xcap_off - XCAP_PROTO_SIZE) / 4;
                    let usb3 = xcap_usb3();
                    if dw_idx < usb3.len() {
                        usb3[dw_idx]
                    } else {
                        0
                    }
                } else {
                    0
                };
                ro.write_dword_at(val, xcap_off)
            }
            RWOp::Write(_) => {
                // Extended capabilities are read-only
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PciDevice trait implementation
// ---------------------------------------------------------------------------

impl PciDevice for XhciController {
    fn cfg_read(&self, offset: u8, len: u8) -> u32 {
        let pci = self.pci_state.lock().expect("xhci: pci_state lock");
        pci.cfg_read(offset, len)
    }

    fn cfg_write(&self, offset: u8, len: u8, val: u32) {
        let mut pci = self.pci_state.lock().expect("xhci: pci_state lock");
        pci.cfg_write(offset, len, val);

        // Check if BAR or command register changed
        let dword_off = offset & 0xFC;
        if dword_off == 0x04 || (0x10..=0x24).contains(&dword_off) {
            let weak = self.self_ref.lock().expect("xhci: self_ref lock");
            if let Some(arc_self) = weak.as_ref().and_then(|w| w.upgrade()) {
                drop(pci);
                arc_self.update_mmio_registration();
            }
        }
    }

    fn bar_rw(&self, bar: BarN, offset: usize, rwo: RWOp<'_>) {
        match bar {
            BarN::BAR0 => self.bar0_rw(offset, rwo),
            _ => {
                if let RWOp::Read(ro) = rwo {
                    ro.write_u32(0);
                }
            }
        }
    }

    fn detach_regions(&self) {
        let mut reg = self.registered_bar0.lock().expect("xhci: bar0 reg lock");
        self.release_mmio(&mut reg);
    }
}

impl vmm_devices::PointerSink for XhciController {
    fn pointer_event(&self, buttons: u8, x: u16, y: u16) {
        self.tablet().pointer_event(buttons, x, y);
        self.notify_pointer_event();
    }
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

impl Lifecycle for XhciController {
    fn type_name(&self) -> &'static str {
        "xhci"
    }

    // Every ring is walked inline on the thread that rang the doorbell,
    // so nothing is in flight once the lock is taken.
    fn pause(&self) {
        self.state.lock().expect("xhci: state lock").paused = true;
    }

    /// A report that came in while paused, or that the source carried,
    /// goes to the TRB the guest left armed.
    fn resume(&self) {
        let mut st = self.state.lock().expect("xhci: state lock");
        st.paused = false;
        if st.ep1_in_has_trbs {
            self.process_ep1_in_transfer_ring(&mut st);
        }
    }

    fn export_migrate_state(
        &self,
    ) -> Result<Option<vmm_devices::DeviceMigrateState>, DeviceStateError> {
        Ok(Some(self.export_state()))
    }

    fn restore_migrate_state(
        &self,
        state: &vmm_devices::DeviceMigrateState,
    ) -> Result<(), DeviceStateError> {
        self.restore_state(state)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use vmm_core::common::{ReadOp, WriteOp};

    /// Helper to create an XhciState for testing register logic.
    fn test_state() -> XhciState {
        XhciState::new()
    }

    fn test_controller() -> Arc<XhciController> {
        XhciController::new(
            Arc::new(PhysMap::new()),
            Arc::new(MmioBus::new()),
            None,
        )
    }

    const RAM_LEN: usize = 0x10_000;

    /// A controller over real guest memory, so `PhysMap::lookup`
    /// answers and the ring walks reach something.
    fn test_controller_with_ram() -> Arc<XhciController> {
        let physmap =
            PhysMap::new_anon(0, RAM_LEN).expect("anonymous guest memory");
        XhciController::new(Arc::new(physmap), Arc::new(MmioBus::new()), None)
    }

    fn write_ram(ctrl: &XhciController, gpa: u64, bytes: &[u8]) {
        let sub = ctrl
            .physmap
            .lookup(gpa, bytes.len())
            .expect("guest memory at gpa");
        sub.write_bytes(bytes).expect("write guest memory");
    }

    /// One ERST entry: `base` and `size` TRBs.
    fn write_erst_entry(ctrl: &XhciController, gpa: u64, base: u64, size: u32) {
        let mut entry = [0u8; 16];
        entry[..8].copy_from_slice(&base.to_le_bytes());
        entry[8..12].copy_from_slice(&size.to_le_bytes());
        write_ram(ctrl, gpa, &entry);
    }

    fn write_reg(ctrl: &XhciController, off: usize, bytes: &[u8]) {
        let wo = WriteOp::from_buf(bytes);
        ctrl.bar0_rw(off, RWOp::Write(&wo));
    }

    fn read_reg(ctrl: &XhciController, off: usize, len: usize) -> u64 {
        let mut ro = ReadOp::new(len);
        ctrl.bar0_rw(off, RWOp::Read(&mut ro));
        let mut buf = [0u8; 8];
        buf[..len].copy_from_slice(ro.buf());
        u64::from_le_bytes(buf)
    }

    // ---- Output Device Context -------------------------------------------

    /// Guest memory for a driver that has set up its rings: ERST, event
    /// ring, command ring, DCBAA (slot 1's output context at OUT_CTX)
    /// and an input context at IN_CTX.
    const ERST_GPA: u64 = 0x2000;
    const EVENT_RING_GPA: u64 = 0x3000;
    const CMD_RING_GPA: u64 = 0x4000;
    const DCBAA_GPA: u64 = 0x5000;
    const OUT_CTX: u64 = 0x6000;
    const IN_CTX: u64 = 0x7000;

    struct Driver {
        ctrl: Arc<XhciController>,
        commands: u64,
        /// Events the controller has posted so far.
        events: u64,
    }

    impl Driver {
        fn new() -> Self {
            Self::with_ctrl(test_controller_with_ram())
        }

        fn with_ctrl(ctrl: Arc<XhciController>) -> Self {
            write_erst_entry(&ctrl, ERST_GPA, EVENT_RING_GPA, 16);
            write_reg(&ctrl, ERSTSZ_OFF, &1u32.to_le_bytes());
            write_reg(&ctrl, ERSTBA_OFF, &ERST_GPA.to_le_bytes());
            write_reg(
                &ctrl,
                OP_BASE + OP_CRCR,
                &(CMD_RING_GPA | 1).to_le_bytes(),
            );
            write_reg(&ctrl, OP_BASE + OP_DCBAAP, &DCBAA_GPA.to_le_bytes());
            write_ram(&ctrl, DCBAA_GPA + 8, &OUT_CTX.to_le_bytes());
            Self {
                ctrl,
                commands: 0,
                events: 0,
            }
        }

        /// Post one command, ring doorbell 0 and return its completion
        /// code.
        fn command(
            &mut self,
            trb_type: u32,
            parameter: u64,
            dword3: u32,
        ) -> u32 {
            let trb = RawTrb {
                dword0: parameter as u32,
                dword1: (parameter >> 32) as u32,
                dword2: 0,
                dword3: (trb_type << TRB_TYPE_SHIFT) | dword3 | TRB_CYCLE,
            };
            let at = CMD_RING_GPA + self.commands * TRB_SIZE as u64;
            write_ram(&self.ctrl, at, &trb_bytes(&trb));
            write_reg(&self.ctrl, DBOFF as usize, &0u32.to_le_bytes());
            let event = self
                .read_dwords(EVENT_RING_GPA + self.events * TRB_SIZE as u64, 4);
            self.commands += 1;
            self.events += 1;
            assert_eq!(
                (event[3] >> TRB_TYPE_SHIFT) & 0x3F,
                TRB_TYPE_CMD_COMPLETION,
                "command {trb_type} got no completion"
            );
            event[2] >> 24
        }

        fn read_dwords(&self, gpa: u64, n: usize) -> Vec<u32> {
            let mut buf = vec![0u8; n * 4];
            self.ctrl
                .physmap
                .lookup(gpa, buf.len())
                .expect("guest memory")
                .read_bytes(&mut buf)
                .expect("read guest memory");
            buf.chunks(4)
                .map(|c| u32::from_le_bytes(c.try_into().expect("4 bytes")))
                .collect()
        }

        /// Output context `index`: 0 is the slot, n is DCI n.
        fn out(&self, index: u64) -> Vec<u32> {
            self.read_dwords(OUT_CTX + index * 32, 8)
        }

        /// Input context `index`: 0 is input control, 1 the slot,
        /// n + 1 is DCI n.
        fn set_in(&self, index: u64, dwords: [u32; 8]) {
            let bytes: Vec<u8> =
                dwords.iter().flat_map(|d| d.to_le_bytes()).collect();
            write_ram(&self.ctrl, IN_CTX + index * 32, &bytes);
        }

        fn slot1(&self) -> u32 {
            1 << 24
        }

        /// Enable slot 1 and ask to address it with a control EP0 whose
        /// ring is at 0x8000; the Address Device completion code.
        fn address(&mut self) -> u32 {
            assert_eq!(
                self.command(TRB_TYPE_ENABLE_SLOT, 0, 0),
                TRB_CC_SUCCESS
            );
            self.set_in(0, [0, 0b11, 0, 0, 0, 0, 0, 0]);
            // Slot: one context entry, SuperSpeed.
            self.set_in(1, [(1 << 27) | (4 << 20), 0, 0, 0, 0, 0, 0, 0]);
            // EP0: control, max packet 512, dequeue 0x8000 with DCS 1.
            self.set_in(2, [0, (4 << 3) | (512 << 16), 0x8001, 0, 8, 0, 0, 0]);
            let slot = self.slot1();
            self.command(TRB_TYPE_ADDRESS_DEVICE, IN_CTX, slot)
        }

        /// The slot's state as the controller holds it.
        fn controller_slot_state(&self) -> SlotState {
            self.ctrl.state.lock().expect("xhci: state lock").slot_state
        }

        /// Enable slot 1 and address it with a control EP0 whose ring
        /// is at 0x8000.
        fn addressed(&mut self) {
            assert_eq!(self.address(), TRB_CC_SUCCESS);
        }

        /// Configure EP1 IN (DCI 3), interrupt, ring at 0x9000.
        fn configured(&mut self) {
            self.addressed();
            self.set_in(0, [0, (1 << 0) | (1 << 3), 0, 0, 0, 0, 0, 0]);
            self.set_in(1, [(3 << 27) | (4 << 20), 0, 0, 0, 0, 0, 0, 0]);
            self.set_in(4, [0, (7 << 3) | (8 << 16), 0x9001, 0, 8, 0, 0, 0]);
            let slot = self.slot1();
            assert_eq!(
                self.command(TRB_TYPE_CONFIGURE_EP, IN_CTX, slot),
                TRB_CC_SUCCESS
            );
        }
    }

    fn trb_bytes(trb: &RawTrb) -> [u8; TRB_SIZE] {
        let mut b = [0u8; TRB_SIZE];
        for (i, d) in [trb.dword0, trb.dword1, trb.dword2, trb.dword3]
            .iter()
            .enumerate()
        {
            b[i * 4..][..4].copy_from_slice(&d.to_le_bytes());
        }
        b
    }

    fn slot_state(slot: &[u32]) -> u32 {
        slot[3] >> 27
    }

    fn ep_state(ep: &[u32]) -> u32 {
        ep[0] & 0x7
    }

    /// Linux reads the output context the controller writes: an EP0 it
    /// finds Disabled gets "WARN urb submitted to disabled ep" and the
    /// tablet never enumerates.
    /// An input context pointer at the top of the address space is the
    /// guest's to choose; adding the context offsets must not overflow.
    #[test]
    fn an_input_context_at_the_top_of_memory_is_survived() {
        let mut d = Driver::new();
        assert_eq!(d.command(TRB_TYPE_ENABLE_SLOT, 0, 0), TRB_CC_SUCCESS);
        let top = 0xFFFF_FFFF_FFFF_FFF0;
        let slot = d.slot1();
        assert_eq!(
            d.command(TRB_TYPE_ADDRESS_DEVICE, top, slot),
            TRB_CC_PARAMETER_ERROR
        );
        assert_eq!(
            d.command(TRB_TYPE_CONFIGURE_EP, top, slot),
            TRB_CC_PARAMETER_ERROR
        );
        assert_eq!(d.controller_slot_state(), SlotState::Enabled);
    }

    #[test]
    fn address_device_fills_the_output_context() {
        let mut d = Driver::new();
        d.addressed();

        let slot = d.out(0);
        assert_eq!(slot_state(&slot), SLOT_STATE_ADDRESSED);
        assert_eq!(slot[3] & 0xFF, 1, "USB device address");
        assert_eq!(slot[0], (1 << 27) | (4 << 20), "copied from input");

        let ep0 = d.out(1);
        assert_eq!(ep_state(&ep0), EP_STATE_RUNNING);
        assert_eq!(ep0[1], (4 << 3) | (512 << 16));
        assert_eq!(ep0[2], 0x8001);
    }

    #[test]
    fn configure_endpoint_runs_the_added_endpoint() {
        let mut d = Driver::new();
        d.configured();

        let slot = d.out(0);
        assert_eq!(slot_state(&slot), SLOT_STATE_CONFIGURED);
        assert_eq!(slot[0] >> 27, 3, "context entries from input");
        assert_eq!(slot[3] & 0xFF, 1, "the address survives");
        let ep = d.out(3);
        assert_eq!(ep_state(&ep), EP_STATE_RUNNING);
        assert_eq!(ep[1], (7 << 3) | (8 << 16));
        assert_eq!(ep[2], 0x9001);
    }

    #[test]
    fn deconfigure_returns_the_slot_to_addressed() {
        let mut d = Driver::new();
        d.configured();
        // Configure Endpoint with DC (dword 3 bit 9).
        let dword3 = d.slot1() | (1 << 9);
        assert_eq!(d.command(TRB_TYPE_CONFIGURE_EP, 0, dword3), TRB_CC_SUCCESS);
        assert_eq!(slot_state(&d.out(0)), SLOT_STATE_ADDRESSED);
        assert_eq!(ep_state(&d.out(3)), EP_STATE_DISABLED);
        assert_eq!(ep_state(&d.out(1)), EP_STATE_RUNNING, "EP0 stays");
    }

    /// Linux stops an endpoint to cancel a transfer. An unknown-command
    /// error there leaves it unable to cancel anything.
    #[test]
    fn stop_endpoint_stops_it_and_a_doorbell_runs_it_again() {
        let mut d = Driver::new();
        d.configured();
        let dword3 = d.slot1() | (3 << 16);
        assert_eq!(d.command(TRB_TYPE_STOP_EP, 0, dword3), TRB_CC_SUCCESS);
        assert_eq!(ep_state(&d.out(3)), EP_STATE_STOPPED);

        // Ring slot 1's doorbell for DCI 3.
        write_reg(&d.ctrl, DBOFF as usize + DB_REG_SIZE, &3u32.to_le_bytes());
        assert_eq!(ep_state(&d.out(3)), EP_STATE_RUNNING);
    }

    /// Linux checks the dequeue pointer in the output context against
    /// the one it asked for.
    #[test]
    fn set_tr_dequeue_records_the_pointer() {
        let mut d = Driver::new();
        d.configured();
        let dword3 = d.slot1() | (3 << 16);
        assert_eq!(d.command(TRB_TYPE_STOP_EP, 0, dword3), TRB_CC_SUCCESS);
        assert_eq!(
            d.command(TRB_TYPE_SET_TR_DEQUEUE, 0xA000 | 1, dword3),
            TRB_CC_SUCCESS
        );
        let ep = d.out(3);
        assert_eq!(ep[2], 0xA001);
        assert_eq!(ep[3], 0);
        assert_eq!(ep_state(&ep), EP_STATE_STOPPED);
    }

    #[test]
    fn reset_endpoint_leaves_it_stopped() {
        let mut d = Driver::new();
        d.configured();
        let dword3 = d.slot1() | (3 << 16);
        assert_eq!(d.command(TRB_TYPE_RESET_EP, 0, dword3), TRB_CC_SUCCESS);
        assert_eq!(ep_state(&d.out(3)), EP_STATE_STOPPED);
    }

    #[test]
    fn evaluate_context_updates_ep0_max_packet_size() {
        let mut d = Driver::new();
        d.addressed();
        d.set_in(0, [0, 1 << 1, 0, 0, 0, 0, 0, 0]);
        d.set_in(2, [0, (4 << 3) | (64 << 16), 0, 0, 0, 0, 0, 0]);
        let slot = d.slot1();
        assert_eq!(
            d.command(TRB_TYPE_EVALUATE_CTX, IN_CTX, slot),
            TRB_CC_SUCCESS
        );
        let ep0 = d.out(1);
        assert_eq!(ep0[1] >> 16, 64);
        assert_eq!(ep0[2], 0x8001, "only the max packet size changes");
        assert_eq!(ep_state(&ep0), EP_STATE_RUNNING);
    }

    /// Point slot 1's DCBAA entry outside guest memory.
    fn lose_the_output_context(d: &Driver) {
        write_ram(
            &d.ctrl,
            DCBAA_GPA + 8,
            &0xFFFF_FFFF_FFFF_F000u64.to_le_bytes(),
        );
    }

    /// A command whose output context the controller cannot write does
    /// not report success: the driver would read back a context that
    /// disagrees with what it was told.
    #[test]
    fn endpoint_commands_on_a_lost_output_context_fail() {
        let mut d = Driver::new();
        d.configured();
        lose_the_output_context(&d);
        let slot = d.slot1();
        let dword3 = slot | (3 << 16);
        d.set_in(0, [0, 1 << 1, 0, 0, 0, 0, 0, 0]);
        d.set_in(2, [0, (4 << 3) | (64 << 16), 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            d.command(TRB_TYPE_EVALUATE_CTX, IN_CTX, slot),
            TRB_CC_PARAMETER_ERROR
        );
        assert_eq!(
            d.command(TRB_TYPE_STOP_EP, 0, dword3),
            TRB_CC_PARAMETER_ERROR
        );
        assert_eq!(
            d.command(TRB_TYPE_RESET_EP, 0, dword3),
            TRB_CC_PARAMETER_ERROR
        );
        let ep1_dequeue = |d: &Driver| {
            d.ctrl
                .state
                .lock()
                .expect("xhci: state lock")
                .ep1_in_ring
                .dequeue
        };
        let before = ep1_dequeue(&d);
        assert_eq!(
            d.command(TRB_TYPE_SET_TR_DEQUEUE, 0xA000 | 1, dword3),
            TRB_CC_PARAMETER_ERROR
        );
        let after = ep1_dequeue(&d);
        assert_eq!(after, before, "the ring stays where the context says");
        assert_eq!(
            d.command(TRB_TYPE_CONFIGURE_EP, 0, slot | CONFIGURE_EP_DC),
            TRB_CC_PARAMETER_ERROR,
            "a deconfigure"
        );
        assert_eq!(d.controller_slot_state(), SlotState::Configured);
    }

    /// Counts warnings.
    struct Warnings(Arc<AtomicUsize>);

    impl slog::Drain for Warnings {
        type Ok = ();
        type Err = slog::Never;

        fn log(
            &self,
            record: &slog::Record<'_>,
            _values: &slog::OwnedKVList,
        ) -> Result<(), slog::Never> {
            if record.level() == slog::Level::Warning {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        }
    }

    /// A failed command reaches the log, so a tablet that never appears
    /// has a cause there; a guest repeating it cannot fill the log.
    #[test]
    fn a_failed_command_is_logged_but_not_without_limit() {
        let warnings = Arc::new(AtomicUsize::new(0));
        let log =
            slog::Logger::root(Warnings(Arc::clone(&warnings)), slog::o!());
        let physmap = PhysMap::new_anon(0, RAM_LEN).expect("guest memory");
        let ctrl = XhciController::new_with_logger(
            Arc::new(physmap),
            Arc::new(MmioBus::new()),
            None,
            log,
        );
        let mut d = Driver::with_ctrl(ctrl);
        d.configured();
        lose_the_output_context(&d);
        let dword3 = d.slot1() | (3 << 16);
        assert_eq!(
            d.command(TRB_TYPE_STOP_EP, 0, dword3),
            TRB_CC_PARAMETER_ERROR
        );
        assert_eq!(warnings.load(Ordering::Relaxed), 1);
        // The test's event ring holds 16, and nothing here wraps it.
        for _ in 0..8 {
            d.command(TRB_TYPE_STOP_EP, 0, dword3);
        }
        let logged = warnings.load(Ordering::Relaxed);
        assert!(
            (1..=FAILURE_LOG_BURST as usize).contains(&logged),
            "{logged}"
        );
    }

    /// The DCBAA entry is guest-written and may point anywhere.
    #[test]
    fn an_output_context_outside_guest_memory_is_not_written() {
        let mut d = Driver::new();
        write_ram(
            &d.ctrl,
            DCBAA_GPA + 8,
            &0xFFFF_FFFF_FFFF_F000u64.to_le_bytes(),
        );
        assert_eq!(
            d.address(),
            TRB_CC_PARAMETER_ERROR,
            "a context the controller cannot write is not a success"
        );
        assert_eq!(slot_state(&d.out(0)), 0, "the real OUT_CTX is untouched");
        assert_eq!(d.controller_slot_state(), SlotState::Enabled);

        d.set_in(0, [0, 0b1001, 0, 0, 0, 0, 0, 0]);
        let slot = d.slot1();
        assert_eq!(
            d.command(TRB_TYPE_CONFIGURE_EP, IN_CTX, slot),
            TRB_CC_PARAMETER_ERROR
        );
        assert_eq!(d.controller_slot_state(), SlotState::Enabled);
    }

    /// An INTx line that remembers its level.
    #[derive(Default)]
    pub(super) struct LevelPin(std::sync::atomic::AtomicBool);

    impl IntrPin for LevelPin {
        fn assert(&self) {
            self.0.store(true, Ordering::SeqCst);
        }

        fn deassert(&self) {
            self.0.store(false, Ordering::SeqCst);
        }

        fn is_asserted(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    /// PCI INTx is level-triggered. A pulse that arrives while the
    /// interrupt controller still waits for the previous EOI is lost, and
    /// with it the only event of a control transfer: Linux timed out
    /// reading the tablet's descriptor ("error -110"). The line has to
    /// stay up until the driver clears USBSTS.EINT.
    #[test]
    fn an_event_holds_intx_until_the_driver_clears_eint() {
        let pin = Arc::new(LevelPin::default());
        let physmap =
            PhysMap::new_anon(0, RAM_LEN).expect("anonymous guest memory");
        let ctrl = XhciController::new(
            Arc::new(physmap),
            Arc::new(MmioBus::new()),
            Some(pin.clone() as Arc<dyn IntrPin>),
        );
        let mut d = Driver::with_ctrl(ctrl);
        write_reg(
            &d.ctrl,
            RTSOFF as usize + IR0_BASE + IR_IMAN,
            &IMAN_IE.to_le_bytes(),
        );
        write_reg(
            &d.ctrl,
            OP_BASE + OP_USBCMD,
            &(USBCMD_RS | USBCMD_INTE).to_le_bytes(),
        );
        // Starting the controller posts the tablet's port event.
        d.events += 1;
        assert!(pin.is_asserted(), "an event raises the line");
        write_reg(&d.ctrl, OP_BASE + OP_USBSTS, &USBSTS_EINT.to_le_bytes());
        assert!(!pin.is_asserted(), "clearing EINT lowers it");

        assert_eq!(d.command(TRB_TYPE_NOOP_CMD, 0, 0), TRB_CC_SUCCESS);
        assert!(pin.is_asserted(), "and the next event raises it again");
    }

    /// A reset clears EINT, so it has to lower the line with it: a line
    /// left high with no event behind it is an interrupt storm the
    /// guest cannot acknowledge.
    #[test]
    fn host_controller_reset_lowers_intx() {
        let pin = Arc::new(LevelPin::default());
        let physmap =
            PhysMap::new_anon(0, RAM_LEN).expect("anonymous guest memory");
        let ctrl = XhciController::new(
            Arc::new(physmap),
            Arc::new(MmioBus::new()),
            Some(pin.clone() as Arc<dyn IntrPin>),
        );
        let d = Driver::with_ctrl(ctrl);
        write_reg(
            &d.ctrl,
            RTSOFF as usize + IR0_BASE + IR_IMAN,
            &IMAN_IE.to_le_bytes(),
        );
        write_reg(
            &d.ctrl,
            OP_BASE + OP_USBCMD,
            &(USBCMD_RS | USBCMD_INTE).to_le_bytes(),
        );
        assert!(pin.is_asserted());

        write_reg(&d.ctrl, OP_BASE + OP_USBCMD, &USBCMD_HCRST.to_le_bytes());
        assert!(!pin.is_asserted());
        assert_eq!(
            read_reg(&d.ctrl, OP_BASE + OP_USBSTS, 4) as u32 & USBSTS_EINT,
            0
        );
    }

    /// Linux disables the slot after a failed enumeration and asks for
    /// a new one. Without Disable Slot the retry got "No Slots
    /// Available".
    #[test]
    fn disable_slot_frees_the_slot_for_enable_slot() {
        let mut d = Driver::new();
        d.addressed();
        let slot = d.slot1();
        assert_eq!(d.command(TRB_TYPE_DISABLE_SLOT, 0, slot), TRB_CC_SUCCESS);
        assert_eq!(d.command(TRB_TYPE_ENABLE_SLOT, 0, 0), TRB_CC_SUCCESS);
    }

    const EP0_RING: u64 = 0x8000;
    const DATA_BUF: u64 = 0xA000;

    /// Queue a GET_DESCRIPTOR(device) control transfer asking for
    /// `w_length` bytes, laid out as Linux does: Setup without IOC, an
    /// IN Data stage with ISP, a Status stage with IOC. Ring EP0.
    fn get_device_descriptor(d: &Driver, w_length: u16) {
        get_device_descriptor_into(d, w_length, DATA_BUF);
    }

    /// As `get_device_descriptor`, into the buffer at `buf`.
    fn get_device_descriptor_into(d: &Driver, w_length: u16, buf: u64) {
        let setup = RawTrb {
            dword0: 0x80 | (6 << 8) | (0x0100 << 16),
            dword1: u32::from(w_length) << 16,
            dword2: 8,
            dword3: (TRB_TYPE_SETUP_STAGE << TRB_TYPE_SHIFT)
                | TRB_SETUP_IDT
                | (3 << 16)
                | TRB_CYCLE,
        };
        let data = RawTrb {
            dword0: buf as u32,
            dword1: (buf >> 32) as u32,
            dword2: u32::from(w_length),
            dword3: (TRB_TYPE_DATA_STAGE << TRB_TYPE_SHIFT)
                | TRB_DATA_DIR_IN
                | TRB_ISP
                | TRB_CYCLE,
        };
        let status = RawTrb {
            dword0: 0,
            dword1: 0,
            dword2: 0,
            dword3: (TRB_TYPE_STATUS_STAGE << TRB_TYPE_SHIFT)
                | TRB_IOC
                | TRB_CYCLE,
        };
        for (i, trb) in [setup, data, status].iter().enumerate() {
            write_ram(
                &d.ctrl,
                EP0_RING + (i * TRB_SIZE) as u64,
                &trb_bytes(trb),
            );
        }
        write_reg(&d.ctrl, DBOFF as usize + DB_REG_SIZE, &1u32.to_le_bytes());
    }

    /// Transfer events after the command completions, as (TRB pointer,
    /// completion code, transfer length), up to the first empty slot.
    fn transfer_events(d: &Driver) -> Vec<(u64, u32, u32)> {
        let mut events = Vec::new();
        for i in d.events..16 {
            let e = d.read_dwords(EVENT_RING_GPA + i * TRB_SIZE as u64, 4);
            if e[3] & TRB_CYCLE == 0 {
                break;
            }
            assert_eq!(
                (e[3] >> TRB_TYPE_SHIFT) & 0x3F,
                TRB_TYPE_TRANSFER_EVENT
            );
            events.push((
                u64::from(e[0]) | (u64::from(e[1]) << 32),
                e[2] >> 24,
                e[2] & 0xFF_FFFF,
            ));
        }
        events
    }

    /// A descriptor the controller cannot write into the driver's buffer
    /// is not reported as sent: the driver would parse what was there.
    #[test]
    fn a_data_stage_into_lost_memory_is_a_data_buffer_error() {
        let mut d = Driver::new();
        d.addressed();
        get_device_descriptor_into(&d, 18, 0xFFFF_FFFF_FFFF_0000);
        let events = transfer_events(&d);
        assert!(
            events.contains(&(
                EP0_RING + TRB_SIZE as u64,
                TRB_CC_DATA_BUFFER_ERROR,
                18
            )),
            "{events:?}"
        );
    }

    /// The same for a tablet report.
    #[test]
    fn a_report_into_lost_memory_is_a_data_buffer_error() {
        const EP1_RING: u64 = 0x9000;
        let mut d = Driver::new();
        d.configured();
        let normal = RawTrb {
            dword0: 0xFFFF_0000,
            dword1: 0xFFFF_FFFF,
            dword2: 6,
            dword3: (TRB_TYPE_NORMAL << TRB_TYPE_SHIFT) | TRB_IOC | TRB_CYCLE,
        };
        write_ram(&d.ctrl, EP1_RING, &trb_bytes(&normal));
        {
            let mut st = d.ctrl.state.lock().expect("xhci: state lock");
            st.ep1_in_ring = TransferRing {
                base: EP1_RING,
                dequeue: EP1_RING,
                ccs: true,
            };
            st.ep1_in_has_trbs = true;
        }
        d.ctrl.tablet().pointer_event(0b001, 0x1234, 0x0567);
        d.ctrl.notify_pointer_event();
        let events = transfer_events(&d);
        assert_eq!(events, [(EP1_RING, TRB_CC_DATA_BUFFER_ERROR, 6)]);
    }

    /// xHCI 1.2 section 4.10.4: a Transfer Event is posted for a TRB
    /// with IOC, for a short packet on a TRB with ISP, or on an error.
    /// An event for the Setup stage made Linux warn "Success on ctrl
    /// setup TRB without IOC set?".
    #[test]
    fn a_control_transfer_posts_one_event_for_its_status_stage() {
        let mut d = Driver::new();
        d.addressed();
        get_device_descriptor(&d, 18);

        assert_eq!(
            transfer_events(&d),
            [(EP0_RING + 2 * TRB_SIZE as u64, TRB_CC_SUCCESS, 0)]
        );
        let desc = d.read_dwords(DATA_BUF, 1);
        assert_eq!(desc[0] & 0xFF, 18, "bLength");
    }

    /// Linux reads the TR Dequeue Pointer in the output context to find
    /// where the controller stopped, and moves past a stalled transfer
    /// from there. Left at the ring start it walked a whole lap and sent
    /// Set TR Dequeue to the start with DCS 0, where nothing was queued,
    /// and the next request timed out.
    #[test]
    fn the_output_context_follows_the_ep0_dequeue_pointer() {
        let mut d = Driver::new();
        d.addressed();
        get_device_descriptor(&d, 18);
        let ep0 = d.out(1);
        assert_eq!(ep0[2], (EP0_RING + 3 * TRB_SIZE as u64) as u32 | 1);
        assert_eq!(ep0[3], 0);
    }

    #[test]
    fn a_short_data_stage_with_isp_posts_its_own_event() {
        let mut d = Driver::new();
        d.addressed();
        get_device_descriptor(&d, 64);

        assert_eq!(
            transfer_events(&d),
            [
                (EP0_RING + TRB_SIZE as u64, TRB_CC_SHORT_PACKET, 64 - 18),
                (EP0_RING + 2 * TRB_SIZE as u64, TRB_CC_SUCCESS, 0),
            ]
        );
    }

    const ERSTBA_OFF: usize = RTSOFF as usize + IR0_BASE + IR_ERSTBA;
    const ERSTSZ_OFF: usize = RTSOFF as usize + IR0_BASE + IR_ERSTSZ;
    const ERDP_OFF: usize = RTSOFF as usize + IR0_BASE + IR_ERDP;

    #[test]
    fn a_qword_erstba_write_sets_up_the_event_ring() {
        // The illumos xhci driver writes ERSTBA with one ddi_put64, so
        // a device that only latches the high dword never gets a ring.
        let ctrl = test_controller_with_ram();
        write_erst_entry(&ctrl, 0x2000, 0x3000, 16);
        write_reg(&ctrl, ERSTSZ_OFF, &1u32.to_le_bytes());
        write_reg(&ctrl, ERSTBA_OFF, &0x2000u64.to_le_bytes());

        let st = ctrl.state.lock().expect("xhci: state lock");
        assert_eq!(st.erstba, 0x2000);
        assert_eq!(st.event_ring_base, 0x3000);
        assert_eq!(st.event_ring_size, 16);
    }

    #[test]
    fn split_dword_erstba_writes_still_set_up_the_event_ring() {
        // Linux writes the two halves low first; both orders have to
        // end with a configured ring.
        for high_first in [false, true] {
            let ctrl = test_controller_with_ram();
            write_erst_entry(&ctrl, 0x2000, 0x3000, 16);
            write_reg(&ctrl, ERSTSZ_OFF, &1u32.to_le_bytes());
            let halves: [(usize, u32); 2] =
                [(ERSTBA_OFF, 0x2000), (ERSTBA_OFF + 4, 0)];
            let order = if high_first { [1, 0] } else { [0, 1] };
            for i in order {
                write_reg(&ctrl, halves[i].0, &halves[i].1.to_le_bytes());
            }

            let st = ctrl.state.lock().expect("xhci: state lock");
            assert_eq!(st.event_ring_base, 0x3000, "high_first={high_first}");
            assert_eq!(st.event_ring_size, 16, "high_first={high_first}");
        }
    }

    #[test]
    fn qword_writes_keep_the_upper_dword_of_every_64_bit_register() {
        let ctrl = test_controller_with_ram();
        let crcr = 0x0000_0001_2345_6740u64;
        let dcbaap = 0x0000_0002_3456_7800u64;
        let erdp = 0x0000_0003_4567_8900u64;
        write_reg(&ctrl, OP_BASE + OP_CRCR, &(crcr | CRCR_RCS).to_le_bytes());
        write_reg(&ctrl, OP_BASE + OP_DCBAAP, &dcbaap.to_le_bytes());
        write_reg(&ctrl, ERDP_OFF, &erdp.to_le_bytes());

        let st = ctrl.state.lock().expect("xhci: state lock");
        assert_eq!(st.cmd_ring_base, crcr);
        assert_eq!(st.cmd_ring_dequeue, crcr);
        assert!(st.cmd_ring_ccs);
        assert_eq!(st.dcbaap, dcbaap);
        assert_eq!(st.erdp, erdp);
    }

    #[test]
    fn qword_reads_answer_with_all_eight_bytes() {
        let ctrl = test_controller_with_ram();
        let dcbaap = 0x0000_0002_3456_7800u64;
        write_reg(&ctrl, OP_BASE + OP_DCBAAP, &dcbaap.to_le_bytes());
        assert_eq!(read_reg(&ctrl, OP_BASE + OP_DCBAAP, 8), dcbaap);

        write_erst_entry(&ctrl, 0x2000, 0x3000, 16);
        write_reg(&ctrl, ERSTSZ_OFF, &1u32.to_le_bytes());
        write_reg(&ctrl, ERSTBA_OFF, &0x2000u64.to_le_bytes());
        assert_eq!(read_reg(&ctrl, ERSTBA_OFF, 8), 0x2000);
    }

    #[test]
    fn erstsz_is_clamped_to_the_one_segment_we_advertise() {
        // Only ERST entry 0 is ever read, so a larger table would lose
        // every event posted into the segments we never walk.
        assert_eq!((hcsparams2() >> 4) & 0xF, 0, "ERST Max = 2^0 = 1");

        let ctrl = test_controller_with_ram();
        write_reg(&ctrl, ERSTSZ_OFF, &0xFFFFu32.to_le_bytes());
        assert_eq!(ctrl.state.lock().expect("xhci: state lock").erstsz, 1);
    }

    #[test]
    fn initial_state_is_halted() {
        let st = test_state();
        assert_ne!(st.usbsts & USBSTS_HCH, 0);
        assert_eq!(st.usbcmd & USBCMD_RS, 0);
    }

    #[test]
    fn slot_starts_disabled() {
        let st = test_state();
        assert_eq!(st.slot_state, SlotState::Disabled);
    }

    #[test]
    fn enable_slot_twice_reports_no_slots() {
        let mut st = test_state();
        assert_eq!(st.enable_slot(), (TRB_CC_SUCCESS, 1));
        assert_eq!(st.enable_slot(), (TRB_CC_NO_SLOTS_AVAILABLE, 0));
    }

    /// xHCI 1.2 section 6.4.2.3: the Port ID is dword 0 bits 31:24. In
    /// dword 3 it lands in reserved bits and Linux reads port 0, logs
    /// "Port change event with invalid port ID 0" and drops the event.
    #[test]
    fn port_status_change_event_carries_the_port_id_in_dword0() {
        let trb = port_status_change_trb(2);
        assert_eq!(trb.dword0 >> 24, 2);
        assert_eq!(trb.dword0 & 0x00FF_FFFF, 0);
        assert_eq!(trb.dword2 >> 24, TRB_CC_SUCCESS);
        assert_eq!(trb.trb_type(), TRB_TYPE_PORT_STATUS_CHANGE);
        assert_eq!(trb.dword3 >> 16, 0, "dword 3 bits 31:16 are reserved");
    }

    #[test]
    fn port2_connected_at_superspeed() {
        let st = test_state();
        assert_ne!(st.port2_sc & PORTSC_CCS, 0);
        let speed = (st.port2_sc >> PORTSC_SPEED_SHIFT) & PORTSC_SPEED_MASK;
        assert_eq!(speed, SPEED_SUPER);
    }

    #[test]
    fn port1_not_connected() {
        let st = test_state();
        assert_eq!(st.port1_sc & PORTSC_CCS, 0);
    }

    #[test]
    fn cap_caplength_hciversion() {
        let val = u32::from(CAPLENGTH) | (u32::from(HCIVERSION) << 16);
        assert_eq!(val & 0xFF, 0x20);
        assert_eq!((val >> 16) & 0xFFFF, 0x0100);
    }

    #[test]
    fn cap_hcsparams1() {
        let val = hcsparams1();
        // MaxSlots = 64
        assert_eq!(val & 0xFF, 64);
        // MaxIntrs = 1
        assert_eq!((val >> 8) & 0x3FF, 1);
        // MaxPorts = 2
        assert_eq!((val >> 24) & 0xFF, 2);
    }

    #[test]
    fn cap_hccparams1_ac64() {
        let val = hccparams1();
        assert_ne!(val & 1, 0);
    }

    #[test]
    fn cap_dboff_rtsoff() {
        assert_eq!(DBOFF, 0x440);
        assert_eq!(RTSOFF, 0x560);
    }

    #[test]
    fn config_max_slots_is_clamped() {
        let ctrl = test_controller();
        let wo = WriteOp::from_buf(&u32::MAX.to_le_bytes());
        ctrl.bar0_rw(OP_BASE + OP_CONFIG, RWOp::Write(&wo));

        let st = ctrl.state.lock().expect("xhci: state lock");
        assert_eq!(st.config, u32::from(MAX_SLOTS));
    }

    #[test]
    fn doorbell_beyond_max_slots_is_ignored() {
        let ctrl = test_controller();
        let original_erstba = 0x1122_3344_5566_7780;
        ctrl.state.lock().expect("xhci: state lock").erstba = original_erstba;

        let wo = WriteOp::from_buf(&20u32.to_le_bytes());
        ctrl.bar0_rw(DBOFF as usize + 20 * DB_REG_SIZE, RWOp::Write(&wo));
        assert_eq!(
            ctrl.state.lock().expect("xhci: state lock").erstba,
            original_erstba,
        );

        // IR0_BASE means the old layout's ERSTBA collision was slot 28.
        let wo = WriteOp::from_buf(&28u32.to_le_bytes());
        ctrl.bar0_rw(DBOFF as usize + 28 * DB_REG_SIZE, RWOp::Write(&wo));
        assert_eq!(
            ctrl.state.lock().expect("xhci: state lock").erstba,
            original_erstba,
        );

        let beyond_max = usize::from(MAX_SLOTS) + 1;
        let wo = WriteOp::from_buf(&(beyond_max as u32).to_le_bytes());
        ctrl.bar0_rw(
            DBOFF as usize + beyond_max * DB_REG_SIZE,
            RWOp::Write(&wo),
        );
        assert_eq!(
            ctrl.state.lock().expect("xhci: state lock").erstba,
            original_erstba,
        );
    }

    #[test]
    fn trb_type_values() {
        assert_eq!(TRB_TYPE_ENABLE_SLOT, 9);
        assert_eq!(TRB_TYPE_ADDRESS_DEVICE, 11);
        assert_eq!(TRB_TYPE_CONFIGURE_EP, 12);
        assert_eq!(TRB_TYPE_NOOP_CMD, 23);
        assert_eq!(TRB_TYPE_CMD_COMPLETION, 33);
        assert_eq!(TRB_TYPE_PORT_STATUS_CHANGE, 34);
    }

    #[test]
    fn raw_trb_parsing() {
        let trb = RawTrb {
            dword0: 0x1000,
            dword1: 0,
            dword2: (TRB_CC_SUCCESS << 24) | 64,
            dword3: (TRB_TYPE_TRANSFER_EVENT << TRB_TYPE_SHIFT)
                | (1u32 << 24)
                | TRB_CYCLE,
        };
        assert_eq!(trb.trb_type(), TRB_TYPE_TRANSFER_EVENT);
        assert!(trb.cycle_bit());
        assert_eq!(trb.slot_id(), 1);
        assert_eq!(trb.parameter(), 0x1000);
        assert_eq!(trb.completion_code(), TRB_CC_SUCCESS);
        assert_eq!(trb.transfer_length(), 64);
    }

    #[test]
    fn event_ring_pcs_toggles_on_wrap() {
        let mut st = test_state();
        st.event_ring_base = 0x1000;
        st.event_ring_size = 2;
        st.event_ring_pcs = true;

        // After enqueue 0 -> 1
        st.event_ring_enqueue = 1;
        // Simulating the wrap logic
        st.event_ring_enqueue += 1;
        if st.event_ring_enqueue >= st.event_ring_size {
            st.event_ring_enqueue = 0;
            st.event_ring_pcs = !st.event_ring_pcs;
        }
        assert_eq!(st.event_ring_enqueue, 0);
        assert!(!st.event_ring_pcs);
    }

    #[test]
    fn transfer_ring_initial_state() {
        let ring = TransferRing::new();
        assert_eq!(ring.base, 0);
        assert_eq!(ring.dequeue, 0);
        assert!(ring.ccs);
    }

    #[test]
    fn xcap_usb2_cap_id() {
        let cap = xcap_usb2();
        assert_eq!(cap[0] & 0xFF, 2);
    }

    #[test]
    fn xcap_usb3_is_last() {
        let cap = xcap_usb3();
        assert_eq!((cap[0] >> 8) & 0xFF, 0);
    }

    #[test]
    fn crcr_ptr_mask() {
        let addr: u64 = 0x0000_1234_5678_9ABC;
        let masked = addr & CRCR_PTR_MASK;
        assert_eq!(masked & 0x3F, 0);
        assert_eq!(masked, 0x0000_1234_5678_9A80);
    }

    #[test]
    fn bar0_size_is_8k() {
        assert_eq!(BAR0_SIZE, 0x2000);
        assert!(BAR0_SIZE.is_power_of_two());
    }

    #[test]
    fn portsc_change_bits_are_w1c() {
        let mut portsc: u32 = PORTSC_CCS | PORTSC_PP | PORTSC_CSC;
        // Write 1 to CSC to clear it
        portsc &= !(PORTSC_CSC);
        assert_eq!(portsc & PORTSC_CSC, 0);
        // CCS and PP should remain
        assert_ne!(portsc & PORTSC_CCS, 0);
        assert_ne!(portsc & PORTSC_PP, 0);
    }
}
