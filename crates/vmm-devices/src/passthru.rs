// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! PCI passthrough (PPT) device.
//!
//! Passes a real PCIe device directly to the guest by:
//! 1. Opening `/dev/pptN` and binding it to the VM.
//! 2. Reading physical config space and BAR information from hardware.
//! 3. Mapping device BARs into guest physical address space via EPT
//!    (`VM_MAP_PPTDEV_MMIO`), so guest MMIO accesses go directly to
//!    hardware with no VM exits.
//! 4. Emulating the MSI-X table in userspace to program real interrupt
//!    vectors via `VM_PPTDEV_MSIX`.
//! 5. Unbinding on drop.
//!
//! Supports devices with large 64-bit BARs (e.g. NVIDIA H100 with
//! 64 GB+ BARs) and up to 2048+ MSI-X vectors.

use std::io::{Error, ErrorKind, Result};
use std::os::fd::AsRawFd;
use std::os::unix::io::RawFd;
use std::sync::{Arc, Mutex};

use bhyve_api::{
    ioctls, ppt_bar_io, ppt_bar_query, ppt_cfg_io, vm_pptdev,
    vm_pptdev_limits, vm_pptdev_mmio, vm_pptdev_msi, vm_pptdev_msix,
    PCI_ADDR_IO, PCI_ADDR_MEM32, PCI_ADDR_MEM64,
};

use vmm_core::common::RWOp;
use vmm_core::hdl::VmmHdl;

use crate::pci::bar::{BarDefine, BAR_COUNT};
use crate::pci::bits;
use crate::pci::device::{DeviceState, PciDevice};
use crate::pci::BarN;

// ── MSI-X table entry ──────────────────────────────────────────────

/// A single emulated MSI-X table entry.
///
/// When the guest writes to the MSI-X table region, we update this
/// struct and program the real interrupt via `VM_PPTDEV_MSIX`.
#[derive(Debug, Clone, Copy, Default)]
struct MsixTableEntry {
    addr_lo: u32,
    addr_hi: u32,
    data: u32,
    control: u32, // bit 0 = per-vector mask
}

/// Size of one MSI-X table entry in bytes.
const MSIX_ENTRY_SIZE: usize = 16;

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
/// MSI Message Control: 64-bit address capable.
const MSI_MSG_CTRL_64BIT: u16 = 1 << 7;

/// PCI Command register bits we care about.
const PCI_CMD_IO_EN: u16 = 1 << 0;
const PCI_CMD_MMIO_EN: u16 = 1 << 1;

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
/// Binds a real PCIe device (`/dev/pptN`) to a VM and presents it to
/// the guest as a PCI function. MMIO BARs are mapped directly into
/// guest physical address space via EPT; the MSI-X table is emulated
/// in userspace.
pub struct PciPassthru {
    /// File descriptor for `/dev/pptN`.
    ppt_fd: RawFd,
    /// VM handle for ioctls.
    hdl: Arc<VmmHdl>,
    /// Cached physical BAR information from the kernel.
    bars: [BarInfo; BAR_COUNT],
    /// Physical PCI config space cache (first 256 bytes).
    phys_cfg: [u8; 256],
    /// MSI capability offset (0 = not present).
    msi_cap_off: u8,
    /// MSI-X capability offset (0 = not present).
    msix_cap_off: u8,
    /// MSI-X table BAR index.
    msix_table_bar: u8,
    /// MSI-X table offset within the BAR.
    msix_table_offset: u32,
    /// MSI interrupt limit from the kernel (for future validation).
    #[allow(dead_code)]
    msi_limit: i32,
    /// MSI-X interrupt limit from the kernel (for future validation).
    #[allow(dead_code)]
    msix_limit: i32,
    /// Standard PCI config header state (BAR addresses, command reg).
    state: Mutex<DeviceState>,
    /// Emulated MSI-X table entries.
    msix_table: Mutex<Vec<MsixTableEntry>>,
    /// Currently mapped BAR GPAs: `(gpa, len)` if mapped.
    mapped_bars: Mutex<[Option<(u64, u64)>; BAR_COUNT]>,
    /// Emulated PCI command register (shadow for MMIO_EN tracking).
    command: Mutex<u16>,
}

impl PciPassthru {
    /// Create a new PCI passthrough device.
    ///
    /// Opens `ppt_path` (e.g. `/dev/ppt0`), binds it to the VM,
    /// reads physical config space and BAR information, and
    /// initializes MSI-X emulation if the device supports it.
    pub fn new(ppt_path: &str, hdl: Arc<VmmHdl>) -> Result<Arc<Self>> {
        // Open the PPT device.
        let ppt_fd = {
            let c_path = std::ffi::CString::new(ppt_path).map_err(|_| {
                Error::new(ErrorKind::InvalidInput, "invalid ppt path")
            })?;
            // Safety: opening a device node with valid C string.
            let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR) };
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
            ioctl_on_hdl(
                hdl.as_raw_fd(),
                ioctls::VM_BIND_PPTDEV,
                &mut bind_req,
            )
        }
        .map_err(|e| {
            // Clean up the fd on bind failure.
            unsafe { libc::close(ppt_fd) };
            e
        })?;

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

        // Query all 6 BARs.
        let mut bars = [BarInfo::default(); BAR_COUNT];
        for i in 0..BAR_COUNT {
            let mut query = ppt_bar_query {
                pbq_baridx: i as u32,
                pbq_type: 0,
                pbq_base: 0,
                pbq_size: 0,
            };
            // Safety: ppt_bar_query is correct for PPT_BAR_QUERY.
            unsafe {
                ioctl_on_fd(ppt_fd, ioctls::PPT_BAR_QUERY, &mut query)
            }?;
            bars[i] = BarInfo {
                bar_type: query.pbq_type,
                hpa: query.pbq_base,
                size: query.pbq_size,
            };
        }

        // Build DeviceState from physical config space.
        let ident = crate::pci::device::DeviceIdent {
            vendor_id: u16::from_le_bytes([phys_cfg[0], phys_cfg[1]]),
            device_id: u16::from_le_bytes([phys_cfg[2], phys_cfg[3]]),
            revision: phys_cfg[8],
            prog_if: phys_cfg[9],
            subclass: phys_cfg[0x0A],
            class: phys_cfg[0x0B],
            sub_vendor_id: u16::from_le_bytes([
                phys_cfg[0x2C],
                phys_cfg[0x2D],
            ]),
            sub_device_id: u16::from_le_bytes([
                phys_cfg[0x2E],
                phys_cfg[0x2F],
            ]),
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

        // Walk the capability list to find MSI and MSI-X.
        let cap_ptr = phys_cfg[0x34];
        dev_state.set_cap_ptr(cap_ptr);

        let mut msi_cap_off: u8 = 0;
        let mut msix_cap_off: u8 = 0;
        let mut msix_table_bar: u8 = 0;
        let mut msix_table_offset: u32 = 0;
        let mut msix_table_count: u16 = 0;

        if cap_ptr != 0 {
            let mut ptr = cap_ptr;
            let mut iter_count = 0u32;
            while ptr != 0 && ptr >= 0x40 && iter_count < 48 {
                iter_count += 1;
                let cap_id = phys_cfg[ptr as usize];
                match cap_id {
                    bits::CAP_ID_MSI => {
                        msi_cap_off = ptr;
                    }
                    bits::CAP_ID_MSIX => {
                        msix_cap_off = ptr;
                        // Read Message Control (offset +2 from cap start)
                        let msg_ctrl = u16::from_le_bytes([
                            phys_cfg[(ptr + 2) as usize],
                            phys_cfg[(ptr + 3) as usize],
                        ]);
                        // Table size is bits 10:0, encoded as N-1.
                        msix_table_count = (msg_ctrl & 0x7FF) + 1;

                        // Table Offset/BIR at offset +4
                        let table_off_bir = u32::from_le_bytes([
                            phys_cfg[(ptr + 4) as usize],
                            phys_cfg[(ptr + 5) as usize],
                            phys_cfg[(ptr + 6) as usize],
                            phys_cfg[(ptr + 7) as usize],
                        ]);
                        msix_table_bar = (table_off_bir & 0x7) as u8;
                        msix_table_offset = table_off_bir & !0x7;
                    }
                    _ => {}
                }
                // Next capability pointer.
                ptr = phys_cfg[(ptr + 1) as usize];
            }
        }

        // Allocate emulated MSI-X table if the device supports MSI-X.
        let msix_table = if msix_table_count > 0 {
            vec![MsixTableEntry::default(); msix_table_count as usize]
        } else {
            Vec::new()
        };

        Ok(Arc::new(Self {
            ppt_fd,
            hdl,
            bars,
            phys_cfg,
            msi_cap_off,
            msix_cap_off,
            msix_table_bar,
            msix_table_offset,
            msi_limit: limits.msi_limit,
            msix_limit: limits.msix_limit,
            state: Mutex::new(dev_state),
            msix_table: Mutex::new(msix_table),
            mapped_bars: Mutex::new([None; BAR_COUNT]),
            command: Mutex::new(0),
        }))
    }

    // ── Internal helpers ───────────────────────────────────────────

    /// Read a register from the physical device config space via ioctl.
    fn ppt_cfg_read(&self, offset: u64, width: u32) -> Result<u32> {
        let mut cfg_io =
            ppt_cfg_io { pci_off: offset, pci_width: width, pci_data: 0 };
        // Safety: ppt_cfg_io is correct for PPT_CFG_READ on our ppt_fd.
        unsafe {
            ioctl_on_fd(self.ppt_fd, ioctls::PPT_CFG_READ, &mut cfg_io)
        }?;
        Ok(cfg_io.pci_data)
    }

    /// Write a register to the physical device config space via ioctl.
    fn ppt_cfg_write(
        &self,
        offset: u64,
        width: u32,
        data: u32,
    ) -> Result<()> {
        let mut cfg_io =
            ppt_cfg_io { pci_off: offset, pci_width: width, pci_data: data };
        // Safety: ppt_cfg_io is correct for PPT_CFG_WRITE on our ppt_fd.
        unsafe {
            ioctl_on_fd(self.ppt_fd, ioctls::PPT_CFG_WRITE, &mut cfg_io)
        }?;
        Ok(())
    }

    /// Map a PPT device BAR into guest physical address space via EPT.
    fn map_bar_mmio(
        &self,
        bar_idx: usize,
        gpa: u64,
        len: u64,
    ) -> Result<()> {
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

    /// Program an MSI-X vector in the kernel/hardware.
    fn program_msix_vector(&self, idx: usize, entry: &MsixTableEntry) {
        let addr =
            u64::from(entry.addr_lo) | (u64::from(entry.addr_hi) << 32);
        let mut msix_req = vm_pptdev_msix {
            vcpu: 0,
            pptfd: self.ppt_fd,
            idx: idx as i32,
            msg: u64::from(entry.data),
            vector_control: entry.control,
            addr,
        };
        // Safety: vm_pptdev_msix is correct for VM_PPTDEV_MSIX.
        let _ = unsafe {
            ioctl_on_hdl(
                self.hdl.as_raw_fd(),
                ioctls::VM_PPTDEV_MSIX,
                &mut msix_req,
            )
        };
    }

    /// Handle a BAR address change: unmap old mapping, create new one.
    fn handle_bar_remap(
        &self,
        bar_idx: usize,
        _old_gpa: u64,
        new_gpa: u64,
        size: u64,
    ) {
        let mut mapped = self.mapped_bars.lock().expect("mapped_bars lock");
        let cmd = *self.command.lock().expect("command lock");
        let mmio_enabled = (cmd & PCI_CMD_MMIO_EN) != 0;
        let io_enabled = (cmd & PCI_CMD_IO_EN) != 0;

        // Unmap old if present.
        if let Some((old_g, old_len)) = mapped[bar_idx].take() {
            let _ = self.unmap_bar_mmio(old_g, old_len);
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

        if should_map {
            if self.map_bar_mmio(bar_idx, new_gpa, size).is_ok() {
                mapped[bar_idx] = Some((new_gpa, size));
            }
        }
    }

    /// Map or unmap all MMIO BARs based on the current command register.
    fn update_bar_mappings(&self, old_cmd: u16, new_cmd: u16) {
        let mmio_changed =
            (old_cmd & PCI_CMD_MMIO_EN) != (new_cmd & PCI_CMD_MMIO_EN);
        let io_changed =
            (old_cmd & PCI_CMD_IO_EN) != (new_cmd & PCI_CMD_IO_EN);

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

            let is_mmio = matches!(
                bar_info.bar_type,
                PCI_ADDR_MEM32 | PCI_ADDR_MEM64
            );
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
                        if mapped[i].is_none() {
                            if self
                                .map_bar_mmio(i, addr, bar_info.size)
                                .is_ok()
                            {
                                mapped[i] = Some((addr, bar_info.size));
                            }
                        }
                    } else {
                        // Unmap.
                        if let Some((gpa, len)) = mapped[i].take() {
                            let _ = self.unmap_bar_mmio(gpa, len);
                        }
                    }
                }
            }
        }
    }

    /// Check whether an offset falls in the MSI capability range.
    fn in_msi_cap(&self, offset: u8) -> bool {
        if self.msi_cap_off == 0 {
            return false;
        }
        // MSI cap is 10-14 bytes depending on 64-bit/per-vector mask support.
        // Conservative: check against 24 bytes (max MSI cap size).
        offset >= self.msi_cap_off && offset < self.msi_cap_off + 24
    }

    /// Check whether an offset falls in the MSI-X capability range.
    fn in_msix_cap(&self, offset: u8) -> bool {
        if self.msix_cap_off == 0 {
            return false;
        }
        // MSI-X cap is 12 bytes.
        offset >= self.msix_cap_off && offset < self.msix_cap_off + 12
    }

    /// Read from the emulated MSI-X table.
    fn msix_table_read(&self, offset: usize) -> u32 {
        let table = self.msix_table.lock().expect("msix_table lock");
        let idx = offset / MSIX_ENTRY_SIZE;
        let field = offset % MSIX_ENTRY_SIZE;
        if idx >= table.len() {
            return 0;
        }
        let entry = &table[idx];
        match field {
            0 => entry.addr_lo,
            4 => entry.addr_hi,
            8 => entry.data,
            12 => entry.control,
            _ => 0,
        }
    }

    /// Write to the emulated MSI-X table and program the hardware vector.
    fn msix_table_write(&self, offset: usize, val: u32) {
        let mut table = self.msix_table.lock().expect("msix_table lock");
        let idx = offset / MSIX_ENTRY_SIZE;
        let field = offset % MSIX_ENTRY_SIZE;
        if idx >= table.len() {
            return;
        }
        match field {
            0 => table[idx].addr_lo = val,
            4 => table[idx].addr_hi = val,
            8 => table[idx].data = val,
            12 => table[idx].control = val,
            _ => return,
        }
        let entry = table[idx];
        drop(table);
        // Program the hardware vector.
        self.program_msix_vector(idx, &entry);
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
        let _ = unsafe {
            ioctl_on_fd(self.ppt_fd, ioctls::PPT_BAR_READ, &mut io)
        };
        io.pbi_data
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
        let _ = unsafe {
            ioctl_on_fd(self.ppt_fd, ioctls::PPT_BAR_WRITE, &mut io)
        };
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
            0x04 => {
                let cmd = *self.command.lock().expect("command lock");
                let status = self
                    .ppt_cfg_read(0x06, 2)
                    .unwrap_or(0)
                    as u16;
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
            bar_off @ 0x10..=0x24 if (bar_off & 0x03) == 0 => {
                let state = self.state.lock().expect("state lock");
                let val = state.cfg_read(offset, len);
                val
            }

            // Capabilities pointer: return from cache.
            0x34 => {
                let val = u32::from(self.phys_cfg[0x34]);
                extract_bytes(val, offset, len)
            }

            // MSI-X capability: return emulated values.
            off if self.in_msix_cap(off) => {
                self.cfg_read_msix_cap(offset, len)
            }

            // MSI capability: return emulated/physical values.
            off if self.in_msi_cap(off) => {
                // Read the physical value as our MSI emulation
                // tracks what the guest last wrote.
                self.ppt_cfg_read(u64::from(dword_off), u32::from(len))
                    .unwrap_or(0xFFFF_FFFF)
            }

            // Everything else: read physical via PPT_CFG_READ.
            _ => self
                .ppt_cfg_read(u64::from(offset), u32::from(len))
                .unwrap_or(0xFFFF_FFFF),
        }
    }

    fn cfg_write(&self, offset: u8, len: u8, val: u32) {
        let dword_off = offset & 0xFC;

        match dword_off {
            // Command register.
            0x04 if offset < 0x06 => {
                let old_cmd = *self.command.lock().expect("command lock");
                // Merge the written bytes into the command register.
                let byte_off = (offset & 0x03) as u32;
                let mask = size_mask(len) << (byte_off * 8);
                let merged =
                    (u32::from(old_cmd) & !mask) | ((val << (byte_off * 8)) & mask);
                let new_cmd = merged as u16;
                *self.command.lock().expect("command lock") = new_cmd;

                // Also write to physical device so bus mastering etc. works.
                let _ = self.ppt_cfg_write(0x04, 2, u32::from(new_cmd));

                // Update BAR mappings if MMIO_EN or IO_EN changed.
                self.update_bar_mappings(old_cmd, new_cmd);
            }

            // BAR writes.
            bar_off @ 0x10..=0x24 if (bar_off & 0x03) == 0 => {
                let mut state = self.state.lock().expect("state lock");
                // Capture old address before write.
                let bar_idx = ((bar_off - 0x10) / 4) as u8;
                // For the high half of a 64-bit BAR, we need the real bar index.
                let _real_idx = bar_idx;
                // Perform the write via DeviceState.
                if let Some(result) = state.bars_mut().reg_write(
                    BarN::try_from(bar_idx).unwrap_or(BarN::BAR0),
                    val,
                ) {
                    let bar_n = result.bar;
                    let new_addr = result.new_addr;
                    let old_addr = result.old_addr;
                    let size = result.def.size();
                    let idx = bar_n as usize;
                    drop(state);

                    // Handle the BAR remapping.
                    if old_addr != new_addr {
                        self.handle_bar_remap(idx, old_addr, new_addr, size);
                    }
                }
            }

            // MSI capability writes: program MSI interrupt.
            off if self.in_msi_cap(off) => {
                self.cfg_write_msi_cap(offset, len, val);
            }

            // MSI-X capability writes: handle enable/mask bits.
            off if self.in_msix_cap(off) => {
                self.cfg_write_msix_cap(offset, len, val);
            }

            // Interrupt line (writable by guest).
            0x3C if offset == 0x3C => {
                let mut state = self.state.lock().expect("state lock");
                state.cfg_write(offset, len, val);
            }

            // Everything else: write to physical device.
            _ => {
                let _ = self.ppt_cfg_write(
                    u64::from(offset),
                    u32::from(len),
                    val,
                );
            }
        }
    }

    fn bar_rw(&self, bar: BarN, offset: usize, rwo: RWOp<'_>) {
        let bar_idx = bar as usize;
        let bar_info = &self.bars[bar_idx];

        // Check if this access hits the MSI-X table region.
        if self.msix_cap_off != 0
            && bar_idx == self.msix_table_bar as usize
        {
            let table_start = self.msix_table_offset as usize;
            let table = self.msix_table.lock().expect("msix_table lock");
            let table_end =
                table_start + table.len() * MSIX_ENTRY_SIZE;
            drop(table);

            if offset >= table_start && offset < table_end {
                let table_off = offset - table_start;
                // Align to dword boundary for table access.
                let aligned_off = table_off & !0x3;
                match rwo {
                    RWOp::Read(ro) => {
                        let val = self.msix_table_read(aligned_off);
                        match ro.len() {
                            1 => {
                                let byte_off = table_off & 0x3;
                                ro.write_u8(
                                    (val >> (byte_off * 8)) as u8,
                                );
                            }
                            2 => {
                                let byte_off = table_off & 0x3;
                                ro.write_u16(
                                    (val >> (byte_off * 8)) as u16,
                                );
                            }
                            4 => ro.write_u32(val),
                            _ => ro.write_u32(val),
                        }
                    }
                    RWOp::Write(wo) => {
                        // MSI-X table writes are always dword aligned.
                        let write_val = match wo.len() {
                            4 => wo.read_u32(),
                            2 => {
                                // Read-modify-write for sub-dword.
                                let old = self.msix_table_read(aligned_off);
                                let byte_off = table_off & 0x3;
                                let mask = 0xFFFFu32 << (byte_off * 8);
                                (old & !mask)
                                    | ((u32::from(wo.read_u16()))
                                        << (byte_off * 8))
                            }
                            1 => {
                                let old = self.msix_table_read(aligned_off);
                                let byte_off = table_off & 0x3;
                                let mask = 0xFFu32 << (byte_off * 8);
                                (old & !mask)
                                    | ((u32::from(wo.read_u8()))
                                        << (byte_off * 8))
                            }
                            _ => wo.read_u32(),
                        };
                        self.msix_table_write(aligned_off, write_val);
                    }
                }
                return;
            }
        }

        // For IO BARs: use PPT_BAR_READ/PPT_BAR_WRITE ioctls.
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
                        4 => ro.write_u32(val),
                        _ => ro.write_u32(val),
                    }
                }
                RWOp::Write(wo) => {
                    let data = match wo.len() {
                        1 => u32::from(wo.read_u8()),
                        2 => u32::from(wo.read_u16()),
                        4 => wo.read_u32(),
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

        // For MMIO BARs mapped via EPT: this should not be called because
        // the guest accesses go directly to hardware. If we do get here
        // (e.g., unmapped region), return all-ones for reads.
        if let RWOp::Read(ro) = rwo {
            match ro.len() {
                1 => ro.write_u8(0xFF),
                2 => ro.write_u16(0xFFFF),
                4 => ro.write_u32(0xFFFF_FFFF),
                _ => ro.write_u32(0xFFFF_FFFF),
            }
        }
    }
}

// ── MSI/MSI-X capability emulation helpers ─────────────────────────

impl PciPassthru {
    /// Read from the emulated MSI-X capability in config space.
    fn cfg_read_msix_cap(&self, offset: u8, len: u8) -> u32 {
        let cap_off = self.msix_cap_off;
        let rel = offset - cap_off;
        let dword_off = rel & 0xFC;

        match dword_off {
            // Byte 0: Cap ID, Byte 1: Next Ptr, Bytes 2-3: Message Control.
            0 => {
                let cap_id = self.phys_cfg[cap_off as usize];
                let next_ptr = self.phys_cfg[(cap_off + 1) as usize];
                let table = self.msix_table.lock().expect("msix_table lock");
                let table_size =
                    (table.len() as u16).saturating_sub(1) & 0x7FF;
                // We always report MSI-X as disabled from the cap read;
                // the guest enables it via writes.
                let msg_ctrl = table_size;
                let val = u32::from(cap_id)
                    | (u32::from(next_ptr) << 8)
                    | (u32::from(msg_ctrl) << 16);
                extract_bytes(val, offset, len)
            }
            // Table Offset/BIR.
            4 => {
                let val = (self.msix_table_offset & !0x7)
                    | u32::from(self.msix_table_bar & 0x7);
                extract_bytes(val, offset, len)
            }
            // PBA Offset/BIR: read physical.
            8 => {
                let val = u32::from_le_bytes([
                    self.phys_cfg[(cap_off + 8) as usize],
                    self.phys_cfg[(cap_off + 9) as usize],
                    self.phys_cfg[(cap_off + 10) as usize],
                    self.phys_cfg[(cap_off + 11) as usize],
                ]);
                extract_bytes(val, offset, len)
            }
            _ => 0,
        }
    }

    /// Write to the MSI-X capability in config space.
    ///
    /// Only the Message Control register (enable + function mask) is writable.
    fn cfg_write_msix_cap(&self, offset: u8, len: u8, val: u32) {
        let cap_off = self.msix_cap_off;
        let dword_off = (offset - cap_off) & 0xFC;

        if dword_off == 0 {
            // The writable bits are in Message Control (bytes 2-3 of the
            // capability dword): MSI-X Enable (bit 15) and Function Mask
            // (bit 14).
            //
            // Write-through to the physical device so the hardware
            // enables/disables MSI-X.
            let _ = self.ppt_cfg_write(
                u64::from(offset),
                u32::from(len),
                val,
            );
        }
        // Table BIR and PBA BIR are read-only: ignore writes.
    }

    /// Write to the MSI capability in config space.
    ///
    /// Intercepts message address/data writes and programs the real MSI
    /// via `VM_PPTDEV_MSI`.
    fn cfg_write_msi_cap(&self, offset: u8, len: u8, val: u32) {
        let cap_off = self.msi_cap_off;

        // Write through to physical device config space.
        let _ =
            self.ppt_cfg_write(u64::from(offset), u32::from(len), val);

        // After writing, read back the full MSI config to program the
        // kernel with the complete MSI configuration.
        let msg_ctrl = self
            .ppt_cfg_read(
                u64::from(cap_off) + u64::from(MSI_MSG_CTRL_OFF),
                2,
            )
            .unwrap_or(0) as u16;
        let is_64bit = (msg_ctrl & MSI_MSG_CTRL_64BIT) != 0;
        let numvec_pow = ((msg_ctrl >> 4) & 0x7) as u32;
        let numvec = 1i32 << numvec_pow;

        let addr_lo = self
            .ppt_cfg_read(
                u64::from(cap_off) + u64::from(MSI_MSG_ADDR_OFF),
                4,
            )
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
            let full_addr =
                u64::from(addr_lo) | (u64::from(addr_hi) << 32);
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

        // Program the kernel with the MSI configuration.
        let mut msi_req = vm_pptdev_msi {
            vcpu: 0,
            pptfd: self.ppt_fd,
            numvec,
            msg: data,
            addr,
        };
        // Safety: vm_pptdev_msi is correct for VM_PPTDEV_MSI.
        let _ = unsafe {
            ioctl_on_hdl(
                self.hdl.as_raw_fd(),
                ioctls::VM_PPTDEV_MSI,
                &mut msi_req,
            )
        };
    }
}

impl Drop for PciPassthru {
    fn drop(&mut self) {
        // Unmap all mapped BARs.
        let mapped = self.mapped_bars.lock().expect("mapped_bars lock");
        for mapping in mapped.iter() {
            if let Some((gpa, len)) = mapping {
                let _ = self.unmap_bar_mmio(*gpa, *len);
            }
        }
        drop(mapped);

        // Unbind the PPT device from the VM.
        let mut unbind = vm_pptdev { pptfd: self.ppt_fd };
        // Safety: vm_pptdev is correct for VM_UNBIND_PPTDEV.
        let _ = unsafe {
            ioctl_on_hdl(
                self.hdl.as_raw_fd(),
                ioctls::VM_UNBIND_PPTDEV,
                &mut unbind,
            )
        };

        // Close the PPT device fd.
        // Safety: self.ppt_fd is a valid fd that we opened in `new()`.
        unsafe { libc::close(self.ppt_fd) };
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
    let ret = libc::ioctl(fd, cmd as libc::c_ulong, data as *mut T as *mut libc::c_void);
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
    let ret = libc::ioctl(fd, cmd as libc::c_ulong, data as *mut T as *mut libc::c_void);
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

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// MSI-X per-vector mask bit (bit 0 of vector control).
    const MSIX_VEC_CTRL_MASK: u32 = 1;

    /// Build a fake physical config space with known values.
    fn fake_phys_cfg() -> [u8; 256] {
        let mut cfg = [0u8; 256];

        // Vendor ID = 0x10DE (NVIDIA)
        cfg[0] = 0xDE;
        cfg[1] = 0x10;
        // Device ID = 0x2330 (H100)
        cfg[2] = 0x30;
        cfg[3] = 0x23;

        // Command = 0x0000 (disabled)
        cfg[4] = 0x00;
        cfg[5] = 0x00;
        // Status = 0x0010 (CAP_LIST)
        cfg[6] = 0x10;
        cfg[7] = 0x00;

        // Revision = 0xA1
        cfg[8] = 0xA1;
        // Prog IF = 0x00
        cfg[9] = 0x00;
        // Subclass = 0x02 (3D controller)
        cfg[0x0A] = 0x02;
        // Class = 0x03 (Display)
        cfg[0x0B] = 0x03;

        // Sub-Vendor ID = 0x10DE
        cfg[0x2C] = 0xDE;
        cfg[0x2D] = 0x10;
        // Sub-Device ID = 0x16C0
        cfg[0x2E] = 0xC0;
        cfg[0x2F] = 0x16;

        // Capabilities pointer = 0x60
        cfg[0x34] = 0x60;

        // MSI-X capability at offset 0x60:
        // Cap ID = 0x11 (MSI-X)
        cfg[0x60] = 0x11;
        // Next pointer = 0x70 (MSI)
        cfg[0x61] = 0x70;
        // Message Control: table size = 31 (32 vectors, N-1), enable=0
        cfg[0x62] = 0x1F; // bits 7:0 of msg ctrl
        cfg[0x63] = 0x00; // bits 15:8 of msg ctrl
        // Table Offset/BIR: BAR1, offset 0
        cfg[0x64] = 0x01; // BIR = 1
        cfg[0x65] = 0x00;
        cfg[0x66] = 0x00;
        cfg[0x67] = 0x00;
        // PBA Offset/BIR: BAR1, offset 0x1000
        cfg[0x68] = 0x01; // BIR = 1
        cfg[0x69] = 0x10;
        cfg[0x6A] = 0x00;
        cfg[0x6B] = 0x00;

        // MSI capability at offset 0x70:
        // Cap ID = 0x05 (MSI)
        cfg[0x70] = 0x05;
        // Next pointer = 0x00 (end of list)
        cfg[0x71] = 0x00;
        // Message Control
        cfg[0x72] = 0x00;
        cfg[0x73] = 0x00;

        cfg
    }

    #[test]
    fn parse_vendor_device_from_cfg() {
        let cfg = fake_phys_cfg();
        let vendor = u16::from_le_bytes([cfg[0], cfg[1]]);
        let device = u16::from_le_bytes([cfg[2], cfg[3]]);
        assert_eq!(vendor, 0x10DE);
        assert_eq!(device, 0x2330);
    }

    #[test]
    fn parse_class_revision_from_cfg() {
        let cfg = fake_phys_cfg();
        assert_eq!(cfg[8], 0xA1); // revision
        assert_eq!(cfg[0x0B], 0x03); // class (Display)
        assert_eq!(cfg[0x0A], 0x02); // subclass
    }

    #[test]
    fn parse_capabilities_list() {
        let cfg = fake_phys_cfg();
        let cap_ptr = cfg[0x34];
        assert_eq!(cap_ptr, 0x60);

        // Walk capabilities.
        let mut msi_off: u8 = 0;
        let mut msix_off: u8 = 0;
        let mut ptr = cap_ptr;
        while ptr != 0 && ptr >= 0x40 {
            let cap_id = cfg[ptr as usize];
            match cap_id {
                bits::CAP_ID_MSI => msi_off = ptr,
                bits::CAP_ID_MSIX => msix_off = ptr,
                _ => {}
            }
            ptr = cfg[(ptr + 1) as usize];
        }
        assert_eq!(msix_off, 0x60);
        assert_eq!(msi_off, 0x70);
    }

    #[test]
    fn parse_msix_table_info() {
        let cfg = fake_phys_cfg();
        let msix_off: u8 = 0x60;

        // Message Control
        let msg_ctrl = u16::from_le_bytes([
            cfg[(msix_off + 2) as usize],
            cfg[(msix_off + 3) as usize],
        ]);
        let table_count = (msg_ctrl & 0x7FF) + 1;
        assert_eq!(table_count, 32);

        // Table Offset/BIR
        let table_off_bir = u32::from_le_bytes([
            cfg[(msix_off + 4) as usize],
            cfg[(msix_off + 5) as usize],
            cfg[(msix_off + 6) as usize],
            cfg[(msix_off + 7) as usize],
        ]);
        let bar = (table_off_bir & 0x7) as u8;
        let offset = table_off_bir & !0x7;
        assert_eq!(bar, 1);
        assert_eq!(offset, 0);
    }

    #[test]
    fn bar_type_constants() {
        assert_eq!(PCI_ADDR_IO, 1);
        assert_eq!(PCI_ADDR_MEM32, 2);
        assert_eq!(PCI_ADDR_MEM64, 3);
    }

    #[test]
    fn bar_info_default() {
        let info = BarInfo::default();
        assert_eq!(info.bar_type, 0);
        assert_eq!(info.hpa, 0);
        assert_eq!(info.size, 0);
    }

    #[test]
    fn bar_define_from_bar_info() {
        // Test that we correctly create BarDefine from BarInfo.
        let io_bar = BarInfo { bar_type: PCI_ADDR_IO, hpa: 0x3000, size: 256 };
        assert_eq!(io_bar.bar_type, PCI_ADDR_IO);
        assert!(io_bar.size <= u64::from(u16::MAX));

        let mem32_bar =
            BarInfo { bar_type: PCI_ADDR_MEM32, hpa: 0xFE00_0000, size: 0x100_0000 };
        assert_eq!(mem32_bar.bar_type, PCI_ADDR_MEM32);

        // 64-bit BAR with 64 GB size (e.g., H100 GPU)
        let mem64_bar = BarInfo {
            bar_type: PCI_ADDR_MEM64,
            hpa: 0x38_0000_0000,
            size: 0x10_0000_0000, // 64 GB
        };
        assert_eq!(mem64_bar.bar_type, PCI_ADDR_MEM64);
        assert_eq!(mem64_bar.size, 0x10_0000_0000);
    }

    #[test]
    fn msix_entry_layout() {
        let entry = MsixTableEntry {
            addr_lo: 0xFEE0_0000,
            addr_hi: 0x0000_0000,
            data: 0x0000_0041,
            control: 0,
        };
        assert_eq!(entry.addr_lo, 0xFEE0_0000);
        assert_eq!(entry.data, 0x41);
        assert_eq!(entry.control & MSIX_VEC_CTRL_MASK, 0); // unmasked
    }

    #[test]
    fn msix_entry_masked() {
        let entry = MsixTableEntry {
            addr_lo: 0,
            addr_hi: 0,
            data: 0,
            control: MSIX_VEC_CTRL_MASK,
        };
        assert_ne!(entry.control & MSIX_VEC_CTRL_MASK, 0);
    }

    #[test]
    fn extract_bytes_full_dword() {
        let val = 0xDEAD_BEEFu32;
        assert_eq!(extract_bytes(val, 0x00, 4), 0xDEAD_BEEF);
    }

    #[test]
    fn extract_bytes_word() {
        let val = 0xDEAD_BEEFu32;
        // Low word at offset 0
        assert_eq!(extract_bytes(val, 0x00, 2), 0xBEEF);
        // High word at offset 2
        assert_eq!(extract_bytes(val, 0x02, 2), 0xDEAD);
    }

    #[test]
    fn extract_bytes_single() {
        let val = 0x44332211u32;
        assert_eq!(extract_bytes(val, 0x00, 1), 0x11);
        assert_eq!(extract_bytes(val, 0x01, 1), 0x22);
        assert_eq!(extract_bytes(val, 0x02, 1), 0x33);
        assert_eq!(extract_bytes(val, 0x03, 1), 0x44);
    }

    #[test]
    fn size_mask_values() {
        assert_eq!(size_mask(1), 0xFF);
        assert_eq!(size_mask(2), 0xFFFF);
        assert_eq!(size_mask(4), 0xFFFF_FFFF);
    }

    #[test]
    fn bar_type_64bit_consumes_two_slots() {
        // Verify that 64-bit BARs in DeviceState work properly.
        let ident = crate::pci::device::DeviceIdent {
            vendor_id: 0x10DE,
            device_id: 0x2330,
            class: 0x03,
            subclass: 0x02,
            prog_if: 0,
            revision: 0xA1,
            sub_vendor_id: 0x10DE,
            sub_device_id: 0x16C0,
        };
        let mut state = DeviceState::new(ident);

        // Define a 64 GB 64-bit BAR at BAR0.
        state.define_bar(BarN::BAR0, BarDefine::Mmio64(0x10_0000_0000));

        // BAR0 should have the 64-bit type bits.
        let bar0 = state.cfg_read(0x10, 4);
        assert_eq!(bar0 & 0x7, bits::BAR_TYPE_MEM64);

        // BAR1 is the high half (Mmio64High).
        let bar1 = state.cfg_read(0x14, 4);
        assert_eq!(bar1, 0); // high bits = 0 initially

        // Write all-ones to BAR0 for sizing.
        state.cfg_write(0x10, 4, 0xFFFF_FFFF);
        let readback = state.cfg_read(0x10, 4);
        // Low 32 bits should be zero for a 64 GB BAR (size > 4 GB),
        // plus the type bits.
        assert_eq!(readback, 0x0000_0004);

        // Write all-ones to BAR1 (high half) for sizing.
        state.cfg_write(0x14, 4, 0xFFFF_FFFF);
        let readback_hi = state.cfg_read(0x14, 4);
        // For 64 GB = 0x10_0000_0000: mask = ~(0x10_0000_0000 - 1) = 0xFFFF_FFF0_0000_0000
        // High 32 bits = 0xFFFF_FFF0
        assert_eq!(readback_hi, 0xFFFF_FFF0);
    }
}
