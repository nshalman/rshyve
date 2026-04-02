// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Direct Linux kernel boot (Firecracker-style).
//!
//! Loads a Linux bzImage directly into guest RAM, sets up boot_params
//! (zero page), identity-mapped page tables, GDT, and configures the
//! BSP vCPU for 64-bit long mode entry — bypassing all UEFI firmware.
//!
//! # Guest memory layout
//!
//! ```text
//! 0x0000_0500  GDT (24 bytes: null + code64 + data64)
//! 0x0000_7000  boot_params / zero page (4 KiB)
//! 0x0000_9000  PML4 (4 KiB)
//! 0x0000_A000  PDPT (4 KiB)
//! 0x0000_B000  PD0..PD3 (16 KiB, covers 0-4GB)
//! 0x0002_0000  kernel command line (up to 4 KiB)
//! 0x0010_0000  kernel protected-mode code
//! high mem     initrd (page-aligned, below lowmem ceiling)
//! ```

use std::fs;
use std::path::Path;

use anyhow::{bail, ensure, Context, Result};

use vmm_core::mem::MemCtx;
use vmm_core::vcpu::Vcpu;
use vmm_devices::acpi;

// ── Guest physical addresses ────────────────────────────────────────

const GPA_GDT: u64 = 0x500;
const GPA_BOOT_PARAMS: u64 = 0x7000;
const GPA_PML4: u64 = 0x9000;
const GPA_PDPT: u64 = 0xA000;
const GPA_PD_BASE: u64 = 0xB000;
const GPA_CMDLINE: u64 = 0x2_0000;
const GPA_KERNEL: u64 = 0x10_0000;
const CMDLINE_MAX: usize = 4096;

// ── bzImage setup_header offsets ────────────────────────────────────

const SETUP_HEADER_OFFSET: usize = 0x1F1;
const HEADER_MAGIC_OFFSET: usize = 0x202;
const HEADER_MAGIC: u32 = 0x5372_6448; // "HdrS"
const VERSION_OFFSET: usize = 0x206;
const PREF_ADDRESS_OFFSET: usize = 0x258;
const XLOADFLAGS_OFFSET: usize = 0x236;
const XLF_KERNEL_64: u16 = 1;
const SETUP_HEADER_END: usize = 0x290;

// ── boot_params (zero page) offsets ────────────────────────────────

/// Byte at 0x1E8: number of E820 entries.
const BP_E820_ENTRIES: usize = 0x1E8;
/// Dword at 0x210: type_of_loader.
const BP_TYPE_OF_LOADER: usize = 0x210;
/// Dword at 0x218: ramdisk_image (initrd GPA).
const BP_RAMDISK_IMAGE: usize = 0x218;
/// Dword at 0x21C: ramdisk_size.
const BP_RAMDISK_SIZE: usize = 0x21C;
/// Dword at 0x228: cmd_line_ptr.
const BP_CMD_LINE_PTR: usize = 0x228;
/// Qword at 0x070: acpi_rsdp_addr (protocol >= 2.14).
const BP_ACPI_RSDP_ADDR: usize = 0x070;
/// E820 map starts at offset 0x2D0 in boot_params.
const BP_E820_MAP: usize = 0x2D0;
/// Each E820 entry in boot_params is 20 bytes.
const BP_E820_ENTRY_SIZE: usize = 20;
/// Max E820 entries the kernel accepts.
const BP_E820_MAX: usize = 128;

// ── Page table bits ────────────────────────────────────────────────

const PTE_PRESENT: u64 = 1 << 0;
const PTE_WRITABLE: u64 = 1 << 1;
const PTE_PAGE_SIZE: u64 = 1 << 7; // 2MB large page

// ── Parsed bzImage ──────────────────────────────────────────────────

/// A parsed Linux bzImage ready for direct loading.
pub struct KernelImage {
    /// Raw file contents.
    data: Vec<u8>,
    /// Number of 512-byte setup sectors (from setup_header).
    setup_sects: usize,
    /// Protocol version (e.g., 0x020F).
    version: u16,
    /// Preferred load address (typically 0x100_0000).
    pref_address: u64,
}

impl KernelImage {
    /// Open and validate a bzImage file.
    pub fn open(path: &Path) -> Result<Self> {
        let data = fs::read(path)
            .with_context(|| format!("failed to read kernel: {}", path.display()))?;

        ensure!(data.len() > SETUP_HEADER_END, "kernel too small to be a bzImage");

        // Verify "HdrS" magic
        let magic = u32::from_le_bytes(
            data[HEADER_MAGIC_OFFSET..HEADER_MAGIC_OFFSET + 4]
                .try_into()
                .unwrap(),
        );
        ensure!(
            magic == HEADER_MAGIC,
            "not a valid bzImage (missing HdrS magic at 0x202)"
        );

        let version = u16::from_le_bytes(
            data[VERSION_OFFSET..VERSION_OFFSET + 2]
                .try_into()
                .unwrap(),
        );
        ensure!(
            version >= 0x020C,
            "boot protocol {:#06x} too old (need >= 0x020C for 64-bit entry)",
            version,
        );

        // Check XLF_KERNEL_64 flag
        let xloadflags = u16::from_le_bytes(
            data[XLOADFLAGS_OFFSET..XLOADFLAGS_OFFSET + 2]
                .try_into()
                .unwrap(),
        );
        ensure!(
            xloadflags & XLF_KERNEL_64 != 0,
            "kernel does not support 64-bit entry (xloadflags={:#06x})",
            xloadflags,
        );

        let setup_sects = data[SETUP_HEADER_OFFSET] as usize;
        let setup_sects = if setup_sects == 0 { 4 } else { setup_sects };

        let pref_address = u64::from_le_bytes(
            data[PREF_ADDRESS_OFFSET..PREF_ADDRESS_OFFSET + 8]
                .try_into()
                .unwrap(),
        );

        Ok(Self {
            data,
            setup_sects,
            version,
            pref_address,
        })
    }

    /// 64-bit entry point address.
    pub fn entry_point_64(&self) -> u64 {
        self.pref_address + 0x200
    }

    /// Offset of protected-mode kernel in the file.
    fn kernel_offset(&self) -> usize {
        (self.setup_sects + 1) * 512
    }

    /// Protected-mode kernel bytes.
    fn kernel_bytes(&self) -> &[u8] {
        &self.data[self.kernel_offset()..]
    }

    /// Setup header bytes (to embed in boot_params).
    fn setup_header(&self) -> &[u8] {
        &self.data[SETUP_HEADER_OFFSET..SETUP_HEADER_END]
    }
}

/// A loaded initrd image.
pub struct InitrdImage {
    data: Vec<u8>,
}

impl InitrdImage {
    pub fn open(path: &Path) -> Result<Self> {
        let data = fs::read(path)
            .with_context(|| format!("failed to read initrd: {}", path.display()))?;
        ensure!(!data.is_empty(), "initrd is empty");
        Ok(Self { data })
    }
}

// ── Loading ─────────────────────────────────────────────────────────

/// Load kernel, initrd, boot_params, page tables, and GDT into guest memory.
///
/// Returns the kernel entry point address for BSP setup.
pub fn load_kernel(
    mem: &MemCtx,
    kernel: &KernelImage,
    cmdline: &str,
    initrd: Option<&InitrdImage>,
    mem_size: usize,
    acpi_rsdp_addr: u64,
) -> Result<u64> {
    // 1. Write protected-mode kernel at pref_address (typically 0x100_0000 = 16 MB)
    //    The 64-bit entry point is at pref_address + 0x200, so the kernel
    //    code must be physically at pref_address.
    let kernel_bytes = kernel.kernel_bytes();
    let kernel_gpa = kernel.pref_address;
    mem.write(kernel_gpa, kernel_bytes)
        .context("failed to write kernel to guest memory")?;

    // 2. Write kernel command line
    ensure!(cmdline.len() < CMDLINE_MAX, "cmdline too long (max {})", CMDLINE_MAX);
    let mut cmdline_buf = cmdline.as_bytes().to_vec();
    cmdline_buf.push(0); // NUL terminate
    mem.write(GPA_CMDLINE, &cmdline_buf)
        .context("failed to write cmdline")?;

    // 3. Place initrd at highest page-aligned address below lowmem limit
    let initrd_addr = if let Some(rd) = initrd {
        let lowmem_limit = (mem_size as u64).min(0xC000_0000);
        let rd_size = rd.data.len() as u64;
        let rd_addr = (lowmem_limit - rd_size) & !0xFFF; // page-align down
        ensure!(
            rd_addr >= kernel_gpa + kernel_bytes.len() as u64,
            "initrd overlaps kernel (need more RAM)"
        );
        mem.write(rd_addr, &rd.data)
            .context("failed to write initrd")?;
        Some((rd_addr, rd.data.len()))
    } else {
        None
    };

    // 4. Write page tables (identity-map 4 GB with 2 MB pages)
    write_page_tables(mem)?;

    // 5. Write GDT
    write_gdt(mem)?;

    // 6. Write boot_params (zero page) at 0x7000
    write_boot_params(
        mem, kernel, cmdline.len(), initrd_addr, mem_size, acpi_rsdp_addr,
    )?;

    Ok(kernel.entry_point_64())
}

/// Write identity-mapped page tables covering 0-4 GB.
///
/// Uses 2 MB large pages for minimal page count:
/// - 1 PML4 page at 0x9000
/// - 1 PDPT page at 0xA000
/// - 4 PD pages at 0xB000-0xEFFF (one per GB)
fn write_page_tables(mem: &MemCtx) -> Result<()> {
    // PML4[0] → PDPT
    let mut pml4 = [0u8; 4096];
    let pml4_entry = GPA_PDPT | PTE_PRESENT | PTE_WRITABLE;
    pml4[..8].copy_from_slice(&pml4_entry.to_le_bytes());
    mem.write(GPA_PML4, &pml4).context("PML4")?;

    // PDPT[0..3] → PD0..PD3
    let mut pdpt = [0u8; 4096];
    for i in 0..4u64 {
        let pd_addr = GPA_PD_BASE + i * 4096;
        let entry = pd_addr | PTE_PRESENT | PTE_WRITABLE;
        let off = (i as usize) * 8;
        pdpt[off..off + 8].copy_from_slice(&entry.to_le_bytes());
    }
    mem.write(GPA_PDPT, &pdpt).context("PDPT")?;

    // PD pages: 512 entries each, 2 MB per entry
    for gb in 0..4u64 {
        let mut pd = [0u8; 4096];
        for i in 0..512u64 {
            let phys = gb * (1 << 30) + i * (2 << 20);
            let entry = phys | PTE_PRESENT | PTE_WRITABLE | PTE_PAGE_SIZE;
            let off = (i as usize) * 8;
            pd[off..off + 8].copy_from_slice(&entry.to_le_bytes());
        }
        let gpa = GPA_PD_BASE + gb * 4096;
        mem.write(gpa, &pd).context("PD")?;
    }

    Ok(())
}

/// Write a minimal GDT: null + 64-bit code + 64-bit data.
fn write_gdt(mem: &MemCtx) -> Result<()> {
    let mut gdt = [0u8; 24];

    // Entry 0: null descriptor (all zeros)

    // Entry 1 (selector 0x08): 64-bit code segment
    // Base=0, Limit=0xFFFFF, Type=0x0B (exec/read/accessed),
    // S=1, DPL=0, P=1, L=1, G=1
    let code64: u64 = 0x00AF_9B00_0000_FFFF;
    gdt[8..16].copy_from_slice(&code64.to_le_bytes());

    // Entry 2 (selector 0x10): 64-bit data segment
    // Base=0, Limit=0xFFFFF, Type=0x03 (read/write/accessed),
    // S=1, DPL=0, P=1, DB=1, G=1
    let data64: u64 = 0x00CF_9300_0000_FFFF;
    gdt[16..24].copy_from_slice(&data64.to_le_bytes());

    mem.write(GPA_GDT, &gdt).context("GDT")?;
    Ok(())
}

/// Assemble and write the boot_params zero page.
fn write_boot_params(
    mem: &MemCtx,
    kernel: &KernelImage,
    cmdline_len: usize,
    initrd: Option<(u64, usize)>,
    mem_size: usize,
    acpi_rsdp_addr: u64,
) -> Result<()> {
    let mut bp = [0u8; 4096];

    // Copy setup_header from bzImage (preserves kernel's own fields)
    let hdr = kernel.setup_header();
    let hdr_dest = SETUP_HEADER_OFFSET;
    bp[hdr_dest..hdr_dest + hdr.len()].copy_from_slice(hdr);

    // type_of_loader = 0xFF (undefined bootloader)
    bp[BP_TYPE_OF_LOADER] = 0xFF;

    // cmd_line_ptr
    bp[BP_CMD_LINE_PTR..BP_CMD_LINE_PTR + 4]
        .copy_from_slice(&(GPA_CMDLINE as u32).to_le_bytes());

    // initrd
    if let Some((addr, size)) = initrd {
        bp[BP_RAMDISK_IMAGE..BP_RAMDISK_IMAGE + 4]
            .copy_from_slice(&(addr as u32).to_le_bytes());
        bp[BP_RAMDISK_SIZE..BP_RAMDISK_SIZE + 4]
            .copy_from_slice(&(size as u32).to_le_bytes());
    }

    // acpi_rsdp_addr (protocol >= 2.14 = 0x020E)
    if kernel.version >= 0x020E && acpi_rsdp_addr != 0 {
        bp[BP_ACPI_RSDP_ADDR..BP_ACPI_RSDP_ADDR + 8]
            .copy_from_slice(&acpi_rsdp_addr.to_le_bytes());
    }

    // E820 memory map
    let entries = acpi::build_e820_entries(mem_size);
    let count = entries.len().min(BP_E820_MAX);
    bp[BP_E820_ENTRIES] = count as u8;
    for (i, entry) in entries.iter().take(count).enumerate() {
        let off = BP_E820_MAP + i * BP_E820_ENTRY_SIZE;
        bp[off..off + 8].copy_from_slice(&entry.addr.to_le_bytes());
        bp[off + 8..off + 16].copy_from_slice(&entry.size.to_le_bytes());
        bp[off + 16..off + 20].copy_from_slice(&entry.type_.to_le_bytes());
    }

    mem.write(GPA_BOOT_PARAMS, &bp).context("boot_params")?;
    Ok(())
}

// ── BSP setup ───────────────────────────────────────────────────────

/// Configure vCPU 0 for 64-bit long mode entry into the kernel.
pub fn setup_direct_boot_bsp(vcpu: &Vcpu, entry_point: u64) -> Result<()> {
    use bhyve_api::{
        seg_desc, vm_reg_name::*,
        SEG_ACCESS_P, SEG_ACCESS_S, SEG_ACCESS_L, SEG_ACCESS_DB, SEG_ACCESS_G,
        SEG_ACCESS_UNUSABLE,
    };

    vcpu.reboot_state()
        .context("reboot_state")?;

    // Control registers for 64-bit long mode
    vcpu.set_reg(VM_REG_GUEST_CR0, 0x8000_0031)?;  // PG | ET | NE | PE
    vcpu.set_reg(VM_REG_GUEST_CR3, GPA_PML4)?;      // PML4 address
    vcpu.set_reg(VM_REG_GUEST_CR4, 0x20)?;           // PAE
    vcpu.set_reg(VM_REG_GUEST_EFER, 0x500)?;         // LMA | LME

    // 64-bit code segment (selector 0x08)
    let code64_access = SEG_ACCESS_P | SEG_ACCESS_S | SEG_ACCESS_L | SEG_ACCESS_G | 0x0B;
    vcpu.set_segment_desc(VM_REG_GUEST_CS, &seg_desc {
        base: 0,
        limit: 0xFFFF_FFFF,
        access: code64_access,
    })?;
    vcpu.set_reg(VM_REG_GUEST_CS, 0x08)?;

    // Data segments (selector 0x10)
    let data_access = SEG_ACCESS_P | SEG_ACCESS_S | SEG_ACCESS_DB | SEG_ACCESS_G | 0x03;
    let data_desc = seg_desc {
        base: 0,
        limit: 0xFFFF_FFFF,
        access: data_access,
    };
    for reg in [
        VM_REG_GUEST_DS, VM_REG_GUEST_ES, VM_REG_GUEST_FS,
        VM_REG_GUEST_GS, VM_REG_GUEST_SS,
    ] {
        vcpu.set_segment_desc(reg, &data_desc)?;
        vcpu.set_reg(reg, 0x10)?;
    }

    // GDTR
    vcpu.set_segment_desc(VM_REG_GUEST_GDTR, &seg_desc {
        base: GPA_GDT,
        limit: 23, // 3 entries * 8 bytes - 1
        access: 0,
    })?;

    // TR (busy 64-bit TSS) — VMX requires a valid, present TR
    vcpu.set_segment_desc(VM_REG_GUEST_TR, &seg_desc {
        base: 0,
        limit: 0xFFFF,
        access: SEG_ACCESS_P | 0x0B, // type=busy 64-bit TSS, present
    })?;
    vcpu.set_reg(VM_REG_GUEST_TR, 0)?;

    // LDTR — mark unusable (VMX requires UNUSABLE bit when not loaded)
    vcpu.set_segment_desc(VM_REG_GUEST_LDTR, &seg_desc {
        base: 0,
        limit: 0,
        access: SEG_ACCESS_UNUSABLE,
    })?;
    vcpu.set_reg(VM_REG_GUEST_LDTR, 0)?;

    // IDTR — no IDT initially (kernel will set it up)
    vcpu.set_segment_desc(VM_REG_GUEST_IDTR, &seg_desc {
        base: 0,
        limit: 0xFFFF,
        access: 0,
    })?;

    // Entry point and boot protocol registers
    vcpu.set_reg(VM_REG_GUEST_RIP, entry_point)?;
    vcpu.set_reg(VM_REG_GUEST_RSI, GPA_BOOT_PARAMS)?; // boot_params pointer
    vcpu.set_reg(VM_REG_GUEST_RFLAGS, 0x02)?;
    vcpu.set_reg(VM_REG_GUEST_RSP, 0)?;
    vcpu.set_reg(VM_REG_GUEST_RBP, 0)?;
    vcpu.set_reg(VM_REG_GUEST_RDI, 0)?;
    vcpu.set_reg(VM_REG_GUEST_RBX, 0)?;
    vcpu.set_reg(VM_REG_GUEST_RCX, 0)?;
    vcpu.set_reg(VM_REG_GUEST_RDX, 0)?;
    vcpu.set_reg(VM_REG_GUEST_RAX, 0)?;
    vcpu.set_reg(VM_REG_GUEST_R8, 0)?;
    vcpu.set_reg(VM_REG_GUEST_R9, 0)?;
    vcpu.set_reg(VM_REG_GUEST_R10, 0)?;
    vcpu.set_reg(VM_REG_GUEST_R11, 0)?;
    vcpu.set_reg(VM_REG_GUEST_R12, 0)?;
    vcpu.set_reg(VM_REG_GUEST_R13, 0)?;
    vcpu.set_reg(VM_REG_GUEST_R14, 0)?;
    vcpu.set_reg(VM_REG_GUEST_R15, 0)?;

    // Mark runnable
    vcpu.set_run_state(bhyve_api::VRS_RUN)?;

    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_table_structure() {
        // Verify page table byte layout
        let mut pml4 = [0u8; 4096];
        let entry = GPA_PDPT | PTE_PRESENT | PTE_WRITABLE;
        pml4[..8].copy_from_slice(&entry.to_le_bytes());
        let read_back = u64::from_le_bytes(pml4[..8].try_into().unwrap());
        assert_eq!(read_back & !0xFFF, GPA_PDPT);
        assert_ne!(read_back & PTE_PRESENT, 0);
        assert_ne!(read_back & PTE_WRITABLE, 0);
    }

    #[test]
    fn gdt_entries() {
        let mut gdt = [0u8; 24];
        let code64: u64 = 0x00AF_9B00_0000_FFFF;
        let data64: u64 = 0x00CF_9300_0000_FFFF;
        gdt[8..16].copy_from_slice(&code64.to_le_bytes());
        gdt[16..24].copy_from_slice(&data64.to_le_bytes());

        // Null descriptor
        assert_eq!(&gdt[..8], &[0u8; 8]);
        // Code64: check L bit (bit 53) is set, DB bit (bit 54) is clear
        assert_ne!(code64 & (1 << 53), 0, "L bit must be set for 64-bit code");
        assert_eq!(code64 & (1 << 54), 0, "DB bit must be clear for 64-bit code");
        // Data64: check DB bit is set
        assert_ne!(data64 & (1 << 54), 0, "DB bit must be set for data segment");
    }

    #[test]
    fn boot_params_e820_layout() {
        // E820 entries start at offset 0x2D0, each is 20 bytes
        assert_eq!(BP_E820_MAP, 0x2D0);
        assert_eq!(BP_E820_ENTRY_SIZE, 20);
        // With 4 entries: 0x2D0 + 4*20 = 0x2D0 + 80 = 0x320
        // Must fit in 4096-byte page
        assert!(BP_E820_MAP + BP_E820_MAX * BP_E820_ENTRY_SIZE <= 4096);
    }

    #[test]
    fn cmdline_max_fits_in_page() {
        assert!(CMDLINE_MAX <= 4096);
    }

    #[test]
    fn guest_addresses_dont_overlap() {
        // GDT at 0x500 (24 bytes) doesn't overlap boot_params at 0x7000
        assert!(GPA_GDT + 24 < GPA_BOOT_PARAMS);
        // boot_params at 0x7000 (4096) doesn't overlap PML4 at 0x9000
        assert!(GPA_BOOT_PARAMS + 4096 <= GPA_PML4);
        // Page tables: PML4(4K) + PDPT(4K) + 4*PD(4K) = 24K, 0x9000..0xF000
        assert!(GPA_PML4 + 6 * 4096 <= GPA_CMDLINE);
        // Cmdline at 0x20000 doesn't overlap kernel at 0x100000
        assert!(GPA_CMDLINE + CMDLINE_MAX as u64 <= GPA_KERNEL);
    }
}
