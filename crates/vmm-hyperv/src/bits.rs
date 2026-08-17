// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Hyper-V CPUID leaf and MSR constants.
//!
//! Derived from Microsoft's Hypervisor Top-Level Functional Specification
//! (TLFS) v6.0b. Naming follows the TLFS where possible.

use bitflags::bitflags;

/// Highest hypervisor CPUID leaf vmm_hyperv populates.
///
/// TLFS requires at least 0x4000_0005 for any Hyper-V-compatible
/// hypervisor. We expose 0x4000_0006 to advertise hardware features
/// (frequencies leaf).
pub const HYPERV_MAX_CPUID_LEAF: u32 = 0x4000_0006;

/// CPUID leaf 0x4000_0000: hypervisor identification.
///
/// eax = highest valid hypervisor CPUID leaf.
/// ebx/ecx/edx = 12-byte vendor signature. Linux only accepts
/// "Microsoft Hv" here; Windows is more permissive but reads it via
/// leaf 1 anyway, so we always return the Microsoft signature.
pub const HV_CPUID_VENDOR_EBX: u32 = 0x7263_694D; // "Micr"
pub const HV_CPUID_VENDOR_ECX: u32 = 0x666F_736F; // "osof"
pub const HV_CPUID_VENDOR_EDX: u32 = 0x7648_2074; // "t Hv"

/// CPUID leaf 0x4000_0001: interface signature. eax = "Hv#1".
pub const HV_CPUID_INTERFACE_EAX: u32 = 0x3123_7648;

bitflags! {
    /// CPUID leaf 0x4000_0003 EAX: per-MSR access privilege flags.
    ///
    /// Only the bits relevant to vmm_hyperv are enumerated here. The
    /// remaining bits in the TLFS gate features we do not implement
    /// (synic, stimer, virtual APIC, etc.) and must remain clear.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct HvLeaf3Eax: u32 {
        const PARTITION_REFERENCE_COUNTER = 1 << 1;
        const HYPERCALL                  = 1 << 5;
        const VP_INDEX                   = 1 << 6;
        const SYSTEM_RESET               = 1 << 7;
        const FREQUENCIES                = 1 << 8;
        const PARTITION_REFERENCE_TSC    = 1 << 9;
    }
}

bitflags! {
    /// CPUID leaf 0x4000_0003 EDX: misc feature bits.
    ///
    /// `GUEST_DEBUGGING` and `XMM_HYPERCALL_INPUT` are not implemented;
    /// only `HYPERCALL_PARAMS_IN_XMM` is omitted. We surface the bits
    /// we depend on (none are set) as documentation.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct HvLeaf3Edx: u32 {
        const HYPERCALL_RETURN_GUEST_OS_ID_REQ = 1 << 8;
    }
}

bitflags! {
    /// CPUID leaf 0x4000_0004 EAX: recommended guest behaviors.
    ///
    /// `RELAXED_TIMING` disables Windows' CLOCK_WATCHDOG bugcheck on
    /// slow vCPUs. Always recommended for non-Hyper-V hypervisors.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct HvLeaf4Eax: u32 {
        const USE_HYPERCALL_FOR_TLB_FLUSH      = 1 << 1;
        const USE_HYPERCALL_FOR_REMOTE_TLB     = 1 << 2;
        const USE_MSR_FOR_APIC                 = 1 << 3;
        const USE_MSR_FOR_RESET                = 1 << 4;
        const RELAXED_TIMING                   = 1 << 5;
    }
}

// ---- MSR addresses ----

pub const HV_X64_MSR_GUEST_OS_ID: u32 = 0x4000_0000;
pub const HV_X64_MSR_HYPERCALL: u32 = 0x4000_0001;
pub const HV_X64_MSR_VP_INDEX: u32 = 0x4000_0002;
pub const HV_X64_MSR_RESET: u32 = 0x4000_0003;

pub const HV_X64_MSR_TIME_REF_COUNT: u32 = 0x4000_0020;
pub const HV_X64_MSR_REFERENCE_TSC: u32 = 0x4000_0021;

/// Crash dump MSRs. Windows writes BSOD parameters here before the
/// triple fault, then sets the NOTIFY bit on `CRASH_CTL`. We log the
/// parameters via slog and otherwise treat them as opaque.
pub const HV_X64_MSR_CRASH_P0: u32 = 0x4000_0100;
pub const HV_X64_MSR_CRASH_P1: u32 = 0x4000_0101;
pub const HV_X64_MSR_CRASH_P2: u32 = 0x4000_0102;
pub const HV_X64_MSR_CRASH_P3: u32 = 0x4000_0103;
pub const HV_X64_MSR_CRASH_P4: u32 = 0x4000_0104;
pub const HV_X64_MSR_CRASH_CTL: u32 = 0x4000_0105;

/// Bit 63 of CRASH_CTL: written by the guest to signal "crash data
/// has been published".
pub const HV_CRASH_CTL_NOTIFY: u64 = 1 << 63;

// ---- Quick predicates ----

/// True if `msr` is in the Hyper-V synthetic MSR range vmm_hyperv
/// participates in. Used to decide whether the central MSR dispatcher
/// should consult the enlightenment at all.
pub const fn is_hyperv_msr(msr: u32) -> bool {
    matches!(
        msr,
        HV_X64_MSR_GUEST_OS_ID
            | HV_X64_MSR_HYPERCALL
            | HV_X64_MSR_VP_INDEX
            | HV_X64_MSR_RESET
            | HV_X64_MSR_TIME_REF_COUNT
            | HV_X64_MSR_REFERENCE_TSC
            | HV_X64_MSR_CRASH_P0
            | HV_X64_MSR_CRASH_P1
            | HV_X64_MSR_CRASH_P2
            | HV_X64_MSR_CRASH_P3
            | HV_X64_MSR_CRASH_P4
            | HV_X64_MSR_CRASH_CTL
    )
}
