// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The storage interface the NVMe controller drives.
//!
//! The controller hands each I/O command to an [`AsyncBackend`] and
//! learns how it ended through the [`CompletionSink`] it gave the
//! backend when it built it. A backend completes from any thread, in
//! any order, and in batches; the controller owns no thread for it.
//!
//! The backing file or zvol runs behind a worker pool that adapts a
//! synchronous store to this interface. A shared-memory ring backend
//! maps each guest queue to one submission ring and one completion
//! ring: [`RequestId::tag`] is its ring tag, [`AsyncBackend::queue_depth`]
//! its ring depth, [`AsyncBackend::max_queues`] the queues it was
//! granted, and the thread that reaps a completion ring calls the sink.
//! It lives beside this crate, not in it, because it takes the ring
//! layout crate as a dependency.

use std::io;
use std::sync::Arc;

use vmm_core::mem::{GuestIoVec, MemCtx};
use vmm_devices::{FlushError, FlushIntent};

use super::bits::*;
use super::prp;
use super::queues::{QueueEpochs, QueueInstance};

/// Names one request from admission until its completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId {
    pub(super) queue: u16,
    pub(super) tag: u16,
    /// Tells a late or repeated completion from the request that holds
    /// the tag now.
    pub(super) serial: u64,
}

impl RequestId {
    /// The guest submission queue the request came from, from 1.
    pub fn queue(&self) -> u16 {
        self.queue
    }

    /// Below [`AsyncBackend::queue_depth`], and unique among the
    /// requests of [`Self::queue`] that have not completed.
    pub fn tag(&self) -> u16 {
        self.tag
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockOp {
    Read,
    /// `fua`: the write must be durable before it completes.
    Write {
        fua: bool,
    },
    Flush,
}

/// How a request ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockStatus {
    Success,
    /// The guest pages the request names could not be reached.
    DataTransfer,
    /// The store could not read or write the blocks.
    MediaError,
    /// Any other failure, a failed sync among them.
    Internal,
    ReadOnly,
    LbaOutOfRange,
    NoSpace,
    Unsupported,
}

impl BlockStatus {
    /// The NVMe status field for this outcome of an `opcode` command.
    pub(super) fn nvme(self, opcode: u8) -> u16 {
        match self {
            Self::Success => SC_SUCCESS,
            Self::DataTransfer => SC_DATA_XFER_ERROR,
            Self::MediaError => match opcode {
                NVM_OPC_READ => SC_UNRECOVERED_READ,
                NVM_OPC_WRITE => SC_WRITE_FAULT,
                _ => SC_INTERNAL_ERROR,
            },
            Self::Internal => SC_INTERNAL_ERROR,
            Self::ReadOnly => SC_WRITE_TO_RO_RANGE | SC_DNR,
            Self::LbaOutOfRange => SC_LBA_OUT_OF_RANGE | SC_DNR,
            Self::NoSpace => SC_CAPACITY_EXCEEDED | SC_DNR,
            Self::Unsupported => SC_INVALID_OPCODE | SC_DNR,
        }
    }
}

/// One I/O command, checked against the namespace before a backend
/// sees it: the blocks are inside it, the transfer is within MDTS, and
/// a write is to a writable namespace.
pub struct BlockRequest {
    pub(super) id: RequestId,
    pub(super) op: BlockOp,
    pub(super) lba: u64,
    pub(super) blocks: u32,
    pub(super) byte_offset: u64,
    pub(super) data: GuestData,
}

impl BlockRequest {
    pub fn id(&self) -> RequestId {
        self.id
    }

    pub fn op(&self) -> BlockOp {
        self.op
    }

    /// First logical block. Zero for a flush.
    pub fn lba(&self) -> u64 {
        self.lba
    }

    /// Logical blocks. Zero for a flush.
    pub fn blocks(&self) -> u32 {
        self.blocks
    }

    /// Where the first block starts in the store, in bytes.
    pub fn byte_offset(&self) -> u64 {
        self.byte_offset
    }

    pub fn data(&self) -> &GuestData {
        &self.data
    }
}

/// Whether the queues a request came from are still the driver's.
pub(super) struct Liveness {
    pub(super) epochs: Arc<QueueEpochs>,
    pub(super) sq: usize,
    pub(super) sq_instance: QueueInstance,
    pub(super) cq: usize,
    pub(super) cq_instance: QueueInstance,
}

/// The guest memory a request reads or writes.
///
/// Holds the PRPs, not the pages. A backend maps them on its own
/// thread, when it moves the data, and only while the queues the
/// request came from exist: after a Delete Queue or a reset those
/// pages are the driver's again.
pub struct GuestData {
    pub(super) mem: MemCtx,
    pub(super) prp1: u64,
    pub(super) prp2: u64,
    pub(super) len: usize,
    pub(super) live: Liveness,
}

impl GuestData {
    /// Bytes the request moves: its blocks times the block size.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// False once the driver has deleted a queue the request came from
    /// or reset the controller. The result can go stale at once; the
    /// controller does not finish a reset or a Delete Queue until every
    /// request that saw `true` has completed.
    pub fn is_live(&self) -> bool {
        self.live.epochs.is_current(
            self.live.sq,
            self.live.sq_instance,
            self.live.cq,
            self.live.cq_instance,
        )
    }

    fn check_live(&self) -> io::Result<()> {
        if self.is_live() {
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "the request's queue is gone",
        ))
    }

    /// Pin the pages for one vectored transfer straight to or from the
    /// store.
    pub fn iovec(&self) -> io::Result<GuestIoVec> {
        self.check_live()?;
        prp::prp_to_iovec(&self.mem, self.prp1, self.prp2, self.len)
    }

    /// Copy the data a write carries out of guest memory. `dst` must be
    /// [`Self::len`] bytes.
    pub fn copy_from_guest(&self, dst: &mut [u8]) -> io::Result<()> {
        self.check_live()?;
        prp::copy_from_prp(&self.mem, self.prp1, self.prp2, self.len, dst)
    }

    /// Copy the data a read returns into guest memory. `src` must be
    /// [`Self::len`] bytes.
    pub fn copy_to_guest(&self, src: &[u8]) -> io::Result<()> {
        self.check_live()?;
        prp::copy_to_prp(&self.mem, self.prp1, self.prp2, self.len, src)
    }
}

/// Where a backend reports finished requests.
pub trait CompletionSink: Send + Sync + 'static {
    /// Finish every request in `done`, each exactly once. The controller
    /// writes the guest completions, then raises the interrupts for the
    /// whole call. An id that is not in flight is ignored.
    fn complete(&self, done: &[(RequestId, BlockStatus)]);
}

/// A store the controller submits to and hears back from later.
pub trait AsyncBackend: Send + Sync + 'static {
    /// Bytes the namespace holds.
    fn capacity(&self) -> u64;

    /// Requests one guest queue may have in flight. The controller
    /// admits no more than this and leaves the rest in the guest's
    /// submission queue.
    fn queue_depth(&self) -> u16;

    /// Guest I/O queues this backend serves, at most 15.
    fn max_queues(&self) -> u16 {
        MAX_IO_QUEUES as u16
    }

    /// Whether a write is durable when it completes. A namespace that
    /// reports no volatile write cache needs it.
    fn durable_writes(&self) -> bool {
        false
    }

    /// Take one request.
    ///
    /// Runs with the controller lock held, on a vCPU thread or on the
    /// thread that called the sink, so it must not block on I/O. It
    /// must not call the sink before it returns: the sink takes the
    /// same lock.
    fn submit(&self, req: BlockRequest);

    /// Make every completed write durable. Nothing is in flight.
    fn flush(&self, intent: FlushIntent) -> Result<(), FlushError>;

    /// Stop for good and release the store. Nothing is submitted after.
    fn halt(&self);
}

/// Builds a backend around the sink it completes into.
pub type BackendFactory =
    Box<dyn FnOnce(Arc<dyn CompletionSink>) -> Arc<dyn AsyncBackend> + Send>;
