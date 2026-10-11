// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An NVMe backend that sends guest I/O to the MantaBlock engine over a
//! shared-memory ring.
//!
//! The engine never maps guest memory: data is copied between the guest
//! and the queue's buffer area.
//!
//! When the engine is lost, the ring holds every request and reattaches
//! to the engine's socket with the same generation for a bounded time.
//! It replays what was in flight with the same op ids, the write data
//! copied from the old region, then goes on. Past the bound, everything
//! held fails with an internal error.
//!
//! A live migration moves the disk through [`Handover`]: the source
//! detaches after the commit, and the destination attaches a fresh ring
//! before the guest runs (spec `61`).
//!
//! Not done yet:
//!
//! - **No remote path.** Only the same engine socket is tried.
//! - **No Pause message.** The controller drains every request before it
//!   pauses, resets or exports, so the ring is empty by then.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use vmm_storage::nvme::backend::{AsyncBackend, BackendFactory, BlockRequest};
use vmm_storage::nvme::NvmeConfig;

mod attach;
mod conn;
mod control;
mod copy;
mod handover;
mod ids;
mod job;
mod out;
mod queue;
mod reaper;
mod reattach;
mod ring;
mod shape;
mod split;
mod tags;
mod wake;

#[cfg(test)]
mod tests;

pub use handover::{Handover, RingShape};
pub use job::{Job, Sink};
pub use ring::Ring;

/// Most queues the NVMe controller serves.
const MAX_GUEST_QUEUES: u16 = 15;

/// Ring Hello feature: a fresh Attach takes a generation above every
/// one the volume used on any CN (spec `61` §6.3 rule 1). Without it a
/// migration destination may reuse an attachment id.
///
/// manta-block-ring does not name this bit yet. An engine that does not
/// know it never sets it, as it answers only bits it knows.
pub const MIGRATE_V1: u64 = 1 << 3;

/// How to reach the engine and what to ask it for.
#[derive(Clone)]
pub struct Config {
    /// The engine's socket for this volume.
    pub socket: PathBuf,
    /// The volume.
    pub volume: uuid::Uuid,
    /// Queues to ask for; the guest may use at most 15.
    pub queues: u16,
    /// Ring entries per queue to ask for: a power of 2.
    pub depth: u32,
    /// 4 KiB pages per buffer slot to ask for. A request larger than a
    /// slot goes as several parts.
    pub part_blocks: u32,
    /// Pages of buffer area per queue to ask for; `depth` times
    /// `part_blocks` when `None`.
    pub buf_pages: Option<u32>,
    /// How long a completion thread polls after its last completion
    /// before it sleeps. The next completion after a sleep waits for a
    /// pipe wake-up, about 15 µs, so this should cover one engine batch.
    /// While completions come closer together than this, the thread
    /// keeps one CPU busy; idle, it sleeps and wakes every 50 ms to look
    /// for a halt.
    pub spin: Duration,
    /// Take an engine whose writes are not durable when they complete.
    /// For tests and measurement: the namespace must then show a
    /// volatile write cache.
    pub allow_volatile: bool,
    /// How often to Ping.
    pub heartbeat: Duration,
    /// The engine is lost when no Pong comes within this.
    pub heartbeat_limit: Duration,
    /// The engine is lost when a request stays on the ring longer.
    pub io_limit: Duration,
    /// Time for Hello, Attach and each control message.
    pub deadline: Duration,
    /// How long to hold I/O and try to reattach after the engine is lost.
    /// Zero fails held I/O at once.
    pub reattach: Duration,
    /// Where the ring logs.
    pub log: slog::Logger,
    /// Time a reattach spends before it goes live, as a slow replay would.
    #[cfg(test)]
    pub(crate) replay_stall: Duration,
}

impl Config {
    /// The defaults for `volume` at `socket`.
    pub fn new(socket: PathBuf, volume: uuid::Uuid, log: slog::Logger) -> Self {
        Self {
            socket,
            volume,
            queues: 4,
            depth: 64,
            part_blocks: 32,
            buf_pages: None,
            spin: Duration::from_micros(50),
            allow_volatile: false,
            heartbeat: Duration::from_millis(100),
            heartbeat_limit: Duration::from_secs(1),
            io_limit: Duration::from_secs(30),
            deadline: Duration::from_secs(5),
            reattach: Duration::from_secs(30),
            log,
            #[cfg(test)]
            replay_stall: Duration::ZERO,
        }
    }
}

/// An attachment that is ready for I/O and has no threads yet.
pub struct Connected {
    conn: conn::Conn,
    att: attach::Attached,
    cfg: Config,
}

/// Connects to the engine and attaches the volume.
pub fn connect(cfg: Config) -> io::Result<Connected> {
    let ask = attach::Ask {
        volume: *cfg.volume.as_bytes(),
        generation: 0,
        queues: cfg.queues.clamp(1, MAX_GUEST_QUEUES),
        depth: cfg.depth,
        buf_pages: match cfg.buf_pages {
            Some(pages) => pages,
            None => cfg
                .depth
                .checked_mul(cfg.part_blocks)
                .ok_or_else(|| io::Error::other("depth times part size"))?,
        },
        max_blocks: cfg.part_blocks,
        allow_volatile: cfg.allow_volatile,
    };
    let deadline = Instant::now() + cfg.deadline;
    let mut conn = conn::Conn::connect(&cfg.socket, deadline)?;
    let att = attach::attach(&mut conn, &ask, deadline)?;
    slog::info!(cfg.log, "mantablock: attached";
        "volume" => %cfg.volume,
        "generation" => att.generation,
        "queues" => att.pipes.len(),
        "depth" => att.slots.tags,
        "part_blocks" => att.slots.part_blocks,
        "durable" => att.durable);
    Ok(Connected { conn, att, cfg })
}

impl Connected {
    /// Bytes the volume holds.
    pub fn capacity(&self) -> u64 {
        self.att.volume_blocks * 4096
    }

    /// Whether a write is durable when it completes.
    pub fn durable(&self) -> bool {
        self.att.durable
    }

    /// The volume's logical sector, fixed at its create: 512 or 4096.
    pub fn sector_bytes(&self) -> u32 {
        self.att.sector_bytes
    }

    /// The volume's sector as the LBA size, its 4 KiB blocks as the
    /// physical block, deallocate and write zeroes when the engine takes
    /// them, and a volatile write cache unless every write is durable when
    /// it completes.
    pub fn nvme_config(&self) -> NvmeConfig {
        self.shape().nvme_config()
    }

    /// What the guest sees of this attachment.
    pub fn shape(&self) -> RingShape {
        RingShape::of(&self.att)
    }

    /// Whether the engine agreed to [`MIGRATE_V1`].
    pub fn migrate_v1(&self) -> bool {
        self.att.migrate_v1
    }

    /// Starts the ring, reporting to `sink`.
    pub fn start<J: Job>(self, sink: Arc<dyn Sink<J::Id>>) -> Arc<Ring<J>> {
        Ring::start(self.att, Some(self.conn), &self.cfg, sink)
    }

    /// The NVMe controller's backend, which a live migration can
    /// detach and attach again.
    pub fn into_handover_factory(self) -> BackendFactory {
        Box::new(move |sink| {
            let sink = Arc::new(job::Controller(sink));
            Handover::<BlockRequest>::attached(self, sink)
                as Arc<dyn AsyncBackend>
        })
    }

    /// The NVMe controller's backend, with no handover.
    pub fn into_factory(self) -> BackendFactory {
        Box::new(move |sink| {
            let sink = Arc::new(job::Controller(sink));
            self.start::<BlockRequest>(sink) as Arc<dyn AsyncBackend>
        })
    }
}

/// The backend of a migration destination: `shape` as the source had
/// it, and nothing attached until the guest is about to run.
pub fn deferred_factory(cfg: Config, shape: RingShape) -> BackendFactory {
    Box::new(move |sink| {
        let sink = Arc::new(job::Controller(sink));
        Handover::<BlockRequest>::deferred(cfg, shape, sink)
            as Arc<dyn AsyncBackend>
    })
}
