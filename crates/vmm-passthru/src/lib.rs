// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! PCI passthrough (PPT) device.
//!
//! Gives a real PCI function to the guest:
//! 1. Open `/dev/pptN` and bind it to the VM.
//! 2. Read physical config space and BAR information from hardware.
//! 3. Map device MMIO BARs into guest physical address space with EPT
//!    (`VM_MAP_PPTDEV_MMIO`), so guest MMIO goes to hardware with no
//!    VM exit.
//! 4. Interpose on config space. Only the command register, the BARs,
//!    the interrupt line and the MSI capability have a handler. Every
//!    other write is dropped.
//! 5. Relay MSI through `VM_PPTDEV_MSI`. The kernel owns the physical
//!    MSI registers and injects into the guest LAPIC.
//! 6. Unbind on drop.
//!
//! # Limits
//!
//! **An MSI-X capable device is refused.** `update_bar_mappings` maps a
//! full BAR, and on an MSI-X device that BAR holds the physical MSI-X
//! table. illumos VT-d remaps DMA but not interrupts, so a guest that
//! writes that table makes the device raise a guest-chosen vector on
//! the host APIC. Linux gates the same hole behind
//! `allow_unsafe_interrupts`; this refuses the device instead. To lift
//! the refusal, carve the MSI-X table and PBA pages out of the mapped
//! range, serve them from an emulated table on the MMIO bus, and
//! program the hardware through `VM_PPTDEV_MSIX`. That work needs a
//! host with a real assigned device to test against.
//!
//! **`bar_rw` never runs.** Nothing registers this device on the MMIO
//! or PIO bus, unlike `vmm_virtio::pci`, which registers its own BAR
//! handlers. MMIO BARs do not need it, because EPT maps them. PIO BARs
//! do, so a passthrough device with an I/O BAR decodes nothing.

use std::io::{Error, ErrorKind, Result};
use std::os::fd::AsRawFd;
use std::os::unix::io::RawFd;
use std::sync::{Arc, Mutex};

use bhyve_api::{
    ioctls, ppt_bar_io, ppt_bar_query, ppt_cfg_io, vm_pptdev, vm_pptdev_limits,
    vm_pptdev_mmio, vm_pptdev_msi, PCI_ADDR_IO, PCI_ADDR_MEM32, PCI_ADDR_MEM64,
};
use slog::{o, warn, Logger};

use vmm_core::common::RWOp;
use vmm_core::hdl::VmmHdl;

use vmm_devices::pci::bar::{BarDefine, BAR_COUNT};
use vmm_devices::pci::device::{DeviceState, PciDevice};
use vmm_devices::pci::BarN;

mod cfg;

use cfg::{
    check_passthrough_supported, classify_cfg_write, msi_numvec,
    scan_capabilities, CapLayout, CfgWrite,
};

// ── PCI capability constants ───────────────────────────────────────

/// MSI capability: Message Control register offset within capability.
const MSI_MSG_CTRL_OFF: u8 = 2;
/// MSI capability: Message Address (lower 32 bits).
const MSI_MSG_ADDR_OFF: u8 = 4;
/// MSI capability: Message Data offset for 32-bit address variant.
const MSI_MSG_DATA_32_OFF: u8 = 8;
/// MSI capability: Message Upper Address (64-bit).
const MSI_MSG_UPPER_ADDR_OFF: u8 = 8;
/// MSI capability: Message Data offset for 64-bit address variant.
const MSI_MSG_DATA_64_OFF: u8 = 12;
/// MSI Message Control: MSI Enable.
const MSI_MSG_CTRL_ENABLE: u16 = 1 << 0;
/// MSI Message Control: 64-bit address capable.
const MSI_MSG_CTRL_64BIT: u16 = 1 << 7;
/// MSI Message Control: per-vector masking capable.
const MSI_MSG_CTRL_PVM: u16 = 1 << 8;

/// PCI Command register bits we care about.
const PCI_CMD_IO_EN: u16 = 1 << 0;
const PCI_CMD_MMIO_EN: u16 = 1 << 1;

/// PCI config space offsets this device interposes on.
const CFG_COMMAND: u8 = 0x04;
const CFG_STATUS: u8 = 0x06;
const CFG_BAR0: u8 = 0x10;
const CFG_BAR5: u8 = 0x24;
const CFG_CAP_PTR: usize = 0x34;
const CFG_INTERRUPT_LINE: u8 = 0x3C;
/// The first offset a capability may live at.
const CFG_CAP_FIRST: u8 = 0x40;

// ── PPT BAR info ───────────────────────────────────────────────────

/// Cached information about one physical BAR.
#[derive(Debug, Clone, Copy, Default)]
struct BarInfo {
    /// BAR type: `PCI_ADDR_IO`, `PCI_ADDR_MEM32`, or `PCI_ADDR_MEM64`.
    bar_type: u32,
    /// Host physical address (base) of the BAR.
    hpa: u64,
    /// Size of the BAR in bytes.
    size: u64,
}

// ── Main struct ────────────────────────────────────────────────────

/// PCI passthrough device.
///
/// Binds a real PCI function (`/dev/pptN`) to a VM and shows it to the
/// guest. MMIO BARs go into guest physical address space with EPT.
/// Config space is interposed, deny by default.
pub struct PciPassthru {
    /// File descriptor for `/dev/pptN`.
    ppt_fd: RawFd,
    /// VM handle for ioctls.
    hdl: Arc<VmmHdl>,
    /// Cached physical BAR information from the kernel.
    bars: [BarInfo; BAR_COUNT],
    /// Physical PCI config space cache (first 256 bytes).
    phys_cfg: [u8; 256],
    /// Capability layout of the physical device.
    caps: CapLayout,
    /// MSI vector limit from the kernel. It bounds what a guest can
    /// ask `VM_PPTDEV_MSI` for.
    msi_limit: i32,
    /// Standard PCI config header state (BAR addresses, command reg).
    state: Mutex<DeviceState>,
    /// Currently mapped BAR GPAs: `(gpa, len)` if mapped.
    mapped_bars: Mutex<[Option<(u64, u64)>; BAR_COUNT]>,
    /// Emulated PCI command register (shadow for MMIO_EN tracking).
    command: Mutex<u16>,
    /// Last MSI Enable bit the guest wrote. The bit itself never
    /// reaches the hardware, so this is the only record of the intent.
    msi_enabled: Mutex<bool>,
    log: Logger,
}

impl PciPassthru {
    /// Create a new PCI passthrough device with no logging.
    ///
    /// Prefer [`PciPassthru::new_with_logger`]. Without a logger a
    /// dropped config-space write leaves no trace.
    pub fn new(ppt_path: &str, hdl: Arc<VmmHdl>) -> Result<Arc<Self>> {
        Self::new_with_logger(ppt_path, hdl, Logger::root(slog::Discard, o!()))
    }

    /// Create a new PCI passthrough device.
    ///
    /// Opens `ppt_path` (e.g. `/dev/ppt0`), binds it to the VM, and
    /// reads physical config space and BAR information.
    ///
    /// Returns [`ErrorKind::Unsupported`] if the device advertises
    /// MSI-X. See the module documentation for why.
    pub fn new_with_logger(
        ppt_path: &str,
        hdl: Arc<VmmHdl>,
        log: Logger,
    ) -> Result<Arc<Self>> {
        // Open the PPT device.
        let ppt_fd = {
            let c_path = std::ffi::CString::new(ppt_path).map_err(|_| {
                Error::new(ErrorKind::InvalidInput, "invalid ppt path")
            })?;
            // Safety: opening a device node with valid C string.
            // Guest-reset re-exec must not inherit passthrough descriptors.
            let fd = unsafe {
                libc::open(c_path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC)
            };
            if fd < 0 {
                return Err(Error::last_os_error());
            }
            fd
        };

        // Bind the PPT device to the VM.
        let mut bind_req = vm_pptdev { pptfd: ppt_fd };
        // Safety: vm_pptdev is the correct struct for VM_BIND_PPTDEV.
        // The VM fd and PPT fd are both valid at this point.
        unsafe {
            ioctl_on_hdl(hdl.as_raw_fd(), ioctls::VM_BIND_PPTDEV, &mut bind_req)
        }
        .inspect_err(|_| {
            // Clean up the fd on bind failure.
            unsafe { libc::close(ppt_fd) };
        })?;

        // From here on every early return must give the device back to
        // the host, so the bind is owned by a guard.
        let bound = BoundPpt {
            fd: ppt_fd,
            hdl: Arc::clone(&hdl),
            armed: true,
            log: log.clone(),
        };

        // Query MSI/MSI-X limits.
        let mut limits = vm_pptdev_limits {
            pptfd: ppt_fd,
            msi_limit: 0,
            msix_limit: 0,
        };
        // Safety: vm_pptdev_limits is correct for VM_GET_PPTDEV_LIMITS.
        unsafe {
            ioctl_on_hdl(
                hdl.as_raw_fd(),
                ioctls::VM_GET_PPTDEV_LIMITS,
                &mut limits,
            )
        }?;

        // Read the full PCI config space (256 bytes) via PPT_CFG_READ.
        let mut phys_cfg = [0u8; 256];
        for off in (0..256u64).step_by(4) {
            let mut cfg_io = ppt_cfg_io {
                pci_off: off,
                pci_width: 4,
                pci_data: 0,
            };
            // Safety: ppt_cfg_io is correct for PPT_CFG_READ.
            // The ppt_fd is a valid /dev/pptN fd.
            unsafe { ioctl_on_fd(ppt_fd, ioctls::PPT_CFG_READ, &mut cfg_io) }?;
            let bytes = cfg_io.pci_data.to_le_bytes();
            let idx = off as usize;
            phys_cfg[idx..idx + 4].copy_from_slice(&bytes);
        }

        // Refuse the device before any BAR is mapped or any interrupt
        // resource is claimed.
        let caps = scan_capabilities(&phys_cfg);
        check_passthrough_supported(&caps)?;

        // Query all 6 BARs.
        let mut bars = [BarInfo::default(); BAR_COUNT];
        for (i, bar) in bars.iter_mut().enumerate() {
            let mut query = ppt_bar_query {
                pbq_baridx: i as u32,
                pbq_type: 0,
                pbq_base: 0,
                pbq_size: 0,
            };
            // Safety: ppt_bar_query is correct for PPT_BAR_QUERY.
            unsafe { ioctl_on_fd(ppt_fd, ioctls::PPT_BAR_QUERY, &mut query) }?;
            *bar = BarInfo {
                bar_type: query.pbq_type,
                hpa: query.pbq_base,
                size: query.pbq_size,
            };
        }

        // Build DeviceState from physical config space.
        let ident = vmm_devices::pci::device::DeviceIdent {
            vendor_id: u16::from_le_bytes([phys_cfg[0], phys_cfg[1]]),
            device_id: u16::from_le_bytes([phys_cfg[2], phys_cfg[3]]),
            revision: phys_cfg[8],
            prog_if: phys_cfg[9],
            subclass: phys_cfg[0x0A],
            class: phys_cfg[0x0B],
            sub_vendor_id: u16::from_le_bytes([phys_cfg[0x2C], phys_cfg[0x2D]]),
            sub_device_id: u16::from_le_bytes([phys_cfg[0x2E], phys_cfg[0x2F]]),
        };
        let mut dev_state = DeviceState::new(ident);

        // Define BARs in DeviceState based on physical BAR info.
        let mut i = 0;
        while i < BAR_COUNT {
            let info = &bars[i];
            if info.size == 0 {
                i += 1;
                continue;
            }
            if let Ok(bar_n) = BarN::try_from(i as u8) {
                match info.bar_type {
                    PCI_ADDR_IO => {
                        if info.size <= u64::from(u16::MAX) {
                            dev_state.define_bar(
                                bar_n,
                                BarDefine::Pio(info.size as u16),
                            );
                        }
                        i += 1;
                    }
                    PCI_ADDR_MEM32 => {
                        dev_state.define_bar(
                            bar_n,
                            BarDefine::Mmio(info.size as u32),
                        );
                        i += 1;
                    }
                    PCI_ADDR_MEM64 => {
                        dev_state
                            .define_bar(bar_n, BarDefine::Mmio64(info.size));
                        i += 2; // 64-bit BAR consumes two slots
                    }
                    _ => {
                        i += 1;
                    }
                }
            } else {
                i += 1;
            }
        }

        dev_state.set_cap_ptr(phys_cfg[CFG_CAP_PTR]);

        Ok(Arc::new(Self {
            ppt_fd: bound.release(),
            hdl,
            bars,
            phys_cfg,
            caps,
            msi_limit: limits.msi_limit,
            state: Mutex::new(dev_state),
            mapped_bars: Mutex::new([None; BAR_COUNT]),
            command: Mutex::new(0),
            msi_enabled: Mutex::new(false),
            log,
        }))
    }

    // ── Internal helpers ───────────────────────────────────────────

    /// Read a register from the physical device config space via ioctl.
    fn ppt_cfg_read(&self, offset: u64, width: u32) -> Result<u32> {
        let mut cfg_io = ppt_cfg_io {
            pci_off: offset,
            pci_width: width,
            pci_data: 0,
        };
        // Safety: ppt_cfg_io is correct for PPT_CFG_READ on our ppt_fd.
        unsafe { ioctl_on_fd(self.ppt_fd, ioctls::PPT_CFG_READ, &mut cfg_io) }?;
        Ok(cfg_io.pci_data)
    }

    /// Write a register to the physical device config space via ioctl.
    ///
    /// Every caller must first pass the offset through
    /// [`classify_cfg_write`]. The illumos PPT driver does no filtering
    /// of its own: `PPT_CFG_WRITE` is a raw `pci_config_put32`.
    fn ppt_cfg_write(&self, offset: u64, width: u32, data: u32) -> Result<()> {
        let mut cfg_io = ppt_cfg_io {
            pci_off: offset,
            pci_width: width,
            pci_data: data,
        };
        // Safety: ppt_cfg_io is correct for PPT_CFG_WRITE on our ppt_fd.
        unsafe {
            ioctl_on_fd(self.ppt_fd, ioctls::PPT_CFG_WRITE, &mut cfg_io)
        }?;
        Ok(())
    }

    /// Map a PPT device BAR into guest physical address space via EPT.
    fn map_bar_mmio(&self, bar_idx: usize, gpa: u64, len: u64) -> Result<()> {
        let hpa = self.bars[bar_idx].hpa;
        let mut mmio = vm_pptdev_mmio {
            pptfd: self.ppt_fd,
            gpa,
            hpa,
            len: len as usize,
        };
        // Safety: vm_pptdev_mmio is correct for VM_MAP_PPTDEV_MMIO.
        unsafe {
            ioctl_on_hdl(
                self.hdl.as_raw_fd(),
                ioctls::VM_MAP_PPTDEV_MMIO,
                &mut mmio,
            )
        }?;
        Ok(())
    }

    /// Unmap a PPT device BAR from guest physical address space.
    fn unmap_bar_mmio(&self, gpa: u64, len: u64) -> Result<()> {
        let mut mmio = vm_pptdev_mmio {
            pptfd: self.ppt_fd,
            gpa,
            hpa: 0,
            len: len as usize,
        };
        // Safety: vm_pptdev_mmio is correct for VM_UNMAP_PPTDEV_MMIO.
        unsafe {
            ioctl_on_hdl(
                self.hdl.as_raw_fd(),
                ioctls::VM_UNMAP_PPTDEV_MMIO,
                &mut mmio,
            )
        }?;
        Ok(())
    }

    /// Handle a BAR address change: unmap old mapping, create new one.
    fn handle_bar_remap(&self, bar_idx: usize, new_gpa: u64, size: u64) {
        let mut mapped = self.mapped_bars.lock().expect("mapped_bars lock");
        let cmd = *self.command.lock().expect("command lock");
        let mmio_enabled = (cmd & PCI_CMD_MMIO_EN) != 0;
        let io_enabled = (cmd & PCI_CMD_IO_EN) != 0;

        // Unmap old if present.
        if let Some((old_g, old_len)) = mapped[bar_idx].take() {
            self.report_unmap(old_g, old_len);
        }

        // Map new if the appropriate enable bit is set and address is valid.
        if new_gpa == 0 || size == 0 {
            return;
        }

        let bar_type = self.bars[bar_idx].bar_type;
        let should_map = match bar_type {
            PCI_ADDR_IO => io_enabled,
            PCI_ADDR_MEM32 | PCI_ADDR_MEM64 => mmio_enabled,
            _ => false,
        };

        if should_map && self.map_bar_mmio(bar_idx, new_gpa, size).is_ok() {
            mapped[bar_idx] = Some((new_gpa, size));
        }
    }

    /// Map or unmap all MMIO BARs based on the current command register.
    fn update_bar_mappings(&self, old_cmd: u16, new_cmd: u16) {
        let mmio_changed =
            (old_cmd & PCI_CMD_MMIO_EN) != (new_cmd & PCI_CMD_MMIO_EN);
        let io_changed = (old_cmd & PCI_CMD_IO_EN) != (new_cmd & PCI_CMD_IO_EN);

        if !mmio_changed && !io_changed {
            return;
        }

        let state = self.state.lock().expect("state lock");
        let mut mapped = self.mapped_bars.lock().expect("mapped_bars lock");

        for i in 0..BAR_COUNT {
            let bar_info = &self.bars[i];
            if bar_info.size == 0 {
                continue;
            }

            let is_mmio =
                matches!(bar_info.bar_type, PCI_ADDR_MEM32 | PCI_ADDR_MEM64);
            let is_io = bar_info.bar_type == PCI_ADDR_IO;

            if (is_mmio && !mmio_changed) || (is_io && !io_changed) {
                continue;
            }

            if let Ok(bar_n) = BarN::try_from(i as u8) {
                if let Some((_def, addr)) = state.bars().get(bar_n) {
                    let now_enabled = if is_mmio {
                        (new_cmd & PCI_CMD_MMIO_EN) != 0
                    } else {
                        (new_cmd & PCI_CMD_IO_EN) != 0
                    };

                    if now_enabled && addr != 0 {
                        // Map.
                        if mapped[i].is_none()
                            && self.map_bar_mmio(i, addr, bar_info.size).is_ok()
                        {
                            mapped[i] = Some((addr, bar_info.size));
                        }
                    } else {
                        // Unmap.
                        if let Some((gpa, len)) = mapped[i].take() {
                            self.report_unmap(gpa, len);
                        }
                    }
                }
            }
        }
    }

    /// Unmap a BAR and say so if the kernel refuses.
    ///
    /// A failed unmap leaves the guest with an EPT window onto device
    /// registers it no longer owns, so it must not pass in silence.
    fn report_unmap(&self, gpa: u64, len: u64) {
        if let Err(e) = self.unmap_bar_mmio(gpa, len) {
            warn!(self.log, "passthru: BAR unmap failed, window still open";
                "gpa" => format!("{gpa:#x}"),
                "len" => len,
                "error" => %e);
        }
    }

    /// Read from a BAR region at the given byte offset using PPT ioctl.
    fn bar_io_read(&self, bar: u32, offset: u32, width: u32) -> u32 {
        let mut io = ppt_bar_io {
            pbi_bar: bar,
            pbi_off: offset,
            pbi_width: width,
            pbi_data: 0,
        };
        // Safety: ppt_bar_io is correct for PPT_BAR_READ.
        match unsafe { ioctl_on_fd(self.ppt_fd, ioctls::PPT_BAR_READ, &mut io) }
        {
            Ok(_) => io.pbi_data,
            // An undecoded PCI read returns all ones.
            Err(_) => u32::MAX,
        }
    }

    /// Write to a BAR region at the given byte offset using PPT ioctl.
    fn bar_io_write(&self, bar: u32, offset: u32, width: u32, data: u32) {
        let mut io = ppt_bar_io {
            pbi_bar: bar,
            pbi_off: offset,
            pbi_width: width,
            pbi_data: data,
        };
        // Safety: ppt_bar_io is correct for PPT_BAR_WRITE.
        if let Err(e) =
            unsafe { ioctl_on_fd(self.ppt_fd, ioctls::PPT_BAR_WRITE, &mut io) }
        {
            warn!(self.log, "passthru: BAR I/O write failed";
                "bar" => bar, "off" => offset, "error" => %e);
        }
    }
}

impl PciDevice for PciPassthru {
    fn cfg_read(&self, offset: u8, len: u8) -> u32 {
        let dword_off = offset & 0xFC;

        match dword_off {
            // Vendor ID / Device ID: return from cache.
            0x00 => {
                let val = u32::from_le_bytes([
                    self.phys_cfg[0],
                    self.phys_cfg[1],
                    self.phys_cfg[2],
                    self.phys_cfg[3],
                ]);
                extract_bytes(val, offset, len)
            }

            // Command register: return emulated value (low 16) +
            // physical status (high 16).
            CFG_COMMAND => {
                let cmd = *self.command.lock().expect("command lock");
                let status = self
                    .ppt_cfg_read(u64::from(CFG_STATUS), 2)
                    .unwrap_or(0) as u16;
                let val = u32::from(cmd) | (u32::from(status) << 16);
                extract_bytes(val, offset, len)
            }

            // Class / Revision: return from cache.
            0x08 => {
                let val = u32::from_le_bytes([
                    self.phys_cfg[8],
                    self.phys_cfg[9],
                    self.phys_cfg[0x0A],
                    self.phys_cfg[0x0B],
                ]);
                extract_bytes(val, offset, len)
            }

            // BARs: return from DeviceState (guest-programmed values).
            CFG_BAR0..=CFG_BAR5 => {
                let state = self.state.lock().expect("state lock");
                state.cfg_read(offset, len)
            }

            // Capabilities pointer: return from cache.
            0x34 => {
                let val = u32::from(self.phys_cfg[CFG_CAP_PTR]);
                extract_bytes(val, offset, len)
            }

            // Everything else, the MSI capability included, reads the
            // real device. Reads have no side effect on the host.
            _ => self
                .ppt_cfg_read(u64::from(offset), u32::from(len))
                .unwrap_or(0xFFFF_FFFF),
        }
    }

    fn cfg_write(&self, offset: u8, len: u8, val: u32) {
        match classify_cfg_write(offset, len, &self.caps) {
            CfgWrite::Command => self.cfg_write_command(offset, len, val),
            CfgWrite::Bar => self.cfg_write_bar(offset, val),
            CfgWrite::MsiCap => self.cfg_write_msi_cap(offset, len, val),
            CfgWrite::InterruptLine => {
                let mut state = self.state.lock().expect("state lock");
                state.cfg_write(offset, len, val);
            }
            CfgWrite::Deny => {
                warn!(self.log, "passthru: config write dropped";
                    "offset" => format!("{offset:#04x}"),
                    "len" => len,
                    "value" => format!("{val:#010x}"));
            }
        }
    }

    /// Never called: nothing registers this device on a bus.
    ///
    /// MMIO BARs do not need it, because EPT maps them straight to
    /// hardware. A PIO BAR does, and until this device registers a PIO
    /// handler its ports decode nothing.
    fn bar_rw(&self, bar: BarN, offset: usize, rwo: RWOp<'_>) {
        let bar_idx = bar as usize;
        let bar_info = &self.bars[bar_idx];

        if bar_info.bar_type == PCI_ADDR_IO {
            match rwo {
                RWOp::Read(ro) => {
                    let val = self.bar_io_read(
                        bar_idx as u32,
                        offset as u32,
                        ro.len() as u32,
                    );
                    match ro.len() {
                        1 => ro.write_u8(val as u8),
                        2 => ro.write_u16(val as u16),
                        _ => ro.write_u32(val),
                    }
                }
                RWOp::Write(wo) => {
                    let data = match wo.len() {
                        1 => u32::from(wo.read_u8()),
                        2 => u32::from(wo.read_u16()),
                        _ => wo.read_u32(),
                    };
                    self.bar_io_write(
                        bar_idx as u32,
                        offset as u32,
                        wo.len() as u32,
                        data,
                    );
                }
            }
            return;
        }

        // An MMIO BAR reaching here is an unmapped region. An undecoded
        // PCI read returns all ones.
        if let RWOp::Read(ro) = rwo {
            match ro.len() {
                1 => ro.write_u8(0xFF),
                2 => ro.write_u16(0xFFFF),
                _ => ro.write_u32(0xFFFF_FFFF),
            }
        }
    }
}

// ── Config-space write handlers ────────────────────────────────────

impl PciPassthru {
    /// Merge a guest write into the command register.
    ///
    /// Bus mastering and the two decode enables must reach the device,
    /// so this is the one header register written through.
    fn cfg_write_command(&self, offset: u8, len: u8, val: u32) {
        let old_cmd = *self.command.lock().expect("command lock");
        let byte_off = u32::from(offset & 0x03);
        let mask = size_mask(len) << (byte_off * 8);
        let merged =
            (u32::from(old_cmd) & !mask) | ((val << (byte_off * 8)) & mask);
        let new_cmd = merged as u16;
        *self.command.lock().expect("command lock") = new_cmd;

        if let Err(e) =
            self.ppt_cfg_write(u64::from(CFG_COMMAND), 2, u32::from(new_cmd))
        {
            warn!(self.log, "passthru: command register write failed";
                "value" => format!("{new_cmd:#06x}"), "error" => %e);
        }

        self.update_bar_mappings(old_cmd, new_cmd);
    }

    /// Apply a guest BAR write to the emulated header, then remap.
    ///
    /// The value never reaches the hardware BAR. The device keeps the
    /// host physical address the platform gave it.
    fn cfg_write_bar(&self, offset: u8, val: u32) {
        let bar_idx = (offset - CFG_BAR0) / 4;
        let Ok(bar_n) = BarN::try_from(bar_idx) else {
            return;
        };

        let mut state = self.state.lock().expect("state lock");
        let Some(result) = state.bars_mut().reg_write(bar_n, val) else {
            return;
        };
        let idx = result.bar as usize;
        let new_addr = result.new_addr;
        let changed = result.old_addr != new_addr;
        let size = result.def.size();
        drop(state);

        if changed {
            self.handle_bar_remap(idx, new_addr, size);
        }
    }

    /// Write to the MSI capability and program the real MSI.
    ///
    /// The MSI Enable bit is masked out of the write-through. The
    /// kernel owns the physical MSI registers: `ppt_setup_msi` calls
    /// `ddi_intr_enable`, which puts a host address and a host vector
    /// there and sets Enable itself. Letting the guest set Enable would
    /// open a window in which the device could post to a guest-chosen
    /// host address, and illumos does not remap interrupts.
    fn cfg_write_msi_cap(&self, offset: u8, len: u8, val: u32) {
        let cap_off = self.caps.msi_cap_off;
        let ctrl_off = cap_off.wrapping_add(MSI_MSG_CTRL_OFF);

        // Track the guest's enable intent, then strip the bit.
        let mut forwarded = val;
        if offset <= ctrl_off && offset + len > ctrl_off {
            let shift = u32::from(ctrl_off - offset) * 8;
            let enable = u32::from(MSI_MSG_CTRL_ENABLE) << shift;
            *self.msi_enabled.lock().expect("msi_enabled lock") =
                (val & enable) != 0;
            forwarded &= !enable;
        }

        if let Err(e) =
            self.ppt_cfg_write(u64::from(offset), u32::from(len), forwarded)
        {
            warn!(self.log, "passthru: MSI capability write failed";
                "offset" => format!("{offset:#04x}"), "error" => %e);
            return;
        }

        let msg_ctrl =
            self.ppt_cfg_read(u64::from(ctrl_off), 2).unwrap_or(0) as u16;
        let is_64bit = (msg_ctrl & MSI_MSG_CTRL_64BIT) != 0;

        let addr_lo = self
            .ppt_cfg_read(u64::from(cap_off) + u64::from(MSI_MSG_ADDR_OFF), 4)
            .unwrap_or(0);

        let (addr, data) = if is_64bit {
            let addr_hi = self
                .ppt_cfg_read(
                    u64::from(cap_off) + u64::from(MSI_MSG_UPPER_ADDR_OFF),
                    4,
                )
                .unwrap_or(0);
            let data = self
                .ppt_cfg_read(
                    u64::from(cap_off) + u64::from(MSI_MSG_DATA_64_OFF),
                    2,
                )
                .unwrap_or(0);
            let full_addr = u64::from(addr_lo) | (u64::from(addr_hi) << 32);
            (full_addr, u64::from(data))
        } else {
            let data = self
                .ppt_cfg_read(
                    u64::from(cap_off) + u64::from(MSI_MSG_DATA_32_OFF),
                    2,
                )
                .unwrap_or(0);
            (u64::from(addr_lo), u64::from(data))
        };

        let enabled = *self.msi_enabled.lock().expect("msi_enabled lock");
        let numvec = msi_numvec(msg_ctrl, enabled, self.msi_limit);

        let mut msi_req = vm_pptdev_msi {
            vcpu: 0,
            pptfd: self.ppt_fd,
            numvec,
            msg: data,
            addr,
        };
        // Safety: vm_pptdev_msi is correct for VM_PPTDEV_MSI.
        let res = unsafe {
            ioctl_on_hdl(
                self.hdl.as_raw_fd(),
                ioctls::VM_PPTDEV_MSI,
                &mut msi_req,
            )
        };
        if let Err(e) = res {
            warn!(self.log, "passthru: MSI setup failed, device has no \
                interrupt";
                "numvec" => numvec, "error" => %e);
        }
    }
}

// ── Device capability layout ───────────────────────────────────────
// ── Bind guard and teardown ────────────────────────────────────────

/// Gives a bound PPT device back to the host if `new` does not finish.
///
/// Without this, an error after `VM_BIND_PPTDEV` leaks the descriptor
/// and leaves the device owned by a VM that never starts.
struct BoundPpt {
    fd: RawFd,
    hdl: Arc<VmmHdl>,
    armed: bool,
    log: Logger,
}

impl BoundPpt {
    /// Give up ownership once the device is built.
    fn release(mut self) -> RawFd {
        self.armed = false;
        self.fd
    }
}

impl Drop for BoundPpt {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        unbind_ppt(self.fd, &self.hdl, &self.log);
    }
}

/// Unbind a PPT device from the VM and close its descriptor.
fn unbind_ppt(fd: RawFd, hdl: &VmmHdl, log: &Logger) {
    let mut unbind = vm_pptdev { pptfd: fd };
    // Safety: vm_pptdev is correct for VM_UNBIND_PPTDEV.
    let res = unsafe {
        ioctl_on_hdl(hdl.as_raw_fd(), ioctls::VM_UNBIND_PPTDEV, &mut unbind)
    };
    if let Err(e) = res {
        // Nothing else can run here, so the record is the only remedy:
        // the device stays owned by the VM until the instance goes.
        warn!(log, "passthru: unbind failed, device still held";
            "error" => %e);
    }
    // Safety: fd is the descriptor opened in `new`.
    unsafe { libc::close(fd) };
}

impl Drop for PciPassthru {
    fn drop(&mut self) {
        let mapped = self.mapped_bars.lock().expect("mapped_bars lock");
        let windows: Vec<(u64, u64)> =
            mapped.iter().flatten().copied().collect();
        drop(mapped);
        for (gpa, len) in windows {
            self.report_unmap(gpa, len);
        }

        unbind_ppt(self.ppt_fd, &self.hdl, &self.log);
    }
}

// ── Ioctl helpers ──────────────────────────────────────────────────

/// Issue an ioctl on the VM fd.
///
/// # Safety
///
/// The caller must provide a `data` pointer to a struct that is correct
/// for the given `cmd` ioctl number.
unsafe fn ioctl_on_hdl<T>(fd: RawFd, cmd: i32, data: &mut T) -> Result<i32> {
    #[cfg(target_os = "illumos")]
    let ret = libc::ioctl(fd, cmd, data as *mut T as *mut libc::c_void);
    #[cfg(not(target_os = "illumos"))]
    let ret = libc::ioctl(
        fd,
        cmd as libc::c_ulong,
        data as *mut T as *mut libc::c_void,
    );
    if ret < 0 {
        Err(Error::last_os_error())
    } else {
        Ok(ret)
    }
}

/// Issue an ioctl on the PPT device fd.
///
/// # Safety
///
/// The caller must provide a `data` pointer to a struct that is correct
/// for the given `cmd` ioctl number.
unsafe fn ioctl_on_fd<T>(fd: RawFd, cmd: i32, data: &mut T) -> Result<i32> {
    #[cfg(target_os = "illumos")]
    let ret = libc::ioctl(fd, cmd, data as *mut T as *mut libc::c_void);
    #[cfg(not(target_os = "illumos"))]
    let ret = libc::ioctl(
        fd,
        cmd as libc::c_ulong,
        data as *mut T as *mut libc::c_void,
    );
    if ret < 0 {
        Err(Error::last_os_error())
    } else {
        Ok(ret)
    }
}

/// Extract bytes from a dword value based on the access offset and length.
fn extract_bytes(dword: u32, offset: u8, len: u8) -> u32 {
    let byte_off = (offset & 0x03) as u32;
    (dword >> (byte_off * 8)) & size_mask(len)
}

/// Build a bitmask for the given access width.
fn size_mask(len: u8) -> u32 {
    match len {
        1 => 0xFF,
        2 => 0xFFFF,
        4 => 0xFFFF_FFFF,
        _ => 0xFFFF_FFFF,
    }
}

#[cfg(test)]
mod tests;
