// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bootrom (UEFI firmware) loading.
//!
//! The bootrom is a UEFI firmware image (e.g., BHYVE_UEFI.fd) that is
//! mapped at the top of the 32-bit address space. The x86 reset vector
//! at physical address 0xFFFF_FFF0 falls within this region, so the
//! BSP executes firmware code immediately after activation.
//!
//! # Memory layout
//!
//! The ROM region is allocated at a fixed size (`ROM_REGION_SIZE`) and
//! the firmware image is loaded right-justified within it:
//!
//! ```text
//!   ROM_BASE (0xFFE0_0000)  ┌──────────────────┐
//!                           │   (padding/zero)  │
//!   ROM_BASE + offset       ├──────────────────┤
//!                           │  UEFI firmware    │
//!                           │  image data       │
//!   0xFFFF_FFF0             │  ← reset vector   │
//!   0xFFFF_FFFF             └──────────────────┘
//! ```
//!
//! # Security
//!
//! The bootrom file is read from the host filesystem with the
//! permissions of the VMM process. The file size is validated against
//! a maximum to prevent excessive memory allocation. The file is read
//! into a pre-allocated, pre-zeroed kernel segment — no guest-accessible
//! memory is exposed until the write completes.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context};

use vmm_core::mem::PhysMap;

/// Maximum size of the ROM region in the guest address space.
/// This is the entire region allocated at the top of 32-bit space.
/// Matches bhyve C: 16 MiB at 0xFF00_0000.
/// The UEFI firmware uses the space below the ROM file for variables
/// and SEC-phase temporary stack (Cache As RAM equivalent).
const ROM_REGION_SIZE: usize = 16 * 1024 * 1024;

/// GPA of the ROM region base.
const ROM_BASE_GPA: u64 = 0x1_0000_0000 - ROM_REGION_SIZE as u64;

/// Size of the SEC firmware temporary RAM region.
/// OVMF for bhyve uses memory below the ROM region for its PEI phase
/// stack and heap during early boot (before main RAM discovery).
/// We map 48 MiB of writable RAM from 0xFC000000 to 0xFEFFFFFF
/// (immediately below the 16 MiB ROM at 0xFF000000).
const SEC_TEMP_RAM_SIZE: usize = 4 * 1024 * 1024; // 4 MiB

/// GPA base of the SEC temp RAM.
const SEC_TEMP_RAM_GPA: u64 = 0xFCC0_0000;

/// Maximum firmware file size (16 MiB — well beyond any UEFI image).
const MAX_ROM_FILE_SIZE: usize = 16 * 1024 * 1024;

/// Minimum firmware file size (4 KiB — must be at least one page).
const MIN_ROM_FILE_SIZE: usize = 4096;

/// A validated bootrom file, ready to be loaded into guest memory.
///
/// Constructed via [`BootromFile::open`], which validates the file
/// exists, is readable, and has a valid size. This type guarantees
/// that loading will not fail due to file issues.
///
/// Follows the parse-don't-validate pattern: once you have a
/// `BootromFile`, it is guaranteed to be valid.
pub struct BootromFile {
    path: PathBuf,
    data: Vec<u8>,
}

impl BootromFile {
    /// Open and validate a bootrom file.
    ///
    /// Validates:
    /// - File exists and is readable
    /// - File size is within bounds (4 KiB to 16 MiB)
    /// - File size is page-aligned (multiple of 4096)
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let mut file = File::open(path)
            .with_context(|| format!("cannot open bootrom: {}", path.display()))?;

        let metadata = file.metadata()
            .with_context(|| format!("cannot stat bootrom: {}", path.display()))?;

        let size = metadata.len() as usize;

        ensure!(
            size >= MIN_ROM_FILE_SIZE,
            "bootrom {} is too small ({} bytes, minimum {})",
            path.display(), size, MIN_ROM_FILE_SIZE,
        );

        ensure!(
            size <= MAX_ROM_FILE_SIZE,
            "bootrom {} is too large ({} bytes, maximum {})",
            path.display(), size, MAX_ROM_FILE_SIZE,
        );

        ensure!(
            size % 4096 == 0,
            "bootrom {} size ({} bytes) is not page-aligned (must be multiple of 4096)",
            path.display(), size,
        );

        let mut data = Vec::with_capacity(size);
        file.read_to_end(&mut data)
            .with_context(|| format!("failed to read bootrom: {}", path.display()))?;

        ensure!(
            data.len() == size,
            "bootrom {} short read: expected {} bytes, got {}",
            path.display(), size, data.len(),
        );

        Ok(Self {
            path: path.to_path_buf(),
            data,
        })
    }

    /// Get the path of the bootrom file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Get the size of the firmware image in bytes.
    pub fn size(&self) -> usize {
        self.data.len()
    }
}

/// Load a bootrom into the guest physical address space.
///
/// This:
/// 1. Adds a ROM region to the PhysMap at the top of 32-bit space
/// 2. Writes the firmware image right-justified within the region
///    (so the reset vector at 0xFFFF_FFF0 is within the image)
///
/// Must be called before vCPUs are started.
pub fn load_bootrom(
    map: &mut PhysMap,
    hdl: &vmm_core::VmmHdl,
    rom: &BootromFile,
) -> anyhow::Result<()> {
    ensure!(
        rom.size() <= ROM_REGION_SIZE,
        "bootrom ({} bytes) exceeds ROM region size ({} bytes)",
        rom.size(), ROM_REGION_SIZE,
    );

    // Add SEC temp RAM below the ROM for firmware PEI-phase stack/heap.
    // This must be writable RAM — the firmware uses it for its temp
    // stack before discovering and initializing main memory.
    map.add_ram(hdl, SEC_TEMP_RAM_GPA, SEC_TEMP_RAM_SIZE)
        .context("failed to map SEC temp RAM region")?;

    // Add the ROM segment to the guest physical address space
    map.add_rom(hdl, ROM_BASE_GPA, ROM_REGION_SIZE)
        .context("failed to map ROM region into guest address space")?;

    // Write the firmware image right-justified within the ROM region.
    // The padding at the start is already zero (kernel-allocated segment).
    let offset = ROM_REGION_SIZE - rom.size();
    let rom_gpa = ROM_BASE_GPA + offset as u64;

    let sub = map.lookup(rom_gpa, rom.size())
        .ok_or_else(|| anyhow::anyhow!(
            "ROM region at GPA {:#x} len {} not found after mapping",
            rom_gpa, rom.size(),
        ))?;

    sub.write_bytes(&rom.data)
        .context("failed to write bootrom data into guest memory")?;

    Ok(())
}

/// Parse the bootrom path from the `-l` flags.
///
/// bhyve convention: `-l bootrom,/path/to/BHYVE_UEFI.fd`
///
/// Returns the path if found, or the default SmartOS path.
pub fn find_bootrom_path(lpc_args: &[String]) -> PathBuf {
    for arg in lpc_args {
        if let Some(path) = arg.strip_prefix("bootrom,") {
            let path = path.trim();
            // Handle bhyve symbolic names
            return match path {
                "uefi" => PathBuf::from("/usr/share/bhyve/uefi-rom.bin"),
                "bios" => PathBuf::from("/usr/share/bhyve/uefi-csm-rom.bin"),
                _ => PathBuf::from(path),
            };
        }
    }
    // Default: SmartOS UEFI ROM
    PathBuf::from("/usr/share/bhyve/uefi-rom.bin")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_bootrom_path() {
        let path = find_bootrom_path(&[]);
        assert_eq!(path, PathBuf::from("/usr/share/bhyve/uefi-rom.bin"));
    }

    #[test]
    fn explicit_bootrom_path() {
        let args = vec!["bootrom,/my/custom/rom.bin".to_string()];
        let path = find_bootrom_path(&args);
        assert_eq!(path, PathBuf::from("/my/custom/rom.bin"));
    }

    #[test]
    fn bootrom_symbolic_uefi() {
        let args = vec!["bootrom,uefi".to_string()];
        let path = find_bootrom_path(&args);
        assert_eq!(path, PathBuf::from("/usr/share/bhyve/uefi-rom.bin"));
    }

    #[test]
    fn bootrom_with_other_lpc_args() {
        let args = vec![
            "com1,/dev/zconsole".to_string(),
            "bootrom,/opt/fw/rom.bin".to_string(),
            "com2,socket,/tmp/vm.ttyb".to_string(),
        ];
        let path = find_bootrom_path(&args);
        assert_eq!(path, PathBuf::from("/opt/fw/rom.bin"));
    }
}
