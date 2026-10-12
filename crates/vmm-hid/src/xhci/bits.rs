// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! xHCI register offsets, TRB definitions, and USB protocol constants.
//!
//! Covers the minimal subset needed for a single-device (tablet) xHCI
//! controller: capability/operational/runtime/doorbell registers, TRB
//! types, port status bits, and extended capability structures.

// ---- PCI identity --------------------------------------------------------

pub use vmm_devices::pci::bits::PCI_VENDOR_INTEL;
pub const PCI_DEVICE_XHCI_PPT: u16 = 0x1E31; // Panther Point xHCI
pub const PCI_CLASS_SERIAL: u8 = 0x0C;
pub const PCI_SUBCLASS_USB: u8 = 0x03;
pub const PCI_PROGIF_XHCI: u8 = 0x30;

// ---- BAR0 size -----------------------------------------------------------

/// 8 KiB MMIO BAR for all xHCI registers.
pub const BAR0_SIZE: u32 = 0x2000;

// ---- Capability register offsets (0x00..0x1F) ----------------------------

/// CAPLENGTH + HCIVERSION (dword at 0x00).
/// Low byte = capability length (0x20), high word = HCI version (0x0100).
pub const CAPLENGTH: u8 = 0x20;
pub const HCIVERSION: u16 = 0x0100;

/// Windows xHCI drivers require the controller to advertise at least 64 slots.
pub const MAX_SLOTS: u8 = 64;

/// Structural Parameters 1 (HCSPARAMS1) at offset 0x04.
/// Bits [7:0]   = MaxSlots (64)
/// Bits [17:8]  = MaxIntrs (1)
/// Bits [31:24] = MaxPorts (2)
pub fn hcsparams1() -> u32 {
    let max_slots = u32::from(MAX_SLOTS);
    let max_intrs: u32 = 1;
    let max_ports: u32 = 2;
    max_slots | (max_intrs << 8) | (max_ports << 24)
}

/// Structural Parameters 2 (HCSPARAMS2) at offset 0x08.
/// Bits [3:0] = IST (0 = no isochronous scheduling threshold)
/// Bits [7:4] = ERST max (0 => 2^0 = 1 entry)
/// Bits [25:21] = Max Scratchpad Bufs Hi = 0
/// Bit  [26]    = SPR = 0
/// Bits [31:27] = Max Scratchpad Bufs Lo = 0
///
/// `setup_event_ring` reads ERST entry 0 and treats the ring as one
/// segment, so advertising more would lose every event that falls in a
/// segment the controller never walks.
pub const fn hcsparams2() -> u32 {
    0
}

/// Structural Parameters 3 (HCSPARAMS3) at offset 0x0C.
/// Bits [7:0]  = U1 device exit latency (0)
/// Bits [31:16] = U2 device exit latency (0)
pub const HCSPARAMS3: u32 = 0;

/// Capability Parameters 1 (HCCPARAMS1) at offset 0x10.
/// Bit  [0] = AC64 (64-bit addressing capable)
/// Bit  [1] = BNC (bandwidth negotiation capability) = 0
/// Bit  [2] = CSZ (context size) = 0 => 32-byte contexts
/// Bit  [3] = PPC (port power control) = 0
/// Bit  [4] = PIND (port indicators) = 0
/// Bit  [5] = LHRC (light host controller reset) = 0
/// Bit  [6] = LTC (latency tolerance messaging) = 0
/// Bit  [7] = NSS (no secondary SID support) = 0
/// Bits [15:12] = xECP (extended capabilities pointer, in dwords from BAR)
///   We place extended caps at XCAP_BASE.
pub fn hccparams1() -> u32 {
    let ac64: u32 = 1;
    let xecp: u32 = XCAP_BASE as u32 / 4;
    ac64 | (xecp << 16)
}

/// Doorbell Array Offset (DBOFF) at offset 0x14.
pub const DBOFF: u32 = 0x440;

/// Runtime Register Space Offset (RTSOFF) at offset 0x18.
pub const RTSOFF: u32 = 0x560;

/// Capability Parameters 2 (HCCPARAMS2) at offset 0x1C.
pub const HCCPARAMS2: u32 = 0;

// ---- Operational register offsets (base = CAPLENGTH = 0x20) ---------------

/// Base of operational registers.
pub const OP_BASE: usize = CAPLENGTH as usize;

/// USBCMD - USB Command Register.
pub const OP_USBCMD: usize = 0x00;
/// USBSTS - USB Status Register.
pub const OP_USBSTS: usize = 0x04;
/// PAGESIZE register.
pub const OP_PAGESIZE: usize = 0x08;
/// DNCTRL - Device Notification Control.
pub const OP_DNCTRL: usize = 0x14;
/// CRCR - Command Ring Control Register (64-bit).
pub const OP_CRCR: usize = 0x18;
/// DCBAAP - Device Context Base Address Array Pointer (64-bit).
pub const OP_DCBAAP: usize = 0x30;
/// CONFIG - Configure Register.
pub const OP_CONFIG: usize = 0x38;

// USBCMD bits
pub const USBCMD_RS: u32 = 1 << 0; // Run/Stop
pub const USBCMD_HCRST: u32 = 1 << 1; // Host Controller Reset
pub const USBCMD_INTE: u32 = 1 << 2; // Interrupter Enable
pub const USBCMD_HSEE: u32 = 1 << 3; // Host System Error Enable

// USBSTS bits
pub const USBSTS_HCH: u32 = 1 << 0; // HCHalted
pub const USBSTS_EINT: u32 = 1 << 3; // Event Interrupt
pub const USBSTS_PCD: u32 = 1 << 4; // Port Change Detect
pub const USBSTS_CNR: u32 = 1 << 11; // Controller Not Ready

// CRCR bits
pub const CRCR_RCS: u64 = 1 << 0; // Ring Cycle State
pub const CRCR_CS: u64 = 1 << 1; // Command Stop
pub const CRCR_CA: u64 = 1 << 2; // Command Abort
pub const CRCR_CRR: u64 = 1 << 3; // Command Ring Running
pub const CRCR_PTR_MASK: u64 = !0x3F; // Pointer mask (64-byte aligned)

/// ERSTBA pointer mask (64-byte aligned).
pub const ERSTBA_PTR_MASK: u64 = !0x3F;
/// ERDP pointer mask: bits 3:0 are DESI and the write-1-to-clear EHB.
pub const ERDP_PTR_MASK: u64 = !0xF;

// ---- Port Status Register offsets (relative to OP_BASE) -------------------

/// Base of port register sets (each port = 16 bytes).
pub const PORTSC_BASE: usize = 0x400;

/// Port register offsets within each 16-byte port set.
pub const PORTSC_PORTSC: usize = 0x00;
pub const PORTSC_PORTPMSC: usize = 0x04;
pub const PORTSC_PORTLI: usize = 0x08;
pub const PORTSC_PORTHLPMC: usize = 0x0C;

/// Size of each port register set.
pub const PORT_REG_SIZE: usize = 16;

// PORTSC bits
pub const PORTSC_CCS: u32 = 1 << 0; // Current Connect Status
pub const PORTSC_PED: u32 = 1 << 1; // Port Enabled/Disabled
pub const PORTSC_PR: u32 = 1 << 4; // Port Reset
pub const PORTSC_PLS_SHIFT: u32 = 5; // Port Link State (bits 8:5)
pub const PORTSC_PLS_MASK: u32 = 0xF;
pub const PORTSC_PP: u32 = 1 << 9; // Port Power
pub const PORTSC_SPEED_SHIFT: u32 = 10; // Port Speed (bits 13:10)
pub const PORTSC_SPEED_MASK: u32 = 0xF;
pub const PORTSC_LWS: u32 = 1 << 16; // Port Link State Write Strobe
pub const PORTSC_CSC: u32 = 1 << 17; // Connect Status Change
pub const PORTSC_PRC: u32 = 1 << 21; // Port Reset Change
pub const PORTSC_WRC: u32 = 1 << 19; // Warm Port Reset Change

// Port Link State values
pub const PLS_U0: u32 = 0; // U0 (active)
pub const PLS_RXDETECT: u32 = 5; // RxDetect
pub const PLS_DISABLED: u32 = 4; // Disabled

// Port Speed values
pub const SPEED_NONE: u32 = 0;
pub const SPEED_FULL: u32 = 1; // Full-speed (12 Mbps)
pub const SPEED_HIGH: u32 = 3; // High-speed (480 Mbps)
pub const SPEED_SUPER: u32 = 4; // SuperSpeed (5 Gbps)

// ---- Doorbell register offsets (base = DBOFF = 0x440) ---------------------

/// Size of each doorbell register (4 bytes).
pub const DB_REG_SIZE: usize = 4;

// ---- Runtime register offsets (base = RTSOFF = 0x560) ---------------------

/// Microframe Index Register.
pub const RT_MFINDEX: usize = 0x00;

/// Interrupter register set offset (from runtime base).
/// Interrupter 0 starts at runtime_base + 0x20.
pub const IR0_BASE: usize = 0x20;

/// Interrupter Management Register (IMAN).
pub const IR_IMAN: usize = 0x00;
/// Interrupter Moderation Register (IMOD).
pub const IR_IMOD: usize = 0x04;
/// Event Ring Segment Table Size (ERSTSZ).
pub const IR_ERSTSZ: usize = 0x08;
/// Event Ring Segment Table Base Address (ERSTBA, 64-bit).
pub const IR_ERSTBA: usize = 0x10;
/// Event Ring Dequeue Pointer (ERDP, 64-bit).
pub const IR_ERDP: usize = 0x18;

// IMAN bits
pub const IMAN_IP: u32 = 1 << 0; // Interrupt Pending
pub const IMAN_IE: u32 = 1 << 1; // Interrupt Enable

// ---- Extended Capability structures (base = 0x600) -----------------------

/// Offset of first extended capability (USB2 supported protocol).
pub const XCAP_BASE: usize = 0x600;

/// Size of a Supported Protocol capability (16 bytes).
pub const XCAP_PROTO_SIZE: usize = 16;

/// Build a Supported Protocol extended capability (xHCI 1.2 section 7.2).
///
/// - dword 0: ID 2, `next` (in dwords, 0 = last), minor revision 0,
///   `major` revision
/// - dword 1: name string "USB "
/// - dword 2: compatible port offset `port`, port count 1, no protocol
///   speed IDs (the default speeds for the revision apply)
/// - dword 3: protocol slot type 0
fn xcap_protocol(next: u32, major: u32, port: u32) -> [u32; 4] {
    let id_next_rev: u32 = 0x02 | (next << 8) | (major << 24);
    let name = u32::from_le_bytes(*b"USB ");
    let ports: u32 = port | (1 << 8);
    [id_next_rev, name, ports, 0]
}

/// USB 2.0 on port 1; the next capability is USB3's, 16 bytes on.
pub fn xcap_usb2() -> [u32; 4] {
    xcap_protocol(XCAP_PROTO_SIZE as u32 / 4, 2, 1)
}

/// USB 3.0 on port 2; the last capability.
pub fn xcap_usb3() -> [u32; 4] {
    xcap_protocol(0, 3, 2)
}

// ---- TRB (Transfer Request Block) definitions ----------------------------

/// Size of a TRB in bytes.
pub const TRB_SIZE: usize = 16;

// TRB type field location in dword 3
pub const TRB_TYPE_SHIFT: u32 = 10;
pub const TRB_TYPE_MASK: u32 = 0x3F;

// Cycle bit (bit 0 of dword 3)
pub const TRB_CYCLE: u32 = 1 << 0;
// Interrupt on Short Packet (bit 2 of dword 3, transfer TRBs)
pub const TRB_ISP: u32 = 1 << 2;
// Interrupt On Completion (bit 5 of dword 3, transfer TRBs)
pub const TRB_IOC: u32 = 1 << 5;

// ---- TRB types: Transfer ring ----

pub const TRB_TYPE_NORMAL: u32 = 1;
pub const TRB_TYPE_SETUP_STAGE: u32 = 2;
pub const TRB_TYPE_DATA_STAGE: u32 = 3;
pub const TRB_TYPE_STATUS_STAGE: u32 = 4;
pub const TRB_TYPE_LINK: u32 = 6;

// ---- TRB types: Command ring ----

pub const TRB_TYPE_ENABLE_SLOT: u32 = 9;
pub const TRB_TYPE_DISABLE_SLOT: u32 = 10;
pub const TRB_TYPE_ADDRESS_DEVICE: u32 = 11;
pub const TRB_TYPE_CONFIGURE_EP: u32 = 12;
pub const TRB_TYPE_EVALUATE_CTX: u32 = 13;
pub const TRB_TYPE_RESET_EP: u32 = 14;
pub const TRB_TYPE_STOP_EP: u32 = 15;
pub const TRB_TYPE_SET_TR_DEQUEUE: u32 = 16;
pub const TRB_TYPE_NOOP_CMD: u32 = 23;

// ---- TRB types: Event ring ----

pub const TRB_TYPE_TRANSFER_EVENT: u32 = 32;
pub const TRB_TYPE_CMD_COMPLETION: u32 = 33;
pub const TRB_TYPE_PORT_STATUS_CHANGE: u32 = 34;

// ---- TRB completion codes ----

pub const TRB_CC_INVALID: u32 = 0;
pub const TRB_CC_SUCCESS: u32 = 1;
pub const TRB_CC_DATA_BUFFER_ERROR: u32 = 2;
pub const TRB_CC_NO_SLOTS_AVAILABLE: u32 = 9;
pub const TRB_CC_SHORT_PACKET: u32 = 13;
pub const TRB_CC_TRB_ERROR: u32 = 5;
pub const TRB_CC_STALL: u32 = 6;
pub const TRB_CC_SLOT_NOT_ENABLED: u32 = 11;
pub const TRB_CC_PARAMETER_ERROR: u32 = 17;

// Setup Stage TRB fields
pub const TRB_SETUP_IDT: u32 = 1 << 6; // Immediate Data in TRB
pub const TRB_SETUP_TRT_SHIFT: u32 = 16; // Transfer Type (bits 17:16 of dword3)
pub const TRB_SETUP_TRT_NO_DATA: u32 = 0;
pub const TRB_SETUP_TRT_IN: u32 = 3;
pub const TRB_SETUP_TRT_OUT: u32 = 2;

// Data Stage TRB fields
pub const TRB_DATA_DIR_IN: u32 = 1 << 16; // Direction: 1 = IN (device to host)

// ---- TRB raw structure ---------------------------------------------------

/// Raw 16-byte TRB as read from guest memory.
///
/// All fields are little-endian u32 matching the hardware layout.
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
pub struct RawTrb {
    pub dword0: u32,
    pub dword1: u32,
    pub dword2: u32,
    pub dword3: u32,
}

impl RawTrb {
    /// Extract the TRB type from dword3.
    pub fn trb_type(&self) -> u32 {
        (self.dword3 >> TRB_TYPE_SHIFT) & TRB_TYPE_MASK
    }

    /// Check the cycle bit in dword3.
    pub fn cycle_bit(&self) -> bool {
        (self.dword3 & TRB_CYCLE) != 0
    }

    /// Extract slot ID from dword3 bits [31:24].
    pub fn slot_id(&self) -> u8 {
        (self.dword3 >> 24) as u8
    }

    /// 64-bit pointer from dword0 + dword1.
    pub fn parameter(&self) -> u64 {
        u64::from(self.dword0) | (u64::from(self.dword1) << 32)
    }

    /// Transfer length from dword2 bits [16:0].
    pub fn transfer_length(&self) -> u32 {
        self.dword2 & 0x1FFFF
    }

    /// Completion code from dword2 bits [31:24] (for event TRBs).
    pub fn completion_code(&self) -> u32 {
        (self.dword2 >> 24) & 0xFF
    }
}

/// Event Ring Segment Table Entry (16 bytes).
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
pub struct ErstEntry {
    /// Ring Segment Base Address (64-bit, 64-byte aligned).
    pub base_lo: u32,
    pub base_hi: u32,
    /// Ring Segment Size (number of TRBs).
    pub size: u32,
    /// Reserved.
    pub rsvd: u32,
}

// ---- USB standard request codes ------------------------------------------

pub const USB_REQ_GET_STATUS: u8 = 0;
pub const USB_REQ_CLEAR_FEATURE: u8 = 1;
pub const USB_REQ_SET_FEATURE: u8 = 3;
pub const USB_REQ_SET_ADDRESS: u8 = 5;
pub const USB_REQ_GET_DESCRIPTOR: u8 = 6;
pub const USB_REQ_SET_DESCRIPTOR: u8 = 7;
pub const USB_REQ_GET_CONFIGURATION: u8 = 8;
pub const USB_REQ_SET_CONFIGURATION: u8 = 9;

// HID class requests
pub const USB_REQ_HID_GET_REPORT: u8 = 1;
pub const USB_REQ_HID_GET_IDLE: u8 = 2;
pub const USB_REQ_HID_SET_IDLE: u8 = 10;
pub const USB_REQ_HID_SET_PROTOCOL: u8 = 11;

// USB descriptor types
pub const USB_DT_DEVICE: u8 = 1;
pub const USB_DT_CONFIG: u8 = 2;
pub const USB_DT_STRING: u8 = 3;
pub const USB_DT_INTERFACE: u8 = 4;
pub const USB_DT_ENDPOINT: u8 = 5;
pub const USB_DT_HID: u8 = 0x21;
pub const USB_DT_HID_REPORT: u8 = 0x22;
pub const USB_DT_BOS: u8 = 0x0F;
pub const USB_DT_DEVICE_CAPABILITY: u8 = 0x10;
pub const USB_DT_SS_ENDPOINT_COMP: u8 = 0x30;

// USB request type direction bit
pub const USB_DIR_IN: u8 = 0x80;
pub const USB_DIR_OUT: u8 = 0x00;
pub const USB_TYPE_STANDARD: u8 = 0x00;
pub const USB_TYPE_CLASS: u8 = 0x20;
pub const USB_RECIP_DEVICE: u8 = 0x00;
pub const USB_RECIP_INTERFACE: u8 = 0x01;

// ---- xHCI slot/endpoint context ------------------------------------------

/// Device Context size: slot context (32 bytes) + 31 endpoint contexts.
/// We only use slot context + EP0 + EP1 IN = 3 * 32 = 96 bytes.
pub const SLOT_CTX_SIZE: usize = 32;
pub const EP_CTX_SIZE: usize = 32;

/// Input context has an additional Input Control Context (32 bytes) prepended.
pub const INPUT_CTRL_CTX_SIZE: usize = 32;

/// Every context in a Device or Input Context is 32 bytes (HCCPARAMS1
/// CSZ = 0).
pub const CTX_SIZE: usize = 32;

/// Configure Endpoint Command TRB dword 3 bit 9: Deconfigure.
pub const CONFIGURE_EP_DC: u32 = 1 << 9;

// Slot Context dword 3: Device Address (bits 7:0), Slot State (bits 31:27)
pub const SLOT_STATE_SHIFT: u32 = 27;
pub const SLOT_STATE_DISABLED: u32 = 0;
pub const SLOT_STATE_DEFAULT: u32 = 1;
pub const SLOT_STATE_ADDRESSED: u32 = 2;
pub const SLOT_STATE_CONFIGURED: u32 = 3;

// Endpoint Context dword 0: EP State (bits 2:0)
pub const EP_STATE_MASK: u32 = 0x7;
pub const EP_STATE_DISABLED: u32 = 0;
pub const EP_STATE_RUNNING: u32 = 1;
pub const EP_STATE_STOPPED: u32 = 3;

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    #[test]
    fn trb_size_is_16() {
        assert_eq!(size_of::<RawTrb>(), TRB_SIZE);
    }

    #[test]
    fn erst_entry_size_is_16() {
        assert_eq!(size_of::<ErstEntry>(), 16);
    }

    #[test]
    fn hcsparams1_fields() {
        let val = hcsparams1();
        assert_eq!(val & 0xFF, 64); // MaxSlots = 64
        assert_eq!((val >> 8) & 0x3FF, 1); // MaxIntrs = 1
        assert_eq!((val >> 24) & 0xFF, 2); // MaxPorts = 2
    }

    #[test]
    fn hccparams1_ac64_set() {
        let val = hccparams1();
        assert_ne!(val & 1, 0); // AC64 = 1
    }

    #[test]
    fn hccparams1_xecp_points_to_xcap_base() {
        let val = hccparams1();
        let xecp = (val >> 16) & 0xFFFF;
        assert_eq!(xecp * 4, XCAP_BASE as u32);
    }

    /// The Supported Protocol layout of xHCI 1.2 section 7.2: revision
    /// in the top half of dword 0, the name in dword 1, the port range
    /// in dword 2 and the slot type in dword 3. Linux reads exactly
    /// these fields; with the revision and ports anywhere else it finds
    /// no root hub ports and refuses the controller.
    fn assert_protocol(cap: [u32; 4], next: u32, major: u32, first_port: u32) {
        // Capability ID 2, Supported Protocol.
        assert_eq!(cap[0] & 0xFF, 2);
        assert_eq!((cap[0] >> 8) & 0xFF, next, "next pointer");
        assert_eq!((cap[0] >> 16) & 0xFF, 0, "minor revision");
        assert_eq!(cap[0] >> 24, major, "major revision");
        assert_eq!(cap[1], u32::from_le_bytes(*b"USB "), "name string");
        assert_eq!(cap[2] & 0xFF, first_port, "compatible port offset");
        assert_eq!((cap[2] >> 8) & 0xFF, 1, "compatible port count");
        assert_eq!(cap[2] >> 28, 0, "no speed IDs: the defaults apply");
        assert_eq!(cap[3] & 0x1F, 0, "protocol slot type");
    }

    #[test]
    fn xcap_usb2_fields() {
        // Next = 4 dwords, to the USB3 capability. Port 1.
        assert_protocol(xcap_usb2(), 4, 2, 1);
    }

    #[test]
    fn xcap_usb3_fields() {
        // Next = 0, the last capability. Port 2.
        assert_protocol(xcap_usb3(), 0, 3, 2);
    }

    /// Every root hub port belongs to exactly one protocol, and together
    /// they cover MaxPorts.
    #[test]
    fn xcap_ports_cover_max_ports_once() {
        let max_ports = hcsparams1() >> 24;
        let mut owners = vec![0; max_ports as usize + 1];
        for cap in [xcap_usb2(), xcap_usb3()] {
            let first = cap[2] & 0xFF;
            let count = (cap[2] >> 8) & 0xFF;
            for port in first..first + count {
                owners[port as usize] += 1;
            }
        }
        assert_eq!(&owners[1..], vec![1; max_ports as usize]);
    }

    #[test]
    fn raw_trb_type_extraction() {
        let trb = RawTrb {
            dword0: 0,
            dword1: 0,
            dword2: 0,
            dword3: (TRB_TYPE_ENABLE_SLOT << TRB_TYPE_SHIFT) | TRB_CYCLE,
        };
        assert_eq!(trb.trb_type(), TRB_TYPE_ENABLE_SLOT);
        assert!(trb.cycle_bit());
    }

    #[test]
    fn raw_trb_slot_id() {
        let trb = RawTrb {
            dword0: 0,
            dword1: 0,
            dword2: 0,
            dword3: 1u32 << 24,
        };
        assert_eq!(trb.slot_id(), 1);
    }

    #[test]
    fn raw_trb_parameter() {
        let trb = RawTrb {
            dword0: 0xDEAD_BEEF,
            dword1: 0x0000_1234,
            dword2: 0,
            dword3: 0,
        };
        assert_eq!(trb.parameter(), 0x0000_1234_DEAD_BEEF);
    }

    #[test]
    fn portsc_base_alignment() {
        // xHCI 1.2 section 5.4.8: port register set n is at Operational
        // Base + 0x400 + 0x10 * (n - 1). Linux's xhci_op_regs puts
        // port_status_base at 0x400 too. With CAPLENGTH 0x20 that is BAR0
        // offset 0x420; at 0x400 the guest reads port 1 as all zeroes.
        assert_eq!(PORTSC_BASE, 0x400);
        assert_eq!(PORTSC_BASE + OP_BASE, 0x420);
    }

    #[test]
    fn register_layout_no_overlap() {
        // Verify the register regions don't overlap
        let cap_end = CAPLENGTH as usize; // 0x20
        let op_end = OP_BASE + PORTSC_BASE; // operational regs end at port base
        let port_end = op_end + 2 * PORT_REG_SIZE; // 2 ports
        let db_start = DBOFF as usize; // 0x440
        let db_end = db_start + (usize::from(MAX_SLOTS) + 1) * DB_REG_SIZE;
        let rt_start = RTSOFF as usize; // 0x560
        let runtime_window_len = IR0_BASE + 0x20;
        let xcap_start = XCAP_BASE; // 0x600

        assert!(cap_end <= OP_BASE + OP_USBCMD);
        assert!(op_end <= db_start);
        assert!(port_end <= db_start);
        assert!(db_end <= rt_start);
        assert_eq!(rt_start % 32, 0);
        assert!(rt_start + runtime_window_len <= xcap_start);
        assert!(xcap_start + 2 * XCAP_PROTO_SIZE <= BAR0_SIZE as usize);
    }
}
