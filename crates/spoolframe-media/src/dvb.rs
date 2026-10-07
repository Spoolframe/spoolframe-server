//! DVB subtitles (EN 300 743, European digital TV): the cues of a track,
//! each as a grey picture of its text, for OCR.
//!
//! Each packet holds segments, each `0x0f`, a type, a page id and a
//! length. A display set is a page composition (which regions show, and
//! where), region compositions (each region's size, colour depth, CLUT and
//! the objects drawn in it), CLUT definitions, object data (the pixels,
//! run-length coded in two fields), then an end of display set. Regions,
//! CLUTs and objects stay until a page starts a new epoch (acquisition
//! point or mode change). A page shows from its packet's time until the
//! next page, or until its time-out, whichever is first.

use std::collections::HashMap;

use crate::bitmap::{self, Budget, Colour, Cue, Kept, MAX_SIDE, OverBudget, Packet};

const PAGE: u8 = 0x10;
const REGION: u8 = 0x11;
const CLUT: u8 = 0x12;
const OBJECT: u8 = 0x13;
const END: u8 = 0x80;

/// A region: its pixel codes, and how to colour them.
#[derive(Debug, Clone)]
struct Region {
    width: usize,
    height: usize,
    /// 2, 4 or 8 bits a pixel.
    depth: u8,
    clut: u8,
    /// The objects in it and where.
    objects: Vec<(u16, usize, usize)>,
    pixels: Vec<u8>,
}

/// A CLUT's entries for each depth: 4, 16 and 256 colours.
#[derive(Debug, Clone)]
struct Clut {
    two: [Colour; 4],
    four: [Colour; 16],
    eight: [Colour; 256],
}

impl Clut {
    fn get(&self, depth: u8, code: u8) -> Colour {
        match depth {
            2 => self.two[usize::from(code & 3)],
            4 => self.four[usize::from(code & 0x0f)],
            _ => self.eight[usize::from(code)],
        }
    }
}

/// What the page shows: regions and where.
#[derive(Debug, Default)]
struct Page {
    time_out_ms: u64,
    regions: Vec<(u8, usize, usize)>,
}

/// The cues of a track's packets, in order, within [`Budget::DEFAULT`].
/// Segments that do not parse end their packet's reading, and what was
/// read of it still counts.
pub fn cues(packets: &[Packet]) -> Result<Vec<Cue>, OverBudget> {
    cues_within(packets, &Budget::DEFAULT)
}

/// The cues of a track's packets, in order. A track that decodes to more
/// than `budget` is an error.
pub fn cues_within(packets: &[Packet], budget: &Budget) -> Result<Vec<Cue>, OverBudget> {
    let mut regions = Regions::default();
    let mut cluts: HashMap<u8, Clut> = HashMap::new();
    let mut out = Kept::new(budget);
    // What shows: since when, until its time-out (if it has one), and the
    // picture.
    let mut shown: Option<(u64, Option<u64>, bitmap::Image)> = None;
    let mut page: Option<Page> = None;
    let mut broken = 0;

    for packet in packets {
        let mut data = packet.data.as_slice();
        // In MPEG-TS, a data identifier (0x20) and stream id come first.
        if data.first() == Some(&0x20) {
            data = &data[2.min(data.len())..];
        }
        let mut composed = false;
        while let [0x0f, kind, _, _, high, low, rest @ ..] = data {
            let len = usize::from(u16::from_be_bytes([*high, *low]));
            let Some(body) = rest.get(..len) else {
                broken += 1;
                break;
            };
            data = &rest[len..];
            let parsed = match *kind {
                PAGE => match page_composition(body) {
                    Some((p, epoch)) => {
                        // The page before ends here, if its time-out has
                        // not ended it already.
                        if let Some((start, time_out, image)) = shown.take() {
                            let end = time_out.map_or(packet.pts_ms, |t| t.min(packet.pts_ms));
                            out.push(start, end, image)?;
                        }
                        if epoch {
                            regions = Regions::default();
                            cluts.clear();
                        }
                        page = Some(p);
                        composed = true;
                        Some(())
                    }
                    None => None,
                },
                REGION => region_composition(body, &mut regions, budget)?,
                CLUT => clut_definition(body, &mut cluts),
                OBJECT => object_data(body, &mut regions.by_id),
                END => {
                    if composed {
                        shown = render(page.as_ref(), &regions.by_id, &cluts, packet.pts_ms, &out)?;
                        composed = false;
                    }
                    Some(())
                }
                // Display definition, disparity and the like.
                _ => Some(()),
            };
            if parsed.is_none() {
                broken += 1;
                break;
            }
        }
        // A display set whose end segment is missing ends with its packet.
        if composed {
            shown = render(page.as_ref(), &regions.by_id, &cluts, packet.pts_ms, &out)?;
        }
    }
    // The last page ends at its time-out; one without is dropped, as the
    // last PGS cue is.
    if let Some((start, Some(time_out), image)) = shown {
        out.push(start, time_out, image)?;
    }
    if broken > 0 {
        tracing::warn!(
            broken,
            "DVB subtitle segments that could not be read were left out"
        );
    }
    Ok(out.into_cues())
}

/// The regions sent this epoch, and their pixels added up.
#[derive(Default)]
struct Regions {
    by_id: HashMap<u8, Region>,
    held: u64,
}

/// The page, and whether it starts a new epoch.
fn page_composition(body: &[u8]) -> Option<(Page, bool)> {
    let [time_out, flags, entries @ ..] = body else {
        return None;
    };
    // State 1 is an acquisition point, 2 a mode change: everything is sent
    // again. 0 is an update of what was sent.
    let epoch = (flags >> 2) & 3 != 0;
    let regions = entries
        .as_chunks::<6>()
        .0
        .iter()
        .map(|&[id, _, x1, x2, y1, y2]| {
            (
                id,
                usize::from(u16::from_be_bytes([x1, x2])),
                usize::from(u16::from_be_bytes([y1, y2])),
            )
        })
        .collect();
    Some((
        Page {
            time_out_ms: u64::from(*time_out) * 1000,
            regions,
        },
        epoch,
    ))
}

/// A region's size, colours and objects. `None` if it does not parse; an
/// error if holding it would pass `budget`.
fn region_composition(
    body: &[u8],
    regions: &mut Regions,
    budget: &Budget,
) -> Result<Option<()>, OverBudget> {
    let Some(head) = body.get(..10) else {
        return Ok(None);
    };
    let id = head[0];
    let fill = head[1] & 0x08 != 0;
    let width = usize::from(u16::from_be_bytes([head[2], head[3]]));
    let height = usize::from(u16::from_be_bytes([head[4], head[5]]));
    // Allocated as told (PX-09 B-3): past the cap the segment is refused,
    // and counted with those that do not parse.
    if width > MAX_SIDE || height > MAX_SIDE {
        return Ok(None);
    }
    let depth = match (head[6] >> 2) & 7 {
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let clut = head[7];
    let background = match depth {
        2 => (head[9] >> 2) & 3,
        4 => head[9] >> 4,
        _ => head[8],
    };
    let mut objects = Vec::new();
    let mut at = 10;
    while let Some(entry) = body.get(at..at + 6) {
        let object = u16::from_be_bytes([entry[0], entry[1]]);
        let kind = entry[2] >> 6;
        let x = usize::from(u16::from_be_bytes([entry[2], entry[3]]) & 0x0fff);
        let y = usize::from(u16::from_be_bytes([entry[4], entry[5]]) & 0x0fff);
        objects.push((object, x, y));
        // Character objects carry a foreground and background code.
        at += if kind == 1 || kind == 2 { 8 } else { 6 };
    }
    // Counted before it is allocated; a region it resizes is let go.
    let before = regions.by_id.get(&id).map(|r| r.pixels.len());
    if before != Some(width * height) {
        let held = regions.held - before.unwrap_or(0) as u64;
        regions.held = budget.hold(held, (width * height) as u64)?;
    }
    let region = regions.by_id.entry(id).or_insert_with(|| Region {
        width,
        height,
        depth,
        clut,
        objects: Vec::new(),
        pixels: vec![0; width * height],
    });
    if (region.width, region.height) != (width, height) {
        region.pixels = vec![0; width * height];
    }
    (region.width, region.height, region.depth, region.clut) = (width, height, depth, clut);
    region.objects = objects;
    if fill {
        region.pixels.fill(background);
    }
    Ok(Some(()))
}

fn clut_definition(body: &[u8], cluts: &mut HashMap<u8, Clut>) -> Option<()> {
    let (&id, rest) = body.split_first()?;
    let clut = cluts.entry(id).or_insert_with(default_clut);
    let mut at = 1; // after the version
    while let Some(&[entry, flags]) = rest.get(at..at + 2) {
        // Y, Cr, Cb and T; the colour does not matter, only how bright.
        let (y, t) = if flags & 1 != 0 {
            let b = rest.get(at + 2..at + 6)?;
            at += 6;
            (b[0], b[3])
        } else {
            // Y in 6 bits, Cr and Cb in 4, T in 2.
            let b = rest.get(at + 2..at + 4)?;
            at += 4;
            (b[0] & 0xfc, (b[1] & 3) << 6)
        };
        // Y 0 is fully transparent whatever T says. Otherwise Y is in
        // video range: 16 is black, 235 white.
        let colour = Colour {
            luma: (u32::from(y.saturating_sub(16)) * 255 / 219).min(255) as u8,
            alpha: if y == 0 { 0 } else { 255 - t },
        };
        let entry = usize::from(entry);
        if flags & 0x80 != 0 && entry < 4 {
            clut.two[entry] = colour;
        }
        if flags & 0x40 != 0 && entry < 16 {
            clut.four[entry] = colour;
        }
        if flags & 0x20 != 0 {
            clut.eight[entry] = colour;
        }
    }
    Some(())
}

/// The CLUT a region has before (or without) its definition: the
/// standard's defaults.
fn default_clut() -> Clut {
    let opaque = |r: u8, g: u8, b: u8, alpha: u8| Colour {
        luma: bitmap::luma(r, g, b),
        alpha,
    };
    let bit = |i: usize, mask: usize, value: u8| if i & mask != 0 { value } else { 0 };
    let mut clut = Clut {
        two: [
            Colour::default(),
            opaque(255, 255, 255, 255),
            opaque(0, 0, 0, 255),
            opaque(127, 127, 127, 255),
        ],
        four: [Colour::default(); 16],
        eight: [Colour::default(); 256],
    };
    for i in 1..16 {
        let v = if i < 8 { 255 } else { 127 };
        clut.four[i] = opaque(bit(i, 1, v), bit(i, 2, v), bit(i, 4, v), 255);
    }
    for i in 1..256 {
        clut.eight[i] = if i < 8 {
            opaque(bit(i, 1, 255), bit(i, 2, 255), bit(i, 4, 255), 63)
        } else {
            let channel = |low: usize, high: usize| -> (u8, u8) {
                (
                    bit(i, low, 85) + bit(i, high, 170),
                    bit(i, low, 43) + bit(i, high, 85),
                )
            };
            let ((r, r_dim), (g, g_dim), (b, b_dim)) =
                (channel(1, 0x10), channel(2, 0x20), channel(4, 0x40));
            match i & 0x88 {
                0x00 => opaque(r, g, b, 255),
                0x08 => opaque(r, g, b, 127),
                0x80 => opaque(127 + r_dim, 127 + g_dim, 127 + b_dim, 255),
                _ => opaque(r_dim, g_dim, b_dim, 255),
            }
        };
    }
    clut
}

fn object_data(body: &[u8], regions: &mut HashMap<u8, Region>) -> Option<()> {
    let head = body.get(..3)?;
    let id = u16::from_be_bytes([head[0], head[1]]);
    // Coded as pixels; character strings are not drawn.
    if (head[2] >> 2) & 3 != 0 {
        return Some(());
    }
    let non_modifying = head[2] & 0x02 != 0;
    let lengths = body.get(3..7)?;
    let top_len = usize::from(u16::from_be_bytes([lengths[0], lengths[1]]));
    let bottom_len = usize::from(u16::from_be_bytes([lengths[2], lengths[3]]));
    let top = body.get(7..7 + top_len)?;
    // No bottom field: the top one is drawn on both.
    let bottom = match bottom_len {
        0 => top,
        n => body.get(7 + top_len..7 + top_len + n)?,
    };
    for region in regions.values_mut() {
        let places: Vec<(usize, usize)> = region
            .objects
            .iter()
            .filter(|(object, ..)| *object == id)
            .map(|&(_, x, y)| (x, y))
            .collect();
        for (x, y) in places {
            draw_field(region, top, x, y, non_modifying);
            draw_field(region, bottom, x, y + 1, non_modifying);
        }
    }
    Some(())
}

/// One field's pixel data into `region`, from (`x`, `y`), every other line.
fn draw_field(region: &mut Region, data: &[u8], x0: usize, y0: usize, non_modifying: bool) {
    let mut maps = Maps::default();
    let (mut x, mut y) = (x0, y0);
    let mut at = 0;
    while let Some(&kind) = data.get(at) {
        at += 1;
        let mut bits = Bits { data, at: at * 8 };
        let mut runs = Vec::new();
        match kind {
            0x10 => two_bit(&mut bits, &mut runs),
            0x11 => four_bit(&mut bits, &mut runs),
            0x12 => eight_bit(&mut bits, &mut runs),
            0x20 => {
                maps.two_to_four = [0; 4].map(|_| bits.read(4).unwrap_or(0) as u8);
            }
            0x21 => {
                maps.two_to_eight = [0; 4].map(|_| bits.read(8).unwrap_or(0) as u8);
            }
            0x22 => {
                maps.four_to_eight = std::array::from_fn(|_| bits.read(8).unwrap_or(0) as u8);
            }
            0xf0 => {
                x = x0;
                y += 2;
            }
            _ => return,
        }
        at = bits.at.div_ceil(8);
        let map = |code: u8| match (kind, region.depth) {
            (0x10, 4) => maps.two_to_four[usize::from(code)],
            (0x10, 8) => maps.two_to_eight[usize::from(code)],
            (0x11, 8) => maps.four_to_eight[usize::from(code)],
            _ => code,
        };
        for (run, code) in runs {
            // Non-modifying colour: code 1 leaves what is there.
            if !(non_modifying && code == 1) && y < region.height {
                let line = &mut region.pixels[y * region.width..(y + 1) * region.width];
                let from = x.min(region.width);
                let to = (x + run).min(region.width);
                line[from..to].fill(map(code));
            }
            x += run;
        }
    }
}

/// Codes from fewer bits to a region's more, as the standard sets them
/// unless a map table segment says otherwise.
struct Maps {
    two_to_four: [u8; 4],
    two_to_eight: [u8; 4],
    four_to_eight: [u8; 16],
}

impl Default for Maps {
    fn default() -> Self {
        Self {
            two_to_four: [0x0, 0x7, 0x8, 0xf],
            two_to_eight: [0x00, 0x77, 0x88, 0xff],
            four_to_eight: std::array::from_fn(|i| (i * 0x11) as u8),
        }
    }
}

/// Bits, high first.
struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn read(&mut self, n: usize) -> Option<usize> {
        let mut v = 0;
        for _ in 0..n {
            let byte = self.data.get(self.at / 8)?;
            v = (v << 1) | usize::from((byte >> (7 - self.at % 8)) & 1);
            self.at += 1;
        }
        Some(v)
    }
}

/// A 2-bit/pixel code string as `(run, code)`, up to its end code.
fn two_bit(bits: &mut Bits, runs: &mut Vec<(usize, u8)>) {
    let mut step = || -> Option<bool> {
        let code = bits.read(2)?;
        if code != 0 {
            runs.push((1, code as u8));
        } else if bits.read(1)? == 1 {
            let run = bits.read(3)? + 3;
            runs.push((run, bits.read(2)? as u8));
        } else if bits.read(1)? == 1 {
            runs.push((1, 0));
        } else {
            match bits.read(2)? {
                0 => return Some(false),
                1 => runs.push((2, 0)),
                2 => {
                    let run = bits.read(4)? + 12;
                    runs.push((run, bits.read(2)? as u8));
                }
                _ => {
                    let run = bits.read(8)? + 29;
                    runs.push((run, bits.read(2)? as u8));
                }
            }
        }
        Some(true)
    };
    while step() == Some(true) {}
}

/// A 4-bit/pixel code string as `(run, code)`, up to its end code.
fn four_bit(bits: &mut Bits, runs: &mut Vec<(usize, u8)>) {
    let mut step = || -> Option<bool> {
        let code = bits.read(4)?;
        if code != 0 {
            runs.push((1, code as u8));
        } else if bits.read(1)? == 0 {
            match bits.read(3)? {
                0 => return Some(false),
                run => runs.push((run + 2, 0)),
            }
        } else if bits.read(1)? == 0 {
            let run = bits.read(2)? + 4;
            runs.push((run, bits.read(4)? as u8));
        } else {
            match bits.read(2)? {
                0 => runs.push((1, 0)),
                1 => runs.push((2, 0)),
                2 => {
                    let run = bits.read(4)? + 9;
                    runs.push((run, bits.read(4)? as u8));
                }
                _ => {
                    let run = bits.read(8)? + 25;
                    runs.push((run, bits.read(4)? as u8));
                }
            }
        }
        Some(true)
    };
    while step() == Some(true) {}
}

/// An 8-bit/pixel code string as `(run, code)`, up to its end code.
fn eight_bit(bits: &mut Bits, runs: &mut Vec<(usize, u8)>) {
    let mut step = || -> Option<bool> {
        let code = bits.read(8)?;
        if code != 0 {
            runs.push((1, code as u8));
            return Some(true);
        }
        let coloured = bits.read(1)? == 1;
        let run = bits.read(7)?;
        match (coloured, run) {
            (false, 0) => return Some(false),
            (false, run) => runs.push((run, 0)),
            (true, run) => runs.push((run, bits.read(8)? as u8)),
        }
        Some(true)
    };
    while step() == Some(true) {}
}

/// The page's regions drawn where they sit, shown from `pts_ms` until its
/// time-out at the latest (a time-out of 0: until the next page). `None`
/// if nothing shows; an error if `kept` has no room for its picture.
fn render(
    page: Option<&Page>,
    regions: &HashMap<u8, Region>,
    cluts: &HashMap<u8, Clut>,
    pts_ms: u64,
    kept: &Kept,
) -> Result<Option<(u64, Option<u64>, bitmap::Image)>, OverBudget> {
    let Some(page) = page else {
        return Ok(None);
    };
    let placed: Vec<(&Region, usize, usize)> = page
        .regions
        .iter()
        .filter_map(|&(id, x, y)| Some((regions.get(&id)?, x, y)))
        .collect();
    let (Some(left), Some(top), Some(right), Some(bottom)) = (
        placed.iter().map(|&(_, x, _)| x).min(),
        placed.iter().map(|&(_, _, y)| y).min(),
        placed.iter().map(|&(r, x, _)| x + r.width).max(),
        placed.iter().map(|&(r, _, y)| y + r.height).max(),
    ) else {
        return Ok(None);
    };
    let (width, height) = (right - left, bottom - top);
    if width > MAX_SIDE || height > MAX_SIDE {
        tracing::warn!(
            width,
            height,
            "a DVB subtitle page too large to read was left out"
        );
        return Ok(None);
    }
    let defaults = default_clut();
    let mut pixels = vec![Colour::default(); width * height];
    for (region, x, y) in placed {
        let clut = cluts.get(&region.clut).unwrap_or(&defaults);
        for row in 0..region.height {
            for col in 0..region.width {
                let code = region.pixels[row * region.width + col];
                pixels[(y - top + row) * width + (x - left + col)] = clut.get(region.depth, code);
            }
        }
    }
    let Some(image) = bitmap::text_picture(width, height, &pixels, kept)? else {
        return Ok(None);
    };
    let time_out = (page.time_out_ms > 0).then_some(pts_ms + page.time_out_ms);
    Ok(Some((pts_ms, time_out, image)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cues within the default budget, which these never reach.
    fn cues(packets: &[Packet]) -> Vec<Cue> {
        super::cues(packets).unwrap()
    }

    /// Bits, high first, for writing code strings.
    #[derive(Default)]
    struct Writer {
        bytes: Vec<u8>,
        bits: usize,
    }

    impl Writer {
        fn put(&mut self, n: usize, v: usize) -> &mut Self {
            for i in (0..n).rev() {
                if self.bits.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                let bit = ((v >> i) & 1) as u8;
                *self.bytes.last_mut().unwrap() |= bit << (7 - self.bits % 8);
                self.bits += 1;
            }
            self
        }
    }

    fn runs(read: fn(&mut Bits, &mut Vec<(usize, u8)>), data: &[u8]) -> Vec<(usize, u8)> {
        let mut runs = Vec::new();
        read(&mut Bits { data, at: 0 }, &mut runs);
        runs
    }

    #[test]
    fn every_two_bit_form_decodes_to_its_runs() {
        let mut w = Writer::default();
        w.put(2, 3); // one pixel of 3
        w.put(2, 0).put(1, 1).put(3, 2).put(2, 1); // 3 + 2 = 5 of 1
        w.put(2, 0).put(1, 0).put(1, 1); // one of 0
        w.put(2, 0).put(1, 0).put(1, 0).put(2, 1); // two of 0
        w.put(2, 0)
            .put(1, 0)
            .put(1, 0)
            .put(2, 2)
            .put(4, 8)
            .put(2, 2); // 12 + 8 of 2
        w.put(2, 0)
            .put(1, 0)
            .put(1, 0)
            .put(2, 3)
            .put(8, 71)
            .put(2, 3); // 29 + 71 of 3
        w.put(2, 0).put(1, 0).put(1, 0).put(2, 0); // the end
        w.put(2, 1); // after the end: not read
        assert_eq!(
            runs(two_bit, &w.bytes),
            [(1, 3), (5, 1), (1, 0), (2, 0), (20, 2), (100, 3)]
        );
    }

    #[test]
    fn every_four_bit_form_decodes_to_its_runs() {
        let mut w = Writer::default();
        w.put(4, 9); // one pixel of 9
        w.put(4, 0).put(1, 0).put(3, 3); // 3 + 2 = 5 of 0
        w.put(4, 0).put(1, 1).put(1, 0).put(2, 2).put(4, 5); // 4 + 2 of 5
        w.put(4, 0).put(1, 1).put(1, 1).put(2, 0); // one of 0
        w.put(4, 0).put(1, 1).put(1, 1).put(2, 1); // two of 0
        w.put(4, 0)
            .put(1, 1)
            .put(1, 1)
            .put(2, 2)
            .put(4, 6)
            .put(4, 7); // 9 + 6 of 7
        w.put(4, 0)
            .put(1, 1)
            .put(1, 1)
            .put(2, 3)
            .put(8, 175)
            .put(4, 12); // 25 + 175 of 12
        w.put(4, 0).put(1, 0).put(3, 0); // the end
        assert_eq!(
            runs(four_bit, &w.bytes),
            [(1, 9), (5, 0), (6, 5), (1, 0), (2, 0), (15, 7), (200, 12)]
        );
    }

    #[test]
    fn every_eight_bit_form_decodes_to_its_runs() {
        let data = [7, 0, 5, 0, 0x80 | 10, 0x33, 0, 0, 9];
        assert_eq!(runs(eight_bit, &data), [(1, 7), (5, 0), (10, 0x33)]);
    }

    fn segment(out: &mut Vec<u8>, kind: u8, body: &[u8]) {
        out.extend([0x0f, kind, 0, 1]);
        out.extend((body.len() as u16).to_be_bytes());
        out.extend(body);
    }

    /// A page showing region 0 at (100, 400), `state` 0 (an update) or 2
    /// (a mode change), with `time_out` in seconds.
    fn page(out: &mut Vec<u8>, time_out: u8, state: u8) {
        segment(out, PAGE, &[time_out, state << 2, 0, 0xff, 0, 100, 1, 0x90]);
    }

    /// Region 0: 8 × 2, 4 bits a pixel, CLUT 0, filled with code 0, object
    /// 0 at (0, 0); CLUT 0 as below; object 0 a 2-bit
    /// line of four pixels of 1, then four of 0, in each field.
    fn display_set(out: &mut Vec<u8>) {
        segment(
            out,
            REGION,
            &[0, 0x08, 0, 8, 0, 2, 2 << 2, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        );
        // 4-bit entries, full range: 7 white (Y 235, T 0), 1 transparent
        // (Y 0), so a code 1 left unmapped would not show.
        segment(
            out,
            CLUT,
            &[0, 0, 7, 0x40 | 1, 235, 128, 128, 0, 1, 0x40 | 1, 0, 0, 0, 0],
        );
        let mut w = Writer::default();
        w.put(8, 0x10);
        // Four of 1 (3 + 1), four of 0 (3 + 1), then the end.
        w.put(2, 0).put(1, 1).put(3, 1).put(2, 1);
        w.put(2, 0).put(1, 1).put(3, 1).put(2, 0);
        w.put(2, 0).put(1, 0).put(1, 0).put(2, 0);
        while !w.bits.is_multiple_of(8) {
            w.put(1, 0);
        }
        w.put(8, 0xf0);
        let field = w.bytes;
        let mut object = vec![0, 0, 0];
        object.extend((field.len() as u16).to_be_bytes());
        object.extend(0u16.to_be_bytes());
        object.extend(&field);
        segment(out, OBJECT, &object);
    }

    fn packet(pts_ms: u64, data: Vec<u8>) -> Packet {
        Packet {
            pts_ms,
            duration_ms: None,
            data,
        }
    }

    fn shown(time_out: u8) -> Vec<u8> {
        let mut data = Vec::new();
        page(&mut data, time_out, 2);
        display_set(&mut data);
        segment(&mut data, END, &[]);
        data
    }

    /// Finding 45 in DVB: a page of one `side` × `side` region filled
    /// with an opaque colour, so all of it is text: 36 bytes a cue.
    fn filled(pts_ms: u64, side: u16) -> Packet {
        let mut data = Vec::new();
        // A mode change, region 0 at (0, 0), a time-out of 1 s.
        segment(&mut data, PAGE, &[1, 2 << 2, 0, 0xff, 0, 0, 0, 0]);
        // Region 0, filled with 2-bit code 1 (white), CLUT 0, no objects.
        let mut region = vec![0, 0x08];
        region.extend(side.to_be_bytes());
        region.extend(side.to_be_bytes());
        region.extend([1 << 2, 0, 0, 1 << 2]);
        segment(&mut data, REGION, &region);
        segment(&mut data, END, &[]);
        packet(pts_ms, data)
    }

    #[test]
    fn a_track_past_its_budget_of_pixels_is_refused() {
        let packets: Vec<Packet> = (0..8).map(|n| filled(n * 2_000, 1024)).collect();
        assert!(packets.iter().all(|p| p.data.len() == 36));
        let picture = 1056 * 1056;
        let budget = Budget {
            cues: 1_000,
            pixels: 3 * picture,
            held: 64 << 20,
        };
        match cues_within(&packets, &budget) {
            Err(e) => assert_eq!(e, OverBudget::Pixels(budget.pixels)),
            Ok(cues) => panic!(
                "{} bytes decoded to {} cues of {} pixels",
                packets.len() * 36,
                cues.len(),
                cues.iter().map(|c| c.image.pixels.len()).sum::<usize>()
            ),
        }
    }

    #[test]
    fn a_track_past_its_budget_of_cues_is_refused() {
        let packets: Vec<Packet> = (0..5).map(|n| filled(n * 2_000, 8)).collect();
        let budget = Budget {
            cues: 4,
            ..Budget::DEFAULT
        };
        assert_eq!(
            cues_within(&packets, &budget).map(|c| c.len()),
            Err(OverBudget::Cues(4))
        );
        assert_eq!(
            cues_within(&packets, &Budget::DEFAULT).map(|c| c.len()),
            Ok(5)
        );
    }

    /// Regions stay until the next epoch: many full-size ones under
    /// different ids are refused as they come, before each is allocated.
    #[test]
    fn regions_held_past_the_budget_are_refused() {
        let mut data = Vec::new();
        segment(&mut data, PAGE, &[1, 2 << 2]);
        for id in 0..=255u8 {
            let mut region = vec![id, 0];
            region.extend(4096u16.to_be_bytes());
            region.extend(4096u16.to_be_bytes());
            region.extend([1 << 2, 0, 0, 0]);
            segment(&mut data, REGION, &region);
        }
        let held = 3 * 4096 * 4096;
        let budget = Budget {
            held,
            ..Budget::DEFAULT
        };
        assert_eq!(
            cues_within(&[packet(0, data)], &budget).map(|c| c.len()),
            Err(OverBudget::Held(held))
        );
    }

    fn times(cues: &[Cue]) -> Vec<(u64, u64)> {
        cues.iter().map(|c| (c.start_ms, c.end_ms)).collect()
    }

    /// PX-09 B-3: a region's size is the region's pixels, allocated as
    /// told; past 4096 a side the region is refused, and nothing shows.
    #[test]
    fn a_region_wider_than_4096_is_refused() {
        let mut wide = shown(5);
        // The region's width, after the page (14 bytes) and its header.
        assert_eq!(&wide[22..24], &[0, 8]);
        wide[22..24].copy_from_slice(&4096u16.to_be_bytes());
        assert_eq!(cues(&[packet(1_000, wide.clone())]).len(), 1);
        wide[22..24].copy_from_slice(&4097u16.to_be_bytes());
        assert_eq!(cues(&[packet(1_000, wide)]), Vec::new());
    }

    /// Regions placed far apart would make one picture as wide as the gap.
    #[test]
    fn regions_placed_further_apart_than_4096_show_nothing() {
        let far = |x: u16| {
            let mut data = Vec::new();
            let mut body = vec![5, 2 << 2];
            body.extend([0, 0xff, 0, 0, 0, 0]);
            body.extend([0, 0xff]);
            body.extend(x.to_be_bytes());
            body.extend([0, 0]);
            segment(&mut data, PAGE, &body);
            display_set(&mut data);
            segment(&mut data, END, &[]);
            data
        };
        assert_eq!(cues(&[packet(1_000, far(3_000))]).len(), 1);
        assert_eq!(cues(&[packet(1_000, far(60_000))]), Vec::new());
    }

    #[test]
    fn a_two_bit_line_in_a_four_bit_region_is_coloured_through_the_default_map() {
        let cues = cues(&[packet(1_000, shown(5))]);
        // Code 1 maps to 7, which is white: four pixels wide, both lines.
        let image = &cues[0].image;
        assert_eq!((image.width, image.height), (4 + 32, 2 + 32));
        assert!(image.pixels[16 * image.width + 16] < 128);
    }

    #[test]
    fn a_page_ends_at_the_next_or_at_its_time_out() {
        let mut clear = Vec::new();
        page(&mut clear, 5, 0);
        // Region 0 is no longer on the page.
        clear.truncate(clear.len() - 6);
        clear[5] = 2;
        segment(&mut clear, END, &[]);
        let cues = cues(&[
            packet(1_000, shown(5)),
            packet(3_500, clear),
            packet(10_000, shown(2)),
        ]);
        assert_eq!(times(&cues), [(1_000, 3_500), (10_000, 12_000)]);
    }

    #[test]
    fn an_update_keeps_what_was_sent_and_a_new_epoch_drops_it() {
        // The same page again with nothing sent but the page itself.
        let again = |state: u8| {
            let mut data = Vec::new();
            page(&mut data, 5, state);
            segment(&mut data, END, &[]);
            data
        };
        let updated = cues(&[packet(0, shown(5)), packet(3_000, again(0))]);
        assert_eq!(times(&updated), [(0, 8_000)]);
        let restarted = cues(&[packet(0, shown(5)), packet(3_000, again(2))]);
        assert_eq!(times(&restarted), [(0, 3_000)]);
    }

    #[test]
    fn a_segment_running_past_its_packet_ends_the_packet_only() {
        let mut cut = shown(5);
        cut.truncate(cut.len() - 10);
        let cues = cues(&[packet(0, cut), packet(6_000, shown(1))]);
        assert_eq!(times(&cues), [(6_000, 7_000)]);
    }
}
