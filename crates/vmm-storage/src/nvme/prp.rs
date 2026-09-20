// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! PRP walking.

use std::io;

use vmm_core::common::PAGE_SIZE;
use vmm_core::mem::{GuestIoVec, MemCtx, PhysMap};

// ---------------------------------------------------------------------------
// PRP handling
// ---------------------------------------------------------------------------

/// Resolve PRP entries into a list of (guest_physical_addr, length) segments.
///
/// For NVMe, PRP entry 1 is the first page (possibly unaligned start),
/// PRP entry 2 is either a second page or a PRP list page for larger
/// transfers.
fn prp_collect(
    mem: &MemCtx,
    prp1: u64,
    prp2: u64,
    total_len: usize,
) -> io::Result<Vec<(u64, usize)>> {
    let mut segments = Vec::new();
    let mut remaining = total_len;

    if remaining == 0 {
        return Ok(segments);
    }

    // PRP1: first page, possibly unaligned
    let offset_in_page = (prp1 as usize) & (PAGE_SIZE - 1);
    let first_len = (PAGE_SIZE - offset_in_page).min(remaining);
    segments.push((prp1, first_len));
    remaining -= first_len;

    if remaining == 0 {
        return Ok(segments);
    }

    // If remaining fits in a single page, PRP2 is a direct page address
    if remaining <= PAGE_SIZE {
        segments.push((prp2, remaining));
        return Ok(segments);
    }

    // PRP2 points to a PRP list page
    // Each entry in the PRP list is an 8-byte physical address
    let max_entries = PAGE_SIZE / 8;
    let list_gpa = prp2;
    let mut entry_idx = 0;

    while remaining > 0 && entry_idx < max_entries {
        let entry_gpa = list_gpa + (entry_idx as u64) * 8;
        let mut addr_buf = [0u8; 8];
        mem.read(entry_gpa, &mut addr_buf)?;
        let page_addr = u64::from_le_bytes(addr_buf);

        let seg_len = PAGE_SIZE.min(remaining);
        segments.push((page_addr, seg_len));
        remaining -= seg_len;
        entry_idx += 1;
    }

    if remaining > 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "PRP list insufficient for transfer size",
        ));
    }

    Ok(segments)
}

/// Resolve PRP entries into a scatter list over guest memory.
///
/// The list pins the guest pages, so the transfer goes straight to
/// them: no intermediate buffer, no copies.
pub(super) fn prp_to_iovec(
    physmap: &PhysMap,
    prp1: u64,
    prp2: u64,
    total_len: usize,
) -> io::Result<GuestIoVec> {
    let mut result = GuestIoVec::with_capacity(16);
    let mut remaining = total_len;

    if remaining == 0 {
        return Ok(result);
    }

    // PRP1: first page, possibly unaligned
    let offset_in_page = (prp1 as usize) & (PAGE_SIZE - 1);
    let first_len = (PAGE_SIZE - offset_in_page).min(remaining);
    let sub = physmap.lookup(prp1, first_len).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "PRP1 unmapped")
    })?;
    result.push(sub);
    remaining -= first_len;

    if remaining == 0 {
        return Ok(result);
    }

    // If remaining fits in a single page, PRP2 is a direct page address
    if remaining <= PAGE_SIZE {
        let sub = physmap.lookup(prp2, remaining).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "PRP2 unmapped")
        })?;
        result.push(sub);
        return Ok(result);
    }

    // PRP2 points to a PRP list page — read entries from guest memory
    let max_entries = PAGE_SIZE / 8;
    let mut entry_idx = 0;

    while remaining > 0 && entry_idx < max_entries {
        let entry_gpa = prp2 + (entry_idx as u64) * 8;
        // Read the 8-byte PRP list entry from guest memory
        let entry_sub = physmap.lookup(entry_gpa, 8).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "PRP list unmapped")
        })?;
        let mut addr_buf = [0u8; 8];
        entry_sub.read_bytes(&mut addr_buf)?;
        let page_addr = u64::from_le_bytes(addr_buf);

        let seg_len = PAGE_SIZE.min(remaining);
        let sub = physmap.lookup(page_addr, seg_len).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "PRP page unmapped")
        })?;
        result.push(sub);
        remaining -= seg_len;
        entry_idx += 1;
    }

    if remaining > 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "PRP list insufficient for transfer size",
        ));
    }

    Ok(result)
}

/// Write data to guest memory using PRP entries (for Identify etc.).
pub(super) fn write_to_prp(
    mem: &MemCtx,
    prp1: u64,
    prp2: u64,
    data: &[u8],
) -> io::Result<()> {
    let segments = prp_collect(mem, prp1, prp2, data.len())?;
    let mut offset = 0;
    for (gpa, len) in segments {
        mem.write(gpa, &data[offset..offset + len])?;
        offset += len;
    }
    Ok(())
}
