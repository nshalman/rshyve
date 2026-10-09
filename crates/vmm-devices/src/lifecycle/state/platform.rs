// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! State of the devices with no PCI address: the LPC and ACPI devices
//! that user space emulates.
//!
//! The wire names each one by its registry id, such as `lpc/com1`.
//! The RTC, the PIT, the i8259 pair, the I/O APIC, the HPET and the
//! ACPI PM timer are in the kernel, and travel as kernel payloads.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{wire_bytes, DeviceMigrateState, MigratePciState};

/// A device with no PCI address, as the preamble names it.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize,
)]
pub struct PlatformIdentity {
    /// The registry id. Stable across hosts for one VM shape.
    pub id: String,
    /// The device's `type_name()`.
    pub kind: String,
}

impl fmt::Display for PlatformIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.kind, self.id)
    }
}

/// One device's state under its registry id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformStatePayload {
    pub id: String,
    pub state: DeviceMigrateState,
}

/// A 16550 UART: every register the guest can set, the THRE interrupt
/// it has not taken yet, and both FIFOs.
///
/// IIR and the interrupt line follow from these, so they are computed
/// again on restore. FCR is accepted and dropped by the model, so it
/// has no state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UartMigrateState {
    pub ier: u8,
    pub lcr: u8,
    pub mcr: u8,
    /// LSR. DR is taken from the RX FIFO on restore.
    pub lsr: u8,
    pub msr: u8,
    pub scr: u8,
    pub dll: u8,
    pub dlh: u8,
    /// A THRE interrupt the guest has not cleared by reading IIR.
    pub thre_pending: bool,
    /// Bytes for the guest, oldest first.
    #[serde(with = "wire_bytes")]
    pub rx: Vec<u8>,
    /// Bytes from the guest that no host backend took yet.
    #[serde(with = "wire_bytes")]
    pub tx: Vec<u8>,
}

/// The i8042 controller with its keyboard and mouse ports.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ps2MigrateState {
    pub resp: Option<u8>,
    pub cmd_prefix: Option<u8>,
    pub ctrl_cfg: u8,
    pub ctrl_out_port: u8,
    #[serde(with = "wire_bytes")]
    pub ram: Vec<u8>,
    pub kbd: Ps2KbdMigrateState,
    pub mouse: Ps2MouseMigrateState,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ps2KbdMigrateState {
    #[serde(with = "wire_bytes")]
    pub buf: Vec<u8>,
    pub cur_cmd: Option<u8>,
    pub enabled: bool,
    pub led_status: u8,
    pub typematic: u8,
    /// 1 or 2.
    pub scan_code_set: u8,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ps2MouseMigrateState {
    #[serde(with = "wire_bytes")]
    pub buf: Vec<u8>,
    pub cur_cmd: Option<u8>,
    pub status: u8,
    pub resolution: u8,
    pub sample_rate: u8,
}

/// The ACPI PM1 event and control registers, and the reset register.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpiPmMigrateState {
    pub pm1_status: u16,
    pub pm1_enable: u16,
    /// Carries SCI_EN, which the guest set through SMI_CMD.
    pub pm1_control: u16,
    pub reset_control: u8,
}

/// The GPE0 block.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpiGpeMigrateState {
    pub status: u16,
    pub enable: u16,
}

/// The i440FX: the PCI config address latch, the PIRQ routing the
/// firmware wrote, and the two bridge headers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChipsetMigrateState {
    /// 0xCF8. A guest paused between the address and the data access
    /// reads the wrong register without it.
    pub cfg_addr: u32,
    pub pir: [u8; 4],
    pub post_code: u8,
    pub hostbridge: MigratePciState,
    pub lpc_bridge: MigratePciState,
}

/// The fw_cfg item cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FwCfgMigrateState {
    pub selector: u16,
    pub offset: u64,
}

/// The CPU hotplug latches the AML sets before it reads a slot.
///
/// Only these move while nothing is plugged: a VM that hot-added or
/// removed anything since boot does not migrate, so no event, `_OST`
/// report or eject is in flight. The PCI hotplug block holds nothing
/// else, so it carries no state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuHotplugMigrateState {
    pub selector: u32,
    pub command: u8,
}

/// The memory hotplug slot selector, for the same reason.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemHotplugMigrateState {
    pub selected: Option<u8>,
}
