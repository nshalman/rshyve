// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the ring needs from a request and where it reports the end.
//!
//! The NVMe controller's [`BlockRequest`] and [`CompletionSink`] are the
//! production pair. Only the controller can make a `BlockRequest`, so the
//! ring works on these traits, and the tests drive it with requests over
//! plain memory.

use std::io;
use std::sync::Arc;

use manta_block_ring::Status;
use vmm_storage::nvme::backend::{
    BlockOp, BlockRequest, BlockStatus, CompletionSink, RequestId, Segments,
};

/// One guest I/O request.
pub trait Job: Send + 'static {
    /// What the sink is told.
    type Id: Copy + Send + 'static;
    /// The guest memory of a read or write, found once when the ring
    /// takes the request.
    type Pages: Send + 'static;

    /// Names the request to the sink.
    fn id(&self) -> Self::Id;
    /// The guest submission queue, from 1.
    fn queue(&self) -> u16;
    /// Unique among the queue's requests in flight, below the depth the
    /// ring reports.
    fn tag(&self) -> u16;
    /// Read, write or flush.
    fn op(&self) -> BlockOp;
    /// Where the data starts on the volume, in bytes.
    fn byte_offset(&self) -> u64;
    /// Bytes moved; zero for a flush.
    fn len(&self) -> usize;
    /// True when the request moves no data.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Finds the guest memory of a read or write.
    fn pages(&self) -> io::Result<Self::Pages>;
    /// The byte ranges of a deallocate or write zeroes, as offset and
    /// length.
    fn zero_ranges(&self) -> Vec<(u64, u64)> {
        Vec::new()
    }
    /// Copies `dst.len()` bytes of a write out of guest memory, from `at`
    /// bytes into the transfer.
    fn copy_from_guest(
        &self,
        pages: &Self::Pages,
        at: usize,
        dst: &mut [u8],
    ) -> io::Result<()>;
    /// Copies `src` into guest memory for a read, from `at` bytes into
    /// the transfer.
    fn copy_to_guest(
        &self,
        pages: &Self::Pages,
        at: usize,
        src: &[u8],
    ) -> io::Result<()>;
}

/// Where finished requests go, in batches, from any thread.
pub trait Sink<Id>: Send + Sync + 'static {
    /// Finishes every request in `done`.
    fn complete(&self, done: &[(Id, BlockStatus)]);
}

impl Job for BlockRequest {
    type Id = RequestId;
    type Pages = Segments;

    fn id(&self) -> RequestId {
        BlockRequest::id(self)
    }

    fn queue(&self) -> u16 {
        BlockRequest::id(self).queue()
    }

    fn tag(&self) -> u16 {
        BlockRequest::id(self).tag()
    }

    fn op(&self) -> BlockOp {
        BlockRequest::op(self)
    }

    fn byte_offset(&self) -> u64 {
        BlockRequest::byte_offset(self)
    }

    fn len(&self) -> usize {
        self.data().len()
    }

    fn pages(&self) -> io::Result<Segments> {
        self.data().segments()
    }

    fn zero_ranges(&self) -> Vec<(u64, u64)> {
        BlockRequest::zero_ranges(self).to_vec()
    }

    fn copy_from_guest(
        &self,
        pages: &Segments,
        at: usize,
        dst: &mut [u8],
    ) -> io::Result<()> {
        self.data().copy_from_guest_at(pages, at, dst)
    }

    fn copy_to_guest(
        &self,
        pages: &Segments,
        at: usize,
        src: &[u8],
    ) -> io::Result<()> {
        self.data().copy_to_guest_at(pages, at, src)
    }
}

pub(crate) struct Controller(pub(crate) Arc<dyn CompletionSink>);

impl Sink<RequestId> for Controller {
    fn complete(&self, done: &[(RequestId, BlockStatus)]) {
        self.0.complete(done);
    }
}

pub(crate) fn block_status(s: Status) -> BlockStatus {
    match s {
        Status::Success => BlockStatus::Success,
        Status::MediaError => BlockStatus::MediaError,
        Status::ReadOnly => BlockStatus::ReadOnly,
        Status::Unsupported => BlockStatus::Unsupported,
        Status::OutOfRange => BlockStatus::LbaOutOfRange,
        Status::NoSpace => BlockStatus::NoSpace,
        Status::Internal => BlockStatus::Internal,
    }
}
