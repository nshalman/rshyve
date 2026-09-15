// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// The declarations follow crates/dladm/src/sys.rs in Propolis.

//! The libdladm entry points this crate calls.

#![allow(non_camel_case_types)]

use libc::{c_int, c_uint};

/// Opaque handle to libdladm.
pub enum dladm_handle {}
pub type dladm_handle_t = *mut dladm_handle;
pub type datalink_id_t = u32;

pub const MAXMACADDRLEN: usize = 20;

/// The `DLADM_STATUS_NOTFOUND` member of `dladm_status_t`.
pub const DLADM_STATUS_NOTFOUND: c_int = 5;

/// `DLADM_OPT_ACTIVE`: read the running state, not the persistent one.
pub const DLADM_OPT_ACTIVE: u32 = 0x1;

/// The head of `dladm_vnic_attr_t`. The real struct ends with a
/// `mac_resource_props_t` that is 15,428 bytes long, which is why the
/// tail is padding here: the caller reads only the MAC.
#[repr(C)]
pub struct dladm_vnic_attr_t {
    pub va_vnic_id: datalink_id_t,
    pub va_link_id: datalink_id_t,
    pub va_mac_addr_type: c_int,
    pub va_mac_len: c_uint,
    pub va_mac_addr: [u8; MAXMACADDRLEN],
    pub va_mac_slot: c_int,
    pub va_mac_prefix_len: c_uint,
    pub va_vid: u16,
    pub va_force: c_int,
    pub va_vrid: u32,
    pub va_af: c_int,
    _resource_props: [u8; 15432],
}

impl dladm_vnic_attr_t {
    pub fn zeroed() -> Self {
        // Safety: every field is plain data for which zero is valid.
        unsafe { std::mem::zeroed() }
    }
}

#[cfg(target_os = "illumos")]
#[link(name = "dladm")]
extern "C" {
    pub fn dladm_open(handle: *mut dladm_handle_t) -> c_int;
    pub fn dladm_close(handle: dladm_handle_t);
    pub fn dladm_name2info(
        handle: dladm_handle_t,
        link: *const libc::c_char,
        linkidp: *mut datalink_id_t,
        flagp: *mut u32,
        classp: *mut c_int,
        mediap: *mut u32,
    ) -> c_int;
    pub fn dladm_vnic_info(
        handle: dladm_handle_t,
        linkid: datalink_id_t,
        attrp: *mut dladm_vnic_attr_t,
        flags: u32,
    ) -> c_int;
}
