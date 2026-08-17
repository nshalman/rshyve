// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bootrom (UEFI firmware) loading.
//!
//! The bootrom is a UEFI firmware image (e.g., BHYVE_UEFI.fd) mapped so
//! that its final byte is at the top of the 32-bit address space. The x86
//! reset vector at physical address 0xFFFF_FFF0 falls within this region,
//! so the BSP executes firmware code immediately after activation.
//!
//! # Memory layout
//!
//! Only the firmware image is mapped as ROM. Space reserved for an optional
//! variable store immediately below the ROM remains unmapped so flash accesses
//! can be emulated separately.
//!
//! ```text
//!   var_gpa                 ┌──────────────────┐
//!                           │ variable store   │  unmapped
//!   rom_gpa                 ├──────────────────┤
//!                           │ UEFI firmware    │  read/execute
//!   0xFFFF_FFF0             │ ← reset vector   │
//!   0xFFFF_FFFF             └──────────────────┘
//! ```
//!
//! # Security
//!
//! The bootrom file is read from the host filesystem with the
//! permissions of the VMM process. The file size is validated against
//! a maximum to prevent excessive memory allocation. The file is read
//! into a pre-allocated, pre-zeroed kernel segment; no guest-accessible
//! memory is exposed until the write completes.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context};
use slog::{debug, info, warn, Logger};

use vmm_core::{mem::PhysMap, VmmHdl};

/// End of the 32-bit guest physical address space.
pub const GUEST_4GIB: u64 = 0x1_0000_0000;

/// Maximum aggregate size of the boot ROM and variable store.
pub const BOOTROM_WINDOW_MAX: usize = 16 * 1024 * 1024;

/// Size of the SEC firmware temporary RAM region.
/// OVMF for bhyve uses memory below the ROM region for its PEI phase
/// stack and heap during early boot (before main RAM discovery).
/// We map 48 MiB of writable RAM from 0xFC000000 to 0xFEFFFFFF
/// (immediately below the 16 MiB ROM at 0xFF000000).
const SEC_TEMP_RAM_SIZE: usize = 4 * 1024 * 1024; // 4 MiB

/// GPA base of the SEC temp RAM.
const SEC_TEMP_RAM_GPA: u64 = 0xFCC0_0000;

/// Maximum firmware file size (16 MiB, well beyond any UEFI image).
const MAX_ROM_FILE_SIZE: usize = BOOTROM_WINDOW_MAX;

/// Minimum firmware file size (4 KiB; must be at least one page).
const MIN_ROM_FILE_SIZE: usize = 4096;

/// Guest physical layout of the firmware flash window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootromLayout {
    pub rom_gpa: u64,
    pub rom_size: usize,
    pub var_gpa: Option<u64>,
    pub var_size: usize,
}

/// Compute the firmware flash layout at the top of the 32-bit address space.
pub fn compute_layout(
    rom_size: usize,
    var_size: usize,
) -> anyhow::Result<BootromLayout> {
    ensure!(rom_size != 0, "bootrom size must be non-zero");
    ensure!(
        rom_size.is_multiple_of(4096),
        "bootrom size ({rom_size} bytes) is not page-aligned"
    );
    ensure!(
        rom_size <= BOOTROM_WINDOW_MAX,
        "bootrom size ({rom_size} bytes) exceeds flash window maximum ({BOOTROM_WINDOW_MAX} bytes)"
    );
    ensure!(
        var_size == 0 || var_size >= 4096,
        "variable store size ({var_size} bytes) is smaller than one page"
    );
    ensure!(
        var_size.is_multiple_of(4096),
        "variable store size ({var_size} bytes) is not page-aligned"
    );
    ensure!(
        var_size <= BOOTROM_WINDOW_MAX,
        "variable store size ({var_size} bytes) exceeds flash window maximum ({BOOTROM_WINDOW_MAX} bytes)"
    );

    let total_size = rom_size
        .checked_add(var_size)
        .context("bootrom and variable store aggregate size overflow")?;
    ensure!(
        total_size <= BOOTROM_WINDOW_MAX,
        "bootrom and variable store aggregate size ({total_size} bytes) exceeds flash window maximum ({BOOTROM_WINDOW_MAX} bytes)"
    );

    let rom_gpa = GUEST_4GIB - rom_size as u64;
    let var_gpa = (var_size != 0).then_some(rom_gpa - var_size as u64);

    Ok(BootromLayout {
        rom_gpa,
        rom_size,
        var_gpa,
        var_size,
    })
}

/// Check whether a ROM and variable store resemble a matched firmware pair.
pub fn flash_window_looks_consistent(rom_size: usize, var_size: usize) -> bool {
    if var_size == 0 {
        return true;
    }

    let Some(total_size) = rom_size.checked_add(var_size) else {
        return false;
    };
    let Ok(total_size) = u64::try_from(total_size) else {
        return false;
    };

    total_size <= GUEST_4GIB
        && total_size.is_power_of_two()
        && (GUEST_4GIB - total_size).is_multiple_of(total_size)
}

/// Log the flash layout and flag firmware files that appear mismatched.
pub(super) fn log_flash_layout(
    rom_path: &Path,
    var_path: Option<&Path>,
    layout: &BootromLayout,
    log: &Logger,
) {
    let rom_path = rom_path.display().to_string();
    let var_path = var_path
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "<none>".to_string());
    let rom_gpa = format!("{:#x}", layout.rom_gpa);
    let var_gpa = layout
        .var_gpa
        .map(|gpa| format!("{gpa:#x}"))
        .unwrap_or_else(|| "<none>".to_string());

    info!(log, "bootrom flash layout";
        "rom_path" => rom_path.clone(),
        "rom_size" => layout.rom_size,
        "rom_gpa" => rom_gpa.clone(),
        "vars_path" => var_path.clone(),
        "vars_size" => layout.var_size,
        "vars_gpa" => var_gpa.clone(),
    );
    if !flash_window_looks_consistent(layout.rom_size, layout.var_size) {
        warn!(log, "bootrom CODE and VARS files are probably from different builds";
            "rom_path" => rom_path,
            "rom_size" => layout.rom_size,
            "rom_gpa" => rom_gpa,
            "vars_path" => var_path,
            "vars_size" => layout.var_size,
            "vars_gpa" => var_gpa,
        );
    }
}

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
        let mut file = File::open(path).with_context(|| {
            format!("cannot open bootrom: {}", path.display())
        })?;

        let metadata = file.metadata().with_context(|| {
            format!("cannot stat bootrom: {}", path.display())
        })?;

        let size = metadata.len() as usize;

        ensure!(
            size >= MIN_ROM_FILE_SIZE,
            "bootrom {} is too small ({} bytes, minimum {})",
            path.display(),
            size,
            MIN_ROM_FILE_SIZE,
        );

        ensure!(
            size <= MAX_ROM_FILE_SIZE,
            "bootrom {} is too large ({} bytes, maximum {})",
            path.display(),
            size,
            MAX_ROM_FILE_SIZE,
        );

        ensure!(
            size.is_multiple_of(4096),
            "bootrom {} size ({} bytes) is not page-aligned (must be multiple of 4096)",
            path.display(), size,
        );

        let mut data = Vec::with_capacity(size);
        file.read_to_end(&mut data).with_context(|| {
            format!("failed to read bootrom: {}", path.display())
        })?;

        ensure!(
            data.len() == size,
            "bootrom {} short read: expected {} bytes, got {}",
            path.display(),
            size,
            data.len(),
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

/// Load a bootrom into its exact-size guest physical mapping.
///
/// The optional variable-store range below the ROM remains unmapped so its
/// accesses can be handled as emulated flash instead of read-only memory.
pub fn load_bootrom(
    map: &mut PhysMap,
    hdl: &VmmHdl,
    rom: &BootromFile,
    layout: &BootromLayout,
) -> anyhow::Result<()> {
    ensure!(
        rom.size() == layout.rom_size,
        "bootrom size ({} bytes) does not match layout size ({} bytes)",
        rom.size(),
        layout.rom_size,
    );

    // Add SEC temp RAM below the ROM for firmware PEI-phase stack/heap.
    // This must be writable RAM — the firmware uses it for its temp
    // stack before discovering and initializing main memory.
    map.add_ram(hdl, SEC_TEMP_RAM_GPA, SEC_TEMP_RAM_SIZE)
        .context("failed to map SEC temp RAM region")?;

    map.add_rom(hdl, layout.rom_gpa, layout.rom_size)
        .context("failed to map ROM region into guest address space")?;

    let sub = map.lookup(layout.rom_gpa, layout.rom_size).ok_or_else(|| {
        anyhow::anyhow!(
            "ROM region at GPA {:#x} len {} not found after mapping",
            layout.rom_gpa,
            layout.rom_size,
        )
    })?;

    sub.write_bytes(&rom.data)
        .context("failed to write bootrom data into guest memory")?;

    Ok(())
}

/// ROM and optional variable-store paths parsed from a bootrom LPC option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootromSpec {
    /// Firmware ROM image path.
    pub rom: PathBuf,
    /// Persistent firmware variable-store path.
    pub vars: Option<PathBuf>,
}

/// Default SmartOS UEFI firmware image.
pub const DEFAULT_UEFI_ROM: &str = "/usr/share/bhyve/uefi-rom.bin";
/// Default SmartOS compatibility-support firmware image.
pub const DEFAULT_CSM_ROM: &str = "/usr/share/bhyve/uefi-csm-rom.bin";

pub(super) fn should_warn_missing_tcg2(resolved_rom: &Path) -> bool {
    // WHY: TCG2 modules live in an LZMA-compressed firmware volume, so a byte
    // scan would produce false negatives. Comparing the resolved path with the
    // known default is the honest heuristic available here.
    resolved_rom == Path::new(DEFAULT_UEFI_ROM)
}

fn bootrom_remainder(arg: &str) -> Option<&str> {
    let (device, remainder) = arg.split_once(',').unwrap_or((arg, ""));
    device.eq_ignore_ascii_case("bootrom").then_some(remainder)
}

/// Parse the ROM and optional variable-store paths from the `-l` flags.
pub fn find_bootrom_spec(lpc_args: &[String]) -> anyhow::Result<BootromSpec> {
    let Some((arg, remainder)) = lpc_args.iter().find_map(|arg| {
        bootrom_remainder(arg).map(|remainder| (arg, remainder))
    }) else {
        return Ok(BootromSpec {
            rom: DEFAULT_UEFI_ROM.into(),
            vars: None,
        });
    };

    let mut fields = remainder.split(',').map(str::trim);
    let rom = fields.next().unwrap_or_default();
    if rom.is_empty() {
        bail!("invalid bootrom option \"{arg}\": missing rom path");
    }

    let rom = match rom {
        "uefi" => DEFAULT_UEFI_ROM.into(),
        "bios" => DEFAULT_CSM_ROM.into(),
        path => PathBuf::from(path),
    };
    let vars = match fields.next() {
        Some(field) if !field.is_empty() && !field.contains('=') => {
            Some(PathBuf::from(field))
        }
        _ => None,
    };

    Ok(BootromSpec { rom, vars })
}

/// Emit diagnostics for accepted but ignored bootrom fields.
pub(super) fn log_bootrom_diagnostics(lpc_args: &[String], log: &Logger) {
    let mut bootrom_args = lpc_args.iter().filter_map(|arg| {
        bootrom_remainder(arg).map(|remainder| (arg, remainder))
    });
    let Some((_arg, remainder)) = bootrom_args.next() else {
        return;
    };

    if let Some((arg, _remainder)) = bootrom_args.next() {
        warn!(log, "additional bootrom option ignored";
            "option" => arg.as_str());
    }

    let mut fields = remainder.split(',').map(str::trim);
    let _rom = fields.next();
    if let Some(field) = fields.next() {
        if !field.is_empty() && field.contains('=') {
            debug!(log, "legacy bootrom LPC option ignored";
                "option" => field);
        }
    }
    if fields.next().is_some() {
        warn!(log, "extra bootrom fields ignored");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_illumos_rom_only() {
        let layout = compute_layout(0x1E0000, 0).unwrap();
        assert_eq!(layout.rom_gpa, 0xFFE20000);
        assert_eq!(layout.rom_size, 0x1E0000);
        assert_eq!(layout.var_gpa, None);
        assert_eq!(layout.var_size, 0);
    }

    #[test]
    fn layout_illumos_pair() {
        let layout = compute_layout(0x1E0000, 0x20000).unwrap();
        assert_eq!(layout.rom_gpa, 0xFFE20000);
        assert_eq!(layout.var_gpa, Some(0xFFE00000));
        assert!(flash_window_looks_consistent(
            layout.rom_size,
            layout.var_size,
        ));
    }

    #[test]
    fn layout_freebsd_pair() {
        let layout = compute_layout(0x37C000, 0x84000).unwrap();
        assert_eq!(layout.rom_gpa, 0xFFC84000);
        assert_eq!(layout.var_gpa, Some(0xFFC00000));
        assert!(flash_window_looks_consistent(
            layout.rom_size,
            layout.var_size,
        ));
    }

    #[test]
    fn layout_csm_rom() {
        let layout = compute_layout(0x100000, 0).unwrap();
        assert_eq!(layout.rom_gpa, 0xFFF00000);
        assert_eq!(layout.var_gpa, None);
    }

    #[test]
    fn layout_flags_mismatched_varstore() {
        let layout = compute_layout(0x1E0000, 0x40000).unwrap();
        assert_eq!(layout.rom_gpa, 0xFFE20000);
        assert_eq!(layout.var_gpa, Some(0xFFDE0000));
        assert!(!flash_window_looks_consistent(
            layout.rom_size,
            layout.var_size,
        ));
    }

    #[test]
    fn layout_rejects_unaligned_rom() {
        assert!(compute_layout(0x1E0001, 0).is_err());
    }

    #[test]
    fn layout_rejects_unaligned_vars() {
        assert!(compute_layout(0x1E0000, 0x20001).is_err());
    }

    #[test]
    fn layout_rejects_tiny_vars() {
        assert!(compute_layout(0x1E0000, 2048).is_err());
    }

    #[test]
    fn layout_rejects_aggregate_over_window() {
        assert!(compute_layout(0xF00000, 0x200000).is_err());
    }

    #[test]
    fn layout_rejects_zero_rom() {
        assert!(compute_layout(0, 0).is_err());
    }

    #[test]
    fn rom_region_ends_at_4gib() {
        for (rom_size, var_size) in [
            (0x1E0000, 0),
            (0x1E0000, 0x20000),
            (0x37C000, 0x84000),
            (0x100000, 0),
            (0x1E0000, 0x40000),
        ] {
            let layout = compute_layout(rom_size, var_size).unwrap();
            assert_eq!(layout.rom_gpa + layout.rom_size as u64, GUEST_4GIB,);
        }
    }

    #[test]
    fn default_bootrom_path() {
        let spec = find_bootrom_spec(&[]).unwrap();
        assert_eq!(
            spec,
            BootromSpec {
                rom: PathBuf::from(DEFAULT_UEFI_ROM),
                vars: None,
            }
        );
    }

    #[test]
    fn missing_tcg2_warning_only_for_resolved_default_rom() {
        assert!(should_warn_missing_tcg2(Path::new(DEFAULT_UEFI_ROM)));
        assert!(!should_warn_missing_tcg2(Path::new(
            "/opt/fw/BHYVE_UEFI_CODE.fd"
        )));
    }

    #[test]
    fn explicit_bootrom_path() {
        let args = vec!["bootrom,/my/custom/rom.bin".to_string()];
        let spec = find_bootrom_spec(&args).unwrap();
        assert_eq!(spec.rom, PathBuf::from("/my/custom/rom.bin"));
        assert_eq!(spec.vars, None);
    }

    #[test]
    fn bootrom_symbolic_uefi() {
        let args = vec!["bootrom,uefi".to_string()];
        let spec = find_bootrom_spec(&args).unwrap();
        assert_eq!(spec.rom, PathBuf::from(DEFAULT_UEFI_ROM));
        assert_eq!(spec.vars, None);
    }

    #[test]
    fn bootrom_with_other_lpc_args() {
        let args = vec![
            "com1,/dev/zconsole".to_string(),
            "bootrom,/opt/fw/rom.bin".to_string(),
            "com2,socket,/tmp/vm.ttyb".to_string(),
        ];
        let spec = find_bootrom_spec(&args).unwrap();
        assert_eq!(spec.rom, PathBuf::from("/opt/fw/rom.bin"));
        assert_eq!(spec.vars, None);
    }

    #[test]
    fn bootrom_symbolic_bios() {
        let args = vec!["bootrom,bios".to_string()];
        let spec = find_bootrom_spec(&args).unwrap();
        assert_eq!(spec.rom, PathBuf::from(DEFAULT_CSM_ROM));
        assert_eq!(spec.vars, None);
    }

    #[test]
    fn rom_and_varfile() {
        let args =
            vec!["bootrom,/fw/CODE.fd,/var/db/vmm/x/VARS.fd".to_string()];
        let spec = find_bootrom_spec(&args).unwrap();
        assert_eq!(spec.rom, PathBuf::from("/fw/CODE.fd"));
        assert_eq!(spec.vars, Some(PathBuf::from("/var/db/vmm/x/VARS.fd")));
    }

    #[test]
    fn symbolic_rom_with_varfile() {
        let args = vec!["bootrom,uefi,/v.fd".to_string()];
        let spec = find_bootrom_spec(&args).unwrap();
        assert_eq!(spec.rom, PathBuf::from(DEFAULT_UEFI_ROM));
        assert_eq!(spec.vars, Some(PathBuf::from("/v.fd")));
    }

    #[test]
    fn equals_field_is_legacy_option_not_varfile() {
        let args = vec!["bootrom,/rom.bin,fwcfg=qemu".to_string()];
        let spec = find_bootrom_spec(&args).unwrap();
        assert_eq!(spec.rom, PathBuf::from("/rom.bin"));
        assert_eq!(spec.vars, None);
    }

    #[test]
    fn legacy_option_after_varfile() {
        let args = vec!["bootrom,/rom.bin,/v.fd,fwcfg=qemu".to_string()];
        let spec = find_bootrom_spec(&args).unwrap();
        assert_eq!(spec.rom, PathBuf::from("/rom.bin"));
        assert_eq!(spec.vars, Some(PathBuf::from("/v.fd")));
    }

    #[test]
    fn empty_varfile_field_is_none() {
        let args = vec!["bootrom,/rom.bin,".to_string()];
        let spec = find_bootrom_spec(&args).unwrap();
        assert_eq!(spec.rom, PathBuf::from("/rom.bin"));
        assert_eq!(spec.vars, None);
    }

    #[test]
    fn empty_rom_field_is_error() {
        for arg in ["bootrom,", "bootrom,   "] {
            let err = find_bootrom_spec(&[arg.to_string()]).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("invalid bootrom option \"{arg}\": missing rom path")
            );
        }
    }

    #[test]
    fn fields_are_trimmed() {
        let args = vec!["bootrom, /rom.bin , /v.fd ".to_string()];
        let spec = find_bootrom_spec(&args).unwrap();
        assert_eq!(spec.rom, PathBuf::from("/rom.bin"));
        assert_eq!(spec.vars, Some(PathBuf::from("/v.fd")));
    }

    #[test]
    fn device_token_is_case_insensitive() {
        let args = vec!["BootRom,/rom.bin".to_string()];
        let spec = find_bootrom_spec(&args).unwrap();
        assert_eq!(spec.rom, PathBuf::from("/rom.bin"));
        assert_eq!(spec.vars, None);
    }

    #[test]
    fn symbolic_name_does_not_apply_to_varfile() {
        let args = vec!["bootrom,/rom.bin,uefi".to_string()];
        let spec = find_bootrom_spec(&args).unwrap();
        assert_eq!(spec.rom, PathBuf::from("/rom.bin"));
        assert_eq!(spec.vars, Some(PathBuf::from("uefi")));
        assert_ne!(spec.vars, Some(PathBuf::from(DEFAULT_UEFI_ROM)));
    }

    #[test]
    fn first_bootrom_arg_wins() {
        let args = vec![
            "bootrom,/first.bin,/first-vars.fd".to_string(),
            "BOOTROM,/second.bin,/second-vars.fd".to_string(),
        ];
        let spec = find_bootrom_spec(&args).unwrap();
        assert_eq!(spec.rom, PathBuf::from("/first.bin"));
        assert_eq!(spec.vars, Some(PathBuf::from("/first-vars.fd")));
    }
}
