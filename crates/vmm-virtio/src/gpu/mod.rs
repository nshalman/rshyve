// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VirtIO GPU device (type 16), 2D and guest-memory blobs.
//!
//! The guest draws into resources it backs with its own pages, copies
//! changed rectangles to the host with `TRANSFER_TO_HOST_2D`, and says
//! which rectangle of the scanout changed with `RESOURCE_FLUSH`. That
//! flush is what a remote display needs and fbuf cannot provide: the
//! device knows exactly what changed, so a display server sends only
//! that.
//!
//! The host side of the picture is a [`GpuDisplay`]: the pixels of
//! scanout 0 in B8G8R8X8 byte order (the XRGB8888 that fbuf and RFB
//! use), the damage accumulated since a display server last took it,
//! and the cursor, which the guest sends on its own queue so a client
//! can draw it locally.
//!
//! A guest that takes `VIRTIO_GPU_F_RESOURCE_BLOB` may instead make
//! its buffers blobs of its own memory (`BLOB_MEM_GUEST`). The device
//! then reads the pages a flush names straight onto the display, with
//! no host copy of the resource in between, and a transfer is a no-op.
//!
//! Linux fences the flush of a blob primary plane and waits for its
//! answer before it flips again, so the device paces the guest's frame
//! rate by answering a fenced flush no sooner than an interval after the
//! last: [`FLIP_INTERVAL`] when nobody asks, or the pace a display server
//! asks for, between 60 and 10 frames a second. The flush itself is shown
//! at once; only its answer waits, on a thread of the device's own.
//!
//! Scope: one scanout, the two 32-bit formats Linux and Windows use for
//! scanouts and cursors, no EDID, no 3D, no host-memory blobs. Requests
//! are served inline on the notifying vCPU.

pub mod protocol;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use vmm_core::mem::PhysMap;
use vmm_core::ratelimit::TokenBucket;
use vmm_devices::lifecycle::{
    DeviceStateError, GpuMigrateBlob, GpuMigrateBlobImage, GpuMigrateCursor,
    GpuMigrateResource, GpuMigrateScanout, GpuMigrateState,
    VirtioDeviceMigrateState,
};
use vmm_devices::Lifecycle;
use zerocopy::{FromBytes, IntoBytes};

use super::access::{GuestAccess, RingTag};
use super::chain_io::{ChainReader, ChainWriter};
use super::pci::intr::{BackendIntr, IntrSession, IntrSlot};
use super::queue::{ChainBuf, VirtQueue, VirtioCompletion};
use super::VirtioDevice;
use protocol::*;

/// DTrace probes for the flip pacing. `head` is the flip's descriptor
/// head, so a script can pair an answer with the flip it answers.
#[usdt::provider(provider = "vmm")]
mod probes {
    /// A fenced flush arrived. `held` is 1 when its answer waits for
    /// the pacer, 0 when it went back at once; `interval_us` is the pace.
    fn gpu_flip(head: u16, held: u8, interval_us: u64) {}
    /// The pacer published a held flip's answer.
    fn gpu_flip_answer(head: u16) {}
}

/// The control queue carries every 2D command.
pub const QUEUE_CONTROL: u16 = 0;
/// The cursor queue carries cursor updates and moves.
pub const QUEUE_CURSOR: u16 = 1;
/// virtio-gpu has these two queues.
pub const NUM_QUEUES: usize = 2;

/// Size of the device config: events_read, events_clear, num_scanouts,
/// num_capsets.
pub const CONFIG_SIZE: u16 = 16;

/// Host memory every resource together may hold: their pixels, page
/// lists and records.
///
/// A resource's dimensions come from the guest and become a host
/// allocation, so without a cap one request could ask for 1 GiB. This
/// fits a 4K scanout double-buffered with room to spare. The display
/// surface a scanout is copied to is apart from it and no larger than
/// it, so the device holds at most twice this.
pub const MAX_HOST_BYTES: usize = 256 * 1024 * 1024;

/// Backing entries one resource may have. Linux sends one per page when
/// the pages are not contiguous, so this covers [`MAX_HOST_BYTES`] of
/// 4 KiB pages.
pub const MAX_BACKING_ENTRIES: u32 = (MAX_HOST_BYTES / 4096) as u32;

/// Width and height of a cursor image, which the spec fixes.
pub const CURSOR_SIZE: u32 = 64;

/// How soon after one flip the device answers the next when no display
/// server has asked for a pace: 30 Hz, VNC's frame rate. A guest that
/// flips faster with nobody pacing it spends vCPU time, and the host
/// copies, on frames no client sees.
pub const FLIP_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 30);

/// The fastest a display server may ask the guest to flip: 60 Hz, what
/// a client's display shows. Faster would be frames nobody sees.
pub const FASTEST_FLIP_INTERVAL: Duration =
    Duration::from_nanos(1_000_000_000 / 60);

/// The slowest a display server may ask the guest to flip. Linux waits
/// at most 50 ms for a flip's answer and then flips again, blocking only
/// when it comes back to a buffer whose answer is still out: past 50 ms
/// a request slows the guest less, and holds its compositor longer.
pub const SLOWEST_FLIP_INTERVAL: Duration = Duration::from_millis(100);

/// Refusals the device logs in a burst, and how long the bucket takes to
/// refill: enough to see what the guest asked for, not enough for it to
/// fill the log.
const REFUSAL_LOG_BURST: u32 = 4;
const REFUSAL_LOG_PERIOD: Duration = Duration::from_secs(15);

/// Every supported format has four bytes per pixel.
const BYTES_PER_PIXEL: usize = 4;

// ---------------------------------------------------------------------------
// Host-side display
// ---------------------------------------------------------------------------

/// The pixels of scanout 0 as the host shows them.
#[derive(Debug, Default)]
pub struct Surface {
    pub width: u32,
    pub height: u32,
    /// `width * height` pixels, B8G8R8X8, rows packed.
    pub pixels: Vec<u8>,
}

/// The cursor image and where it is.
#[derive(Debug, Clone)]
pub struct Cursor {
    pub x: u32,
    pub y: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    /// [`CURSOR_SIZE`] square, B8G8R8A8.
    pub pixels: Vec<u8>,
}

#[derive(Debug, Default)]
struct DisplayState {
    surface: Surface,
    damage: Option<Rect>,
    cursor: Option<Cursor>,
    /// How many times a resource was copied onto the surface.
    copies: usize,
    /// Whether the driver has set the cursor since the last reset:
    /// without, `None` is "no pointer set" rather than "hidden".
    cursor_set: bool,
    /// Changes whenever the cursor image or its visibility does.
    cursor_serial: u64,
}

/// Runs when the device config changes; the transport's
/// configuration change interrupt.
pub type ConfigNotify = Box<dyn Fn() + Send + Sync>;

/// What a display server reads: the scanout, its damage and the cursor.
/// It also carries the mode the device offers the guest, because a
/// server's size request changes it.
#[derive(Default)]
pub struct GpuDisplay {
    state: Mutex<DisplayState>,
    /// The mode `GET_DISPLAY_INFO` offers for scanout 0.
    preferred: Mutex<(u32, u32)>,
    /// Config `events_read`: events the driver has not cleared.
    events: std::sync::atomic::AtomicU32,
    /// Tells the transport the config changed.
    notify: Mutex<Option<ConfigNotify>>,
    /// How far apart a display server asked for flips, in nanoseconds;
    /// 0 when none asked. Shared with the device's pacing.
    asked: Arc<std::sync::atomic::AtomicU64>,
    /// Servers to wake when the picture or the pointer changes.
    wakers: vmm_devices::display::Wakers,
}

impl GpuDisplay {
    fn preferred(&self) -> (u32, u32) {
        *self.preferred.lock().expect("virtio-gpu: preferred lock")
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, DisplayState> {
        self.state.lock().expect("virtio-gpu: display lock")
    }

    /// The scanout's size; `(0, 0)` when it is disabled.
    pub fn size(&self) -> (u32, u32) {
        let st = self.lock();
        (st.surface.width, st.surface.height)
    }

    /// The rectangle that changed since the last call, if any.
    pub fn take_damage(&self) -> Option<Rect> {
        self.lock().damage.take()
    }

    /// Run `f` over the current pixels.
    pub fn with_surface<R>(&self, f: impl FnOnce(&Surface) -> R) -> R {
        f(&self.lock().surface)
    }

    /// The cursor, or `None` when the guest hid it.
    pub fn cursor(&self) -> Option<Cursor> {
        self.lock().cursor.clone()
    }

    /// Set the cursor the driver asked for; `None` hides it.
    fn set_cursor(&self, cursor: Option<Cursor>) {
        {
            let mut st = self.lock();
            st.cursor = cursor;
            st.cursor_set = true;
            st.cursor_serial = st.cursor_serial.wrapping_add(1);
        }
        self.wakers.wake();
    }

    /// Forget the cursor: no driver has set one.
    fn forget_cursor(&self) {
        {
            let mut st = self.lock();
            st.cursor = None;
            st.cursor_set = false;
            st.cursor_serial = st.cursor_serial.wrapping_add(1);
        }
        self.wakers.wake();
    }

    fn damage(st: &mut DisplayState, r: Rect) {
        st.damage = Some(match st.damage {
            Some(d) => d.union(&r),
            None => r,
        });
    }

    /// Show a scanout of `width` x `height`, all of it damaged. A new
    /// size starts blank; the same size keeps the buffer, as a page flip
    /// does, for the caller to fill.
    fn resize(&self, width: u32, height: u32) {
        let mut st = self.lock();
        if (st.surface.width, st.surface.height) != (width, height) {
            st.surface = Surface {
                width,
                height,
                pixels: vec![
                    0;
                    width as usize * height as usize * BYTES_PER_PIXEL
                ],
            };
        }
        st.damage = (width > 0 && height > 0).then_some(Rect {
            x: 0,
            y: 0,
            width,
            height,
        });
    }
}

impl vmm_devices::display::DisplaySource for GpuDisplay {
    fn size(&self) -> (u32, u32) {
        GpuDisplay::size(self)
    }

    /// Offer the guest `width` x `height` and raise the display event.
    /// A mode [`preferred_fits`] refuses is refused.
    fn request_size(&self, width: u32, height: u32) -> bool {
        if !preferred_fits(width, height) {
            return false;
        }
        {
            let mut preferred =
                self.preferred.lock().expect("virtio-gpu: preferred lock");
            // Already offered: the driver has been told, or will read
            // it when it next asks.
            if *preferred == (width, height) {
                return true;
            }
            *preferred = (width, height);
        }
        self.events
            .fetch_or(EVENT_DISPLAY, std::sync::atomic::Ordering::AcqRel);
        if let Some(notify) = self
            .notify
            .lock()
            .expect("virtio-gpu: notify lock")
            .as_ref()
        {
            notify();
        }
        true
    }

    fn cursor_serial(&self) -> u64 {
        self.lock().cursor_serial
    }

    fn on_change(
        &self,
        waker: std::sync::Weak<dyn vmm_devices::display::DisplayWaker>,
    ) {
        self.wakers.add(waker);
    }

    fn pace(&self, interval: Option<Duration>) {
        let nanos = interval.map_or(0, |i| {
            u64::try_from(i.as_nanos()).unwrap_or(u64::MAX).max(1)
        });
        self.asked
            .store(nanos, std::sync::atomic::Ordering::Relaxed);
    }

    fn cursor(&self) -> vmm_devices::display::CursorShape {
        use vmm_devices::display::{CursorImage, CursorShape};
        let st = self.lock();
        match &st.cursor {
            _ if !st.cursor_set => CursorShape::Default,
            None => CursorShape::Hidden,
            Some(c) => CursorShape::Image(Arc::new(CursorImage {
                width: CURSOR_SIZE,
                height: CURSOR_SIZE,
                hot_x: c.hot_x,
                hot_y: c.hot_y,
                pixels: c.pixels.clone(),
            })),
        }
    }

    fn take_damage(&self) -> Option<vmm_devices::display::Rect> {
        GpuDisplay::take_damage(self).map(|r| vmm_devices::display::Rect {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        })
    }

    fn read_rect(
        &self,
        r: vmm_devices::display::Rect,
        dst: &mut [u8],
    ) -> std::io::Result<()> {
        let bad =
            |what| std::io::Error::new(std::io::ErrorKind::InvalidInput, what);
        let st = self.lock();
        let s = &st.surface;
        let wire = Rect {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        };
        if !wire.fits_within(s.width, s.height) {
            return Err(bad("rectangle outside the display"));
        }
        let row_bytes = r.width as usize * BYTES_PER_PIXEL;
        if dst.len() != row_bytes * r.height as usize {
            return Err(bad("destination does not match the rectangle"));
        }
        let stride = s.width as usize * BYTES_PER_PIXEL;
        for row in 0..r.height as usize {
            let at =
                (r.y as usize + row) * stride + r.x as usize * BYTES_PER_PIXEL;
            dst[row * row_bytes..][..row_bytes]
                .copy_from_slice(&s.pixels[at..][..row_bytes]);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Device state
// ---------------------------------------------------------------------------

struct Resource {
    width: u32,
    height: u32,
    /// Host copy, `width * height` pixels in the resource's format.
    pixels: Vec<u8>,
    /// Guest pages, in order, from `ATTACH_BACKING`.
    backing: Option<Vec<MemEntry>>,
    /// Bumped by every transfer, so the display can tell whether it
    /// already shows the resource's current pixels.
    version: u64,
}

/// What each resource and blob costs the host beyond its pixels and
/// page list: its record and its map entry. Charged to the budget, so a
/// guest cannot make unbounded host memory out of 1x1 resources at four
/// bytes apiece.
const RESOURCE_OVERHEAD: usize = 256;

/// Whether the device may offer the guest a `width` x `height` display:
/// a mode that is not empty and whose pixels take at most half the
/// resource budget.
fn preferred_fits(width: u32, height: u32) -> bool {
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(BYTES_PER_PIXEL))
        .is_some_and(|n| n > 0 && n <= MAX_HOST_BYTES / 2)
}

/// The host memory a backing page list of `entries` takes.
fn backing_bytes(entries: usize) -> usize {
    entries.saturating_mul(size_of::<MemEntry>())
}

impl Resource {
    /// The host memory this resource holds: its record, its pixels and
    /// its backing page list.
    fn host_bytes(&self) -> usize {
        RESOURCE_OVERHEAD
            + self.pixels.len()
            + backing_bytes(self.backing.as_ref().map_or(0, Vec::len))
    }

    fn stride(&self) -> usize {
        self.width as usize * BYTES_PER_PIXEL
    }
}

/// A blob in guest memory: the device reads its pages where they are,
/// so it holds no pixels of its own.
struct Blob {
    size: u64,
    /// Guest pages, in order, from `RESOURCE_CREATE_BLOB`.
    backing: Vec<MemEntry>,
}

impl Blob {
    /// Its record and its page list.
    fn host_bytes(&self) -> usize {
        RESOURCE_OVERHEAD + backing_bytes(self.backing.len())
    }
}

/// How a blob holds the image a scanout shows: `width` x `height`
/// pixels, rows `stride` bytes apart from `offset`.
#[derive(Clone, Copy)]
struct BlobImage {
    width: u32,
    height: u32,
    stride: u32,
    offset: u32,
}

impl BlobImage {
    /// Where pixel (`x`, `y`) of the image is in the blob. With `x`
    /// in a row no longer than the stride, as in an image that
    /// [`Self::fits`], this is at most (`y` + 1) strides plus a u32
    /// offset, which a u64 holds.
    fn at(&self, x: u32, y: u32) -> u64 {
        u64::from(self.offset)
            + u64::from(y) * u64::from(self.stride)
            + u64::from(x) * BYTES_PER_PIXEL as u64
    }

    /// Whether the image lies inside a blob of `size` bytes with rows
    /// that do not overlap, and `window` is a part of it no bigger than
    /// a resource the budget could hold, whatever size the blob claims.
    fn fits(&self, size: u64, window: &Rect) -> bool {
        let row = u64::from(self.width) * BYTES_PER_PIXEL as u64;
        let inside = self.width > 0
            && self.height > 0
            && u64::from(self.stride) >= row
            && self
                .at(0, self.height - 1)
                .checked_add(row)
                .is_some_and(|end| end <= size);
        let window_fits = u64::from(window.width)
            .checked_mul(u64::from(window.height))
            .and_then(|n| n.checked_mul(BYTES_PER_PIXEL as u64))
            .is_some_and(|bytes| bytes <= MAX_HOST_BYTES as u64);
        inside
            && window.width > 0
            && window.height > 0
            && window.fits_within(self.width, self.height)
            && window_fits
    }
}

/// Which part of which resource scanout 0 shows.
#[derive(Clone, Copy)]
struct Scanout {
    resource_id: u32,
    r: Rect,
    /// The image, when the resource is a blob.
    blob: Option<BlobImage>,
}

#[derive(Default)]
struct GpuState {
    resources: HashMap<u32, Resource>,
    blobs: HashMap<u32, Blob>,
    /// What the resources and blobs hold: the sum of their
    /// `host_bytes()`. Changed through the methods below, and set whole
    /// on a restore.
    host_bytes: usize,
    scanout: Option<Scanout>,
    /// The resource, and its version, whose whole scanout rectangle the
    /// display shows: what the last scanout switch copied.
    shown: Option<(u32, u64)>,
}

impl GpuState {
    /// Whether a resource of either kind has `id`.
    fn has(&self, id: u32) -> bool {
        self.resources.contains_key(&id) || self.blobs.contains_key(&id)
    }

    /// The host memory held with `new` more in place of `old`, if that
    /// fits the budget.
    fn replacing(&self, old: usize, new: usize) -> Result<usize, u32> {
        debug_assert!(old <= self.host_bytes, "{old} of {}", self.host_bytes);
        self.host_bytes
            .saturating_sub(old)
            .checked_add(new)
            .filter(|&total| total <= MAX_HOST_BYTES)
            .ok_or(RESP_ERR_OUT_OF_MEMORY)
    }

    /// The host memory resource `id`'s backing page list takes.
    fn backing_of(&self, id: u32) -> Result<usize, u32> {
        let res = self
            .resources
            .get(&id)
            .ok_or(RESP_ERR_INVALID_RESOURCE_ID)?;
        Ok(backing_bytes(res.backing.as_ref().map_or(0, Vec::len)))
    }

    /// Hold `cost` more, if it fits the budget.
    fn charge(&mut self, cost: usize) -> CmdResult {
        self.host_bytes = self.replacing(0, cost)?;
        Ok(())
    }

    /// Hold `cost` less: what something charged gives back.
    fn release(&mut self, cost: usize) {
        debug_assert!(cost <= self.host_bytes, "{cost} of {}", self.host_bytes);
        self.host_bytes = self.host_bytes.saturating_sub(cost);
    }
}

/// A request failed with this response type.
type CmdResult = Result<(), u32>;

/// VirtIO GPU device, 2D and guest-memory blobs, one scanout.
pub struct VirtioGpu {
    log: slog::Logger,
    /// Caps the refusals logged: the guest chooses how often it makes
    /// them.
    refusals: TokenBucket,
    state: Mutex<GpuState>,
    display: Arc<GpuDisplay>,
    /// Run once at halt: how a display server outside this crate stops.
    halt_hooks: Mutex<Vec<HaltHook>>,
    pacing: Arc<Pacing>,
    pacer: Mutex<Option<thread::JoinHandle<()>>>,
}

/// The control ring's answers, and the flips among them that wait.
///
/// Every answer on the control ring goes through one completion
/// handler, because one of them may be published late, off the vCPU.
/// That happens inside a [`GuestAccess`] session, so a reset can take
/// the ring back and know nothing waiting writes to it afterwards.
struct Pacing {
    physmap: Arc<PhysMap>,
    access: GuestAccess,
    interrupt: Arc<IntrSlot>,
    /// For the ring the driver programmed; dropped when it retires it.
    completion: Mutex<Option<Arc<VirtioCompletion>>>,
    flips: Mutex<Flips>,
    wake: Condvar,
    /// The device's own pace, when no display server asks for one.
    interval: Duration,
    /// What a display server asked for; see [`GpuDisplay::asked`].
    asked: Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Default)]
struct Flips {
    /// Answers written to the guest but not yet published.
    waiting: Vec<Waiting>,
    /// When the next flip may be answered.
    next: Option<Instant>,
    /// Nothing waits while the device is paused: a migration exports
    /// the rings next.
    paused: bool,
    /// The pacer is publishing outside the lock.
    busy: bool,
    shutdown: bool,
}

/// A flip's answer, written and waiting to be published.
struct Waiting {
    tag: RingTag,
    head: u16,
    len: u32,
}

impl Pacing {
    /// How far apart flips are answered now: the device's own pace, or
    /// what a display server asked for, held between
    /// [`FASTEST_FLIP_INTERVAL`] and [`SLOWEST_FLIP_INTERVAL`].
    fn interval(&self) -> Duration {
        match self.asked.load(std::sync::atomic::Ordering::Relaxed) {
            0 => self.interval,
            asked => Duration::from_nanos(asked)
                .clamp(FASTEST_FLIP_INTERVAL, SLOWEST_FLIP_INTERVAL),
        }
    }

    fn flips(&self) -> std::sync::MutexGuard<'_, Flips> {
        self.flips.lock().expect("virtio-gpu: flips lock")
    }

    /// Whether the answer to a flip has to wait; if so, it is queued
    /// for the pacer.
    fn hold(&self, tag: RingTag, head: u16, len: u32) -> bool {
        let mut flips = self.flips();
        let now = Instant::now();
        let interval = self.interval();
        let interval_us =
            u64::try_from(interval.as_micros()).unwrap_or(u64::MAX);
        if flips.paused
            || (flips.waiting.is_empty()
                && flips.next.is_none_or(|next| next <= now))
        {
            flips.next = Some(now + interval);
            probes::gpu_flip!(|| (head, 0, interval_us));
            return false;
        }
        flips.waiting.push(Waiting { tag, head, len });
        self.wake.notify_all();
        probes::gpu_flip!(|| (head, 1, interval_us));
        true
    }

    /// Publish answers whose ring is still the one they came from.
    fn answer(&self, waiting: &[Waiting]) {
        for w in waiting {
            let published = {
                let Some(_session) = self.access.enter(w.tag) else {
                    continue;
                };
                let Some(completion) = self
                    .completion
                    .lock()
                    .expect("virtio-gpu: completion lock")
                    .clone()
                else {
                    continue;
                };
                let raise = completion.publish_batch(&[(w.head, w.len)]);
                (raise, completion)
            };
            // Delivery can block, so it leaves the session behind.
            if let (true, completion) = published {
                self.access.deliver(w.tag, || completion.signal());
            }
            probes::gpu_flip_answer!(|| w.head);
        }
    }
}

/// The pacing thread: publish the waiting answers once their frame
/// comes, or at once while the device is paused.
fn pace(p: &Pacing) {
    let mut flips = p.flips();
    loop {
        if flips.shutdown {
            return;
        }
        let now = Instant::now();
        let due = flips.next.unwrap_or(now);
        if !flips.waiting.is_empty() && (flips.paused || due <= now) {
            let waiting = std::mem::take(&mut flips.waiting);
            // Frames keep their cadence across a late wake-up.
            flips.next = Some((due + p.interval()).max(now));
            flips.busy = true;
            drop(flips);
            p.answer(&waiting);
            flips = p.flips();
            flips.busy = false;
            continue;
        }
        flips = if flips.waiting.is_empty() {
            p.wake.wait(flips).expect("virtio-gpu: flips lock")
        } else {
            p.wake
                .wait_timeout(flips, due - now)
                .expect("virtio-gpu: flips lock")
                .0
        };
    }
}

/// Work to do when the device halts.
pub type HaltHook = Box<dyn FnOnce() + Send>;

impl VirtioGpu {
    /// A device that offers the guest one `width` x `height` display.
    ///
    /// # Errors
    ///
    /// Returns an error if the pacing thread cannot start.
    pub fn new(
        width: u32,
        height: u32,
        physmap: Arc<PhysMap>,
        log: slog::Logger,
    ) -> anyhow::Result<Self> {
        Self::paced(width, height, physmap, FLIP_INTERVAL, log)
    }

    /// A device that answers flips `interval` apart.
    pub(crate) fn paced(
        width: u32,
        height: u32,
        physmap: Arc<PhysMap>,
        interval: Duration,
        log: slog::Logger,
    ) -> anyhow::Result<Self> {
        // The size a migration destination would take, so a VM that
        // starts can also move.
        anyhow::ensure!(
            preferred_fits(width, height),
            "virtio-gpu: {width}x{height} is empty or more than {} MiB \
             of pixels",
            (MAX_HOST_BYTES / 2) >> 20
        );
        let display = GpuDisplay {
            preferred: Mutex::new((width, height)),
            ..Default::default()
        };
        let pacing = Arc::new(Pacing {
            asked: Arc::clone(&display.asked),
            physmap,
            access: GuestAccess::new(NUM_QUEUES),
            interrupt: Arc::new(IntrSlot::new()),
            completion: Mutex::new(None),
            flips: Mutex::default(),
            wake: Condvar::new(),
            interval,
        });
        let pacer_pacing = Arc::clone(&pacing);
        let pacer = thread::Builder::new()
            .name("virtio-gpu-pacer".into())
            .spawn(move || pace(&pacer_pacing))
            .map_err(|e| anyhow::anyhow!("spawn virtio-gpu pacer: {e}"))?;
        Ok(Self {
            log,
            refusals: TokenBucket::new(REFUSAL_LOG_BURST, REFUSAL_LOG_PERIOD),
            state: Mutex::default(),
            display: Arc::new(display),
            halt_hooks: Mutex::default(),
            pacing,
            pacer: Mutex::new(Some(pacer)),
        })
    }

    /// Install the transport interrupt path once the transport exists.
    pub fn install_interrupt(&self, intr: Arc<BackendIntr>) {
        self.pacing.interrupt.install(intr);
    }

    /// Get or create the control ring's completion handler.
    fn ensure_completion(&self, queue: &VirtQueue) -> Arc<VirtioCompletion> {
        let mut slot = self
            .pacing
            .completion
            .lock()
            .expect("virtio-gpu: completion lock");
        if let Some(completion) = slot.as_ref() {
            return Arc::clone(completion);
        }
        let intr = Arc::clone(&self.pacing.interrupt);
        let session = self.pacing.interrupt.next_session(IntrSession::INITIAL);
        let completion = VirtioCompletion::new(
            queue,
            Arc::clone(&self.pacing.physmap),
            session,
            move |session| intr.raise(session, QUEUE_CONTROL),
        );
        *slot = Some(Arc::clone(&completion));
        completion
    }

    /// Take the rings back: close them to waiting answers, wait out
    /// what is inside, and forget the handler and the answers. With a
    /// queue, that ring alone; without, every ring.
    fn retire(&self, queue: Option<u16>) {
        let access = &self.pacing.access;
        match queue {
            Some(idx) => {
                access.close_queue(idx);
            }
            None => access.close_all(),
        }
        access.drain();
        *self
            .pacing
            .completion
            .lock()
            .expect("virtio-gpu: completion lock") = None;
        {
            let mut flips = self.pacing.flips();
            flips.waiting.clear();
            flips.next = None;
        }
        access
            .reopen(self.pacing.interrupt.next_session(access.intr_session()));
    }

    fn stop_pacer(&self) {
        self.pacing.flips().shutdown = true;
        self.pacing.wake.notify_all();
        if let Some(pacer) =
            self.pacer.lock().expect("virtio-gpu: pacer lock").take()
        {
            if pacer.join().is_err() {
                slog::warn!(self.log, "virtio-gpu pacing thread panicked");
            }
        }
    }

    /// Run `notify` when the device config changes: the binary wires
    /// this to the transport's configuration change interrupt.
    pub fn on_config_change(&self, notify: ConfigNotify) {
        *self.display.notify.lock().expect("virtio-gpu: notify lock") =
            Some(notify);
    }

    /// Run `hook` when the device halts.
    pub fn on_halt(&self, hook: HaltHook) {
        self.halt_hooks
            .lock()
            .expect("virtio-gpu: halt hooks lock")
            .push(hook);
    }

    /// The display a server reads.
    pub fn display(&self) -> &GpuDisplay {
        &self.display
    }

    /// A handle on the display for a server thread.
    pub fn display_handle(&self) -> Arc<GpuDisplay> {
        Arc::clone(&self.display)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, GpuState> {
        self.state.lock().expect("virtio-gpu: state lock")
    }

    /// Serve one control-queue chain and return the bytes written.
    pub fn handle_ctrl(&self, bufs: &[ChainBuf], physmap: &PhysMap) -> u32 {
        self.serve_ctrl(bufs, physmap).0
    }

    /// Serve one control-queue chain: the bytes written, and whether it
    /// was a flip, a fenced flush, whose answer the pacing may hold.
    fn serve_ctrl(&self, bufs: &[ChainBuf], physmap: &PhysMap) -> (u32, bool) {
        let mut reader = ChainReader::new(bufs, physmap);
        let mut writer = ChainWriter::new(bufs, physmap);

        let Some(req) = self.read_hdr(&mut reader) else {
            return (0, false);
        };
        let flip =
            req.type_ == CMD_RESOURCE_FLUSH && req.flags & FLAG_FENCE != 0;

        if req.type_ == CMD_GET_DISPLAY_INFO {
            let mut resp = RespDisplayInfo {
                hdr: response(&req, RESP_OK_DISPLAY_INFO),
                ..Default::default()
            };
            resp.pmodes[0] = DisplayOne {
                r: {
                    let (width, height) = self.display.preferred();
                    Rect {
                        x: 0,
                        y: 0,
                        width,
                        height,
                    }
                },
                enabled: 1,
                flags: 0,
            };
            return (self.respond_ctrl(&mut writer, resp.as_bytes()), false);
        }

        let result = match req.type_ {
            CMD_RESOURCE_CREATE_2D => {
                read_body(&mut reader).and_then(|b| self.create_2d(b))
            }
            CMD_RESOURCE_UNREF => {
                read_body(&mut reader).and_then(|b| self.unref(b))
            }
            CMD_RESOURCE_ATTACH_BACKING => read_body(&mut reader)
                .and_then(|b| self.attach_backing(b, &mut reader)),
            CMD_RESOURCE_DETACH_BACKING => {
                read_body(&mut reader).and_then(|b| self.detach_backing(b))
            }
            CMD_TRANSFER_TO_HOST_2D => read_body(&mut reader)
                .and_then(|b| self.transfer_to_host_2d(b, physmap)),
            CMD_SET_SCANOUT => {
                read_body(&mut reader).and_then(|b| self.set_scanout(b))
            }
            CMD_RESOURCE_FLUSH => {
                read_body(&mut reader).and_then(|b| self.flush(b, physmap))
            }
            CMD_RESOURCE_CREATE_BLOB => read_body(&mut reader)
                .and_then(|b| self.create_blob(b, &mut reader)),
            CMD_SET_SCANOUT_BLOB => read_body(&mut reader)
                .and_then(|b| self.set_scanout_blob(b, physmap)),
            _ => Err(RESP_ERR_UNSPEC),
        };
        // Whatever these changed, a server reads now rather than at its
        // next poll; a wake that finds nothing new costs it a look.
        if matches!(
            req.type_,
            CMD_SET_SCANOUT
                | CMD_SET_SCANOUT_BLOB
                | CMD_RESOURCE_FLUSH
                | CMD_RESOURCE_UNREF
        ) {
            self.display.wakers.wake();
        }
        let type_ = self.response_type(req.type_, result);
        (
            self.respond_ctrl(&mut writer, response(&req, type_).as_bytes()),
            flip,
        )
    }

    /// The response to `command`, which a refusal is logged with.
    fn response_type(&self, command: u32, result: CmdResult) -> u32 {
        match result {
            Ok(()) => RESP_OK_NODATA,
            Err(refusal) => {
                self.refused(command, refusal);
                refusal
            }
        }
    }

    /// Write a control response; a chain with no room for it is logged,
    /// since the driver waits for an answer it cannot get.
    fn respond_ctrl(&self, writer: &mut ChainWriter<'_>, bytes: &[u8]) -> u32 {
        if writer.capacity() < bytes.len() {
            self.malformed("no room for the response");
        }
        respond(writer, bytes)
    }

    /// A request's header; one the chain is too short for is logged.
    fn read_hdr(&self, reader: &mut ChainReader<'_>) -> Option<CtrlHdr> {
        let hdr = read_body::<CtrlHdr>(reader).ok();
        if hdr.is_none() {
            self.malformed("a header shorter than 24 bytes");
        }
        hdr
    }

    /// Log a request the device could not serve at all, as a refusal is.
    fn malformed(&self, why: &'static str) {
        if self.refusals.take() {
            slog::warn!(self.log, "virtio-gpu dropped a request"; "why" => why);
        }
    }

    /// Log a refused command, so a display that goes black has a cause in
    /// the log, at most as often as the bucket lets the guest.
    fn refused(&self, command: u32, response: u32) {
        if self.refusals.take() {
            slog::warn!(self.log, "virtio-gpu refused a command";
                "command" => format!("{command:#06x}"),
                "response" => format!("{response:#06x}"));
        }
    }

    /// Serve one cursor-queue chain and return the bytes written.
    ///
    /// Linux gives cursor commands no response buffer; one that has
    /// room gets the header back.
    pub fn handle_cursor(&self, bufs: &[ChainBuf], physmap: &PhysMap) -> u32 {
        let mut reader = ChainReader::new(bufs, physmap);
        let mut writer = ChainWriter::new(bufs, physmap);

        let Some(req) = self.read_hdr(&mut reader) else {
            return 0;
        };
        let result = read_body::<UpdateCursor>(&mut reader).and_then(|cur| {
            match req.type_ {
                CMD_UPDATE_CURSOR => self.update_cursor(cur, physmap),
                CMD_MOVE_CURSOR => {
                    self.move_cursor(cur);
                    Ok(())
                }
                _ => Err(RESP_ERR_UNSPEC),
            }
        });
        let type_ = self.response_type(req.type_, result);
        respond(&mut writer, response(&req, type_).as_bytes())
    }

    fn create_2d(&self, c: ResourceCreate2d) -> CmdResult {
        if c.resource_id == 0 {
            return Err(RESP_ERR_INVALID_RESOURCE_ID);
        }
        if !matches!(c.format, FORMAT_B8G8R8A8_UNORM | FORMAT_B8G8R8X8_UNORM)
            || c.width == 0
            || c.height == 0
        {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        let mut st = self.lock();
        if st.has(c.resource_id) {
            return Err(RESP_ERR_INVALID_RESOURCE_ID);
        }
        let bytes = (c.width as usize)
            .checked_mul(c.height as usize)
            .and_then(|n| n.checked_mul(BYTES_PER_PIXEL))
            .ok_or(RESP_ERR_OUT_OF_MEMORY)?;
        let cost = bytes
            .checked_add(RESOURCE_OVERHEAD)
            .ok_or(RESP_ERR_OUT_OF_MEMORY)?;
        st.charge(cost)?;
        let res = Resource {
            width: c.width,
            height: c.height,
            pixels: vec![0; bytes],
            backing: None,
            version: 0,
        };
        debug_assert_eq!(cost, res.host_bytes());
        st.resources.insert(c.resource_id, res);
        Ok(())
    }

    fn unref(&self, u: ResourceUnref) -> CmdResult {
        let mut st = self.lock();
        let freed = match st.resources.remove(&u.resource_id) {
            Some(res) => res.host_bytes(),
            None => st
                .blobs
                .remove(&u.resource_id)
                .ok_or(RESP_ERR_INVALID_RESOURCE_ID)?
                .host_bytes(),
        };
        st.release(freed);
        if st.scanout.is_some_and(|s| s.resource_id == u.resource_id) {
            st.scanout = None;
            drop(st);
            self.display.resize(0, 0);
        }
        Ok(())
    }

    fn attach_backing(
        &self,
        a: ResourceAttachBacking,
        reader: &mut ChainReader<'_>,
    ) -> CmdResult {
        if !self.lock().resources.contains_key(&a.resource_id) {
            return Err(RESP_ERR_INVALID_RESOURCE_ID);
        }
        // The count is guest-chosen, so check it against what the chain
        // holds before it sizes anything.
        let wanted = (a.nr_entries as usize)
            .checked_mul(size_of::<MemEntry>())
            .ok_or(RESP_ERR_INVALID_PARAMETER)?;
        if a.nr_entries > MAX_BACKING_ENTRIES || wanted > reader.remaining() {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        // The list is host memory, so it counts against the budget, and
        // is refused before it is read.
        self.backing_fits(a.resource_id, wanted)?;
        let mut entries = Vec::with_capacity(a.nr_entries as usize);
        for _ in 0..a.nr_entries {
            entries.push(read_body::<MemEntry>(reader)?);
        }
        let mut st = self.lock();
        let old = st.backing_of(a.resource_id)?;
        st.host_bytes = st.replacing(old, wanted)?;
        // Found above, under this same lock.
        if let Some(res) = st.resources.get_mut(&a.resource_id) {
            res.backing = Some(entries);
        }
        Ok(())
    }

    /// Whether a list of `bytes` would fit the budget in place of the
    /// one `resource_id` has now.
    fn backing_fits(&self, resource_id: u32, bytes: usize) -> CmdResult {
        let st = self.lock();
        let old = st.backing_of(resource_id)?;
        st.replacing(old, bytes).map(|_| ())
    }

    fn detach_backing(&self, d: ResourceDetachBacking) -> CmdResult {
        let mut st = self.lock();
        let res = st
            .resources
            .get_mut(&d.resource_id)
            .ok_or(RESP_ERR_INVALID_RESOURCE_ID)?;
        let freed = backing_bytes(res.backing.take().map_or(0, |b| b.len()));
        st.release(freed);
        Ok(())
    }

    /// Copy a rectangle from guest backing into the host resource.
    ///
    /// Row `h` of the rectangle is read at `offset + h * stride` in the
    /// backing, where the stride is the resource's. That is how Linux
    /// computes `offset` and how QEMU reads it.
    fn transfer_to_host_2d(
        &self,
        t: TransferToHost2d,
        physmap: &PhysMap,
    ) -> CmdResult {
        let mut st = self.lock();
        // The device reads a blob's pages when it shows them.
        if st.blobs.contains_key(&t.resource_id) {
            return Ok(());
        }
        let res = st
            .resources
            .get_mut(&t.resource_id)
            .ok_or(RESP_ERR_INVALID_RESOURCE_ID)?;
        if !t.r.fits_within(res.width, res.height) {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        let stride = res.stride();
        let row_bytes = t.r.width as usize * BYTES_PER_PIXEL;
        // Before the copy: a transfer that fails part way has still
        // changed the pixels.
        res.version += 1;
        let Resource {
            pixels, backing, ..
        } = res;
        let backing = backing.as_deref().ok_or(RESP_ERR_UNSPEC)?;
        // Rows are read at growing offsets, so one walk serves them all.
        let mut cursor = BackingCursor::new(backing);
        for h in 0..t.r.height as usize {
            let src = (h as u64)
                .checked_mul(stride as u64)
                .and_then(|n| n.checked_add(t.offset))
                .ok_or(RESP_ERR_INVALID_PARAMETER)?;
            let dst = (t.r.y as usize + h) * stride
                + t.r.x as usize * BYTES_PER_PIXEL;
            cursor
                .read(physmap, src, &mut pixels[dst..][..row_bytes])
                .ok_or(RESP_ERR_UNSPEC)?;
        }
        Ok(())
    }

    fn set_scanout(&self, s: SetScanout) -> CmdResult {
        if s.scanout_id != 0 {
            return Err(RESP_ERR_INVALID_SCANOUT_ID);
        }
        let mut st = self.lock();
        if s.resource_id == 0 {
            st.scanout = None;
            st.shown = None;
            drop(st);
            self.display.resize(0, 0);
            return Ok(());
        }
        let res = st
            .resources
            .get(&s.resource_id)
            .ok_or(RESP_ERR_INVALID_RESOURCE_ID)?;
        if s.r.width == 0
            || s.r.height == 0
            || !s.r.fits_within(res.width, res.height)
        {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        let scanout = Scanout {
            resource_id: s.resource_id,
            r: s.r,
            blob: None,
        };
        // Show what the resource already holds. A page-flipping guest
        // transfers into the next buffer before it moves the scanout, and
        // flushes only afterwards; a blank surface in between is a black
        // frame a display server can catch.
        self.display.resize(s.r.width, s.r.height);
        show(&mut self.display.lock(), res, scanout, s.r);
        st.shown = Some((s.resource_id, res.version));
        st.scanout = Some(scanout);
        Ok(())
    }

    /// Show a rectangle of a resource, if the scanout shows that part.
    fn flush(&self, f: ResourceFlush, physmap: &PhysMap) -> CmdResult {
        let st = self.lock();
        if let Some(blob) = st.blobs.get(&f.resource_id) {
            let Some((scanout, image)) = st
                .scanout
                .filter(|s| s.resource_id == f.resource_id)
                .and_then(|s| Some((s, s.blob?)))
            else {
                return Ok(());
            };
            if !f.r.fits_within(image.width, image.height) {
                return Err(RESP_ERR_INVALID_PARAMETER);
            }
            let Some(area) = f.r.intersect(&scanout.r) else {
                return Ok(());
            };
            let mut disp = self.display.lock();
            return show_blob(&mut disp, blob, image, scanout, area, physmap)
                .ok_or(RESP_ERR_UNSPEC);
        }
        let res = st
            .resources
            .get(&f.resource_id)
            .ok_or(RESP_ERR_INVALID_RESOURCE_ID)?;
        if !f.r.fits_within(res.width, res.height) {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        let Some(scanout) =
            st.scanout.filter(|s| s.resource_id == f.resource_id)
        else {
            return Ok(());
        };
        let Some(area) = f.r.intersect(&scanout.r) else {
            return Ok(());
        };
        // The flush that follows a page flip shows what the flip already
        // copied, unless a transfer has changed the resource since.
        if st.shown == Some((f.resource_id, res.version)) {
            let mut disp = self.display.lock();
            GpuDisplay::damage(&mut disp, shown_rect(scanout, area));
            return Ok(());
        }
        show(&mut self.display.lock(), res, scanout, area);
        Ok(())
    }

    fn update_cursor(&self, c: UpdateCursor, physmap: &PhysMap) -> CmdResult {
        if c.resource_id == 0 {
            self.display.set_cursor(None);
            return Ok(());
        }
        let st = self.lock();
        let pixels = match st.blobs.get(&c.resource_id) {
            // The driver sends no layout for a cursor, so a blob holds
            // it packed from its start, as QEMU reads it.
            Some(blob) => {
                let len =
                    (CURSOR_SIZE * CURSOR_SIZE) as usize * BYTES_PER_PIXEL;
                if blob.size < len as u64 {
                    return Err(RESP_ERR_INVALID_PARAMETER);
                }
                let mut pixels = vec![0; len];
                read_backing(&blob.backing, physmap, 0, &mut pixels)
                    .ok_or(RESP_ERR_UNSPEC)?;
                pixels
            }
            None => {
                let res = st
                    .resources
                    .get(&c.resource_id)
                    .ok_or(RESP_ERR_INVALID_RESOURCE_ID)?;
                if res.width != CURSOR_SIZE || res.height != CURSOR_SIZE {
                    return Err(RESP_ERR_INVALID_PARAMETER);
                }
                res.pixels.clone()
            }
        };
        let cursor = Cursor {
            x: c.pos.x,
            y: c.pos.y,
            hot_x: c.hot_x.min(CURSOR_SIZE - 1),
            hot_y: c.hot_y.min(CURSOR_SIZE - 1),
            pixels,
        };
        drop(st);
        self.display.set_cursor(Some(cursor));
        Ok(())
    }

    fn create_blob(
        &self,
        c: ResourceCreateBlob,
        reader: &mut ChainReader<'_>,
    ) -> CmdResult {
        if c.resource_id == 0 {
            return Err(RESP_ERR_INVALID_RESOURCE_ID);
        }
        // Only guest memory: there is no host memory to offer a blob.
        if c.blob_mem != BLOB_MEM_GUEST || c.size == 0 {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        if self.lock().has(c.resource_id) {
            return Err(RESP_ERR_INVALID_RESOURCE_ID);
        }
        // As for ATTACH_BACKING: the count is checked against the chain,
        // and the list against the budget, before anything is read.
        let wanted = (c.nr_entries as usize)
            .checked_mul(size_of::<MemEntry>())
            .ok_or(RESP_ERR_INVALID_PARAMETER)?;
        if c.nr_entries > MAX_BACKING_ENTRIES || wanted > reader.remaining() {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        let cost = wanted
            .checked_add(RESOURCE_OVERHEAD)
            .ok_or(RESP_ERR_OUT_OF_MEMORY)?;
        self.lock().replacing(0, cost)?;
        let mut backing = Vec::with_capacity(c.nr_entries as usize);
        let mut length = 0u64;
        for _ in 0..c.nr_entries {
            let e = read_body::<MemEntry>(reader)?;
            length = length.saturating_add(u64::from(e.length));
            backing.push(e);
        }
        // The blob is read wherever its image says, so its pages must
        // cover all of it.
        if length < c.size {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        let mut st = self.lock();
        if st.has(c.resource_id) {
            return Err(RESP_ERR_INVALID_RESOURCE_ID);
        }
        st.charge(cost)?;
        let blob = Blob {
            size: c.size,
            backing,
        };
        debug_assert_eq!(cost, blob.host_bytes());
        st.blobs.insert(c.resource_id, blob);
        Ok(())
    }

    /// Show a blob's image, read from its pages now.
    fn set_scanout_blob(
        &self,
        s: SetScanoutBlob,
        physmap: &PhysMap,
    ) -> CmdResult {
        if s.scanout_id != 0 {
            return Err(RESP_ERR_INVALID_SCANOUT_ID);
        }
        if s.resource_id == 0 {
            return self.set_scanout(SetScanout {
                r: s.r,
                scanout_id: 0,
                resource_id: 0,
            });
        }
        let mut st = self.lock();
        let blob = st
            .blobs
            .get(&s.resource_id)
            .ok_or(RESP_ERR_INVALID_RESOURCE_ID)?;
        let image = BlobImage {
            width: s.width,
            height: s.height,
            stride: s.strides[0],
            offset: s.offsets[0],
        };
        let format_ok =
            matches!(s.format, FORMAT_B8G8R8A8_UNORM | FORMAT_B8G8R8X8_UNORM);
        if !format_ok || !image.fits(blob.size, &s.r) {
            return Err(RESP_ERR_INVALID_PARAMETER);
        }
        let scanout = Scanout {
            resource_id: s.resource_id,
            r: s.r,
            blob: Some(image),
        };
        self.display.resize(s.r.width, s.r.height);
        let shown = show_blob(
            &mut self.display.lock(),
            blob,
            image,
            scanout,
            s.r,
            physmap,
        );
        if shown.is_none() {
            st.scanout = None;
            st.shown = None;
            drop(st);
            self.display.resize(0, 0);
            return Err(RESP_ERR_UNSPEC);
        }
        st.scanout = Some(scanout);
        st.shown = None;
        Ok(())
    }

    fn move_cursor(&self, c: UpdateCursor) {
        if let Some(cur) = self.display.lock().cursor.as_mut() {
            cur.x = c.pos.x;
            cur.y = c.pos.y;
        }
    }
}

/// Check a migration payload against what this device could have
/// built, and return the host memory its resources take.
fn check_migrate_state(s: &GpuMigrateState) -> Result<usize, DeviceStateError> {
    let bad =
        |what: String| DeviceStateError::invalid(format!("virtio-gpu: {what}"));
    let mut total = 0usize;
    let mut ids = std::collections::HashSet::new();
    for r in &s.resources {
        if r.id == 0 || !ids.insert(r.id) {
            return Err(bad(format!(
                "resource id {} is zero or repeated",
                r.id
            )));
        }
        total = (r.width as usize)
            .checked_mul(r.height as usize)
            .and_then(|n| n.checked_mul(BYTES_PER_PIXEL))
            .filter(|&n| n > 0)
            .and_then(|n| n.checked_add(RESOURCE_OVERHEAD))
            .and_then(|cost| total.checked_add(cost))
            .filter(|&t| t <= MAX_HOST_BYTES)
            .ok_or_else(|| {
                bad(format!(
                    "resource {} of {}x{} is empty or past the budget",
                    r.id, r.width, r.height
                ))
            })?;
        let entries = r.backing.as_ref().map_or(0, Vec::len);
        if entries > MAX_BACKING_ENTRIES as usize {
            return Err(bad(format!(
                "resource {} has too many backing entries",
                r.id
            )));
        }
        // A backing list is host memory too.
        total = total
            .checked_add(backing_bytes(entries))
            .filter(|&t| t <= MAX_HOST_BYTES)
            .ok_or_else(|| {
                bad(format!("resource {}'s backing is past the budget", r.id))
            })?;
    }
    for b in &s.blobs {
        if b.id == 0 || !ids.insert(b.id) {
            return Err(bad(format!("blob id {} is zero or repeated", b.id)));
        }
        let length: u64 =
            b.backing.iter().map(|&(_, len)| u64::from(len)).sum();
        if b.size == 0
            || length < b.size
            || b.backing.len() > MAX_BACKING_ENTRIES as usize
        {
            return Err(bad(format!(
                "blob {} of {} bytes is empty, longer than its {} bytes of \
                 pages, or has too many of them",
                b.id, b.size, length
            )));
        }
        total = backing_bytes(b.backing.len())
            .checked_add(RESOURCE_OVERHEAD)
            .and_then(|cost| total.checked_add(cost))
            .filter(|&t| t <= MAX_HOST_BYTES)
            .ok_or_else(|| {
                bad(format!("blob {}'s pages are past the budget", b.id))
            })?;
    }
    if let Some(so) = &s.scanout {
        let r = Rect {
            x: so.x,
            y: so.y,
            width: so.width,
            height: so.height,
        };
        let fits = match &so.blob {
            None => {
                s.resources
                    .iter()
                    .find(|r| r.id == so.resource_id)
                    .map(|res| {
                        so.width > 0
                            && so.height > 0
                            && r.fits_within(res.width, res.height)
                    })
            }
            Some(image) => s
                .blobs
                .iter()
                .find(|b| b.id == so.resource_id)
                .map(|b| blob_image(image).fits(b.size, &r)),
        };
        match fits {
            None => {
                return Err(bad(format!(
                    "the scanout shows no resource {} of its kind",
                    so.resource_id
                )))
            }
            Some(false) => {
                return Err(bad(format!(
                    "the scanout {so:?} is not inside its resource"
                )))
            }
            Some(true) => {}
        }
    }
    if let Some(c) = &s.cursor {
        if c.pixels.len()
            != (CURSOR_SIZE * CURSOR_SIZE) as usize * BYTES_PER_PIXEL
        {
            return Err(bad(format!("a cursor of {} bytes", c.pixels.len())));
        }
        if c.hot_x >= CURSOR_SIZE || c.hot_y >= CURSOR_SIZE {
            return Err(bad(format!(
                "a cursor hotspot at ({}, {})",
                c.hot_x, c.hot_y
            )));
        }
    }
    if !preferred_fits(s.preferred.0, s.preferred.1) {
        return Err(bad(format!(
            "an offered size of {}x{}",
            s.preferred.0, s.preferred.1
        )));
    }
    if s.events & !EVENT_DISPLAY != 0 {
        return Err(bad(format!("events {:#x}", s.events)));
    }
    Ok(total)
}

fn blob_image(i: &GpuMigrateBlobImage) -> BlobImage {
    BlobImage {
        width: i.width,
        height: i.height,
        stride: i.stride,
        offset: i.offset,
    }
}

fn mem_entries(entries: &[(u64, u32)]) -> Vec<MemEntry> {
    entries
        .iter()
        .map(|&(addr, length)| MemEntry {
            addr,
            length,
            padding: 0,
        })
        .collect()
}

/// Copy `area` of `res`, which `scanout` shows, onto the display and
/// damage it. `area` is in resource coordinates and inside the scanout.
fn show(disp: &mut DisplayState, res: &Resource, scanout: Scanout, area: Rect) {
    disp.copies += 1;
    let dst_stride = disp.surface.width as usize * BYTES_PER_PIXEL;
    let row_bytes = area.width as usize * BYTES_PER_PIXEL;
    for row in 0..area.height as usize {
        let y = area.y as usize + row;
        let src = y * res.stride() + area.x as usize * BYTES_PER_PIXEL;
        let dst = (y - scanout.r.y as usize) * dst_stride
            + (area.x - scanout.r.x) as usize * BYTES_PER_PIXEL;
        disp.surface.pixels[dst..][..row_bytes]
            .copy_from_slice(&res.pixels[src..][..row_bytes]);
    }
    GpuDisplay::damage(disp, shown_rect(scanout, area));
}

/// Read `area` of a blob's `image`, which `scanout` shows, from its
/// pages onto the display and damage it. `area` is in image
/// coordinates and inside the scanout. `None` if the pages are not
/// mapped; the rows read before then stay on the display.
fn show_blob(
    disp: &mut DisplayState,
    blob: &Blob,
    image: BlobImage,
    scanout: Scanout,
    area: Rect,
    physmap: &PhysMap,
) -> Option<()> {
    disp.copies += 1;
    let dst_stride = disp.surface.width as usize * BYTES_PER_PIXEL;
    let row_bytes = area.width as usize * BYTES_PER_PIXEL;
    // Rows are read at growing offsets, so one walk serves them all.
    let mut cursor = BackingCursor::new(&blob.backing);
    for row in 0..area.height {
        let y = area.y + row;
        let dst = (y - scanout.r.y) as usize * dst_stride
            + (area.x - scanout.r.x) as usize * BYTES_PER_PIXEL;
        cursor.read(
            physmap,
            image.at(area.x, y),
            &mut disp.surface.pixels[dst..][..row_bytes],
        )?;
    }
    GpuDisplay::damage(disp, shown_rect(scanout, area));
    Some(())
}

/// Where `area` of the resource `scanout` shows lands on the display.
fn shown_rect(scanout: Scanout, area: Rect) -> Rect {
    Rect {
        x: area.x - scanout.r.x,
        y: area.y - scanout.r.y,
        width: area.width,
        height: area.height,
    }
}

/// Read one wire struct from the chain.
fn read_body<T: FromBytes>(reader: &mut ChainReader<'_>) -> Result<T, u32> {
    let mut buf = vec![0u8; size_of::<T>()];
    reader.read_exact(&mut buf).map_err(|_| RESP_ERR_UNSPEC)?;
    T::read_from_bytes(&buf).map_err(|_| RESP_ERR_UNSPEC)
}

/// The response header for `req`, carrying its fence when it asked.
fn response(req: &CtrlHdr, type_: u32) -> CtrlHdr {
    CtrlHdr {
        type_,
        flags: req.flags & FLAG_FENCE,
        fence_id: req.fence_id,
        ctx_id: req.ctx_id,
        ring_idx: req.ring_idx,
        padding: [0; 3],
    }
}

/// Write a whole response or nothing: a partial header is worse for the
/// driver than none.
fn respond(writer: &mut ChainWriter<'_>, bytes: &[u8]) -> u32 {
    if writer.capacity() < bytes.len() || writer.write_all(bytes).is_err() {
        return writer.written();
    }
    writer.written()
}

/// Copy `dst.len()` bytes starting at `offset` in the backing.
///
/// Returns `None` if the backing ends first or names memory that is not
/// mapped. The driver handed these pages to the device for the request,
/// so a plain copy is allowed (see `SubMapping::copy_out`).
fn read_backing(
    backing: &[MemEntry],
    physmap: &PhysMap,
    offset: u64,
    dst: &mut [u8],
) -> Option<()> {
    BackingCursor::new(backing).read(physmap, offset, dst)
}

/// Reads a backing at offsets that never go back, as a transfer's rows
/// do, walking the page list once across all of them rather than from
/// its start for each: a list may have 65536 entries and a transfer as
/// many rows as the resource, and the walk runs on the vCPU.
struct BackingCursor<'a> {
    entries: &'a [MemEntry],
    /// The entry the last read began in, and that entry's offset.
    idx: usize,
    start: u64,
    /// Entries stepped through, so a test can bound the walk.
    #[cfg_attr(not(test), allow(dead_code))]
    visited: usize,
}

impl<'a> BackingCursor<'a> {
    fn new(entries: &'a [MemEntry]) -> Self {
        Self {
            entries,
            idx: 0,
            start: 0,
            visited: 0,
        }
    }

    /// Copy `dst.len()` bytes from `offset`, which must not be below the
    /// last read's. `None` if it is, if the backing ends first, or if it
    /// names memory that is not mapped.
    fn read(
        &mut self,
        physmap: &PhysMap,
        offset: u64,
        dst: &mut [u8],
    ) -> Option<()> {
        if dst.is_empty() {
            return Some(());
        }
        if offset < self.start {
            return None;
        }
        // Move up to the entry that holds `offset`.
        loop {
            let e = self.entries.get(self.idx)?;
            let end = self.start.checked_add(u64::from(e.length))?;
            if offset < end {
                break;
            }
            self.start = end;
            self.idx += 1;
            self.visited += 1;
        }
        // Copy from there, across as many entries as `dst` spans.
        let mut skip = offset - self.start;
        let mut done = 0usize;
        let mut i = self.idx;
        while done < dst.len() {
            let e = self.entries.get(i)?;
            self.visited += 1;
            i += 1;
            let len = u64::from(e.length);
            if skip >= len {
                skip -= len;
                continue;
            }
            let n = ((len - skip) as usize).min(dst.len() - done);
            let gpa = e.addr.checked_add(skip)?;
            physmap
                .lookup(gpa, n)?
                .copy_out(&mut dst[done..][..n])
                .ok()?;
            done += n;
            skip = 0;
        }
        Some(())
    }
}

impl VirtioDevice for VirtioGpu {
    fn device_features(&self) -> u64 {
        // Ring features: legacy transport strips these in pci.rs.
        super::bits::VIRTIO_F_RING_EVENT_IDX
            | super::bits::VIRTIO_F_RING_INDIRECT_DESC
            | F_RESOURCE_BLOB
    }

    fn set_features(&self, _features: u64) {}

    fn cfg_read(&self, offset: u16, len: u8) -> u32 {
        // events_read, events_clear (reads 0), num_scanouts, num_capsets.
        let mut cfg = [0u8; CONFIG_SIZE as usize];
        let events = self
            .display
            .events
            .load(std::sync::atomic::Ordering::Acquire);
        cfg[0..4].copy_from_slice(&events.to_le_bytes());
        cfg[8..12].copy_from_slice(&1u32.to_le_bytes());
        super::cfg_read_bytes(&cfg, offset, len)
    }

    /// events_clear (offset 4) clears the events the driver has seen.
    /// Every other field is read-only.
    fn cfg_write(&self, offset: u16, val: u32, len: u8) {
        if offset == 4 && len == 4 {
            self.display
                .events
                .fetch_and(!val, std::sync::atomic::Ordering::AcqRel);
        }
    }

    fn process_queue(
        &self,
        queue_idx: u16,
        queue: &mut VirtQueue,
        head: u16,
        physmap: &PhysMap,
    ) -> u32 {
        let Some(bufs) = queue.collect_chain(physmap, head) else {
            self.malformed("a descriptor chain that cannot be walked");
            return 0;
        };
        match queue_idx {
            QUEUE_CONTROL => self.handle_ctrl(&bufs, physmap),
            QUEUE_CURSOR => self.handle_cursor(&bufs, physmap),
            _ => 0,
        }
    }

    /// The control ring is answered through the pacing's completion
    /// handler, inline but for a flip that comes early. The cursor ring
    /// is answered inline.
    fn notify_queue(
        &self,
        queue_idx: u16,
        queues: &mut [VirtQueue],
        physmap: &PhysMap,
    ) -> bool {
        let Some(queue) = queues.get_mut(usize::from(queue_idx)) else {
            return false;
        };
        let max = usize::from(queue.size());
        if queue_idx != QUEUE_CONTROL {
            let old_used_idx = queue.read_used_idx(physmap);
            let popped = queue.drain_avail(physmap, max, |q, head| {
                let written = self.process_queue(queue_idx, q, head, physmap);
                q.push_used(physmap, head, written);
            });
            return popped > 0
                && queue.should_notify_guest(physmap, old_used_idx);
        }
        let completion = self.ensure_completion(queue);
        let Some(session) = self.pacing.access.enter_current(QUEUE_CONTROL)
        else {
            return false;
        };
        let tag = session.tag();
        let mut answered = Vec::new();
        queue.drain_avail(physmap, max, |q, head| {
            let (written, flip) = match q.collect_chain(physmap, head) {
                Some(bufs) => self.serve_ctrl(&bufs, physmap),
                None => {
                    self.malformed("a descriptor chain that cannot be walked");
                    (0, false)
                }
            };
            if !(flip && self.pacing.hold(tag, head, written)) {
                answered.push((head, written));
            }
        });
        let raise = completion.publish_batch(&answered);
        drop(session);
        // The transport raises for these; the pacer raises for its own.
        raise
    }

    /// The control ring's handler names the ring it was built on.
    fn queue_addr_set(&self, queue_idx: u16, _queue: &VirtQueue) {
        if queue_idx == QUEUE_CONTROL {
            self.retire(Some(queue_idx));
        }
    }

    /// The resources, scanout, cursor and mode, without the pixels:
    /// every resource the guest draws into is backed by guest pages,
    /// which the migration carries.
    fn export_device_state(
        &self,
    ) -> Result<Option<VirtioDeviceMigrateState>, DeviceStateError> {
        // A paused device has answered every flip; one still waiting
        // would never be answered on the destination.
        if !self.pacing.flips().waiting.is_empty() {
            return Err(DeviceStateError::Export(
                "virtio-gpu: a flip's answer is still waiting".into(),
            ));
        }
        let st = self.lock();
        let mut resources: Vec<GpuMigrateResource> = st
            .resources
            .iter()
            .map(|(&id, r)| GpuMigrateResource {
                id,
                width: r.width,
                height: r.height,
                backing: r.backing.as_ref().map(|entries| {
                    entries.iter().map(|e| (e.addr, e.length)).collect()
                }),
            })
            .collect();
        resources.sort_by_key(|r| r.id);
        let mut blobs: Vec<GpuMigrateBlob> = st
            .blobs
            .iter()
            .map(|(&id, b)| GpuMigrateBlob {
                id,
                size: b.size,
                backing: b.backing.iter().map(|e| (e.addr, e.length)).collect(),
            })
            .collect();
        blobs.sort_by_key(|b| b.id);
        let scanout = st.scanout.map(|s| GpuMigrateScanout {
            resource_id: s.resource_id,
            x: s.r.x,
            y: s.r.y,
            width: s.r.width,
            height: s.r.height,
            blob: s.blob.map(|i| GpuMigrateBlobImage {
                width: i.width,
                height: i.height,
                stride: i.stride,
                offset: i.offset,
            }),
        });
        drop(st);
        let cursor = self.display.cursor().map(|c| GpuMigrateCursor {
            x: c.x,
            y: c.y,
            hot_x: c.hot_x,
            hot_y: c.hot_y,
            pixels: c.pixels,
        });
        Ok(Some(VirtioDeviceMigrateState::Gpu(GpuMigrateState {
            resources,
            scanout,
            cursor,
            preferred: self.display.preferred(),
            events: self
                .display
                .events
                .load(std::sync::atomic::Ordering::Acquire),
            blobs,
        })))
    }

    /// Take the source's resources, read each back from its backing,
    /// and show the scanout whole. Checked whole first: a payload this
    /// device could not have built is refused with nothing kept.
    fn restore_device_state(
        &self,
        state: Option<&VirtioDeviceMigrateState>,
        physmap: &PhysMap,
    ) -> Result<(), DeviceStateError> {
        let s = match state {
            Some(VirtioDeviceMigrateState::Gpu(s)) => s,
            Some(other) => {
                return Err(DeviceStateError::WrongKind {
                    want: vmm_devices::lifecycle::GPU_KIND,
                    got: other.kind(),
                })
            }
            None => {
                return Err(DeviceStateError::invalid(
                    "virtio-gpu: payload carries no device state",
                ))
            }
        };
        let checked = check_migrate_state(s)?;

        // Restored without their pixels: logged once, after the restore.
        let mut blank = 0usize;
        let mut resources = HashMap::with_capacity(s.resources.len());
        for r in &s.resources {
            let mut pixels =
                vec![0; r.width as usize * r.height as usize * BYTES_PER_PIXEL];
            let backing = r.backing.as_deref().map(mem_entries);
            // A guest may attach backing it never draws from; such a
            // resource comes back blank rather than failing the
            // migration.
            if let Some(backing) = &backing {
                if read_backing(backing, physmap, 0, &mut pixels).is_none() {
                    pixels.fill(0);
                    blank += 1;
                }
            }
            resources.insert(
                r.id,
                Resource {
                    width: r.width,
                    height: r.height,
                    pixels,
                    backing,
                    version: 0,
                },
            );
        }
        let scanout = s.scanout.as_ref().map(|so| Scanout {
            resource_id: so.resource_id,
            r: Rect {
                x: so.x,
                y: so.y,
                width: so.width,
                height: so.height,
            },
            blob: so.blob.as_ref().map(blob_image),
        });
        let blobs: HashMap<u32, Blob> = s
            .blobs
            .iter()
            .map(|b| {
                let blob = Blob {
                    size: b.size,
                    backing: mem_entries(&b.backing),
                };
                (b.id, blob)
            })
            .collect();

        // What was rebuilt, which the check above worked out on its own.
        let host_bytes =
            resources.values().map(Resource::host_bytes).sum::<usize>()
                + blobs.values().map(Blob::host_bytes).sum::<usize>();
        debug_assert_eq!(host_bytes, checked);
        let mut st = self.lock();
        *st = GpuState {
            resources,
            blobs,
            host_bytes,
            scanout,
            shown: None,
        };
        *self
            .display
            .preferred
            .lock()
            .expect("virtio-gpu: preferred lock") = s.preferred;
        self.display
            .events
            .store(s.events, std::sync::atomic::Ordering::Release);
        match scanout {
            Some(so) => {
                self.display.resize(so.r.width, so.r.height);
                let mut disp = self.display.lock();
                match so.blob {
                    None => show(
                        &mut disp,
                        &st.resources[&so.resource_id],
                        so,
                        so.r,
                    ),
                    // As with a 2D resource's backing, pages that cannot
                    // be read leave the picture blank rather than fail
                    // the migration.
                    Some(image) => {
                        let blob = &st.blobs[&so.resource_id];
                        if show_blob(&mut disp, blob, image, so, so.r, physmap)
                            .is_none()
                        {
                            disp.surface.pixels.fill(0);
                            blank += 1;
                        }
                    }
                }
            }
            None => self.display.resize(0, 0),
        }
        // A cursor the source had hidden comes back as none set, so a
        // client shows its own pointer until the guest sets one again:
        // the payload does not say which of the two it was.
        match &s.cursor {
            Some(c) => self.display.set_cursor(Some(Cursor {
                x: c.x,
                y: c.y,
                hot_x: c.hot_x,
                hot_y: c.hot_y,
                pixels: c.pixels.clone(),
            })),
            None => self.display.forget_cursor(),
        }
        if blank > 0 {
            slog::warn!(self.log, "virtio-gpu restored resources blank: \
                their pages could not be read; the guest redraws them";
                "resources" => blank);
        }
        Ok(())
    }

    fn reset(&self) {
        self.retire(None);
        // The requested size outlives the driver; its events do not.
        self.display
            .events
            .store(0, std::sync::atomic::Ordering::Release);
        *self.lock() = GpuState::default();
        self.display.resize(0, 0);
        self.display.forget_cursor();
    }
}

impl Lifecycle for VirtioGpu {
    fn type_name(&self) -> &'static str {
        vmm_devices::lifecycle::GPU_KIND
    }

    /// Answer every waiting flip, and every one that comes until the
    /// device resumes, so nothing is in flight for a migration.
    fn pause(&self) {
        self.pacing.flips().paused = true;
        self.pacing.wake.notify_all();
    }

    fn is_quiesced(&self) -> bool {
        let flips = self.pacing.flips();
        flips.waiting.is_empty() && !flips.busy
    }

    fn resume(&self) {
        self.pacing.flips().paused = false;
    }

    fn halt(&self) {
        self.stop_pacer();
        let hooks = std::mem::take(
            &mut *self.halt_hooks.lock().expect("virtio-gpu: halt hooks lock"),
        );
        for hook in hooks {
            hook();
        }
    }
}

impl Drop for VirtioGpu {
    fn drop(&mut self) {
        self.stop_pacer();
    }
}

#[cfg(test)]
mod tests;
