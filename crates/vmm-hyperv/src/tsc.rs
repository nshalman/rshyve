// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Reference TSC overlay page (TLFS 12.7).
//!
//! Windows reads RDTSC and converts it to 100 ns units using a
//! per-VM scaling factor + offset that the hypervisor publishes in a
//! guest-physical page. Avoids the slow ACPI PM timer fallback that
//! Windows otherwise uses; this is the largest single perf win in the
//! Tier 1 enlightenment set.

use std::mem::size_of;

use crate::overlay::PAGE_SIZE;

/// REFERENCE_TSC MSR layout (same shape as HYPERCALL — see TLFS 12.7.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MsrReferenceTscValue(pub u64);

impl MsrReferenceTscValue {
    pub const ENABLED: u64 = 1 << 0;
    pub const PFN_MASK: u64 = !0xFFF;

    pub fn enabled(self) -> bool {
        self.0 & Self::ENABLED != 0
    }
    pub fn gpa(self) -> u64 {
        self.0 & Self::PFN_MASK
    }
    pub fn raw(self) -> u64 {
        self.0
    }
}

/// Reference TSC page layout (TLFS 12.7.3).
///
/// Guest formula: `time_in_100ns = ((rdtsc * scale) >> 64) + offset`.
/// `sequence == 0` tells the guest the page is invalid and to fall
/// back to the OS clock; we set sequence = 1 and never change it.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct ReferenceTscPage {
    pub sequence: u32,
    pub reserved: u32,
    pub scale: u64,
    pub offset: i64,
}

impl ReferenceTscPage {
    /// Compose the 4 KiB page bytes.
    pub fn into_page(self) -> Box<[u8; PAGE_SIZE]> {
        let mut page = Box::new([0u8; PAGE_SIZE]);
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &self as *const Self as *const u8,
                size_of::<Self>(),
            )
        };
        page[..bytes.len()].copy_from_slice(bytes);
        page
    }
}

/// Compute the 0.64 fixed-point scale factor that converts a guest
/// TSC tick into 100 ns units: `scale = (1e7 << 64) / guest_freq_hz`.
///
/// Returns `None` if `guest_freq_hz` is zero or so low that the
/// scaling factor would not fit in 64 bits (i.e. < 10 MHz, which is
/// not a real x86 part). The caller should publish sequence = 0 in
/// that case so the guest falls back to PM timer.
pub fn compute_scale(guest_freq_hz: u64) -> Option<u64> {
    if guest_freq_hz == 0 {
        return None;
    }
    const HUNDRED_NS_PER_SEC: u128 = 10_000_000;
    let scale: u128 = (HUNDRED_NS_PER_SEC << 64) / guest_freq_hz as u128;
    if (scale >> 64) != 0 {
        None
    } else {
        Some(scale as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify a few realistic guest TSC frequencies round-trip:
    /// applying the scale to a one-second-worth of ticks should yield
    /// 1e7 100ns units (== 1 second), within fixed-point rounding.
    #[track_caller]
    fn roundtrip(freq_hz: u64) {
        let scale = compute_scale(freq_hz).expect("in range");
        let ticks = freq_hz as u128;
        let hundred_ns = (ticks * scale as u128) >> 64;
        // Allow 1 unit of rounding error.
        assert!(
            (hundred_ns as i64 - 10_000_000).abs() <= 1,
            "freq={} scale={:#x} got={}",
            freq_hz,
            scale,
            hundred_ns
        );
    }

    #[test]
    fn typical_frequencies_roundtrip() {
        roundtrip(2_400_000_000); // 2.4 GHz
        roundtrip(3_000_000_000); // 3.0 GHz
        roundtrip(3_600_000_000); // 3.6 GHz
        roundtrip(2_100_000_000); // 2.1 GHz (NUC AVX2 box)
    }

    #[test]
    fn zero_frequency_rejected() {
        assert_eq!(compute_scale(0), None);
    }

    #[test]
    fn page_layout_matches_tlfs() {
        // sequence=4, reserved=4, scale=8, offset=8 → 24 bytes.
        assert_eq!(size_of::<ReferenceTscPage>(), 24);
    }

    #[test]
    fn page_serialization_round_trips() {
        let p = ReferenceTscPage {
            sequence: 1,
            reserved: 0,
            scale: 0x1234_5678_9abc_def0,
            offset: -42,
        };
        let bytes = p.into_page();
        assert_eq!(&bytes[0..4], &1u32.to_le_bytes());
        assert_eq!(&bytes[8..16], &0x1234_5678_9abc_def0u64.to_le_bytes());
        assert_eq!(&bytes[16..24], &(-42i64).to_le_bytes());
        assert!(bytes[24..].iter().all(|&b| b == 0));
    }
}
