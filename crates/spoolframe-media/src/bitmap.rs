//! What every bitmap subtitle format comes to for OCR: each cue's time and
//! a grey picture of its text. PGS ([`crate::pgs`]) is read from a raw
//! `.sup`; VobSub ([`crate::vobsub`]) and DVB ([`crate::dvb`]) from the
//! track's packets as ffprobe gives them ([`Track`]).

use std::collections::HashMap;

/// The widest or tallest subtitle picture read (PX-09 B-3): a 4K screen
/// is 3840 × 2160, and VobSub's 12-bit coordinates stop at 4096 too. Sizes
/// come from the file, so without a cap a header of 65535 × 65535 would
/// ask for 4 GiB of pixels.
pub const MAX_SIDE: usize = 4096;

/// How much a track may decode to (finding 45). [`MAX_SIDE`] bounds one
/// picture, not a track: run-length coding lets a few bytes describe a
/// 4096 × 4096 picture, and a track can show one again and again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Cues kept.
    pub cues: usize,
    /// Pixels of every kept cue's picture, added up.
    pub pixels: u64,
    /// Pixels a decoder holds at once between cues: PGS objects, DVB
    /// regions.
    pub held: u64,
}

impl Budget {
    /// A film's track with room to spare. A feature film's PGS track is 1
    /// to 3 thousand cues, each a picture of a line or two of text with a
    /// 16-pixel margin: about 100 thousand pixels, and 1,952 × 180 at most
    /// for two lines as wide as a 1080p screen. 20,000 cues is one every
    /// 0.36 s of a two-hour film. 1 Gi pixels is 3,000 of the widest
    /// pictures, and 3 to 5 times a usual film's 0.2 to 0.4 G. The
    /// decoder holds the pictures in memory, then writes them as PGM files
    /// (a byte a pixel) for tesseract, so this bounds both. Held is 32
    /// times a 1080p screen; a PGS decoder of the spec has a 4 MiB object
    /// buffer.
    pub const DEFAULT: Self = Self {
        cues: 20_000,
        pixels: 1 << 30,
        held: 64 << 20,
    };

    /// Whether holding `held` pixels, `adding` more, stays within it;
    /// counted before they are allocated.
    pub(crate) fn hold(&self, held: u64, adding: u64) -> Result<u64, OverBudget> {
        let total = held.saturating_add(adding);
        if total > self.held {
            return Err(OverBudget::Held(self.held));
        }
        Ok(total)
    }
}

/// The cues a decoder keeps, within its [`Budget`].
pub(crate) struct Kept<'a> {
    budget: &'a Budget,
    pixels: u64,
    cues: Vec<Cue>,
}

impl<'a> Kept<'a> {
    pub(crate) fn new(budget: &'a Budget) -> Self {
        Self {
            budget,
            pixels: 0,
            cues: Vec::new(),
        }
    }

    /// Whether a `width` × `height` picture may still be kept: asked
    /// before it is made.
    pub(crate) fn afford(&self, width: usize, height: usize) -> Result<(), OverBudget> {
        let size = (width as u64).saturating_mul(height as u64);
        if self.pixels.saturating_add(size) > self.budget.pixels {
            return Err(OverBudget::Pixels(self.budget.pixels));
        }
        Ok(())
    }

    /// A `width` × `height` composite about to be drawn, counted against
    /// the budget's pixels before it is allocated, whatever of it is kept:
    /// a picture is cut to its text afterwards, and a few bytes of stream
    /// can ask for many full-screen composites (finding 45).
    pub(crate) fn draw(&mut self, width: usize, height: usize) -> Result<(), OverBudget> {
        self.afford(width, height)?;
        self.pixels += (width as u64).saturating_mul(height as u64);
        Ok(())
    }

    /// Adds a cue, or lengthens the last one when this is the same picture
    /// straight after it (a palette change, as in a fade, or a page sent
    /// again). Only a cue kept counts against the budget.
    pub(crate) fn push(
        &mut self,
        start_ms: u64,
        end_ms: u64,
        image: Image,
    ) -> Result<(), OverBudget> {
        if end_ms <= start_ms {
            return Ok(());
        }
        if let Some(last) = self.cues.last_mut()
            && last.end_ms == start_ms
            && same_text(&last.image, &image)
        {
            last.end_ms = end_ms;
            return Ok(());
        }
        if self.cues.len() >= self.budget.cues {
            return Err(OverBudget::Cues(self.budget.cues));
        }
        self.afford(image.width, image.height)?;
        self.pixels += image.pixels.len() as u64;
        self.cues.push(Cue {
            start_ms,
            end_ms,
            image,
        });
        Ok(())
    }

    pub(crate) fn into_cues(self) -> Vec<Cue> {
        self.cues
    }
}

/// A track that decodes to more than its [`Budget`]. Decoding stops there:
/// a track cut short would read as a whole one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OverBudget {
    #[error("the track has more than {0} cues")]
    Cues(usize),
    #[error("the track's pictures come to more than {0} pixels")]
    Pixels(u64),
    #[error("the track holds more than {0} pixels of pictures at once")]
    Held(u64),
}

/// A grey picture: 0 is black, 255 white.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

impl Image {
    pub(crate) fn blank(width: usize, height: usize, value: u8) -> Self {
        Self {
            width,
            height,
            pixels: vec![value; width * height],
        }
    }

    /// As a binary PGM, which tesseract (through Leptonica) reads.
    pub fn to_pgm(&self) -> Vec<u8> {
        let mut out = format!("P5\n{} {}\n255\n", self.width, self.height).into_bytes();
        out.extend_from_slice(&self.pixels);
        out
    }
}

/// One subtitle on screen: its time and its picture, dark text on white.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    pub start_ms: u64,
    pub end_ms: u64,
    pub image: Image,
}

/// A subtitle track as its container holds it: the codec's private data
/// (VobSub's `.idx` header) and its packets, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Track {
    pub extradata: Vec<u8>,
    pub packets: Vec<Packet>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub pts_ms: u64,
    pub duration_ms: Option<u64>,
    pub data: Vec<u8>,
}

/// A palette entry as OCR sees it: how bright and how opaque.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(crate) struct Colour {
    pub luma: u8,
    pub alpha: u8,
}

/// Luma (BT.601) of an RGB colour.
pub(crate) fn luma(r: u8, g: u8, b: u8) -> u8 {
    ((299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b)) / 1000) as u8
}

/// A video-range luma (16 black, 235 white) stretched to full range, so
/// an opaque black outline (Y = 16) counts as black, not grey 16: PGS and
/// DVB palettes both, so the two split text from outline alike.
pub(crate) fn video_luma(y: u8) -> u8 {
    (u32::from(y.saturating_sub(16)) * 255 / 219).min(255) as u8
}

/// A picture of `width` × `height` pixels, each a colour, as dark text on
/// white: the text without its outline, cut to the box around it with a
/// margin (tesseract reads text that touches the edge badly). `None` if no
/// text shows; an error if `kept` has no room for the picture.
///
/// It is not scaled up. Over a real DVD's VobSub (720×480), two and three
/// times the size read worse, not better: tesseract scales lines itself,
/// and blocky letters made bigger are only bigger blocks.
pub(crate) fn text_picture(
    width: usize,
    height: usize,
    pixels: &[Colour],
    kept: &Kept,
) -> Result<Option<Image>, OverBudget> {
    const MARGIN: usize = 16;
    // The ink and its rows are as large as the picture: counted before
    // they are allocated (review 2026-10-11 L68).
    kept.afford(width, height)?;
    let ink = ink(width, pixels);
    let text = |at: usize| ink[at] > 0;
    let rows: Vec<usize> = (0..height)
        .filter(|&y| (0..width).any(|x| text(y * width + x)))
        .collect();
    let (Some(&top), Some(&bottom)) = (rows.first(), rows.last()) else {
        return Ok(None);
    };
    let Some(left) = (0..width).find(|&x| rows.iter().any(|&y| text(y * width + x))) else {
        return Ok(None);
    };
    let Some(right) = (0..width)
        .rev()
        .find(|&x| rows.iter().any(|&y| text(y * width + x)))
    else {
        return Ok(None);
    };
    let (w, h) = (right - left + 1 + 2 * MARGIN, bottom - top + 1 + 2 * MARGIN);
    kept.afford(w, h)?;
    let mut image = Image::blank(w, h, 255);
    for y in top..=bottom {
        for x in left..=right {
            let at = (y - top + MARGIN) * image.width + (x - left + MARGIN);
            image.pixels[at] = 255 - ink[y * width + x];
        }
    }
    Ok(Some(image))
}

/// How much each pixel is text, from 0 (not) to 255. A subtitle is text in
/// one colour wrapped in an outline of another, often with an edge colour
/// between them, over a clear background; tesseract wants the text alone,
/// as the outline thickens every letter into the next. The colours that
/// show split at the midpoint of their brightness. Text is mostly the
/// bright side, but not always, so the split is checked by shape: an
/// outline wraps the text, so more of its pixels touch the background.
/// Every colour then counts as text as near as it is to the text's
/// brightness: the outline not at all, the edge's grey in part, which
/// keeps letters smooth (black and white read worse over a real DVD).
fn ink(width: usize, pixels: &[Colour]) -> Vec<u8> {
    let light = |c: Colour| u32::from(c.luma) * u32::from(c.alpha) / 255;
    let clear = |at: Option<&Colour>| at.is_none_or(|c| c.alpha == 0);
    // Per colour that shows: its pixels, and those next to the background.
    let mut seen: HashMap<Colour, (usize, usize)> = HashMap::new();
    for (at, &c) in pixels.iter().enumerate() {
        if c.alpha == 0 {
            continue;
        }
        let (x, y) = (at % width, at / width);
        let edge = x == 0
            || x + 1 == width
            || y == 0
            || clear(pixels.get(at - 1))
            || clear(pixels.get(at + 1))
            || clear(pixels.get(at - width))
            || clear(pixels.get(at + width));
        let counts = seen.entry(c).or_default();
        counts.0 += 1;
        counts.1 += usize::from(edge);
    }
    let lo = seen.keys().map(|&c| light(c)).min().unwrap_or(0);
    let hi = seen.keys().map(|&c| light(c)).max().unwrap_or(0);
    let bright = |c: Colour| 2 * light(c) >= lo + hi;
    // How much of one side touches the background, as a fraction.
    let touching = |side: bool| {
        let (n, edge) = seen
            .iter()
            .filter(|&(&c, _)| bright(c) == side)
            .fold((0, 0), |(n, e), (_, &(cn, ce))| (n + cn, e + ce));
        (n > 0).then(|| edge as f64 / n as f64)
    };
    let text_is_bright = match (touching(true), touching(false)) {
        (Some(b), Some(d)) => b <= d,
        _ => true,
    };
    let span = (hi - lo).max(1);
    pixels
        .iter()
        .map(|&c| match c.alpha {
            0 => 0,
            _ if lo == hi => 255,
            _ if text_is_bright => ((light(c) - lo) * 255 / span) as u8,
            _ => ((hi - light(c)) * 255 / span) as u8,
        })
        .collect()
}

/// The same shapes, whatever their brightness: a fade changes the
/// palette, not which pixels are text.
fn same_text(a: &Image, b: &Image) -> bool {
    a.width == b.width
        && a.height == b.height
        && a.pixels
            .iter()
            .zip(&b.pixels)
            .all(|(&p, &q)| (p == 255) == (q == 255))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLEAR: Colour = Colour { luma: 0, alpha: 0 };

    fn text_picture(width: usize, height: usize, pixels: &[Colour]) -> Option<Image> {
        super::text_picture(width, height, pixels, &Kept::new(&Budget::DEFAULT)).unwrap()
    }

    /// A picture is counted against the budget at its full size, before
    /// its ink is allocated, not only once trimmed to its text. Review
    /// 2026-10-11 L68.
    #[test]
    fn a_picture_is_counted_before_its_ink_is_allocated() {
        let budget = Budget {
            pixels: 5_000,
            ..Budget::DEFAULT
        };
        // 100 × 100, a 5 × 3 bar of text: trimmed with its margin it
        // would be 37 × 35, under the budget; whole it is over.
        let pixels: Vec<Colour> = (0..10_000)
            .map(|at| match (at % 100, at / 100) {
                (40..=44, 50..=52) => opaque(255),
                _ => CLEAR,
            })
            .collect();
        assert!(super::text_picture(100, 100, &pixels, &Kept::new(&budget)).is_err());
    }

    fn opaque(luma: u8) -> Colour {
        Colour { luma, alpha: 255 }
    }

    /// A 3-pixel-high bar of `text` across the middle of a 9 × 9 picture,
    /// wrapped in a 2-pixel `outline`, on a clear background.
    fn outlined(text: Colour, outline: Colour) -> Vec<Colour> {
        (0..81)
            .map(|at| match (at % 9, at / 9) {
                (2..=6, 3..=5) => text,
                (0..=8, 1..=7) => outline,
                _ => CLEAR,
            })
            .collect()
    }

    /// The size of what the picture holds inside its margin, and how many
    /// of its pixels are dark.
    fn inked(image: &Image) -> (usize, usize, usize) {
        let dark = image.pixels.iter().filter(|&&p| p < 128).count();
        (image.width - 32, image.height - 32, dark)
    }

    #[test]
    fn light_text_is_inked_and_its_grey_outline_is_not() {
        // A grey outline still shows over black: it must not count.
        let image = text_picture(9, 9, &outlined(opaque(235), opaque(80))).unwrap();
        assert_eq!(inked(&image), (5, 3, 15));
    }

    #[test]
    fn dark_text_in_a_light_outline_is_the_text() {
        let image = text_picture(9, 9, &outlined(opaque(20), opaque(235))).unwrap();
        assert_eq!(inked(&image), (5, 3, 15));
    }

    #[test]
    fn an_edge_between_text_and_outline_is_inked_in_part() {
        // Text 255, an edge of 128 around it, then the outline of 0.
        let pixels: Vec<Colour> = (0..81)
            .map(|at| match (at % 9, at / 9) {
                (3..=5, 4) => opaque(255),
                (2..=6, 3..=5) => opaque(128),
                (0..=8, 1..=7) => opaque(0),
                _ => CLEAR,
            })
            .collect();
        let image = text_picture(9, 9, &pixels).unwrap();
        let at = |x: usize, y: usize| image.pixels[(16 + y) * image.width + 16 + x];
        // Half text: half dark.
        assert_eq!((at(1, 1), at(0, 1)), (0, 127));
    }

    #[test]
    fn one_colour_alone_is_text() {
        let pixels: Vec<Colour> = (0..81)
            .map(|at| if at % 9 == 4 { opaque(200) } else { CLEAR })
            .collect();
        let image = text_picture(9, 9, &pixels).unwrap();
        assert_eq!(inked(&image), (1, 9, 9));
        assert_eq!(text_picture(9, 9, &[CLEAR; 81]), None);
    }
}
