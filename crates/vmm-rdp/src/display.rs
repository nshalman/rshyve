// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The display side of the RDP server.
//!
//! IronRDP pulls updates from a stream. This one looks at the display
//! about 30 times a second: a size change goes out as a Resize, and
//! damage as a bitmap of the damaged area. IronRDP keeps the client's
//! picture and sends only what differs from it, so the damage does not
//! have to be exact. A new connection, and every size change, start with
//! the whole display.
//!
//! The guest's pointer goes out as a pointer update, for the client to
//! draw at its own mouse position, and it does whether the picture goes
//! over bitmaps or over EGFX.

use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp_server::{
    BitmapUpdate, DesktopSize, DisplayUpdate, PixelFormat, RGBAPointer,
    RdpServerDisplay, RdpServerDisplayUpdates,
};
use vmm_devices::display::{
    CursorImage, CursorShape, DisplaySource, DisplayWaker, Rect,
};

use crate::egfx::Egfx;

/// The largest pointer a client takes without the large pointer
/// capability (MS-RDPBCGR 2.2.9.1.1.4.4).
const MAX_POINTER: u32 = 96;

/// How often the stream looks for changes.
const FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// Hands IronRDP the display's size and, on request, a stream of its
/// updates.
pub(crate) struct Display {
    pub(crate) source: Arc<dyn DisplaySource>,
    /// EGFX, which takes the display over once a client's channel is
    /// ready.
    pub(crate) egfx: Option<Arc<Egfx>>,
    pub(crate) log: slog::Logger,
}

#[async_trait::async_trait]
impl RdpServerDisplay for Display {
    async fn size(&mut self) -> DesktopSize {
        let (width, height) = rdp_size(self.source.size());
        DesktopSize { width, height }
    }

    async fn updates(
        &mut self,
    ) -> anyhow::Result<Box<dyn RdpServerDisplayUpdates>> {
        Ok(Box::new(Updates::new(
            Arc::clone(&self.source),
            self.egfx.clone(),
        )))
    }

    /// A client whose window changed size asks for its primary monitor's
    /// size; the guest decides whether to follow.
    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        let Some(primary) = layout
            .monitors()
            .iter()
            .find(|m| m.is_primary())
            .or_else(|| layout.monitors().first())
        else {
            return;
        };
        let (width, height) = primary.dimensions();
        let passed_on = self.source.request_size(width, height);
        slog::info!(self.log, "RDP client asked for a new size";
            "width" => width, "height" => height, "passed_on" => passed_on);
    }
}

/// The display's size as RDP carries it.
fn rdp_size((width, height): (u32, u32)) -> (u16, u16) {
    (
        u16::try_from(width).unwrap_or(u16::MAX),
        u16::try_from(height).unwrap_or(u16::MAX),
    )
}

/// The updates of one connection.
pub(crate) struct Updates {
    source: Arc<dyn DisplaySource>,
    egfx: Option<Arc<Egfx>>,
    /// Woken when the display changes.
    changed: Arc<Notified>,
    /// How often to look anyway, for a display that cannot say.
    fallback: Duration,
    /// The size the client was last given.
    size: (u16, u16),
    /// Whether the client needs the whole display: a new connection, or
    /// the first update after a Resize.
    whole: bool,
    /// The pointer serial the client was last sent. A client starts
    /// with its own pointer, which is what serial 0 is.
    pointer_serial: u64,
}

impl Updates {
    pub(crate) fn new(
        source: Arc<dyn DisplaySource>,
        egfx: Option<Arc<Egfx>>,
    ) -> Self {
        let size = rdp_size(source.size());
        let changed = Arc::new(Notified(tokio::sync::Notify::new()));
        source.on_change(
            Arc::downgrade(&changed) as std::sync::Weak<dyn DisplayWaker>
        );
        Self {
            source,
            egfx,
            changed,
            fallback: FRAME_INTERVAL,
            size,
            whole: true,
            pointer_serial: 0,
        }
    }

    /// The next update that is ready now, without waiting. While EGFX
    /// carries the display there is none: EGFX's worker sends frames.
    pub(crate) fn poll(&mut self) -> Option<DisplayUpdate> {
        if let Some(pointer) = self.pointer() {
            return Some(pointer);
        }
        // EGFX's worker sends the frames: encoding one here would stall
        // this loop, which also sends the audio.
        if self.egfx.as_ref().is_some_and(|e| e.active()) {
            return None;
        }
        let size = rdp_size(self.source.size());
        if size != self.size {
            self.size = size;
            self.whole = true;
            if size.0 > 0 && size.1 > 0 {
                return Some(DisplayUpdate::Resize(DesktopSize {
                    width: size.0,
                    height: size.1,
                }));
            }
        }
        let (width, height) =
            (NonZeroU16::new(size.0)?, NonZeroU16::new(size.1)?);
        let whole = Rect {
            x: 0,
            y: 0,
            width: u32::from(width.get()),
            height: u32::from(height.get()),
        };
        // Take the damage even when sending everything, so it is not
        // sent again.
        let damage = self.source.take_damage();
        let area = if std::mem::take(&mut self.whole) {
            whole
        } else {
            clip(damage?, whole)?
        };
        self.bitmap(area)
    }

    /// The guest's pointer, if it changed since the client was sent it.
    fn pointer(&mut self) -> Option<DisplayUpdate> {
        let serial = self.source.cursor_serial();
        if serial == self.pointer_serial {
            return None;
        }
        self.pointer_serial = serial;
        Some(match self.source.cursor() {
            CursorShape::Default => DisplayUpdate::DefaultPointer,
            CursorShape::Hidden => DisplayUpdate::HidePointer,
            CursorShape::Image(image) => rdp_pointer(&image).map_or(
                DisplayUpdate::DefaultPointer,
                DisplayUpdate::RGBAPointer,
            ),
        })
    }

    fn bitmap(&self, area: Rect) -> Option<DisplayUpdate> {
        let stride = area.width as usize * 4;
        let mut data = vec![0u8; stride * area.height as usize];
        self.source.read_rect(area, &mut data).ok()?;
        // `clip` kept the area inside a display whose edges are u16.
        Some(DisplayUpdate::Bitmap(BitmapUpdate {
            x: area.x as u16,
            y: area.y as u16,
            width: NonZeroU16::new(area.width as u16)?,
            height: NonZeroU16::new(area.height as u16)?,
            format: PixelFormat::BgrX32,
            data: Bytes::from(data),
            stride: NonZeroUsize::new(stride)?,
        }))
    }
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for Updates {
    /// Cancellation safe, as IronRDP requires: an update is taken from
    /// the display and returned with no await between.
    async fn next_update(&mut self) -> anyhow::Result<Option<DisplayUpdate>> {
        loop {
            if let Some(update) = self.poll() {
                return Ok(Some(update));
            }
            // A wake while no one waits is kept for the next wait.
            tokio::select! {
                () = self.changed.0.notified() => {}
                () = tokio::time::sleep(self.fallback) => {}
            }
        }
    }
}

/// Wakes the update stream when the display changes.
struct Notified(tokio::sync::Notify);

impl DisplayWaker for Notified {
    fn wake(&self) {
        self.0.notify_one();
    }
}

/// `image` as RDP's 32-bit pointer: the same B, G, R, A bytes, with the
/// rows bottom to top. `None` for one a client could not take: past
/// [`MAX_POINTER`], a hotspot outside it, or pixels that do not match
/// its size.
fn rdp_pointer(image: &CursorImage) -> Option<RGBAPointer> {
    let (w, h) = (image.width, image.height);
    if w == 0
        || h == 0
        || w > MAX_POINTER
        || h > MAX_POINTER
        || image.hot_x >= w
        || image.hot_y >= h
        || image.pixels.len() != (w * h * 4) as usize
    {
        return None;
    }
    let data = image
        .pixels
        .rchunks(w as usize * 4)
        .flatten()
        .copied()
        .collect();
    // All five fit in a u16: they are below MAX_POINTER.
    Some(RGBAPointer {
        cache_index: 0,
        width: w as u16,
        height: h as u16,
        hot_x: image.hot_x as u16,
        hot_y: image.hot_y as u16,
        data,
    })
}

/// The part of `r` inside `bounds`, which starts at the origin.
fn clip(r: Rect, bounds: Rect) -> Option<Rect> {
    let right = r.x.saturating_add(r.width).min(bounds.width);
    let bottom = r.y.saturating_add(r.height).min(bounds.height);
    (right > r.x && bottom > r.y).then(|| Rect {
        x: r.x,
        y: r.y,
        width: right - r.x,
        height: bottom - r.y,
    })
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use ironrdp_server::{DesktopSize, PixelFormat};
    use vmm_devices::display::{CursorImage, CursorShape, Rect};

    use super::*;
    use crate::testsupport::FakeDisplay;

    fn updates(width: u32, height: u32) -> (Arc<FakeDisplay>, Updates) {
        let fake = Arc::new(FakeDisplay::new(width, height));
        let updates = Updates::new(fake.clone(), None);
        (fake, updates)
    }

    /// (x, y, width, height) of a bitmap, after checking its format and
    /// that its pixels are the display's.
    fn bitmap(
        fake: &FakeDisplay,
        update: Option<DisplayUpdate>,
    ) -> (u16, u16, u16, u16) {
        let Some(DisplayUpdate::Bitmap(b)) = update else {
            panic!("expected a bitmap, got {update:?}");
        };
        assert_eq!(b.format, PixelFormat::BgrX32, "B, G, R, X bytes");
        assert_eq!(
            b.stride,
            NonZeroUsize::new(usize::from(b.width.get()) * 4).unwrap()
        );
        for y in 0..b.height.get() {
            for x in 0..b.width.get() {
                let at = usize::from(y) * b.stride.get() + usize::from(x) * 4;
                let got =
                    u32::from_le_bytes(b.data[at..at + 4].try_into().unwrap());
                let want = fake.pixel(u32::from(b.x + x), u32::from(b.y + y));
                assert_eq!(got, want, "pixel ({x}, {y}) of the bitmap");
            }
        }
        (b.x, b.y, b.width.get(), b.height.get())
    }

    /// Once EGFX carries the display, the classic bitmap path is quiet,
    /// and the stream does not encode EGFX frames either: the server
    /// loop runs it, and also sends the audio. EGFX's worker does that.
    #[test]
    fn an_active_egfx_takes_over_from_bitmaps() {
        let h = crate::egfx::tests::Harness::new();
        h.ready();
        let fake = Arc::new(FakeDisplay::new(64, 48));
        let mut updates = Updates::new(fake.clone(), Some(Arc::clone(&h.egfx)));
        assert!(updates.poll().is_none(), "no bitmap");
        assert_eq!(h.frames(), 0, "no EGFX frame on the server loop");
    }

    #[test]
    fn a_connection_starts_with_the_whole_display() {
        let (fake, mut updates) = updates(64, 48);
        assert_eq!(bitmap(&fake, updates.poll()), (0, 0, 64, 48));
        assert!(updates.poll().is_none(), "nothing changed since");
    }

    #[test]
    fn damage_is_sent_as_a_bitmap_of_that_area() {
        let (fake, mut updates) = updates(64, 48);
        updates.poll();
        *fake.damage.lock().unwrap() = Some(Rect {
            x: 8,
            y: 4,
            width: 16,
            height: 10,
        });
        assert_eq!(bitmap(&fake, updates.poll()), (8, 4, 16, 10));
    }

    #[test]
    fn damage_past_the_edge_is_clipped() {
        let (fake, mut updates) = updates(64, 48);
        updates.poll();
        *fake.damage.lock().unwrap() = Some(Rect {
            x: 60,
            y: 40,
            width: 16,
            height: 16,
        });
        assert_eq!(bitmap(&fake, updates.poll()), (60, 40, 4, 8));
    }

    /// A mode change is a Resize, and the client's new framebuffer then
    /// gets the whole display.
    #[test]
    fn a_size_change_is_a_resize_then_the_whole_display() {
        let (fake, mut updates) = updates(64, 48);
        updates.poll();
        *fake.size.lock().unwrap() = (80, 60);
        match updates.poll() {
            Some(DisplayUpdate::Resize(DesktopSize {
                width: 80,
                height: 60,
            })) => {}
            other => panic!("expected a resize, got {other:?}"),
        }
        assert_eq!(bitmap(&fake, updates.poll()), (0, 0, 80, 60));
    }

    /// A client whose window changed size asks for its primary
    /// monitor's size.
    #[test]
    fn a_layout_request_asks_the_display_for_the_primary_size() {
        use ironrdp_displaycontrol::pdu::MonitorLayoutEntry;

        let fake = Arc::new(FakeDisplay::new(64, 48));
        let mut display = Display {
            source: fake.clone(),
            egfx: None,
            log: slog::Logger::root(slog::Discard, slog::o!()),
        };
        let layout = DisplayControlMonitorLayout::new(&[
            MonitorLayoutEntry::new_secondary(800, 600).unwrap(),
            MonitorLayoutEntry::new_primary(1600, 900).unwrap(),
        ])
        .unwrap();
        display.request_layout(layout);
        assert_eq!(*fake.requests.lock().unwrap(), [(1600, 900)]);
    }

    /// A 2x3 pointer whose pixel (x, y) is the bytes [x, y, 0x80, 0xff],
    /// hotspot (1, 2).
    fn pointer() -> CursorShape {
        let pixels = (0..3u8)
            .flat_map(|y| (0..2u8).flat_map(move |x| [x, y, 0x80, 0xff]))
            .collect();
        CursorShape::Image(Arc::new(CursorImage {
            width: 2,
            height: 3,
            hot_x: 1,
            hot_y: 2,
            pixels,
        }))
    }

    /// Every update ready now. Capped, so a stream that never runs dry,
    /// which is a bug, fails the test instead of filling memory.
    fn drain(updates: &mut Updates) -> Vec<DisplayUpdate> {
        let got: Vec<_> =
            std::iter::from_fn(|| updates.poll()).take(64).collect();
        assert!(got.len() < 64, "the update stream never ran dry");
        got
    }

    fn pointers(updates: &mut Updates) -> Vec<DisplayUpdate> {
        drain(updates)
            .into_iter()
            .filter(|u| !matches!(u, DisplayUpdate::Bitmap(_)))
            .collect()
    }

    /// Until the guest sets a pointer, the client keeps its own.
    #[test]
    fn no_pointer_goes_out_until_the_guest_sets_one() {
        let (_fake, mut updates) = updates(64, 48);
        assert!(pointers(&mut updates).is_empty());
    }

    /// RDP's 32-bit pointer is B, G, R, A like the guest's, but with
    /// its rows bottom to top.
    #[test]
    fn the_guest_pointer_goes_out_for_the_client_to_draw() {
        let (fake, mut updates) = updates(64, 48);
        *fake.cursor.lock().unwrap() = (1, pointer());
        let got = pointers(&mut updates);
        let [DisplayUpdate::RGBAPointer(p)] = got.as_slice() else {
            panic!("expected one pointer, got {got:?}");
        };
        assert_eq!((p.width, p.height, p.hot_x, p.hot_y), (2, 3, 1, 2));
        let rows: Vec<&[u8]> = p.data.chunks(8).collect();
        assert_eq!(
            rows[0],
            [0, 2, 0x80, 0xff, 1, 2, 0x80, 0xff],
            "bottom row first"
        );
        assert_eq!(rows[2], [0, 0, 0x80, 0xff, 1, 0, 0x80, 0xff]);
    }

    #[test]
    fn a_pointer_goes_out_once_per_change() {
        let (fake, mut updates) = updates(64, 48);
        *fake.cursor.lock().unwrap() = (1, pointer());
        assert_eq!(pointers(&mut updates).len(), 1);
        assert!(pointers(&mut updates).is_empty(), "unchanged");
        *fake.cursor.lock().unwrap() = (2, pointer());
        assert_eq!(pointers(&mut updates).len(), 1);
    }

    #[test]
    fn a_hidden_or_reset_pointer_goes_out_as_such() {
        let (fake, mut updates) = updates(64, 48);
        *fake.cursor.lock().unwrap() = (1, CursorShape::Hidden);
        assert!(matches!(
            pointers(&mut updates).as_slice(),
            [DisplayUpdate::HidePointer]
        ));
        *fake.cursor.lock().unwrap() = (2, CursorShape::Default);
        assert!(matches!(
            pointers(&mut updates).as_slice(),
            [DisplayUpdate::DefaultPointer]
        ));
    }

    /// A pointer no client could take leaves the client with its own
    /// rather than sending one it would refuse.
    #[test]
    fn a_pointer_a_client_cannot_take_falls_back_to_the_default() {
        let image = |width, height, hot_x| {
            CursorShape::Image(Arc::new(CursorImage {
                width,
                height,
                hot_x,
                hot_y: 0,
                pixels: vec![0; (width * height * 4) as usize],
            }))
        };
        for (serial, shape) in [
            (1, image(97, 1, 0)),
            (2, image(4, 4, 4)),
            (3, image(0, 4, 0)),
        ] {
            let (fake, mut updates) = updates(64, 48);
            *fake.cursor.lock().unwrap() = (serial, shape);
            assert!(
                matches!(
                    pointers(&mut updates).as_slice(),
                    [DisplayUpdate::DefaultPointer]
                ),
                "serial {serial}"
            );
        }
    }

    /// EGFX carries the picture but not the pointer, which still goes
    /// out as a pointer update.
    #[test]
    fn the_pointer_goes_out_while_egfx_carries_the_display() {
        let h = crate::egfx::tests::Harness::new();
        h.ready();
        let fake = Arc::new(FakeDisplay::new(64, 48));
        let mut updates = Updates::new(fake.clone(), Some(Arc::clone(&h.egfx)));
        *fake.cursor.lock().unwrap() = (1, pointer());
        assert!(matches!(
            pointers(&mut updates).as_slice(),
            [DisplayUpdate::RGBAPointer(_)]
        ));
    }

    /// A disabled scanout has no pixels to send.
    /// The pointer goes out when the guest sets it, not at the next
    /// poll.
    #[test]
    fn a_change_wakes_the_update_stream() {
        let (fake, mut updates) = updates(64, 48);
        updates.fallback = Duration::from_secs(3600);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("a runtime");
        rt.block_on(async {
            // The whole display goes first.
            let first = updates.next_update().await.expect("an update");
            assert!(matches!(first, Some(DisplayUpdate::Bitmap(_))));
            let guest = Arc::clone(&fake);
            let setter = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                guest.set_cursor(1, CursorShape::Hidden);
            });
            let next = tokio::time::timeout(
                Duration::from_secs(5),
                updates.next_update(),
            )
            .await
            .expect("woken before the poll")
            .expect("an update");
            assert!(matches!(next, Some(DisplayUpdate::HidePointer)));
            setter.join().expect("setter");
        });
    }

    #[test]
    fn a_blank_display_sends_nothing() {
        let (fake, mut updates) = updates(0, 0);
        assert!(updates.poll().is_none());
        *fake.size.lock().unwrap() = (16, 16);
        assert!(matches!(updates.poll(), Some(DisplayUpdate::Resize(_))));
        assert_eq!(bitmap(&fake, updates.poll()), (0, 0, 16, 16));
    }
}
