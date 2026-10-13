// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The numbers the EGFX worker draws over its frames while DTrace has
//! its `egfx_overlay` probe enabled: the frame rate, the encode time,
//! the pace asked of the guest and the frames the client refused, over
//! the last second.
//!
//! The text goes into the worker's own copy of the frame, after the
//! display is read, so the guest's framebuffer never holds it. A small
//! 5x7 font covers the characters the text uses.

use std::time::{Duration, Instant};

use crate::display::Rect;

/// Pixels per font pixel, so the text reads on a large display.
const SCALE: u32 = 2;
const GLYPH_WIDTH: u32 = 5;
const GLYPH_HEIGHT: u32 = 7;
/// One glyph and the column between glyphs.
const ADVANCE: u32 = (GLYPH_WIDTH + 1) * SCALE;
/// Around the text, inside the box.
const PADDING: u32 = 6;
/// From the corner of the frame to the box.
const MARGIN: u32 = 8;

const BLACK: [u8; 4] = [0x00, 0x00, 0x00, 0xFF];
const WHITE: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];

/// The rectangle `text` covers in the top-left corner of a frame, before
/// it is clipped to the frame.
pub fn area(text: &str) -> Rect {
    let chars = text.chars().count() as u32;
    Rect {
        x: MARGIN,
        y: MARGIN,
        width: 2 * PADDING + (chars * ADVANCE).saturating_sub(SCALE),
        height: 2 * PADDING + GLYPH_HEIGHT * SCALE,
    }
}

/// Draw `text`, white on a black box, into `pixels`, a `width` x
/// `height` frame of 4-byte XRGB pixels. What falls outside the frame is
/// left out.
pub fn draw(pixels: &mut [u8], width: u32, height: u32, text: &str) {
    let r = area(text);
    let mut put = |x: u32, y: u32, color: [u8; 4]| {
        if x < width && y < height {
            let at = (y as usize * width as usize + x as usize) * 4;
            if let Some(px) = pixels.get_mut(at..at + 4) {
                px.copy_from_slice(&color);
            }
        }
    };
    for y in r.y..r.y + r.height {
        for x in r.x..r.x + r.width {
            put(x, y, BLACK);
        }
    }
    for (i, c) in text.chars().enumerate() {
        let left = r.x + PADDING + i as u32 * ADVANCE;
        for (row, bits) in glyph(c).iter().enumerate() {
            for col in 0..GLYPH_WIDTH {
                if bits & (1 << (GLYPH_WIDTH - 1 - col)) == 0 {
                    continue;
                }
                for dy in 0..SCALE {
                    for dx in 0..SCALE {
                        put(
                            left + col * SCALE + dx,
                            r.y + PADDING + row as u32 * SCALE + dy,
                            WHITE,
                        );
                    }
                }
            }
        }
    }
}

/// The rows of `c`, top first, the leftmost pixel in bit 4. A character
/// the font lacks is a box, so a gap in the font shows.
fn glyph(c: char) -> [u8; 7] {
    match c {
        ' ' => [0; 7],
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'D' => [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x11, 0x19, 0x15, 0x13, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        '/' => [0x01, 0x02, 0x02, 0x04, 0x08, 0x08, 0x10],
        _ => [0x1F, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1F],
    }
}

/// The worker's frames over the current second, and the text the last
/// whole second made.
pub struct Stats {
    since: Instant,
    frames: u32,
    refused: u32,
    encode: Duration,
    worst: Duration,
    pace: Duration,
    text: String,
}

impl Stats {
    pub fn new(now: Instant) -> Self {
        Self {
            since: now,
            frames: 0,
            refused: 0,
            encode: Duration::ZERO,
            worst: Duration::ZERO,
            pace: Duration::ZERO,
            text: "0 FPS".to_string(),
        }
    }

    /// Count a frame that took `cost` to make, which the client took or
    /// refused, after which the guest is asked for `pace`.
    pub fn frame(&mut self, cost: Duration, taken: bool, pace: Duration) {
        self.frames += 1;
        if !taken {
            self.refused += 1;
        }
        self.encode += cost;
        self.worst = self.worst.max(cost);
        self.pace = pace;
    }

    /// At `now`, close the second if it is over and make its text.
    pub fn roll(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.since);
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let fps = (f64::from(self.frames) / elapsed.as_secs_f64()).round();
        let average = self
            .encode
            .checked_div(self.frames)
            .unwrap_or(Duration::ZERO);
        self.text = format!(
            "{fps} FPS  ENCODE {}/{} MS  PACE {} MS  REFUSED {}",
            average.as_millis(),
            self.worst.as_millis(),
            self.pace.as_millis(),
            self.refused,
        );
        *self = Self {
            text: std::mem::take(&mut self.text),
            pace: self.pace,
            ..Self::new(now)
        };
    }

    /// The text the last whole second made.
    pub fn text(&self) -> &str {
        &self.text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn pixel(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let at = (y as usize * width as usize + x as usize) * 4;
        pixels[at..at + 4].try_into().expect("4 bytes")
    }

    /// The box is black, the glyphs white, and nothing outside the box
    /// changes.
    #[test]
    fn the_text_is_drawn_in_its_box_and_nowhere_else() {
        let (w, h) = (400, 100);
        let mut pixels = vec![0x40; (w * h * 4) as usize];
        draw(&mut pixels, w, h, "F");
        let r = area("F");

        // F's top row is solid: five font pixels, each SCALE wide.
        let top = r.y + PADDING;
        for x in r.x + PADDING..r.x + PADDING + GLYPH_WIDTH * SCALE {
            assert_eq!(pixel(&pixels, w, x, top), WHITE, "({x}, {top})");
        }
        // The box's corner is black.
        assert_eq!(pixel(&pixels, w, r.x, r.y), BLACK);
        for (x, y) in [
            (r.x - 1, r.y),
            (r.x, r.y - 1),
            (r.x + r.width, r.y),
            (r.x, r.y + r.height),
        ] {
            assert_eq!(pixel(&pixels, w, x, y), [0x40; 4], "({x}, {y})");
        }
    }

    /// A frame smaller than the box gets what fits, and no write past
    /// its end.
    #[test]
    fn a_small_frame_is_clipped() {
        let (w, h) = (20, 12);
        let mut pixels = vec![0x40; (w * h * 4) as usize];
        draw(&mut pixels, w, h, "60 FPS");
        assert_eq!(pixel(&pixels, w, MARGIN, MARGIN), BLACK);
        assert_eq!(pixel(&pixels, w, 0, 0), [0x40; 4]);

        let mut none: Vec<u8> = Vec::new();
        draw(&mut none, 0, 0, "60 FPS");
    }

    /// The box grows with the text, a glyph's advance a character.
    #[test]
    fn the_box_fits_the_text() {
        let one = area("1");
        let three = area("123");
        assert_eq!(three.width - one.width, 2 * ADVANCE);
        assert_eq!(one.height, three.height);
    }

    /// Every character the stats text uses has its own glyph rather than
    /// the box a missing one gets.
    #[test]
    fn the_font_covers_the_stats_text() {
        let missing = glyph('\u{1}');
        let text = "0123456789 FPS  ENCODE 1/2 MS  PACE 16 MS  REFUSED 0";
        for c in text.chars() {
            assert_ne!(glyph(c), missing, "{c:?}");
        }
    }

    /// A second's frames make the text: the rate, the average and worst
    /// encode, the last pace and the refusals.
    #[test]
    fn a_second_of_frames_makes_its_text() {
        let start = Instant::now();
        let mut stats = Stats::new(start);
        assert_eq!(stats.text(), "0 FPS");
        for i in 0..60 {
            stats.frame(MS * if i == 0 { 9 } else { 3 }, i != 5, MS * 16);
        }
        stats.roll(start + MS * 500);
        assert_eq!(stats.text(), "0 FPS", "the second is not over");
        stats.roll(start + Duration::from_secs(1));
        assert_eq!(
            stats.text(),
            "60 FPS  ENCODE 3/9 MS  PACE 16 MS  REFUSED 1"
        );

        // The next second starts empty and keeps the pace.
        stats.roll(start + Duration::from_secs(2));
        assert_eq!(stats.text(), "0 FPS  ENCODE 0/0 MS  PACE 16 MS  REFUSED 0");
    }
}
