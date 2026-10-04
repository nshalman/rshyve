// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Moving one part's data between guest memory and its buffer slot.
//!
//! Guest memory takes host slices only, and so does the region, so the
//! data passes through a small staging buffer. It is a few pages and
//! stays in L1, so the data still crosses memory once each way.

use manta_block_ring::layout::PAGE;
use manta_block_ring::Region;
use vmm_storage::nvme::backend::BlockStatus;

use crate::job::Job;

/// Pages moved per step.
const STEP_PAGES: usize = 4;

/// One thread's staging buffer.
pub(crate) struct Stage(Vec<u8>);

impl Stage {
    pub(crate) fn new() -> Self {
        Self(vec![0; STEP_PAGES * PAGE])
    }
}

/// Where one part goes: `blocks` pages of the transfer from block
/// `first`, in the slot that starts at buffer page `page`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Span {
    pub(crate) q: u16,
    pub(crate) page: u32,
    pub(crate) first: u32,
    pub(crate) blocks: u32,
}

impl Span {
    /// Each step: its first buffer page, where it starts in the
    /// transfer, and its length.
    fn steps(self) -> impl Iterator<Item = (u32, usize, usize)> {
        (0..self.blocks).step_by(STEP_PAGES).map(move |done| {
            let n = (self.blocks - done).min(STEP_PAGES as u32);
            let at = (self.first + done) as usize * PAGE;
            (self.page + done, at, n as usize * PAGE)
        })
    }
}

/// Write data, guest to ring.
pub(crate) fn to_ring<J: Job>(
    region: &Region,
    job: &J,
    pages: &J::Pages,
    span: Span,
    stage: &mut Stage,
) -> Result<(), BlockStatus> {
    for (page, at, len) in span.steps() {
        let buf = &mut stage.0[..len];
        job.copy_from_guest(pages, at, buf)
            .map_err(|_| BlockStatus::DataTransfer)?;
        if !region.write_buffer(span.q, page, buf) {
            return Err(BlockStatus::Internal);
        }
    }
    Ok(())
}

/// Read data, ring to guest.
pub(crate) fn from_ring<J: Job>(
    region: &Region,
    job: &J,
    pages: &J::Pages,
    span: Span,
    stage: &mut Stage,
) -> Result<(), BlockStatus> {
    for (page, at, len) in span.steps() {
        let buf = &mut stage.0[..len];
        if !region.read_buffer(span.q, page, buf) {
            return Err(BlockStatus::Internal);
        }
        job.copy_to_guest(pages, at, buf)
            .map_err(|_| BlockStatus::DataTransfer)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_cover_the_part_once() {
        let span = Span {
            q: 0,
            page: 20,
            first: 9,
            blocks: 10,
        };
        let steps: Vec<_> = span.steps().collect();
        assert_eq!(
            steps,
            vec![
                (20, 9 * PAGE, 4 * PAGE),
                (24, 13 * PAGE, 4 * PAGE),
                (28, 17 * PAGE, 2 * PAGE),
            ]
        );
    }
}
