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
//! - **BAR1** (16 MiB): devmem-backed XRGB8888 pixel framebuffer
//!
//! A VNC server runs in a dedicated thread, listening on a Unix domain
//! socket. It reads pixels from the devmem host view and streams them
//! to connected VNC clients using the RFB 3.8 protocol with Raw encoding.
//!
//! # PCI Identity
//!
//! - Vendor: `0xFB5D`, Device: `0x40FB`
//! - Class: Display (0x03), Subclass: VGA (0x00)

pub mod rfb;

use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use slog;
use vmm_core::common::RWOp;
use vmm_core::hdl::VmmHdl;
use vmm_core::mem::DevMemSeg;
use vmm_core::mmio::{MmioBus, MmioFn};

use vmm_devices::pci::bar::BarDefine;
use vmm_devices::pci::device::{DeviceIdent, DeviceState, PciDevice};
use vmm_devices::pci::BarN;
use vmm_devices::{InputBroker, Lifecycle};

use rfb::{ClientMessage, PixelFormat, RectHeader, ENCODING_RAW};

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
        .is_some_and(|n| n <= u64::from(BAR1_SIZE))
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
// Framebuffer PCI device
// ---------------------------------------------------------------------------

/// PCI framebuffer device with integrated VNC server.
pub struct Framebuffer {
    /// Standard PCI configuration state.
    pci_state: Mutex<DeviceState>,

    /// Current display width and height packed into a single atomic:
    /// high 16 bits = width, low 16 bits = height.
    resolution: AtomicU32,

    /// Device memory segment backing BAR1.
    fb: DevMemSeg,

    /// VM handle retained for the device memory segment lifetime.
    #[allow(dead_code)]
    hdl: Option<Arc<VmmHdl>>,

    /// MMIO bus for BAR registration/unregistration.
    bus_mmio: Arc<MmioBus>,

    /// Currently registered BAR0 base address (if any).
    registered_bar0: Mutex<Option<u64>>,

    /// Weak self-reference for MMIO handler closures.
    self_ref: Mutex<Option<std::sync::Weak<Self>>>,

    /// VNC password (None = no auth).
    vnc_password: Option<String>,

    /// Path to the VNC Unix socket.
    vnc_path: PathBuf,

    /// Structured logger.
    log: slog::Logger,

    /// Late-bound remote input destinations.
    input: Arc<InputBroker>,

    /// Flag to signal VNC thread to shut down.
    shutdown: AtomicBool,
}

impl Framebuffer {
    /// Create a new framebuffer device and start the VNC server.
    ///
    /// The VNC server listens on a Unix domain socket at `vnc_path`.
    /// If `vnc_password` is `Some`, VNC authentication is required.
    ///
    /// The returned device is shared between the PCI bus, MMIO handlers, and
    /// the VNC server thread.
    ///
    /// # Errors
    ///
    /// Returns an error if the framebuffer devmem segment cannot be created.
    pub fn new(
        width: u16,
        height: u16,
        vnc_path: &Path,
        vnc_password: Option<String>,
        bus_mmio: Arc<MmioBus>,
        hdl: Arc<VmmHdl>,
        segid: i32,
        input: Arc<InputBroker>,
        log: slog::Logger,
    ) -> anyhow::Result<Arc<Self>> {
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
        let fb = DevMemSeg::new(
            Arc::clone(&hdl),
            segid,
            "framebuffer",
            BAR1_SIZE as usize,
        )?;

        let dev = Arc::new(Self {
            pci_state: Mutex::new(pci_state),
            resolution: AtomicU32::new(resolution),
            fb,
            hdl: Some(hdl),
            bus_mmio,
            registered_bar0: Mutex::new(None),
            self_ref: Mutex::new(None),
            vnc_password,
            vnc_path: vnc_path.to_owned(),
            log,
            input,
            shutdown: AtomicBool::new(false),
        });

        // Store weak self-reference for MMIO closures
        *dev.self_ref.lock().expect("fbuf: self_ref lock") =
            Some(Arc::downgrade(&dev));

        let listener = vmm_core::unixsock::bind_restricted(
            vnc_path,
            vmm_core::unixsock::SocketPolicy::default(),
        )?;

        // Start VNC server thread
        let vnc_dev = Arc::clone(&dev);
        thread::Builder::new()
            .name("fbuf-vnc".into())
            .spawn(move || {
                vnc_server_loop(vnc_dev, listener);
            })
            .expect("fbuf: failed to spawn VNC server thread");

        Ok(dev)
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

fn scale_abs(v: u16, extent: u16) -> u16 {
    let divisor = u32::from(extent.saturating_sub(1)).max(1);
    ((u32::from(v) * 0x7FFF) / divisor).min(0x7FFF) as u16
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
            _ => {
                if let RWOp::Read(ro) = rwo {
                    ro.write_u32(0);
                }
            }
        }
    }

    fn detach_regions(&self) {
        {
            let mut reg =
                self.registered_bar0.lock().expect("fbuf: bar reg lock");
            self.release_mmio(&mut reg);
        }
        // BAR1 is a devmem segment, not a bus region, but it decodes
        // guest addresses the same way.
        self.remap_framebuffer(None);
    }
}

// ---------------------------------------------------------------------------
// MMIO registration (follows NVMe pattern)
// ---------------------------------------------------------------------------

impl Framebuffer {
    /// Unregister the recorded BAR0 base and clear the record.
    fn release_mmio(&self, reg: &mut Option<u64>) {
        if let Some(addr) = reg.take() {
            if let Err(e) = self.bus_mmio.unregister(addr) {
                slog::warn!(self.log, "fbuf: MMIO unregister failed";
                    "bar" => ?BarN::BAR0,
                    "addr" => format!("{addr:#x}"),
                    "error" => %e);
            }
        }
    }

    /// Move the framebuffer devmem mapping to `want`, or unmap it.
    fn remap_framebuffer(&self, want: Option<u64>) {
        if self.fb.mapped_gpa() == want {
            return;
        }
        if let Err(e) = self.fb.unmap() {
            slog::warn!(self.log, "fbuf: framebuffer unmap failed"; "error" => %e);
        }
        if let Some(gpa) = want {
            if let Err(e) = self.fb.map_at(gpa) {
                // The guest may transiently select a GPA that overlaps ROM
                // while sizing the BAR, so a rejected mapping is not fatal.
                slog::warn!(self.log, "fbuf: framebuffer map failed, BAR1 undecoded";
                    "gpa" => format!("{gpa:#x}"), "error" => %e);
            }
        }
    }

    /// Update BAR0 MMIO registration and the BAR1 devmem mapping.
    ///
    /// Called when the PCI command register or BAR addresses change.
    /// Uses the same weak-self pattern as NVMe to avoid Arc cycles.
    fn update_mmio_registration(self: &Arc<Self>) {
        let pci = self.pci_state.lock().expect("fbuf: pci lock");
        let mmio_enabled = pci
            .command()
            .contains(vmm_devices::pci::bits::RegCmd::MMIO_EN);

        let mut reg = self.registered_bar0.lock().expect("fbuf: bar reg lock");
        self.release_mmio(&mut reg);
        if mmio_enabled {
            if let Some((def, addr)) = pci.bars().get(BarN::BAR0) {
                if addr != 0 && def.is_mmio() {
                    let size = def.size();
                    let dev = Arc::clone(self);
                    let handler: Arc<MmioFn> =
                        Arc::new(move |offset: usize, rwo: RWOp<'_>| {
                            dev.bar_rw(BarN::BAR0, offset, rwo);
                        });
                    match self.bus_mmio.register(addr, size, handler) {
                        Ok(()) => *reg = Some(addr),
                        // The device now decodes nothing on BAR0.
                        Err(e) => {
                            slog::error!(self.log,
                                "fbuf: MMIO register failed, BAR0 dark";
                                "bar" => ?BarN::BAR0,
                                "addr" => format!("{addr:#x}"),
                                "size" => size,
                                "error" => %e);
                        }
                    }
                }
            }
        }
        drop(reg);

        let bar1_addr = pci
            .bars()
            .get(BarN::BAR1)
            .map(|(_, addr)| addr)
            .unwrap_or(0);
        self.remap_framebuffer(bar1_target(mmio_enabled, bar1_addr));
    }
}

fn bar1_target(mmio_enabled: bool, addr: u64) -> Option<u64> {
    (mmio_enabled && addr != 0).then_some(addr)
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
    // A 32-bit depth read will eventually include refresh in the high half;
    // preserve the width split until that register behavior is implemented.
    #[allow(clippy::if_same_then_else)]
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
                        self.resolution.store(
                            pack_resolution(new_w, new_h),
                            Ordering::Release,
                        );
                    }
                } else {
                    // 2-byte write: just width
                    let new_w = val as u16;
                    if new_w > 0 {
                        let old = self.resolution.load(Ordering::Relaxed);
                        let h = unpack_height(old);
                        if resolution_fits(new_w, h) {
                            self.resolution.store(
                                pack_resolution(new_w, h),
                                Ordering::Release,
                            );
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
                        self.resolution.store(
                            pack_resolution(w, new_h),
                            Ordering::Release,
                        );
                    }
                }
            }
            // FBSIZE, DEPTH, REFRESH are read-only
            _ => {}
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
fn vnc_server_loop(dev: Arc<Framebuffer>, listener: UnixListener) {
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

/// Dispatch a VNC message and report whether it requested a display update.
fn dispatch_client_message(dev: &Framebuffer, msg: ClientMessage) -> bool {
    match msg {
        ClientMessage::FbUpdateRequest { .. } => true,
        ClientMessage::SetPixelFormat(pf) => {
            if pf.bits_per_pixel != 32 {
                slog::warn!(dev.log, "client requested unsupported pixel format";
                    "bpp" => pf.bits_per_pixel);
            }
            false
        }
        ClientMessage::SetEncodings(encs) => {
            if !encs.contains(&ENCODING_RAW) {
                slog::warn!(dev.log, "client encodings do not include Raw, updates may not display correctly");
            }
            false
        }
        ClientMessage::KeyEvent { down, key } => {
            dev.input.key_event(down, key);
            false
        }
        ClientMessage::PointerEvent { button_mask, x, y } => {
            // The tablet only clamps values, so scaling must use fbuf's
            // current resolution before the event reaches the input broker.
            let res = dev.resolution.load(Ordering::Acquire);
            let sx = scale_abs(x, unpack_width(res));
            let sy = scale_abs(y, unpack_height(res));
            dev.input.pointer_event(button_mask, sx, sy);
            false
        }
        ClientMessage::CutText(_) => false,
    }
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

    while !shutdown_ref.load(Ordering::Relaxed)
        && client_alive.load(Ordering::Relaxed)
    {
        // Poll for client messages (non-blocking due to timeout)
        match rfb::read_client_message(&mut reader) {
            Ok(Some(msg)) => {
                if dispatch_client_message(dev, msg) {
                    update_requested.store(true, Ordering::Release);
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
            if let Err(e) = send_framebuffer_update(
                dev,
                &mut writer,
                &mut framebuffer_scratch,
            ) {
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
    scratch: &mut Vec<u8>,
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

    let clamped = frame_bytes.min(dev.fb.len());

    // Derive the actual dimensions we can send from the clamped size
    // so the VNC rectangle header matches the data we transmit.
    let (send_w, send_h) = if clamped < frame_bytes && stride > 0 {
        let rows = clamped / stride;
        (width, rows as u16)
    } else {
        (width, height)
    };

    let send_bytes = usize::from(send_w) * 4 * usize::from(send_h);
    scratch.resize(send_bytes, 0);
    dev.fb
        .view()
        .subregion(0, send_bytes)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "framebuffer update exceeds devmem bounds",
            )
        })?
        .copy_out(scratch)?;

    rfb::write_fb_update_header(writer, 1)?;
    let rect = RectHeader {
        x: 0,
        y: 0,
        width: send_w,
        height: send_h,
        encoding: ENCODING_RAW,
    };
    writer.write_all(&rect.to_bytes())?;
    writer.write_all(scratch)?;

    writer.flush()?;

    Ok(())
}

// ---------------------------------------------------------------------------
// CombinedStream helper for handshake
// ---------------------------------------------------------------------------

/// Wrapper that implements `Read + Write` by combining a BufReader and
/// BufWriter, needed for the handshake functions which want a single
/// stream that can both read and write.
struct CombinedStream<'a, R: Read, W: Write> {
    reader: &'a mut BufReader<R>,
    writer: &'a mut BufWriter<W>,
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
// Lifecycle
// ---------------------------------------------------------------------------

impl Lifecycle for Framebuffer {
    // The VNC thread holds no guest state that must drain or migrate.
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

    #[derive(Default)]
    struct RecordingKeyboard(Mutex<Vec<(bool, u32)>>);

    impl RecordingKeyboard {
        fn events(&self) -> Vec<(bool, u32)> {
            self.0.lock().expect("recording keyboard lock").clone()
        }
    }

    impl vmm_devices::KeyboardSink for RecordingKeyboard {
        fn key_event(&self, down: bool, keysym: u32) {
            self.0
                .lock()
                .expect("recording keyboard lock")
                .push((down, keysym));
        }
    }

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
    fn absolute_pointer_scaling() {
        assert_eq!(scale_abs(0, 1024), 0);
        assert_eq!(scale_abs(1023, 1024), 0x7FFF);

        let midpoint = scale_abs(512, 1024);
        assert!((0x3FE0..=0x4020).contains(&midpoint));
    }

    #[test]
    fn absolute_pointer_scaling_handles_degenerate_extents() {
        assert_eq!(scale_abs(0, 0), 0);
        assert_eq!(scale_abs(5, 1), 0x7FFF);
    }

    #[test]
    fn absolute_pointer_scaling_never_exceeds_tablet_range() {
        for extent in [0, 1, 2, 768, 1024, u16::MAX] {
            for value in 0..=u16::MAX {
                assert!(scale_abs(value, extent) <= 0x7FFF);
            }
        }
    }

    #[test]
    fn key_message_reaches_input_broker() {
        let input = Arc::new(InputBroker::default());
        let keyboard = Arc::new(RecordingKeyboard::default());
        input.set_keyboard(keyboard.clone());
        let dev = make_test_dev_with_input(input);

        assert!(!dispatch_client_message(
            &dev,
            ClientMessage::KeyEvent {
                down: true,
                key: 0xff0d,
            },
        ));
        assert_eq!(keyboard.events(), vec![(true, 0xff0d)]);
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

    #[test]
    fn bar1_is_not_registered_on_the_mmio_bus() {
        let dev = make_test_dev();
        let bar0_addr = 0xE000_0000;
        let bar1_addr = 0xE100_0000;

        dev.cfg_write(0x10, 4, bar0_addr as u32);
        dev.cfg_write(0x14, 4, bar1_addr as u32);
        dev.cfg_write(
            0x04,
            2,
            u32::from(vmm_devices::pci::bits::RegCmd::MMIO_EN.bits()),
        );

        assert_eq!(
            dev.bus_mmio.handle_read(bar0_addr, 4),
            u64::from(BAR1_SIZE)
        );
        assert_eq!(dev.bus_mmio.handle_read(bar1_addr, 4), u64::from(u32::MAX));
    }

    #[test]
    fn bar1_remap_is_gated_on_mmio_en() {
        let addr = 0xE100_0000;

        assert_eq!(bar1_target(false, addr), None);
        assert_eq!(bar1_target(true, 0), None);
        assert_eq!(bar1_target(true, addr), Some(addr));
        assert_eq!(bar1_target(false, addr), None);
    }

    /// Create a test device without starting the VNC server.
    fn make_test_dev() -> Arc<Framebuffer> {
        make_test_dev_with_input(Arc::new(InputBroker::default()))
    }

    fn make_test_dev_with_input(input: Arc<InputBroker>) -> Arc<Framebuffer> {
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

        let dev = Arc::new(Framebuffer {
            pci_state: Mutex::new(pci_state),
            resolution: AtomicU32::new(pack_resolution(1024, 768)),
            fb: DevMemSeg::new_anon(BAR1_SIZE as usize).unwrap(),
            hdl: None,
            bus_mmio: Arc::new(MmioBus::new()),
            registered_bar0: Mutex::new(None),
            self_ref: Mutex::new(None),
            vnc_password: None,
            vnc_path: PathBuf::from("/tmp/test-fbuf.vnc"),
            log: slog::Logger::root(slog::Discard, slog::o!()),
            input,
            shutdown: AtomicBool::new(false),
        });
        *dev.self_ref.lock().expect("fbuf: self_ref lock") =
            Some(Arc::downgrade(&dev));
        dev
    }
}
