// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Migration protocol implementation.
//!
//! A wire-format change bumps [`PROTOCOL_RON`]. Each end speaks only
//! the current version: a peer that reads a field as something else, or
//! leaves one out, resumes a guest on state that is not its own.

/// Protocol identifier for the RON migration format.
///
/// v7 has the v6 payload. It says the destination attaches a MantaBlock
/// ring disk after the commit, so a source with one moves only to a v7
/// destination. A v6 peer still takes any other VM.
///
/// v6 carries each vCPU's FPU and XSAVE area with the source host's
/// layout, and the NVMe Asynchronous Event Requests a controller holds.
/// A v4 destination resumed the guest with the destination's FPU state
/// (OSF-2), so no older version is accepted in either direction.
///
/// v5 is reserved for the virtio-gpu work (PR #15).
///
/// v4 named the store under each NVMe namespace. v3 carried held NVMe
/// completions and the namespace shape. v2 carried the CQ
/// interrupt-enable bit. v1 put device identity on the wire.
pub const PROTOCOL_RON: &str = "vmm-migrate-ron/7";

/// The version before, which differs only in the ring disk.
pub const PROTOCOL_RON_V6: &str = "vmm-migrate-ron/6";

/// What a source offers, newest first.
pub const SOURCE_OFFER: [&str; 2] = [PROTOCOL_RON, PROTOCOL_RON_V6];

/// The version a destination answers an offer with, newest first, or
/// `None` when it can read none of them.
pub fn select_protocol(offer: &[String]) -> Option<&'static str> {
    SOURCE_OFFER
        .into_iter()
        .find(|ours| offer.iter().any(|theirs| theirs == ours))
}

/// Page size used for dirty tracking (4 KiB).
pub use vmm_core::common::PAGE_SIZE;

/// Number of pages per byte in the dirty bitmap.
pub const PAGES_PER_BYTE: usize = 8;

/// Calculate the bitmap size needed for a given memory region.
pub fn bitmap_size(region_len: usize) -> usize {
    let num_pages = region_len.div_ceil(PAGE_SIZE);
    num_pages.div_ceil(PAGES_PER_BYTE)
}

/// Iterator over set bits in a dirty page bitmap.
pub struct PageIter<'a> {
    bitmap: &'a [u8],
    base_gpa: u64,
    bit_index: usize,
    total_bits: usize,
}

impl<'a> PageIter<'a> {
    pub fn new(bitmap: &'a [u8], base_gpa: u64, region_len: usize) -> Self {
        let total_bits = region_len / PAGE_SIZE;
        Self {
            bitmap,
            base_gpa,
            bit_index: 0,
            total_bits,
        }
    }
}

impl<'a> Iterator for PageIter<'a> {
    /// Returns the GPA of the next dirty page.
    type Item = u64;

    fn next(&mut self) -> Option<Self::Item> {
        while self.bit_index < self.total_bits {
            let byte_idx = self.bit_index / 8;
            let bit_idx = self.bit_index % 8;
            self.bit_index += 1;

            if byte_idx < self.bitmap.len()
                && (self.bitmap[byte_idx] & (1 << bit_idx)) != 0
            {
                let page_offset =
                    (self.bit_index - 1) as u64 * PAGE_SIZE as u64;
                return Some(self.base_gpa + page_offset);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(versions: &[&str]) -> Vec<String> {
        versions.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn a_destination_takes_the_current_version() {
        let mixed = offer(&["vmm-migrate-ron/4", PROTOCOL_RON]);
        assert_eq!(select_protocol(&mixed), Some(PROTOCOL_RON));
        assert_eq!(select_protocol(&offer(&SOURCE_OFFER)), Some(PROTOCOL_RON));
    }

    #[test]
    fn a_destination_takes_a_v6_source() {
        let v6 = offer(&[PROTOCOL_RON_V6]);
        assert_eq!(select_protocol(&v6), Some(PROTOCOL_RON_V6));
        // What a v6 build's destination picks from this offer.
        let ours = offer(&SOURCE_OFFER);
        assert!(ours.iter().any(|o| o == PROTOCOL_RON_V6));
    }

    #[test]
    fn a_destination_refuses_a_source_without_fpu_state() {
        // v5 is the virtio-gpu stream, and v4 and older carry no XSAVE
        // area (OSF-2).
        for old in [
            "vmm-migrate-ron/5",
            "vmm-migrate-ron/4",
            "vmm-migrate-ron/3",
        ] {
            assert_eq!(select_protocol(&offer(&[old])), None, "{old}");
        }
        assert_eq!(select_protocol(&[]), None);
    }

    #[test]
    fn an_old_destination_finds_nothing_it_reads_in_the_offer() {
        // What a v4 build's destination picks from this offer.
        let ours = offer(&SOURCE_OFFER);
        let picked = [
            "vmm-migrate-ron/4",
            "vmm-migrate-ron/3",
            "vmm-migrate-ron/2",
        ]
        .into_iter()
        .find(|v| ours.iter().any(|o| o == v));
        assert_eq!(picked, None);
    }

    #[test]
    fn test_bitmap_size() {
        assert_eq!(bitmap_size(4096), 1);
        assert_eq!(bitmap_size(4096 * 8), 1);
        assert_eq!(bitmap_size(4096 * 9), 2);
        assert_eq!(bitmap_size(1024 * 1024), 32); // 1 MiB = 256 pages = 32 bytes
    }

    #[test]
    fn test_page_iter_empty() {
        let bitmap = [0u8; 1];
        let iter = PageIter::new(&bitmap, 0, PAGE_SIZE * 8);
        assert_eq!(iter.count(), 0);
    }

    #[test]
    fn test_page_iter_all_dirty() {
        let bitmap = [0xFFu8; 1];
        let pages: Vec<u64> =
            PageIter::new(&bitmap, 0, PAGE_SIZE * 8).collect();
        assert_eq!(pages.len(), 8);
        assert_eq!(pages[0], 0);
        assert_eq!(pages[7], 7 * PAGE_SIZE as u64);
    }

    #[test]
    fn test_page_iter_with_base() {
        let bitmap = [0x01u8]; // only first page dirty
        let pages: Vec<u64> =
            PageIter::new(&bitmap, 0x1000_0000, PAGE_SIZE * 8).collect();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0], 0x1000_0000);
    }
}
