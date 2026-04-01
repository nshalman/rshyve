// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Thin wrappers around bhyve kernel-emulated devices.
//!
//! These devices are emulated entirely by the bhyve kernel VMM. The
//! userspace wrappers exist for:
//! 1. Lifecycle coordination (pause/resume during migration)
//! 2. State export/import via the vmm-data interface (future)
//! 3. PM timer IO port configuration via VM_PMTMR_LOCATE
//!
//! The kernel handles all PIO for these devices — no userspace PIO
//! handler registration is needed.

pub mod pmtimer;

use std::sync::Arc;
use vmm_core::hdl::VmmHdl;

/// Bhyve kernel-emulated AT PIC (Intel 8259A).
///
/// The kernel handles PIO at ports 0x20-0x21 (master) and 0xA0-0xA1
/// (slave) directly. No userspace configuration needed.
pub struct BhyveAtPic {
    #[allow(dead_code)]
    hdl: Arc<VmmHdl>,
}

impl BhyveAtPic {
    pub fn create(hdl: Arc<VmmHdl>) -> Arc<Self> {
        Arc::new(Self { hdl })
    }
}

/// Bhyve kernel-emulated AT PIT (Intel 8254).
///
/// The kernel handles PIO at ports 0x40-0x43 directly.
pub struct BhyveAtPit {
    #[allow(dead_code)]
    hdl: Arc<VmmHdl>,
}

impl BhyveAtPit {
    pub fn create(hdl: Arc<VmmHdl>) -> Arc<Self> {
        Arc::new(Self { hdl })
    }
}

/// Bhyve kernel-emulated HPET (High Precision Event Timer).
///
/// The kernel handles MMIO at 0xFED00000 directly.
pub struct BhyveHpet {
    #[allow(dead_code)]
    hdl: Arc<VmmHdl>,
}

impl BhyveHpet {
    pub fn create(hdl: Arc<VmmHdl>) -> Arc<Self> {
        Arc::new(Self { hdl })
    }
}

/// Bhyve kernel-emulated I/O APIC.
///
/// The kernel handles MMIO at 0xFEC00000 directly.
pub struct BhyveIoApic {
    #[allow(dead_code)]
    hdl: Arc<VmmHdl>,
}

impl BhyveIoApic {
    pub fn create(hdl: Arc<VmmHdl>) -> Arc<Self> {
        Arc::new(Self { hdl })
    }
}

/// Bhyve kernel-emulated RTC (MC146818).
///
/// The kernel handles PIO at ports 0x70-0x71 directly.
pub struct BhyveRtc {
    #[allow(dead_code)]
    hdl: Arc<VmmHdl>,
}

impl BhyveRtc {
    pub fn create(hdl: Arc<VmmHdl>) -> Arc<Self> {
        Arc::new(Self { hdl })
    }

    /// Write the memory size to NVRAM for BIOS/UEFI discovery.
    ///
    /// The RTC NVRAM at offsets 0x34-0x35 holds extended memory size
    /// (above 16 MiB) in 64 KiB units, capped at 0xFFFF (4 GiB - 16 MiB).
    /// Offset 0x5b-0x5d holds memory above 4 GiB in 64 KiB units.
    pub fn memsize_to_nvram(
        &self,
        lowmem: u32,
        highmem: u64,
    ) -> std::io::Result<()> {
        // Extended memory (above 16 MiB) in 64 KiB units
        let ext_mem = if lowmem > 16 * 1024 * 1024 {
            ((lowmem - 16 * 1024 * 1024) / (64 * 1024)).min(0xFFFF) as u16
        } else {
            0
        };
        self.hdl.rtc_write(0x34, ext_mem as u8)?;
        self.hdl.rtc_write(0x35, (ext_mem >> 8) as u8)?;

        // High memory (above 4 GiB) in 64 KiB units
        let high_units = (highmem / (64 * 1024)) as u32;
        self.hdl.rtc_write(0x5B, high_units as u8)?;
        self.hdl.rtc_write(0x5C, (high_units >> 8) as u8)?;
        self.hdl.rtc_write(0x5D, (high_units >> 16) as u8)?;

        Ok(())
    }
}
