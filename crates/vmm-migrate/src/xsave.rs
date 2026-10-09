// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Guest FPU and XSAVE state across hosts (OSF-2).
//!
//! Each kernel reads and writes the XSAVE area in its own host's
//! standard format, sized for every component the host enabled. Two
//! hosts can differ in size and in the components they have, so the
//! source sends its layout and the destination moves each component the
//! guest can use to the same offset in a local area. A component the
//! guest can use must have the same size and offset on both hosts, or
//! the guest's registers would land in the wrong bytes.
//!
//! The area is checked here before the kernel sees it: a header with
//! reserved or compacted bits faults the first XRSTOR.

use serde::{Deserialize, Serialize};
use vmm_core::hdl::{FpuArea, VmmHdl, MAX_FPU_AREA_LEN};

use crate::codec::MigrateError;

/// The legacy x87/SSE region.
const LEGACY_LEN: usize = 512;
/// The legacy region and the XSAVE header.
const HEADER_END: usize = LEGACY_LEN + 64;
const XSTATE_BV: usize = LEGACY_LEN;
const XCOMP_BV: usize = LEGACY_LEN + 8;
/// x87 and SSE live in the legacy region on every host.
const LEGACY_FEATURES: u64 = 0b11;

/// One XSAVE state component in the standard format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct XsaveComponent {
    /// The component's XCR0 bit, as a mask.
    pub feature: u64,
    pub size: u32,
    pub offset: u32,
}

/// A host's XSAVE area: the size the kernel reads and writes, and where
/// each component the host enabled sits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XsaveLayout {
    pub size: u32,
    pub components: Vec<XsaveComponent>,
}

impl XsaveLayout {
    /// This host's layout.
    pub fn local(hdl: &VmmHdl) -> Result<Self, MigrateError> {
        let area = hdl.fpu_area().map_err(|e| {
            MigrateError::VmmData(format!("describe the FPU area: {e}"))
        })?;
        let layout = Self::from_area(&area)?;
        layout
            .validate()
            .map_err(|why| MigrateError::State(format!("local {why}")))?;
        Ok(layout)
    }

    fn from_area(area: &FpuArea) -> Result<Self, MigrateError> {
        Ok(Self {
            size: u32::try_from(area.size).map_err(|_| {
                MigrateError::State(format!("FPU area of {} bytes", area.size))
            })?,
            components: area
                .components
                .iter()
                .map(|c| XsaveComponent {
                    feature: c.feature,
                    size: c.size,
                    offset: c.offset,
                })
                .collect(),
        })
    }

    pub fn size(&self) -> usize {
        self.size as usize
    }

    fn features(&self) -> u64 {
        self.components.iter().fold(0, |all, c| all | c.feature)
    }

    fn find(&self, feature: u64) -> Option<&XsaveComponent> {
        self.components.iter().find(|c| c.feature == feature)
    }

    /// Whether every component is inside the area, once, and where the
    /// architecture puts the legacy ones. The peer chooses every field.
    pub fn validate(&self) -> Result<(), String> {
        let size = self.size();
        if !(HEADER_END..=MAX_FPU_AREA_LEN).contains(&size) {
            return Err(format!(
                "XSAVE area of {size} bytes is outside \
                 {HEADER_END}..={MAX_FPU_AREA_LEN}",
            ));
        }
        let mut seen = 0u64;
        for c in &self.components {
            if c.feature.count_ones() != 1 {
                return Err(format!(
                    "XSAVE component {:#x} is not one feature",
                    c.feature,
                ));
            }
            if seen & c.feature != 0 {
                return Err(format!(
                    "XSAVE component {:#x} appears twice",
                    c.feature,
                ));
            }
            seen |= c.feature;
            let (offset, len) = (c.offset as usize, c.size as usize);
            let fits = if c.feature & LEGACY_FEATURES != 0 {
                offset == 0 && len == LEGACY_LEN
            } else {
                offset >= HEADER_END
                    && offset.checked_add(len).is_some_and(|end| end <= size)
            };
            if !fits {
                return Err(format!(
                    "XSAVE component {:#x} at {offset}+{len} does not fit \
                     a {size}-byte area",
                    c.feature,
                ));
            }
        }
        if seen & LEGACY_FEATURES != LEGACY_FEATURES {
            return Err("XSAVE layout has no x87 or SSE component".into());
        }
        Ok(())
    }
}

/// How the destination turns the source's areas into its own.
pub struct XsavePlan<'a> {
    pub source: &'a XsaveLayout,
    pub local: &'a XsaveLayout,
    /// The XCR0 bits the guest was offered. x87 and SSE always count.
    pub guest: u64,
}

impl XsavePlan<'_> {
    fn guest_can_use(&self, feature: u64) -> bool {
        feature & (self.guest | LEGACY_FEATURES) != 0
    }

    /// Refuse a source whose guest can use a component this host cannot
    /// hold at the same place. Run before a page moves.
    pub fn check(&self) -> Result<(), String> {
        self.source
            .validate()
            .map_err(|why| format!("source {why}"))?;
        // A guest bit with no source component was never enabled on the
        // source host, and the kernel keeps guest XCR0 inside host XCR0,
        // so the guest has no state there to lose.
        let lost: Vec<String> = self
            .source
            .components
            .iter()
            .filter(|c| self.guest_can_use(c.feature))
            .filter(|c| self.local.find(c.feature) != Some(*c))
            .map(|c| format!("{:#x}", c.feature))
            .collect();
        if lost.is_empty() {
            return Ok(());
        }
        Err(format!(
            "this host cannot hold the guest's XSAVE components {} at the \
             source's size and offset",
            lost.join(" "),
        ))
    }

    /// The local area for one vCPU's source area.
    ///
    /// A component the guest was never offered and this host does not
    /// hold the same way goes back to its initial state: the guest
    /// cannot read it.
    pub fn translate(
        &self,
        vcpuid: i32,
        src: &[u8],
    ) -> Result<Vec<u8>, String> {
        let fail = |why: String| format!("vcpu {vcpuid} FPU: {why}");
        self.source
            .validate()
            .map_err(|why| fail(format!("source {why}")))?;
        if src.len() != self.source.size() {
            return Err(fail(format!(
                "{} bytes, the source area is {}",
                src.len(),
                self.source.size(),
            )));
        }
        let word = |at: usize| {
            u64::from_le_bytes(src[at..at + 8].try_into().expect("8 bytes"))
        };
        if word(XCOMP_BV) != 0 {
            return Err(fail("the area is in the compacted format".into()));
        }
        if src[XCOMP_BV + 8..HEADER_END].iter().any(|&b| b != 0) {
            return Err(fail("reserved XSAVE header bytes are set".into()));
        }
        let mut xstate_bv = word(XSTATE_BV);
        let unknown = xstate_bv & !self.source.features();
        if unknown != 0 {
            return Err(fail(format!(
                "XSTATE_BV {unknown:#x} names components the source lacks"
            )));
        }

        let mut out = vec![0u8; self.local.size()];
        out[..HEADER_END].copy_from_slice(&src[..HEADER_END]);
        for c in &self.source.components {
            if c.feature & LEGACY_FEATURES != 0 || xstate_bv & c.feature == 0 {
                continue;
            }
            if self.local.find(c.feature) == Some(c) {
                let at = c.offset as usize;
                let range = at..at + c.size as usize;
                out[range.clone()].copy_from_slice(&src[range]);
            } else if self.guest_can_use(c.feature) {
                return Err(fail(format!(
                    "component {:#x} has no place on this host",
                    c.feature,
                )));
            } else {
                xstate_bv &= !c.feature;
            }
        }
        out[XSTATE_BV..XSTATE_BV + 8].copy_from_slice(&xstate_bv.to_le_bytes());
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AVX: u64 = 1 << 2;
    const OPMASK: u64 = 1 << 5;
    const ZMM_HI256: u64 = 1 << 6;
    const HI16_ZMM: u64 = 1 << 7;
    const PKRU: u64 = 1 << 9;

    fn legacy() -> Vec<XsaveComponent> {
        [1, 2]
            .into_iter()
            .map(|feature| XsaveComponent {
                feature,
                size: LEGACY_LEN as u32,
                offset: 0,
            })
            .collect()
    }

    fn comp(feature: u64, offset: u32, size: u32) -> XsaveComponent {
        XsaveComponent {
            feature,
            size,
            offset,
        }
    }

    /// An AVX2 host: x87, SSE, AVX.
    fn avx2() -> XsaveLayout {
        let mut components = legacy();
        components.push(comp(AVX, 576, 256));
        XsaveLayout {
            size: 832,
            components,
        }
    }

    /// An AVX-512 host with PKRU, at the architectural offsets.
    fn avx512() -> XsaveLayout {
        let mut components = legacy();
        components.extend([
            comp(AVX, 576, 256),
            comp(OPMASK, 1088, 64),
            comp(ZMM_HI256, 1152, 512),
            comp(HI16_ZMM, 1664, 1024),
            comp(PKRU, 2688, 8),
        ]);
        XsaveLayout {
            size: 2696,
            components,
        }
    }

    /// An area with a byte pattern in each component `xstate_bv` names.
    fn area(layout: &XsaveLayout, xstate_bv: u64) -> Vec<u8> {
        let mut buf = vec![0u8; layout.size()];
        for (i, b) in buf[..LEGACY_LEN].iter_mut().enumerate() {
            *b = i as u8;
        }
        buf[XSTATE_BV..XSTATE_BV + 8].copy_from_slice(&xstate_bv.to_le_bytes());
        for c in &layout.components {
            if c.feature & LEGACY_FEATURES == 0 && xstate_bv & c.feature != 0 {
                let at = c.offset as usize;
                buf[at..at + c.size as usize]
                    .fill(c.feature.trailing_zeros() as u8);
            }
        }
        buf
    }

    fn xstate_bv(buf: &[u8]) -> u64 {
        u64::from_le_bytes(buf[XSTATE_BV..XSTATE_BV + 8].try_into().unwrap())
    }

    fn plan<'a>(
        source: &'a XsaveLayout,
        local: &'a XsaveLayout,
        guest: u64,
    ) -> XsavePlan<'a> {
        XsavePlan {
            source,
            local,
            guest,
        }
    }

    #[test]
    fn a_layout_survives_the_wire() {
        let layout = avx512();
        let text = ron::ser::to_string(&layout).expect("encode");
        let back: XsaveLayout = ron::from_str(&text).expect("decode");
        assert_eq!(back, layout);
    }

    #[test]
    fn the_same_host_round_trips_every_byte() {
        let layout = avx512();
        let src = area(&layout, LEGACY_FEATURES | AVX | OPMASK | PKRU);
        let out = plan(&layout, &layout, 0x2e7)
            .translate(0, &src)
            .expect("same layout");
        assert_eq!(out, src);
    }

    #[test]
    fn a_larger_destination_gets_the_components_at_their_offsets() {
        let (src_layout, dst_layout) = (avx2(), avx512());
        let src = area(&src_layout, LEGACY_FEATURES | AVX);
        let p = plan(&src_layout, &dst_layout, 0x7);
        p.check().expect("the destination holds AVX");
        let out = p.translate(0, &src).expect("translate");
        assert_eq!(out.len(), dst_layout.size());
        assert_eq!(&out[..832], &src[..]);
        assert!(out[832..].iter().all(|&b| b == 0));
    }

    #[test]
    fn a_destination_without_a_component_the_guest_uses_is_refused() {
        let (src_layout, dst_layout) = (avx512(), avx2());
        let p = plan(&src_layout, &dst_layout, 0xe7);
        let why = p.check().expect_err("no AVX-512 here");
        assert!(why.contains("0x20"), "{why}");
        assert!(!why.contains("0x200"), "PKRU was not offered: {why}");
        let src = area(&src_layout, LEGACY_FEATURES | AVX | ZMM_HI256);
        p.translate(0, &src).expect_err("no place for ZMM_Hi256");
    }

    #[test]
    fn a_component_the_guest_was_not_offered_returns_to_init() {
        // An AVX-512 source with an AVX2 baseline: the guest saw only
        // AVX, so it migrates to an AVX2 host.
        let (src_layout, dst_layout) = (avx512(), avx2());
        let p = plan(&src_layout, &dst_layout, 0x7);
        p.check().expect("the guest saw only AVX");
        let src = area(&src_layout, LEGACY_FEATURES | AVX | PKRU);
        let out = p.translate(0, &src).expect("translate");
        assert_eq!(out.len(), dst_layout.size());
        assert_eq!(xstate_bv(&out), LEGACY_FEATURES | AVX);
        assert_eq!(&out[576..832], &src[576..832]);
    }

    #[test]
    fn a_moved_component_is_refused() {
        let src_layout = avx2();
        let mut dst_layout = avx2();
        dst_layout.size = 1088;
        dst_layout.components[2].offset = 832;
        plan(&src_layout, &dst_layout, 0x7)
            .check()
            .expect_err("AVX at another offset");
    }

    #[test]
    fn an_area_of_the_wrong_length_is_refused() {
        let layout = avx2();
        let mut src = area(&layout, LEGACY_FEATURES);
        src.pop();
        let why = plan(&layout, &layout, 0x7)
            .translate(3, &src)
            .expect_err("short");
        assert!(why.starts_with("vcpu 3 FPU"), "{why}");
    }

    #[test]
    fn an_empty_area_is_refused() {
        // What a source that carries no FPU state sends.
        let layout = avx2();
        plan(&layout, &layout, 0x7)
            .translate(0, &[])
            .expect_err("no FPU state");
    }

    #[test]
    fn a_header_that_faults_xrstor_is_refused() {
        let layout = avx2();
        let p = plan(&layout, &layout, 0x7);

        let mut compacted = area(&layout, LEGACY_FEATURES);
        compacted[XCOMP_BV + 7] = 0x80;
        p.translate(0, &compacted).expect_err("compacted");

        let mut reserved = area(&layout, LEGACY_FEATURES);
        reserved[HEADER_END - 1] = 1;
        p.translate(0, &reserved)
            .expect_err("reserved header bytes");

        let unknown = area(&layout, LEGACY_FEATURES | OPMASK);
        p.translate(0, &unknown)
            .expect_err("a component the source lacks");
    }

    #[test]
    fn a_layout_the_peer_made_up_is_refused() {
        let mut big = avx2();
        big.size = (MAX_FPU_AREA_LEN + 1) as u32;
        big.validate().expect_err("past the kernel's limit");

        let mut outside = avx2();
        outside.components[2].offset = 700;
        outside.validate().expect_err("past the end of the area");

        let mut overflow = avx2();
        overflow.components[2].offset = u32::MAX;
        overflow.validate().expect_err("offset overflow");

        let mut in_header = avx2();
        in_header.components[2].offset = 520;
        in_header.validate().expect_err("inside the header");

        let mut twice = avx2();
        twice.components.push(comp(AVX, 576, 256));
        twice.validate().expect_err("one component twice");

        let mut two_bits = avx2();
        two_bits.components[2].feature = AVX | OPMASK;
        two_bits.validate().expect_err("two features in one");

        let mut no_sse = avx2();
        no_sse.components.remove(1);
        no_sse.validate().expect_err("no SSE");

        let mut moved_sse = avx2();
        moved_sse.components[1].offset = 576;
        moved_sse
            .validate()
            .expect_err("SSE outside the legacy region");

        avx2().validate().expect("a real AVX2 layout");
        avx512().validate().expect("a real AVX-512 layout");
    }
}
