// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The VNC server that streams a display over a unix socket.
//!
//! One thread accepts on the socket, and serves one client at a time on
//! a thread of its own, which polls it for RFB messages and sends a
//! frame when one is asked for. A new client replaces the one before.
//! The pixels come from a [`DisplaySource`].

use std::io::{self, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use vmm_core::unixsock::accept::accept_loop;
use vmm_core::unixsock::write_all_bounded;
use vmm_devices::display::{
    changed_tiles, CursorShape, DisplaySource, FramePace, Pixels, Rect,
};
use vmm_devices::input::scale_to_tablet;
use vmm_devices::InputBroker;

use rfb::{
    ClientMessage, PixelFormat, RectHeader, EDS_REASON_CLIENT,
    EDS_REASON_SERVER, EDS_STATUS_OK, EDS_STATUS_PROHIBITED, ENCODING_CURSOR,
    ENCODING_DESKTOP_SIZE, ENCODING_EXTENDED_DESKTOP_SIZE, ENCODING_RAW,
    ENCODING_ZRLE,
};
use zrle::ZrleEncoder;
pub mod rfb;
pub mod zrle;

/// Bytes per pixel on the wire (XRGB8888).
const BYTES_PER_PIXEL: u64 = 4;

/// How long one send may wait for the VNC client to take bytes.
///
/// The VNC thread also runs the accept loop, so a client that stops
/// reading costs every later viewer as well.
const VNC_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// VNC update interval (~30 fps) for a display the guest cannot be paced
/// on, fbuf: it reports its whole frame changed on every pass.
const VNC_FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// The update interval for a display that takes a pace: 60 frames a
/// second, what a client's display shows, while the client keeps up; the
/// guest is paced to what it gets.
const FAST_FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 60);

/// How often to look for a frame to send for `source`.
fn frame_interval(source: &dyn DisplaySource) -> Duration {
    if source.takes_pace() {
        FAST_FRAME_INTERVAL
    } else {
        VNC_FRAME_INTERVAL
    }
}

/// How long one poll for a client message waits. The same thread sends
/// frames, so this bounds how late a frame can be.
const CLIENT_POLL: Duration = Duration::from_millis(5);

/// DTrace probes for the frames the VNC server sends.
#[usdt::provider(provider = "vmm")]
mod probes {
    /// The server sent a frame: `encode_us` to read, encode and write
    /// it, after which the guest is asked for `pace_us`.
    fn vnc_frame(encode_us: u64, pace_us: u64) {}
    /// Enabled, the server draws its numbers over the frames it sends;
    /// see [`vmm_devices::overlay`].
    fn vnc_overlay() {}
}

/// Whether DTrace has the `vnc_overlay` probe enabled: the probe's
/// arguments are only made then.
fn overlay_asked() -> bool {
    let asked = std::cell::Cell::new(false);
    probes::vnc_overlay!(|| asked.set(true));
    asked.get()
}

/// A duration in whole microseconds, for a probe argument.
fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// Draw `text` over the client's frames from the next update on, or
/// stop with `None`. The corner it covered, or now covers, goes with the
/// next update either way.
fn show_overlay(view: &mut ClientView, text: Option<String>) {
    if view.overlay == text {
        return;
    }
    let corner = [view.overlay.as_deref(), text.as_deref()]
        .into_iter()
        .flatten()
        .map(vmm_devices::overlay::area)
        .chain(view.overlay_corner)
        .reduce(Rect::union);
    view.overlay_corner = corner;
    view.overlay = text;
}

/// Starts the server's passes a frame interval apart, whatever each one
/// cost: a fixed sleep after the work would add the work to every frame.
struct Ticker {
    next: Instant,
    interval: Duration,
}

impl Ticker {
    fn new(start: Instant, interval: Duration) -> Self {
        Self {
            next: start + interval,
            interval,
        }
    }

    /// How long to sleep, at `now`, until the next pass starts. A pass
    /// that ran past its frame starts the next at once, and the frames
    /// count from then: a late one is not made up for.
    fn sleep_after(&mut self, now: Instant) -> Duration {
        let wait = self.next.saturating_duration_since(now);
        if wait.is_zero() {
            self.next = now + self.interval;
        } else {
            self.next += self.interval;
        }
        wait
    }
}

/// While a client is connected, asks the display to have the guest flip
/// no faster than the frames reach the client, as the RDP server does: a
/// frame the guest makes faster is one no client sees. The request goes
/// when this does.
struct ClientPace {
    source: Arc<dyn DisplaySource>,
    pace: FramePace,
}

impl ClientPace {
    /// Paced no faster than `floor`, the server's own frame interval.
    fn new(source: Arc<dyn DisplaySource>, floor: Duration) -> Self {
        Self {
            source,
            pace: FramePace::new(floor),
        }
    }

    /// A frame went to the client, and took `cost` to make and send;
    /// the interval the guest is now asked for.
    fn frame(&mut self, cost: Duration) -> Duration {
        let interval = self.pace.frame(cost, true);
        self.source.pace(Some(interval));
        interval
    }
}

impl Drop for ClientPace {
    fn drop(&mut self) {
        self.source.pace(None);
    }
}

/// Desktop name sent to VNC clients.
const VNC_DESKTOP_NAME: &str = "bhyve framebuffer";

/// A VNC server for one display, on its own thread.
pub struct VncServer {
    /// The display it serves.
    source: Arc<dyn DisplaySource>,

    /// VNC password (None = no auth).
    password: Option<String>,

    /// Path to the VNC Unix socket.
    path: PathBuf,

    /// Late-bound remote input destinations.
    input: Arc<InputBroker>,

    /// Flag to signal the VNC thread to shut down.
    shutdown: AtomicBool,

    /// The VNC thread, so shutdown can wait for it to leave.
    thread: Mutex<Option<thread::JoinHandle<()>>>,

    /// Structured logger.
    log: slog::Logger,
}

impl VncServer {
    /// Bind `path` and serve `source` on a thread called `name`.
    ///
    /// If `password` is `Some`, VNC authentication is required.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket cannot be bound or the server's
    /// thread cannot start.
    pub fn start(
        name: &str,
        source: Arc<dyn DisplaySource>,
        path: &Path,
        password: Option<String>,
        input: Arc<InputBroker>,
        log: slog::Logger,
    ) -> anyhow::Result<Arc<Self>> {
        let listener = vmm_core::unixsock::bind_restricted(
            path,
            vmm_core::unixsock::SocketPolicy::default(),
        )?;
        let srv = Arc::new(Self::new(source, path, password, input, log));

        let server = Arc::clone(&srv);
        let spawned = thread::Builder::new()
            .name(name.into())
            .spawn(move || server_loop(server, listener));
        let handle = match spawned {
            Ok(handle) => handle,
            Err(e) => {
                // The thread that would have removed the socket never ran.
                let _ = std::fs::remove_file(path);
                return Err(e).context("vnc: spawning the server thread");
            }
        };
        *srv.thread.lock().expect("vnc: thread lock") = Some(handle);

        Ok(srv)
    }

    fn new(
        source: Arc<dyn DisplaySource>,
        path: &Path,
        password: Option<String>,
        input: Arc<InputBroker>,
        log: slog::Logger,
    ) -> Self {
        Self {
            source,
            password,
            path: path.to_owned(),
            input,
            shutdown: AtomicBool::new(false),
            thread: Mutex::new(None),
            log,
        }
    }

    /// Stop the VNC server and wait for its thread to leave.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        let handle = self.thread.lock().expect("vnc: thread lock").take();
        if let Some(handle) = handle {
            if handle.join().is_err() {
                slog::warn!(self.log, "VNC server thread panicked");
            }
        }
    }

    /// The display's size as RFB carries it.
    fn size(&self) -> (u16, u16) {
        let (w, h) = self.source.size();
        (
            u16::try_from(w).unwrap_or(u16::MAX),
            u16::try_from(h).unwrap_or(u16::MAX),
        )
    }
}

/// Main VNC server loop running in a dedicated thread.
///
/// One client at a time. The shared accept loop polls the listener, so
/// `shutdown` is seen within its tick and not only when the next client
/// arrives and leaves.
fn server_loop(srv: Arc<VncServer>, listener: UnixListener) {
    slog::info!(srv.log, "VNC server listening"; "path" => ?srv.path);

    let log = srv.log.clone();
    let mut current: Option<Client> = None;
    accept_loop(listener, &srv.shutdown, &log, "vnc", |stream| {
        // The newest client wins: one whose connection went dead would
        // otherwise hold VNC for everyone until it timed out.
        if let Some(old) = current.take() {
            slog::info!(srv.log, "VNC client replaced by a new one");
            old.end(&srv.log);
        }
        current = Client::serve(&srv, stream);
    });
    if let Some(client) = current.take() {
        client.end(&srv.log);
    }

    let _ = std::fs::remove_file(&srv.path);
}

/// The one client being served, on a thread of its own so the server
/// goes on accepting.
struct Client {
    /// Shut down to end the connection, which wakes the client's thread
    /// from whatever it waits on.
    stream: UnixStream,
    thread: thread::JoinHandle<()>,
}

impl Client {
    fn serve(srv: &Arc<VncServer>, stream: UnixStream) -> Option<Self> {
        let ours = match stream.try_clone() {
            Ok(ours) => ours,
            Err(e) => {
                slog::warn!(srv.log, "VNC client not served"; "error" => %e);
                return None;
            }
        };
        let server = Arc::clone(srv);
        let spawned = thread::Builder::new().name("vnc-client".into()).spawn(
            move || {
                slog::info!(server.log, "VNC client connected");
                if let Err(e) = handle_vnc_client(&server, stream) {
                    if !is_expected_disconnect(&e) {
                        slog::warn!(server.log, "VNC client error"; "error" => %e);
                    }
                }
                slog::info!(server.log, "VNC client disconnected");
            },
        );
        match spawned {
            Ok(thread) => Some(Self {
                stream: ours,
                thread,
            }),
            Err(e) => {
                slog::warn!(srv.log, "VNC client not served"; "error" => %e);
                None
            }
        }
    }

    /// End the connection and wait for its thread, so no two clients are
    /// ever served at once.
    fn end(self, log: &slog::Logger) {
        let _ = self.stream.shutdown(Shutdown::Both);
        if self.thread.join().is_err() {
            slog::warn!(log, "VNC client thread panicked");
        }
    }
}

/// Check if an error is an expected client disconnect.
fn is_expected_disconnect(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
    )
}

/// What this client was told, and what it can be told later.
///
/// ServerInit carries the resolution once. A guest mode change after
/// that would otherwise put a rectangle larger than the client's
/// framebuffer on the wire, which most viewers answer by dropping the
/// connection.
struct ClientView {
    width: u16,
    height: u16,
    /// Whether the client listed the DesktopSize pseudo-encoding.
    desktop_size: bool,
    /// Whether the client listed ExtendedDesktopSize, and so may ask for
    /// a size with SetDesktopSize.
    extended_desktop_size: bool,
    /// An ExtendedDesktopSize rectangle the next update owes the client,
    /// as (reason, status): the answer to its SetDesktopSize, or the
    /// first one after it listed the encoding.
    eds_due: Option<(u16, u16)>,
    /// Whether pixels go as ZRLE, which the client listed.
    use_zrle: bool,
    /// The connection's ZRLE stream, made when the client first lists
    /// ZRLE and kept after: its inflater keeps its state as well.
    zrle: Option<ZrleEncoder>,
    /// What the client's framebuffer holds, `width` x `height` XRGB8888,
    /// once it has been sent a whole frame. An incremental update
    /// compares the damaged area against it and sends only the tiles
    /// that differ.
    shadow: Option<Vec<u8>>,
    /// Whether a frame was skipped with the damage it took, so the next
    /// update sends the whole display.
    repaint: bool,
    /// Whether the client listed the Cursor pseudo-encoding, and so draws
    /// the guest's pointer itself.
    cursor: bool,
    /// The pointer's serial the client was last sent, if any.
    cursor_sent: Option<u64>,
    /// Whether the client draws a pointer image the guest gave, which a
    /// pointer the guest takes back must hide.
    cursor_shown: bool,
    /// Updates with pixels sent, so the loop can tell a frame from a
    /// pointer or a size alone.
    frames: u64,
    /// The text drawn over the frames while DTrace asks for it.
    overlay: Option<String>,
    /// Where the overlay was or now is, to go with the next update.
    overlay_corner: Option<Rect>,
}

/// What a client asked for in a FramebufferUpdateRequest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateRequest {
    /// Only what changed, whenever something does.
    Incremental,
    /// The whole framebuffer, now.
    Full,
}

impl UpdateRequest {
    /// Combine with a request still waiting: a full one wins.
    fn merge(self, waiting: Option<UpdateRequest>) -> UpdateRequest {
        match (self, waiting) {
            (UpdateRequest::Full, _) | (_, Some(UpdateRequest::Full)) => {
                UpdateRequest::Full
            }
            _ => UpdateRequest::Incremental,
        }
    }
}

/// Dispatch a VNC message and report the display update it asked for.
fn dispatch_client_message(
    dev: &VncServer,
    view: &mut ClientView,
    msg: ClientMessage,
) -> Option<UpdateRequest> {
    match msg {
        ClientMessage::FbUpdateRequest { incremental, .. } => {
            Some(if incremental {
                UpdateRequest::Incremental
            } else {
                UpdateRequest::Full
            })
        }
        ClientMessage::SetPixelFormat(pf) => {
            if pf.bits_per_pixel != 32 {
                slog::warn!(dev.log, "client requested unsupported pixel format";
                    "bpp" => pf.bits_per_pixel);
            }
            None
        }
        ClientMessage::SetEncodings(encs) => {
            // Pixels go as ZRLE to a client that lists it, else as Raw.
            if !encs.contains(&ENCODING_RAW) && !encs.contains(&ENCODING_ZRLE) {
                slog::warn!(dev.log, "client encodings include neither Raw nor ZRLE; it is sent Raw");
            }
            view.desktop_size = encs.contains(&ENCODING_DESKTOP_SIZE);
            let cursor = encs.contains(&ENCODING_CURSOR);
            if cursor && !view.cursor {
                // A client that starts drawing the pointer needs it now.
                view.cursor_sent = None;
                view.cursor_shown = false;
            }
            view.cursor = cursor;
            let extended = encs.contains(&ENCODING_EXTENDED_DESKTOP_SIZE);
            if extended && !view.extended_desktop_size {
                // The client sends SetDesktopSize only once it has seen
                // an ExtendedDesktopSize rectangle.
                view.eds_due = Some((EDS_REASON_SERVER, EDS_STATUS_OK));
            }
            view.extended_desktop_size = extended;
            view.use_zrle = encs.contains(&ENCODING_ZRLE);
            if view.use_zrle && view.zrle.is_none() {
                view.zrle = Some(ZrleEncoder::new());
            }
            None
        }
        ClientMessage::SetDesktopSize { width, height } => {
            let status =
                if dev.source.request_size(u32::from(width), u32::from(height))
                {
                    EDS_STATUS_OK
                } else {
                    EDS_STATUS_PROHIBITED
                };
            slog::info!(dev.log, "VNC client asked for a new size";
                "width" => width, "height" => height, "status" => status);
            view.eds_due = Some((EDS_REASON_CLIENT, status));
            None
        }
        ClientMessage::KeyEvent { down, key } => {
            dev.input.key_event(down, key);
            None
        }
        ClientMessage::PointerEvent { button_mask, x, y } => {
            // The tablet only clamps values, so scaling must use the
            // display's current size before the event reaches the input
            // broker.
            let (width, height) = dev.size();
            let sx = scale_to_tablet(x, width);
            let sy = scale_to_tablet(y, height);
            dev.input.pointer_event(button_mask, sx, sy);
            None
        }
        ClientMessage::CutText(_) => None,
    }
}

/// Read what the client sent and act on it, folding the updates it asks
/// for into `pending`. False once the client has closed the connection.
///
/// Every message already received is handled, not only the first: a
/// client can ask for updates faster than frames go out, and its input
/// would otherwise wait a frame interval behind each request.
fn read_client_messages<R: Read>(
    dev: &VncServer,
    view: &mut ClientView,
    reader: &mut BufReader<R>,
    pending: &mut Option<UpdateRequest>,
) -> io::Result<bool> {
    loop {
        match rfb::read_client_message(reader) {
            Ok(Some(msg)) => {
                if let Some(req) = dispatch_client_message(dev, view, msg) {
                    *pending = Some(req.merge(*pending));
                }
            }
            Ok(None) => return Ok(false),
            Err(e) if rfb::is_read_timeout(&e) => return Ok(true),
            Err(e) => return Err(e),
        }
        if reader.buffer().is_empty() {
            return Ok(true);
        }
    }
}

/// Handle a single VNC client connection.
fn handle_vnc_client(
    dev: &Arc<VncServer>,
    stream: UnixStream,
) -> io::Result<()> {
    // Set a read timeout to detect dead clients.
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    // Belt and braces only. This bounds a send on the platforms where
    // it works, and reports success and does nothing on illumos, which
    // is why `VncWriter` polls instead. A client is never refused over
    // it.
    if let Err(e) = stream.set_write_timeout(Some(VNC_WRITE_TIMEOUT)) {
        slog::warn!(dev.log, "VNC write timeout not set"; "error" => %e);
    }

    let read_stream = stream.try_clone()?;
    let mut writer = VncWriter::new(&stream, VNC_WRITE_TIMEOUT);
    let mut reader = BufReader::new(read_stream);
    let mut framebuffer_scratch = Vec::new();

    // --- RFB handshake ---

    // Version handshake
    let minor = rfb::handshake_version(&mut CombinedStream {
        reader: &mut reader,
        writer: &mut writer,
    })?;

    // Security handshake, as the version the client chose has it
    rfb::handshake_security(
        &mut CombinedStream {
            reader: &mut reader,
            writer: &mut writer,
        },
        dev.password.as_deref(),
        minor,
    )?;

    // ClientInit
    let _shared_flag = rfb::read_client_init(&mut reader)?;

    // ServerInit
    let (width, height) = dev.size();
    let pf = PixelFormat::xrgb8888();
    rfb::send_server_init(&mut writer, width, height, &pf, VNC_DESKTOP_NAME)?;
    let mut view = ClientView {
        width,
        height,
        desktop_size: false,
        extended_desktop_size: false,
        eds_due: None,
        use_zrle: false,
        zrle: None,
        shadow: None,
        repaint: false,
        cursor: false,
        cursor_sent: None,
        cursor_shown: false,
        frames: 0,
        overlay: None,
        overlay_corner: None,
    };

    // One thread serves both halves: it polls for client messages with a
    // short read timeout and sends a frame when one was asked for. An
    // incremental request waits here until the display has damage.
    let mut pending: Option<UpdateRequest> = None;
    stream.set_read_timeout(Some(CLIENT_POLL))?;
    let interval = frame_interval(dev.source.as_ref());
    let mut ticker = Ticker::new(Instant::now(), interval);
    // Withdrawn when the client goes, however the loop ends.
    let mut pace = ClientPace::new(Arc::clone(&dev.source), interval);
    let mut stats = vmm_devices::overlay::Stats::new(Instant::now());

    while !dev.shutdown.load(Ordering::Relaxed) {
        if !read_client_messages(dev, &mut view, &mut reader, &mut pending)? {
            // EOF: client disconnected
            break;
        }

        if let Some(request) = pending {
            let began = Instant::now();
            let frames = view.frames;
            match send_framebuffer_update(
                dev,
                &mut writer,
                &mut view,
                &mut framebuffer_scratch,
                request,
            ) {
                Ok(true) => pending = None,
                Ok(false) => {}
                Err(e) if is_expected_disconnect(&e) => break,
                Err(e) => return Err(e),
            }
            if view.frames != frames {
                let cost = began.elapsed();
                let interval = pace.frame(cost);
                probes::vnc_frame!(|| (micros(cost), micros(interval)));
                stats.frame(cost, true, interval);
            }
        }

        let now = Instant::now();
        stats.roll(now);
        show_overlay(
            &mut view,
            overlay_asked().then(|| stats.text().to_string()),
        );
        thread::sleep(ticker.sleep_after(now));
    }

    // Clean shutdown
    let _ = stream.shutdown(Shutdown::Both);

    Ok(())
}

/// Send the update `request` asks for, and report whether one was sent.
///
/// A full request sends the whole framebuffer. An incremental one sends
/// the display's damage, and nothing at all when there is none, so the
/// request stays waiting. Either way the damage is taken, so what a full
/// update sent is not sent again.
///
/// A guest mode change since ServerInit is announced first when the
/// client offered DesktopSize, and is followed by the whole framebuffer. A client that did not gets a rectangle
/// no larger than the framebuffer it was told about, because anything
/// larger is a protocol violation that most viewers drop.
fn send_framebuffer_update(
    dev: &Arc<VncServer>,
    writer: &mut VncWriter<'_>,
    view: &mut ClientView,
    scratch: &mut Vec<u8>,
    request: UpdateRequest,
) -> io::Result<bool> {
    let sent = write_framebuffer_update(dev, writer, view, scratch, request)?;
    // Pixels go out as they are written, and flush what came before
    // them. An update with none, a pointer or a size answer alone, would
    // otherwise wait in the buffer, and the client, its request answered,
    // would ask for nothing more.
    writer.flush()?;
    Ok(sent)
}

/// [`send_framebuffer_update`] short of the flush.
fn write_framebuffer_update(
    dev: &Arc<VncServer>,
    writer: &mut VncWriter<'_>,
    view: &mut ClientView,
    scratch: &mut Vec<u8>,
    request: UpdateRequest,
) -> io::Result<bool> {
    let (width, height) = dev.size();

    if width == 0 || height == 0 {
        return Ok(false);
    }

    let resized = (width, height) != (view.width, view.height);
    // A client told of a new size starts from an empty framebuffer. One
    // that was not keeps its picture, and the device's own damage covers
    // what the mode change redrew.
    let announced =
        resized && (view.desktop_size || view.extended_desktop_size);
    let eds_due = view.eds_due.take();
    // Whether this update has put anything on the wire yet.
    let mut sent = false;
    if announced {
        rfb::write_fb_update_header(writer, 1)?;
        if view.extended_desktop_size {
            // A pending answer rides on the change it asked for.
            let (reason, status) =
                eds_due.unwrap_or((EDS_REASON_SERVER, EDS_STATUS_OK));
            rfb::write_extended_desktop_size(
                writer, reason, status, width, height,
            )?;
        } else {
            writer.write_all(
                &RectHeader {
                    x: 0,
                    y: 0,
                    width,
                    height,
                    encoding: ENCODING_DESKTOP_SIZE,
                }
                .to_bytes(),
            )?;
        }
        view.width = width;
        view.height = height;
        sent = true;
    } else if let Some((reason, status)) = eds_due {
        rfb::write_fb_update_header(writer, 1)?;
        rfb::write_extended_desktop_size(
            writer,
            reason,
            status,
            view.width,
            view.height,
        )?;
        sent = true;
    }

    if view.cursor && send_cursor(dev, writer, view)? {
        sent = true;
    }

    // The rectangle may not exceed the client's framebuffer.
    let send_w = width.min(view.width);
    let send_h = height.min(view.height);
    if send_w == 0 || send_h == 0 {
        return Ok(sent);
    }
    let whole = Rect {
        x: 0,
        y: 0,
        width: u32::from(send_w),
        height: u32::from(send_h),
    };

    let damage = dev.source.take_damage();
    let whole_update =
        announced || request == UpdateRequest::Full || view.repaint;
    let area = if whole_update {
        view.overlay_corner = None;
        Some(whole)
    } else {
        // The overlay's corner, where it changed, goes too.
        let corner = view.overlay_corner.take();
        [damage, corner]
            .into_iter()
            .flatten()
            .reduce(Rect::union)
            .and_then(|d| clip(d, whole))
    };
    let Some(area) = area else {
        return Ok(sent);
    };

    let row_bytes = area.width as usize * BYTES_PER_PIXEL as usize;
    scratch.resize(row_bytes * area.height as usize, 0);
    match dev.source.read_rect(area, scratch) {
        Ok(()) => {}
        // The display shrank since `size()`, and the damage taken above
        // goes with this frame: the next one repaints all of it.
        Err(e) if e.kind() == io::ErrorKind::InvalidInput => {
            view.repaint = true;
            return Ok(sent);
        }
        Err(e) => return Err(e),
    }
    view.repaint = false;
    // Over the client's copy of the frame only; the guest's never has it.
    if let Some(text) = &view.overlay {
        vmm_devices::overlay::draw_in(scratch, area, text);
    }

    let shadow_len = usize::from(view.width)
        * usize::from(view.height)
        * BYTES_PER_PIXEL as usize;
    let rects = match &view.shadow {
        Some(shadow) if !whole_update => changed_tiles(
            Pixels {
                data: shadow,
                stride: u32::from(view.width),
                x: 0,
                y: 0,
            },
            Pixels {
                data: scratch,
                stride: area.width,
                x: area.x,
                y: area.y,
            },
            area,
        ),
        _ => vec![area],
    };
    if whole_update && view.shadow.as_ref().map(Vec::len) != Some(shadow_len) {
        view.shadow = Some(vec![0; shadow_len]);
    }
    if let Some(shadow) = view.shadow.as_mut() {
        store_area(shadow, view.width, scratch, area);
    }
    // More rectangles than RFB can count go as the one damaged area.
    let rects = match u16::try_from(rects.len()) {
        Ok(0) => return Ok(sent),
        Ok(_) => rects,
        Err(_) => vec![area],
    };

    rfb::write_fb_update_header(writer, rects.len() as u16)?;
    view.frames += 1;
    let mut pixels = Vec::new();
    let mut body = Vec::new();
    let mut zrle = view.zrle.as_mut().filter(|_| view.use_zrle);
    let encoding = if zrle.is_some() {
        ENCODING_ZRLE
    } else {
        ENCODING_RAW
    };
    for r in rects {
        // Every rectangle lies inside `area`, which `clip` kept inside
        // `whole`, whose edges are u16.
        let (w, h) = (r.width as u16, r.height as u16);
        writer.write_all(
            &RectHeader {
                x: r.x as u16,
                y: r.y as u16,
                width: w,
                height: h,
                encoding,
            }
            .to_bytes(),
        )?;
        let out = if r == area {
            &scratch[..]
        } else {
            copy_out_rect(&mut pixels, scratch, area, r);
            &pixels[..]
        };
        match zrle.as_mut() {
            Some(enc) => {
                body.clear();
                enc.encode(out, w, h, &mut body)?;
                writer.write_all_direct(&body)?;
            }
            None => writer.write_all_direct(out)?,
        }
    }

    Ok(true)
}

/// Send the guest's pointer to a client that draws it, if it changed
/// since the client was last sent it; whether anything was sent.
///
/// A pointer the guest hides is the empty cursor. So is the default,
/// once the client was sent an image: the guest then draws its own, or
/// none, and the client would go on drawing the old one.
fn send_cursor(
    dev: &Arc<VncServer>,
    writer: &mut VncWriter<'_>,
    view: &mut ClientView,
) -> io::Result<bool> {
    let serial = dev.source.cursor_serial();
    if view.cursor_sent == Some(serial) {
        return Ok(false);
    }
    view.cursor_sent = Some(serial);
    let shape = dev.source.cursor();
    let image = match &shape {
        CursorShape::Image(image) => {
            let size = |v: u32| u16::try_from(v).ok();
            match (
                size(image.hot_x),
                size(image.hot_y),
                size(image.width),
                size(image.height),
            ) {
                (Some(hx), Some(hy), Some(w), Some(h)) => {
                    Some((hx, hy, w, h, &image.pixels[..]))
                }
                // No device makes one this large; it is not sent.
                _ => return Ok(false),
            }
        }
        CursorShape::Hidden => None,
        CursorShape::Default if view.cursor_shown => None,
        CursorShape::Default => return Ok(false),
    };
    view.cursor_shown = image.is_some();
    rfb::write_fb_update_header(writer, 1)?;
    rfb::write_cursor(writer, image)?;
    Ok(true)
}

/// Copy `area`'s pixels, rows packed in `fresh`, into `shadow`, a
/// `stride`-pixel-wide frame.
fn store_area(shadow: &mut [u8], stride: u16, fresh: &[u8], area: Rect) {
    let bpp = BYTES_PER_PIXEL as usize;
    let row = area.width as usize * bpp;
    for y in 0..area.height as usize {
        let s = (area.y as usize + y) * usize::from(stride) * bpp
            + area.x as usize * bpp;
        shadow[s..s + row].copy_from_slice(&fresh[y * row..(y + 1) * row]);
    }
}

/// Gather `r`, which lies inside `area`, out of `fresh` (the area's
/// pixels, rows packed) into `out` as packed rows.
fn copy_out_rect(out: &mut Vec<u8>, fresh: &[u8], area: Rect, r: Rect) {
    let bpp = BYTES_PER_PIXEL as usize;
    let fresh_row = area.width as usize * bpp;
    let row = r.width as usize * bpp;
    out.clear();
    for y in r.y..r.y + r.height {
        let f =
            (y - area.y) as usize * fresh_row + (r.x - area.x) as usize * bpp;
        out.extend_from_slice(&fresh[f..f + row]);
    }
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

// ---------------------------------------------------------------------------
// Client socket writer
// ---------------------------------------------------------------------------

/// Buffered writer for a VNC client socket, with a bound on how long
/// one send waits.
///
/// `BufWriter` was here before. Its write to a full AF_UNIX socket has
/// no end on illumos: `SO_SNDTIMEO` reports success and bounds
/// nothing, and `O_NONBLOCK` is a file-status flag that the reader
/// clone of this socket shares. `write_all_bounded` polls for space,
/// which does work.
///
/// Only protocol headers go through the buffer. Pixel data is far too
/// large to copy again, so it goes to the socket from the caller's own
/// storage through [`VncWriter::write_all_direct`].
struct VncWriter<'a> {
    stream: &'a UnixStream,
    buf: Vec<u8>,
    budget: Duration,
}

impl<'a> VncWriter<'a> {
    fn new(stream: &'a UnixStream, budget: Duration) -> Self {
        Self {
            stream,
            buf: Vec::new(),
            budget,
        }
    }

    /// Send the buffer, then `data`, without copying `data` first.
    fn write_all_direct(&mut self, data: &[u8]) -> io::Result<()> {
        self.flush()?;
        write_all_bounded(self.stream, data, self.budget)
    }
}

impl Write for VncWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let result = write_all_bounded(self.stream, &self.buf, self.budget);
        // A send that gave up left part of the buffer on the socket, so
        // the bytes must not be sent again. The caller drops the client.
        self.buf.clear();
        result
    }
}

// ---------------------------------------------------------------------------
// CombinedStream helper for handshake
// ---------------------------------------------------------------------------

/// Wrapper that implements `Read + Write` by combining a reader and a
/// writer, needed for the handshake functions which want a single
/// stream that can both read and write.
struct CombinedStream<'a, R: Read, W: Write> {
    reader: &'a mut BufReader<R>,
    writer: &'a mut W,
}

impl<R: Read, W: Write> Read for CombinedStream<'_, R, W> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

impl<R: Read, W: Write> Write for CombinedStream<'_, R, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use vmm_devices::Lifecycle;

    use crate::fbuf::tests::{
        attach_server, has_server, make_test_dev, set_resolution,
        RecordingKeyboard,
    };
    use crate::fbuf::Framebuffer;

    /// A server for `fb` that is not listening.
    fn serve(fb: &Arc<Framebuffer>) -> Arc<VncServer> {
        serve_with_input(fb, Arc::new(InputBroker::default()))
    }

    fn serve_with_input(
        fb: &Arc<Framebuffer>,
        input: Arc<InputBroker>,
    ) -> Arc<VncServer> {
        Arc::new(VncServer::new(
            Arc::clone(fb) as Arc<dyn DisplaySource>,
            Path::new("/nonexistent/fbuf.vnc"),
            None,
            input,
            slog::Logger::root(slog::Discard, slog::o!()),
        ))
    }

    #[test]
    fn key_message_reaches_input_broker() {
        let input = Arc::new(InputBroker::default());
        let keyboard = Arc::new(RecordingKeyboard::default());
        input.set_keyboard(keyboard.clone());
        let dev = serve_with_input(&make_test_dev(), input);

        assert!(dispatch_client_message(
            &dev,
            &mut announced(1024, 768),
            ClientMessage::KeyEvent {
                down: true,
                key: 0xff0d,
            },
        )
        .is_none());
        assert_eq!(keyboard.events(), vec![(true, 0xff0d)]);
    }

    /// A client may send update requests far faster than frames go out,
    /// as macOS Screen Sharing does. Whatever has arrived is handled in
    /// one pass, so input behind the requests is not held for a frame
    /// interval per message.
    #[test]
    fn messages_already_received_are_handled_together() {
        let input = Arc::new(InputBroker::default());
        let keyboard = Arc::new(RecordingKeyboard::default());
        input.set_keyboard(keyboard.clone());
        let dev = serve_with_input(&make_test_dev(), input);

        let mut wire = Vec::new();
        for _ in 0..500 {
            // FramebufferUpdateRequest: incremental, 3440x1440 at 0,0
            wire.extend_from_slice(&[3, 1, 0, 0, 0, 0, 0x0d, 0x70, 0x05, 0xa0]);
        }
        // KeyEvent: Return down
        wire.extend_from_slice(&[4, 1, 0, 0, 0, 0, 0xff, 0x0d]);
        let mut reader = BufReader::new(io::Cursor::new(wire));
        let mut pending = None;

        let open = read_client_messages(
            &dev,
            &mut announced(1024, 768),
            &mut reader,
            &mut pending,
        )
        .expect("read");
        assert!(open);
        assert_eq!(keyboard.events(), vec![(true, 0xff0d)]);
        assert_eq!(pending, Some(UpdateRequest::Incremental));
    }

    /// Read what one full update put on the wire: the rectangle headers.
    fn sent_rects(
        dev: &Arc<VncServer>,
        view: &mut ClientView,
    ) -> Vec<RectHeader> {
        sent_rects_for(dev, view, UpdateRequest::Full)
    }

    /// Read what one update put on the wire: the rectangle headers.
    fn sent_rects_for(
        dev: &Arc<VncServer>,
        view: &mut ClientView,
        request: UpdateRequest,
    ) -> Vec<RectHeader> {
        sent_update(dev, view, request)
            .into_iter()
            .map(|(rect, _)| rect)
            .collect()
    }

    /// Read what one update put on the wire: each rectangle with its
    /// pixel data.
    fn sent_update(
        dev: &Arc<VncServer>,
        view: &mut ClientView,
        request: UpdateRequest,
    ) -> Vec<(RectHeader, Vec<u8>)> {
        use std::io::Read;

        let (mut peer, server) =
            UnixStream::pair().expect("create a VNC socket pair");
        // A frame is megabytes, far more than the socket buffer, so the
        // peer has to drain while the update is written.
        let drain = thread::spawn(move || {
            let mut wire = Vec::new();
            peer.read_to_end(&mut wire).expect("read the update");
            wire
        });

        let mut writer = VncWriter::new(&server, Duration::from_secs(5));
        let mut scratch = Vec::new();
        send_framebuffer_update(dev, &mut writer, view, &mut scratch, request)
            .expect("the peer reads everything");
        writer.flush().expect("flush the update");
        // A plain close can lose the last send on illumos before the
        // peer reads it.
        vmm_core::unixsock::close_after_write(server, Duration::from_secs(5));
        let wire = drain.join().expect("the draining thread");
        let mut rects = Vec::new();
        let mut at = 0;
        while at + 4 <= wire.len() {
            let count = u16::from_be_bytes([wire[at + 2], wire[at + 3]]);
            at += 4;
            for _ in 0..count {
                let r = &wire[at..at + 12];
                let rect = RectHeader {
                    x: u16::from_be_bytes([r[0], r[1]]),
                    y: u16::from_be_bytes([r[2], r[3]]),
                    width: u16::from_be_bytes([r[4], r[5]]),
                    height: u16::from_be_bytes([r[6], r[7]]),
                    encoding: i32::from_be_bytes([r[8], r[9], r[10], r[11]]),
                };
                at += 12;
                let mut pixels = Vec::new();
                if rect.encoding == ENCODING_ZRLE {
                    let len = u32::from_be_bytes(
                        wire[at..at + 4].try_into().expect("4 bytes"),
                    ) as usize;
                    pixels = wire[at..at + 4 + len].to_vec();
                    at += 4 + len;
                }
                if rect.encoding == ENCODING_EXTENDED_DESKTOP_SIZE {
                    let len = 4 + usize::from(wire[at]) * 16;
                    pixels = wire[at..at + len].to_vec();
                    at += len;
                }
                if rect.encoding == ENCODING_RAW {
                    let len = usize::from(rect.width)
                        * usize::from(rect.height)
                        * BYTES_PER_PIXEL as usize;
                    pixels = wire[at..at + len].to_vec();
                    at += len;
                }
                if rect.encoding == ENCODING_CURSOR {
                    // Pixels, then a mask of whole bytes a row.
                    let (w, h) =
                        (usize::from(rect.width), usize::from(rect.height));
                    let len =
                        w * h * BYTES_PER_PIXEL as usize + w.div_ceil(8) * h;
                    pixels = wire[at..at + len].to_vec();
                    at += len;
                }
                rects.push((rect, pixels));
            }
        }
        rects
    }

    #[test]
    fn a_client_without_desktop_size_never_gets_a_bigger_rectangle() {
        // ServerInit carried 1024x768. A rectangle larger than that is a
        // protocol violation and most viewers drop the connection.
        let fb = make_test_dev();
        set_resolution(&fb, 1920, 1080);
        let dev = serve(&fb);

        let mut view = announced(1024, 768);
        let rects = sent_rects(&dev, &mut view);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].encoding, ENCODING_RAW);
        assert_eq!((rects[0].width, rects[0].height), (1024, 768));
        assert_eq!((view.width, view.height), (1024, 768));
    }

    #[test]
    fn a_client_with_desktop_size_is_told_the_new_resolution() {
        let fb = make_test_dev();
        set_resolution(&fb, 800, 600);
        let dev = serve(&fb);

        let mut view = ClientView {
            width: 1024,
            height: 768,
            desktop_size: true,
            extended_desktop_size: false,
            eds_due: None,
            use_zrle: false,
            zrle: None,
            shadow: None,
            repaint: false,
            cursor: false,
            cursor_sent: None,
            cursor_shown: false,
            frames: 0,
            overlay: None,
            overlay_corner: None,
        };
        let rects = sent_rects(&dev, &mut view);
        assert_eq!(rects.len(), 2, "{rects:?}");
        assert_eq!(rects[0].encoding, ENCODING_DESKTOP_SIZE);
        assert_eq!((rects[0].width, rects[0].height), (800, 600));
        assert_eq!(rects[1].encoding, ENCODING_RAW);
        assert_eq!((rects[1].width, rects[1].height), (800, 600));
        assert_eq!((view.width, view.height), (800, 600));

        // The next update is a plain rectangle: the size is now known.
        let rects = sent_rects(&dev, &mut view);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].encoding, ENCODING_RAW);
    }

    #[test]
    fn set_encodings_records_whether_the_client_takes_desktop_size() {
        let dev = serve(&make_test_dev());
        let mut view = announced(1024, 768);
        dispatch_client_message(
            &dev,
            &mut view,
            ClientMessage::SetEncodings(vec![
                ENCODING_RAW,
                ENCODING_DESKTOP_SIZE,
            ]),
        );
        assert!(view.desktop_size);
        dispatch_client_message(
            &dev,
            &mut view,
            ClientMessage::SetEncodings(vec![ENCODING_RAW]),
        );
        assert!(!view.desktop_size);
    }

    /// Halt has to end the VNC thread, or it keeps streaming devmem to a
    /// connected client until the process exits.
    #[test]
    fn halt_stops_the_vnc_thread() {
        use std::time::Instant;

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("vnc.sock");
        let listener = vmm_core::unixsock::bind_restricted(
            &path,
            vmm_core::unixsock::SocketPolicy::default(),
        )
        .expect("bind the VNC socket");

        let dev = make_test_dev();
        let srv = Arc::new(VncServer::new(
            Arc::clone(&dev) as Arc<dyn DisplaySource>,
            &path,
            None,
            Arc::new(InputBroker::default()),
            slog::Logger::root(slog::Discard, slog::o!()),
        ));
        let server = Arc::clone(&srv);
        let handle = thread::Builder::new()
            .spawn(move || server_loop(server, listener))
            .expect("spawn the VNC server");
        *srv.thread.lock().expect("vnc: thread lock") = Some(handle);
        attach_server(&dev, Arc::clone(&srv));

        let started = Instant::now();
        Lifecycle::halt(dev.as_ref());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "halt waited {:?}",
            started.elapsed()
        );
        assert!(srv.shutdown.load(Ordering::Acquire));
        assert!(srv.thread.lock().expect("vnc: thread lock").is_none());
        assert!(
            !has_server(&dev),
            "halt drops the server, which holds the device"
        );
        assert!(!path.exists(), "the socket file is unlinked");
    }

    /// A VNC client that stops reading must not hold the VNC thread.
    ///
    /// The thread also runs the accept loop, so a stuck write costs
    /// every later viewer as well.
    #[test]
    fn a_client_that_never_reads_does_not_hold_the_vnc_thread() {
        use std::os::unix::net::UnixStream;
        use std::time::Instant;

        let dev = serve(&make_test_dev());
        let (_peer, server) =
            UnixStream::pair().expect("create a VNC socket pair");
        let mut writer = VncWriter::new(&server, Duration::from_millis(200));
        let mut scratch = Vec::new();

        let started = Instant::now();
        let error = send_framebuffer_update(
            &dev,
            &mut writer,
            &mut announced(1024, 768),
            &mut scratch,
            UpdateRequest::Full,
        )
        .expect_err("a client that reads nothing must not be waited on");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the update took {:?}",
            started.elapsed()
        );
    }

    /// A display of `size` whose damage the test sets.
    struct DamagedSource {
        size: (u32, u32),
        damage: Mutex<Option<Rect>>,
        /// XRGB8888, `size.0 * size.1` pixels, rows packed.
        pixels: Mutex<Vec<u8>>,
        /// Every size a client asked for.
        requests: Mutex<Vec<(u32, u32)>>,
    }

    impl DamagedSource {
        /// Set one pixel.
        fn paint(&self, x: u32, y: u32, value: u32) {
            let at = ((y * self.size.0 + x) * 4) as usize;
            self.pixels.lock().expect("pixels lock")[at..at + 4]
                .copy_from_slice(&value.to_le_bytes());
        }

        /// Report the whole display damaged, as a page flip does.
        fn damage_all(&self) {
            *self.damage.lock().expect("damage lock") =
                Some(rect(0, 0, self.size.0, self.size.1));
        }
    }

    impl DisplaySource for DamagedSource {
        fn size(&self) -> (u32, u32) {
            self.size
        }

        fn take_damage(&self) -> Option<Rect> {
            self.damage.lock().expect("damage lock").take()
        }

        fn read_rect(&self, r: Rect, dst: &mut [u8]) -> io::Result<()> {
            assert_eq!(dst.len(), (r.width * r.height * 4) as usize);
            let pixels = self.pixels.lock().expect("pixels lock");
            let row = (r.width * 4) as usize;
            for y in 0..r.height {
                let at = (((r.y + y) * self.size.0 + r.x) * 4) as usize;
                dst[y as usize * row..][..row]
                    .copy_from_slice(&pixels[at..at + row]);
            }
            Ok(())
        }

        fn request_size(&self, width: u32, height: u32) -> bool {
            self.requests
                .lock()
                .expect("requests lock")
                .push((width, height));
            true
        }
    }

    fn damaged(
        width: u32,
        height: u32,
        damage: Option<Rect>,
    ) -> (Arc<DamagedSource>, Arc<VncServer>) {
        let src = Arc::new(DamagedSource {
            size: (width, height),
            damage: Mutex::new(damage),
            pixels: Mutex::new(vec![0; (width * height * 4) as usize]),
            requests: Mutex::default(),
        });
        let srv = Arc::new(VncServer::new(
            Arc::clone(&src) as Arc<dyn DisplaySource>,
            Path::new("/nonexistent/damage.vnc"),
            None,
            Arc::new(InputBroker::default()),
            slog::Logger::root(slog::Discard, slog::o!()),
        ));
        (src, srv)
    }

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn raw(x: u16, y: u16, width: u16, height: u16) -> RectHeader {
        RectHeader {
            x,
            y,
            width,
            height,
            encoding: ENCODING_RAW,
        }
    }

    #[test]
    fn update_requests_say_whether_they_are_incremental() {
        let dev = serve(&make_test_dev());
        let mut view = announced(1024, 768);
        for (incremental, want) in [
            (true, UpdateRequest::Incremental),
            (false, UpdateRequest::Full),
        ] {
            let got = dispatch_client_message(
                &dev,
                &mut view,
                ClientMessage::FbUpdateRequest {
                    incremental,
                    x: 0,
                    y: 0,
                    width: 1024,
                    height: 768,
                },
            );
            assert_eq!(got, Some(want));
        }
    }

    /// A full request after an incremental one must not be downgraded
    /// while both wait for the next send.
    #[test]
    fn a_full_request_outranks_an_incremental_one() {
        use UpdateRequest::*;
        assert_eq!(Incremental.merge(Some(Full)), Full);
        assert_eq!(Full.merge(Some(Incremental)), Full);
        assert_eq!(Incremental.merge(None), Incremental);
    }

    /// A 256x192 display the client has already been sent in full.
    fn sent_in_full() -> (Arc<DamagedSource>, Arc<VncServer>, ClientView) {
        let (src, dev) = damaged(256, 192, None);
        let mut view = announced(256, 192);
        assert_eq!(
            sent_rects_for(&dev, &mut view, UpdateRequest::Full),
            [raw(0, 0, 256, 192)]
        );
        (src, dev, view)
    }

    /// A page-flipping guest damages the whole display every frame. What
    /// the client already has must not be sent again.
    #[test]
    fn an_unchanged_frame_is_not_sent_again() {
        let (src, dev, mut view) = sent_in_full();
        src.damage_all();
        assert!(sent_rects_for(&dev, &mut view, UpdateRequest::Incremental)
            .is_empty());
    }

    #[test]
    fn only_the_tile_that_changed_is_sent() {
        let (src, dev, mut view) = sent_in_full();
        src.paint(100, 70, 0x00AB_CDEF);
        src.damage_all();

        let update = sent_update(&dev, &mut view, UpdateRequest::Incremental);
        assert_eq!(update.len(), 1);
        let (rect, pixels) = &update[0];
        assert_eq!(*rect, raw(64, 64, 64, 64));
        // (100, 70) is (36, 6) within the tile.
        let at = (6 * 64 + 36) * 4;
        assert_eq!(pixels[at..at + 4], 0x00AB_CDEFu32.to_le_bytes());
    }

    #[test]
    fn a_change_sent_once_is_not_sent_again() {
        let (src, dev, mut view) = sent_in_full();
        src.paint(100, 70, 1);
        src.damage_all();
        assert_eq!(
            sent_rects_for(&dev, &mut view, UpdateRequest::Incremental).len(),
            1
        );
        src.damage_all();
        assert!(sent_rects_for(&dev, &mut view, UpdateRequest::Incremental)
            .is_empty());
    }

    #[test]
    fn adjacent_changed_tiles_in_a_row_go_as_one_rect() {
        let (src, dev, mut view) = sent_in_full();
        src.paint(10, 10, 1);
        src.paint(70, 10, 1);
        src.damage_all();
        assert_eq!(
            sent_rects_for(&dev, &mut view, UpdateRequest::Incremental),
            [raw(0, 0, 128, 64)]
        );
    }

    #[test]
    fn changed_tiles_in_different_rows_go_as_separate_rects() {
        let (src, dev, mut view) = sent_in_full();
        src.paint(10, 10, 1);
        src.paint(200, 100, 1);
        src.damage_all();
        assert_eq!(
            sent_rects_for(&dev, &mut view, UpdateRequest::Incremental),
            [raw(0, 0, 64, 64), raw(192, 64, 64, 64)]
        );
    }

    #[test]
    fn tiles_at_the_edge_are_cut_to_the_display() {
        let (src, dev) = damaged(100, 70, None);
        let mut view = announced(100, 70);
        sent_rects_for(&dev, &mut view, UpdateRequest::Full);
        src.paint(90, 68, 1);
        src.damage_all();
        assert_eq!(
            sent_rects_for(&dev, &mut view, UpdateRequest::Incremental),
            [raw(64, 64, 36, 6)]
        );
    }

    /// Only the damaged area is compared: a change the device did not
    /// report waits until it does.
    #[test]
    fn only_the_damaged_area_is_compared() {
        let (src, dev, mut view) = sent_in_full();
        src.paint(10, 10, 1);
        src.paint(200, 100, 1);
        *src.damage.lock().expect("damage lock") = Some(rect(0, 0, 64, 64));
        assert_eq!(
            sent_rects_for(&dev, &mut view, UpdateRequest::Incremental),
            [raw(0, 0, 64, 64)]
        );
    }

    #[test]
    fn a_full_request_sends_everything_even_when_unchanged() {
        let (src, dev, mut view) = sent_in_full();
        src.damage_all();
        assert_eq!(
            sent_rects_for(&dev, &mut view, UpdateRequest::Full),
            [raw(0, 0, 256, 192)]
        );
    }

    /// After a DesktopSize the client's framebuffer is new, so the shadow
    /// has to be too.
    #[test]
    fn a_resize_starts_a_new_shadow() {
        let (src, dev) = damaged(256, 192, None);
        let mut view = ClientView {
            desktop_size: true,
            ..announced(128, 96)
        };
        let rects = sent_rects_for(&dev, &mut view, UpdateRequest::Incremental);
        assert_eq!(rects.len(), 2, "DesktopSize and the whole frame");
        assert_eq!(rects[1], raw(0, 0, 256, 192));

        src.paint(250, 190, 1);
        src.damage_all();
        assert_eq!(
            sent_rects_for(&dev, &mut view, UpdateRequest::Incremental),
            [raw(192, 128, 64, 64)]
        );
    }

    #[test]
    fn an_incremental_request_sends_only_the_damage() {
        let (_src, dev) = damaged(64, 48, Some(rect(8, 4, 16, 8)));
        let rects = sent_rects_for(
            &dev,
            &mut announced(64, 48),
            UpdateRequest::Incremental,
        );
        assert_eq!(rects, [raw(8, 4, 16, 8)]);
    }

    #[test]
    fn an_incremental_request_without_damage_sends_nothing() {
        let (_src, dev) = damaged(64, 48, None);
        let rects = sent_rects_for(
            &dev,
            &mut announced(64, 48),
            UpdateRequest::Incremental,
        );
        assert!(rects.is_empty(), "{rects:?}");
    }

    #[test]
    fn a_full_request_sends_the_frame_and_consumes_the_damage() {
        let (src, dev) = damaged(64, 48, Some(rect(8, 4, 16, 8)));
        let mut view = announced(64, 48);
        let rects = sent_rects_for(&dev, &mut view, UpdateRequest::Full);
        assert_eq!(rects, [raw(0, 0, 64, 48)]);
        assert!(src.damage.lock().expect("damage lock").is_none());
    }

    /// Damage is in display coordinates, and the client may have been
    /// told a smaller framebuffer.
    #[test]
    fn damage_is_clipped_to_the_client_framebuffer() {
        let (_src, dev) = damaged(64, 48, Some(rect(20, 20, 30, 10)));
        let rects = sent_rects_for(
            &dev,
            &mut announced(32, 24),
            UpdateRequest::Incremental,
        );
        assert_eq!(rects, [raw(20, 20, 12, 4)]);
    }

    fn eds(reason: u16, status: u16, width: u16, height: u16) -> RectHeader {
        RectHeader {
            x: reason,
            y: status,
            width,
            height,
            encoding: ENCODING_EXTENDED_DESKTOP_SIZE,
        }
    }

    fn list_encodings(dev: &VncServer, view: &mut ClientView, encs: &[i32]) {
        assert!(dispatch_client_message(
            dev,
            view,
            ClientMessage::SetEncodings(encs.to_vec())
        )
        .is_none());
    }

    fn zrle(x: u16, y: u16, width: u16, height: u16) -> RectHeader {
        RectHeader {
            encoding: ENCODING_ZRLE,
            ..raw(x, y, width, height)
        }
    }

    /// A client that lists ZRLE gets its pixels compressed, through one
    /// zlib stream for the connection.
    #[test]
    fn a_client_that_lists_zrle_gets_zrle() {
        let (src, dev) = damaged(256, 192, None);
        for y in 0..192 {
            src.paint(y, y, 0x00FF_8040 + y);
        }
        let mut view = announced(256, 192);
        list_encodings(&dev, &mut view, &[ENCODING_ZRLE, ENCODING_RAW]);
        let mut decoder = zrle::tests::Decoder::new();

        let update = sent_update(&dev, &mut view, UpdateRequest::Full);
        assert_eq!(update.len(), 1);
        assert_eq!(update[0].0, zrle(0, 0, 256, 192));
        let (frame, _) = decoder.decode(&update[0].1, 256, 192);
        let want =
            zrle::tests::without_x(&src.pixels.lock().expect("pixels lock"));
        assert_eq!(frame, want);

        // The next rectangle continues the same stream.
        src.paint(100, 70, 0x0012_3456);
        src.damage_all();
        let update = sent_update(&dev, &mut view, UpdateRequest::Incremental);
        assert_eq!(update[0].0, zrle(64, 64, 64, 64));
        let (tile, _) = decoder.decode(&update[0].1, 64, 64);
        let at = (6 * 64 + 36) * 4;
        assert_eq!(tile[at..at + 4], [0x56, 0x34, 0x12, 0]);
    }

    #[test]
    fn a_client_that_does_not_list_zrle_gets_raw() {
        let (_src, dev) = damaged(64, 64, None);
        let mut view = announced(64, 64);
        list_encodings(&dev, &mut view, &[ENCODING_RAW]);
        assert_eq!(
            sent_rects_for(&dev, &mut view, UpdateRequest::Full),
            [raw(0, 0, 64, 64)]
        );
    }

    /// A client learns the server takes SetDesktopSize from an
    /// ExtendedDesktopSize rectangle; until it sees one, it will not
    /// send it.
    #[test]
    fn listing_extended_desktop_size_is_answered_with_the_current_size() {
        let (_src, dev) = damaged(256, 192, None);
        let mut view = announced(256, 192);
        list_encodings(
            &dev,
            &mut view,
            &[ENCODING_RAW, ENCODING_EXTENDED_DESKTOP_SIZE],
        );
        let rects = sent_rects_for(&dev, &mut view, UpdateRequest::Full);
        assert_eq!(
            rects,
            [
                eds(EDS_REASON_SERVER, EDS_STATUS_OK, 256, 192),
                raw(0, 0, 256, 192)
            ]
        );
        // Said once.
        let rects = sent_rects_for(&dev, &mut view, UpdateRequest::Full);
        assert_eq!(rects, [raw(0, 0, 256, 192)]);
    }

    /// The request goes to the device, and the client hears back on the
    /// next update even when nothing on the screen changed. The new size
    /// follows once the guest has switched to it.
    #[test]
    fn set_desktop_size_is_passed_on_and_answered() {
        let (src, dev) = damaged(256, 192, None);
        let mut view = announced(256, 192);
        list_encodings(
            &dev,
            &mut view,
            &[ENCODING_RAW, ENCODING_EXTENDED_DESKTOP_SIZE],
        );
        sent_rects_for(&dev, &mut view, UpdateRequest::Full);

        assert!(dispatch_client_message(
            &dev,
            &mut view,
            ClientMessage::SetDesktopSize {
                width: 1600,
                height: 900
            }
        )
        .is_none());
        assert_eq!(*src.requests.lock().expect("requests lock"), [(1600, 900)]);
        let rects = sent_rects_for(&dev, &mut view, UpdateRequest::Incremental);
        assert_eq!(rects, [eds(EDS_REASON_CLIENT, EDS_STATUS_OK, 256, 192)]);
    }

    #[test]
    fn a_device_that_cannot_resize_answers_prohibited() {
        let dev = serve(&make_test_dev());
        let mut view = announced(1024, 768);
        list_encodings(
            &dev,
            &mut view,
            &[ENCODING_RAW, ENCODING_EXTENDED_DESKTOP_SIZE],
        );
        sent_rects_for(&dev, &mut view, UpdateRequest::Full);
        dispatch_client_message(
            &dev,
            &mut view,
            ClientMessage::SetDesktopSize {
                width: 800,
                height: 600,
            },
        );
        let rects = sent_rects_for(&dev, &mut view, UpdateRequest::Full);
        assert_eq!(
            rects[0],
            eds(EDS_REASON_CLIENT, EDS_STATUS_PROHIBITED, 1024, 768)
        );
    }

    /// A client that speaks ExtendedDesktopSize hears of a guest mode
    /// change through it rather than through DesktopSize.
    #[test]
    fn a_mode_change_is_announced_with_extended_desktop_size() {
        let fb = make_test_dev();
        let dev = serve(&fb);
        let mut view = ClientView {
            desktop_size: true,
            ..announced(1024, 768)
        };
        list_encodings(
            &dev,
            &mut view,
            &[
                ENCODING_RAW,
                ENCODING_DESKTOP_SIZE,
                ENCODING_EXTENDED_DESKTOP_SIZE,
            ],
        );
        sent_rects_for(&dev, &mut view, UpdateRequest::Full);

        set_resolution(&fb, 800, 600);
        let rects = sent_rects_for(&dev, &mut view, UpdateRequest::Incremental);
        assert_eq!(
            rects,
            [
                eds(EDS_REASON_SERVER, EDS_STATUS_OK, 800, 600),
                raw(0, 0, 800, 600)
            ]
        );
        assert_eq!((view.width, view.height), (800, 600));
    }

    /// A display that says it is 8 rows taller than its pixels, as one
    /// does when the guest shrinks it between the size and the read.
    struct Grown(Arc<Framebuffer>);

    impl DisplaySource for Grown {
        fn size(&self) -> (u32, u32) {
            let (w, h) = self.0.size();
            (w, h + 8)
        }

        fn take_damage(&self) -> Option<Rect> {
            self.0.take_damage()
        }

        fn read_rect(&self, r: Rect, dst: &mut [u8]) -> io::Result<()> {
            self.0.read_rect(r, dst)
        }
    }

    /// A guest that shrinks its display between the size and the read
    /// costs the client this frame, not its connection: the next frame
    /// repaints the whole display.
    #[test]
    fn a_resize_during_a_frame_skips_the_frame() {
        let fb = make_test_dev();
        let (w, h) = fb.size();
        let dev = Arc::new(VncServer::new(
            Arc::new(Grown(Arc::clone(&fb))) as Arc<dyn DisplaySource>,
            Path::new("/nonexistent/fbuf.vnc"),
            None,
            Arc::new(InputBroker::default()),
            slog::Logger::root(slog::Discard, slog::o!()),
        ));
        let mut view = announced(w as u16, h as u16 + 8);
        let rects = sent_rects(&dev, &mut view);
        assert!(rects.is_empty(), "{rects:?}");
    }

    /// A display that says it is 8 rows taller than its pixels once,
    /// and whose resize damaged it once, as virtio-gpu reports damage:
    /// fbuf's whole-frame damage on every take would hide a lost one.
    struct GrownOnce(Arc<Framebuffer>, AtomicBool, AtomicBool);

    impl DisplaySource for GrownOnce {
        fn size(&self) -> (u32, u32) {
            let (w, h) = self.0.size();
            let grown = self.1.swap(false, Ordering::SeqCst);
            (w, if grown { h + 8 } else { h })
        }

        fn take_damage(&self) -> Option<Rect> {
            let damaged = self.2.swap(false, Ordering::SeqCst);
            self.0.take_damage().filter(|_| damaged)
        }

        fn read_rect(&self, r: Rect, dst: &mut [u8]) -> io::Result<()> {
            self.0.read_rect(r, dst)
        }
    }

    /// The skipped frame's damage was taken with it, so the next update
    /// sends the whole display, damaged since or not: a client with no
    /// DesktopSize would otherwise keep what it had.
    #[test]
    fn the_update_after_a_skipped_frame_is_whole() {
        let fb = make_test_dev();
        let (w, h) = fb.size();
        let dev = Arc::new(VncServer::new(
            Arc::new(GrownOnce(
                Arc::clone(&fb),
                AtomicBool::new(true),
                AtomicBool::new(true),
            )) as Arc<dyn DisplaySource>,
            Path::new("/nonexistent/fbuf.vnc"),
            None,
            Arc::new(InputBroker::default()),
            slog::Logger::root(slog::Discard, slog::o!()),
        ));
        let mut view = announced(w as u16, h as u16 + 8);
        let skipped = sent_rects_for(&dev, &mut view, UpdateRequest::Full);
        assert!(skipped.is_empty(), "{skipped:?}");
        let next = sent_rects_for(&dev, &mut view, UpdateRequest::Incremental);
        assert_eq!(next.len(), 1, "{next:?}");
        assert_eq!((next[0].width, next[0].height), (w as u16, h as u16));
    }

    /// A display with a pointer the test sets, and damage only when the
    /// test says.
    struct Pointed {
        fb: Arc<Framebuffer>,
        cursor: Mutex<(u64, CursorShape)>,
        damaged: AtomicBool,
    }

    impl DisplaySource for Pointed {
        fn size(&self) -> (u32, u32) {
            self.fb.size()
        }

        fn take_damage(&self) -> Option<Rect> {
            let damaged = self.damaged.swap(false, Ordering::SeqCst);
            self.fb.take_damage().filter(|_| damaged)
        }

        fn read_rect(&self, r: Rect, dst: &mut [u8]) -> io::Result<()> {
            self.fb.read_rect(r, dst)
        }

        fn cursor_serial(&self) -> u64 {
            self.cursor.lock().unwrap().0
        }

        fn cursor(&self) -> CursorShape {
            self.cursor.lock().unwrap().1.clone()
        }
    }

    fn pointed() -> (Arc<Pointed>, Arc<VncServer>) {
        let source = Arc::new(Pointed {
            fb: make_test_dev(),
            cursor: Mutex::new((0, CursorShape::Default)),
            damaged: AtomicBool::new(false),
        });
        let dev = Arc::new(VncServer::new(
            Arc::clone(&source) as Arc<dyn DisplaySource>,
            Path::new("/nonexistent/fbuf.vnc"),
            None,
            Arc::new(InputBroker::default()),
            slog::Logger::root(slog::Discard, slog::o!()),
        ));
        (source, dev)
    }

    /// A 2x2 pointer with its hotspot at (1, 0): opaque red, clear,
    /// clear, opaque blue.
    fn arrow() -> CursorShape {
        CursorShape::Image(Arc::new(vmm_devices::display::CursorImage {
            width: 2,
            height: 2,
            hot_x: 1,
            hot_y: 0,
            pixels: vec![
                0, 0, 255, 255, 0, 0, 0, 0, //
                0, 0, 0, 0, 255, 0, 0, 255,
            ],
        }))
    }

    /// A client that draws the pointer gets the guest's image once, in
    /// the server's pixel format, with its hotspot and a mask of the
    /// opaque pixels; nothing more until the pointer changes.
    #[test]
    fn a_new_pointer_goes_to_a_client_that_draws_it() {
        let (source, dev) = pointed();
        let (w, h) = source.size();
        let mut view = announced(w as u16, h as u16);
        view.cursor = true;
        view.shadow = Some(vec![0; w as usize * h as usize * 4]);
        *source.cursor.lock().unwrap() = (1, arrow());

        let sent = sent_update(&dev, &mut view, UpdateRequest::Incremental);
        assert_eq!(sent.len(), 1, "{sent:?}");
        let (rect, data) = &sent[0];
        assert_eq!(
            (rect.x, rect.y, rect.width, rect.height, rect.encoding),
            (1, 0, 2, 2, ENCODING_CURSOR)
        );
        assert_eq!(
            data,
            &[
                0,
                0,
                255,
                0,
                0,
                0,
                0,
                0, //
                0,
                0,
                0,
                0,
                255,
                0,
                0,
                0, //
                0b1000_0000,
                0b0100_0000,
            ]
        );
        let again = sent_update(&dev, &mut view, UpdateRequest::Incremental);
        assert!(again.is_empty(), "{again:?}");
    }

    /// A hidden pointer is an empty cursor; so is a pointer the guest
    /// takes back after it gave one, which the client would otherwise go
    /// on drawing. A client that never had one is sent nothing.
    #[test]
    fn a_hidden_or_taken_back_pointer_is_an_empty_cursor() {
        use CursorShape;

        let (source, dev) = pointed();
        let (w, h) = source.size();
        let mut view = announced(w as u16, h as u16);
        view.cursor = true;
        view.shadow = Some(vec![0; w as usize * h as usize * 4]);
        let empty = (0, 0, 0, 0, ENCODING_CURSOR);
        let headers = |view: &mut ClientView| {
            sent_update(&dev, view, UpdateRequest::Incremental)
                .into_iter()
                .map(|(r, _)| (r.x, r.y, r.width, r.height, r.encoding))
                .collect::<Vec<_>>()
        };

        assert!(headers(&mut view).is_empty(), "the default, never sent");
        *source.cursor.lock().unwrap() = (1, CursorShape::Hidden);
        assert_eq!(headers(&mut view), [empty]);
        *source.cursor.lock().unwrap() = (2, arrow());
        assert_eq!(headers(&mut view)[0].4, ENCODING_CURSOR);
        *source.cursor.lock().unwrap() = (3, CursorShape::Default);
        assert_eq!(headers(&mut view), [empty]);
    }

    /// An update with no pixels in it, a pointer alone, reaches the
    /// client when it is sent: the request it answers is done, and the
    /// client asks for no more until it has it.
    #[test]
    fn an_update_without_pixels_is_not_left_in_the_buffer() {
        let (source, dev) = pointed();
        let (w, h) = source.size();
        let mut view = announced(w as u16, h as u16);
        view.cursor = true;
        view.shadow = Some(vec![0; w as usize * h as usize * 4]);
        *source.cursor.lock().unwrap() = (1, arrow());
        let (_peer, server) =
            UnixStream::pair().expect("create a VNC socket pair");
        let mut writer = VncWriter::new(&server, Duration::from_secs(5));
        let mut scratch = Vec::new();
        let sent = send_framebuffer_update(
            &dev,
            &mut writer,
            &mut view,
            &mut scratch,
            UpdateRequest::Incremental,
        )
        .expect("sent");
        assert!(sent);
        assert!(
            writer.buf.is_empty(),
            "{} bytes held back",
            writer.buf.len()
        );
    }

    /// A client that did not list the Cursor pseudo-encoding keeps its
    /// own pointer, and is sent no cursor.
    #[test]
    fn a_client_without_the_cursor_encoding_is_sent_no_pointer() {
        let (source, dev) = pointed();
        let (w, h) = source.size();
        let mut view = announced(w as u16, h as u16);
        view.shadow = Some(vec![0; w as usize * h as usize * 4]);
        *source.cursor.lock().unwrap() = (1, arrow());
        assert!(
            sent_update(&dev, &mut view, UpdateRequest::Incremental).is_empty()
        );
    }

    /// Counts the warnings logged to it.
    struct Warnings(Arc<std::sync::atomic::AtomicUsize>);

    impl slog::Drain for Warnings {
        type Ok = ();
        type Err = slog::Never;

        fn log(
            &self,
            record: &slog::Record<'_>,
            _values: &slog::OwnedKVList,
        ) -> Result<(), slog::Never> {
            if record.level() == slog::Level::Warning {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }
    }

    /// A client that lists ZRLE without Raw is served ZRLE and needs no
    /// warning: macOS Screen Sharing lists no Raw. One that lists neither
    /// gets Raw it did not ask for, and that is worth a line.
    #[test]
    fn only_a_client_without_raw_or_zrle_is_warned_about() {
        let warnings = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dev = Arc::new(VncServer::new(
            make_test_dev() as Arc<dyn DisplaySource>,
            Path::new("/nonexistent/fbuf.vnc"),
            None,
            Arc::new(InputBroker::default()),
            slog::Logger::root(Warnings(Arc::clone(&warnings)), slog::o!()),
        ));
        let mut view = announced(8, 8);
        dispatch_client_message(
            &dev,
            &mut view,
            ClientMessage::SetEncodings(vec![ENCODING_ZRLE, ENCODING_CURSOR]),
        );
        assert_eq!(warnings.load(Ordering::SeqCst), 0, "ZRLE alone");
        dispatch_client_message(
            &dev,
            &mut view,
            ClientMessage::SetEncodings(vec![ENCODING_CURSOR]),
        );
        assert_eq!(warnings.load(Ordering::SeqCst), 1, "neither");
    }

    /// SetEncodings with Cursor makes the client one that draws it.
    #[test]
    fn set_encodings_with_cursor_turns_it_on() {
        let dev = serve(&make_test_dev());
        let mut view = announced(8, 8);
        dispatch_client_message(
            &dev,
            &mut view,
            ClientMessage::SetEncodings(vec![ENCODING_RAW, ENCODING_CURSOR]),
        );
        assert!(view.cursor);
        dispatch_client_message(
            &dev,
            &mut view,
            ClientMessage::SetEncodings(vec![ENCODING_RAW]),
        );
        assert!(!view.cursor);
    }

    /// A pass that ends early sleeps out the rest of its frame, so frames
    /// start on the frame interval whatever each pass cost; one that
    /// overran starts the next at once, without trying to catch up.
    #[test]
    fn the_ticker_keeps_the_frame_interval() {
        let start = Instant::now();
        let ms = Duration::from_millis;
        let mut ticker = Ticker::new(start, ms(33));
        assert_eq!(ticker.sleep_after(start + ms(10)), ms(23));
        // The next pass started at 33 ms.
        assert_eq!(ticker.sleep_after(start + ms(40)), ms(26));
        // A pass that ran to 120 ms is late: no sleep, and the one
        // after counts from now.
        assert_eq!(ticker.sleep_after(start + ms(120)), Duration::ZERO);
        assert_eq!(ticker.sleep_after(start + ms(130)), ms(23));
    }

    /// A display that records what it was asked to pace to.
    #[derive(Default)]
    struct PaceRecorder(Mutex<Vec<Option<Duration>>>);

    impl DisplaySource for PaceRecorder {
        fn size(&self) -> (u32, u32) {
            (0, 0)
        }

        fn take_damage(&self) -> Option<Rect> {
            None
        }

        fn read_rect(&self, _r: Rect, _dst: &mut [u8]) -> io::Result<()> {
            Ok(())
        }

        fn takes_pace(&self) -> bool {
            true
        }

        fn pace(&self, interval: Option<Duration>) {
            self.0.lock().unwrap().push(interval);
        }
    }

    /// A display the guest can be paced on is served at 60 frames a
    /// second, so a client that keeps up may have the guest flip that
    /// fast; one that cannot, fbuf, at 30, whatever it is asked.
    #[test]
    fn the_frame_interval_follows_whether_the_display_takes_a_pace() {
        assert_eq!(
            frame_interval(&PaceRecorder::default()),
            FAST_FRAME_INTERVAL
        );
        assert_eq!(
            frame_interval(make_test_dev().as_ref()),
            VNC_FRAME_INTERVAL
        );
    }

    /// A client's frames pace the guest to the rate they go out at, no
    /// faster than VNC's own; the request goes with the client.
    #[test]
    fn a_client_paces_the_guest_while_it_is_connected() {
        let source = Arc::new(PaceRecorder::default());
        let ms = Duration::from_millis;
        {
            let mut pace =
                ClientPace::new(Arc::clone(&source) as _, FAST_FRAME_INTERVAL);
            pace.frame(ms(1));
            pace.frame(ms(90));
        }
        let asked = source.0.lock().unwrap().clone();
        assert_eq!(asked[0], Some(FAST_FRAME_INTERVAL), "no faster than VNC");
        assert!(
            asked[1].is_some_and(|i| i > FAST_FRAME_INTERVAL),
            "{asked:?}"
        );
        assert_eq!(asked.last(), Some(&None), "withdrawn: {asked:?}");
    }

    /// The overlay goes over the corner with the next update, is not
    /// sent again while its text stays, and when it goes the corner is
    /// sent again with the display's own pixels.
    #[test]
    fn the_overlay_is_drawn_over_the_frames_while_it_is_shown() {
        let (source, dev) = pointed();
        let (w, h) = source.size();
        let mut view = announced(w as u16, h as u16);
        view.shadow = Some(vec![0x55; w as usize * h as usize * 4]);
        let corner = vmm_devices::overlay::area("60 FPS");
        let shown = |view: &ClientView| {
            let at = ((corner.y + 1) as usize * w as usize
                + (corner.x + 1) as usize)
                * 4;
            view.shadow.as_ref().expect("a shadow")[at..at + 4].to_vec()
        };

        show_overlay(&mut view, Some("60 FPS".to_string()));
        let sent = sent_update(&dev, &mut view, UpdateRequest::Incremental);
        assert!(!sent.is_empty(), "the corner goes with the next update");
        assert_eq!(shown(&view), [0, 0, 0, 0xFF], "the box, black");

        show_overlay(&mut view, Some("60 FPS".to_string()));
        let again = sent_update(&dev, &mut view, UpdateRequest::Incremental);
        assert!(again.is_empty(), "{again:?}");

        show_overlay(&mut view, None);
        let sent = sent_update(&dev, &mut view, UpdateRequest::Incremental);
        assert!(!sent.is_empty(), "the corner goes back");
        let mut own = vec![0; 4];
        source
            .read_rect(
                Rect {
                    x: corner.x + 1,
                    y: corner.y + 1,
                    width: 1,
                    height: 1,
                },
                &mut own,
            )
            .expect("read");
        assert_eq!(shown(&view), own, "the display's own pixel");
    }

    /// Run the RFB 3.8 handshake as a client of a server with no
    /// password, up to the ServerInit; the framebuffer width it gives.
    fn handshake(client: &mut UnixStream) -> io::Result<u16> {
        use std::io::Read;

        let mut version = [0u8; 12];
        client.read_exact(&mut version)?;
        client.write_all(b"RFB 003.008\n")?;
        let mut types = [0u8; 2];
        client.read_exact(&mut types)?;
        client.write_all(&[rfb::SECURITY_NONE])?;
        let mut result = [0u8; 4];
        client.read_exact(&mut result)?;
        client.write_all(&[1])?;
        let mut init = [0u8; 24];
        client.read_exact(&mut init)?;
        let name_len =
            u32::from_be_bytes(init[20..24].try_into().expect("4 bytes"));
        let mut name = vec![0u8; name_len as usize];
        client.read_exact(&mut name)?;
        Ok(u16::from_be_bytes([init[0], init[1]]))
    }

    /// A client that connects while another is served takes over: the
    /// one before it is dropped, so a viewer whose connection went dead
    /// does not hold VNC for everyone until its timeout.
    #[test]
    fn the_newest_client_wins() {
        use std::io::Read;

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("vnc.sock");
        let fb = make_test_dev();
        let srv = VncServer::start(
            "vnc-newest",
            Arc::clone(&fb) as Arc<dyn DisplaySource>,
            &path,
            None,
            Arc::new(InputBroker::default()),
            slog::Logger::root(slog::Discard, slog::o!()),
        )
        .expect("start");
        let timeout = Some(Duration::from_secs(5));

        let mut first = UnixStream::connect(&path).expect("connect");
        first.set_read_timeout(timeout).expect("timeout");
        assert!(handshake(&mut first).is_ok(), "the first is served");

        let mut second = UnixStream::connect(&path).expect("connect");
        second.set_read_timeout(timeout).expect("timeout");
        assert!(handshake(&mut second).is_ok(), "the newest is served");

        // The first is closed: whatever it was sent, then the end.
        let mut rest = Vec::new();
        let ended = first.read_to_end(&mut rest);
        assert!(ended.is_ok(), "the first was dropped: {ended:?}");
        srv.shutdown();
    }

    /// A client that was told `width` x `height` and offered no
    /// DesktopSize.
    fn announced(width: u16, height: u16) -> ClientView {
        ClientView {
            width,
            height,
            desktop_size: false,
            extended_desktop_size: false,
            eds_due: None,
            use_zrle: false,
            zrle: None,
            shadow: None,
            repaint: false,
            cursor: false,
            cursor_sent: None,
            cursor_shown: false,
            frames: 0,
            overlay: None,
            overlay_corner: None,
        }
    }
}
