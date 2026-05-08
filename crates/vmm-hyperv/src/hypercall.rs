// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Hypercall MSR + page.
//!
//! TLFS section 3.13. The guest writes a GPA into HV_X64_MSR_HYPERCALL;
//! the hypervisor populates that page with a code sequence the guest
//! can call into. We do not implement any actual hypercalls — the
//! published page returns "not supported" without trapping. This is
//! sufficient for Windows' boot-time probe; performance hypercalls
//! (TLB flush, IPI) are part of the Tier 2 scope and are not enabled
//! here.

use crate::overlay::PAGE_SIZE;

/// HYPERCALL MSR layout (TLFS 3.13):
///   bit 0     Enabled
///   bit 1     Locked (once set, MSR becomes write-protected)
///   bits 2-11 Reserved (must be 0)
///   bits 12-63 Hypercall page PFN
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MsrHypercallValue(pub u64);

impl MsrHypercallValue {
    pub const ENABLED: u64 = 1 << 0;
    pub const LOCKED: u64 = 1 << 1;
    pub const PFN_SHIFT: u64 = 12;
    pub const PFN_MASK: u64 = !0xFFF;

    pub fn enabled(self) -> bool {
        self.0 & Self::ENABLED != 0
    }
    pub fn locked(self) -> bool {
        self.0 & Self::LOCKED != 0
    }
    pub fn gpa(self) -> u64 {
        self.0 & Self::PFN_MASK
    }
    pub fn raw(self) -> u64 {
        self.0
    }

    /// Apply a guest write, honoring the sticky `Locked` bit.
    ///
    /// Returns the new value. The guest cannot clear `Locked` once
    /// set, and cannot change the PFN/Enabled while locked.
    pub fn apply_write(self, new: u64) -> Self {
        if self.locked() {
            return self;
        }
        Self(new)
    }
}

/// Build the 4 KiB hypercall page contents.
///
/// The first 8 bytes are `mov rax, 2; ret` — i.e. set the return
/// status to "HV_STATUS_INVALID_HYPERCALL_CODE" and return without
/// trapping. Everything else is zero. Windows accepts this and never
/// uses the page for performance-critical paths if the corresponding
/// CPUID/feature bits aren't set, which is our case.
pub fn build_page() -> Box<[u8; PAGE_SIZE]> {
    let mut page = Box::new([0u8; PAGE_SIZE]);
    // 48 c7 c0 02 00 00 00   mov rax, 2
    // c3                     ret
    page[0..8].copy_from_slice(&[0x48, 0xc7, 0xc0, 0x02, 0x00, 0x00, 0x00, 0xc3]);
    page
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locked_bit_is_sticky() {
        let v = MsrHypercallValue(MsrHypercallValue::LOCKED | MsrHypercallValue::ENABLED);
        assert!(v.locked());
        // Guest tries to clear locked + disable; both ignored.
        let v2 = v.apply_write(0);
        assert!(v2.locked());
        assert!(v2.enabled());
        assert_eq!(v2.0, v.0);
    }

    #[test]
    fn unlocked_writes_pass_through() {
        let v = MsrHypercallValue(0);
        let new = MsrHypercallValue::ENABLED | (0xdead_0000 & MsrHypercallValue::PFN_MASK);
        let v2 = v.apply_write(new);
        assert!(v2.enabled());
        assert_eq!(v2.gpa(), 0xdead_0000);
    }

    #[test]
    fn page_starts_with_mov_ret() {
        let page = build_page();
        assert_eq!(&page[0..8], &[0x48, 0xc7, 0xc0, 0x02, 0x00, 0x00, 0x00, 0xc3]);
        assert!(page[8..].iter().all(|&b| b == 0));
    }
}
