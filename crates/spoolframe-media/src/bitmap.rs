//! What every bitmap subtitle format comes to for OCR: each cue's time and
//! a grey picture of its text. PGS ([`crate::pgs`]) is read from a raw
//! `.sup`; VobSub ([`crate::vobsub`]) and DVB ([`crate::dvb`]) from the
//! track's packets as ffprobe gives them ([`Track`]).

use std::collections::HashMap;

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

/// A picture of `width` × `height` pixels, each a colour, as dark text on
/// white: the text without its outline, cut to the box around it with a
/// margin (tesseract reads text that touches the edge badly). `None` if no
/// text shows.
///
/// It is not scaled up. Over a real DVD's VobSub (720×480), two and three
/// times the size read worse, not better: tesseract scales lines itself,
/// and blocky letters made bigger are only bigger blocks.
pub(crate) fn text_picture(width: usize, height: usize, pixels: &[Colour]) -> Option<Image> {
    const MARGIN: usize = 16;
    let ink = ink(width, pixels);
    let text = |at: usize| ink[at] > 0;
    let rows: Vec<usize> = (0..height)
        .filter(|&y| (0..width).any(|x| text(y * width + x)))
        .collect();
    let (&top, &bottom) = (rows.first()?, rows.last()?);
    let left = (0..width).find(|&x| rows.iter().any(|&y| text(y * width + x)))?;
    let right = (0..width)
        .rev()
        .find(|&x| rows.iter().any(|&y| text(y * width + x)))?;
    let mut image = Image::blank(
        right - left + 1 + 2 * MARGIN,
        bottom - top + 1 + 2 * MARGIN,
        255,
    );
    for y in top..=bottom {
        for x in left..=right {
            let at = (y - top + MARGIN) * image.width + (x - left + MARGIN);
            image.pixels[at] = 255 - ink[y * width + x];
        }
    }
    Some(image)
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

/// Adds a cue, or lengthens the last one when this is the same picture
/// straight after it (a palette change, as in a fade, or a page sent
/// again).
pub(crate) fn push(out: &mut Vec<Cue>, start_ms: u64, end_ms: u64, image: Image) {
    if end_ms <= start_ms {
        return;
    }
    if let Some(last) = out.last_mut()
        && last.end_ms == start_ms
        && same_text(&last.image, &image)
    {
        last.end_ms = end_ms;
        return;
    }
    out.push(Cue {
        start_ms,
        end_ms,
        image,
    });
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
