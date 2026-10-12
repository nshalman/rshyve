// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ZRLE encoding (RFC 6143 section 7.7.6).
//!
//! A rectangle is cut into 64x64 tiles, left to right and top to
//! bottom, each sent as one solid colour when it is uniform and as raw
//! pixels when it is not. The tiles go through a zlib stream that lasts
//! for the connection: the client's inflater keeps its window from one
//! rectangle to the next, so the encoder must too, and each rectangle
//! ends on a sync flush.
//!
//! Pixels are XRGB8888 little-endian, depth 24, so a ZRLE CPIXEL is the
//! three low bytes of each pixel.

use std::io;

use flate2::{Compress, Compression, FlushCompress, Status};

/// Tile edge, fixed by the encoding.
const TILE: usize = 64;

const SUBENCODING_RAW: u8 = 0;
const SUBENCODING_SOLID: u8 = 1;

/// Bytes of an XRGB8888 pixel, and of the CPIXEL it becomes.
const PIXEL: usize = 4;
const CPIXEL: usize = 3;

/// Encoder for one connection's ZRLE rectangles.
pub struct ZrleEncoder {
    z: Compress,
    /// The tiles of the rectangle being encoded, before compression.
    tiles: Vec<u8>,
}

impl Default for ZrleEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl ZrleEncoder {
    pub fn new() -> Self {
        Self {
            // The fastest level: a remote display wants latency more
            // than the last few percent of size.
            z: Compress::new(Compression::fast(), true),
            tiles: Vec::new(),
        }
    }

    /// Append the ZRLE body of a `width` x `height` rectangle whose
    /// pixels are `pixels` (rows packed) to `dst`: a 4-byte big-endian
    /// length, then that many bytes of the zlib stream.
    pub fn encode(
        &mut self,
        pixels: &[u8],
        width: u16,
        height: u16,
        dst: &mut Vec<u8>,
    ) -> io::Result<()> {
        let (w, h) = (usize::from(width), usize::from(height));
        if pixels.len() != w * h * PIXEL {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pixels do not match the rectangle",
            ));
        }
        self.tiles.clear();
        for ty in (0..h).step_by(TILE) {
            for tx in (0..w).step_by(TILE) {
                let (tw, th) = ((w - tx).min(TILE), (h - ty).min(TILE));
                self.tile(pixels, w, tx, ty, tw, th);
            }
        }

        let start = dst.len();
        dst.extend_from_slice(&[0; 4]);
        self.deflate(dst)?;
        let len = u32::try_from(dst.len() - start - 4).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "ZRLE body too long")
        })?;
        dst[start..start + 4].copy_from_slice(&len.to_be_bytes());
        Ok(())
    }

    /// Append one tile, solid or raw, to `self.tiles`.
    fn tile(
        &mut self,
        pixels: &[u8],
        stride: usize,
        tx: usize,
        ty: usize,
        tw: usize,
        th: usize,
    ) {
        let at = |x: usize, y: usize| ((ty + y) * stride + tx + x) * PIXEL;
        let first = &pixels[at(0, 0)..at(0, 0) + CPIXEL];
        let solid = (0..th).all(|y| {
            let row = &pixels[at(0, y)..at(0, y) + tw * PIXEL];
            row.as_chunks::<PIXEL>()
                .0
                .iter()
                .all(|p| &p[..CPIXEL] == first)
        });
        if solid {
            self.tiles.push(SUBENCODING_SOLID);
            self.tiles.extend_from_slice(first);
            return;
        }
        self.tiles.push(SUBENCODING_RAW);
        for y in 0..th {
            let row = &pixels[at(0, y)..at(0, y) + tw * PIXEL];
            for p in row.as_chunks::<PIXEL>().0 {
                self.tiles.extend_from_slice(&p[..CPIXEL]);
            }
        }
    }

    /// Compress `self.tiles` onto `dst`, ending on a sync flush so the
    /// client can decode the rectangle without the next one.
    fn deflate(&mut self, dst: &mut Vec<u8>) -> io::Result<()> {
        let mut input = &self.tiles[..];
        loop {
            dst.reserve(input.len() / 2 + 1024);
            let before = self.z.total_in();
            let status = self
                .z
                .compress_vec(input, dst, FlushCompress::Sync)
                .map_err(io::Error::other)?;
            if status == Status::StreamEnd {
                return Err(io::Error::other("zlib stream ended"));
            }
            input = &input[(self.z.total_in() - before) as usize..];
            // The flush is complete once all input is in and the
            // compressor stopped short of the space it was given.
            if input.is_empty() && dst.len() < dst.capacity() {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
