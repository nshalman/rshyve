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
//! A migration carries the PCI header, the mode and the VRAM; see
//! `migrate`.
//!
//! # PCI Identity
//!
//! - Vendor: `0xFB5D`, Device: `0x40FB`
//! - Class: Display (0x03), Subclass: VGA (0x00)

mod migrate;

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slog;
use vmm_core::common::RWOp;
use vmm_core::hdl::VmmHdl;
use vmm_core::mem::DevMemSeg;
use vmm_core::mmio::{MmioBus, MmioFn};
use vmm_core::ratelimit::{Pace, Pacer};
use vmm_core::rings::RingGate;

use vmm_devices::display::{DisplaySource, Rect};
use vmm_devices::lifecycle::DeviceStateError;
use vmm_devices::pci::bar::BarDefine;
use vmm_devices::pci::device::{DeviceIdent, DeviceState, PciDevice};
use vmm_devices::pci::BarN;
use vmm_devices::{DeviceMigrateState, InputBroker, Lifecycle};

use crate::vnc::VncServer;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// PCI vendor ID for the framebuffer device.
const PCI_VENDOR_ID: u16 = 0xFB5D;

/// BAR1 moves a guest gets at once. Each one stops every kernel ring
/// of the VM.
const REMAP_BURST: u32 = 32;

/// How often one more move is allowed once the burst is spent.
const REMAP_REFILL: Duration = Duration::from_secs(1);

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

    /// A BAR1 move takes the VM write lock, so it runs here.
    rings: Arc<RingGate>,

    /// The budget for BAR1 moves, and whether one is held back.
    remap: Mutex<Remap>,

    /// MMIO bus for BAR registration/unregistration.
    bus_mmio: Arc<MmioBus>,

    /// Currently registered BAR0 base address (if any).
    registered_bar0: Mutex<Option<u64>>,

    /// Weak self-reference for MMIO handler closures.
    self_ref: Mutex<Option<std::sync::Weak<Self>>>,

    /// Structured logger.
    log: slog::Logger,

    /// The VNC server showing this framebuffer. It holds the device,
    /// so shutdown takes it to break the cycle.
    vnc: Mutex<Option<Arc<VncServer>>>,
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
        let rings = Arc::clone(hdl.rings());
        let fb = DevMemSeg::new(hdl, segid, "framebuffer", BAR1_SIZE as usize)?;

        let dev = Arc::new(Self {
            pci_state: Mutex::new(pci_state),
            resolution: AtomicU32::new(resolution),
            fb,
            rings,
            remap: Mutex::new(Remap::new()),
            bus_mmio,
            registered_bar0: Mutex::new(None),
            self_ref: Mutex::new(None),
            log: log.clone(),
            vnc: Mutex::new(None),
        });

        // Store weak self-reference for MMIO closures
        *dev.self_ref.lock().expect("fbuf: self_ref lock") =
            Some(Arc::downgrade(&dev));

        let server = VncServer::start(
            "fbuf-vnc",
            Arc::clone(&dev) as Arc<dyn DisplaySource>,
            vnc_path,
            vnc_password,
            input,
            log,
        )?;
        *dev.vnc.lock().expect("fbuf: vnc lock") = Some(server);

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

    /// Stop the VNC server and wait for its thread to leave.
    ///
    /// Without this the thread outlives halt, keeps streaming devmem to
    /// a connected client, and holds this device and its 16 MiB segment
    /// alive until the process exits.
    pub fn shutdown(&self) {
        let server = self.vnc.lock().expect("fbuf: vnc lock").take();
        if let Some(server) = server {
            server.shutdown();
        }
    }
}

impl DisplaySource for Framebuffer {
    /// One read of the resolution: a width and a height read apart can
    /// straddle a resize and name a buffer neither mode has.
    fn size(&self) -> (u32, u32) {
        let packed = self.resolution.load(Ordering::Relaxed);
        (
            u32::from(unpack_width(packed)),
            u32::from(unpack_height(packed)),
        )
    }

    /// The guest writes devmem directly and nothing traps it, so every
    /// frame is all damage.
    fn take_damage(&self) -> Option<Rect> {
        let (width, height) = self.size();
        (width > 0 && height > 0).then_some(Rect {
            x: 0,
            y: 0,
            width,
            height,
        })
    }

    fn read_rect(&self, r: Rect, dst: &mut [u8]) -> io::Result<()> {
        let bad = |what| io::Error::new(io::ErrorKind::InvalidInput, what);
        let (width, height) = self.size();
        let fits = r.x.checked_add(r.width).is_some_and(|e| e <= width)
            && r.y.checked_add(r.height).is_some_and(|e| e <= height);
        if !fits {
            return Err(bad("rectangle outside the framebuffer"));
        }
        let stride = width as usize * BYTES_PER_PIXEL as usize;
        let row_bytes = r.width as usize * BYTES_PER_PIXEL as usize;
        if dst.len() != row_bytes * r.height as usize {
            return Err(bad("destination does not match the rectangle"));
        }

        // Rows are `stride` bytes apart in the segment, so a narrower
        // rectangle is copied row by row.
        let view = self.fb.view();
        for row in 0..r.height as usize {
            let at = (r.y as usize + row) * stride
                + r.x as usize * BYTES_PER_PIXEL as usize;
            let src = view.subregion(at, row_bytes).ok_or_else(|| {
                bad("framebuffer update exceeds devmem bounds")
            })?;
            src.copy_out(&mut dst[row * row_bytes..][..row_bytes])?;
        }
        Ok(())
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
        self.retry_remap();
        let pci = self.pci_state.lock().expect("fbuf: pci_state lock");
        pci.cfg_read(offset, len)
    }

    fn cfg_write(&self, offset: u8, len: u8, val: u32) {
        self.retry_remap();
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
        // guest addresses the same way. A device that is leaving gives
        // its window back whatever the budget says.
        self.remap_framebuffer(None, false);
    }
}

/// The budget for BAR1 moves.
struct Remap {
    pacer: Pacer,
    /// The mapping lags the guest's BAR1. The next config access tries
    /// again.
    pending: bool,
}

impl Remap {
    fn new() -> Self {
        Self {
            pacer: Pacer::new(REMAP_BURST, REMAP_REFILL),
            pending: false,
        }
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
    ///
    /// The move takes the VM write lock, so it runs in the ring gate.
    /// A `paced` move is one the guest asked for: past the budget the
    /// old mapping goes once, and nothing new comes up until a later
    /// config access finds the budget refilled.
    fn remap_framebuffer(&self, want: Option<u64>, paced: bool) {
        let mut remap = self.remap.lock().expect("fbuf: remap lock");
        let have = self.fb.mapped_gpa();
        if have == want {
            remap.pending = false;
            return;
        }
        let pace = if paced {
            let pace = remap.pacer.admit(Instant::now(), have.is_none());
            if remap.pacer.first_throttle() {
                slog::warn!(
                    self.log,
                    "fbuf: BAR1 moved too often, \
                    framebuffer unmapped for now"
                );
            }
            pace
        } else {
            Pace::Change
        };
        let target = match pace {
            Pace::Change => want,
            Pace::Clear => None,
            Pace::Wait => {
                remap.pending = true;
                return;
            }
        };
        let ran = self.rings.run(|rings| {
            if let Err(e) = self.fb.unmap(rings) {
                slog::warn!(self.log, "fbuf: framebuffer unmap failed"; "error" => %e);
            }
            if let Some(gpa) = target {
                if let Err(e) = self.fb.map_at(rings, gpa) {
                    // The guest may transiently select a GPA that overlaps ROM
                    // while sizing the BAR, so a rejected mapping is not fatal.
                    slog::warn!(self.log, "fbuf: framebuffer map failed, BAR1 undecoded";
                        "gpa" => format!("{gpa:#x}"), "error" => %e);
                }
            }
        });
        remap.pending = target != want;
        if let Err(e) = ran {
            slog::warn!(self.log, "fbuf: framebuffer mapping unchanged"; "error" => %e);
            remap.pending = true;
        }
    }

    /// Catch BAR1 up with the guest, if a move was held back.
    fn retry_remap(&self) {
        if !self.remap.lock().expect("fbuf: remap lock").pending {
            return;
        }
        let want = {
            let pci = self.pci_state.lock().expect("fbuf: pci lock");
            bar1_want(&pci)
        };
        self.remap_framebuffer(want, true);
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

        self.remap_framebuffer(bar1_want(&pci), true);
    }
}

/// Where the guest decodes BAR1 now, if anywhere.
fn bar1_want(pci: &DeviceState) -> Option<u64> {
    let mmio_enabled = pci
        .command()
        .contains(vmm_devices::pci::bits::RegCmd::MMIO_EN);
    let bar1_addr = pci
        .bars()
        .get(BarN::BAR1)
        .map(|(_, addr)| addr)
        .unwrap_or(0);
    bar1_target(mmio_enabled, bar1_addr)
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
                ro.write_dword(val)
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
            // There is no refresh register, so the high half of a
            // 4-byte read is zero.
            memreg::DEPTH => u32::from(DEFAULT_BPP),
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
// Lifecycle
// ---------------------------------------------------------------------------

impl Lifecycle for Framebuffer {
    // The VNC thread holds no guest state that must drain.
    fn type_name(&self) -> &'static str {
        "framebuffer"
    }

    fn export_migrate_state(
        &self,
    ) -> Result<Option<DeviceMigrateState>, DeviceStateError> {
        self.export_state().map(Some)
    }

    fn restore_migrate_state(
        &self,
        state: &DeviceMigrateState,
    ) -> Result<(), DeviceStateError> {
        self.restore_state(state)
    }

    fn halt(&self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[derive(Default)]
    pub(crate) struct RecordingKeyboard(Mutex<Vec<(bool, u32)>>);

    impl RecordingKeyboard {
        pub(crate) fn events(&self) -> Vec<(bool, u32)> {
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

        fn scancode_event(&self, _down: bool, _code: u8, _extended: bool) {}
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

    /// The width and height come from one read of the resolution: a pair
    /// read half before and half after a resize can name a buffer far
    /// larger than either mode, and a server allocates by it.
    #[test]
    fn size_is_one_resolution_never_half_of_two() {
        use std::sync::atomic::AtomicBool;
        let dev = make_test_dev();
        let (a, b) = ((4000u16, 16u16), (16u16, 4000u16));
        let stop = Arc::new(AtomicBool::new(false));
        let flipper = {
            let dev = Arc::clone(&dev);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    for (w, h) in [a, b] {
                        dev.resolution
                            .store(pack_resolution(w, h), Ordering::Relaxed);
                    }
                }
            })
        };
        dev.resolution
            .store(pack_resolution(a.0, a.1), Ordering::Relaxed);
        let mut mixed = None;
        for _ in 0..2_000_000 {
            let got = DisplaySource::size(&*dev);
            let ok = [a, b]
                .iter()
                .any(|&(w, h)| got == (u32::from(w), u32::from(h)));
            if !ok {
                mixed = Some(got);
                break;
            }
        }
        stop.store(true, Ordering::Relaxed);
        flipper.join().expect("flipper");
        assert_eq!(mixed, None, "a size made of two resolutions");
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

    /// Counts the times the gate stopped the VM's rings.
    struct CountingRings(
        std::sync::atomic::AtomicUsize,
        std::sync::atomic::AtomicBool,
    );

    impl vmm_core::rings::KernelRings for CountingRings {
        fn stop(&self) -> Result<(), String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn start(&self) {
            self.1.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn a_bar1_move_runs_in_the_ring_gate_and_is_held_to_a_budget() {
        // VM_MMAP_MEMSEG takes the VM write lock, and the guest picks
        // how often BAR1 moves.
        let dev = make_test_dev();
        let rings = Arc::new(CountingRings(
            std::sync::atomic::AtomicUsize::new(0),
            std::sync::atomic::AtomicBool::new(false),
        ));
        dev.rings
            .register(Arc::downgrade(&rings) as std::sync::Weak<_>)
            .expect("register");
        dev.cfg_write(
            0x04,
            2,
            u32::from(vmm_devices::pci::bits::RegCmd::MMIO_EN.bits()),
        );

        dev.cfg_write(0x14, 4, 0xE100_0000);
        assert_eq!(rings.0.load(Ordering::SeqCst), 1);
        assert!(rings.1.load(Ordering::SeqCst), "the rings never started");

        for i in 0..1000u32 {
            dev.cfg_write(0x14, 4, 0xE100_0000 + (i % 2) * 0x0100_0000);
        }
        assert_eq!(rings.0.load(Ordering::SeqCst), REMAP_BURST as usize);
    }

    #[test]
    fn read_rect_copies_a_sub_rectangle_and_refuses_one_outside() {
        let dev = make_test_dev();
        dev.bar0_write(memreg::WIDTH, 4 | (3 << 16), 4);
        let px: Vec<u8> = (0..12u32).flat_map(|i| i.to_le_bytes()).collect();
        dev.fb
            .view()
            .subregion(0, px.len())
            .expect("in the segment")
            .copy_in(&px)
            .expect("write pixels");

        let r = Rect {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        };
        let mut out = vec![0u8; 16];
        dev.read_rect(r, &mut out).expect("inside the framebuffer");
        let got: Vec<u32> = out
            .chunks(4)
            .map(|c| u32::from_le_bytes(c.try_into().expect("4 bytes")))
            .collect();
        assert_eq!(got, [5, 6, 9, 10]);

        let outside = Rect { x: 3, ..r };
        assert!(dev.read_rect(outside, &mut out).is_err());
        assert!(dev.read_rect(r, &mut out[..8]).is_err());
    }

    /// Change the resolution as a guest mode set would.
    pub(crate) fn set_resolution(dev: &Framebuffer, width: u16, height: u16) {
        dev.resolution
            .store(pack_resolution(width, height), Ordering::Release);
    }

    /// Give `dev` a server, as `Framebuffer::new` does.
    pub(crate) fn attach_server(dev: &Framebuffer, srv: Arc<VncServer>) {
        *dev.vnc.lock().expect("fbuf: vnc lock") = Some(srv);
    }

    pub(crate) fn has_server(dev: &Framebuffer) -> bool {
        dev.vnc.lock().expect("fbuf: vnc lock").is_some()
    }

    /// fbuf cannot see guest writes to its devmem, so every frame is
    /// all damage.
    #[test]
    fn every_fbuf_frame_is_fully_damaged() {
        let dev = make_test_dev();
        let full = Rect {
            x: 0,
            y: 0,
            width: 1024,
            height: 768,
        };
        assert_eq!(dev.take_damage(), Some(full));
        assert_eq!(dev.take_damage(), Some(full));
    }

    /// Create a test device without starting the VNC server.
    pub(crate) fn make_test_dev() -> Arc<Framebuffer> {
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
            rings: RingGate::new(),
            remap: Mutex::new(Remap::new()),
            bus_mmio: Arc::new(MmioBus::new()),
            registered_bar0: Mutex::new(None),
            self_ref: Mutex::new(None),
            log: slog::Logger::root(slog::Discard, slog::o!()),
            vnc: Mutex::new(None),
        });
        *dev.self_ref.lock().expect("fbuf: self_ref lock") =
            Some(Arc::downgrade(&dev));
        dev
    }
}
