// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The storage interface the NVMe controller drives.
//!
//! A backend completes through the [`CompletionSink`] it was built with,
//! from any thread, in any order, and in batches; the controller owns no
//! thread for it. A ring backend maps each guest queue to one ring pair,
//! with [`RequestId::tag`] as its ring tag.

use std::io;
use std::sync::Arc;

use vmm_core::mem::{GuestIoVec, MemCtx};
use vmm_devices::lifecycle::{BackingAttach, NvmeMigrateBackend};
use vmm_devices::{FlushError, FlushIntent};

use super::bits::*;
use super::prp;
pub use super::prp::Segments;
use super::queues::{QueueEpochs, QueueInstance};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId {
    pub(super) queue: u16,
    pub(super) tag: u16,
    /// Tells a late or repeated completion from the request that holds
    /// the tag now.
    pub(super) serial: u64,
}

impl RequestId {
    /// Guest submission queue id, from 1.
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
    /// Dataset Management with deallocate: the ranges may be freed, and
    /// read as zeros after.
    Deallocate,
    /// The blocks read as zeros after; `deallocate` lets the store free
    /// them; `fua`: the zeros must be durable before it completes.
    WriteZeroes {
        deallocate: bool,
        fua: bool,
    },
}

impl BlockOp {
    /// True for an op that zeroes ranges and moves no data.
    pub fn is_zeroing(self) -> bool {
        matches!(self, Self::Deallocate | Self::WriteZeroes { .. })
    }
}

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

/// Checked before a backend sees it: the blocks are in the namespace,
/// the transfer is within MDTS, and a write is to a writable namespace.
pub struct BlockRequest {
    pub(super) id: RequestId,
    pub(super) op: BlockOp,
    pub(super) lba: u64,
    pub(super) blocks: u32,
    pub(super) byte_offset: u64,
    pub(super) data: GuestData,
    /// For a zeroing op: the byte ranges, as offset and length.
    pub(super) zero: Vec<(u64, u64)>,
}

impl BlockRequest {
    pub fn id(&self) -> RequestId {
        self.id
    }

    pub fn op(&self) -> BlockOp {
        self.op
    }

    /// Zero for a flush.
    pub fn lba(&self) -> u64 {
        self.lba
    }

    /// Zero for a flush.
    pub fn blocks(&self) -> u32 {
        self.blocks
    }

    /// Where `lba` starts in the store, in bytes.
    pub fn byte_offset(&self) -> u64 {
        self.byte_offset
    }

    pub fn data(&self) -> &GuestData {
        &self.data
    }

    /// The byte ranges a zeroing op names, each inside the namespace and
    /// not empty; none for any other op.
    pub fn zero_ranges(&self) -> &[(u64, u64)] {
        &self.zero
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
/// Holds the PRPs, not the pages. A backend maps them only when it
/// moves the data, and only while the request's queues exist: after a
/// Delete Queue or a reset those pages are the driver's again.
///
/// Not to be used once the request is completed. The check here sees
/// only the queues, and the driver may reuse the pages as soon as it
/// reads the completion.
pub struct GuestData {
    pub(super) mem: MemCtx,
    pub(super) prp1: u64,
    pub(super) prp2: u64,
    pub(super) len: usize,
    pub(super) live: Liveness,
}

impl GuestData {
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

    pub fn iovec(&self) -> io::Result<GuestIoVec> {
        self.check_live()?;
        prp::prp_to_iovec(&self.mem, self.prp1, self.prp2, self.len)
    }

    /// `dst` must be [`Self::len`] bytes.
    pub fn copy_from_guest(&self, dst: &mut [u8]) -> io::Result<()> {
        self.check_live()?;
        prp::copy_from_prp(&self.mem, self.prp1, self.prp2, self.len, dst)
    }

    /// `src` must be [`Self::len`] bytes.
    pub fn copy_to_guest(&self, src: &[u8]) -> io::Result<()> {
        self.check_live()?;
        prp::copy_to_prp(&self.mem, self.prp1, self.prp2, self.len, src)
    }

    /// The guest runs, for a backend that moves the data in pieces with
    /// [`Self::copy_from_guest_at`] and [`Self::copy_to_guest_at`].
    pub fn segments(&self) -> io::Result<Segments> {
        self.check_live()?;
        prp::prp_segments(&self.mem, self.prp1, self.prp2, self.len)
    }

    /// Copies `dst.len()` bytes of the transfer from `at`. `segs` must
    /// come from this request.
    pub fn copy_from_guest_at(
        &self,
        segs: &Segments,
        at: usize,
        dst: &mut [u8],
    ) -> io::Result<()> {
        self.check_live()?;
        segs.copy_out_at(&self.mem, at, dst)
    }

    /// Copies `src` into the transfer at `at`. `segs` must come from
    /// this request.
    pub fn copy_to_guest_at(
        &self,
        segs: &Segments,
        at: usize,
        src: &[u8],
    ) -> io::Result<()> {
        self.check_live()?;
        segs.copy_in_at(&self.mem, at, src)
    }
}

pub trait CompletionSink: Send + Sync + 'static {
    /// Each request completes exactly once; an id that is not in flight
    /// is ignored. Interrupts go out once for the whole call.
    fn complete(&self, done: &[(RequestId, BlockStatus)]);
}

pub trait AsyncBackend: Send + Sync + 'static {
    /// In bytes.
    fn capacity(&self) -> u64;

    /// Requests one guest queue may have in flight, at most 4096. The
    /// rest wait in the guest's submission queue.
    fn queue_depth(&self) -> u16;

    /// Guest I/O queues this backend serves, at most 15.
    fn max_queues(&self) -> u16 {
        MAX_IO_QUEUES as u16
    }

    /// A namespace that reports no volatile write cache needs this.
    fn durable_writes(&self) -> bool {
        false
    }

    /// The store a migration payload names, so a destination on another
    /// store refuses it.
    fn identity(&self) -> NvmeMigrateBackend {
        NvmeMigrateBackend::File
    }

    /// False while a migration cannot hand this store to the
    /// destination.
    fn migratable(&self) -> bool {
        true
    }

    /// Why a migration should not carry this store, when an operator
    /// may still accept the risk. Asked only when [`Self::migratable`]
    /// is true.
    fn migration_waivable(&self) -> Option<String> {
        None
    }

    /// Options a migration destination adds to this disk's `-s` spec.
    fn migration_spec_options(&self) -> Option<String> {
        None
    }

    /// False while nothing is attached and no request may come.
    fn attached(&self) -> bool {
        true
    }

    /// Attach the store, retrying until `how.until`. Called with
    /// nothing in flight and admission closed.
    fn attach(&self, _how: &BackingAttach) -> Result<(), String> {
        Ok(())
    }

    /// Give the store up and keep the backend for a later
    /// [`Self::attach`]. Called with nothing in flight and admission
    /// closed.
    fn detach(&self) -> Result<(), String> {
        Ok(())
    }

    /// Runs with the controller lock held, on a vCPU thread or a sink
    /// caller, so it must not block on I/O. It may complete the request
    /// before it returns. Nothing of `req` may touch guest memory after
    /// its completion.
    fn submit(&self, req: BlockRequest);

    /// Called with nothing in flight.
    fn flush(&self, intent: FlushIntent) -> Result<(), FlushError>;

    /// Final: nothing is submitted after.
    fn halt(&self);
}

/// Builds a backend around the sink it completes into.
/// Fails when the backend cannot start, for example a worker thread
/// that will not spawn.
pub type BackendFactory = Box<
    dyn FnOnce(
            Arc<dyn CompletionSink>,
        ) -> io::Result<Arc<dyn AsyncBackend>>
        + Send,
>;

/// Stands in for a backend that failed to start, only until the
/// controller that holds it is dropped. Nothing reaches it.
pub(super) struct Unbuilt;

impl AsyncBackend for Unbuilt {
    fn capacity(&self) -> u64 {
        0
    }

    fn queue_depth(&self) -> u16 {
        1
    }

    fn submit(&self, _req: BlockRequest) {}

    fn flush(&self, _intent: FlushIntent) -> Result<(), FlushError> {
        Ok(())
    }

    fn halt(&self) {}
}
