// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The xHCI controller with its built-in tablet.
//!
//! The device contexts and the rings live in guest memory, which the
//! RAM passes carry. This is the controller's side of them: its
//! registers and the cursors it keeps into each ring.

use serde::{Deserialize, Serialize};

use super::MigratePciState;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct XhciMigrateState {
    pub pci: MigratePciState,

    pub usbcmd: u32,
    pub usbsts: u32,
    pub dnctrl: u32,
    pub crcr: u64,
    pub dcbaap: u64,
    pub config: u32,
    pub portsc: [u32; 2],

    pub iman: u32,
    pub imod: u32,
    pub erstsz: u32,
    pub erstba: u64,
    pub erdp: u64,

    pub event_ring: XhciMigrateEventRing,
    pub cmd_ring: XhciMigrateRing,

    /// 0 disabled, 1 enabled, 2 addressed, 3 configured.
    pub slot_state: u8,
    pub ep0: XhciMigrateRing,
    pub ep1_in: XhciMigrateRing,
    /// The guest queued interrupt-IN TRBs that wait for a report.
    pub ep1_in_armed: bool,

    pub tablet: XhciMigrateTablet,
}

/// The controller's producer cursor into the event ring.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct XhciMigrateEventRing {
    pub base: u64,
    pub size: u32,
    pub enqueue: u32,
    pub pcs: bool,
}

/// The controller's consumer cursor into a command or transfer ring.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct XhciMigrateRing {
    pub base: u64,
    pub dequeue: u64,
    pub ccs: bool,
}

/// The USB tablet behind slot 1.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct XhciMigrateTablet {
    pub buttons: u8,
    pub x: u16,
    pub y: u16,
    pub report_pending: bool,
    pub address: u8,
    pub configuration: u8,
    pub idle_rate: u8,
}
