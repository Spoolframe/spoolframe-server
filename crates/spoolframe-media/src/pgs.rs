//! PGS (Blu-ray "HDMV Presentation Graphic Stream") subtitles: the cues in
//! a raw `.sup` stream, each as a grey picture of its text, for OCR.
//!
//! A `.sup` is a run of segments, each `"PG"`, a 90 kHz pts, a dts, a type
//! and a length. A display set is a presentation composition (PCS), then
//! windows (WDS), palettes (PDS) and objects (ODS), then END. A PCS with
//! objects shows them from its pts until the next PCS; one without clears
//! the screen. Objects and palettes, once sent, stay usable until the next
//! epoch start, so a later PCS can show an earlier object again with
//! another palette (that is how fades are done).

use std::collections::HashMap;

use crate::bitmap::{self, Budget, Colour, Cue, Image, Kept, MAX_SIDE, OverBudget};

const PCS: u8 = 0x16;
const WDS: u8 = 0x17;
const PDS: u8 = 0x14;
const ODS: u8 = 0x15;
const END: u8 = 0x80;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PgsError {
    #[error("not a PGS stream at byte {0}")]
    BadMagic(usize),
    #[error("a segment at byte {0} runs past the end of the stream")]
    Truncated(usize),
    #[error("a {0} segment is too short")]
    Short(&'static str),
    #[error("a {width} × {height} picture is larger than {MAX_SIDE} × {MAX_SIDE}")]
    TooLarge { width: usize, height: usize },
    #[error("object {0}'s run-length data is longer than its picture can need")]
    Overlong(u16),
    #[error(transparent)]
    Budget(#[from] OverBudget),
}

/// What one PCS asks to show.
struct Composition {
    pts_ms: u64,
    palette: u8,
    objects: Vec<Placed>,
}

struct Placed {
    object: u16,
    x: usize,
    y: usize,
    crop: Option<(usize, usize, usize, usize)>,
}

/// An object as decoded: palette indices.
#[derive(Clone)]
struct Object {
    width: usize,
    height: usize,
    indices: Vec<u8>,
}

/// A palette's entries as OCR sees them: how bright and how opaque.
type Palette = [Colour; 256];

/// The cues of a `.sup` stream, in order, within [`Budget::DEFAULT`].
pub fn cues(stream: &[u8]) -> Result<Vec<Cue>, PgsError> {
    cues_within(stream, &Budget::DEFAULT)
}

/// The cues of a `.sup` stream, in order. A stream that decodes to more
/// than `budget` is an error.
pub fn cues_within(stream: &[u8], budget: &Budget) -> Result<Vec<Cue>, PgsError> {
    let mut palettes: HashMap<u8, Palette> = HashMap::new();
    let mut objects: HashMap<u16, Object> = HashMap::new();
    // The pixels of `objects`, added up.
    let mut held = 0;
    // An object arrives in fragments: its size, then RLE data.
    let mut partial: HashMap<u16, (usize, usize, Vec<u8>)> = HashMap::new();
    let mut shown: Option<(u64, Image)> = None;
    let mut pending: Option<Composition> = None;
    let mut out = Kept::new(budget);

    let mut at = 0;
    while at < stream.len() {
        if stream.len() - at < 13 {
            return Err(PgsError::Truncated(at));
        }
        if &stream[at..at + 2] != b"PG" {
            return Err(PgsError::BadMagic(at));
        }
        let pts = u32::from_be_bytes(stream[at + 2..at + 6].try_into().unwrap());
        let kind = stream[at + 10];
        let len = usize::from(u16::from_be_bytes([stream[at + 11], stream[at + 12]]));
        let body = stream
            .get(at + 13..at + 13 + len)
            .ok_or(PgsError::Truncated(at))?;
        at += 13 + len;
        let pts_ms = u64::from(pts) / 90;

        match kind {
            PCS => {
                let composition = composition(body, pts_ms)?;
                // An epoch start (state 0x80) drops what earlier sets sent.
                if body[7] & 0x80 != 0 {
                    palettes.clear();
                    objects.clear();
                    partial.clear();
                    held = 0;
                }
                pending = Some(composition);
            }
            PDS => {
                let (&id, entries) = body.split_first().ok_or(PgsError::Short("PDS"))?;
                let palette = palettes.entry(id).or_insert([Colour::default(); 256]);
                // After the id: a version byte, then 5-byte entries.
                for &[index, luma, _, _, alpha] in
                    entries.get(1..).unwrap_or_default().as_chunks::<5>().0
                {
                    // Video range, as DVB's are: an opaque black outline
                    // (Y = 16) is black, not grey 16, so it is outline and
                    // not text when the picture's ink is worked out
                    // (review 2026-10-11 L69).
                    palette[usize::from(index)] = Colour {
                        luma: bitmap::video_luma(luma),
                        alpha,
                    };
                }
            }
            ODS => {
                if body.len() < 4 {
                    return Err(PgsError::Short("ODS"));
                }
                let id = u16::from_be_bytes([body[0], body[1]]);
                let sequence = body[3];
                let mut data = &body[4..];
                if sequence & 0x80 != 0 {
                    // First fragment: a 24-bit length, then width, height.
                    if data.len() < 7 {
                        return Err(PgsError::Short("ODS"));
                    }
                    let width = usize::from(u16::from_be_bytes([data[3], data[4]]));
                    let height = usize::from(u16::from_be_bytes([data[5], data[6]]));
                    if width > MAX_SIDE || height > MAX_SIDE {
                        return Err(PgsError::TooLarge { width, height });
                    }
                    data = &data[7..];
                    partial.insert(id, (width, height, Vec::new()));
                }
                if let Some((width, height, rle)) = partial.get_mut(&id) {
                    // Fragments are held until the last, and only a decoded
                    // object counts against the budget: past what any
                    // coding of this picture needs, the stream is refused
                    // rather than held (review 2026-10-11 M35).
                    if rle.len() + data.len() > most_rle(*width, *height) {
                        return Err(PgsError::Overlong(id));
                    }
                    rle.extend_from_slice(data);
                }
                if sequence & 0x40 != 0
                    && let Some((width, height, rle)) = partial.remove(&id)
                {
                    // Counted before it is decoded; one it replaces is
                    // let go.
                    let replaced = objects.get(&id).map_or(0, |o| o.indices.len() as u64);
                    held = budget.hold(held - replaced, (width * height) as u64)?;
                    objects.insert(
                        id,
                        Object {
                            width,
                            height,
                            indices: decode_rle(&rle, width, height),
                        },
                    );
                }
            }
            END => {
                let Some(composition) = pending.take() else {
                    continue;
                };
                // Whatever was shown ends here.
                if let Some((start_ms, image)) = shown.take() {
                    out.push(start_ms, composition.pts_ms, image)?;
                }
                let palette = palettes
                    .get(&composition.palette)
                    .copied()
                    .unwrap_or([Colour::default(); 256]);
                shown = render(&composition, &objects, &palette, &mut out)?
                    .map(|image| (composition.pts_ms, image));
            }
            WDS => {}
            _ => {}
        }
    }
    // A stream that ends while a cue shows: it is dropped, with no end
    // time to give it.
    Ok(out.into_cues())
}

/// The most run-length bytes a `width` × `height` object can take: its
/// longest coding is two bytes a pixel (a single pixel of colour 0) and
/// two at each line's end. The declared length is not trusted for it.
fn most_rle(width: usize, height: usize) -> usize {
    2 * width * height + 2 * height + 4
}

fn composition(body: &[u8], pts_ms: u64) -> Result<Composition, PgsError> {
    if body.len() < 11 {
        return Err(PgsError::Short("PCS"));
    }
    let palette = body[9];
    let count = usize::from(body[10]);
    let mut objects = Vec::with_capacity(count);
    let mut at = 11;
    for _ in 0..count {
        let entry = body.get(at..at + 8).ok_or(PgsError::Short("PCS"))?;
        let object = u16::from_be_bytes([entry[0], entry[1]]);
        let cropped = entry[3] & 0x80 != 0;
        let x = usize::from(u16::from_be_bytes([entry[4], entry[5]]));
        let y = usize::from(u16::from_be_bytes([entry[6], entry[7]]));
        at += 8;
        let crop = if cropped {
            let c = body.get(at..at + 8).ok_or(PgsError::Short("PCS"))?;
            at += 8;
            let n = |i: usize| usize::from(u16::from_be_bytes([c[i], c[i + 1]]));
            Some((n(0), n(2), n(4), n(6)))
        } else {
            None
        };
        objects.push(Placed { object, x, y, crop });
    }
    Ok(Composition {
        pts_ms,
        palette,
        objects,
    })
}

/// PGS run-length coding, line by line. A non-zero byte is one pixel of
/// that colour; a zero byte starts a run, whose next byte says how long and
/// of which colour, or ends the line.
///
/// It never holds more than `width` × `height` pixels (finding 45): runs
/// on a line that does not end would otherwise grow without bound, a
/// 16,383-pixel run per 3 bytes. What is past the size is cut anyway.
fn decode_rle(data: &[u8], width: usize, height: usize) -> Vec<u8> {
    let size = width * height;
    let mut pixels = Vec::with_capacity(size);
    let mut line = 0;
    let mut i = 0;
    let byte = |i: usize| data.get(i).copied().unwrap_or(0);
    while i < data.len() && line < height {
        let b = byte(i);
        i += 1;
        if b != 0 {
            if pixels.len() < size {
                pixels.push(b);
            }
            continue;
        }
        let flags = byte(i);
        i += 1;
        if flags == 0 {
            // End of line: pad or cut it to the width.
            line += 1;
            pixels.resize(line * width, 0);
            continue;
        }
        let long = flags & 0x40 != 0;
        let coloured = flags & 0x80 != 0;
        let mut run = usize::from(flags & 0x3f);
        if long {
            run = (run << 8) | usize::from(byte(i));
            i += 1;
        }
        let colour = if coloured {
            i += 1;
            byte(i - 1)
        } else {
            0
        };
        pixels.extend(std::iter::repeat_n(colour, run.min(size - pixels.len())));
    }
    pixels.resize(size, 0);
    pixels
}

/// The composition's objects drawn where they sit, cut to the box around
/// them, as dark text on white with a margin (tesseract reads text that
/// touches the edge badly). `None` if nothing shows; an error if the box
/// is larger than [`MAX_SIDE`] a side, or if `kept` has no room for it.
fn render(
    composition: &Composition,
    objects: &HashMap<u16, Object>,
    palette: &Palette,
    kept: &mut Kept,
) -> Result<Option<Image>, PgsError> {
    // Each object as it shows: drawn from where it is held, never copied,
    // as a composition may place one object 255 times.
    let placed: Vec<(&Placed, &Object, Visible)> = composition
        .objects
        .iter()
        .filter_map(|p| {
            let object = objects.get(&p.object)?;
            Some((p, object, visible(object, p.crop)))
        })
        .filter(|(_, _, v)| v.width > 0 && v.height > 0)
        .collect();
    let (Some(left), Some(top), Some(right), Some(bottom)) = (
        placed.iter().map(|(p, ..)| p.x).min(),
        placed.iter().map(|(p, ..)| p.y).min(),
        placed.iter().map(|(p, _, v)| p.x + v.width).max(),
        placed.iter().map(|(p, _, v)| p.y + v.height).max(),
    ) else {
        return Ok(None);
    };
    let (width, height) = (right - left, bottom - top);
    // Placed wider than a picture can be (a stale object left at x =
    // 60000): this composition is skipped, as DVB skips such a page, not
    // the whole track failed. An object's own size past it is still an
    // error, at its header (review 2026-10-11 L70).
    if width > MAX_SIDE || height > MAX_SIDE {
        tracing::warn!(
            width,
            height,
            "a PGS composition placed too wide to read was left out"
        );
        return Ok(None);
    }
    // The objects' colours where they show, then through the same text
    // and outline split DVB and VobSub take (review 2026-10-11 L69).
    kept.draw(width, height)?;
    let mut pixels = vec![Colour::default(); width * height];
    for (p, object, v) in &placed {
        for row in 0..v.height {
            for col in 0..v.width {
                let index = object.indices[(v.y + row) * object.width + v.x + col];
                let colour = palette[usize::from(index)];
                if colour.alpha == 0 {
                    continue;
                }
                pixels[(p.y - top + row) * width + (p.x - left + col)] = colour;
            }
        }
    }
    Ok(bitmap::text_picture(width, height, &pixels, kept)?)
}

/// Writes `.sup` streams, for tests: ffmpeg decodes PGS but cannot encode
/// it.
#[cfg(any(test, feature = "fixtures"))]
pub mod fixture {
    use super::{END, ODS, PCS, PDS, WDS};

    /// One cue: a `width` × `height` mask at (`x`, `y`) on a 1920×1080
    /// screen, where 1 is text and 2 its outline.
    pub struct Cue {
        pub start_ms: u64,
        pub end_ms: u64,
        pub x: u16,
        pub y: u16,
        pub width: u16,
        pub height: u16,
        pub mask: Vec<u8>,
    }

    /// A stream showing each cue in turn: white text (palette entry 1)
    /// with a black outline (entry 2), cleared at its end. Objects longer than `fragment` bytes are split
    /// into fragments, as Blu-ray streams split those over 64 KiB.
    pub fn sup(cues: &[Cue], fragment: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for (n, cue) in cues.iter().enumerate() {
            let n = n as u16;
            // An epoch start showing one object: id 0, window 0, not
            // cropped.
            let mut pcs = composition(n * 2, 0x80);
            pcs[10] = 1;
            pcs.extend([0, 0, 0, 0]);
            pcs.extend(cue.x.to_be_bytes());
            pcs.extend(cue.y.to_be_bytes());
            segment(&mut out, cue.start_ms, PCS, &pcs);
            segment(&mut out, cue.start_ms, WDS, &window(cue));
            // Palette 0, version 0; entry 1 is opaque white
            // (Y 235, Cr 128, Cb 128, alpha 255), entry 2 opaque black.
            segment(
                &mut out,
                cue.start_ms,
                PDS,
                &[0, 0, 1, 235, 128, 128, 255, 2, 16, 128, 128, 255],
            );
            let rle = rle(&cue.mask, usize::from(cue.width));
            let mut data = Vec::new();
            data.extend(&((rle.len() + 4) as u32).to_be_bytes()[1..]);
            data.extend(cue.width.to_be_bytes());
            data.extend(cue.height.to_be_bytes());
            data.extend(rle);
            let chunks: Vec<&[u8]> = data.chunks(fragment.max(1)).collect();
            for (i, chunk) in chunks.iter().enumerate() {
                let mut flags = 0;
                if i == 0 {
                    flags |= 0x80;
                }
                if i == chunks.len() - 1 {
                    flags |= 0x40;
                }
                let mut ods = vec![0, 0, 0, flags];
                ods.extend(*chunk);
                segment(&mut out, cue.start_ms, ODS, &ods);
            }
            segment(&mut out, cue.start_ms, END, &[]);
            // The clear: a PCS with no objects.
            segment(&mut out, cue.end_ms, PCS, &composition(n * 2 + 1, 0x00));
            segment(&mut out, cue.end_ms, WDS, &window(cue));
            segment(&mut out, cue.end_ms, END, &[]);
        }
        out
    }

    /// A decompression bomb (finding 45): one `side` × `side` object
    /// whose run-length data is one pixel (the rest of each line is
    /// padding), then shown `times` times, a second apart. Each cue after
    /// the first is 82 bytes of stream; at a side of 4096, a 4128 × 4128
    /// picture (the object and its margin), about 17 million pixels.
    pub fn bomb(times: u16, side: u16) -> Vec<u8> {
        let mut out = Vec::new();
        for n in 0..times {
            let ms = u64::from(n) * 2_000;
            // The first starts an epoch, and sends the palette and object.
            let number = n.wrapping_mul(2);
            let mut pcs = composition(number, if n == 0 { 0x80 } else { 0 });
            pcs[10] = 1;
            pcs.extend([0, 0, 0, 0, 0, 0, 0, 0]);
            segment(&mut out, ms, PCS, &pcs);
            if n == 0 {
                segment(&mut out, ms, PDS, &[0, 0, 1, 235, 128, 128, 255]);
                let mut ods = vec![0, 0, 0, 0xc0];
                ods.extend(&5u32.to_be_bytes()[1..]);
                ods.extend(side.to_be_bytes());
                ods.extend(side.to_be_bytes());
                ods.push(1);
                segment(&mut out, ms, ODS, &ods);
            }
            segment(&mut out, ms, END, &[]);
            segment(&mut out, ms + 1_000, PCS, &composition(number + 1, 0));
            segment(&mut out, ms + 1_000, END, &[]);
        }
        out
    }

    pub(super) fn composition(number: u16, state: u8) -> Vec<u8> {
        let mut pcs = Vec::new();
        pcs.extend(1920u16.to_be_bytes());
        pcs.extend(1080u16.to_be_bytes());
        pcs.push(0x10); // 23.976 fps
        pcs.extend(number.to_be_bytes());
        pcs.extend([state, 0, 0, 0]); // no palette update, palette 0, no objects
        pcs
    }

    pub(super) fn window(cue: &Cue) -> Vec<u8> {
        let mut wds = vec![1, 0];
        for n in [cue.x, cue.y, cue.width, cue.height] {
            wds.extend(n.to_be_bytes());
        }
        wds
    }

    pub(super) fn segment(out: &mut Vec<u8>, ms: u64, kind: u8, body: &[u8]) {
        out.extend(b"PG");
        out.extend(((ms * 90) as u32).to_be_bytes());
        out.extend(0u32.to_be_bytes());
        out.push(kind);
        out.extend((body.len() as u16).to_be_bytes());
        out.extend(body);
    }

    /// Every form PGS run-length coding has, so the decoder meets them all.
    pub fn rle(mask: &[u8], width: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for line in mask.chunks(width) {
            let mut i = 0;
            while i < line.len() {
                let colour = line[i];
                let mut run = 1;
                while i + run < line.len() && line[i + run] == colour && run < 0x3fff {
                    run += 1;
                }
                i += run;
                match (colour, run) {
                    (0, 1..=63) => out.extend([0, run as u8]),
                    (0, _) => out.extend([0, 0x40 | (run >> 8) as u8, run as u8]),
                    (c, 1) => out.push(c),
                    (c, 2..=63) => out.extend([0, 0x80 | run as u8, c]),
                    (c, _) => out.extend([0, 0xc0 | (run >> 8) as u8, run as u8, c]),
                }
            }
            out.extend([0, 0]);
        }
        out
    }
}

/// The part of an object a composition shows: all of it, or its crop.
#[derive(Debug, PartialEq, Eq)]
struct Visible {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

fn visible(object: &Object, crop: Option<(usize, usize, usize, usize)>) -> Visible {
    let Some((x, y, w, h)) = crop else {
        return Visible {
            x: 0,
            y: 0,
            width: object.width,
            height: object.height,
        };
    };
    let w = w.min(object.width.saturating_sub(x));
    let h = h.min(object.height.saturating_sub(y));
    // A crop that starts outside the object shows none of it; its first
    // row would start past the end of the pixels.
    if w == 0 || h == 0 {
        return Visible {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
    }
    Visible {
        x,
        y,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{self, Cue as Shown};
    use super::*;

    /// A `width` × `height` mask with text where `ink` says.
    fn mask(width: u16, height: u16, ink: impl Fn(usize, usize) -> bool) -> Vec<u8> {
        let (w, h) = (usize::from(width), usize::from(height));
        (0..w * h).map(|i| u8::from(ink(i % w, i / w))).collect()
    }

    fn shown(start_ms: u64, end_ms: u64, y: u16, width: u16, height: u16) -> Shown {
        Shown {
            start_ms,
            end_ms,
            x: 100,
            y,
            width,
            height,
            // A bar in the middle third of each line.
            mask: mask(width, height, |x, _| {
                x >= usize::from(width) / 3 && x < 2 * usize::from(width) / 3
            }),
        }
    }

    /// The mask as the parser draws it: dark text on white, inside the
    /// margin.
    fn ink(image: &Image) -> Vec<bool> {
        image.pixels.iter().map(|&p| p < 128).collect()
    }

    /// A stream showing one `width` × `height` object, all text, once at
    /// each of `xs` on the top line (PX-09 B-3).
    fn placed(width: u16, height: u16, xs: &[u16]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut pcs = fixture::composition(0, 0x80);
        pcs[10] = xs.len() as u8;
        for x in xs {
            pcs.extend([0, 0, 0, 0]);
            pcs.extend(x.to_be_bytes());
            pcs.extend(0u16.to_be_bytes());
        }
        fixture::segment(&mut out, 0, PCS, &pcs);
        fixture::segment(&mut out, 0, PDS, &[0, 0, 1, 235, 128, 128, 255]);
        // One line of text when small; past that the lines are padded.
        let line = vec![1; usize::from(width).min(64)];
        let rle = fixture::rle(&line, line.len());
        let mut data = Vec::new();
        data.extend(&((rle.len() + 4) as u32).to_be_bytes()[1..]);
        data.extend(width.to_be_bytes());
        data.extend(height.to_be_bytes());
        data.extend(rle);
        let mut ods = vec![0, 0, 0, 0xc0];
        ods.extend(data);
        fixture::segment(&mut out, 0, ODS, &ods);
        fixture::segment(&mut out, 0, END, &[]);
        // Cleared, so the cue has an end.
        fixture::segment(&mut out, 1_000, PCS, &fixture::composition(1, 0));
        fixture::segment(&mut out, 1_000, END, &[]);
        out
    }

    /// PX-09 B-3: an ODS header says 65535 × 65535, which would be 4 GiB of
    /// pixels; the stream is refused instead.
    #[test]
    fn an_object_of_65535_squared_is_an_error() {
        assert!(cues(&placed(0xffff, 0xffff, &[0])).is_err());
    }

    /// Just past the cap, as small as that allows.
    #[test]
    fn an_object_wider_than_4096_is_an_error() {
        assert!(cues(&placed(4096, 1, &[0])).is_ok());
        assert!(cues(&placed(4097, 1, &[0])).is_err());
    }

    /// Small objects placed far apart would make one picture as wide as
    /// the gap between them: that composition is left out, and the rest of
    /// the track read, not the whole track failed (review 2026-10-11 L70).
    #[test]
    fn objects_placed_further_apart_than_4096_are_left_out() {
        assert_eq!(cues(&placed(8, 1, &[0, 3_000])).unwrap().len(), 1);
        assert!(cues(&placed(8, 1, &[0, 60_000])).unwrap().is_empty());
    }

    /// A bomb cue's composite: the 4096 × 4096 object. It is counted
    /// before it is drawn, though the picture kept is cut to its one pixel
    /// of text (review 2026-10-11 L69).
    const BOMB_PICTURE: u64 = 4096 * 4096;

    /// Finding 45: under a kilobyte of stream is eight full 4096 × 4096
    /// composites. Past its budget the track is refused, not decoded on.
    #[test]
    fn a_track_past_its_budget_of_pixels_is_refused() {
        let bomb = fixture::bomb(8, 4096);
        assert!(bomb.len() < 1024, "{} bytes", bomb.len());
        let budget = Budget {
            cues: 1_000,
            pixels: 3 * BOMB_PICTURE,
            held: 64 << 20,
        };
        match cues_within(&bomb, &budget) {
            Err(e) => assert_eq!(e, PgsError::Budget(OverBudget::Pixels(budget.pixels))),
            Ok(cues) => panic!(
                "{} bytes decoded to {} cues of {} pixels",
                bomb.len(),
                cues.len(),
                cues.iter().map(|c| c.image.pixels.len()).sum::<usize>()
            ),
        }
    }

    /// Small pictures, but more of them than the budget keeps.
    #[test]
    fn a_track_past_its_budget_of_cues_is_refused() {
        let cues: Vec<Shown> = (0..5)
            .map(|n| shown(n * 2_000, n * 2_000 + 1_000, 900, 30, 10))
            .collect();
        let sup = fixture::sup(&cues, 65_000);
        let budget = Budget {
            cues: 4,
            ..Budget::DEFAULT
        };
        assert_eq!(
            cues_within(&sup, &budget).map(|c| c.len()),
            Err(PgsError::Budget(OverBudget::Cues(4)))
        );
        assert_eq!(cues_within(&sup, &Budget::DEFAULT).map(|c| c.len()), Ok(5));
    }

    /// Objects stay until the next epoch: many full-size ones under
    /// different ids, sent before any is shown, are refused as they come,
    /// before each is decoded.
    #[test]
    fn objects_held_past_the_budget_are_refused() {
        let mut sup = Vec::new();
        let mut pcs = fixture::composition(0, 0x80);
        pcs[10] = 1;
        pcs.extend([0, 0, 0, 0, 0, 0, 0, 0]);
        fixture::segment(&mut sup, 0, PCS, &pcs);
        for id in 0..64u16 {
            let mut ods = id.to_be_bytes().to_vec();
            ods.extend([0, 0xc0]);
            ods.extend(&5u32.to_be_bytes()[1..]);
            ods.extend(4096u16.to_be_bytes());
            ods.extend(4096u16.to_be_bytes());
            ods.push(1);
            fixture::segment(&mut sup, 0, ODS, &ods);
        }
        fixture::segment(&mut sup, 0, END, &[]);
        let held = 3 * 4096 * 4096;
        let budget = Budget {
            held,
            ..Budget::DEFAULT
        };
        assert_eq!(
            cues_within(&sup, &budget).map(|c| c.len()),
            Err(PgsError::Budget(OverBudget::Held(held)))
        );
    }

    /// Runs on a line that never ends are not kept past the object's size:
    /// here 1,000 runs of 16,383 pixels, 4 bytes each, on a 1 × 1 object.
    #[test]
    fn runs_past_the_object_s_size_are_not_held() {
        let pixels = decode_rle(&[0, 0xff, 0xff, 1].repeat(1_000), 1, 1);
        assert_eq!(pixels, [1]);
        assert_eq!(pixels.capacity(), 1);
    }

    #[test]
    fn every_run_length_form_decodes_to_the_mask() {
        // Single pixels, runs under 64 and over, of text and of nothing.
        let width = 300;
        let mask = mask(width, 3, |x, y| match y {
            0 => x % 2 == 0,
            1 => (10..40).contains(&x),
            _ => (5..205).contains(&x),
        });
        let decoded = decode_rle(
            &fixture::rle(&mask, usize::from(width)),
            usize::from(width),
            3,
        );
        assert_eq!(decoded, mask);
    }

    #[test]
    fn each_cue_runs_from_its_composition_to_the_next() {
        let sup = fixture::sup(
            &[
                shown(1_000, 3_500, 900, 120, 30),
                shown(5_000, 7_250, 900, 90, 30),
            ],
            65_000,
        );
        let cues = cues(&sup).unwrap();
        let times: Vec<_> = cues.iter().map(|c| (c.start_ms, c.end_ms)).collect();
        assert_eq!(times, [(1_000, 3_500), (5_000, 7_250)]);
        // The text (the middle third of the 120-wide object), cut to it,
        // inside a 16-pixel white margin.
        assert_eq!((cues[0].image.width, cues[0].image.height), (72, 62));
        let row = &ink(&cues[0].image)[20 * 72..21 * 72];
        assert!(!row[15] && row[16] && row[16 + 39] && !row[16 + 40]);
    }

    /// An opaque black outline (Y = 16) around light text is outline, not
    /// text: PGS goes through the same text and outline split as DVB and
    /// VobSub, its luma de-video-ranged. Review 2026-10-11 L69.
    #[test]
    fn a_black_outline_is_not_read_as_text() {
        let mut out = Vec::new();
        let mut pcs = fixture::composition(0, 0x80);
        pcs[10] = 1;
        pcs.extend([0, 0, 0, 0, 0, 100, 0, 100]);
        fixture::segment(&mut out, 0, PCS, &pcs);
        // Index 1 white text, index 2 an opaque black outline.
        fixture::segment(
            &mut out,
            0,
            PDS,
            &[0, 0, 1, 235, 128, 128, 255, 2, 16, 128, 128, 255],
        );
        // 9 × 9: a 5 × 3 bar of text in a 2-pixel outline.
        let mask: Vec<u8> = (0..81)
            .map(|at| match (at % 9, at / 9) {
                (2..=6, 3..=5) => 1,
                (0..=8, 1..=7) => 2,
                _ => 0,
            })
            .collect();
        let rle = fixture::rle(&mask, 9);
        let mut ods = vec![0, 0, 0, 0xc0];
        ods.extend(&((rle.len() + 4) as u32).to_be_bytes()[1..]);
        ods.extend(9u16.to_be_bytes());
        ods.extend(9u16.to_be_bytes());
        ods.extend(rle);
        fixture::segment(&mut out, 0, ODS, &ods);
        fixture::segment(&mut out, 0, END, &[]);
        fixture::segment(&mut out, 1_000, PCS, &fixture::composition(1, 0));
        fixture::segment(&mut out, 1_000, END, &[]);
        let cues = cues(&out).unwrap();
        // Only the text's 5 × 3, in its margin; the outline is not ink.
        let image = &cues[0].image;
        assert_eq!((image.width, image.height), (5 + 32, 3 + 32));
        assert_eq!(ink(image).iter().filter(|&&i| i).count(), 15);
    }

    /// Continuation fragments past what the object's picture can need
    /// refuse the stream, rather than growing what is held without bound
    /// (review 2026-10-11 M35).
    #[test]
    fn fragments_past_what_a_picture_needs_are_refused() {
        let mut sup = Vec::new();
        // Object 7, 2 × 2, its first fragment: a 24-bit length, then size.
        let mut first = vec![0, 7, 0, 0x80, 0, 0, 12, 0, 2, 0, 2];
        first.extend([1, 1, 0, 0]);
        fixture::segment(&mut sup, 0, ODS, &first);
        // Then middle fragments of a megabyte, far past 2 × 2's need.
        let mut middle = vec![0, 7, 0, 0x00];
        middle.extend(std::iter::repeat_n(1u8, 60_000));
        for _ in 0..20 {
            fixture::segment(&mut sup, 0, ODS, &middle);
        }
        assert!(matches!(cues(&sup), Err(PgsError::Overlong(7))));
    }

    #[test]
    fn an_object_in_fragments_is_the_same_object() {
        let cue = || [shown(0, 1_000, 0, 400, 60)];
        let whole = cues(&fixture::sup(&cue(), 65_000)).unwrap();
        let split = cues(&fixture::sup(&cue(), 50)).unwrap();
        assert_eq!(whole, split);
    }

    #[test]
    fn a_fade_is_one_cue() {
        // Shown at 1 s, its palette dimmed at 2 s (the same object, no new
        // ODS), cleared at 3 s.
        let first = shown(1_000, 2_000, 900, 120, 30);
        let mut sup = fixture::sup(std::slice::from_ref(&first), 65_000);
        // Drop the clear the fixture wrote; write the fade, then the clear.
        let clear_len = 3 * 13 + 11 + 10;
        sup.truncate(sup.len() - clear_len);
        let mut pcs = fixture::composition(1, 0x00);
        pcs[8] = 0x80; // palette update only
        pcs[10] = 1;
        pcs.extend([0, 0, 0, 0]);
        pcs.extend(first.x.to_be_bytes());
        pcs.extend(first.y.to_be_bytes());
        fixture::segment(&mut sup, 2_000, PCS, &pcs);
        fixture::segment(&mut sup, 2_000, PDS, &[0, 1, 1, 235, 128, 128, 128]);
        fixture::segment(&mut sup, 2_000, END, &[]);
        fixture::segment(&mut sup, 3_000, PCS, &fixture::composition(2, 0x00));
        fixture::segment(&mut sup, 3_000, END, &[]);
        let cues = cues(&sup).unwrap();
        let times: Vec<_> = cues.iter().map(|c| (c.start_ms, c.end_ms)).collect();
        assert_eq!(times, [(1_000, 3_000)]);
    }

    #[test]
    fn two_objects_are_drawn_on_one_picture_where_they_sit() {
        // Two lines: objects 0 and 1, 40 pixels apart.
        let mut sup = Vec::new();
        let mut pcs = fixture::composition(0, 0x80);
        pcs[10] = 2;
        for (id, y) in [(0u16, 900u16), (1, 940)] {
            pcs.extend(id.to_be_bytes());
            pcs.extend([0, 0]);
            pcs.extend(100u16.to_be_bytes());
            pcs.extend(y.to_be_bytes());
        }
        fixture::segment(&mut sup, 0, PCS, &pcs);
        fixture::segment(&mut sup, 0, PDS, &[0, 0, 1, 235, 128, 128, 255]);
        for id in [0u16, 1] {
            let rle = fixture::rle(&[1; 20 * 10], 20);
            let mut ods = id.to_be_bytes().to_vec();
            ods.extend([0, 0xc0]);
            ods.extend(&((rle.len() + 4) as u32).to_be_bytes()[1..]);
            ods.extend(20u16.to_be_bytes());
            ods.extend(10u16.to_be_bytes());
            ods.extend(rle);
            fixture::segment(&mut sup, 0, ODS, &ods);
        }
        fixture::segment(&mut sup, 0, END, &[]);
        fixture::segment(&mut sup, 1_000, PCS, &fixture::composition(1, 0x00));
        fixture::segment(&mut sup, 1_000, END, &[]);
        let cues = cues(&sup).unwrap();
        assert_eq!(cues.len(), 1);
        let image = &cues[0].image;
        // 900 to 950, plus the margins.
        assert_eq!((image.width, image.height), (52, 82));
        let ink = ink(image);
        let at = |y: usize| ink[(16 + y) * 52 + 26];
        assert!(at(0) && at(9) && !at(10) && !at(39) && at(40) && at(49));
    }

    #[test]
    fn a_stream_that_is_not_pgs_is_refused() {
        assert_eq!(
            cues(b"WEBVTT\n\n00:00.000 --> 00:01.000\nhi\n"),
            Err(PgsError::BadMagic(0))
        );
        let mut sup = fixture::sup(&[shown(0, 1_000, 0, 10, 10)], 65_000);
        sup.truncate(sup.len() - 1);
        assert!(matches!(cues(&sup), Err(PgsError::Truncated(_))));
    }

    /// Found by the `pgs` fuzz target (PX-10): a composition's crop that
    /// starts right of or below its object panicked on a slice out of
    /// range. A crop outside the object shows nothing.
    #[test]
    fn a_crop_outside_its_object_shows_nothing() {
        let object = Object {
            width: 4,
            height: 2,
            indices: vec![1; 8],
        };
        for crop in [(4, 0, 2, 2), (9, 0, 2, 2), (0, 2, 2, 2), (0, 7, 2, 2)] {
            let shown = visible(&object, Some(crop));
            assert_eq!(shown.width * shown.height, 0, "{crop:?}");
        }
        // Inside, as before.
        assert_eq!(
            visible(&object, Some((3, 1, 5, 5))),
            Visible {
                x: 3,
                y: 1,
                width: 1,
                height: 1
            }
        );
    }
}
