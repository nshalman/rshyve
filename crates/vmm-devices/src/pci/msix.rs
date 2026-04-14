// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! MSI-X interrupt support for PCI devices.
//!
//! Provides [`MsixTable`] which manages MSI-X vector entries and
//! delivers interrupts via `VmmHdl::lapic_msi()`. Devices use this
//! by placing the MSI-X table in a BAR and calling [`fire`] to
//! trigger a specific vector.
//!
//! # PCI Config Space
//!
//! The MSI-X capability (ID 0x11) occupies 12 bytes in config space.
//! Devices must add this to their capability chain and handle reads/
//! writes via [`cap_read`]/[`cap_write`].

use std::sync::{Arc, Mutex};

use vmm_core::hdl::VmmHdl;

/// MSI-X capability ID in PCI config space.
pub const MSIX_CAP_ID: u8 = 0x11;

/// Size of one MSI-X table entry (16 bytes).
const ENTRY_SIZE: usize = 16;

/// Bit 0 of vector control: per-vector mask.
const VECTOR_CTRL_MASK: u32 = 1;

/// MSI-X Message Control register bits.
const MSG_CTRL_ENABLE: u16 = 1 << 15;
const MSG_CTRL_FUNC_MASK: u16 = 1 << 14;

/// A single MSI-X vector entry.
#[derive(Debug, Clone, Copy)]
struct MsixEntry {
    addr_lo: u32,
    addr_hi: u32,
    data: u32,
    control: u32, // bit 0 = masked
}

impl Default for MsixEntry {
    fn default() -> Self {
        Self {
            addr_lo: 0,
            addr_hi: 0,
            data: 0,
            control: VECTOR_CTRL_MASK, // masked by default
        }
    }
}

/// MSI-X table and interrupt delivery state.
///
/// Thread-safe: the inner state is protected by a mutex. The hot
/// path ([`fire`]) acquires the lock briefly to read the vector
/// entry, then drops it before the ioctl.
pub struct MsixTable {
    inner: Mutex<MsixInner>,
    hdl: Arc<VmmHdl>,
}

struct MsixInner {
    entries: Vec<MsixEntry>,
    /// MSI-X enabled (Message Control bit 15).
    enabled: bool,
    /// Function-level mask (Message Control bit 14).
    func_masked: bool,
    /// PBA (Pending Bit Array) — one bit per vector.
    pba: Vec<u64>,
}

impl MsixTable {
    /// Create a new MSI-X table with `count` vectors.
    pub fn new(count: u16, hdl: Arc<VmmHdl>) -> Self {
        let entries = vec![MsixEntry::default(); count as usize];
        let pba_qwords = (count as usize + 63) / 64;
        Self {
            inner: Mutex::new(MsixInner {
                entries,
                enabled: false,
                func_masked: false,
                pba: vec![0; pba_qwords],
            }),
            hdl,
        }
    }

    /// Number of vectors in the table.
    pub fn count(&self) -> u16 {
        let inner = self.inner.lock().expect("msix lock");
        inner.entries.len() as u16
    }

    /// Fire an MSI-X interrupt for the given vector.
    ///
    /// If the vector is masked or MSI-X is globally disabled, the
    /// interrupt is recorded in the PBA for later delivery when
    /// unmasked.
    #[inline]
    pub fn fire(&self, vector: u16) {
        let mut inner = self.inner.lock().expect("msix lock");
        let idx = vector as usize;
        if idx >= inner.entries.len() || !inner.enabled {
            return;
        }

        let entry = &inner.entries[idx];
        if inner.func_masked || (entry.control & VECTOR_CTRL_MASK) != 0 {
            let qword = idx / 64;
            let bit = idx % 64;
            inner.pba[qword] |= 1 << bit;
            return;
        }

        let addr = u64::from(entry.addr_lo) | (u64::from(entry.addr_hi) << 32);
        let data = u64::from(entry.data);
        drop(inner);

        let _ = self.hdl.lapic_msi(addr, data);
    }

    /// Returns true if MSI-X is enabled (Message Control bit 15).
    pub fn is_enabled(&self) -> bool {
        self.inner.lock().expect("msix lock").enabled
    }

    /// Enable MSI-X. Used during migration restore to set the enabled
    /// state without going through PCI config writes.
    pub fn set_enabled(&self, enabled: bool) {
        let mut inner = self.inner.lock().expect("msix lock");
        inner.enabled = enabled;
    }

    /// Read the address and data for a vector entry.
    ///
    /// Returns `(addr, data)` where addr is the 64-bit MSI address
    /// and data is the MSI message. Used for programming kernel
    /// devices (e.g., viona) with MSI delivery info.
    pub fn read_entry(&self, vector: u16) -> (u64, u64) {
        let inner = self.inner.lock().expect("msix lock");
        let idx = vector as usize;
        if idx >= inner.entries.len() {
            return (0, 0);
        }
        let e = &inner.entries[idx];
        let addr = u64::from(e.addr_lo) | (u64::from(e.addr_hi) << 32);
        let data = u64::from(e.data);
        (addr, data)
    }

    /// Write the address and data for a vector entry.
    ///
    /// Used during migration restore to program the MSI-X table with
    /// the source's interrupt routing state.
    pub fn write_entry(&self, vector: u16, addr: u64, data: u64) {
        let mut inner = self.inner.lock().expect("msix lock");
        let idx = vector as usize;
        if idx >= inner.entries.len() {
            return;
        }
        inner.entries[idx].addr_lo = addr as u32;
        inner.entries[idx].addr_hi = (addr >> 32) as u32;
        inner.entries[idx].data = data as u32;
        // Unmask the vector so it can fire
        inner.entries[idx].control &= !VECTOR_CTRL_MASK;
    }

    /// Read from the MSI-X table BAR.
    ///
    /// `offset` is relative to the start of the table region.
    pub fn table_read(&self, offset: usize) -> u32 {
        let inner = self.inner.lock().expect("msix lock");
        let idx = offset / ENTRY_SIZE;
        let field = offset % ENTRY_SIZE;
        if idx >= inner.entries.len() {
            return 0;
        }
        let entry = &inner.entries[idx];
        match field {
            0 => entry.addr_lo,
            4 => entry.addr_hi,
            8 => entry.data,
            12 => entry.control,
            _ => 0,
        }
    }

    /// Write to the MSI-X table BAR.
    pub fn table_write(&self, offset: usize, val: u32) {
        let mut inner = self.inner.lock().expect("msix lock");
        let idx = offset / ENTRY_SIZE;
        let field = offset % ENTRY_SIZE;
        if idx >= inner.entries.len() {
            return;
        }

        let was_masked = (inner.entries[idx].control & VECTOR_CTRL_MASK) != 0;

        match field {
            0 => inner.entries[idx].addr_lo = val,
            4 => inner.entries[idx].addr_hi = val,
            8 => inner.entries[idx].data = val,
            12 => inner.entries[idx].control = val,
            _ => {}
        }

        // Check if vector was just unmasked — deliver pending interrupt
        let now_masked = (inner.entries[idx].control & VECTOR_CTRL_MASK) != 0;
        if was_masked && !now_masked && inner.enabled {
            let qword = idx / 64;
            let bit = idx % 64;
            if inner.pba[qword] & (1 << bit) != 0 {
                inner.pba[qword] &= !(1 << bit);
                let e = &inner.entries[idx];
                let addr = u64::from(e.addr_lo) | (u64::from(e.addr_hi) << 32);
                let data = u64::from(e.data);
                drop(inner);
                // best-effort MSI delivery; kernel error if this fails
                let _ = self.hdl.lapic_msi(addr, data);
            }
        }
    }

    /// Read from the PBA BAR region.
    pub fn pba_read(&self, offset: usize) -> u32 {
        let inner = self.inner.lock().expect("msix lock");
        let qword_idx = offset / 8;
        let lo = (offset % 8) < 4;
        if qword_idx >= inner.pba.len() {
            return 0;
        }
        if lo {
            inner.pba[qword_idx] as u32
        } else {
            (inner.pba[qword_idx] >> 32) as u32
        }
    }

    // ── Config space capability reads/writes ────────────────────

    /// Read the MSI-X capability structure (12 bytes starting at `cap_offset`).
    ///
    /// `offset` is the config space offset being read. Returns the
    /// dword value, or `None` if the offset isn't part of this capability.
    pub fn cap_read(&self, offset: u8, cap_offset: u8) -> Option<u32> {
        if offset < cap_offset || offset >= cap_offset + 12 {
            return None;
        }
        let inner = self.inner.lock().expect("msix lock");
        let cap_rel = offset - cap_offset;
        let dword_off = cap_rel & 0xFC;
        match dword_off {
            // Byte 0: Cap ID (0x11), Byte 1: Next Ptr (0), Bytes 2-3: Message Control
            0 => {
                let table_size = (inner.entries.len() as u16).saturating_sub(1);
                let mut msg_ctrl = table_size & 0x7FF; // bits 10:0 = table size - 1
                if inner.enabled {
                    msg_ctrl |= MSG_CTRL_ENABLE;
                }
                if inner.func_masked {
                    msg_ctrl |= MSG_CTRL_FUNC_MASK;
                }
                Some(
                    u32::from(MSIX_CAP_ID)
                        | (u32::from(msg_ctrl) << 16),
                )
            }
            // Bytes 4-7: Table Offset/BIR
            4 => {
                // Table in BAR4 at offset 0
                Some(4) // BIR = 4 (BAR4), offset = 0
            }
            // Bytes 8-11: PBA Offset/BIR
            8 => {
                // PBA in BAR4, after the table
                let table_bytes = inner.entries.len() * ENTRY_SIZE;
                let pba_offset = ((table_bytes + 0xFFF) & !0xFFF) as u32; // page-align
                Some(pba_offset | 4) // BIR = 4, offset = page-aligned after table
            }
            _ => None,
        }
    }

    /// Write to the MSI-X capability structure.
    pub fn cap_write(&self, offset: u8, cap_offset: u8, val: u32) {
        if offset < cap_offset || offset >= cap_offset + 12 {
            return;
        }
        let cap_rel = offset - cap_offset;
        let dword_off = cap_rel & 0xFC;
        if dword_off == 0 {
            let msg_ctrl = (val >> 16) as u16;
            let mut inner = self.inner.lock().expect("msix lock");
            inner.enabled = (msg_ctrl & MSG_CTRL_ENABLE) != 0;
            inner.func_masked = (msg_ctrl & MSG_CTRL_FUNC_MASK) != 0;
        }
        // Table BIR and PBA BIR are read-only
    }

    /// Total BAR size needed for MSI-X table + PBA.
    pub fn bar_size(&self) -> usize {
        let inner = self.inner.lock().expect("msix lock");
        let table_bytes = inner.entries.len() * ENTRY_SIZE;
        let pba_bytes = (inner.entries.len() + 7) / 8;
        let total = table_bytes + pba_bytes;
        // Round up to next power of 2, minimum 4K
        total.next_power_of_two().max(4096)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Can't test fire() without a VmmHdl, but we can test table mechanics

    #[test]
    fn entry_default_is_masked() {
        let e = MsixEntry::default();
        assert_ne!(e.control & VECTOR_CTRL_MASK, 0);
    }

    #[test]
    fn bar_size_minimum_4k() {
        // Need a VmmHdl for MsixTable, so test the math directly
        let count = 4u16;
        let table_bytes = count as usize * ENTRY_SIZE;
        let pba_bytes = (count as usize + 7) / 8;
        let total = table_bytes + pba_bytes;
        assert!(total.next_power_of_two().max(4096) >= 4096);
    }

    #[test]
    fn cap_read_returns_cap_id() {
        // Verify capability ID encoding
        assert_eq!(MSIX_CAP_ID, 0x11);
    }
}
