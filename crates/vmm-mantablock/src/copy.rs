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

/// Where a sub-block part's guest bytes go: `len` bytes from byte `at` of
/// the transfer, at byte `buf` of the queue's buffer area.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Bytes {
    pub(crate) q: u16,
    pub(crate) buf: usize,
    pub(crate) at: usize,
    pub(crate) len: usize,
}

impl Bytes {
    /// Each step: its byte in the buffer area, where it starts in the
    /// transfer, and its length.
    fn steps(self) -> impl Iterator<Item = (usize, usize, usize)> {
        let step = STEP_PAGES * PAGE;
        (0..self.len).step_by(step).map(move |done| {
            let n = (self.len - done).min(step);
            (self.buf + done, self.at + done, n)
        })
    }
}

/// Write data of a sub-block part, guest to ring.
pub(crate) fn to_ring_bytes<J: Job>(
    region: &Region,
    job: &J,
    pages: &J::Pages,
    b: Bytes,
    stage: &mut Stage,
) -> Result<(), BlockStatus> {
    for (buf, at, len) in b.steps() {
        let tmp = &mut stage.0[..len];
        job.copy_from_guest(pages, at, tmp)
            .map_err(|_| BlockStatus::DataTransfer)?;
        if !region.write_buffer_at(b.q, buf, tmp) {
            return Err(BlockStatus::Internal);
        }
    }
    Ok(())
}

/// Read data of a sub-block part, ring to guest.
pub(crate) fn from_ring_bytes<J: Job>(
    region: &Region,
    job: &J,
    pages: &J::Pages,
    b: Bytes,
    stage: &mut Stage,
) -> Result<(), BlockStatus> {
    for (buf, at, len) in b.steps() {
        let tmp = &mut stage.0[..len];
        if !region.read_buffer_at(b.q, buf, tmp) {
            return Err(BlockStatus::Internal);
        }
        job.copy_to_guest(pages, at, tmp)
            .map_err(|_| BlockStatus::DataTransfer)?;
    }
    Ok(())
}

/// Write data of a replay, from the old region into the same pages of
/// the new one.
pub(crate) fn between(
    old: &Region,
    new: &Region,
    span: Span,
    stage: &mut Stage,
) -> Result<(), BlockStatus> {
    for (page, _, len) in span.steps() {
        let buf = &mut stage.0[..len];
        if !old.read_buffer(span.q, page, buf)
            || !new.write_buffer(span.q, page, buf)
        {
            return Err(BlockStatus::Internal);
        }
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
