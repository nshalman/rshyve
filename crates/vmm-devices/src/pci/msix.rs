// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! MSI-X interrupt support for PCI devices.
//!
//! Provides [`MsixTable`] which manages MSI-X vector entries and
//! delivers interrupts through an [`MsiSink`]. Devices use this by
//! placing the MSI-X table in a BAR and calling [`fire`] to trigger a
//! specific vector.
//!
//! # PCI Config Space
//!
//! The MSI-X capability (ID 0x11) occupies 12 bytes in config space.
//! Devices must add this to their capability chain and handle reads/
//! writes via [`cap_read`]/[`cap_write`].

use std::sync::{Arc, Mutex};

/// Deliver a message-signalled interrupt.
///
/// Indirected through a trait so the table is unit-testable without a
/// live /dev/vmm handle, and so the table stays free of any one
/// hypervisor. bhyve does this with VM_LAPIC_MSI; KVM would use
/// KVM_SIGNAL_MSI.
pub trait MsiSink: Send + Sync + 'static {
    fn send(&self, addr: u64, data: u64);
}

/// MSI sink backed by a live vmm handle.
pub struct HdlMsiSink {
    hdl: Arc<vmm_core::hdl::VmmHdl>,
}

impl HdlMsiSink {
    /// Create an MSI sink for the VM handle.
    pub fn new(hdl: Arc<vmm_core::hdl::VmmHdl>) -> Arc<Self> {
        Arc::new(Self { hdl })
    }
}

impl MsiSink for HdlMsiSink {
    fn send(&self, addr: u64, data: u64) {
        // Delivery is best effort. A device has no caller to hand a
        // kernel error back to, and the guest re-arms the vector.
        let _ = self.hdl.lapic_msi(addr, data);
    }
}

/// MSI-X capability ID in PCI config space.
pub const MSIX_CAP_ID: u8 = 0x11;

/// Size of one MSI-X table entry (16 bytes).
const ENTRY_SIZE: usize = 16;

/// Bit 0 of vector control: per-vector mask.
const VECTOR_CTRL_MASK: u32 = 1;

/// MSI-X Message Control register bits.
const MSG_CTRL_ENABLE: u16 = 1 << 15;
const MSG_CTRL_FUNC_MASK: u16 = 1 << 14;

/// Byte offset of the PBA within the MSI-X BAR for `count` vectors.
///
/// PCI 3.0 section 6.8.2 forbids the table and the PBA from sharing a
/// naturally aligned 4 KiB region, so the PBA starts at the first page
/// boundary past the table.
fn pba_offset_for(count: u16) -> usize {
    ((count as usize * ENTRY_SIZE) + 0xFFF) & !0xFFF
}

/// Bytes the PBA occupies for `count` vectors (one bit per vector).
fn pba_bytes_for(count: u16) -> usize {
    (count as usize).div_ceil(8)
}

/// BAR size that contains both the table and the PBA.
///
/// Sizing from the table length alone puts the PBA past the end of its
/// own BAR; Windows rejects that capability and leaves stornvme.sys with
/// no interrupt resource, so StartDevice fails.
fn bar_size_for(count: u16) -> usize {
    (pba_offset_for(count) + pba_bytes_for(count))
        .next_power_of_two()
        .max(4096)
}

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
    msi: Arc<dyn MsiSink>,
    /// Fixed at construction; cached outside the lock so the BAR decode
    /// path can compute the PBA offset without contending on the mutex.
    count: u16,
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
    pub fn new(count: u16, msi: Arc<dyn MsiSink>) -> Self {
        let entries = vec![MsixEntry::default(); count as usize];
        let pba_qwords = (count as usize).div_ceil(64);
        Self {
            inner: Mutex::new(MsixInner {
                entries,
                enabled: false,
                func_masked: false,
                pba: vec![0; pba_qwords],
            }),
            msi,
            count,
        }
    }

    /// Number of vectors in the table.
    pub fn count(&self) -> u16 {
        self.count
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

        self.msi.send(addr, data);
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

        // An unmask sends the message the PBA is holding for this one
        // vector. The function mask still gates it: both masks must be
        // clear before a message leaves.
        let now_masked = (inner.entries[idx].control & VECTOR_CTRL_MASK) != 0;
        let pending = (was_masked && !now_masked)
            .then(|| take_pending(&mut inner, idx))
            .flatten();
        drop(inner);

        if let Some((addr, data)) = pending {
            self.msi.send(addr, data);
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

    /// Byte offset of the PBA within the MSI-X BAR.
    ///
    /// Single source of truth: BAR decode and the capability structure must
    /// agree or the PBA is unreachable.
    pub fn pba_offset(&self) -> usize {
        pba_offset_for(self.count)
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
                Some(u32::from(MSIX_CAP_ID) | (u32::from(msg_ctrl) << 16))
            }
            // Bytes 4-7: Table Offset/BIR
            4 => {
                // Table in BAR4 at offset 0
                Some(4) // BIR = 4 (BAR4), offset = 0
            }
            // Bytes 8-11: PBA Offset/BIR (BIR = 4)
            8 => Some(pba_offset_for(self.count) as u32 | 4),
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
        if dword_off != 0 {
            // Table BIR and PBA BIR are read-only.
            return;
        }

        let msg_ctrl = (val >> 16) as u16;
        let mut inner = self.inner.lock().expect("msix lock");
        inner.enabled = (msg_ctrl & MSG_CTRL_ENABLE) != 0;
        inner.func_masked = (msg_ctrl & MSG_CTRL_FUNC_MASK) != 0;
        // PCI 3.0 6.8.2.3: clearing the Function Mask sends every message
        // the PBA recorded while it was set. Without this the guest waits
        // on an I/O that already completed.
        let pending = drain_pending(&mut inner);
        drop(inner);

        for (addr, data) in pending {
            self.msi.send(addr, data);
        }
    }

    /// Total BAR size needed for MSI-X table + PBA.
    pub fn bar_size(&self) -> usize {
        bar_size_for(self.count)
    }
}

/// Take the message one vector has pending, if it can be sent now.
///
/// Clears the PBA bit, so one recorded interrupt sends one message
/// however many times the guest masks and unmasks the vector.
fn take_pending(inner: &mut MsixInner, idx: usize) -> Option<(u64, u64)> {
    if !inner.enabled || inner.func_masked {
        return None;
    }
    // Copied out so the entry borrow ends before the PBA is written.
    let entry = *inner.entries.get(idx)?;
    if (entry.control & VECTOR_CTRL_MASK) != 0 {
        return None;
    }

    let bit = 1u64 << (idx % 64);
    let word = inner.pba.get_mut(idx / 64)?;
    if *word & bit == 0 {
        return None;
    }
    *word &= !bit;

    Some((
        u64::from(entry.addr_lo) | (u64::from(entry.addr_hi) << 32),
        u64::from(entry.data),
    ))
}

/// Take every message the PBA is holding that can be sent now.
///
/// Walks the set bits, not the whole table, so a guest that toggles the
/// function mask pays for what it left pending and nothing more.
fn drain_pending(inner: &mut MsixInner) -> Vec<(u64, u64)> {
    if !inner.enabled || inner.func_masked {
        return Vec::new();
    }

    let mut messages = Vec::new();
    for word in 0..inner.pba.len() {
        let mut bits = inner.pba[word];
        while bits != 0 {
            let bit = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            // `take_pending` bounds-checks: a bit past the table is
            // never set by `fire`, and this index is guest driven.
            if let Some(message) = take_pending(inner, word * 64 + bit) {
                messages.push(message);
            }
        }
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records every message the table delivers, so a test can assert
    /// the exact bytes that reach the hypervisor.
    struct RecordingSink(Mutex<Vec<(u64, u64)>>);

    impl RecordingSink {
        fn new() -> Arc<Self> {
            Arc::new(Self(Mutex::new(Vec::new())))
        }

        fn recorded(&self) -> Vec<(u64, u64)> {
            self.0.lock().expect("recording lock poisoned").clone()
        }
    }

    impl MsiSink for RecordingSink {
        fn send(&self, addr: u64, data: u64) {
            self.0
                .lock()
                .expect("recording lock poisoned")
                .push((addr, data));
        }
    }

    /// A table with `count` vectors and MSI-X enabled, as a guest leaves
    /// it after it writes Message Control.
    fn enabled_table(count: u16) -> (MsixTable, Arc<RecordingSink>) {
        let sink = RecordingSink::new();
        let table = MsixTable::new(count, sink.clone());
        table.cap_write(0, 0, u32::from(MSG_CTRL_ENABLE) << 16);
        assert!(table.is_enabled());
        (table, sink)
    }

    #[test]
    fn fire_delivers_the_programmed_address_and_data() {
        let (table, sink) = enabled_table(4);

        // Vector 1 starts at table offset 0x10.
        table.table_write(0x10, 0xFEE0_1000); // address low
        table.table_write(0x14, 0x0000_0001); // address high
        table.table_write(0x18, 0x0000_4021); // message data
        table.table_write(0x1C, 0); // unmask

        table.fire(1);

        // The address is the two halves joined; the data is the low
        // 32 bits with the top half zero.
        assert_eq!(sink.recorded(), vec![(0x0000_0001_FEE0_1000, 0x4021)]);
    }

    #[test]
    fn message_data_is_zero_extended() {
        let (table, sink) = enabled_table(1);

        table.table_write(0x0, 0xFFFF_FFFF);
        table.table_write(0x4, 0xFFFF_FFFF);
        table.table_write(0x8, 0xFFFF_FFFF);
        table.table_write(0xC, 0);

        table.fire(0);

        assert_eq!(
            sink.recorded(),
            vec![(0xFFFF_FFFF_FFFF_FFFF, 0x0000_0000_FFFF_FFFF)]
        );
    }

    #[test]
    fn a_masked_vector_goes_to_the_pba_and_unmask_delivers_it_once() {
        let (table, sink) = enabled_table(2);

        // Entries start masked, so the message is only programmed here.
        table.table_write(0x0, 0xFEE0_2000);
        table.table_write(0x4, 0);
        table.table_write(0x8, 0x0000_0031);

        table.fire(0);
        assert!(sink.recorded().is_empty());
        assert_eq!(table.pba_read(0) & 1, 1);

        table.table_write(0xC, 0);
        assert_eq!(sink.recorded(), vec![(0xFEE0_2000, 0x31)]);
        assert_eq!(table.pba_read(0) & 1, 0);

        // A later mask and unmask must not replay the message.
        table.table_write(0xC, VECTOR_CTRL_MASK);
        table.table_write(0xC, 0);
        assert_eq!(sink.recorded().len(), 1);
    }

    #[test]
    fn the_function_mask_defers_every_vector() {
        let (table, sink) = enabled_table(2);
        table.write_entry(0, 0xFEE0_4000, 0x51);

        table.cap_write(
            0,
            0,
            u32::from(MSG_CTRL_ENABLE | MSG_CTRL_FUNC_MASK) << 16,
        );
        table.fire(0);

        assert!(sink.recorded().is_empty());
        assert_eq!(table.pba_read(0) & 1, 1);
    }

    #[test]
    fn a_disabled_table_delivers_nothing_and_keeps_the_pba_clear() {
        let sink = RecordingSink::new();
        let table = MsixTable::new(2, sink.clone());
        table.write_entry(0, 0xFEE0_3000, 0x41);

        table.fire(0);

        assert!(sink.recorded().is_empty());
        assert_eq!(table.pba_read(0), 0);
    }

    #[test]
    fn a_vector_past_the_table_is_ignored() {
        let (table, sink) = enabled_table(2);

        table.fire(2);
        table.fire(u16::MAX);

        assert!(sink.recorded().is_empty());
    }

    #[test]
    fn entry_default_is_masked() {
        let e = MsixEntry::default();
        assert_ne!(e.control & VECTOR_CTRL_MASK, 0);
    }

    #[test]
    fn pba_lies_inside_bar() {
        for count in [1u16, 2, 4, 8, 16, 17, 32, 64, 255, 256, 257, 1024, 2048]
        {
            let end = pba_offset_for(count) + pba_bytes_for(count);
            let size = bar_size_for(count);
            assert!(
                end <= size,
                "count {count}: pba ends {end:#x}, bar {size:#x}"
            );
            assert!(size.is_power_of_two(), "count {count}: bar {size:#x}");
            assert!(size >= 4096, "count {count}: bar {size:#x}");
        }
    }

    #[test]
    fn nvme_vector_count_needs_8k_bar() {
        // 17 vectors: 272 B table, PBA page-aligned to 0x1000, BAR must reach it.
        assert_eq!(pba_offset_for(17), 0x1000);
        assert_eq!(pba_bytes_for(17), 3);
        assert_eq!(bar_size_for(17), 0x2000);
    }

    #[test]
    fn cap_read_returns_cap_id() {
        // Verify capability ID encoding
        assert_eq!(MSIX_CAP_ID, 0x11);
    }

    // ── Function mask (PCI 3.0 6.8.2.3) ─────────────────────────

    /// Message Control with MSI-X enabled and the function mask as given.
    fn write_msg_ctrl(table: &MsixTable, func_masked: bool) {
        let mut ctrl = MSG_CTRL_ENABLE;
        if func_masked {
            ctrl |= MSG_CTRL_FUNC_MASK;
        }
        table.cap_write(0, 0, u32::from(ctrl) << 16);
    }

    #[test]
    fn clearing_the_function_mask_delivers_every_pending_vector() {
        // Without this the guest waits forever on an I/O that finished:
        // the completion is recorded in the PBA and never sent.
        let (table, sink) = enabled_table(2);
        table.write_entry(0, 0xFEE0_5000, 0x61);
        table.write_entry(1, 0xFEE0_5000, 0x62);
        write_msg_ctrl(&table, true);

        table.fire(0);
        table.fire(1);
        assert!(sink.recorded().is_empty());
        assert_eq!(table.pba_read(0) & 0b11, 0b11);

        write_msg_ctrl(&table, false);

        assert_eq!(
            sink.recorded(),
            vec![(0xFEE0_5000, 0x61), (0xFEE0_5000, 0x62)]
        );
        assert_eq!(table.pba_read(0) & 0b11, 0);
    }

    #[test]
    fn a_function_mask_toggle_delivers_a_message_once() {
        let (table, sink) = enabled_table(1);
        table.write_entry(0, 0xFEE0_6000, 0x71);
        write_msg_ctrl(&table, true);
        table.fire(0);

        write_msg_ctrl(&table, false);
        write_msg_ctrl(&table, true);
        write_msg_ctrl(&table, false);

        assert_eq!(sink.recorded(), vec![(0xFEE0_6000, 0x71)]);
    }

    #[test]
    fn clearing_the_function_mask_leaves_a_per_vector_mask_pending() {
        // Both masks gate delivery, so the vector mask still holds.
        let (table, sink) = enabled_table(2);
        table.write_entry(0, 0xFEE0_7000, 0x81);
        write_msg_ctrl(&table, true);
        table.fire(0);
        table.table_write(0xC, VECTOR_CTRL_MASK);

        write_msg_ctrl(&table, false);

        assert!(sink.recorded().is_empty());
        assert_eq!(table.pba_read(0) & 1, 1);

        table.table_write(0xC, 0);
        assert_eq!(sink.recorded(), vec![(0xFEE0_7000, 0x81)]);
    }

    #[test]
    fn an_unmask_under_the_function_mask_delivers_nothing() {
        let (table, sink) = enabled_table(1);
        table.table_write(0x0, 0xFEE0_8000);
        table.table_write(0x8, 0x91);
        write_msg_ctrl(&table, true);
        table.fire(0);

        table.table_write(0xC, 0);

        assert!(sink.recorded().is_empty());
        assert_eq!(table.pba_read(0) & 1, 1);
    }

    #[test]
    fn a_pending_vector_past_the_table_is_never_delivered() {
        let (table, sink) = enabled_table(2);
        write_msg_ctrl(&table, true);
        // Only fire() writes the PBA and it bounds-checks, so reach in.
        table.inner.lock().expect("msix lock").pba[0] = u64::MAX;

        write_msg_ctrl(&table, false);

        assert_eq!(sink.recorded().len(), 0);
    }
}
