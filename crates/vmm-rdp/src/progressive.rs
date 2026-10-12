// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! RemoteFX Progressive encoding for EGFX.
//!
//! The display is cut into 64x64 tiles on a fixed grid. Each tile the
//! damage touches is converted to YCbCr the way classic RemoteFX does,
//! wavelet-transformed and quantized as IronRDP's tile encoder does,
//! RLGR1-coded by [`crate::rlgr1`], and sent as a
//! TILE_SIMPLE: one pass at full quality, no progressive refinement. The
//! tiles go out as one REGION between FRAME_BEGIN and FRAME_END, after
//! SYNC and CONTEXT, which every frame repeats. Tiles that run past the
//! display are padded with copies of their last real row and column, and
//! the region's rectangles clip them.
//!
//! The colour conversion is `to_64x64_ycbcr_tile`, classic RemoteFX's,
//! and not `progressive::rgba_to_ycbcr`: RemoteFX works with YCbCr scaled
//! by 32 (Y from -4096 to 4095), which is what its quantization tables
//! assume, and the progressive module's conversion is unscaled. With it,
//! quantizing the coarsest band by 6 kept only multiples of 32 of a
//! -128..127 value.

use anyhow::Context as _;
use ironrdp_graphics::color_conversion::to_64x64_ycbcr_tile;
use ironrdp_graphics::dwt_extrapolate::{self, band_layout};
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_graphics::subband_reconstruction;
use ironrdp_pdu::codecs::rfx::progressive::{
    encode_progressive_stream, ComponentCodecQuant, ProgressiveBlock,
    ProgressiveContextPdu, ProgressiveFrameBeginPdu, ProgressiveFrameEndPdu,
    ProgressiveRegion, ProgressiveSyncPdu, ProgressiveTile, TileSimple,
    FLAG_DWT_REDUCE_EXTRAPOLATE,
};
use ironrdp_pdu::codecs::rfx::RfxRectangle;
#[cfg(test)]
use vmm_devices::display::Rect;

/// Threads one frame's tiles are encoded on, at most. A full 1440p
/// frame is hundreds of tiles and tens of milliseconds on one thread;
/// four keep one display from taking a host's every core.
const ENCODE_THREADS: usize = 4;

/// Tiles worth starting a thread for.
const TILES_PER_THREAD: usize = 16;

/// How many threads `tiles` tiles are encoded on, at most `max`.
fn threads_for(tiles: usize, max: usize) -> usize {
    (tiles / TILES_PER_THREAD).min(max).max(1)
}

/// Tile edge, fixed by the codec.
const TILE: u32 = 64;
const TILE_PIXELS: usize = 64 * 64;

/// Base quantization for every component: RemoteFX's usual table,
/// coarsest bands finest-quantized. Lower is sharper and larger.
const QUANT: ComponentCodecQuant = ComponentCodecQuant {
    ll3: 6,
    hl3: 6,
    lh3: 6,
    hh3: 6,
    hl2: 7,
    lh2: 7,
    hh2: 8,
    hl1: 8,
    lh1: 8,
    hh1: 9,
};

/// Encode one component of a tile for TILE_SIMPLE: the reduce-extrapolate
/// wavelet, base quantization per band, delta coding of the LL3 band and
/// RLGR1. This is ironrdp-graphics' `encode_first_pass` at full quality,
/// but with this crate's RLGR1 encoder; see `rlgr1` for why.
fn encode_component(
    coefficients: &mut [i16; TILE_PIXELS],
    quant: &ComponentCodecQuant,
    out: &mut Vec<u8>,
) {
    let mut temp = [0i16; TILE_PIXELS];
    dwt_extrapolate::encode(coefficients, &mut temp);
    let bands = band_layout();
    for (index, band) in bands.iter().enumerate() {
        // Each band keeps its value divided by 2^(quant - 1), truncated
        // toward zero.
        let shift = quant.for_band(index).saturating_sub(1);
        for c in &mut coefficients[band.offset..band.offset + band.count()] {
            *c = if *c >= 0 {
                *c >> shift
            } else {
                -((-*c) >> shift)
            };
        }
    }
    subband_reconstruction::encode(&mut coefficients[ll3_offset()..]);
    crate::rlgr1::encode(coefficients, out);
}

/// Fill a tile plane past its `width` x `height` of real pixels with
/// copies of the last real column and row. A flat fill would put an edge
/// there, and a quantized wavelet rings at edges into the real pixels.
fn pad_by_replication(
    plane: &mut [i16; TILE_PIXELS],
    width: usize,
    height: usize,
) {
    let side = TILE as usize;
    for row in 0..height {
        let last = plane[row * side + width - 1];
        plane[row * side + width..(row + 1) * side].fill(last);
    }
    for row in height..side {
        plane.copy_within((height - 1) * side..height * side, row * side);
    }
}

/// Where the LL3 band starts: the last band in the layout.
fn ll3_offset() -> usize {
    let [.., ll3] = band_layout();
    ll3.offset
}

/// One tile's coefficient streams, Y, Cb and Cr, and where it is.
struct EncodedTile {
    x_idx: u16,
    y_idx: u16,
    streams: [Vec<u8>; 3],
    rect: RfxRectangle,
}

/// Encode tile (`tx`, `ty`) of the `w` x `h` display in `pixels`.
fn encode_tile(
    pixels: &[u8],
    w: u32,
    h: u32,
    tx: u32,
    ty: u32,
) -> anyhow::Result<EncodedTile> {
    anyhow::ensure!(
        tx * TILE < w && ty * TILE < h,
        "tile ({tx}, {ty}) is outside the display"
    );
    // Tile indices and edges fit u16: the display does.
    let (x0, y0) = (tx * TILE, ty * TILE);
    let (tw, th) = ((x0 + TILE).min(w) - x0, (y0 + TILE).min(h) - y0);
    let mut ycbcr = [[0i16; TILE_PIXELS]; 3];
    let [y, cb, cr] = &mut ycbcr;
    let origin = ((y0 * w + x0) * 4) as usize;
    to_64x64_ycbcr_tile(
        &pixels[origin..],
        tw,
        th,
        w * 4,
        PixelFormat::BgrX32,
        y,
        cb,
        cr,
    )
    .map_err(|e| anyhow::anyhow!("colour conversion: {e:?}"))?;
    for plane in &mut ycbcr {
        pad_by_replication(plane, tw as usize, th as usize);
    }
    let mut streams: [Vec<u8>; 3] = Default::default();
    for (c, out) in ycbcr.iter_mut().zip(streams.iter_mut()) {
        encode_component(c, &QUANT, out);
    }
    Ok(EncodedTile {
        x_idx: tx as u16,
        y_idx: ty as u16,
        streams,
        rect: RfxRectangle {
            x: x0 as u16,
            y: y0 as u16,
            width: tw as u16,
            height: th as u16,
        },
    })
}

/// Encode `tiles` in order, split among up to `threads` threads.
fn encode_all(
    pixels: &[u8],
    w: u32,
    h: u32,
    tiles: &[(u32, u32)],
    threads: usize,
) -> anyhow::Result<Vec<EncodedTile>> {
    let serial = |part: &[(u32, u32)]| {
        part.iter()
            .map(|&(tx, ty)| encode_tile(pixels, w, h, tx, ty))
            .collect::<anyhow::Result<Vec<_>>>()
    };
    let threads = threads_for(tiles.len(), threads);
    if threads == 1 {
        return serial(tiles);
    }
    std::thread::scope(|scope| {
        let parts = tiles
            .chunks(tiles.len().div_ceil(threads))
            .map(|part| {
                std::thread::Builder::new()
                    .name("rdp-encode".into())
                    .spawn_scoped(scope, move || serial(part))
                    .context("starting an encoding thread")
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let mut encoded = Vec::with_capacity(tiles.len());
        for part in parts {
            let part = part
                .join()
                .map_err(|_| anyhow::anyhow!("an encoding thread panicked"))?;
            encoded.extend(part?);
        }
        Ok(encoded)
    })
}

/// Encodes damaged areas of a display as progressive block streams.
pub(crate) struct ProgressiveEncoder {
    frame_index: u32,
}

impl ProgressiveEncoder {
    pub(crate) fn new() -> Self {
        Self { frame_index: 0 }
    }

    /// The progressive block stream for the tiles `dirty` touches, from
    /// `pixels`: the whole `width` x `height` display, B, G, R, X, rows
    /// packed.
    #[cfg(test)]
    pub(crate) fn encode(
        &mut self,
        pixels: &[u8],
        width: u16,
        height: u16,
        dirty: Rect,
    ) -> anyhow::Result<Vec<u8>> {
        let (w, h) = (u32::from(width), u32::from(height));
        let right = dirty.x.saturating_add(dirty.width).min(w);
        let bottom = dirty.y.saturating_add(dirty.height).min(h);
        let mut tiles = Vec::new();
        if dirty.x < right && dirty.y < bottom {
            for ty in dirty.y / TILE..bottom.div_ceil(TILE) {
                for tx in dirty.x / TILE..right.div_ceil(TILE) {
                    tiles.push((tx, ty));
                }
            }
        }
        self.encode_tiles(pixels, width, height, &tiles)
    }

    /// The progressive block stream for `tiles`, given as tile column
    /// and row on the display's 64-pixel grid, from `pixels` as for
    /// [`Self::encode`].
    pub(crate) fn encode_tiles(
        &mut self,
        pixels: &[u8],
        width: u16,
        height: u16,
        tiles: &[(u32, u32)],
    ) -> anyhow::Result<Vec<u8>> {
        self.encode_tiles_on(pixels, width, height, tiles, ENCODE_THREADS)
    }

    /// [`Self::encode_tiles`], on up to `threads` threads.
    fn encode_tiles_on(
        &mut self,
        pixels: &[u8],
        width: u16,
        height: u16,
        tiles: &[(u32, u32)],
        threads: usize,
    ) -> anyhow::Result<Vec<u8>> {
        let (w, h) = (u32::from(width), u32::from(height));
        anyhow::ensure!(
            pixels.len() == w as usize * h as usize * 4,
            "pixels do not match the display"
        );

        // Owned coefficient streams, which the tiles borrow.
        let encoded = encode_all(pixels, w, h, tiles, threads)?;
        let rects = encoded.iter().map(|t| t.rect.clone()).collect();

        let tiles = encoded
            .iter()
            .map(
                |EncodedTile {
                     x_idx,
                     y_idx,
                     streams: [y, cb, cr],
                     ..
                 }| {
                    ProgressiveTile::Simple(TileSimple {
                        quant_idx_y: 0,
                        quant_idx_cb: 0,
                        quant_idx_cr: 0,
                        x_idx: *x_idx,
                        y_idx: *y_idx,
                        flags: 0,
                        y_data: y,
                        cb_data: cb,
                        cr_data: cr,
                        tail_data: &[],
                    })
                },
            )
            .collect();
        let blocks = [
            ProgressiveBlock::Sync(ProgressiveSyncPdu),
            ProgressiveBlock::Context(ProgressiveContextPdu {
                context_id: 0,
                tile_size: TILE as u16,
                flags: FLAG_DWT_REDUCE_EXTRAPOLATE,
            }),
            ProgressiveBlock::FrameBegin(ProgressiveFrameBeginPdu {
                frame_index: self.frame_index,
                region_count: 1,
            }),
            ProgressiveBlock::Region(ProgressiveRegion {
                tile_size: TILE as u8,
                rects,
                quant_vals: vec![QUANT],
                quant_prog_vals: Vec::new(),
                flags: FLAG_DWT_REDUCE_EXTRAPOLATE,
                tiles,
            }),
            ProgressiveBlock::FrameEnd(ProgressiveFrameEndPdu),
        ];
        self.frame_index = self.frame_index.wrapping_add(1);
        encode_progressive_stream(&blocks)
            .map_err(|e| anyhow::anyhow!("{e:?}"))
            .context("encoding the progressive stream")
    }
}

#[cfg(test)]
mod tests {
    use ironrdp_graphics::color_conversion::{ycbcr_to_rgba, YCbCrBuffer};
    use ironrdp_graphics::progressive::decode_first_pass;
    use ironrdp_graphics::{dwt, dwt_extrapolate};
    use ironrdp_pdu::codecs::rfx::progressive::decode_progressive_stream;

    use super::*;

    /// A `width` x `height` BGRX image with smooth gradients, which a
    /// wavelet codec keeps close to the original.
    fn gradient(width: u16, height: u16) -> Vec<u8> {
        let mut px = Vec::new();
        for y in 0..u32::from(height) {
            for x in 0..u32::from(width) {
                let r = (x * 255 / u32::from(width)) as u8;
                let g = (y * 255 / u32::from(height)) as u8;
                let b = ((x + y) * 127 / u32::from(width + height)) as u8;
                px.extend_from_slice(&[b, g, r, 0xFF]);
            }
        }
        px
    }

    /// A decoded tile: its grid position and 64x64 R, G, B, A pixels.
    struct DecodedTile {
        x_idx: u16,
        y_idx: u16,
        pixels: Vec<u8>,
    }

    /// Decode with IronRDP's stream parser, first-pass decoder and
    /// inverse wavelet, and classic RemoteFX's colour conversion.
    /// IronRDP's ProgressiveDecoder is not used: it converts back from
    /// unscaled YCbCr, the counterpart of the conversion this encoder
    /// must not use.
    fn decode(stream: &[u8], _width: u16, _height: u16) -> Vec<DecodedTile> {
        let blocks = decode_progressive_stream(stream).expect("a valid stream");
        let mut tiles = Vec::new();
        for block in &blocks {
            let ProgressiveBlock::Region(region) = block else {
                continue;
            };
            let rex = region.uses_reduce_extrapolate();
            for tile in &region.tiles {
                let ProgressiveTile::Simple(t) = tile else {
                    panic!("only simple tiles are sent");
                };
                let mut planes = [[0i16; 4096]; 3];
                let data = [t.y_data, t.cb_data, t.cr_data];
                let quant = [t.quant_idx_y, t.quant_idx_cb, t.quant_idx_cr];
                for c in 0..3 {
                    let mut sign = [0i8; 4096];
                    decode_first_pass(
                        data[c],
                        &region.quant_vals[usize::from(quant[c])],
                        &ComponentCodecQuant::LOSSLESS,
                        rex,
                        &mut planes[c],
                        &mut sign,
                    )
                    .expect("RLGR decodes");
                    let mut temp = [0i16; 4096];
                    if rex {
                        dwt_extrapolate::decode(&mut planes[c], &mut temp);
                    } else {
                        dwt::decode(&mut planes[c], &mut temp);
                    }
                }
                let mut pixels = vec![0u8; 4096 * 4];
                ycbcr_to_rgba(
                    YCbCrBuffer {
                        y: &planes[0],
                        cb: &planes[1],
                        cr: &planes[2],
                    },
                    &mut pixels,
                )
                .expect("colour conversion");
                tiles.push(DecodedTile {
                    x_idx: t.x_idx,
                    y_idx: t.y_idx,
                    pixels,
                });
            }
        }
        tiles
    }

    /// The largest difference of any channel between the decoded tiles
    /// and the image, over the pixels inside the image.
    fn max_error(
        tiles: &[DecodedTile],
        image: &[u8],
        width: u16,
        height: u16,
    ) -> u8 {
        let mut worst = 0;
        for tile in tiles {
            for ty in 0..64usize {
                for tx in 0..64usize {
                    let x = usize::from(tile.x_idx) * 64 + tx;
                    let y = usize::from(tile.y_idx) * 64 + ty;
                    if x >= usize::from(width) || y >= usize::from(height) {
                        continue;
                    }
                    let d = &tile.pixels[(ty * 64 + tx) * 4..][..3]; // R, G, B
                    let s = &image[(y * usize::from(width) + x) * 4..][..3]; // B, G, R
                    for (got, want) in
                        [(d[0], s[2]), (d[1], s[1]), (d[2], s[0])]
                    {
                        worst = worst.max(got.abs_diff(want));
                    }
                }
            }
        }
        worst
    }

    fn tile_indices(tiles: &[DecodedTile]) -> Vec<(u16, u16)> {
        let mut idx: Vec<_> =
            tiles.iter().map(|t| (t.x_idx, t.y_idx)).collect();
        idx.sort_unstable();
        idx
    }

    /// IronRDP's first-pass decoder delta-decodes the LL3 band at 4015
    /// for the reduce-extrapolate layout: 81 coefficients, the last.
    #[test]
    fn the_ll3_band_is_where_the_decoder_expects_it() {
        assert_eq!(ll3_offset(), 4015);
        assert_eq!(ll3_offset() + 81, TILE_PIXELS);
    }

    fn every_tile(w: u16, h: u16) -> Vec<(u32, u32)> {
        let (w, h) = (u32::from(w), u32::from(h));
        (0..h.div_ceil(TILE))
            .flat_map(|ty| (0..w.div_ceil(TILE)).map(move |tx| (tx, ty)))
            .collect()
    }

    /// Tiles encoded on several threads make the very stream one thread
    /// makes: the same tiles, in the order asked.
    #[test]
    fn tiles_encoded_in_parallel_are_the_serial_stream() {
        // 300 tiles, with ragged edges.
        let (w, h) = (1250, 950);
        let image = gradient(w, h);
        let tiles = every_tile(w, h);
        let serial = ProgressiveEncoder::new()
            .encode_tiles_on(&image, w, h, &tiles, 1)
            .expect("serial");
        for threads in [2, 3, 4, 7] {
            let parallel = ProgressiveEncoder::new()
                .encode_tiles_on(&image, w, h, &tiles, threads)
                .expect("parallel");
            assert!(parallel == serial, "{threads} threads differ");
        }
    }

    /// A tile one thread cannot encode fails the frame.
    #[test]
    fn a_bad_tile_fails_a_parallel_frame() {
        let (w, h) = (1250, 950);
        let image = gradient(w, h);
        let mut tiles = every_tile(w, h);
        tiles.push((99, 0));
        assert!(ProgressiveEncoder::new()
            .encode_tiles_on(&image, w, h, &tiles, 4)
            .is_err());
    }

    /// Few tiles stay on the calling thread: a thread costs more to
    /// start than a handful of tiles take.
    #[test]
    fn few_tiles_use_one_thread() {
        assert_eq!(threads_for(1, 4), 1);
        assert_eq!(threads_for(TILES_PER_THREAD * 2 - 1, 4), 1);
        assert_eq!(threads_for(TILES_PER_THREAD * 2, 4), 2);
        assert_eq!(threads_for(TILES_PER_THREAD * 100, 4), 4);
        assert_eq!(threads_for(TILES_PER_THREAD * 100, 0), 1);
    }

    #[test]
    fn a_whole_frame_decodes_close_to_the_image() {
        let (w, h) = (128, 64);
        let image = gradient(w, h);
        let stream = ProgressiveEncoder::new()
            .encode(
                &image,
                w,
                h,
                Rect {
                    x: 0,
                    y: 0,
                    width: 128,
                    height: 64,
                },
            )
            .expect("encode");
        let tiles = decode(&stream, w, h);
        assert_eq!(tile_indices(&tiles), [(0, 0), (1, 0)]);
        let err = max_error(&tiles, &image, w, h);
        assert!(err <= 12, "worst channel error {err}");
    }

    /// Only the tiles the damage touches are sent.
    #[test]
    fn only_damaged_tiles_are_sent() {
        let (w, h) = (256, 192);
        let image = gradient(w, h);
        let stream = ProgressiveEncoder::new()
            .encode(
                &image,
                w,
                h,
                Rect {
                    x: 70,
                    y: 70,
                    width: 60,
                    height: 10,
                },
            )
            .expect("encode");
        assert_eq!(tile_indices(&decode(&stream, w, h)), [(1, 1), (2, 1)]);
    }

    /// Edge tiles run past the display; they are padded, and the pixels
    /// inside the display still decode right.
    #[test]
    fn edge_tiles_decode_inside_the_display() {
        let (w, h) = (100, 70);
        let image = gradient(w, h);
        let stream = ProgressiveEncoder::new()
            .encode(
                &image,
                w,
                h,
                Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 70,
                },
            )
            .expect("encode");
        let tiles = decode(&stream, w, h);
        assert_eq!(tile_indices(&tiles), [(0, 0), (0, 1), (1, 0), (1, 1)]);
        let err = max_error(&tiles, &image, w, h);
        assert!(err <= 12, "worst channel error {err}");
    }

    /// A flat colour survives almost exactly: what a desktop is mostly
    /// made of.
    #[test]
    fn a_flat_colour_is_nearly_exact() {
        let (w, h) = (64, 64);
        let image = [0x30, 0x80, 0xC0, 0xFF].repeat(64 * 64);
        let stream = ProgressiveEncoder::new()
            .encode(
                &image,
                w,
                h,
                Rect {
                    x: 0,
                    y: 0,
                    width: 64,
                    height: 64,
                },
            )
            .expect("encode");
        let err = max_error(&decode(&stream, w, h), &image, w, h);
        assert!(err <= 2, "worst channel error {err}");
    }
}
