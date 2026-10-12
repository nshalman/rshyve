// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! RLGR1 entropy encoding (MS-RDPRFX 3.1.8.1.7.1).
//!
//! A port of ironrdp-graphics 0.9's `rlgr::encode` for RLGR1, with one
//! difference: after a zero in Golomb-Rice mode the adaptation parameter
//! grows by UQ_GR, as the spec, FreeRDP and IronRDP's own decoder have
//! it. IronRDP's encoder adds UP_GR there, so every decoder falls out of
//! step after the first such zero and the rest of the tile decodes
//! wrong. This module can go once IronRDP's encoder is fixed.

/// Largest value of the adaptation parameters.
const KP_MAX: u32 = 80;
/// Fractional bits of the adaptation parameters.
const LS_GR: u32 = 3;
/// Run-length mode: growth after a full run of zeros.
const UP_GR: u32 = 4;
/// Run-length mode: shrink after a run ends in a value.
const DN_GR: u32 = 6;
/// Golomb-Rice mode: growth after a zero.
const UQ_GR: u32 = 3;
/// Golomb-Rice mode: shrink after a non-zero value.
const DQ_GR: u32 = 3;

/// Bits written most significant first.
struct BitWriter<'a> {
    out: &'a mut Vec<u8>,
    acc: u8,
    used: u32,
}

impl<'a> BitWriter<'a> {
    fn new(out: &'a mut Vec<u8>) -> Self {
        Self {
            out,
            acc: 0,
            used: 0,
        }
    }

    fn bit(&mut self, bit: bool) {
        self.acc = (self.acc << 1) | u8::from(bit);
        self.used += 1;
        if self.used == 8 {
            self.out.push(self.acc);
            (self.acc, self.used) = (0, 0);
        }
    }

    fn repeat(&mut self, count: u32, bit: bool) {
        for _ in 0..count {
            self.bit(bit);
        }
    }

    /// The low `count` bits of `value`, most significant first.
    fn bits(&mut self, count: u32, value: u32) {
        for i in (0..count).rev() {
            self.bit(value >> i & 1 == 1);
        }
    }

    /// Pad the last byte with zeros.
    fn finish(mut self) {
        if self.used > 0 {
            self.acc <<= 8 - self.used;
            self.out.push(self.acc);
        }
    }
}

/// Append the RLGR1 code for `input` to `out`.
pub(crate) fn encode(input: &[i16], out: &mut Vec<u8>) {
    let mut w = BitWriter::new(out);
    let mut kp: u32 = 1 << LS_GR;
    let mut krp: u32 = 1 << LS_GR;
    let mut i = 0;
    while i < input.len() {
        let k = kp >> LS_GR;
        if k != 0 {
            // Run-length mode: a run of zeros, then the value ending it.
            let mut zeros =
                input[i..].iter().take_while(|v| **v == 0).count() as u32;
            i += zeros as usize;
            let mut k = k;
            while zeros >= 1 << k {
                w.bit(false);
                zeros -= 1 << k;
                kp = (kp + UP_GR).min(KP_MAX);
                k = kp >> LS_GR;
            }
            w.bit(true);
            w.bits(k, zeros);
            if let Some(&value) = input.get(i) {
                i += 1;
                w.bit(value < 0);
                golomb_rice(
                    &mut w,
                    &mut krp,
                    u32::from(value.unsigned_abs()) - 1,
                );
            }
            kp = kp.saturating_sub(DN_GR);
        } else {
            // Golomb-Rice mode: one value, folded to 2|v| or 2|v| - 1.
            let value = input[i];
            i += 1;
            let folded =
                u32::from(value.unsigned_abs()) * 2 - u32::from(value < 0);
            golomb_rice(&mut w, &mut krp, folded);
            if folded == 0 {
                kp = (kp + UQ_GR).min(KP_MAX);
            } else {
                kp = kp.saturating_sub(DQ_GR);
            }
        }
    }
    w.finish();
}

/// Golomb-Rice code `value` with parameter `krp >> LS_GR`, then adapt.
fn golomb_rice(w: &mut BitWriter<'_>, krp: &mut u32, value: u32) {
    let kr = *krp >> LS_GR;
    let quotient = value >> kr;
    w.repeat(quotient, true);
    w.bit(false);
    w.bits(kr, value & ((1 << kr) - 1));
    if quotient == 0 {
        *krp = krp.saturating_sub(2);
    } else if quotient > 1 {
        *krp = (*krp + quotient).min(KP_MAX);
    }
}

#[cfg(test)]
mod tests {
    use ironrdp_graphics::rlgr;
    use ironrdp_pdu::codecs::rfx::EntropyAlgorithm;

    use super::*;

    /// Encode here, decode with IronRDP, whose decoder follows the spec.
    fn round_trip(input: &[i16]) {
        let mut out = Vec::new();
        encode(input, &mut out);
        let mut back = vec![0i16; input.len()];
        rlgr::decode(EntropyAlgorithm::Rlgr1, &out, &mut back)
            .expect("decodes");
        let first_bad = input.iter().zip(&back).position(|(a, b)| a != b);
        assert_eq!(first_bad, None, "first mismatch at {first_bad:?}");
    }

    /// The wavelet coefficients of a sharp edge: long zero runs, then
    /// values, then zeros again in Golomb-Rice mode. IronRDP 0.9's own
    /// encoder desynchronizes here, adding UP_GR where the spec and its
    /// decoder add UQ_GR after a zero.
    #[test]
    fn a_sharp_edge_round_trips() {
        let mut coef = [0i16; 4096];
        for r in 0..64 {
            for c in 0..36 {
                coef[r * 64 + c] = 300;
            }
        }
        let mut temp = [0i16; 4096];
        ironrdp_graphics::dwt::encode(&mut coef, &mut temp);
        round_trip(&coef);
    }

    #[test]
    fn zeros_round_trip() {
        round_trip(&[0i16; 4096]);
    }

    #[test]
    fn large_magnitudes_round_trip() {
        let input: Vec<i16> = (0..4096)
            .map(|i| {
                if i % 7 == 0 {
                    i16::MAX - i as i16
                } else {
                    -(i as i16)
                }
            })
            .collect();
        round_trip(&input);
    }

    /// Runs of zeros of every length between values of both signs, so
    /// both modes and every adaptation step are exercised.
    #[test]
    fn mixed_runs_round_trip() {
        let mut input = Vec::new();
        let mut v: i16 = 1;
        for run in 0..90 {
            input.extend(std::iter::repeat_n(0, run % 23));
            input.push(v);
            input.push(0);
            v = -(v + (run as i16 % 5));
        }
        input.resize(4096, 0);
        round_trip(&input);
    }
}
