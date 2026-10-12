// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A ring disk that can change hands in a live migration.
//!
//! The source detaches its ring after the commit. The destination is
//! built from the source's shape with nothing attached, and attaches a
//! fresh ring only after the commit, before any device resumes. No ring
//! state crosses: the source drained every request before the export.

use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use vmm_devices::lifecycle::{BackingAttach, NvmeMigrateBackend};
use vmm_devices::{FlushError, FlushIntent};
use vmm_storage::nvme::backend::{AsyncBackend, BlockRequest, BlockStatus};
use vmm_storage::nvme::{LbaSize, NsHints, NvmeConfig};

use crate::attach::{self, Attached};
use crate::job::{Job, Sink};
use crate::{connect, Config, Connected, Ring, MAX_GUEST_QUEUES};

// The guest stays paused until this attach, and a try where the engine
// has no socket yet is one failed connect: so a short, flat retry for
// FAST_FOR, then a backoff, so an engine that stays unready is not
// asked thousands of times.
const FIRST_BACKOFF: Duration = Duration::from_millis(5);
const FAST_BACKOFF: Duration = Duration::from_millis(20);
const FAST_FOR: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_millis(500);
/// The shortest Hello and Attach a retry near the deadline gets.
const MIN_TRY: Duration = Duration::from_millis(200);

/// What the guest sees of an attachment. A destination is built from
/// the source's shape, and refuses an attachment of any other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingShape {
    /// 4 KiB blocks in the volume.
    pub blocks: u64,
    /// Logical sector: 512 or 4096.
    pub sector: u32,
    /// Queues granted.
    pub queues: u16,
    /// Ring entries per queue granted.
    pub depth: u32,
    /// Pages of buffer area per queue granted.
    pub buf_pages: u32,
    /// Requests one queue may have in flight.
    pub tags: u16,
    /// 4 KiB pages per buffer slot.
    pub part_blocks: u32,
    /// Every write is durable when it completes.
    pub durable: bool,
    /// The volume takes deallocate and write zeroes.
    pub zeroing: bool,
}

/// The text form's version. A new field takes a new version.
const SHAPE_V1: &str = "1";

impl RingShape {
    pub(crate) fn of(att: &Attached) -> Self {
        Self {
            blocks: att.volume_blocks,
            sector: att.sector_bytes,
            queues: u16::try_from(att.pipes.len()).unwrap_or(u16::MAX),
            depth: att.region.geometry().depth(),
            buf_pages: att.region.geometry().buf_pages(),
            tags: att.slots.tags,
            part_blocks: att.slots.part_blocks,
            durable: att.durable,
            zeroing: att.zeroing,
        }
    }

    /// Bytes the volume holds.
    pub fn capacity(&self) -> u64 {
        self.blocks * 4096
    }

    /// The volume's sector as the LBA size, its 4 KiB blocks as the
    /// physical block, deallocate and write zeroes when the engine takes
    /// them, and a volatile write cache unless every write is durable
    /// when it completes.
    pub fn nvme_config(&self) -> NvmeConfig {
        NvmeConfig {
            lba_size: if self.sector == 512 {
                LbaSize::B512
            } else {
                LbaSize::B4096
            },
            volatile_write_cache: !self.durable,
            deallocate: self.zeroing,
            hints: Some(NsHints {
                physical_bytes: 4096,
                optimal_write_bytes: self.part_blocks * 4096,
            }),
            ..NvmeConfig::default()
        }
    }
}

/// `1:<blocks>:<sector>:<queues>:<depth>:<buf_pages>:<tags>:<part_blocks>:<flags>`,
/// where flags are `d` (durable) and `z` (zeroing), or `-` for neither.
impl fmt::Display for RingShape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut flags = String::new();
        if self.durable {
            flags.push('d');
        }
        if self.zeroing {
            flags.push('z');
        }
        if flags.is_empty() {
            flags.push('-');
        }
        write!(
            f,
            "{SHAPE_V1}:{}:{}:{}:{}:{}:{}:{}:{flags}",
            self.blocks,
            self.sector,
            self.queues,
            self.depth,
            self.buf_pages,
            self.tags,
            self.part_blocks
        )
    }
}

impl FromStr for RingShape {
    type Err = String;

    /// Strict: the shape comes from argv, and a wrong one would show the
    /// guest another disk.
    fn from_str(s: &str) -> Result<Self, String> {
        let bad = |why: &str| format!("ring shape '{s}': {why}");
        let parts: Vec<&str> = s.split(':').collect();
        let [v, blocks, sector, queues, depth, buf, tags, part, flags] =
            parts[..]
        else {
            return Err(bad("needs 9 fields"));
        };
        if v != SHAPE_V1 {
            return Err(bad("unknown version"));
        }
        let num = |what: &str, t: &str| -> Result<u64, String> {
            if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad(&format!("bad {what}")));
            }
            t.parse::<u64>().map_err(|_| bad(&format!("bad {what}")))
        };
        let blocks = num("blocks", blocks)?;
        if blocks == 0 || blocks.checked_mul(4096).is_none() {
            return Err(bad("blocks out of range"));
        }
        let sector = match num("sector", sector)? {
            512 => 512,
            4096 => 4096,
            _ => return Err(bad("sector is 512 or 4096")),
        };
        let queues = u16::try_from(num("queues", queues)?)
            .ok()
            .filter(|q| (1..=MAX_GUEST_QUEUES).contains(q))
            .ok_or_else(|| bad("queues out of range"))?;
        let depth = u32::try_from(num("depth", depth)?)
            .ok()
            .filter(|d| d.is_power_of_two() && *d <= 4096)
            .ok_or_else(|| bad("depth is a power of 2 up to 4096"))?;
        let buf_pages = u32::try_from(num("buffer pages", buf)?)
            .ok()
            .filter(|b| *b >= 1)
            .ok_or_else(|| bad("buffer pages out of range"))?;
        let tags = u16::try_from(num("tags", tags)?)
            .ok()
            .filter(|t| *t >= 1)
            .ok_or_else(|| bad("tags out of range"))?;
        let part_blocks = u32::try_from(num("part blocks", part)?)
            .ok()
            .filter(|p| (1..=512).contains(p))
            .ok_or_else(|| bad("part blocks out of range"))?;
        let (durable, zeroing) = match flags {
            "-" => (false, false),
            "d" => (true, false),
            "z" => (false, true),
            "dz" => (true, true),
            _ => return Err(bad("flags are -, d, z or dz")),
        };
        Ok(Self {
            blocks,
            sector,
            queues,
            depth,
            buf_pages,
            tags,
            part_blocks,
            durable,
            zeroing,
        })
    }
}

/// The NVMe backend over a ring that a migration can detach and attach.
pub struct Handover<J: Job> {
    cfg: Config,
    shape: RingShape,
    sink: Arc<dyn Sink<J::Id>>,
    ring: RwLock<Option<Arc<Ring<J>>>>,
    /// One attach at a time, so two never both connect and race for
    /// the slot.
    attaching: Mutex<()>,
    /// The engine of the last attachment agreed to [`crate::MIGRATE_V1`].
    migrate_v1: AtomicBool,
    halted: AtomicBool,
}

impl<J: Job> Handover<J> {
    /// The backend over an attachment made at device create.
    pub fn attached(c: Connected, sink: Arc<dyn Sink<J::Id>>) -> Arc<Self> {
        let shape = RingShape::of(&c.att);
        let migrate_v1 = c.att.migrate_v1;
        let cfg = c.cfg.clone();
        let ring = c.start(Arc::clone(&sink));
        Arc::new(Self {
            cfg,
            shape,
            sink,
            ring: RwLock::new(Some(ring)),
            attaching: Mutex::new(()),
            migrate_v1: AtomicBool::new(migrate_v1),
            halted: AtomicBool::new(false),
        })
    }

    /// The backend of a migration destination: the source's shape and
    /// nothing attached until [`Self::attach`].
    pub fn deferred(
        mut cfg: Config,
        shape: RingShape,
        sink: Arc<dyn Sink<J::Id>>,
    ) -> Arc<Self> {
        // Ask for what the source was granted, so an engine that granted
        // it less there grants the same here.
        cfg.queues = shape.queues;
        cfg.depth = shape.depth;
        cfg.buf_pages = Some(shape.buf_pages);
        cfg.part_blocks = shape.part_blocks;
        Arc::new(Self {
            cfg,
            shape,
            sink,
            ring: RwLock::new(None),
            attaching: Mutex::new(()),
            migrate_v1: AtomicBool::new(false),
            halted: AtomicBool::new(false),
        })
    }

    /// What the guest sees.
    pub fn shape(&self) -> RingShape {
        self.shape
    }

    fn current(&self) -> Option<Arc<Ring<J>>> {
        self.ring
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(Arc::clone)
    }

    /// True while a ring is attached.
    pub fn is_attached(&self) -> bool {
        self.ring
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Takes one request. With nothing attached it fails at once: the
    /// controller keeps admission closed until an attach, so only a bug
    /// gets here.
    pub fn submit(&self, job: J) {
        let slot = self.ring.read().unwrap_or_else(|e| e.into_inner());
        match slot.as_ref() {
            Some(ring) => ring.submit(job),
            None => {
                drop(slot);
                self.sink.complete(&[(job.id(), BlockStatus::Internal)]);
            }
        }
    }

    /// Why a migration should not carry this disk unless the operator
    /// accepts the risk.
    pub fn waivable(&self) -> Option<String> {
        if !self.is_attached() || self.migrate_v1.load(Ordering::Acquire) {
            return None;
        }
        Some(format!(
            "the engine for MantaBlock volume {} does not report \
             migrate-v1; a fresh attach on the destination may reuse an \
             attachment id",
            self.cfg.volume
        ))
    }

    /// Does a fresh Attach, retrying until `how.until`: on a migration
    /// destination the engine opens the volume's socket only after the
    /// orchestrator moved the volume. The grant must have this shape.
    pub fn attach(&self, how: &BackingAttach) -> Result<(), String> {
        let _one = self.attaching.lock().unwrap_or_else(|e| e.into_inner());
        if self.is_attached() {
            return Ok(());
        }
        let mut backoff = FIRST_BACKOFF;
        let mut tries = 0u32;
        let fast_until = Instant::now() + FAST_FOR;
        loop {
            if self.halted.load(Ordering::Acquire) {
                return Err("the disk is halted".into());
            }
            let left = how.until.saturating_duration_since(Instant::now());
            tries += 1;
            let mut cfg = self.cfg.clone();
            cfg.deadline = cfg.deadline.min(left.max(MIN_TRY));
            let last = match connect(cfg) {
                Ok(c) => return self.adopt(c, how),
                Err(e) if attach::is_final(&e) => {
                    return Err(format!("attach refused for good: {e}"));
                }
                Err(e) => e,
            };
            let left = how.until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!(
                    "no attach in time after {tries} attempts: {last}"
                ));
            }
            thread::sleep(backoff.min(left));
            let cap = if Instant::now() < fast_until {
                FAST_BACKOFF
            } else {
                MAX_BACKOFF
            };
            backoff = (backoff * 2).min(cap);
        }
    }

    /// Checks a fresh attachment and starts its ring. A refused one is
    /// dropped, and the engine ends it when its socket closes.
    fn adopt(&self, c: Connected, how: &BackingAttach) -> Result<(), String> {
        let got = RingShape::of(&c.att);
        if got != self.shape {
            return Err(format!(
                "the engine granted shape {got}; the guest has {}",
                self.shape
            ));
        }
        let v1 = c.att.migrate_v1;
        if !v1 && !how.allow_unproven {
            return Err(format!(
                "the engine for volume {} does not report migrate-v1",
                self.cfg.volume
            ));
        }
        let generation = c.att.generation;
        let ring = c.start(Arc::clone(&self.sink));
        let mut slot = self.ring.write().unwrap_or_else(|e| e.into_inner());
        // A ring in the slot is never replaced: dropped under the lock,
        // its completions would call back into a controller that
        // submits under the same lock. Each refusal halts the new ring
        // outside it.
        if self.halted.load(Ordering::Acquire) {
            drop(slot);
            ring.halt();
            return Err("the disk is halted".into());
        }
        if slot.is_some() {
            drop(slot);
            ring.halt();
            return Err("a ring is attached already".into());
        }
        *slot = Some(ring);
        drop(slot);
        self.migrate_v1.store(v1, Ordering::Release);
        slog::info!(self.cfg.log, "mantablock: attached for the guest";
            "volume" => %self.cfg.volume,
            "generation" => generation,
            "migrate_v1" => v1);
        Ok(())
    }

    /// Detaches and keeps the backend for a later [`Self::attach`].
    pub fn detach(&self) -> Result<(), String> {
        // Out of the lock first: a completion thread that runs during
        // the halt may call back into the controller, which submits.
        let ring = self.ring.write().unwrap_or_else(|e| e.into_inner()).take();
        match ring {
            None => Ok(()),
            Some(ring) => {
                let done = ring.detach().map_err(|e| e.to_string());
                slog::info!(self.cfg.log, "mantablock: detached";
                    "volume" => %self.cfg.volume,
                    "confirmed" => done.is_ok());
                done
            }
        }
    }

    /// Makes every completed write durable.
    pub fn sync(&self) -> std::io::Result<()> {
        match self.current() {
            Some(ring) => ring.sync(),
            None => Ok(()),
        }
    }

    /// Final: detaches and stops every thread.
    pub fn halt(&self) {
        self.halted.store(true, Ordering::Release);
        let ring = self.ring.write().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(ring) = ring {
            ring.halt();
        }
    }
}

impl AsyncBackend for Handover<BlockRequest> {
    fn capacity(&self) -> u64 {
        self.shape.capacity()
    }

    fn queue_depth(&self) -> u16 {
        self.shape.tags
    }

    fn max_queues(&self) -> u16 {
        self.shape.queues
    }

    fn durable_writes(&self) -> bool {
        self.shape.durable
    }

    fn identity(&self) -> NvmeMigrateBackend {
        NvmeMigrateBackend::MantaBlock {
            volume: self.cfg.volume,
        }
    }

    fn migratable(&self) -> bool {
        true
    }

    fn migration_waivable(&self) -> Option<String> {
        self.waivable()
    }

    fn migration_spec_options(&self) -> Option<String> {
        Some(format!("ring-shape={}", self.shape))
    }

    fn attached(&self) -> bool {
        self.is_attached()
    }

    fn attach(&self, how: &BackingAttach) -> Result<(), String> {
        Handover::attach(self, how)
    }

    fn detach(&self) -> Result<(), String> {
        Handover::detach(self)
    }

    fn submit(&self, req: BlockRequest) {
        Handover::submit(self, req);
    }

    fn flush(&self, _intent: FlushIntent) -> Result<(), FlushError> {
        self.sync().map_err(FlushError::Sync)
    }

    fn halt(&self) {
        Handover::halt(self);
    }
}

impl<J: Job> Drop for Handover<J> {
    fn drop(&mut self) {
        self.halt();
    }
}
