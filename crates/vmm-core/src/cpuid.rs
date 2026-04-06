// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CPUID feature masking for cross-host migration compatibility.
//!
//! Reads host CPUID values, applies a feature mask (baseline profile),
//! and programs the masked entries into the kernel VMM via `VM_SET_CPUID`.

use bhyve_api::{
    vcpu_cpuid_entry, VCE_FLAG_MATCH_INDEX,
    VCC_FLAG_INTEL_FALLBACK,
};

use slog;

use crate::hdl::VmmHdl;

/// CPU baseline profiles for migration compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuBaseline {
    /// No masking — pass through host CPUID as-is (default).
    Host,
    /// Mask out AVX-512 and related features.
    /// Compatible with any x86-64 host that has at least AVX2.
    Avx2,
    /// Mask out AVX-512, AVX2, FMA, BMI1/2, and other Haswell+ features.
    /// Maximum compatibility — SSE4.2 baseline.
    Sse42,
}

impl CpuBaseline {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "host" | "native" => Some(Self::Host),
            "avx2" | "no-avx512" => Some(Self::Avx2),
            "sse42" | "sse4.2" | "westmere" => Some(Self::Sse42),
            _ => None,
        }
    }
}

/// Read a CPUID leaf from the host CPU.
///
/// On non-x86_64 (cross-compile from macOS), returns zeros.
fn host_cpuid(leaf: u32, subleaf: u32) -> (u32, u32, u32, u32) {
    #[cfg(target_arch = "x86_64")]
    {
        let result = unsafe {
            std::arch::x86_64::__cpuid_count(leaf, subleaf)
        };
        (result.eax, result.ebx, result.ecx, result.edx)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (leaf, subleaf);
        (0, 0, 0, 0)
    }
}

fn entry(
    func: u32, index: u32, flags: u32,
    eax: u32, ebx: u32, ecx: u32, edx: u32,
) -> vcpu_cpuid_entry {
    vcpu_cpuid_entry {
        vce_function: func,
        vce_index: index,
        vce_flags: flags,
        vce_eax: eax,
        vce_ebx: ebx,
        vce_ecx: ecx,
        vce_edx: edx,
        _pad: 0,
    }
}

/// Build CPUID entries with the given baseline mask applied.
///
/// Reads host CPUID values and masks out features above the baseline.
/// Returns a sorted Vec of entries suitable for `VmmHdl::set_cpuid`.
#[allow(unused_mut, unused_assignments)]
pub fn build_cpuid_entries(baseline: CpuBaseline) -> Vec<vcpu_cpuid_entry> {
    if baseline == CpuBaseline::Host {
        return Vec::new();
    }

    let mut entries = Vec::with_capacity(64);

    // Leaf 0: max standard leaf + vendor string
    let (max_leaf, ebx, ecx, edx) = host_cpuid(0, 0);
    entries.push(entry(0, 0, 0, max_leaf, ebx, ecx, edx));

    let is_intel = ebx == 0x756e_6547 // "Genu"
        && edx == 0x4965_6e69         // "ineI"
        && ecx == 0x6c65_746e;        // "ntel"

    // Leaf 1: basic features
    let (eax1, ebx1, mut ecx1, edx1) = host_cpuid(1, 0);
    if baseline == CpuBaseline::Sse42 {
        // Clear AVX (28), FMA (12), F16C (29), RDRAND (30)
        ecx1 &= !(1 << 28 | 1 << 12 | 1 << 29 | 1 << 30);
    }
    entries.push(entry(1, 0, 0, eax1, ebx1, ecx1, edx1));

    // Leaf 2: cache/TLB descriptors (Intel only)
    if is_intel {
        let (a, b, c, d) = host_cpuid(2, 0);
        entries.push(entry(2, 0, 0, a, b, c, d));
    }

    // Leaf 4: deterministic cache (Intel, subleaf enumerated)
    if is_intel && max_leaf >= 4 {
        for idx in 0..8u32 {
            let (a, b, c, d) = host_cpuid(4, idx);
            if a & 0x1f == 0 { break; }
            entries.push(entry(4, idx, VCE_FLAG_MATCH_INDEX, a, b, c, d));
        }
    }

    // Leaf 5: MONITOR/MWAIT
    if max_leaf >= 5 {
        let (a, b, c, d) = host_cpuid(5, 0);
        entries.push(entry(5, 0, 0, a, b, c, d));
    }

    // Leaf 6: thermal/power
    if max_leaf >= 6 {
        let (a, b, c, d) = host_cpuid(6, 0);
        entries.push(entry(6, 0, 0, a, b, c, d));
    }

    // Leaf 7, ECX=0: structured extended features — THE KEY LEAF
    if max_leaf >= 7 {
        let (eax7, mut ebx7, mut ecx7, mut edx7) = host_cpuid(7, 0);

        // AVX-512 bits in EBX
        const AVX512_EBX: u32 = (1 << 16) | (1 << 17) | (1 << 21) |
            (1 << 26) | (1 << 27) | (1 << 28) | (1 << 30) | (1 << 31);
        // AVX-512 bits in ECX
        const AVX512_ECX: u32 = (1 << 1) | (1 << 6) | (1 << 11) |
            (1 << 12) | (1 << 14);
        // AVX-512 + AMX bits in EDX
        const AVX512_EDX: u32 = (1 << 2) | (1 << 3) | (1 << 8) |
            (1 << 23) | (1 << 24) | (1 << 25);

        match baseline {
            CpuBaseline::Avx2 => {
                ebx7 &= !AVX512_EBX;
                ecx7 &= !AVX512_ECX;
                edx7 &= !AVX512_EDX;
            }
            CpuBaseline::Sse42 => {
                // Also mask AVX2 (5), BMI1 (3), BMI2 (8)
                ebx7 &= !(AVX512_EBX | (1 << 3) | (1 << 5) | (1 << 8));
                ecx7 &= !AVX512_ECX;
                edx7 &= !AVX512_EDX;
            }
            CpuBaseline::Host => unreachable!(),
        }

        entries.push(entry(7, 0, VCE_FLAG_MATCH_INDEX, eax7, ebx7, ecx7, edx7));

        // Leaf 7, ECX=1: AVX-512_BF16, AVX-VNNI, etc.
        if eax7 >= 1 {
            let (mut eax71, ebx71, ecx71, edx71) = host_cpuid(7, 1);
            // AVX-512_BF16 (5), AVX-VNNI (4), AVX-IFMA (23)
            eax71 &= !(1 << 4 | 1 << 5 | 1 << 23);
            entries.push(entry(7, 1, VCE_FLAG_MATCH_INDEX, eax71, ebx71, ecx71, edx71));
        }
    }

    // Leaf 0xA: performance monitoring
    if max_leaf >= 0xA {
        let (a, b, c, d) = host_cpuid(0xA, 0);
        entries.push(entry(0xA, 0, 0, a, b, c, d));
    }

    // Leaf 0xB: extended topology (subleaf enumerated)
    if max_leaf >= 0xB {
        for idx in 0..4u32 {
            let (a, b, c, d) = host_cpuid(0xB, idx);
            if a == 0 && b == 0 { break; }
            entries.push(entry(0xB, idx, VCE_FLAG_MATCH_INDEX, a, b, c, d));
        }
    }

    // Leaf 0xD: XSAVE state — must match masked features
    if max_leaf >= 0xD {
        let (mut eax_d0, mut ebx_d0, mut ecx_d0, edx_d0) = host_cpuid(0xD, 0);

        match baseline {
            CpuBaseline::Avx2 => {
                // Clear AVX-512 opmask (5), hi256 (6), hi16_zmm (7), AMX (17,18)
                eax_d0 &= !(1 << 5 | 1 << 6 | 1 << 7 | 1 << 17 | 1 << 18);
            }
            CpuBaseline::Sse42 => {
                // Clear AVX (2), AVX-512 (5-7), AMX (17-18)
                eax_d0 &= !(1 << 2 | 1 << 5 | 1 << 6 | 1 << 7 |
                             1 << 17 | 1 << 18);
            }
            CpuBaseline::Host => unreachable!(),
        }

        // Recalculate XSAVE area size
        let mut xsave_size: u32 = 576; // x87 + SSE
        if eax_d0 & (1 << 2) != 0 {
            xsave_size = 832; // + AVX YMM hi128
        }
        ebx_d0 = xsave_size;
        ecx_d0 = xsave_size;

        entries.push(entry(0xD, 0, VCE_FLAG_MATCH_INDEX, eax_d0, ebx_d0, ecx_d0, edx_d0));

        // Subleaf 1: XSAVE features
        let (a, b, c, d) = host_cpuid(0xD, 1);
        entries.push(entry(0xD, 1, VCE_FLAG_MATCH_INDEX, a, b, c, d));

        // Subleaf 2+: per-component info (only for enabled components)
        for bit in 2..32u32 {
            if eax_d0 & (1 << bit) == 0 { continue; }
            let (a, b, c, d) = host_cpuid(0xD, bit);
            if a == 0 { continue; }
            entries.push(entry(0xD, bit, VCE_FLAG_MATCH_INDEX, a, b, c, d));
        }
    }

    // Leaf 0x15: TSC/core crystal clock
    if max_leaf >= 0x15 {
        let (a, b, c, d) = host_cpuid(0x15, 0);
        entries.push(entry(0x15, 0, 0, a, b, c, d));
    }

    // Leaf 0x16: processor frequency
    if max_leaf >= 0x16 {
        let (a, b, c, d) = host_cpuid(0x16, 0);
        entries.push(entry(0x16, 0, 0, a, b, c, d));
    }

    // Extended leaves
    let (max_ext, _, _, _) = host_cpuid(0x8000_0000, 0);
    entries.push(entry(0x8000_0000, 0, 0, max_ext, 0, 0, 0));

    if max_ext >= 0x8000_0001 {
        let (a, b, c, d) = host_cpuid(0x8000_0001, 0);
        entries.push(entry(0x8000_0001, 0, 0, a, b, c, d));
    }

    // Brand string (0x80000002-4)
    for leaf in 0x8000_0002..=0x8000_0004 {
        if max_ext >= leaf {
            let (a, b, c, d) = host_cpuid(leaf, 0);
            entries.push(entry(leaf, 0, 0, a, b, c, d));
        }
    }

    // Cache info (0x80000005-6)
    for leaf in 0x8000_0005..=0x8000_0006 {
        if max_ext >= leaf {
            let (a, b, c, d) = host_cpuid(leaf, 0);
            entries.push(entry(leaf, 0, 0, a, b, c, d));
        }
    }

    // Advanced power management
    if max_ext >= 0x8000_0007 {
        let (a, b, c, d) = host_cpuid(0x8000_0007, 0);
        entries.push(entry(0x8000_0007, 0, 0, a, b, c, d));
    }

    // Address sizes
    if max_ext >= 0x8000_0008 {
        let (a, b, c, d) = host_cpuid(0x8000_0008, 0);
        entries.push(entry(0x8000_0008, 0, 0, a, b, c, d));
    }

    entries.sort_by(vcpu_cpuid_entry::eval_sort);
    entries
}

/// Apply a CPUID baseline to all vCPUs of a VM.
///
/// If baseline is `Host`, this is a no-op (kernel uses native CPUID).
pub fn apply_cpuid_baseline(
    hdl: &VmmHdl,
    num_cpus: u32,
    baseline: CpuBaseline,
    log: &slog::Logger,
) -> std::io::Result<()> {
    if baseline == CpuBaseline::Host {
        return Ok(());
    }

    let entries = build_cpuid_entries(baseline);
    if entries.is_empty() {
        return Ok(());
    }

    // Intel fallback for Intel CPUs
    let (_, ebx, ecx, edx) = host_cpuid(0, 0);
    let is_intel = ebx == 0x756e_6547
        && edx == 0x4965_6e69
        && ecx == 0x6c65_746e;
    let flags = if is_intel { VCC_FLAG_INTEL_FALLBACK } else { 0 };

    slog::info!(log, "applying CPU baseline";
        "baseline" => ?baseline,
        "entries" => entries.len(),
        "vcpus" => num_cpus,
    );

    for vcpuid in 0..num_cpus as i32 {
        let mut cpu_entries = entries.clone();
        hdl.set_cpuid(vcpuid, &mut cpu_entries, flags)?;
    }

    Ok(())
}

/// Query the host CPU's key feature bits for migration compatibility.
///
/// Returns (leaf1_ecx, leaf1_edx, leaf7_ebx, leaf7_ecx, leaf7_edx, xcr0).
pub fn query_host_features() -> (u32, u32, u32, u32, u32, u32) {
    let (_, _, ecx1, edx1) = host_cpuid(1, 0);

    let (max_leaf, _, _, _) = host_cpuid(0, 0);
    let (ebx7, ecx7, edx7) = if max_leaf >= 7 {
        let (_, b, c, d) = host_cpuid(7, 0);
        (b, c, d)
    } else {
        (0, 0, 0)
    };

    let xcr0 = if max_leaf >= 0xD {
        let (a, _, _, _) = host_cpuid(0xD, 0);
        a
    } else {
        0x3 // x87 + SSE minimum
    };

    (ecx1, edx1, ebx7, ecx7, edx7, xcr0)
}
