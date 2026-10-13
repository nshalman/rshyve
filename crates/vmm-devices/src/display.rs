// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a remote display server reads from a display device.
//!
//! A display device (fbuf, virtio-gpu) implements [`DisplaySource`]; a
//! server (VNC, RDP) reads it without knowing which device it is.

use std::io;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

/// A rectangle in pixels.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    /// The smallest rectangle covering both.
    pub fn union(self, other: Rect) -> Rect {
        let (x0, y0) = (self.x.min(other.x), self.y.min(other.y));
        let x1 = self
            .x
            .saturating_add(self.width)
            .max(other.x.saturating_add(other.width));
        let y1 = self
            .y
            .saturating_add(self.height)
            .max(other.y.saturating_add(other.height));
        Rect {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        }
    }
}

/// A pointer image for a client to draw itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorImage {
    pub width: u32,
    pub height: u32,
    /// The pixel that is the pointer's position.
    pub hot_x: u32,
    pub hot_y: u32,
    /// `width * height` pixels, bytes B, G, R, A, rows top to bottom.
    pub pixels: Vec<u8>,
}

/// The pointer the guest asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorShape {
    /// The guest has set no pointer: it draws its own into the display,
    /// or none. A client keeps its own pointer.
    Default,
    /// The guest hid its pointer.
    Hidden,
    /// The guest's pointer, for the client to draw at its own mouse
    /// position.
    Image(Arc<CursorImage>),
}

/// The pixels of one display, as XRGB8888 (bytes B, G, R, X).
pub trait DisplaySource: Send + Sync {
    /// The current size; `(0, 0)` when nothing is displayed.
    fn size(&self) -> (u32, u32);

    /// The part of the display that changed since the last call.
    ///
    /// A device that cannot tell reports the whole display every time.
    fn take_damage(&self) -> Option<Rect>;

    /// Copy `r` into `dst` as packed rows, `r.width * 4` bytes each.
    ///
    /// Fails if `r` does not lie within the current size or `dst` is
    /// not exactly the size of `r`.
    fn read_rect(&self, r: Rect, dst: &mut [u8]) -> io::Result<()>;

    /// Ask the guest to switch to `width` x `height`, as a client whose
    /// window changed size does. Returns whether the request was passed
    /// on; the guest decides whether and when the size changes, and
    /// [`size`](Self::size) reports it once it has. A device that
    /// cannot ask refuses, which is the default.
    fn request_size(&self, _width: u32, _height: u32) -> bool {
        false
    }

    /// A count that changes whenever [`cursor`](Self::cursor) does, so
    /// a server reads the pointer only when it changed. A device the
    /// guest cannot set a pointer on stays at 0, the default.
    fn cursor_serial(&self) -> u64 {
        0
    }

    /// The pointer the guest asked for; [`CursorShape::Default`] for a
    /// device that has none.
    fn cursor(&self) -> CursorShape {
        CursorShape::Default
    }

    /// Wake `waker` whenever the display or the pointer changes, for as
    /// long as its server keeps it. A device that cannot tell never
    /// wakes it, which is the default, so a server still polls.
    fn on_change(&self, _waker: Weak<dyn DisplayWaker>) {}

    /// Ask the guest to make a frame every `interval`, the rate a server
    /// is managing to send them on, which may be faster or slower than
    /// the device's own pace; `None` withdraws the request. A device that
    /// cannot pace the guest ignores it, which is the default.
    fn pace(&self, _interval: Option<Duration>) {}
}

/// What a server hands a display to be woken by.
///
/// A device wakes it from the thread that changed the display, a vCPU
/// among them, so `wake` must be quick and must not block.
pub trait DisplayWaker: Send + Sync {
    fn wake(&self);
}

/// The servers a device wakes. A server that dropped its waker is
/// forgotten at the next wake.
#[derive(Default)]
pub struct Wakers(Mutex<Vec<Weak<dyn DisplayWaker>>>);

impl Wakers {
    pub fn add(&self, waker: Weak<dyn DisplayWaker>) {
        lock(&self.0).push(waker);
    }

    pub fn wake(&self) {
        // Woken outside the lock: a waker may itself read the display.
        let live: Vec<_> = {
            let mut wakers = lock(&self.0);
            wakers.retain(|w| w.strong_count() > 0);
            wakers.iter().filter_map(Weak::upgrade).collect()
        };
        for waker in live {
            waker.wake();
        }
    }
}

/// One display read by several servers.
///
/// A device keeps one record of what changed, and the first server to
/// take it empties it, so a second server never sees those changes.
/// Each server reads through a [`view`](Self::view) of its own instead,
/// which gathers every change since that server last took them.
pub struct SharedDisplay {
    inner: Arc<dyn DisplaySource>,
    views: Mutex<Vec<Weak<ViewState>>>,
}

#[derive(Default)]
struct ViewState {
    damage: Mutex<Option<Rect>>,
    /// The pace this view's server asked for.
    pace: Mutex<Option<Duration>>,
}

impl SharedDisplay {
    pub fn new(inner: Arc<dyn DisplaySource>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            views: Mutex::default(),
        })
    }

    /// A view for one more server.
    pub fn view(self: &Arc<Self>) -> Arc<dyn DisplaySource> {
        let state = Arc::new(ViewState::default());
        self.views().push(Arc::downgrade(&state));
        Arc::new(DisplayView {
            shared: Arc::clone(self),
            state,
        })
    }

    fn views(&self) -> std::sync::MutexGuard<'_, Vec<Weak<ViewState>>> {
        self.views.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Hand what changed on the device to every view, and forget views
    /// whose server has gone.
    fn gather(&self) {
        let mut views = self.views();
        views.retain(|v| v.strong_count() > 0);
        let Some(r) = self.inner.take_damage() else {
            return;
        };
        for view in views.iter().filter_map(Weak::upgrade) {
            let mut damage = lock(&view.damage);
            *damage = Some(damage.map_or(r, |d| d.union(r)));
        }
    }

    /// Ask the device for the fastest pace any server asked for: at that
    /// rate every frame reaches one of them. A server that never asked
    /// has no say; with no request left, the request is withdrawn.
    fn repace(&self) {
        let fastest = self
            .views()
            .iter()
            .filter_map(Weak::upgrade)
            .filter_map(|v| *lock(&v.pace))
            .min();
        self.inner.pace(fastest);
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One server's view of a [`SharedDisplay`].
struct DisplayView {
    shared: Arc<SharedDisplay>,
    state: Arc<ViewState>,
}

impl DisplaySource for DisplayView {
    fn size(&self) -> (u32, u32) {
        self.shared.inner.size()
    }

    fn take_damage(&self) -> Option<Rect> {
        self.shared.gather();
        lock(&self.state.damage).take()
    }

    fn read_rect(&self, r: Rect, dst: &mut [u8]) -> io::Result<()> {
        self.shared.inner.read_rect(r, dst)
    }

    fn request_size(&self, width: u32, height: u32) -> bool {
        self.shared.inner.request_size(width, height)
    }

    fn cursor_serial(&self) -> u64 {
        self.shared.inner.cursor_serial()
    }

    fn cursor(&self) -> CursorShape {
        self.shared.inner.cursor()
    }

    fn on_change(&self, waker: Weak<dyn DisplayWaker>) {
        self.shared.inner.on_change(waker);
    }

    fn pace(&self, interval: Option<Duration>) {
        *lock(&self.state.pace) = interval;
        self.shared.repace();
    }
}

impl Drop for DisplayView {
    /// A server that goes takes its pace request with it.
    fn drop(&mut self) {
        if lock(&self.state.pace).take().is_some() {
            self.shared.repace();
        }
    }
}

/// Width and height of the tiles [`changed_tiles`] compares.
pub const TILE: u32 = 64;

/// Bytes in one XRGB8888 pixel.
const BYTES_PER_PIXEL: usize = 4;

/// Pixels of part of a display, XRGB8888, rows `stride` pixels apart,
/// whose first pixel is the display's (`x`, `y`).
#[derive(Debug, Clone, Copy)]
pub struct Pixels<'a> {
    pub data: &'a [u8],
    pub stride: u32,
    pub x: u32,
    pub y: u32,
}

impl Pixels<'_> {
    /// The byte offset of the display's (`x`, `y`) in `data`.
    fn at(&self, x: u32, y: u32) -> usize {
        ((y - self.y) as usize * self.stride as usize + (x - self.x) as usize)
            * BYTES_PER_PIXEL
    }
}

/// The tiles of `area` whose pixels in `new` differ from those in `old`;
/// both must cover `area`. Tiles sit on a fixed 64-pixel grid from the
/// display's origin, cut to `area`, and changed tiles next to each other
/// in one tile row merge into one rectangle.
pub fn changed_tiles(
    old: Pixels<'_>,
    new: Pixels<'_>,
    area: Rect,
) -> Vec<Rect> {
    let (right, bottom) = (area.x + area.width, area.y + area.height);
    let differs = |x0: u32, x1: u32, y0: u32, y1: u32| {
        let len = (x1 - x0) as usize * BYTES_PER_PIXEL;
        (y0..y1).any(|y| {
            let (o, n) = (old.at(x0, y), new.at(x0, y));
            old.data[o..o + len] != new.data[n..n + len]
        })
    };

    let mut rects = Vec::new();
    let mut ty = area.y / TILE * TILE;
    while ty < bottom {
        let (y0, y1) = (ty.max(area.y), (ty + TILE).min(bottom));
        let mut run: Option<(u32, u32)> = None;
        let mut tx = area.x / TILE * TILE;
        while tx < right {
            let (x0, x1) = (tx.max(area.x), (tx + TILE).min(right));
            if differs(x0, x1, y0, y1) {
                run = Some((run.map_or(x0, |(start, _)| start), x1));
            } else if let Some((start, end)) = run.take() {
                rects.push(Rect {
                    x: start,
                    y: y0,
                    width: end - start,
                    height: y1 - y0,
                });
            }
            tx += TILE;
        }
        if let Some((start, end)) = run {
            rects.push(Rect {
                x: start,
                y: y0,
                width: end - start,
                height: y1 - y0,
            });
        }
        ty += TILE;
    }
    rects
}

/// The frame interval a server sustains, which it asks the guest to
/// flip no faster than: a frame the guest makes faster is one no client
/// sees, and its vCPU time is spent for nothing.
pub struct FramePace {
    /// The server's own frame interval, the fastest it asks for.
    floor: Duration,
    interval: Option<Duration>,
}

impl FramePace {
    pub fn new(floor: Duration) -> Self {
        Self {
            floor,
            interval: None,
        }
    }

    /// Account a frame that took `cost` to make, which the client took
    /// or refused, and return the interval to ask for.
    ///
    /// A frame costs at least the server's own interval. A refused frame
    /// lost its slot to the client's flow control as well: the client
    /// takes frames more slowly than they go out. The average over the
    /// last few frames rides out a single slow one.
    pub fn frame(&mut self, cost: Duration, taken: bool) -> Duration {
        let mut sample = cost.max(self.floor);
        if !taken {
            sample += self.floor;
        }
        let interval = match self.interval {
            Some(i) => (i * 3 + sample) / 4,
            None => sample,
        };
        self.interval = Some(interval);
        interval
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;

    const FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 60);

    /// A display whose damage and pace the test sets and reads.
    #[derive(Default)]
    struct Fake {
        damage: Mutex<Option<Rect>>,
        paced: Mutex<Vec<Option<Duration>>>,
    }

    impl Fake {
        fn damage(&self, r: Rect) {
            let mut d = self.damage.lock().unwrap();
            *d = Some(d.map_or(r, |d| d.union(r)));
        }

        fn last_pace(&self) -> Option<Option<Duration>> {
            self.paced.lock().unwrap().last().copied()
        }
    }

    impl DisplaySource for Fake {
        fn size(&self) -> (u32, u32) {
            (64, 48)
        }

        fn take_damage(&self) -> Option<Rect> {
            self.damage.lock().unwrap().take()
        }

        fn read_rect(&self, r: Rect, dst: &mut [u8]) -> io::Result<()> {
            dst.fill(u8::try_from(r.x).unwrap_or(0));
            Ok(())
        }

        fn pace(&self, interval: Option<Duration>) {
            self.paced.lock().unwrap().push(interval);
        }
    }

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn every_view_sees_every_change() {
        let fake = Arc::new(Fake::default());
        let shared = SharedDisplay::new(fake.clone());
        let (vnc, rdp) = (shared.view(), shared.view());
        fake.damage(rect(1, 2, 3, 4));
        assert_eq!(vnc.take_damage(), Some(rect(1, 2, 3, 4)));
        assert_eq!(rdp.take_damage(), Some(rect(1, 2, 3, 4)));
        assert_eq!(vnc.take_damage(), None);
        assert_eq!(rdp.take_damage(), None);
    }

    /// Each view keeps what changed since it last looked.
    #[test]
    fn each_view_gathers_damage_until_it_takes_it() {
        let fake = Arc::new(Fake::default());
        let shared = SharedDisplay::new(fake.clone());
        let (vnc, rdp) = (shared.view(), shared.view());
        fake.damage(rect(0, 0, 10, 10));
        assert_eq!(vnc.take_damage(), Some(rect(0, 0, 10, 10)));
        fake.damage(rect(20, 20, 10, 10));
        assert_eq!(rdp.take_damage(), Some(rect(0, 0, 30, 30)));
        assert_eq!(vnc.take_damage(), Some(rect(20, 20, 10, 10)));
    }

    /// A server that stopped is no longer kept up to date.
    #[test]
    fn a_dropped_view_is_forgotten() {
        let fake = Arc::new(Fake::default());
        let shared = SharedDisplay::new(fake.clone());
        let vnc = shared.view();
        drop(shared.view());
        fake.damage(rect(0, 0, 1, 1));
        assert_eq!(vnc.take_damage(), Some(rect(0, 0, 1, 1)));
        assert_eq!(shared.views.lock().unwrap().len(), 1);
    }

    /// The guest is paced to the fastest server that asked: at that
    /// rate every frame reaches someone.
    #[test]
    fn the_guest_is_paced_to_the_fastest_request() {
        let fake = Arc::new(Fake::default());
        let shared = SharedDisplay::new(fake.clone());
        let (a, b, quiet) = (shared.view(), shared.view(), shared.view());
        a.pace(Some(MS * 50));
        assert_eq!(fake.last_pace(), Some(Some(MS * 50)));
        b.pace(Some(MS * 40));
        assert_eq!(fake.last_pace(), Some(Some(MS * 40)));
        b.pace(None);
        assert_eq!(fake.last_pace(), Some(Some(MS * 50)));
        // A server that never asked has no say.
        drop(quiet);
        assert_eq!(fake.last_pace(), Some(Some(MS * 50)));
        drop(a);
        assert_eq!(fake.last_pace(), Some(None), "the last request went");
    }

    #[derive(Default)]
    struct Count(std::sync::atomic::AtomicUsize);

    impl DisplayWaker for Count {
        fn wake(&self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl Count {
        fn get(&self) -> usize {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[test]
    fn wakers_are_woken_while_their_server_keeps_them() {
        let wakers = Wakers::default();
        let (a, b) = (Arc::new(Count::default()), Arc::new(Count::default()));
        wakers.add(Arc::downgrade(&a) as Weak<dyn DisplayWaker>);
        wakers.add(Arc::downgrade(&b) as Weak<dyn DisplayWaker>);
        wakers.wake();
        assert_eq!((a.get(), b.get()), (1, 1));
        drop(b);
        wakers.wake();
        assert_eq!(a.get(), 2);
        assert_eq!(
            wakers.0.lock().unwrap().len(),
            1,
            "a gone server is forgotten"
        );
    }

    /// A view hands its server's waker to the display.
    #[test]
    fn a_view_passes_its_waker_on() {
        #[derive(Default)]
        struct Waking(Wakers);
        impl DisplaySource for Waking {
            fn size(&self) -> (u32, u32) {
                (0, 0)
            }
            fn take_damage(&self) -> Option<Rect> {
                None
            }
            fn read_rect(&self, _: Rect, _: &mut [u8]) -> io::Result<()> {
                Ok(())
            }
            fn on_change(&self, waker: Weak<dyn DisplayWaker>) {
                self.0.add(waker);
            }
        }
        let inner = Arc::new(Waking::default());
        let shared = SharedDisplay::new(inner.clone());
        let view = shared.view();
        let count = Arc::new(Count::default());
        view.on_change(Arc::downgrade(&count) as Weak<dyn DisplayWaker>);
        inner.0.wake();
        assert_eq!(count.get(), 1);
    }

    #[test]
    fn a_view_reads_the_display() {
        let fake = Arc::new(Fake::default());
        let shared = SharedDisplay::new(fake);
        let view = shared.view();
        assert_eq!(view.size(), (64, 48));
        let mut px = [0u8; 4];
        view.read_rect(rect(7, 0, 1, 1), &mut px).expect("read");
        assert_eq!(px, [7; 4]);
    }

    /// Frames cheaper than the worker's own interval ask for that
    /// interval: the guest need not flip faster than frames go out.
    #[test]
    fn cheap_frames_pace_the_guest_at_the_frame_interval() {
        let mut pace = FramePace::new(FRAME_INTERVAL);
        for _ in 0..10 {
            assert_eq!(pace.frame(MS, true), FRAME_INTERVAL);
        }
    }

    /// Frames that take longer to make than the interval slow the guest
    /// to the rate they go out at, smoothed over a few frames.
    #[test]
    fn slow_frames_slow_the_guest_to_their_cost() {
        let mut pace = FramePace::new(FRAME_INTERVAL);
        assert_eq!(pace.frame(MS, true), FRAME_INTERVAL);
        let mut asked = Vec::new();
        for _ in 0..30 {
            asked.push(pace.frame(MS * 60, true));
        }
        assert!(asked.windows(2).all(|w| w[0] <= w[1]), "{asked:?}");
        assert!(asked[0] < MS * 60, "smoothed: {asked:?}");
        let last = *asked.last().expect("asked");
        assert!(last > MS * 59 && last <= MS * 60, "{last:?}");
    }

    /// A frame the client refused cost its slot: the client is slower
    /// than the frames, so the guest is slowed further.
    #[test]
    fn refused_frames_slow_the_guest_further() {
        let mut taken = FramePace::new(FRAME_INTERVAL);
        let mut refused = FramePace::new(FRAME_INTERVAL);
        for _ in 0..30 {
            taken.frame(MS * 40, true);
            refused.frame(MS * 40, false);
        }
        // By the slot it lost, one frame interval.
        assert!(
            refused.frame(MS * 40, false)
                > taken.frame(MS * 40, true) + FRAME_INTERVAL - MS
        );
    }

    /// Once frames are cheap again the guest speeds back up.
    #[test]
    fn the_pace_recovers() {
        let mut pace = FramePace::new(FRAME_INTERVAL);
        for _ in 0..30 {
            pace.frame(MS * 90, false);
        }
        let mut last = Duration::MAX;
        for _ in 0..60 {
            last = pace.frame(MS, true);
        }
        assert!(last < FRAME_INTERVAL + MS, "{last:?}");
    }
}
