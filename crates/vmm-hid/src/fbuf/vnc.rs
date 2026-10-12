// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The VNC server that streams a display over a unix socket.
//!
//! One thread serves the socket: it accepts one client at a time, polls
//! that client for RFB messages and sends a frame when one is asked
//! for. The pixels come from a [`DisplaySource`].

use std::io::{self, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use vmm_core::unixsock::accept::accept_loop;
use vmm_core::unixsock::write_all_bounded;
use vmm_devices::display::{changed_tiles, DisplaySource, Pixels, Rect};
use vmm_devices::input::scale_to_tablet;
use vmm_devices::InputBroker;

use rfb::{
    ClientMessage, PixelFormat, RectHeader, EDS_REASON_CLIENT,
    EDS_REASON_SERVER, EDS_STATUS_OK, EDS_STATUS_PROHIBITED,
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

/// VNC update interval (~30 fps).
const VNC_FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// How long one poll for a client message waits. The same thread sends
/// frames, so this bounds how late a frame can be.
const CLIENT_POLL: Duration = Duration::from_millis(5);

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
    /// Returns an error if the socket cannot be bound.
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
        let handle = thread::Builder::new()
            .name(name.into())
            .spawn(move || server_loop(server, listener))
            .expect("vnc: failed to spawn VNC server thread");
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
    accept_loop(listener, &srv.shutdown, &log, "vnc", |stream| {
        slog::info!(srv.log, "VNC client connected");
        if let Err(e) = handle_vnc_client(&srv, stream) {
            if !is_expected_disconnect(&e) {
                slog::warn!(srv.log, "VNC client error"; "error" => %e);
            }
        }
        slog::info!(srv.log, "VNC client disconnected");
    });

    let _ = std::fs::remove_file(&srv.path);
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
            if !encs.contains(&ENCODING_RAW) {
                slog::warn!(dev.log, "client encodings do not include Raw, updates may not display correctly");
            }
            view.desktop_size = encs.contains(&ENCODING_DESKTOP_SIZE);
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
    rfb::handshake_version(&mut CombinedStream {
        reader: &mut reader,
        writer: &mut writer,
    })?;

    // Security handshake
    rfb::handshake_security(
        &mut CombinedStream {
            reader: &mut reader,
            writer: &mut writer,
        },
        dev.password.as_deref(),
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
    };

    // One thread serves both halves: it polls for client messages with a
    // short read timeout and sends a frame when one was asked for. An
    // incremental request waits here until the display has damage.
    let mut pending: Option<UpdateRequest> = None;
    stream.set_read_timeout(Some(CLIENT_POLL))?;

    while !dev.shutdown.load(Ordering::Relaxed) {
        match rfb::read_client_message(&mut reader) {
            Ok(Some(msg)) => {
                if let Some(req) = dispatch_client_message(dev, &mut view, msg)
                {
                    pending = Some(req.merge(pending));
                }
            }
            // EOF: client disconnected
            Ok(None) => break,
            Err(e) if rfb::is_read_timeout(&e) => {}
            Err(e) => return Err(e),
        }

        if let Some(request) = pending {
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
        }

        thread::sleep(VNC_FRAME_INTERVAL);
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
    let whole_update = announced || request == UpdateRequest::Full;
    let area = if whole_update {
        Some(whole)
    } else {
        damage.and_then(|d| clip(d, whole))
    };
    let Some(area) = area else {
        return Ok(sent);
    };

    let row_bytes = area.width as usize * BYTES_PER_PIXEL as usize;
    scratch.resize(row_bytes * area.height as usize, 0);
    match dev.source.read_rect(area, scratch) {
        Ok(()) => {}
        // The display shrank since `size()`. Its resize damaged all of
        // it, so the next frame repaints it; this one is skipped.
        Err(e) if e.kind() == io::ErrorKind::InvalidInput => return Ok(sent),
        Err(e) => return Err(e),
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
    /// costs the client this frame, not its connection: the resize
    /// damages the whole display, so the next frame repaints it.
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
        }
    }
}
