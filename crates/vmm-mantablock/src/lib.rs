// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An NVMe backend that sends guest I/O to the MantaBlock engine over a
//! shared-memory ring.
//!
//! The engine never maps guest memory: data is copied between the guest
//! and the queue's buffer area.
//!
//! Not done yet:
//!
//! - **No remote path.** When the engine is lost, every request held
//!   fails with an internal error. They are meant to be replayed to
//!   another engine later, but write data is not kept past the copy into
//!   the ring for that yet.
//! - **No reattach.** Each attach asks for a new generation.
//! - **No Pause message.** The controller drains every request before it
//!   pauses, resets or exports, so the ring is empty by then.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use vmm_storage::nvme::backend::{AsyncBackend, BackendFactory, BlockRequest};
use vmm_storage::nvme::{LbaSize, NvmeConfig};

mod attach;
mod conn;
mod control;
mod copy;
mod ids;
mod job;
mod out;
mod queue;
mod reaper;
mod ring;
mod split;
mod tags;
mod wake;

#[cfg(test)]
mod tests;

pub use job::{Job, Sink};
pub use ring::Ring;

/// Most queues the NVMe controller serves.
const MAX_GUEST_QUEUES: u16 = 15;

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
    /// Where the ring logs.
    pub log: slog::Logger,
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
            spin: Duration::from_micros(50),
            allow_volatile: false,
            heartbeat: Duration::from_millis(100),
            heartbeat_limit: Duration::from_secs(1),
            io_limit: Duration::from_secs(30),
            deadline: Duration::from_secs(5),
            log,
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
        queues: cfg.queues.clamp(1, MAX_GUEST_QUEUES),
        depth: cfg.depth,
        buf_pages: cfg
            .depth
            .checked_mul(cfg.part_blocks)
            .ok_or_else(|| io::Error::other("depth times part size"))?,
        max_blocks: cfg.part_blocks,
        allow_volatile: cfg.allow_volatile,
    };
    let mut conn = conn::Conn::connect(&cfg.socket)?;
    let att = attach::attach(&mut conn, &ask, Instant::now() + cfg.deadline)?;
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

    /// 4 KiB blocks, as the ring moves whole pages, and a volatile write
    /// cache unless every write is durable when it completes.
    pub fn nvme_config(&self) -> NvmeConfig {
        NvmeConfig {
            lba_size: LbaSize::B4096,
            volatile_write_cache: !self.att.durable,
            ..NvmeConfig::default()
        }
    }

    /// Starts the ring, reporting to `sink`.
    pub fn start<J: Job>(self, sink: Arc<dyn Sink<J::Id>>) -> Arc<Ring<J>> {
        Ring::start(self.att, Some(self.conn), &self.cfg, sink)
    }

    /// The NVMe controller's backend.
    pub fn into_factory(self) -> BackendFactory {
        Box::new(move |sink| {
            let sink = Arc::new(job::Controller(sink));
            self.start::<BlockRequest>(sink) as Arc<dyn AsyncBackend>
        })
    }
}
