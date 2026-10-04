// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Buffer slots, and requests cut into parts that fit them.
//!
//! Each ring tag owns one fixed slot of `part_blocks` pages. NVMe MDTS
//! is 2 MiB and a backend cannot lower it, so a larger request goes as
//! several parts, each with its own tag.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Slots {
    /// Ring tags in use: at most the ring depth.
    pub(crate) tags: u16,
    /// 4 KiB pages per slot, and the most blocks one part carries.
    pub(crate) part_blocks: u32,
}

impl Slots {
    /// Fits `depth` slots into `buf_pages`, no slot larger than
    /// `max_blocks`. When the area has fewer pages than the depth, each
    /// slot is one page and some tags stay unused.
    pub(crate) fn new(
        depth: u32,
        buf_pages: u32,
        max_blocks: u32,
    ) -> Option<Self> {
        if depth == 0 || buf_pages == 0 || max_blocks == 0 {
            return None;
        }
        let part_blocks = max_blocks.min(buf_pages / depth).max(1);
        let tags = depth.min(buf_pages / part_blocks);
        Some(Self {
            tags: u16::try_from(tags).ok()?,
            part_blocks,
        })
    }

    pub(crate) fn page(&self, tag: u16) -> u32 {
        u32::from(tag) * self.part_blocks
    }

    pub(crate) fn parts(&self, blocks: u32) -> u32 {
        blocks.div_ceil(self.part_blocks)
    }

    /// Blocks in the part that starts at block `next`.
    pub(crate) fn next_part(&self, blocks: u32, next: u32) -> u32 {
        blocks.saturating_sub(next).min(self.part_blocks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_fill_the_area_without_passing_it() {
        let s = Slots::new(64, 1024, 512).unwrap();
        assert_eq!(
            s,
            Slots {
                tags: 64,
                part_blocks: 16
            }
        );
        assert!(s.page(63) + s.part_blocks <= 1024);

        // The engine caps blocks per request below the area's share.
        let s = Slots::new(64, 4096, 8).unwrap();
        assert_eq!(
            s,
            Slots {
                tags: 64,
                part_blocks: 8
            }
        );

        // Fewer pages than tags: one page each, the rest unused.
        let s = Slots::new(64, 10, 512).unwrap();
        assert_eq!(
            s,
            Slots {
                tags: 10,
                part_blocks: 1
            }
        );
        assert_eq!(s.page(9), 9);

        assert_eq!(Slots::new(0, 10, 1), None);
        assert_eq!(Slots::new(2, 0, 1), None);
        assert_eq!(Slots::new(2, 10, 0), None);
    }

    #[test]
    fn a_large_request_splits_into_parts_that_cover_it_once() {
        let s = Slots::new(16, 128, 8).unwrap();
        assert_eq!(s.part_blocks, 8);
        // MDTS: 2 MiB is 512 blocks.
        for blocks in [1u32, 7, 8, 9, 64, 511, 512] {
            let mut next = 0;
            let mut parts = 0;
            while next < blocks {
                let n = s.next_part(blocks, next);
                assert!(n >= 1 && n <= s.part_blocks);
                next += n;
                parts += 1;
            }
            assert_eq!(next, blocks);
            assert_eq!(parts, s.parts(blocks), "{blocks} blocks");
            assert_eq!(s.next_part(blocks, blocks), 0);
        }
    }
}
