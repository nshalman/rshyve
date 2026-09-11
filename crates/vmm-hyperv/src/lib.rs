// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Microsoft Hyper-V enlightenment ("Tier 1") for vmm_hyperv.
//!
//! Implements the subset of the Hyper-V Top-Level Functional
//! Specification (TLFS v6.0b) needed to make Windows guests happy:
//!
//!   * Hypervisor identification CPUID leaves 0x4000_0000–0x4000_0006
//!   * `relaxed timing` recommendation (suppresses CLOCK_WATCHDOG bugcheck)
//!   * `spinlock retries = 0xFFFFFFFF` (suppresses spinlock notifications)
//!   * `HV_X64_MSR_GUEST_OS_ID` / `HV_X64_MSR_HYPERCALL` (with overlay page)
//!   * `HV_X64_MSR_VP_INDEX` (per-vCPU readback)
//!   * `HV_X64_MSR_TIME_REF_COUNT` (monotonic 100 ns counter since VM boot)
//!   * `HV_X64_MSR_REFERENCE_TSC` (with overlay page; the big perf win)
//!   * `HV_X64_MSR_RESET` (writing the reset bit triggers a guest reset)
//!   * Crash dump MSRs `CRASH_P0..P4` + `CRASH_CTL` (BSOD payload logged
//!     via slog so we can see why a Windows guest is dying)
//!
//! Synthetic interrupts (synic), synthetic timers (stimer), virtual
//! APIC, TLB-flush hypercalls, and IPI hypercalls are intentionally
//! **not** implemented. They require kernel cooperation in vmm.ko and
//! belong to a later tier.

pub mod bits;
pub mod hypercall;
pub mod overlay;
pub mod tsc;

use std::sync::{Arc, Mutex};
use std::time::Instant;

use bhyve_api::vcpu_cpuid_entry;
use slog::{info, warn, Logger};
use vmm_core::mem::PhysMap;
use vmm_core::msr::{MsrHandler, RdmsrOutcome, WrmsrOutcome};

use crate::bits::*;
use crate::hypercall::MsrHypercallValue;
use crate::overlay::{Overlay, PAGE_SIZE};
use crate::tsc::{MsrReferenceTscValue, ReferenceTscPage};

/// Feature gates for the enlightenment.
#[derive(Debug, Clone, Copy)]
pub struct Features {
    /// Advertise + handle `HV_X64_MSR_REFERENCE_TSC` (and the related
    /// `TIME_REF_COUNT`). Off-by-default so a CLI/config can opt in;
    /// the rest of Tier 1 is always-on.
    pub reference_tsc: bool,
    /// Advertise + handle `HV_X64_MSR_RESET`.
    pub reset: bool,
    /// Advertise the `frequencies` CPUID leaf (0x4000_0006). Required
    /// for some Windows guests to skip TSC calibration.
    pub frequencies: bool,
    /// Guest TSC frequency in Hz, used to compute the reference-TSC
    /// page scale factor. Should match what the kernel VMM advertises
    /// to the guest. Ignored if `reference_tsc` is false.
    pub tsc_freq_hz: u64,
}

impl Default for Features {
    fn default() -> Self {
        Self {
            reference_tsc: true,
            reset: true,
            frequencies: true,
            tsc_freq_hz: 0,
        }
    }
}

/// Per-VM Hyper-V state. All MSR storage that is shared across vCPUs
/// lives behind one `Mutex<Inner>`. Hot-path RDMSRs are rare (Windows
/// caches the values), so a single mutex is fine.
pub struct HyperV {
    log: Logger,
    physmap: Arc<PhysMap>,
    features: Features,
    boot: Instant,
    inner: Mutex<Inner>,
}

struct Inner {
    msr_guest_os_id: u64,
    msr_hypercall: MsrHypercallValue,
    msr_reference_tsc: MsrReferenceTscValue,

    hypercall_overlay: Option<Overlay>,
    reference_tsc_overlay: Option<Overlay>,

    crash_p: [u64; 5],
}

impl Inner {
    fn new() -> Self {
        Self {
            msr_guest_os_id: 0,
            msr_hypercall: MsrHypercallValue::default(),
            msr_reference_tsc: MsrReferenceTscValue::default(),
            hypercall_overlay: None,
            reference_tsc_overlay: None,
            crash_p: [0; 5],
        }
    }
}

impl HyperV {
    pub fn new(
        log: Logger,
        physmap: Arc<PhysMap>,
        features: Features,
    ) -> Arc<Self> {
        Arc::new(Self {
            log,
            physmap,
            features,
            boot: Instant::now(),
            inner: Mutex::new(Inner::new()),
        })
    }

    /// Append the Hyper-V CPUID leaves to `entries`.
    ///
    /// Caller is responsible for re-sorting with
    /// `vcpu_cpuid_entry::eval_sort` before programming the kernel
    /// VMM, since `set_cpuid` requires sorted input.
    pub fn add_cpuid(&self, entries: &mut Vec<vcpu_cpuid_entry>) {
        let max_leaf = if self.features.frequencies {
            HYPERV_MAX_CPUID_LEAF
        } else {
            0x4000_0005
        };

        // 0x4000_0000: vendor signature.
        entries.push(cpuid_entry(
            0x4000_0000,
            0,
            max_leaf,
            HV_CPUID_VENDOR_EBX,
            HV_CPUID_VENDOR_ECX,
            HV_CPUID_VENDOR_EDX,
        ));

        // 0x4000_0001: interface signature ("Hv#1").
        entries.push(cpuid_entry(
            0x4000_0001,
            0,
            HV_CPUID_INTERFACE_EAX,
            0,
            0,
            0,
        ));

        // 0x4000_0002: hypervisor version. Zeroed to avoid having to
        // reason about specific version semantics across migration.
        entries.push(cpuid_entry(0x4000_0002, 0, 0, 0, 0, 0));

        // 0x4000_0003: per-MSR access privileges.
        let mut leaf3 = HvLeaf3Eax::HYPERCALL | HvLeaf3Eax::VP_INDEX;
        if self.features.reset {
            leaf3 |= HvLeaf3Eax::SYSTEM_RESET;
        }
        if self.features.frequencies {
            leaf3 |= HvLeaf3Eax::FREQUENCIES;
        }
        if self.features.reference_tsc {
            leaf3 |= HvLeaf3Eax::PARTITION_REFERENCE_COUNTER;
            leaf3 |= HvLeaf3Eax::PARTITION_REFERENCE_TSC;
        }
        entries.push(cpuid_entry(0x4000_0003, 0, leaf3.bits(), 0, 0, 0));

        // 0x4000_0004: recommended guest behaviors.
        // EAX = RELAXED_TIMING (the watchdog suppression bit).
        // EBX = 0xFFFFFFFF: the guest should never call out to the
        //       hypervisor on spinlock contention.
        let leaf4_eax = HvLeaf4Eax::RELAXED_TIMING.bits();
        entries.push(cpuid_entry(0x4000_0004, 0, leaf4_eax, 0xFFFF_FFFF, 0, 0));

        // 0x4000_0005: implementation limits. All zero (TLFS-allowed).
        entries.push(cpuid_entry(0x4000_0005, 0, 0, 0, 0, 0));

        // 0x4000_0006: hardware features advertised by the host.
        // Only emitted when `frequencies` is on; otherwise the leaf
        // is invisible because we capped max_leaf at 0x4000_0005.
        if self.features.frequencies {
            entries.push(cpuid_entry(0x4000_0006, 0, 0, 0, 0, 0));
        }
    }

    fn handle_wr_guest_os_id(&self, inner: &mut Inner, value: u64) {
        inner.msr_guest_os_id = value;
        // Per TLFS 3.13: writing 0 to GUEST_OS_ID forces the hypercall
        // MSR's Enabled bit to clear, which means we drop the overlay.
        if value == 0 {
            inner.msr_hypercall = MsrHypercallValue(
                inner.msr_hypercall.raw() & !MsrHypercallValue::ENABLED,
            );
            if let Some(ov) = inner.hypercall_overlay.take() {
                overlay::remove(&self.physmap, &ov);
            }
        }
    }

    fn handle_wr_hypercall(&self, inner: &mut Inner, value: u64) {
        let old = inner.msr_hypercall;
        let new = old.apply_write(value);
        inner.msr_hypercall = new;

        // If GUEST_OS_ID is still 0, the guest must enable that first
        // before the hypercall page can become live (TLFS 3.13).
        if inner.msr_guest_os_id == 0 {
            return;
        }

        let want_enabled = new.enabled();
        let new_gpa = new.gpa();
        let payload = hypercall::build_page();

        match (inner.hypercall_overlay.take(), want_enabled) {
            (Some(ov), true) if ov.gpa == new_gpa => {
                inner.hypercall_overlay = Some(ov);
            }
            (Some(ov), true) => {
                match overlay::relocate(&self.physmap, ov, new_gpa, &payload) {
                    Ok(new_ov) => inner.hypercall_overlay = Some(new_ov),
                    Err(e) => warn!(
                        self.log, "could not relocate hypercall overlay";
                        "gpa" => format!("{:#x}", new_gpa),
                        "error" => %e
                    ),
                }
            }
            (Some(ov), false) => {
                overlay::remove(&self.physmap, &ov);
            }
            (None, true) => {
                match overlay::install(&self.physmap, new_gpa, &payload) {
                    Ok(ov) => inner.hypercall_overlay = Some(ov),
                    Err(e) => warn!(
                        self.log, "could not install hypercall overlay";
                        "gpa" => format!("{:#x}", new_gpa),
                        "error" => %e
                    ),
                }
            }
            (None, false) => {}
        }
    }

    fn handle_wr_reference_tsc(&self, inner: &mut Inner, value: u64) {
        let new = MsrReferenceTscValue(value);
        inner.msr_reference_tsc = new;

        let scale = match tsc::compute_scale(self.features.tsc_freq_hz) {
            Some(s) => s,
            None => {
                // Frequency unknown or out of range: publish the page
                // with sequence=0 so the guest falls back. We still
                // honor the GPA so subsequent writes can succeed.
                warn!(self.log, "reference-TSC freq unusable, publishing invalid page";
                      "freq_hz" => self.features.tsc_freq_hz);
                self.publish_tsc_page(inner, new, 0)
                    .unwrap_or_else(|e| warn!(self.log, "tsc page publish failed"; "error" => %e));
                return;
            }
        };
        if let Err(e) = self.publish_tsc_page(inner, new, scale) {
            warn!(self.log, "tsc page publish failed"; "error" => %e);
        }
    }

    fn publish_tsc_page(
        &self,
        inner: &mut Inner,
        msr: MsrReferenceTscValue,
        scale: u64,
    ) -> Result<(), overlay::OverlayError> {
        let gpa = msr.gpa();
        let page = if scale == 0 {
            // Sequence=0 means "page invalid"; guest falls back.
            ReferenceTscPage {
                sequence: 0,
                reserved: 0,
                scale: 0,
                offset: 0,
            }
        } else {
            ReferenceTscPage {
                sequence: 1,
                reserved: 0,
                scale,
                offset: 0,
            }
        }
        .into_page();

        match (inner.reference_tsc_overlay.take(), msr.enabled()) {
            (Some(ov), true) if ov.gpa == gpa => {
                inner.reference_tsc_overlay = Some(ov);
                if let Some(map) = self.physmap.lookup(gpa, PAGE_SIZE) {
                    let _ = map.write_bytes(page.as_slice());
                }
            }
            (Some(ov), true) => {
                let new_ov = overlay::relocate(&self.physmap, ov, gpa, &page)?;
                inner.reference_tsc_overlay = Some(new_ov);
            }
            (Some(ov), false) => overlay::remove(&self.physmap, &ov),
            (None, true) => {
                let ov = overlay::install(&self.physmap, gpa, &page)?;
                inner.reference_tsc_overlay = Some(ov);
            }
            (None, false) => {}
        }
        Ok(())
    }

    fn log_crash(&self, inner: &Inner) {
        info!(
            self.log,
            "Windows guest BSOD";
            "bugcheck" => format!("{:#x}", inner.crash_p[0]),
            "p1" => format!("{:#x}", inner.crash_p[1]),
            "p2" => format!("{:#x}", inner.crash_p[2]),
            "p3" => format!("{:#x}", inner.crash_p[3]),
            "p4" => format!("{:#x}", inner.crash_p[4]),
        );
    }

    /// Drop overlay pages and restore the original guest contents.
    ///
    /// Called before VM reset and before live migration so the guest
    /// memory image being copied does not contain our overlay data.
    pub fn pause_overlays(&self) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(ov) = inner.hypercall_overlay.take() {
            overlay::remove(&self.physmap, &ov);
        }
        if let Some(ov) = inner.reference_tsc_overlay.take() {
            overlay::remove(&self.physmap, &ov);
        }
    }

    /// Reset all guest-visible state (called on guest reboot).
    pub fn reset(&self) {
        self.pause_overlays();
        let mut inner = self.inner.lock().unwrap();
        *inner = Inner::new();
    }
}

impl MsrHandler for HyperV {
    fn rdmsr(&self, vcpu_id: i32, msr: u32) -> RdmsrOutcome {
        if !is_hyperv_msr(msr) {
            return RdmsrOutcome::NotHandled;
        }
        let inner = self.inner.lock().unwrap();
        match msr {
            HV_X64_MSR_GUEST_OS_ID => {
                RdmsrOutcome::Handled(inner.msr_guest_os_id)
            }
            HV_X64_MSR_HYPERCALL => {
                RdmsrOutcome::Handled(inner.msr_hypercall.raw())
            }
            HV_X64_MSR_VP_INDEX => RdmsrOutcome::Handled(vcpu_id as u64),

            // Read-only counter; gated on the reference-TSC feature
            // because TLFS leaf 0x4000_0003 EAX bit 1 governs both.
            HV_X64_MSR_TIME_REF_COUNT if self.features.reference_tsc => {
                let elapsed_100ns = self.boot.elapsed().as_nanos() / 100;
                RdmsrOutcome::Handled(elapsed_100ns as u64)
            }
            HV_X64_MSR_REFERENCE_TSC if self.features.reference_tsc => {
                RdmsrOutcome::Handled(inner.msr_reference_tsc.raw())
            }
            HV_X64_MSR_TIME_REF_COUNT | HV_X64_MSR_REFERENCE_TSC => {
                RdmsrOutcome::GpException
            }

            // Reset MSR is write-triggered; reading returns the magic
            // signature `0` (no reset in progress).
            HV_X64_MSR_RESET if self.features.reset => RdmsrOutcome::Handled(0),
            HV_X64_MSR_RESET => RdmsrOutcome::GpException,

            // Crash MSRs: the guest reads back what it last wrote.
            HV_X64_MSR_CRASH_P0 => RdmsrOutcome::Handled(inner.crash_p[0]),
            HV_X64_MSR_CRASH_P1 => RdmsrOutcome::Handled(inner.crash_p[1]),
            HV_X64_MSR_CRASH_P2 => RdmsrOutcome::Handled(inner.crash_p[2]),
            HV_X64_MSR_CRASH_P3 => RdmsrOutcome::Handled(inner.crash_p[3]),
            HV_X64_MSR_CRASH_P4 => RdmsrOutcome::Handled(inner.crash_p[4]),
            // CRASH_CTL: bit 63 (NOTIFY) reads back as set so the guest
            // knows we accept its writes. Lower bits zeroed.
            HV_X64_MSR_CRASH_CTL => RdmsrOutcome::Handled(HV_CRASH_CTL_NOTIFY),

            _ => RdmsrOutcome::NotHandled,
        }
    }

    fn wrmsr(&self, _vcpu_id: i32, msr: u32, value: u64) -> WrmsrOutcome {
        if !is_hyperv_msr(msr) {
            return WrmsrOutcome::NotHandled;
        }
        let mut inner = self.inner.lock().unwrap();
        match msr {
            HV_X64_MSR_GUEST_OS_ID => {
                self.handle_wr_guest_os_id(&mut inner, value);
                WrmsrOutcome::Handled
            }
            HV_X64_MSR_HYPERCALL => {
                self.handle_wr_hypercall(&mut inner, value);
                WrmsrOutcome::Handled
            }
            HV_X64_MSR_VP_INDEX => WrmsrOutcome::GpException,

            HV_X64_MSR_RESET if self.features.reset => {
                if value & 1 != 0 {
                    info!(self.log, "guest requested reset via Hyper-V MSR");
                    WrmsrOutcome::Reset
                } else {
                    WrmsrOutcome::Handled
                }
            }
            HV_X64_MSR_RESET => WrmsrOutcome::GpException,

            HV_X64_MSR_TIME_REF_COUNT => WrmsrOutcome::GpException,
            HV_X64_MSR_REFERENCE_TSC if self.features.reference_tsc => {
                self.handle_wr_reference_tsc(&mut inner, value);
                WrmsrOutcome::Handled
            }
            HV_X64_MSR_REFERENCE_TSC => WrmsrOutcome::GpException,

            HV_X64_MSR_CRASH_P0 => {
                inner.crash_p[0] = value;
                WrmsrOutcome::Handled
            }
            HV_X64_MSR_CRASH_P1 => {
                inner.crash_p[1] = value;
                WrmsrOutcome::Handled
            }
            HV_X64_MSR_CRASH_P2 => {
                inner.crash_p[2] = value;
                WrmsrOutcome::Handled
            }
            HV_X64_MSR_CRASH_P3 => {
                inner.crash_p[3] = value;
                WrmsrOutcome::Handled
            }
            HV_X64_MSR_CRASH_P4 => {
                inner.crash_p[4] = value;
                WrmsrOutcome::Handled
            }
            HV_X64_MSR_CRASH_CTL => {
                if value & HV_CRASH_CTL_NOTIFY != 0 {
                    self.log_crash(&inner);
                }
                WrmsrOutcome::Handled
            }

            _ => WrmsrOutcome::NotHandled,
        }
    }
}

fn cpuid_entry(
    func: u32,
    index: u32,
    eax: u32,
    ebx: u32,
    ecx: u32,
    edx: u32,
) -> vcpu_cpuid_entry {
    // Hyper-V leaves are not sub-leafed, so we leave VCE_FLAG_MATCH_INDEX
    // clear (matches any ECX input) to mirror how other non-indexed
    // leaves are programmed in vmm-core/cpuid.rs.
    vcpu_cpuid_entry {
        vce_function: func,
        vce_index: index,
        vce_flags: 0,
        vce_eax: eax,
        vce_ebx: ebx,
        vce_ecx: ecx,
        vce_edx: edx,
        _pad: 0,
    }
}
