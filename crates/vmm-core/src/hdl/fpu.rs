// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest FPU state: the host's XSAVE area layout, and one vCPU's area.

use std::io::{Error, ErrorKind, Result};

use bhyve_api::{ioctls, vm_fpu_desc, vm_fpu_desc_entry, vm_fpu_state};

use super::VmmHdl;

/// The kernel refuses a description request for more entries than this.
const MAX_FPU_DESC_ENTRIES: u32 = 64;

/// The kernel refuses a VM_GET_FPU or VM_SET_FPU buffer past two pages.
pub const MAX_FPU_AREA_LEN: usize = 2 * crate::common::PAGE_SIZE;

/// One XSAVE state component in the standard (non-compacted) format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FpuComponent {
    /// The component's XCR0 bit, as a mask.
    pub feature: u64,
    pub size: u32,
    pub offset: u32,
}

/// What VM_GET_FPU fills and VM_SET_FPU reads on this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FpuArea {
    pub size: usize,
    pub components: Vec<FpuComponent>,
}

/// The kernel's entries, each component once.
///
/// illumos `hma_fpu_describe_xsave_state` builds each bit with an `int`
/// shift, so positions 32 to 63 alias 0 to 31: every component comes
/// back twice, the second time with the size and offset of a CPUID
/// subleaf that does not exist. The first entry for a bit is the real
/// one, since the kernel walks the positions upward.
fn unique_components(entries: &[vm_fpu_desc_entry]) -> Vec<FpuComponent> {
    let mut seen = 0u64;
    entries
        .iter()
        .filter(|e| e.vfde_feature.count_ones() == 1)
        .filter(|e| {
            let first = seen & e.vfde_feature == 0;
            seen |= e.vfde_feature;
            first
        })
        .map(|e| FpuComponent {
            feature: e.vfde_feature,
            size: e.vfde_size,
            offset: e.vfde_off,
        })
        .collect()
}

impl VmmHdl {
    /// The XSAVE area layout of this host, for every component the host
    /// enabled in its own XCR0.
    pub fn fpu_area(&self) -> Result<FpuArea> {
        let mut desc = vm_fpu_desc::default();
        // SAFETY: with no entries the kernel writes only the count and
        // the size into the struct.
        unsafe { self.inner.ioctl(ioctls::VM_DESC_FPU_AREA, &mut desc) }?;
        let count = desc.vfd_num_entries;
        if count > MAX_FPU_DESC_ENTRIES {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("kernel reports {count} FPU components"),
            ));
        }
        let mut entries = vec![vm_fpu_desc_entry::default(); count as usize];
        desc.vfd_entry_data = entries.as_mut_ptr();
        desc.vfd_num_entries = count;
        // SAFETY: the kernel writes at most `count` entries, and
        // `entries` holds that many.
        unsafe { self.inner.ioctl(ioctls::VM_DESC_FPU_AREA, &mut desc) }?;
        if desc.vfd_num_entries != count {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "the FPU component count changed between two reads",
            ));
        }
        Ok(FpuArea {
            size: usize::try_from(desc.vfd_req_size).map_err(|_| {
                Error::new(ErrorKind::InvalidData, "FPU area size")
            })?,
            components: unique_components(&entries),
        })
    }

    /// Read `cpuid`'s XSAVE area into `buf`, which must be at least the
    /// host's area size.
    pub(crate) fn vcpu_get_fpu(
        &self,
        cpuid: i32,
        buf: &mut [u8],
    ) -> Result<()> {
        let mut req = vm_fpu_state {
            vcpuid: cpuid,
            buf: buf.as_mut_ptr().cast(),
            len: buf.len() as u64,
        };
        // SAFETY: the kernel writes at most `len` bytes into `buf`.
        unsafe { self.inner.ioctl(ioctls::VM_GET_FPU, &mut req) }?;
        Ok(())
    }

    /// Load `buf` as `cpuid`'s XSAVE area. The kernel checks the header
    /// and loads the area into the FPU under a trap handler before it
    /// takes it.
    pub(crate) fn vcpu_set_fpu(&self, cpuid: i32, buf: &[u8]) -> Result<()> {
        let mut req = vm_fpu_state {
            vcpuid: cpuid,
            // The kernel only reads through this pointer.
            buf: buf.as_ptr().cast_mut().cast(),
            len: buf.len() as u64,
        };
        // SAFETY: the kernel reads at most `len` bytes from `buf`.
        unsafe { self.inner.ioctl(ioctls::VM_SET_FPU, &mut req) }?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(feature: u64, size: u32, off: u32) -> vm_fpu_desc_entry {
        vm_fpu_desc_entry {
            vfde_feature: feature,
            vfde_size: size,
            vfde_off: off,
        }
    }

    #[test]
    fn an_aliased_component_is_kept_once() {
        // What the kernel reports on an AVX host: positions 32 to 34
        // alias 0 to 2, and subleaf 34 of leaf 0xD reads as zero.
        let kernel = [
            entry(1, 512, 0),
            entry(2, 512, 0),
            entry(4, 256, 576),
            entry(1, 512, 0),
            entry(2, 512, 0),
            entry(4, 0, 0),
        ];
        assert_eq!(
            unique_components(&kernel),
            [
                FpuComponent {
                    feature: 1,
                    size: 512,
                    offset: 0
                },
                FpuComponent {
                    feature: 2,
                    size: 512,
                    offset: 0
                },
                FpuComponent {
                    feature: 4,
                    size: 256,
                    offset: 576
                },
            ]
        );
    }

    #[test]
    fn a_sign_extended_bit_is_dropped() {
        // Position 31 of the same `int` shift sign-extends.
        let kernel = [entry(1, 512, 0), entry(0xFFFF_FFFF_8000_0000, 0, 0)];
        assert_eq!(unique_components(&kernel).len(), 1);
    }
}
