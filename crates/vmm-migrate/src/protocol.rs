// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Migration protocol implementation.
//!
//! A wire-format change bumps [`PROTOCOL_RON`]. A destination also takes
//! the previous version only while its payload is a strict subset of this
//! one, with defaults that match what the old source did. Anything older
//! is refused: a peer that reads a field as something else resumes a
//! guest on state that is not its own.

use vmm_devices::lifecycle::{
    DeviceMigrateState, DeviceStatePayload, NvmeMigrateBackend,
};

/// Protocol identifier for the RON migration format.
///
/// v4 names the store under each NVMe namespace, so a destination on
/// another store or another MantaBlock volume refuses the payload. A v3
/// or v2 payload names none, which means a host file.
///
/// v3 carries held NVMe completions and the NVMe namespace shape. A v2
/// payload restores with none held and the one shape v2 had.
///
/// v2 carries the NVMe completion queue interrupt-enable bit. A v1 peer
/// has no field for it, and a destination that assumes one restores a
/// polled queue as an interrupting one.
///
/// v1 put device identity on the wire. In v0 every virtio queue group
/// was tagged slot 0 and the destination took the first group whose
/// length matched its queue count, so two devices of one queue count
/// restored each other's rings.
pub const PROTOCOL_RON: &str = "vmm-migrate-ron/4";

/// Older versions a destination still takes.
pub const PROTOCOL_RON_V3: &str = "vmm-migrate-ron/3";
pub const PROTOCOL_RON_V2: &str = "vmm-migrate-ron/2";

/// What a source offers, newest first.
///
/// v3 stays on offer so a file-backed VM can still move to a host that
/// predates v4. A v3 destination skips the store field and assumes a
/// host file, which is true unless [`payload_refusal`] says otherwise.
pub const SOURCE_OFFER: [&str; 2] = [PROTOCOL_RON, PROTOCOL_RON_V3];

/// Why `devices` cannot go to a destination that chose `protocol`.
pub fn payload_refusal(
    protocol: &str,
    devices: &[DeviceStatePayload],
) -> Option<String> {
    if protocol != PROTOCOL_RON_V3 {
        return None;
    }
    devices.iter().find_map(|d| match &d.state {
        Some(DeviceMigrateState::Nvme(nvme)) => match &nvme.backend {
            None | Some(NvmeMigrateBackend::File) => None,
            Some(store) => Some(format!(
                "the destination speaks {PROTOCOL_RON_V3}, which cannot \
                 name {store} under the disk at {}",
                d.bdf,
            )),
        },
        _ => None,
    })
}

/// The version a destination answers an offer with, newest first, or
/// `None` when it can read none of them.
pub fn select_protocol(offer: &[String]) -> Option<&'static str> {
    [PROTOCOL_RON, PROTOCOL_RON_V3, PROTOCOL_RON_V2]
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
    fn a_destination_prefers_the_current_version() {
        let both = offer(&[PROTOCOL_RON_V2, PROTOCOL_RON_V3, PROTOCOL_RON]);
        assert_eq!(select_protocol(&both), Some(PROTOCOL_RON));
    }

    #[test]
    fn a_destination_takes_a_v2_source() {
        let v2 = offer(&[PROTOCOL_RON_V2]);
        assert_eq!(select_protocol(&v2), Some(PROTOCOL_RON_V2));
    }

    #[test]
    fn a_destination_takes_a_v3_source() {
        let v3 = offer(&[PROTOCOL_RON_V2, PROTOCOL_RON_V3]);
        assert_eq!(select_protocol(&v3), Some(PROTOCOL_RON_V3));
    }

    #[test]
    fn a_new_source_reaches_an_old_destination() {
        let ours = offer(&SOURCE_OFFER);
        assert_eq!(select_protocol(&ours), Some(PROTOCOL_RON));
        // What a v3 build's destination picks from the same offer.
        let picked = [PROTOCOL_RON_V3, PROTOCOL_RON_V2]
            .into_iter()
            .find(|v| ours.iter().any(|o| o == v));
        assert_eq!(picked, Some(PROTOCOL_RON_V3));
    }

    fn nvme_on(backend: Option<NvmeMigrateBackend>) -> DeviceStatePayload {
        use vmm_devices::lifecycle::NvmeMigrateState;
        DeviceStatePayload {
            bdf: vmm_devices::lifecycle::WireBdf {
                bus: 0,
                dev: 4,
                func: 0,
            },
            state: Some(DeviceMigrateState::Nvme(NvmeMigrateState {
                backend,
                ..Default::default()
            })),
        }
    }

    #[test]
    fn only_host_files_go_to_a_v3_destination() {
        let volume = NvmeMigrateBackend::MantaBlock {
            volume: "6f1c2a8e-0000-4000-8000-000000000001"
                .parse()
                .expect("a UUID"),
        };
        for files in [None, Some(NvmeMigrateBackend::File)] {
            let payload = [nvme_on(files)];
            assert_eq!(payload_refusal(PROTOCOL_RON_V3, &payload), None);
        }
        let mut ring = nvme_on(Some(volume));
        ring.bdf.dev = 5;
        let at = ring.bdf.to_string();
        let payload = [nvme_on(Some(NvmeMigrateBackend::File)), ring];
        let why = payload_refusal(PROTOCOL_RON_V3, &payload)
            .expect("a volume a v3 destination cannot check");
        assert!(why.contains("MantaBlock volume 6f1c2a8e"), "{why}");
        assert!(why.ends_with(&at), "{why}");
        assert_eq!(payload_refusal(PROTOCOL_RON, &payload), None);
    }

    #[test]
    fn a_destination_refuses_anything_older() {
        let old = offer(&["vmm-migrate-ron/1", "vmm-migrate-ron/0"]);
        assert_eq!(select_protocol(&old), None);
        assert_eq!(select_protocol(&[]), None);
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
