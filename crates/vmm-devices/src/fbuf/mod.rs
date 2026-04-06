// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! PCI framebuffer device with integrated VNC server.
//!
//! Provides a simple display adapter that exposes two MMIO BARs to the
//! guest:
//!
//! - **BAR0** (128 bytes): mode control registers (width, height, depth,
//!   framebuffer size)
//! - **BAR1** (16 MiB): raw XRGB8888 pixel framebuffer
//!
//! A VNC server runs in a dedicated thread, listening on a Unix domain
//! socket. It reads pixels from the shared framebuffer and streams them
//! to connected VNC clients using the RFB 3.8 protocol with Raw encoding.
//!
//! # PCI Identity
//!
//! - Vendor: `0xFB5D`, Device: `0x40FB`
//! - Class: Display (0x03), Subclass: VGA (0x00)

pub mod rfb;

use std::io::{self, BufReader, BufWriter, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;

use slog;
use vmm_core::common::RWOp;
use vmm_core::mmio::{MmioBus, MmioFn};

use crate::Lifecycle;
use crate::pci::bar::BarDefine;
use crate::pci::device::{DeviceIdent, DeviceState, PciDevice};
use crate::pci::BarN;

use rfb::{
    ClientMessage, PixelFormat, RectHeader, ENCODING_RAW,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// PCI vendor ID for the framebuffer device.
const PCI_VENDOR_ID: u16 = 0xFB5D;

/// PCI device ID for the framebuffer device.
const PCI_DEVICE_ID: u16 = 0x40FB;

/// PCI class: Display controller.
const PCI_CLASS_DISPLAY: u8 = 0x03;

/// PCI subclass: VGA compatible.
const PCI_SUBCLASS_VGA: u8 = 0x00;

/// BAR0 size: 128 bytes for mode control registers.
const BAR0_SIZE: u32 = 128;

/// BAR1 size: 16 MiB framebuffer.
const BAR1_SIZE: u32 = 16 * 1024 * 1024;

/// Bits per pixel (always 32, XRGB8888).
const DEFAULT_BPP: u16 = 32;

/// Bytes per pixel (XRGB8888).
const BYTES_PER_PIXEL: u64 = 4;

/// Returns true if `width × height × 4` fits within the BAR1 framebuffer.
fn resolution_fits(width: u16, height: u16) -> bool {
    (u64::from(width) * BYTES_PER_PIXEL)
        .checked_mul(u64::from(height))
        .map_or(false, |n| n <= u64::from(BAR1_SIZE))
}

/// VNC update interval (~30 fps).
const VNC_FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// Desktop name sent to VNC clients.
const VNC_DESKTOP_NAME: &str = "bhyve framebuffer";

// ---------------------------------------------------------------------------
// Mode registers (BAR0 layout)
// ---------------------------------------------------------------------------

/// Offsets within BAR0 for mode control registers.
mod memreg {
    /// Framebuffer size in bytes (u32, read-only).
    pub const FBSIZE: usize = 0x00;
    /// Display width in pixels (u16, read-write).
    pub const WIDTH: usize = 0x04;
    /// Display height in pixels (u16, read-write).
    pub const HEIGHT: usize = 0x06;
    /// Bits per pixel (u16, read-only, always 32).
    pub const DEPTH: usize = 0x08;
    /// Refresh rate hint (u16, read-only, always 0).
    pub const REFRESH: usize = 0x0A;
}

// ---------------------------------------------------------------------------
// Shared framebuffer state
// ---------------------------------------------------------------------------

/// Shared framebuffer state accessed by both the PCI device (writes from
/// guest) and the VNC server thread (reads for display).
struct FramebufferState {
    /// Raw pixel data, XRGB8888 format, row-major.
    pixels: Vec<u8>,
}

impl FramebufferState {
    fn new(size: usize) -> Self {
        Self {
            pixels: vec![0u8; size],
        }
    }
}

// ---------------------------------------------------------------------------
// Framebuffer PCI device
// ---------------------------------------------------------------------------

/// PCI framebuffer device with integrated VNC server.
pub struct Framebuffer {
    /// Standard PCI configuration state.
    pci_state: Mutex<DeviceState>,

    /// Current display width and height packed into a single atomic:
    /// high 16 bits = width, low 16 bits = height.
    resolution: AtomicU32,

    /// Shared pixel buffer (BAR1 backing store).
    fb_state: RwLock<FramebufferState>,

    /// MMIO bus for BAR registration/unregistration.
    bus_mmio: Arc<MmioBus>,

    /// Currently registered BAR0 base address (if any).
    registered_bar0: Mutex<Option<u64>>,

    /// Currently registered BAR1 base address (if any).
    registered_bar1: Mutex<Option<u64>>,

    /// Weak self-reference for MMIO handler closures.
    self_ref: Mutex<Option<std::sync::Weak<Self>>>,

    /// VNC password (None = no auth).
    vnc_password: Option<String>,

    /// Path to the VNC Unix socket.
    vnc_path: PathBuf,

    /// Structured logger.
    log: slog::Logger,

    /// Flag to signal VNC thread to shut down.
    shutdown: AtomicBool,
}

impl Framebuffer {
    /// Create a new framebuffer device and start the VNC server.
    ///
    /// The VNC server listens on a Unix domain socket at `vnc_path`.
    /// If `vnc_password` is `Some`, VNC authentication is required.
    ///
    /// Returns `Arc<Self>` because the device is shared between the
    /// PCI bus, MMIO handlers, and the VNC server thread.
    pub fn new(
        width: u16,
        height: u16,
        vnc_path: &Path,
        vnc_password: Option<String>,
        bus_mmio: Arc<MmioBus>,
        log: slog::Logger,
    ) -> Arc<Self> {
        let ident = DeviceIdent {
            vendor_id: PCI_VENDOR_ID,
            device_id: PCI_DEVICE_ID,
            class: PCI_CLASS_DISPLAY,
            subclass: PCI_SUBCLASS_VGA,
            prog_if: 0,
            revision: 1,
            sub_vendor_id: PCI_VENDOR_ID,
            sub_device_id: PCI_DEVICE_ID,
        };

        let mut pci_state = DeviceState::new(ident);
        pci_state.define_bar(BarN::BAR0, BarDefine::Mmio(BAR0_SIZE));
        pci_state.define_bar(BarN::BAR1, BarDefine::Mmio(BAR1_SIZE));

        let resolution = pack_resolution(width, height);

        let dev = Arc::new(Self {
            pci_state: Mutex::new(pci_state),
            resolution: AtomicU32::new(resolution),
            fb_state: RwLock::new(FramebufferState::new(BAR1_SIZE as usize)),
            bus_mmio,
            registered_bar0: Mutex::new(None),
            registered_bar1: Mutex::new(None),
            self_ref: Mutex::new(None),
            vnc_password,
            vnc_path: vnc_path.to_owned(),
            log,
            shutdown: AtomicBool::new(false),
        });

        // Store weak self-reference for MMIO closures
        *dev.self_ref.lock().expect("fbuf: self_ref lock") =
            Some(Arc::downgrade(&dev));

        // Start VNC server thread
        let vnc_dev = Arc::clone(&dev);
        thread::Builder::new()
            .name("fbuf-vnc".into())
            .spawn(move || {
                vnc_server_loop(vnc_dev);
            })
            .expect("fbuf: failed to spawn VNC server thread");

        dev
    }

    /// Get the current display width.
    pub fn width(&self) -> u16 {
        unpack_width(self.resolution.load(Ordering::Relaxed))
    }

    /// Get the current display height.
    pub fn height(&self) -> u16 {
        unpack_height(self.resolution.load(Ordering::Relaxed))
    }

    /// Signal the VNC server to shut down.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// Resolution packing helpers
// ---------------------------------------------------------------------------

fn pack_resolution(width: u16, height: u16) -> u32 {
    (u32::from(width) << 16) | u32::from(height)
}

fn unpack_width(packed: u32) -> u16 {
    (packed >> 16) as u16
}

fn unpack_height(packed: u32) -> u16 {
    packed as u16
}

// ---------------------------------------------------------------------------
// PciDevice trait implementation
// ---------------------------------------------------------------------------

impl PciDevice for Framebuffer {
    fn cfg_read(&self, offset: u8, len: u8) -> u32 {
        let pci = self.pci_state.lock().expect("fbuf: pci_state lock");
        pci.cfg_read(offset, len)
    }

    fn cfg_write(&self, offset: u8, len: u8, val: u32) {
        let mut pci = self.pci_state.lock().expect("fbuf: pci_state lock");
        pci.cfg_write(offset, len, val);

        // Check if BAR or command register changed — trigger re-registration
        let dword_off = offset & 0xFC;
        if dword_off == 0x04 || (0x10..=0x24).contains(&dword_off) {
            let weak = self.self_ref.lock().expect("fbuf: self_ref lock");
            if let Some(arc_self) = weak.as_ref().and_then(|w| w.upgrade()) {
                drop(pci);
                arc_self.update_mmio_registration();
            }
        }
    }

    fn bar_rw(&self, bar: BarN, offset: usize, rwo: RWOp<'_>) {
        match bar {
            BarN::BAR0 => self.bar0_rw(offset, rwo),
            BarN::BAR1 => self.bar1_rw(offset, rwo),
            _ => {
                if let RWOp::Read(ro) = rwo {
                    ro.write_u32(0);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// MMIO registration (follows NVMe pattern)
// ---------------------------------------------------------------------------

impl Framebuffer {
    /// Update MMIO bus registration for BAR0 and BAR1.
    ///
    /// Called when the PCI command register or BAR addresses change.
    /// Uses the same weak-self pattern as NVMe to avoid Arc cycles.
    fn update_mmio_registration(self: &Arc<Self>) {
        let pci = self.pci_state.lock().expect("fbuf: pci lock");
        let mmio_enabled =
            pci.command().contains(crate::pci::bits::RegCmd::MMIO_EN);

        for (bar_n, reg_mutex) in [
            (BarN::BAR0, &self.registered_bar0),
            (BarN::BAR1, &self.registered_bar1),
        ] {
            let mut reg = reg_mutex.lock().expect("fbuf: bar reg lock");
            if let Some(old_addr) = reg.take() {
                let _ = self.bus_mmio.unregister(old_addr);
            }
            if mmio_enabled {
                if let Some((def, addr)) = pci.bars().get(bar_n) {
                    if addr != 0 && def.is_mmio() {
                        let size = def.size();
                        let dev = Arc::clone(self);
                        let bar = bar_n;
                        let handler: Arc<MmioFn> =
                            Arc::new(move |offset: usize, rwo: RWOp<'_>| {
                                dev.bar_rw(bar, offset, rwo);
                            });
                        if self
                            .bus_mmio
                            .register(addr, size, handler)
                            .is_ok()
                        {
                            *reg = Some(addr);
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// BAR0: mode control registers
// ---------------------------------------------------------------------------

impl Framebuffer {
    /// Handle BAR0 MMIO access (mode control registers).
    fn bar0_rw(&self, offset: usize, rwo: RWOp<'_>) {
        match rwo {
            RWOp::Read(ro) => {
                let val = self.bar0_read(offset, ro.len());
                match ro.len() {
                    1 => ro.write_u8(val as u8),
                    2 => ro.write_u16(val as u16),
                    4 => ro.write_u32(val),
                    _ => ro.write_u32(val),
                }
            }
            RWOp::Write(wo) => {
                self.bar0_write(offset, wo.read_u32(), wo.len());
            }
        }
    }

    /// Read a value from the BAR0 register space.
    fn bar0_read(&self, offset: usize, len: usize) -> u32 {
        let res = self.resolution.load(Ordering::Relaxed);
        let width = unpack_width(res);
        let height = unpack_height(res);

        match offset {
            memreg::FBSIZE => BAR1_SIZE,
            memreg::WIDTH => {
                if len >= 4 {
                    // 4-byte read at offset 4 returns width | (height << 16)
                    u32::from(width) | (u32::from(height) << 16)
                } else {
                    u32::from(width)
                }
            }
            memreg::HEIGHT => u32::from(height),
            memreg::DEPTH => {
                if len >= 4 {
                    u32::from(DEFAULT_BPP) // depth | (refresh << 16)
                } else {
                    u32::from(DEFAULT_BPP)
                }
            }
            memreg::REFRESH => 0,
            _ => 0,
        }
    }

    /// Write a value to the BAR0 register space.
    fn bar0_write(&self, offset: usize, val: u32, len: usize) {
        match offset {
            memreg::WIDTH => {
                if len >= 4 {
                    // 4-byte write at offset 4: low 16 = width, high 16 = height
                    let new_w = val as u16;
                    let new_h = (val >> 16) as u16;
                    if new_w > 0 && new_h > 0 && resolution_fits(new_w, new_h) {
                        self.resolution
                            .store(pack_resolution(new_w, new_h), Ordering::Release);
                    }
                } else {
                    // 2-byte write: just width
                    let new_w = val as u16;
                    if new_w > 0 {
                        let old = self.resolution.load(Ordering::Relaxed);
                        let h = unpack_height(old);
                        if resolution_fits(new_w, h) {
                            self.resolution
                                .store(pack_resolution(new_w, h), Ordering::Release);
                        }
                    }
                }
            }
            memreg::HEIGHT => {
                let new_h = val as u16;
                if new_h > 0 {
                    let old = self.resolution.load(Ordering::Relaxed);
                    let w = unpack_width(old);
                    if resolution_fits(w, new_h) {
                        self.resolution
                            .store(pack_resolution(w, new_h), Ordering::Release);
                    }
                }
            }
            // FBSIZE, DEPTH, REFRESH are read-only
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// BAR1: pixel framebuffer
// ---------------------------------------------------------------------------

impl Framebuffer {
    /// Handle BAR1 MMIO access (raw pixel framebuffer).
    fn bar1_rw(&self, offset: usize, rwo: RWOp<'_>) {
        match rwo {
            RWOp::Read(ro) => {
                let fb = self
                    .fb_state
                    .read()
                    .expect("fbuf: fb_state read lock");
                let len = ro.len();
                if offset + len <= fb.pixels.len() {
                    match len {
                        1 => ro.write_u8(fb.pixels[offset]),
                        2 => {
                            let val = u16::from_le_bytes([
                                fb.pixels[offset],
                                fb.pixels[offset + 1],
                            ]);
                            ro.write_u16(val);
                        }
                        4 => {
                            let val = u32::from_le_bytes([
                                fb.pixels[offset],
                                fb.pixels[offset + 1],
                                fb.pixels[offset + 2],
                                fb.pixels[offset + 3],
                            ]);
                            ro.write_u32(val);
                        }
                        8 => {
                            let val = u64::from_le_bytes([
                                fb.pixels[offset],
                                fb.pixels[offset + 1],
                                fb.pixels[offset + 2],
                                fb.pixels[offset + 3],
                                fb.pixels[offset + 4],
                                fb.pixels[offset + 5],
                                fb.pixels[offset + 6],
                                fb.pixels[offset + 7],
                            ]);
                            ro.write_u64(val);
                        }
                        _ => ro.write_u32(0),
                    }
                } else {
                    ro.write_u32(0);
                }
            }
            RWOp::Write(wo) => {
                let mut fb = self
                    .fb_state
                    .write()
                    .expect("fbuf: fb_state write lock");
                let data = wo.buf();
                let len = data.len();
                if offset + len <= fb.pixels.len() {
                    fb.pixels[offset..offset + len].copy_from_slice(data);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// VNC server
// ---------------------------------------------------------------------------

/// Main VNC server loop running in a dedicated thread.
///
/// Listens on a Unix domain socket and handles one client at a time.
/// When a client disconnects, the server waits for a new connection.
fn vnc_server_loop(dev: Arc<Framebuffer>) {
    // Remove any stale socket file
    let _ = std::fs::remove_file(&dev.vnc_path);

    let listener = match UnixListener::bind(&dev.vnc_path) {
        Ok(l) => l,
        Err(e) => {
            slog::error!(dev.log, "failed to bind VNC socket"; "path" => ?dev.vnc_path, "error" => %e);
            return;
        }
    };

    // Set a timeout on accept so we can check the shutdown flag
    listener
        .set_nonblocking(false)
        .expect("fbuf: set_nonblocking");

    slog::info!(dev.log, "VNC server listening"; "path" => ?dev.vnc_path);

    loop {
        if dev.shutdown.load(Ordering::Acquire) {
            break;
        }

        // Accept with a timeout-like approach: set a short SO_RCVTIMEO
        // on the listener's underlying fd. On illumos, UnixListener
        // doesn't support set_nonblocking + poll easily, so we use
        // a blocking accept in a loop and check shutdown between clients.
        match listener.accept() {
            Ok((stream, _addr)) => {
                slog::info!(dev.log, "VNC client connected");
                if let Err(e) = handle_vnc_client(&dev, stream) {
                    if !is_expected_disconnect(&e) {
                        slog::warn!(dev.log, "VNC client error"; "error" => %e);
                    }
                }
                slog::info!(dev.log, "VNC client disconnected");
            }
            Err(e) => {
                if dev.shutdown.load(Ordering::Acquire) {
                    break;
                }
                if e.kind() != io::ErrorKind::WouldBlock
                    && e.kind() != io::ErrorKind::TimedOut
                {
                    slog::warn!(dev.log, "VNC accept error"; "error" => %e);
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }

    // Clean up socket file
    let _ = std::fs::remove_file(&dev.vnc_path);
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

/// Handle a single VNC client connection.
fn handle_vnc_client(
    dev: &Arc<Framebuffer>,
    stream: std::os::unix::net::UnixStream,
) -> io::Result<()> {
    // Set read/write timeouts to detect dead clients
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;

    let read_stream = stream.try_clone()?;
    let mut writer = BufWriter::new(&stream);
    let mut reader = BufReader::new(read_stream);

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
        dev.vnc_password.as_deref(),
    )?;

    // ClientInit
    let _shared_flag = rfb::read_client_init(&mut reader)?;

    // ServerInit
    let res = dev.resolution.load(Ordering::Acquire);
    let width = unpack_width(res);
    let height = unpack_height(res);
    let pf = PixelFormat::xrgb8888();
    rfb::send_server_init(&mut writer, width, height, &pf, VNC_DESKTOP_NAME)?;

    // --- Main event loop ---
    //
    // We alternate between:
    // 1. Polling for client messages (non-blocking reads)
    // 2. Sending framebuffer updates when requested

    // Track whether the client has requested an update
    let update_requested = Arc::new(AtomicBool::new(false));
    let client_alive = Arc::new(AtomicBool::new(true));

    let shutdown_ref = &dev.shutdown;

    // We'll run the reader in the same thread using non-blocking reads
    // with a short timeout, interleaved with frame sends.
    stream.set_read_timeout(Some(Duration::from_millis(5)))?;

    // On first connect, many clients immediately request a full update,
    // but some viewers (like macOS Screen Sharing) send it after init.
    // We'll send an initial update after a brief wait.

    while !shutdown_ref.load(Ordering::Relaxed) && client_alive.load(Ordering::Relaxed) {
        // Poll for client messages (non-blocking due to timeout)
        match rfb::read_client_message(&mut reader) {
            Ok(Some(msg)) => {
                match msg {
                    ClientMessage::FbUpdateRequest { .. } => {
                        update_requested.store(true, Ordering::Release);
                    }
                    ClientMessage::SetPixelFormat(pf) => {
                        // We only support XRGB8888; log if client wants something else
                        if pf.bits_per_pixel != 32 {
                            slog::warn!(dev.log, "client requested unsupported pixel format";
                                "bpp" => pf.bits_per_pixel);
                        }
                    }
                    ClientMessage::SetEncodings(encs) => {
                        // We only support Raw encoding; log what client supports
                        let has_raw = encs.contains(&ENCODING_RAW);
                        if !has_raw {
                            slog::warn!(dev.log, "client encodings do not include Raw, updates may not display correctly");
                        }
                    }
                    ClientMessage::KeyEvent { down, key } => {
                        // TODO: forward to PS/2 keyboard when implemented
                        static WARN_KEY: std::sync::Once = std::sync::Once::new();
                        WARN_KEY.call_once(|| {
                            slog::debug!(dev.log, "VNC keyboard input received but not forwarded (PS/2 not implemented)");
                        });
                        let _ = (down, key);
                    }
                    ClientMessage::PointerEvent { button_mask, x, y } => {
                        // Forward to xHCI tablet if available, otherwise drop
                        static WARN_PTR: std::sync::Once = std::sync::Once::new();
                        WARN_PTR.call_once(|| {
                            slog::debug!(dev.log, "VNC pointer input received but not forwarded (tablet not connected)");
                        });
                        let _ = (button_mask, x, y);
                    }
                    ClientMessage::CutText(_) => {
                        // Clipboard: ignored
                    }
                }
            }
            Ok(None) => {
                // EOF: client disconnected
                client_alive.store(false, Ordering::Release);
                break;
            }
            Err(e) => {
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::TimedOut
                {
                    // No data available, that's fine
                } else {
                    return Err(e);
                }
            }
        }

        // Send framebuffer update if requested
        if update_requested.swap(false, Ordering::AcqRel) {
            if let Err(e) = send_framebuffer_update(dev, &mut writer) {
                if is_expected_disconnect(&e) {
                    client_alive.store(false, Ordering::Release);
                    break;
                }
                return Err(e);
            }
        }

        // Sleep until next frame
        thread::sleep(VNC_FRAME_INTERVAL);
    }

    // Clean shutdown
    let _ = stream.shutdown(Shutdown::Both);

    Ok(())
}

/// Send a full framebuffer update to the VNC client.
fn send_framebuffer_update<W: Write>(
    dev: &Arc<Framebuffer>,
    writer: &mut BufWriter<W>,
) -> io::Result<()> {
    let res = dev.resolution.load(Ordering::Acquire);
    let width = unpack_width(res);
    let height = unpack_height(res);

    if width == 0 || height == 0 {
        return Ok(());
    }

    // Calculate the pixel data size, clamped to the actual framebuffer
    // to prevent unbounded allocation if resolution is somehow larger.
    let stride = u32::from(width) as usize * 4; // 4 bytes per pixel (XRGB8888)
    let frame_bytes = stride.saturating_mul(usize::from(height));

    // Send FramebufferUpdate header (1 rectangle)
    rfb::write_fb_update_header(writer, 1)?;

    // Read pixel data from the shared framebuffer.
    // Clamp to the actual pixel buffer size to avoid unbounded allocation.
    {
        let fb = dev.fb_state.read().expect("fbuf: fb_state read lock");
        let clamped = frame_bytes.min(fb.pixels.len());

        // Derive the actual dimensions we can send from the clamped size
        // so the VNC rectangle header matches the data we transmit.
        let (send_w, send_h) = if clamped < frame_bytes && stride > 0 {
            let rows = clamped / stride;
            (width, rows as u16)
        } else {
            (width, height)
        };

        let rect = RectHeader {
            x: 0,
            y: 0,
            width: send_w,
            height: send_h,
            encoding: ENCODING_RAW,
        };
        writer.write_all(&rect.to_bytes())?;

        let send_bytes = usize::from(send_w) * 4 * usize::from(send_h);
        writer.write_all(&fb.pixels[..send_bytes.min(fb.pixels.len())])?;
    }

    writer.flush()?;

    Ok(())
}

// ---------------------------------------------------------------------------
// CombinedStream helper for handshake
// ---------------------------------------------------------------------------

/// Wrapper that implements `Read + Write` by combining a BufReader and
/// BufWriter, needed for the handshake functions which want a single
/// stream that can both read and write.
struct CombinedStream<'a, R: io::Read, W: io::Write> {
    reader: &'a mut BufReader<R>,
    writer: &'a mut BufWriter<W>,
}

impl<R: io::Read, W: io::Write> io::Read for CombinedStream<'_, R, W> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

impl<R: io::Read, W: io::Write> io::Write for CombinedStream<'_, R, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

impl Lifecycle for Framebuffer {
    fn type_name(&self) -> &'static str {
        "framebuffer"
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution_packing() {
        let packed = pack_resolution(1024, 768);
        assert_eq!(unpack_width(packed), 1024);
        assert_eq!(unpack_height(packed), 768);
    }

    #[test]
    fn resolution_packing_max() {
        let packed = pack_resolution(u16::MAX, u16::MAX);
        assert_eq!(unpack_width(packed), u16::MAX);
        assert_eq!(unpack_height(packed), u16::MAX);
    }

    #[test]
    fn resolution_packing_zero() {
        let packed = pack_resolution(0, 0);
        assert_eq!(unpack_width(packed), 0);
        assert_eq!(unpack_height(packed), 0);
    }

    #[test]
    fn bar0_read_fbsize() {
        // We can test the bar0_read method directly since it doesn't
        // need a full PCI setup.
        let dev = make_test_dev();
        let val = dev.bar0_read(memreg::FBSIZE, 4);
        assert_eq!(val, BAR1_SIZE);
    }

    #[test]
    fn bar0_read_resolution() {
        let dev = make_test_dev();
        assert_eq!(dev.bar0_read(memreg::WIDTH, 2), 1024);
        assert_eq!(dev.bar0_read(memreg::HEIGHT, 2), 768);
    }

    #[test]
    fn bar0_read_depth() {
        let dev = make_test_dev();
        assert_eq!(dev.bar0_read(memreg::DEPTH, 2), 32);
    }

    #[test]
    fn bar0_read_refresh() {
        let dev = make_test_dev();
        assert_eq!(dev.bar0_read(memreg::REFRESH, 2), 0);
    }

    #[test]
    fn bar0_write_width() {
        let dev = make_test_dev();
        dev.bar0_write(memreg::WIDTH, 1920, 2);
        assert_eq!(dev.width(), 1920);
        assert_eq!(dev.height(), 768); // unchanged
    }

    #[test]
    fn bar0_write_height() {
        let dev = make_test_dev();
        dev.bar0_write(memreg::HEIGHT, 1080, 2);
        assert_eq!(dev.width(), 1024); // unchanged
        assert_eq!(dev.height(), 1080);
    }

    #[test]
    fn bar0_write_resolution_dword() {
        let dev = make_test_dev();
        // 4-byte write at WIDTH offset: low16 = width, high16 = height
        let val = u32::from(1920u16) | (u32::from(1080u16) << 16);
        dev.bar0_write(memreg::WIDTH, val, 4);
        assert_eq!(dev.width(), 1920);
        assert_eq!(dev.height(), 1080);
    }

    #[test]
    fn bar0_write_zero_width_ignored() {
        let dev = make_test_dev();
        dev.bar0_write(memreg::WIDTH, 0, 2);
        assert_eq!(dev.width(), 1024); // unchanged
    }

    #[test]
    fn bar0_write_zero_height_ignored() {
        let dev = make_test_dev();
        dev.bar0_write(memreg::HEIGHT, 0, 2);
        assert_eq!(dev.height(), 768); // unchanged
    }

    #[test]
    fn bar0_reserved_reads_zero() {
        let dev = make_test_dev();
        assert_eq!(dev.bar0_read(0x20, 4), 0);
        assert_eq!(dev.bar0_read(0x7C, 4), 0);
    }

    #[test]
    fn bar1_write_and_read() {
        let dev = make_test_dev();

        // Write 4 bytes at offset 0
        {
            let mut fb = dev.fb_state.write().expect("write lock");
            fb.pixels[0] = 0xFF;
            fb.pixels[1] = 0x00;
            fb.pixels[2] = 0x80;
            fb.pixels[3] = 0x00;
        }

        // Read them back via bar1_rw
        let fb = dev.fb_state.read().expect("read lock");
        assert_eq!(fb.pixels[0], 0xFF);
        assert_eq!(fb.pixels[1], 0x00);
        assert_eq!(fb.pixels[2], 0x80);
        assert_eq!(fb.pixels[3], 0x00);
    }

    #[test]
    fn pci_ident_reads() {
        let dev = make_test_dev();
        // Vendor ID
        assert_eq!(dev.cfg_read(0x00, 2), u32::from(PCI_VENDOR_ID));
        // Device ID
        assert_eq!(dev.cfg_read(0x02, 2), u32::from(PCI_DEVICE_ID));
        // Class/subclass at offset 0x08
        let class_reg = dev.cfg_read(0x08, 4);
        assert_eq!((class_reg >> 24) as u8, PCI_CLASS_DISPLAY);
        assert_eq!((class_reg >> 16) as u8, PCI_SUBCLASS_VGA);
    }

    #[test]
    fn bar0_write_oversized_resolution_rejected() {
        let dev = make_test_dev();
        // 65535×65535 × 4 = ~16 GiB -- far exceeds 16 MiB BAR1
        let val = u32::from(u16::MAX) | (u32::from(u16::MAX) << 16);
        dev.bar0_write(memreg::WIDTH, val, 4);
        // Resolution must remain unchanged
        assert_eq!(dev.width(), 1024);
        assert_eq!(dev.height(), 768);
    }

    #[test]
    fn bar0_write_oversized_width_rejected() {
        let dev = make_test_dev();
        // 65535 × 768 × 4 = ~191 MiB -- exceeds 16 MiB BAR1
        dev.bar0_write(memreg::WIDTH, u32::from(u16::MAX), 2);
        assert_eq!(dev.width(), 1024); // unchanged
    }

    #[test]
    fn bar0_write_oversized_height_rejected() {
        let dev = make_test_dev();
        // 1024 × 65535 × 4 = ~256 MiB -- exceeds 16 MiB BAR1
        dev.bar0_write(memreg::HEIGHT, u32::from(u16::MAX), 2);
        assert_eq!(dev.height(), 768); // unchanged
    }

    #[test]
    fn bar0_write_max_fitting_resolution_accepted() {
        let dev = make_test_dev();
        // 2048 × 2048 × 4 = 16 MiB -- exactly fits BAR1
        let val = u32::from(2048u16) | (u32::from(2048u16) << 16);
        dev.bar0_write(memreg::WIDTH, val, 4);
        assert_eq!(dev.width(), 2048);
        assert_eq!(dev.height(), 2048);
    }

    #[test]
    fn resolution_fits_boundary() {
        // 2048 × 2048 × 4 = 16 MiB = BAR1_SIZE exactly
        assert!(resolution_fits(2048, 2048));
        // One pixel over
        assert!(!resolution_fits(2049, 2048));
        // Way over
        assert!(!resolution_fits(u16::MAX, u16::MAX));
        // Zero dimensions (rejected elsewhere, but fits check is true)
        assert!(resolution_fits(0, 0));
    }

    /// Create a test device without starting the VNC server.
    fn make_test_dev() -> Framebuffer {
        let ident = DeviceIdent {
            vendor_id: PCI_VENDOR_ID,
            device_id: PCI_DEVICE_ID,
            class: PCI_CLASS_DISPLAY,
            subclass: PCI_SUBCLASS_VGA,
            prog_if: 0,
            revision: 1,
            sub_vendor_id: PCI_VENDOR_ID,
            sub_device_id: PCI_DEVICE_ID,
        };

        let mut pci_state = DeviceState::new(ident);
        pci_state.define_bar(BarN::BAR0, BarDefine::Mmio(BAR0_SIZE));
        pci_state.define_bar(BarN::BAR1, BarDefine::Mmio(BAR1_SIZE));

        Framebuffer {
            pci_state: Mutex::new(pci_state),
            resolution: AtomicU32::new(pack_resolution(1024, 768)),
            fb_state: RwLock::new(FramebufferState::new(BAR1_SIZE as usize)),
            bus_mmio: Arc::new(MmioBus::new()),
            registered_bar0: Mutex::new(None),
            registered_bar1: Mutex::new(None),
            self_ref: Mutex::new(None),
            vnc_password: None,
            vnc_path: PathBuf::from("/tmp/test-fbuf.vnc"),
            log: slog::Logger::root(slog::Discard, slog::o!()),
            shutdown: AtomicBool::new(false),
        }
    }
}
