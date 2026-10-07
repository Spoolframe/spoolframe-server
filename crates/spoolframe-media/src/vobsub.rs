//! VobSub (DVD) subtitles: the cues of a track, each as a grey picture of
//! its text, for OCR.
//!
//! A track is a header and a run of packets ([`Track`]). The header (the
//! top of a `.idx` file, or an embedded track's codec private data) gives
//! the 16-colour palette. Each packet is one subtitle unit (SPU): a picture
//! of four colours, run-length coded a nibble at a time in two fields (even
//! lines, then odd), then control sequences. Each sequence has a date (in
//! 1024/90000 s from the packet's time) and commands: start or stop
//! showing, which palette entry each of the four colours is, how opaque
//! each is, where the picture sits and where its fields start.
//!
//! [`Track`]: crate::bitmap::Track

use crate::bitmap::{self, Budget, Colour, Cue, Kept, OverBudget, Packet};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VobSubError {
    #[error("the VobSub header has no palette")]
    NoPalette,
    #[error(transparent)]
    Budget(#[from] OverBudget),
}

/// One subtitle unit's control sequences, as read.
#[derive(Debug, Default, PartialEq, Eq)]
struct Unit {
    /// From the packet's time.
    start_ms: Option<u64>,
    stop_ms: Option<u64>,
    /// Palette entries of the four colours: background, pattern, then the
    /// two emphasis colours (in practice the outline and its edge).
    colours: [u8; 4],
    /// 0 transparent, 15 opaque.
    alpha: [u8; 4],
    width: usize,
    height: usize,
    /// Where the even and the odd lines start.
    fields: [usize; 2],
}

/// The cues of a track with `header` and `packets`, in order, within
/// [`Budget::DEFAULT`].
pub fn cues(header: &[u8], packets: &[Packet]) -> Result<Vec<Cue>, VobSubError> {
    cues_within(header, packets, &Budget::DEFAULT)
}

/// The cues of a track with `header` and `packets`, in order. A track
/// that decodes to more than `budget` is an error.
pub fn cues_within(
    header: &[u8],
    packets: &[Packet],
    budget: &Budget,
) -> Result<Vec<Cue>, VobSubError> {
    let lumas = palette(header)?;
    let mut out = Kept::new(budget);
    let mut unreadable = 0;
    for (i, packet) in packets.iter().enumerate() {
        let Some(unit) = unit(&packet.data) else {
            unreadable += 1;
            continue;
        };
        // A unit that only clears the screen.
        let Some(start) = unit.start_ms else {
            continue;
        };
        // A unit says when it stops. One that does not stops where its
        // container says, or at the next; the last of those is dropped, as
        // PGS's is.
        let end_ms = match unit.stop_ms {
            Some(stop) => Some(packet.pts_ms + stop),
            None => packet
                .duration_ms
                .map(|d| packet.pts_ms + d)
                .or_else(|| packets.get(i + 1).map(|p| p.pts_ms)),
        };
        let Some(end_ms) = end_ms else {
            continue;
        };
        let palette = [0, 1, 2, 3].map(|c| Colour {
            luma: lumas[usize::from(unit.colours[c] & 0x0f)],
            alpha: unit.alpha[c] * 17,
        });
        let pixels: Vec<Colour> = picture(&packet.data, &unit)
            .into_iter()
            .map(|i| palette[usize::from(i)])
            .collect();
        if let Some(image) = bitmap::text_picture(unit.width, unit.height, &pixels, &out)? {
            out.push(packet.pts_ms + start, end_ms, image)?;
        }
    }
    if unreadable > 0 {
        tracing::warn!(
            unreadable,
            "VobSub units that could not be read were left out"
        );
    }
    Ok(out.into_cues())
}

/// The palette's colours as luma: `palette: rrggbb, rrggbb, …` in the
/// header.
fn palette(header: &[u8]) -> Result<[u8; 16], VobSubError> {
    let text = String::from_utf8_lossy(header);
    let line = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("palette:"))
        .ok_or(VobSubError::NoPalette)?;
    let mut lumas = [0; 16];
    for (luma, hex) in lumas.iter_mut().zip(line.split(',')) {
        let rgb = u32::from_str_radix(hex.trim(), 16).map_err(|_| VobSubError::NoPalette)?;
        *luma = bitmap::luma((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
    }
    Ok(lumas)
}

fn be16(data: &[u8], at: usize) -> Option<usize> {
    Some(usize::from(u16::from_be_bytes([
        *data.get(at)?,
        *data.get(at + 1)?,
    ])))
}

/// The four nibbles of a colour or alpha command, background first.
fn nibbles(b: &[u8]) -> [u8; 4] {
    [b[1] & 0x0f, b[1] >> 4, b[0] & 0x0f, b[0] >> 4]
}

/// A unit's control sequences; `None` if they run past its end.
fn unit(data: &[u8]) -> Option<Unit> {
    let data = data.get(..be16(data, 0)?)?;
    let mut unit = Unit::default();
    let mut at = be16(data, 2)?;
    // The last sequence points at itself; a broken unit may loop.
    for _ in 0..16 {
        let date = be16(data, at)? as u64 * 1024 / 90;
        let next = be16(data, at + 2)?;
        let mut pos = at + 4;
        loop {
            let command = *data.get(pos)?;
            pos += 1;
            match command {
                // Forced start (shown even with subtitles off), or start.
                0x00 | 0x01 => unit.start_ms = Some(date),
                0x02 => unit.stop_ms = Some(date),
                0x03 => {
                    unit.colours = nibbles(data.get(pos..pos + 2)?);
                    pos += 2;
                }
                0x04 => {
                    unit.alpha = nibbles(data.get(pos..pos + 2)?);
                    pos += 2;
                }
                0x05 => {
                    let b = data.get(pos..pos + 6)?;
                    let n = |hi: u8, lo: u8, high_nibble: bool| {
                        if high_nibble {
                            (usize::from(hi) << 4) | usize::from(lo >> 4)
                        } else {
                            (usize::from(hi & 0x0f) << 8) | usize::from(lo)
                        }
                    };
                    let (x1, x2) = (n(b[0], b[1], true), n(b[1], b[2], false));
                    let (y1, y2) = (n(b[3], b[4], true), n(b[4], b[5], false));
                    unit.width = (x2 + 1).checked_sub(x1)?;
                    unit.height = (y2 + 1).checked_sub(y1)?;
                    pos += 6;
                }
                0x06 => {
                    unit.fields = [be16(data, pos)?, be16(data, pos + 2)?];
                    pos += 4;
                }
                // Colour and contrast changes within the picture: skipped,
                // by the size it starts with.
                0x07 => pos += be16(data, pos)?,
                // The end of the sequence, or a command we do not know,
                // whose length we cannot know either.
                _ => break,
            }
        }
        if next <= at {
            break;
        }
        at = next;
    }
    Some(unit)
}

/// The unit's picture: an index (0–3) into its four colours per pixel.
fn picture(data: &[u8], unit: &Unit) -> Vec<u8> {
    let (width, height) = (unit.width, unit.height);
    let mut pixels = vec![0; width * height];
    for (field, &offset) in unit.fields.iter().enumerate() {
        let mut nibbles = Nibbles {
            data,
            at: offset * 2,
        };
        'lines: for y in (field..height).step_by(2) {
            let line = &mut pixels[y * width..(y + 1) * width];
            let mut x = 0;
            while x < width {
                // A code is one to four nibbles: as many as it takes for the
                // value to pass 4, 16, 64, so a longer code is a longer run.
                let mut v = 0;
                let mut bound = 1;
                while v < bound && bound <= 0x40 {
                    let Some(n) = nibbles.next() else {
                        break 'lines;
                    };
                    v = (v << 4) | n;
                    bound <<= 2;
                }
                let colour = (v & 3) as u8;
                // A run of 0: to the end of the line.
                let run = match (v >> 2) as usize {
                    0 => width - x,
                    run => run.min(width - x),
                };
                line[x..x + run].fill(colour);
                x += run;
            }
            // Each line starts on a byte.
            nibbles.at += nibbles.at % 2;
        }
    }
    pixels
}

/// A field's data, a nibble at a time.
struct Nibbles<'a> {
    data: &'a [u8],
    at: usize,
}

impl Nibbles<'_> {
    fn next(&mut self) -> Option<u32> {
        let byte = self.data.get(self.at / 2)?;
        let n = if self.at.is_multiple_of(2) {
            byte >> 4
        } else {
            byte & 0x0f
        };
        self.at += 1;
        Some(u32::from(n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitmap::{Budget, OverBudget};

    /// The header ffmpeg writes: its default palette, where entry 7 is
    /// white and 0 black.
    const HEADER: &[u8] = b"size: 720x480\n\
        palette: 000000, 0000ff, 00ff00, ff0000, ffff00, ff00ff, 00ffff, ffffff, \
        808000, 8080ff, 800080, 80ff80, 008080, ff8080, 555555, aaaaaa\n";

    /// Nibbles packed into bytes.
    fn pack(nibbles: &[u8]) -> Vec<u8> {
        nibbles
            .chunks(2)
            .map(|p| (p[0] << 4) | p.get(1).copied().unwrap_or(0))
            .collect()
    }

    /// One line of `(run, colour)`, a run of 0 being the rest of the
    /// line, coded as short as each run allows.
    fn line(runs: &[(u16, u8)]) -> Vec<u8> {
        let mut nibbles = Vec::new();
        for &(run, colour) in runs {
            let v = (run << 2) | u16::from(colour);
            let count = match run {
                1..=3 => 1,
                4..=15 => 2,
                16..=63 => 3,
                _ => 4,
            };
            for i in (0..count).rev() {
                nibbles.push(((v >> (4 * i)) & 0x0f) as u8);
            }
        }
        if nibbles.len() % 2 == 1 {
            nibbles.push(0);
        }
        pack(&nibbles)
    }

    /// A unit of `width` × 2 pixels: line 0 (the even field) and line 1
    /// (the odd one), shown from `start` and stopped at `stop`, in
    /// control dates.
    fn unit_with(width: u16, even: &[u8], odd: &[u8], start: u16, stop: Option<u16>) -> Vec<u8> {
        unit_of(width, 2, even, odd, start, stop)
    }

    /// The same, `height` lines high: `even` and `odd` hold a field each.
    fn unit_of(
        width: u16,
        height: u16,
        even: &[u8],
        odd: &[u8],
        start: u16,
        stop: Option<u16>,
    ) -> Vec<u8> {
        let mut data = vec![0; 4];
        data.extend(even);
        data.extend(odd);
        let fields = [4u16, 4 + even.len() as u16];
        let control = data.len();
        let mut first = vec![];
        first.extend(start.to_be_bytes());
        let second = control + 4 + 3 + 3 + 7 + 5 + 1 + 1;
        first.extend((if stop.is_some() { second } else { control } as u16).to_be_bytes());
        // Background black and clear; pattern white, opaque; the rest
        // black, opaque.
        first.extend([0x03, 0x00, 0x70, 0x04, 0xff, 0xf0]);
        let x2 = width - 1;
        let y2 = height - 1;
        first.extend([
            0x05,
            0,
            (x2 >> 8) as u8,
            x2 as u8,
            0,
            (y2 >> 8) as u8,
            y2 as u8,
        ]);
        first.push(0x06);
        first.extend(fields[0].to_be_bytes());
        first.extend(fields[1].to_be_bytes());
        first.extend([0x01, 0xff]);
        data.extend(first);
        if let Some(stop) = stop {
            data.extend(stop.to_be_bytes());
            data.extend((second as u16).to_be_bytes());
            data.extend([0x02, 0xff]);
        }
        let size = data.len() as u16;
        data[..2].copy_from_slice(&size.to_be_bytes());
        data[2..4].copy_from_slice(&(control as u16).to_be_bytes());
        data
    }

    fn packet(pts_ms: u64, data: Vec<u8>) -> Packet {
        Packet {
            pts_ms,
            duration_ms: None,
            data,
        }
    }

    #[test]
    fn every_run_length_form_decodes_to_its_runs() {
        // Runs of one, two, three and four nibbles, then the rest of the
        // line; and the odd line all background by the same code.
        let even = line(&[(2, 1), (10, 2), (40, 3), (200, 1), (0, 2)]);
        let odd = line(&[(0, 0)]);
        let data = unit_with(300, &even, &odd, 0, Some(90));
        let unit = unit(&data).unwrap();
        assert_eq!((unit.width, unit.height), (300, 2));
        let pixels = picture(&data, &unit);
        let mut expected = Vec::new();
        for (run, colour) in [(2, 1), (10, 2), (40, 3), (200, 1), (48, 2), (300, 0)] {
            expected.extend(std::iter::repeat_n(colour, run));
        }
        assert_eq!(pixels, expected);
    }

    #[test]
    fn each_line_of_a_field_starts_on_a_byte() {
        // Five nibbles a line: the sixth is padding, not the next line.
        let even = [line(&[(1, 1), (0, 2)]), line(&[(2, 3), (0, 1)])].concat();
        let odd = [line(&[(0, 0)]), line(&[(0, 3)])].concat();
        let data = unit_of(6, 4, &even, &odd, 0, Some(90));
        let pixels = picture(&data, &unit(&data).unwrap());
        assert_eq!(
            pixels,
            [
                [1, 2, 2, 2, 2, 2],
                [0, 0, 0, 0, 0, 0],
                [3, 3, 1, 1, 1, 1],
                [3, 3, 3, 3, 3, 3]
            ]
            .concat()
        );
    }

    #[test]
    fn a_unit_shows_from_its_start_date_to_its_stop_date() {
        // Dates in 1024/90000 s: 45 is 512 ms.
        let data = unit_with(8, &line(&[(0, 1)]), &line(&[(0, 1)]), 45, Some(225));
        let unit = unit(&data).unwrap();
        assert_eq!((unit.start_ms, unit.stop_ms), (Some(512), Some(2_560)));
        assert_eq!(unit.colours, [0, 7, 0, 0]);
        assert_eq!(unit.alpha, [0, 15, 15, 15]);
        let cues = cues(HEADER, &[packet(1_000, data)]).unwrap();
        let times: Vec<_> = cues.iter().map(|c| (c.start_ms, c.end_ms)).collect();
        assert_eq!(times, [(1_512, 3_560)]);
    }

    #[test]
    fn a_unit_without_a_stop_ends_at_the_next_and_the_last_is_dropped() {
        let shown = || unit_with(8, &line(&[(0, 1)]), &line(&[(0, 1)]), 0, None);
        let other = unit_with(8, &line(&[(4, 1), (0, 0)]), &line(&[(0, 1)]), 0, None);
        let cues = cues(
            HEADER,
            &[
                packet(1_000, shown()),
                packet(4_000, other),
                packet(6_000, shown()),
            ],
        )
        .unwrap();
        let times: Vec<_> = cues.iter().map(|c| (c.start_ms, c.end_ms)).collect();
        assert_eq!(times, [(1_000, 4_000), (4_000, 6_000)]);
    }

    #[test]
    fn the_white_pattern_is_the_ink_and_the_clear_background_is_not() {
        // Background, then pattern (white), then emphasis 1 (black, opaque).
        let even = line(&[(3, 0), (2, 1), (3, 2)]);
        let data = unit_with(8, &even, &line(&[(0, 0)]), 0, Some(90));
        let cues = cues(HEADER, &[packet(0, data)]).unwrap();
        let image = &cues[0].image;
        // Cut to the two white pixels and the black ones after, which do
        // not show over black: a 16-pixel margin around 2 × 1.
        assert_eq!((image.width, image.height), (34, 33));
        let row = &image.pixels[16 * 34..17 * 34];
        assert!(row[16] < 128 && row[17] < 128 && row[15] == 255 && row[18] == 255);
    }

    #[test]
    fn a_unit_cut_short_is_left_out_and_the_rest_are_read() {
        let whole = unit_with(8, &line(&[(0, 1)]), &line(&[(0, 1)]), 0, Some(90));
        let mut cut = whole.clone();
        cut.truncate(cut.len() - 6);
        let cues = cues(HEADER, &[packet(0, cut), packet(5_000, whole)]).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ms, 5_000);
    }

    /// Finding 45 in VobSub: a unit whose every line is one code, "the
    /// rest of the line in white", is two bytes a line of text.
    #[test]
    fn a_track_past_its_budget_of_pixels_is_refused() {
        let field = line(&[(0, 1)]).repeat(256);
        let units: Vec<Packet> = (0..8)
            .map(|n| packet(n * 2_000, unit_of(512, 512, &field, &field, 0, Some(45))))
            .collect();
        let picture = 544 * 544;
        let budget = Budget {
            pixels: 3 * picture,
            ..Budget::DEFAULT
        };
        match cues_within(HEADER, &units, &budget) {
            Err(e) => assert_eq!(e, VobSubError::Budget(OverBudget::Pixels(budget.pixels))),
            Ok(cues) => panic!(
                "{} bytes decoded to {} cues of {} pixels",
                units.iter().map(|u| u.data.len()).sum::<usize>(),
                cues.len(),
                cues.iter().map(|c| c.image.pixels.len()).sum::<usize>()
            ),
        }
    }

    #[test]
    fn a_header_without_a_palette_is_refused() {
        assert_eq!(cues(b"size: 720x480\n", &[]), Err(VobSubError::NoPalette));
    }
}
