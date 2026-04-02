// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CLI argument parsing and configuration for the VMM.
//!
//! Maintains compatibility with existing bhyve flags used by vmadm/boot.c
//! while adding new flags for migration and management.

use std::path::PathBuf;

use clap::Parser;

/// Parse a memory size string into bytes.
///
/// Accepts suffixes: K/k (KiB), M/m (MiB), G/g (GiB), T/t (TiB).
/// A bare number without suffix is treated as mebibytes (matching
/// bhyve's convention where `-m 1024` means 1 GiB).
pub fn parse_mem_size(s: &str) -> anyhow::Result<usize> {
    let s = s.trim();
    anyhow::ensure!(!s.is_empty(), "empty memory size");

    let (num_str, multiplier) = match s.as_bytes().last() {
        Some(b'k' | b'K') => (&s[..s.len() - 1], 1024usize),
        Some(b'm' | b'M') => (&s[..s.len() - 1], 1024 * 1024),
        Some(b'g' | b'G') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        Some(b't' | b'T') => (&s[..s.len() - 1], 1024 * 1024 * 1024 * 1024),
        // Bare number = mebibytes (bhyve convention)
        _ => (s, 1024 * 1024),
    };

    let num: usize = num_str.parse().map_err(|e| {
        anyhow::anyhow!("invalid memory size '{s}': {e}")
    })?;

    num.checked_mul(multiplier).ok_or_else(|| {
        anyhow::anyhow!("memory size '{s}' overflows")
    })
}

/// New Rust-based bhyve VMM with live migration support.
#[derive(Parser, Debug)]
#[command(name = "vmm", version, about)]
pub struct Cli {
    /// CPU count or topology: N or cpus=N,sockets=M,cores=C,threads=T
    #[arg(short = 'c')]
    pub cpus: Option<String>,

    /// Memory size: bare number = MiB (bhyve compat), or use suffix
    /// K/M/G/T. Examples: "256M", "2G", "1024" (= 1 GiB)
    #[arg(short = 'm')]
    pub memory: Option<String>,

    /// PCI device: slot,driver[,config...]  (repeatable)
    #[arg(short = 's', action = clap::ArgAction::Append)]
    pub pci_slot: Vec<String>,

    /// LPC device: device,config  (repeatable)
    #[arg(short = 'l', action = clap::ArgAction::Append)]
    pub lpc: Vec<String>,

    /// Configuration file (key=value or TOML)
    #[arg(short = 'k')]
    pub config_file: Option<PathBuf>,

    /// VM UUID
    #[arg(short = 'U')]
    pub uuid: Option<String>,

    /// SMBIOS info: type,key=value,...
    #[arg(short = 'B')]
    pub smbios: Option<String>,

    /// Config option: key=value  (repeatable)
    #[arg(short = 'o', action = clap::ArgAction::Append)]
    pub config_option: Vec<String>,

    /// Destroy VM on power-off
    #[arg(short = 'D')]
    pub destroy_on_poweroff: bool,

    /// VM exit on HLT instruction
    #[arg(short = 'H')]
    pub vmexit_on_hlt: bool,

    /// Wire guest memory (don't swap)
    #[arg(short = 'S')]
    pub wire_memory: bool,

    /// Create ACPI tables
    #[arg(short = 'A')]
    pub acpi: bool,

    /// Control socket path for migration/management
    #[arg(long)]
    pub control_socket: Option<PathBuf>,

    /// JSON configuration file (alternative to CLI flags)
    #[arg(long)]
    pub json_config: Option<PathBuf>,

    /// Enable dirty page tracking for live migration
    #[arg(long)]
    pub track_dirty: bool,

    /// Direct boot: Linux kernel bzImage path (bypasses UEFI firmware)
    #[arg(long)]
    pub kernel: Option<PathBuf>,

    /// Direct boot: kernel command line
    #[arg(long)]
    pub cmdline: Option<String>,

    /// Direct boot: initrd/initramfs path
    #[arg(long)]
    pub initrd: Option<PathBuf>,

    /// Migration destination: listen for incoming migration on this address
    /// (e.g., "0.0.0.0:4567"). When set, the VM is created with RAM but
    /// does NOT start vCPUs — it waits for migration data.
    #[arg(long)]
    pub migrate_listen: Option<String>,

    /// VM name
    pub vm_name: String,
}

impl Cli {
    /// Parse the CPU count from the -c argument.
    ///
    /// Returns an error if the argument is present but unparseable.
    pub fn num_cpus(&self) -> anyhow::Result<u32> {
        match &self.cpus {
            None => Ok(1),
            Some(s) => {
                // Simple number
                if let Ok(n) = s.parse::<u32>() {
                    anyhow::ensure!(n > 0, "CPU count must be > 0");
                    return Ok(n);
                }
                // cpus=N,sockets=M,cores=C,threads=T format
                for part in s.split(',') {
                    if let Some(val) = part.strip_prefix("cpus=") {
                        let n: u32 = val.parse().map_err(|e| {
                            anyhow::anyhow!("invalid cpus value '{val}': {e}")
                        })?;
                        anyhow::ensure!(n > 0, "CPU count must be > 0");
                        return Ok(n);
                    }
                }
                anyhow::bail!("could not parse CPU specification: '{s}'")
            }
        }
    }

    /// Parse the memory size from the -m argument.
    ///
    /// Returns an error if the argument is present but unparseable.
    pub fn mem_size(&self) -> anyhow::Result<usize> {
        match &self.memory {
            None => Ok(256 * 1024 * 1024), // 256 MiB default
            Some(s) => parse_mem_size(s),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mem_suffixes() {
        assert_eq!(parse_mem_size("256M").unwrap(), 256 * 1024 * 1024);
        assert_eq!(parse_mem_size("2G").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_mem_size("512k").unwrap(), 512 * 1024);
        // Bare number = MiB
        assert_eq!(parse_mem_size("1024").unwrap(), 1024 * 1024 * 1024);
    }

    #[test]
    fn parse_mem_errors() {
        assert!(parse_mem_size("").is_err());
        assert!(parse_mem_size("abc").is_err());
        assert!(parse_mem_size("-1M").is_err());
    }
}
