// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use flate2::{Decompress, FlushDecompress};

use super::*;

/// A client's side of one connection: one zlib stream for every
/// rectangle.
pub(crate) struct Decoder {
    z: Decompress,
}

impl Decoder {
    pub(crate) fn new() -> Self {
        Self {
            z: Decompress::new(true),
        }
    }

    /// Decode one rectangle body back to XRGB8888 with X = 0, and
    /// report each tile's subencoding.
    pub(crate) fn decode(
        &mut self,
        body: &[u8],
        width: u16,
        height: u16,
    ) -> (Vec<u8>, Vec<u8>) {
        let len =
            u32::from_be_bytes(body[..4].try_into().expect("length")) as usize;
        assert_eq!(len, body.len() - 4, "the length prefix covers the data");
        let mut raw = Vec::with_capacity(
            usize::from(width) * usize::from(height) * 4 + 1024,
        );
        self.z
            .decompress_vec(&body[4..], &mut raw, FlushDecompress::Sync)
            .expect("valid zlib data");
        let (w, h) = (usize::from(width), usize::from(height));
        let mut out = vec![0u8; w * h * 4];
        let mut subencodings = Vec::new();
        let mut at = 0;
        for ty in (0..h).step_by(64) {
            for tx in (0..w).step_by(64) {
                let (tw, th) = ((w - tx).min(64), (h - ty).min(64));
                let sub = raw[at];
                at += 1;
                subencodings.push(sub);
                let mut put = |x: usize, y: usize, c: &[u8]| {
                    let o = ((ty + y) * w + tx + x) * 4;
                    out[o..o + 3].copy_from_slice(c);
                };
                match sub {
                    0 => {
                        for y in 0..th {
                            for x in 0..tw {
                                put(x, y, &raw[at..at + 3]);
                                at += 3;
                            }
                        }
                    }
                    1 => {
                        let c = raw[at..at + 3].to_vec();
                        at += 3;
                        for y in 0..th {
                            for x in 0..tw {
                                put(x, y, &c);
                            }
                        }
                    }
                    other => panic!("unexpected subencoding {other}"),
                }
            }
        }
        assert_eq!(at, raw.len(), "no bytes left over");
        (out, subencodings)
    }
}

/// XRGB8888 pixels with a pattern, X bytes set so the encoder has to
/// drop them.
fn pattern(width: u16, height: u16) -> Vec<u8> {
    (0..u32::from(width) * u32::from(height))
        .flat_map(|i| {
            (0xFF00_0000 | i.wrapping_mul(2_654_435_761) >> 8).to_le_bytes()
        })
        .collect()
}

pub(crate) fn without_x(pixels: &[u8]) -> Vec<u8> {
    pixels
        .chunks(4)
        .flat_map(|p| [p[0], p[1], p[2], 0])
        .collect()
}

#[test]
fn a_rectangle_decodes_to_its_pixels() {
    let mut enc = ZrleEncoder::new();
    let pixels = pattern(100, 70);
    let mut body = Vec::new();
    enc.encode(&pixels, 100, 70, &mut body).expect("encode");

    let (out, subs) = Decoder::new().decode(&body, 100, 70);
    assert_eq!(out, without_x(&pixels));
    // 64-pixel tiles: two columns, two rows, the edges cut short.
    assert_eq!(subs.len(), 4);
}

#[test]
fn a_uniform_tile_is_sent_solid() {
    let mut enc = ZrleEncoder::new();
    let pixels: Vec<u8> = [0x12, 0x34, 0x56, 0xFF].repeat(64 * 64);
    let mut body = Vec::new();
    enc.encode(&pixels, 64, 64, &mut body).expect("encode");

    let (out, subs) = Decoder::new().decode(&body, 64, 64);
    assert_eq!(subs, [1]);
    assert_eq!(out, without_x(&pixels));
}

/// The zlib stream runs for the whole connection: the client's
/// inflater carries its window from one rectangle to the next.
#[test]
fn rectangles_share_one_stream() {
    let mut enc = ZrleEncoder::new();
    let mut dec = Decoder::new();
    for (w, h) in [(64, 64), (130, 10), (1, 1)] {
        let pixels = pattern(w, h);
        let mut body = Vec::new();
        enc.encode(&pixels, w, h, &mut body).expect("encode");
        let (out, _) = dec.decode(&body, w, h);
        assert_eq!(out, without_x(&pixels), "{w}x{h}");
    }
}

#[test]
fn a_flat_screen_compresses() {
    let mut enc = ZrleEncoder::new();
    let pixels: Vec<u8> = [0x80, 0x80, 0x80, 0].repeat(1280 * 800);
    let mut body = Vec::new();
    enc.encode(&pixels, 1280, 800, &mut body).expect("encode");
    assert!(body.len() < pixels.len() / 100, "{} bytes", body.len());
    let (out, _) = Decoder::new().decode(&body, 1280, 800);
    assert_eq!(out, without_x(&pixels));
}
