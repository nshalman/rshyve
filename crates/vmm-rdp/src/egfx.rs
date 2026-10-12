// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The display over EGFX, the graphics pipeline, as RemoteFX Progressive.
//!
//! Clients such as the macOS Windows App keep their better codecs for
//! EGFX: offered only the classic bitmap path, they take NSCodec. Once a
//! connection's EGFX channel is ready, the display goes to one surface
//! the size of the display, mapped to the output, as progressive frames
//! of the tiles that changed; the classic path stops.
//!
//! IronRDP builds a pipeline per connection through [`Factory`], which
//! keeps a handle to it here. The pipeline queues what it has to send,
//! and the server loop sends it on a `ServerEvent::Egfx`. The handler's
//! callbacks run with the pipeline locked, so they only log; the work
//! happens in [`Egfx::update`], on a [`Worker`] thread of its own.
//!
//! A frame of a large display takes tens of milliseconds to read and
//! encode. The server loop runs on one thread and sends the audio too,
//! so a frame encoded there stalled the sound; and it takes EGFX's state
//! and the pipeline's lock for its own work, the client's frame
//! acknowledgements among it. So an update holds those only briefly,
//! to plan a frame and then to send it, and encodes with neither.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ironrdp_egfx::pdu::PixelFormat;
use ironrdp_egfx::server::{GraphicsPipelineHandler, GraphicsPipelineServer};
use ironrdp_pdu::gcc::{Monitor, MonitorFlags};
use ironrdp_server::{
    EgfxServerMessage, GfxDvcBridge, GfxServerFactory, GfxServerHandle,
    ServerEvent, ServerEventSender,
};
use ironrdp_svc::ChannelFlags;
use tokio::sync::mpsc::UnboundedSender;
use vmm_core::ratelimit::TokenBucket;
use vmm_devices::display::{
    changed_tiles, DisplaySource, DisplayWaker, Pixels, Rect, TILE,
};

use crate::progressive::ProgressiveEncoder;

/// How often the worker looks for changes, and the fastest it asks the
/// guest to flip: 60 frames a second, what a client's display shows.
/// Slower frames, or a client that falls behind, slow both.
const FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 60);

/// The codec context the progressive frames use; one per surface.
const CODEC_CONTEXT: u32 = 1;

/// Failed encodes logged: a failure can repeat every frame.
const ENCODE_LOG_BURST: u32 = 4;
const ENCODE_LOG_PERIOD: Duration = Duration::from_secs(15);

/// EGFX for one server: the current connection's pipeline, and what
/// the display has sent it.
pub(crate) struct Egfx {
    state: Mutex<State>,
    /// What only an update touches: the encoder and a frame buffer,
    /// apart from `state` so a frame is encoded with no lock the server
    /// loop needs.
    render: Mutex<Render>,
    log: slog::Logger,
    /// Caps the failed encodes logged.
    failures: TokenBucket,
}

struct State {
    /// The current connection's pipeline, once IronRDP built one.
    pipeline: Option<GfxServerHandle>,
    /// Where the server loop takes EGFX output from.
    sender: Option<UnboundedSender<ServerEvent>>,
    /// The surface showing the display: id, width, height.
    surface: Option<(u16, u16, u16)>,
    /// Damage not yet sent, held back by the client's flow control.
    pending: Option<Rect>,
    started: Instant,
    /// Counts connections, so an update notices a new one and starts
    /// its encoder over, and drops a frame planned for the last one.
    connection: u64,
}

impl State {
    /// Start over for a connection with `pipeline`, or for none: the
    /// client has no surface, and a frame planned for the last one is
    /// dropped.
    fn new_connection(&mut self, pipeline: Option<GfxServerHandle>) {
        self.pipeline = pipeline;
        self.surface = None;
        self.pending = None;
        self.connection += 1;
    }
}

struct Render {
    encoder: ProgressiveEncoder,
    /// The connection the encoder's state is for.
    connection: u64,
    /// The display's pixels, read for the frame being made.
    pixels: Vec<u8>,
    /// The pixels the client has: the last frame it took, on the
    /// surface `sent_surface` names. A frame encodes only the tiles
    /// that differ from these.
    sent: Vec<u8>,
    sent_surface: Option<u16>,
    /// How many tiles the last frame encoded.
    tiles_sent: usize,
}

/// A frame an update planned under the locks, to encode without them.
struct Frame {
    connection: u64,
    surface: u16,
    width: u16,
    height: u16,
}

/// Runs a server's EGFX updates on a thread of their own, at most 60 a
/// second, until dropped.
pub(crate) struct Worker {
    stop: Arc<AtomicBool>,
    changed: Arc<Changed>,
    thread: Option<thread::JoinHandle<()>>,
    log: slog::Logger,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.changed.wake();
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                slog::warn!(self.log, "EGFX worker panicked");
            }
        }
    }
}

/// Wakes the worker when the display changes.
#[derive(Default)]
struct Changed {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl DisplayWaker for Changed {
    fn wake(&self) {
        *self.flag.lock().expect("egfx: changed lock") = true;
        self.cv.notify_all();
    }
}

impl Changed {
    /// Wait up to `timeout` for a change since the last wait.
    fn wait(&self, timeout: Duration) {
        let flag = self.flag.lock().expect("egfx: changed lock");
        let (mut flag, _) = self
            .cv
            .wait_timeout_while(flag, timeout, |changed| !*changed)
            .expect("egfx: changed lock");
        *flag = false;
    }
}

impl Egfx {
    pub(crate) fn new(log: slog::Logger) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                pipeline: None,
                sender: None,
                surface: None,
                pending: None,
                started: Instant::now(),
                connection: 0,
            }),
            render: Mutex::new(Render {
                encoder: ProgressiveEncoder::new(),
                connection: 0,
                pixels: Vec::new(),
                sent: Vec::new(),
                sent_surface: None,
                tiles_sent: 0,
            }),
            log,
            failures: TokenBucket::new(ENCODE_LOG_BURST, ENCODE_LOG_PERIOD),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("egfx: state lock")
    }

    /// Update from `source` on a thread of its own while a client's
    /// EGFX channel is ready.
    pub(crate) fn start_worker(
        self: &Arc<Self>,
        source: Arc<dyn DisplaySource>,
    ) -> std::io::Result<Worker> {
        self.start_worker_polling(source, FRAME_INTERVAL)
    }

    /// Update when `source` changes, frames at least [`FRAME_INTERVAL`]
    /// apart, and every `poll` besides: a display that cannot say when
    /// it changed, and a frame the client refused, are looked at again
    /// then.
    pub(crate) fn start_worker_polling(
        self: &Arc<Self>,
        source: Arc<dyn DisplaySource>,
        poll: Duration,
    ) -> std::io::Result<Worker> {
        let stop = Arc::new(AtomicBool::new(false));
        let changed = Arc::new(Changed::default());
        source.on_change(
            Arc::downgrade(&changed) as std::sync::Weak<dyn DisplayWaker>
        );
        let egfx = Arc::clone(self);
        let stopped = Arc::clone(&stop);
        let woken = Arc::clone(&changed);
        let thread = thread::Builder::new().name("rdp-egfx".into()).spawn(
            move || {
                // Whether the guest is paced to this worker's frames.
                let mut pace: Option<Pace> = None;
                while !stopped.load(Ordering::Acquire) {
                    let began = Instant::now();
                    if egfx.active() {
                        if let Some(taken) = egfx.update(source.as_ref()) {
                            let interval = pace
                                .get_or_insert_with(Pace::default)
                                .frame(began.elapsed(), taken);
                            source.pace(Some(interval));
                        }
                    } else if pace.take().is_some() {
                        source.pace(None);
                    }
                    thread::sleep(
                        FRAME_INTERVAL.saturating_sub(began.elapsed()),
                    );
                    if !stopped.load(Ordering::Acquire) {
                        woken.wait(poll);
                    }
                }
                if pace.is_some() {
                    source.pace(None);
                }
            },
        )?;
        Ok(Worker {
            stop,
            changed,
            thread: Some(thread),
            log: self.log.clone(),
        })
    }

    /// Whether the EGFX channel is ready to carry the display.
    pub(crate) fn active(&self) -> bool {
        let pipeline = self.lock().pipeline.clone();
        pipeline.is_some_and(|p| p.lock().expect("egfx: pipeline").is_ready())
    }

    /// Send the display's changes over EGFX: a new surface when there is
    /// none or the size changed, then the damaged tiles. `None` when
    /// there was no frame to make; otherwise whether the client took it.
    pub(crate) fn update(&self, source: &dyn DisplaySource) -> Option<bool> {
        let mut render = self.render.lock().expect("egfx: render lock");
        let frame = self.plan(&mut render, source)?;
        let encoded = encode(&mut render, &frame, source);
        let taken = self.finish(&frame, encoded);
        if taken {
            let render = &mut *render;
            std::mem::swap(&mut render.sent, &mut render.pixels);
            render.sent_surface = Some(frame.surface);
        }
        Some(taken)
    }

    /// Under the locks the server loop also takes, briefly: fit the
    /// surface to the display, gather damage and choose what to encode.
    /// `None` when there is nothing to encode now; what the pipeline
    /// queued meanwhile is sent.
    fn plan(
        &self,
        render: &mut Render,
        source: &dyn DisplaySource,
    ) -> Option<Frame> {
        let mut st = self.lock();
        let pipeline = st.pipeline.clone()?;
        let mut gfx = pipeline.lock().expect("egfx: pipeline");
        if !gfx.is_ready() {
            return None;
        }
        if render.connection != st.connection {
            render.encoder = ProgressiveEncoder::new();
            render.connection = st.connection;
            render.sent_surface = None;
        }
        // One read: a width and a height read apart can straddle a
        // resize and size a frame neither mode has.
        let (w, h) = source.size();
        let (Ok(width), Ok(height)) = (u16::try_from(w), u16::try_from(h))
        else {
            return None;
        };
        if width == 0 || height == 0 {
            return None;
        }
        let whole = Rect {
            x: 0,
            y: 0,
            width: u32::from(width),
            height: u32::from(height),
        };

        let surface_id = match st.surface {
            Some((id, w, h)) if (w, h) == (width, height) => id,
            previous => {
                if previous.is_some() {
                    // Deletes the old surface and resets the output, to
                    // one primary monitor the size of the display.
                    gfx.resize_with_monitors(
                        width,
                        height,
                        vec![Monitor {
                            left: 0,
                            top: 0,
                            // TS_MONITOR_DEF corners are inclusive.
                            right: i32::from(width) - 1,
                            bottom: i32::from(height) - 1,
                            flags: MonitorFlags::PRIMARY,
                        }],
                    );
                }
                let id = gfx.create_surface_with_format(
                    width,
                    height,
                    PixelFormat::XRgb,
                )?;
                gfx.map_surface_to_output(id, 0, 0);
                slog::info!(self.log, "EGFX surface";
                    "id" => id, "width" => width, "height" => height);
                st.surface = Some((id, width, height));
                st.pending = Some(whole);
                id
            }
        };

        if let Some(damage) = source.take_damage() {
            st.pending = Some(match st.pending {
                Some(p) => p.union(damage),
                None => damage,
            });
        }
        match st.pending.and_then(|p| clip(p, whole)) {
            Some(_) => Some(Frame {
                connection: st.connection,
                surface: surface_id,
                width,
                height,
            }),
            None => {
                st.pending = None;
                self.send(&st, &mut gfx);
                None
            }
        }
    }

    /// Under the locks again, briefly: queue the encoded frame and send
    /// what the pipeline has. A frame for a connection or surface that
    /// has gone since is dropped, and its damage stays pending.
    /// True when the client has the frame now: queued, or with nothing
    /// in it to send.
    fn finish(
        &self,
        frame: &Frame,
        encoded: anyhow::Result<Option<Vec<u8>>>,
    ) -> bool {
        let mut st = self.lock();
        let Some(pipeline) = st.pipeline.clone() else {
            return false;
        };
        let mut gfx = pipeline.lock().expect("egfx: pipeline");
        let taken = self.queue(&mut st, &mut gfx, frame, encoded);
        self.send(&st, &mut gfx);
        taken
    }

    /// Queue `encoded` on the pipeline, unless the connection or surface
    /// it was planned for has gone. True when the client has the frame
    /// now: queued, or with no tile changed and so nothing to queue.
    fn queue(
        &self,
        st: &mut State,
        gfx: &mut GraphicsPipelineServer,
        frame: &Frame,
        encoded: anyhow::Result<Option<Vec<u8>>>,
    ) -> bool {
        if st.connection != frame.connection
            || st.surface.map(|(id, _, _)| id) != Some(frame.surface)
            || !gfx.is_ready()
        {
            return false;
        }
        match encoded {
            Ok(None) => {
                st.pending = None;
                true
            }
            Ok(Some(data)) => {
                let ms = st.started.elapsed().as_millis() as u32;
                // None: the client has too many frames in flight; the
                // damage waits for the next round.
                let queued = gfx
                    .send_remotefx_progressive_frame(
                        frame.surface,
                        CODEC_CONTEXT,
                        data,
                        ms,
                    )
                    .is_some();
                if queued {
                    st.pending = None;
                }
                queued
            }
            // The damage stays pending: the client never had the frame.
            Err(e) => {
                if self.failures.take() {
                    slog::warn!(self.log, "EGFX frame not encoded"; "error" => format!("{e:#}"));
                }
                false
            }
        }
    }

    /// Hand what the pipeline queued to the server loop.
    fn send(&self, st: &State, gfx: &mut GraphicsPipelineServer) {
        let messages = gfx.drain_output();
        if messages.is_empty() {
            return;
        }
        let (Some(channel), Some(sender)) =
            (gfx.channel_id(), st.sender.as_ref())
        else {
            return;
        };
        match ironrdp_dvc::encode_dvc_messages(
            channel,
            messages,
            ChannelFlags::empty(),
        ) {
            Ok(messages) => {
                // The receiver goes with the connection; a send after it
                // ends has nowhere to go and nothing to do.
                let _ = sender.send(ServerEvent::Egfx(
                    EgfxServerMessage::SendMessages { messages },
                ));
            }
            // The drained frames are lost, and the pipeline still counts
            // them in flight, so the client could be left waiting for
            // frames that never come: the connection ends instead, and
            // the client's next one starts over. It warns only until the
            // server loop takes the Quit, so the warning needs no limit.
            Err(e) => {
                slog::warn!(self.log, "EGFX output not framed; ending the connection";
                    "error" => format!("{e:?}"));
                let _ = sender.send(ServerEvent::Quit(
                    "EGFX output not framed".to_string(),
                ));
            }
        }
    }
}

/// The frame interval a worker sustains, which it asks the guest to
/// flip no faster than: a frame the guest makes faster is one no client
/// sees, and its vCPU time is spent for nothing.
#[derive(Default)]
struct Pace {
    interval: Option<Duration>,
}

impl Pace {
    /// Account a frame that took `cost` to make, which the client took
    /// or refused, and return the interval to ask for.
    ///
    /// A frame costs at least the worker's own interval. A refused
    /// frame lost its slot to the client's flow control as well: the
    /// client acknowledges more slowly than the frames go out. The
    /// average over the last few frames rides out a single slow one.
    fn frame(&mut self, cost: Duration, taken: bool) -> Duration {
        let mut sample = cost.max(FRAME_INTERVAL);
        if !taken {
            sample += FRAME_INTERVAL;
        }
        let interval = match self.interval {
            Some(i) => (i * 3 + sample) / 4,
            None => sample,
        };
        self.interval = Some(interval);
        interval
    }
}

/// Read the display and encode the tiles of the frame planned that
/// differ from what the client has, holding only the worker's own lock.
/// `None` when no tile differs.
fn encode(
    render: &mut Render,
    frame: &Frame,
    source: &dyn DisplaySource,
) -> anyhow::Result<Option<Vec<u8>>> {
    let (w, h) = (u32::from(frame.width), u32::from(frame.height));
    let whole = Rect {
        x: 0,
        y: 0,
        width: w,
        height: h,
    };
    let Render {
        encoder,
        pixels,
        sent,
        sent_surface,
        tiles_sent,
        ..
    } = render;
    pixels.resize(w as usize * h as usize * 4, 0);
    source.read_rect(whole, pixels)?;
    let tiles: Vec<(u32, u32)> = if *sent_surface == Some(frame.surface) {
        let view = |data| Pixels {
            data,
            stride: w,
            x: 0,
            y: 0,
        };
        changed_tiles(view(sent), view(pixels), whole)
            .iter()
            .flat_map(|r| {
                let ty = r.y / TILE;
                (r.x / TILE..(r.x + r.width).div_ceil(TILE))
                    .map(move |tx| (tx, ty))
            })
            .collect()
    } else {
        (0..h.div_ceil(TILE))
            .flat_map(|ty| (0..w.div_ceil(TILE)).map(move |tx| (tx, ty)))
            .collect()
    };
    *tiles_sent = tiles.len();
    if tiles.is_empty() {
        return Ok(None);
    }
    encoder
        .encode_tiles(pixels, frame.width, frame.height, &tiles)
        .map(Some)
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

/// Hands IronRDP a pipeline per connection, and keeps its handle.
pub(crate) struct Factory(pub(crate) Arc<Egfx>);

/// Each connection's server owns one, so the pipeline ends with the
/// connection even when the client never closed its EGFX channel, and a
/// frame planned for it is dropped.
impl Drop for Factory {
    fn drop(&mut self) {
        let mut st = self.0.lock();
        st.new_connection(None);
        st.sender = None;
    }
}

impl ServerEventSender for Factory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        self.0.lock().sender = Some(sender);
    }
}

impl GfxServerFactory for Factory {
    fn build_gfx_handler(&self) -> Box<dyn GraphicsPipelineHandler> {
        Box::new(Handler {
            log: self.0.log.clone(),
        })
    }

    /// A new connection: a new pipeline, and nothing sent to it yet.
    fn build_server_with_handle(
        &self,
    ) -> Option<(GfxDvcBridge, GfxServerHandle)> {
        let handle = Arc::new(Mutex::new(GraphicsPipelineServer::new(
            self.build_gfx_handler(),
        )));
        self.0.lock().new_connection(Some(Arc::clone(&handle)));
        Some((GfxDvcBridge::new(Arc::clone(&handle)), handle))
    }
}

/// Logs the negotiation. Its callbacks run with the pipeline locked.
struct Handler {
    log: slog::Logger,
}

impl GraphicsPipelineHandler for Handler {
    fn capabilities_advertise(
        &mut self,
        pdu: &ironrdp_egfx::pdu::CapabilitiesAdvertisePdu,
    ) {
        slog::debug!(self.log, "EGFX client capabilities"; "caps" => format!("{pdu:?}"));
    }

    fn on_ready(&mut self, negotiated: &ironrdp_egfx::pdu::CapabilitySet) {
        slog::info!(self.log, "EGFX ready"; "negotiated" => format!("{negotiated:?}"));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::atomic::AtomicUsize;

    use ironrdp_dvc::DvcProcessor;
    use ironrdp_egfx::pdu::{
        CapabilitiesAdvertisePdu, CapabilitiesV107Flags, CapabilitySet, GfxPdu,
        RawCapabilitySet,
    };
    use tokio::sync::mpsc::UnboundedReceiver;
    use vmm_devices::display::Rect;

    use super::*;
    use crate::testsupport::FakeDisplay;

    const CHANNEL: u32 = 3;

    /// A server's EGFX, one connection's pipeline and the event queue
    /// the server loop would drain.
    pub(crate) struct Harness {
        pub(crate) egfx: Arc<Egfx>,
        factory: Factory,
        handle: GfxServerHandle,
        events: UnboundedReceiver<ServerEvent>,
    }

    impl Harness {
        pub(crate) fn new() -> Self {
            Self::logged(slog::Logger::root(slog::Discard, slog::o!()))
        }

        /// A harness whose EGFX logs to `log`.
        fn logged(log: slog::Logger) -> Self {
            let egfx = Egfx::new(log);
            let mut factory = Factory(Arc::clone(&egfx));
            let (tx, events) = ServerEvent::create_channel();
            factory.set_sender(tx);
            let (_bridge, handle) =
                factory.build_server_with_handle().expect("a pipeline");
            Self {
                egfx,
                factory,
                handle,
                events,
            }
        }

        /// Open the channel and negotiate version 10.7, as the macOS
        /// client does.
        pub(crate) fn ready(&self) {
            ready(&self.handle);
        }

        fn surfaces(&self) -> Vec<(u16, u16)> {
            let gfx = self.handle.lock().unwrap();
            gfx.surface_ids()
                .filter_map(|id| {
                    gfx.get_surface(id).map(|s| (s.width, s.height))
                })
                .collect()
        }

        pub(crate) fn frames(&self) -> u32 {
            self.handle.lock().unwrap().frames_in_flight()
        }

        /// Render `source` and return the EGFX PDUs that queued, read
        /// back as a client reads them.
        fn rendered(&self, source: &dyn DisplaySource) -> Vec<GfxPdu> {
            let mut render = self.egfx.render.lock().unwrap();
            if let Some(frame) = self.egfx.plan(&mut render, source) {
                let encoded = encode(&mut render, &frame, source);
                let mut st = self.egfx.lock();
                let mut gfx = self.handle.lock().unwrap();
                self.egfx.queue(&mut st, &mut gfx, &frame, encoded);
            }
            let mut gfx = self.handle.lock().unwrap();
            let mut zgfx = ironrdp_graphics::zgfx::Decompressor::new();
            gfx.drain_output()
                .into_iter()
                .map(|m| {
                    let wire =
                        ironrdp_core::encode_vec(m.as_ref()).expect("encode");
                    let mut pdu = Vec::new();
                    zgfx.decompress(&wire, &mut pdu).expect("unwrap");
                    ironrdp_core::decode::<GfxPdu>(&pdu).expect("decode")
                })
                .collect()
        }

        /// How many EGFX batches the server loop was asked to send.
        fn sent(&mut self) -> usize {
            let mut n = 0;
            while let Ok(event) = self.events.try_recv() {
                assert!(matches!(event, ServerEvent::Egfx(_)), "{event:?}");
                n += 1;
            }
            n
        }
    }

    #[test]
    fn nothing_is_sent_before_the_channel_is_ready() {
        let mut h = Harness::new();
        h.egfx.update(&FakeDisplay::new(64, 48));
        assert!(!h.egfx.active());
        assert!(h.surfaces().is_empty());
        assert_eq!(h.sent(), 0);
    }

    #[test]
    fn a_ready_channel_gets_a_surface_and_the_whole_display() {
        let mut h = Harness::new();
        h.ready();
        let display = FakeDisplay::new(200, 100);
        assert!(h.egfx.active());
        h.egfx.update(&display);
        assert_eq!(h.surfaces(), [(200, 100)]);
        assert_eq!(h.frames(), 1, "the whole display, one frame");
        assert!(h.sent() > 0, "and the server loop was told to send it");
    }

    #[test]
    fn a_change_sends_a_frame_and_no_damage_sends_none() {
        let mut h = Harness::new();
        h.ready();
        let display = FakeDisplay::new(200, 100);
        h.egfx.update(&display);
        h.sent();

        h.egfx.update(&display);
        assert_eq!(h.frames(), 1, "nothing changed");
        assert_eq!(h.sent(), 0);

        display.paint(rect(10, 10, 5, 5), 1);
        h.egfx.update(&display);
        assert_eq!(h.frames(), 2);
        assert!(h.sent() > 0);
    }

    #[test]
    fn a_size_change_makes_a_new_surface() {
        let mut h = Harness::new();
        h.ready();
        let display = FakeDisplay::new(200, 100);
        h.egfx.update(&display);
        *display.size.lock().unwrap() = (320, 240);
        h.egfx.update(&display);
        assert_eq!(h.surfaces(), [(320, 240)]);
        assert!(h.sent() > 0);
    }

    /// A size change resets the client's graphics to the new size with
    /// one primary monitor covering it. The macOS client did not show
    /// the new surface after a reset that named no monitor, until it
    /// reconnected.
    #[test]
    fn a_size_change_resets_the_client_to_one_monitor_of_that_size() {
        let h = Harness::new();
        h.ready();
        let display = FakeDisplay::new(200, 100);
        h.rendered(&display);
        *display.size.lock().unwrap() = (320, 240);
        let pdus = h.rendered(&display);
        let reset = pdus
            .iter()
            .find_map(|p| match p {
                GfxPdu::ResetGraphics(r) => Some(r),
                _ => None,
            })
            .expect("a size change resets the client's graphics");
        assert_eq!((reset.width, reset.height), (320, 240));
        assert_eq!(
            reset.monitors,
            [Monitor {
                left: 0,
                top: 0,
                right: 319,
                bottom: 239,
                flags: MonitorFlags::PRIMARY,
            }]
        );
    }

    /// The surface's width and height come from one read of the size: a
    /// pair read across a resize can size a frame neither mode has.
    #[test]
    fn a_surface_is_one_size_never_half_of_two() {
        let h = Harness::new();
        h.ready();
        let display = FakeDisplay::new(200, 100);
        display
            .resizes
            .lock()
            .unwrap()
            .extend([(200, 100), (320, 240)]);
        h.rendered(&display);
        let surfaces = h.surfaces();
        assert!(
            surfaces
                .iter()
                .all(|s| [(200, 100), (320, 240)].contains(s)),
            "{surfaces:?}"
        );
    }

    /// Open `handle`'s channel and negotiate version 10.7, as the macOS
    /// client does.
    fn ready(handle: &GfxServerHandle) {
        let caps =
            GfxPdu::CapabilitiesAdvertise(CapabilitiesAdvertisePdu(vec![
                RawCapabilitySet::from(&CapabilitySet::V10_7 {
                    flags: CapabilitiesV107Flags::SMALL_CACHE,
                }),
            ]));
        let bytes = ironrdp_core::encode_vec(&caps).expect("encode caps");
        let mut gfx = handle.lock().unwrap();
        gfx.start(CHANNEL).expect("start");
        gfx.process(CHANNEL, &bytes).expect("process caps");
        assert!(gfx.is_ready());
    }

    /// A display whose `read_rect` waits until the test lets it go, and
    /// says when it has started waiting.
    struct SlowDisplay {
        inner: FakeDisplay,
        reading: std::sync::mpsc::SyncSender<()>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl DisplaySource for SlowDisplay {
        fn size(&self) -> (u32, u32) {
            self.inner.size()
        }
        fn take_damage(&self) -> Option<Rect> {
            self.inner.take_damage()
        }
        fn read_rect(&self, r: Rect, dst: &mut [u8]) -> std::io::Result<()> {
            let _ = self.reading.send(());
            let _ = self.release.lock().unwrap().recv();
            self.inner.read_rect(r, dst)
        }
    }

    /// Encoding a frame takes tens of milliseconds at a large size, and
    /// the server loop takes EGFX's state and the pipeline's lock for its
    /// own work, the client's frame acknowledgements among it. Neither
    /// may be held while a frame is read and encoded.
    #[test]
    fn nothing_the_server_loop_needs_is_held_while_a_frame_encodes() {
        use std::sync::mpsc::{channel, sync_channel};
        use std::time::Duration;

        let h = Harness::new();
        h.ready();
        let (reading, started) = sync_channel(1);
        let (release, wait) = channel();
        let display = Arc::new(SlowDisplay {
            inner: FakeDisplay::new(64, 48),
            reading,
            release: Mutex::new(wait),
        });
        let worker = {
            let egfx = Arc::clone(&h.egfx);
            let display = Arc::clone(&display);
            thread::spawn(move || egfx.update(display.as_ref()))
        };
        started
            .recv_timeout(Duration::from_secs(5))
            .expect("the update reads the display");

        let (done, finished) = channel();
        let probe = {
            let egfx = Arc::clone(&h.egfx);
            let handle = Arc::clone(&h.handle);
            thread::spawn(move || {
                let active = egfx.active();
                drop(handle.lock().unwrap());
                let _ = done.send(active);
            })
        };
        let free = finished.recv_timeout(Duration::from_millis(500));
        release.send(()).expect("let the read finish");
        worker.join().expect("update");
        probe.join().expect("probe");
        assert_eq!(free, Ok(true), "the server loop waited on the encode");
        assert_eq!(h.frames(), 1, "and the frame still went out");
    }

    /// A frame planned for a connection that ended while it encoded is
    /// not queued on any pipeline.
    #[test]
    fn a_frame_for_a_connection_that_ended_is_dropped() {
        let h = Harness::new();
        h.ready();
        let display = FakeDisplay::new(64, 48);
        let mut render = h.egfx.render.lock().unwrap();
        let frame = h.egfx.plan(&mut render, &display).expect("a frame");
        let encoded = encode(&mut render, &frame, &display);
        let (_bridge, _next) = h
            .factory
            .build_server_with_handle()
            .expect("a new connection");
        let mut st = h.egfx.lock();
        let mut gfx = h.handle.lock().unwrap();
        assert!(!h.egfx.queue(&mut st, &mut gfx, &frame, encoded));
        drop(gfx);
        assert_eq!(h.frames(), 0);
    }

    /// Counts the warnings logged to it.
    struct Warnings(Arc<AtomicUsize>);

    impl slog::Drain for Warnings {
        type Ok = ();
        type Err = slog::Never;

        fn log(
            &self,
            record: &slog::Record<'_>,
            _values: &slog::OwnedKVList,
        ) -> Result<(), slog::Never> {
            if record.level() == slog::Level::Warning {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        }
    }

    /// A frame that failed to encode never reached the client, so its
    /// damage stays for the next round, and a still screen is sent once
    /// the encode works. A failure that repeats every frame cannot fill
    /// the log.
    #[test]
    fn a_frame_that_failed_to_encode_is_tried_again() {
        let warnings = Arc::new(AtomicUsize::new(0));
        let log =
            slog::Logger::root(Warnings(Arc::clone(&warnings)), slog::o!());
        let h = Harness::logged(log);
        h.ready();
        let display = FakeDisplay::new(64, 48);
        for _ in 0..50 {
            let mut render = h.egfx.render.lock().unwrap();
            let frame = h
                .egfx
                .plan(&mut render, &display)
                .expect("the damage is still pending");
            let mut st = h.egfx.lock();
            let mut gfx = h.handle.lock().unwrap();
            let failed = Err(anyhow::anyhow!("no pixels"));
            assert!(!h.egfx.queue(&mut st, &mut gfx, &frame, failed));
        }
        let logged = warnings.load(Ordering::Relaxed);
        assert!(logged >= 1, "the failure is logged");
        assert!(logged <= ENCODE_LOG_BURST as usize, "{logged} warnings");

        assert_eq!(h.egfx.update(&display), Some(true));
        assert_eq!(h.frames(), 1);
    }

    /// A connection's server owns its factory, so the factory going
    /// means the connection did, even when the client never closed its
    /// EGFX channel: nothing is encoded or paced for it after that.
    #[test]
    fn a_connection_that_ends_leaves_egfx_inactive() {
        let h = Harness::new();
        h.ready();
        assert!(h.egfx.active());
        let Harness { egfx, factory, .. } = h;
        drop(factory);
        assert!(!egfx.active());
        assert_eq!(egfx.update(&FakeDisplay::new(64, 48)), None);
    }

    /// The worker sends frames on its own thread, and stops when told.
    #[test]
    fn a_worker_sends_frames_and_stops() {
        use std::time::{Duration, Instant};

        let h = Harness::new();
        h.ready();
        let display: Arc<dyn DisplaySource> =
            Arc::new(FakeDisplay::new(64, 48));
        let worker = h.egfx.start_worker(display).expect("a worker");
        let deadline = Instant::now() + Duration::from_secs(5);
        while h.frames() == 0 {
            assert!(Instant::now() < deadline, "the worker sent no frame");
            thread::sleep(Duration::from_millis(10));
        }
        // Joins the worker: a worker that never stopped would hang
        // here, so the bound only turns that into a failure.
        let stopping = Instant::now();
        drop(worker);
        assert!(stopping.elapsed() < Duration::from_secs(10));
    }

    const MS: Duration = Duration::from_millis(1);

    /// Frames cheaper than the worker's own interval ask for that
    /// interval: the guest need not flip faster than frames go out.
    #[test]
    fn cheap_frames_pace_the_guest_at_the_frame_interval() {
        let mut pace = Pace::default();
        for _ in 0..10 {
            assert_eq!(pace.frame(MS, true), FRAME_INTERVAL);
        }
    }

    /// Frames that take longer to make than the interval slow the guest
    /// to the rate they go out at, smoothed over a few frames.
    #[test]
    fn slow_frames_slow_the_guest_to_their_cost() {
        let mut pace = Pace::default();
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
        let mut taken = Pace::default();
        let mut refused = Pace::default();
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
        let mut pace = Pace::default();
        for _ in 0..30 {
            pace.frame(MS * 90, false);
        }
        let mut last = Duration::MAX;
        for _ in 0..60 {
            last = pace.frame(MS, true);
        }
        assert!(last < FRAME_INTERVAL + MS, "{last:?}");
    }

    /// The worker asks the display to pace the guest while it sends,
    /// and withdraws the request when it stops.
    #[test]
    fn a_worker_paces_the_guest_while_it_sends() {
        use std::time::Instant;

        let h = Harness::new();
        h.ready();
        let display = Arc::new(FakeDisplay::new(64, 48));
        let source: Arc<dyn DisplaySource> = display.clone();
        let worker = h.egfx.start_worker(source).expect("a worker");
        let deadline = Instant::now() + Duration::from_secs(5);
        while display.paced.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline, "the worker never paced");
            thread::sleep(MS * 10);
        }
        drop(worker);
        let paced = display.paced.lock().unwrap();
        assert!(
            paced[..paced.len() - 1]
                .iter()
                .all(|p| p.is_some_and(|i| i >= FRAME_INTERVAL)),
            "{paced:?}"
        );
        assert_eq!(paced.last(), Some(&None), "withdrawn at the end");
    }

    /// When the client goes, so does its request.
    #[test]
    fn a_worker_withdraws_its_pace_when_the_client_goes() {
        use std::time::Instant;

        let h = Harness::new();
        h.ready();
        let display = Arc::new(FakeDisplay::new(64, 48));
        let source: Arc<dyn DisplaySource> = display.clone();
        let _worker = h.egfx.start_worker(source).expect("a worker");
        let deadline = Instant::now() + Duration::from_secs(5);
        while display.paced.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline, "the worker never paced");
            thread::sleep(MS * 10);
        }
        // A new connection, not yet ready, replaces the client's.
        let (_bridge, _next) = h
            .factory
            .build_server_with_handle()
            .expect("a new connection");
        while display.paced.lock().unwrap().last() != Some(&None) {
            assert!(Instant::now() < deadline, "the pace was never withdrawn");
            thread::sleep(MS * 10);
        }
    }

    /// A change wakes the worker: it need not wait out its poll.
    #[test]
    fn a_change_wakes_the_worker() {
        use std::time::Instant;

        let h = Harness::new();
        h.ready();
        let display = Arc::new(FakeDisplay::new(64, 48));
        let source: Arc<dyn DisplaySource> = display.clone();
        let worker = h
            .egfx
            .start_worker_polling(source, Duration::from_secs(3600))
            .expect("a worker");
        let deadline = Instant::now() + Duration::from_secs(5);
        while h.frames() == 0 {
            assert!(Instant::now() < deadline, "no first frame");
            thread::sleep(MS * 10);
        }
        display.paint(rect(0, 0, 8, 8), 9);
        while h.frames() < 2 {
            assert!(Instant::now() < deadline, "the change was not sent");
            thread::sleep(MS * 10);
        }
        // Stopping does not wait out the poll either, once the worker
        // is past its frame spacing and waiting for a change.
        // Stopped on a thread of its own, so a stop that waits fails
        // the test rather than hanging it.
        thread::sleep(FRAME_INTERVAL * 3);
        let (done, stopped) = std::sync::mpsc::channel();
        thread::spawn(move || {
            drop(worker);
            let _ = done.send(());
        });
        stopped
            .recv_timeout(Duration::from_secs(1))
            .expect("stopping waited out the poll");
    }

    /// With no client the worker asks nothing of the guest.
    #[test]
    fn a_worker_without_a_client_does_not_pace() {
        let h = Harness::new();
        let display = Arc::new(FakeDisplay::new(64, 48));
        let source: Arc<dyn DisplaySource> = display.clone();
        let worker = h.egfx.start_worker(source).expect("a worker");
        thread::sleep(FRAME_INTERVAL * 3);
        drop(worker);
        assert!(display.paced.lock().unwrap().is_empty());
    }

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn tiles_sent(h: &Harness) -> usize {
        h.egfx.render.lock().unwrap().tiles_sent
    }

    /// A page-flipping guest reports its whole display changed on every
    /// flip, so EGFX compares each tile with the frame the client has
    /// and encodes only those that differ.
    #[test]
    fn only_tiles_that_changed_are_encoded() {
        let h = Harness::new();
        h.ready();
        // Four tiles by two.
        let display = FakeDisplay::new(256, 128);
        h.egfx.update(&display);
        assert_eq!(tiles_sent(&h), 8, "the first frame is every tile");

        display.paint(rect(70, 10, 10, 10), 7);
        h.egfx.update(&display);
        assert_eq!(tiles_sent(&h), 1, "one tile changed");
        assert_eq!(h.frames(), 2);

        // Reported changed, but nothing is.
        display.paint(rect(70, 10, 10, 10), 7);
        h.egfx.update(&display);
        assert_eq!(h.frames(), 2, "no frame for no change");
    }

    /// A frame the client's flow control refused never reached it, so
    /// its tiles go with the next frame.
    #[test]
    fn tiles_of_a_refused_frame_go_with_the_next() {
        let h = Harness::new();
        h.ready();
        let display = FakeDisplay::new(256, 128);
        h.handle.lock().unwrap().set_max_frames_in_flight(1);
        h.egfx.update(&display);
        assert_eq!(h.frames(), 1);

        display.paint(rect(0, 0, 10, 10), 1);
        h.egfx.update(&display);
        assert_eq!(h.frames(), 1, "refused: one frame already in flight");

        h.handle.lock().unwrap().set_max_frames_in_flight(3);
        display.paint(rect(200, 100, 10, 10), 2);
        h.egfx.update(&display);
        assert_eq!(h.frames(), 2);
        assert_eq!(tiles_sent(&h), 2, "the refused tile and the new one");
    }

    /// A new surface starts blank on the client, so it gets every tile.
    #[test]
    fn a_new_surface_gets_every_tile() {
        let h = Harness::new();
        h.ready();
        let display = FakeDisplay::new(256, 128);
        h.egfx.update(&display);
        *display.size.lock().unwrap() = (128, 128);
        h.egfx.update(&display);
        assert_eq!(tiles_sent(&h), 4);
    }

    /// A new connection's client has nothing, whatever the last one had.
    #[test]
    fn a_new_connection_gets_every_tile() {
        let h = Harness::new();
        h.ready();
        let display = FakeDisplay::new(256, 128);
        h.egfx.update(&display);
        let (_bridge, next) = h
            .factory
            .build_server_with_handle()
            .expect("a new connection");
        ready(&next);
        h.egfx.update(&display);
        assert_eq!(tiles_sent(&h), 8);
    }

    /// Each connection gets a new pipeline, which starts unready.
    #[test]
    fn a_new_connection_starts_over() {
        let h = Harness::new();
        h.ready();
        assert!(h.egfx.active());
        let (_bridge, _handle) =
            h.factory.build_server_with_handle().expect("a pipeline");
        assert!(!h.egfx.active());
    }
}
