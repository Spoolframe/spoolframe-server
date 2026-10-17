//! A text subtitle's cue times, read and written in place (PX-03). The
//! file is someone else's (a sidecar beside the film, a download), so it
//! is read as bytes: only the timestamps, which are ASCII, are parsed and
//! rewritten, and everything else (the text in whatever encoding it is,
//! ASS styling, WebVTT settings) is copied through untouched.

use std::ops::Range;

use super::SyncError;

/// The largest subtitle file read: a film's SRT is some 100 KB, an ASS
/// with heavy styling a few MB.
pub const MAX_TEXT_BYTES: usize = 16 << 20;

/// The most cues taken from one file: a film has one or two thousand.
pub const MAX_CUES: usize = 100_000;

/// The latest time a cue may have, ms: 48 hours, past any film.
const MAX_MS: i64 = 48 * 60 * 60 * 1000;

/// A text subtitle format whose times are read and rewritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextFormat {
    /// SubRip: `00:01:02,500 --> 00:01:04,000`.
    Srt,
    /// WebVTT: `00:01:02.500 --> 00:01:04.000`, the hours optional.
    WebVtt,
    /// ASS and SSA: `Dialogue:` lines, their `Start` and `End` fields
    /// `0:01:02.50`, in centiseconds.
    Ass,
}

impl TextFormat {
    /// The format of a track ffprobe names `codec`, if it is one of these.
    pub fn of_codec(codec: &str) -> Option<Self> {
        match codec {
            "subrip" | "srt" => Some(Self::Srt),
            "webvtt" => Some(Self::WebVtt),
            "ass" | "ssa" => Some(Self::Ass),
            _ => None,
        }
    }
}

/// A cue's times, and where they are in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    pub start_ms: i64,
    pub end_ms: i64,
    start_at: Range<usize>,
    end_at: Range<usize>,
}

/// The cues of `bytes`, a subtitle in `format`, in the file's order. A
/// cue whose times cannot be read is skipped, as a player skips it; a file
/// with none is an error, as is one past [`MAX_TEXT_BYTES`] or with more
/// than [`MAX_CUES`].
pub fn cues(bytes: &[u8], format: TextFormat) -> Result<Vec<Cue>, SyncError> {
    if bytes.len() > MAX_TEXT_BYTES {
        return Err(SyncError::Unreadable(format!(
            "the subtitle is larger than {MAX_TEXT_BYTES} bytes"
        )));
    }
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        return Err(SyncError::Unreadable(
            "the subtitle is UTF-16, whose times are not read".into(),
        ));
    }
    let cues = match format {
        TextFormat::Srt | TextFormat::WebVtt => arrow_cues(bytes)?,
        TextFormat::Ass => ass_cues(bytes)?,
    };
    if cues.is_empty() {
        return Err(SyncError::Unreadable("the subtitle has no cues".into()));
    }
    Ok(cues)
}

/// The lines of `bytes`, each with where it starts; without its `\n`.
fn lines(bytes: &[u8]) -> impl Iterator<Item = (usize, &[u8])> {
    let mut at = 0;
    bytes.split(|&b| b == b'\n').map(move |line| {
        let start = at;
        at += line.len() + 1;
        (start, line)
    })
}

fn too_many() -> SyncError {
    SyncError::Unreadable(format!("the subtitle has more than {MAX_CUES} cues"))
}

/// SRT and WebVTT: every line with `-->` between two timestamps.
fn arrow_cues(bytes: &[u8]) -> Result<Vec<Cue>, SyncError> {
    let mut cues = Vec::new();
    for (at, line) in lines(bytes) {
        let Some(arrow) = find(line, b"-->") else {
            continue;
        };
        let before = &line[..arrow];
        let after = &line[arrow + 3..];
        let start_at = trimmed(before);
        let Some(start_ms) = arrow_time(&before[start_at.clone()]) else {
            continue;
        };
        // The end is the first word after the arrow: WebVTT's settings
        // and SRT's coordinates follow it.
        let skip = after.iter().take_while(|b| b.is_ascii_whitespace()).count();
        let word = after[skip..]
            .iter()
            .take_while(|b| !b.is_ascii_whitespace())
            .count();
        let end_at = skip..skip + word;
        let Some(end_ms) = arrow_time(&after[end_at.clone()]) else {
            continue;
        };
        if cues.len() == MAX_CUES {
            return Err(too_many());
        }
        let end_base = at + arrow + 3;
        cues.push(Cue {
            start_ms,
            end_ms,
            start_at: at + start_at.start..at + start_at.end,
            end_at: end_base + end_at.start..end_base + end_at.end,
        });
    }
    Ok(cues)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// `bytes` without leading and trailing whitespace, as a range of it.
fn trimmed(bytes: &[u8]) -> Range<usize> {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |p| p + 1);
    start..end
}

/// `[h:]mm:ss[,.]mmm` in ms. The fraction may be 1 to 3 digits, as files
/// in the wild have it, and is read as a decimal fraction.
fn arrow_time(text: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(text).ok()?;
    let (clock, fraction) = text.split_once([',', '.'])?;
    let parts: Vec<&str> = clock.split(':').collect();
    let (h, m, s) = match parts.as_slice() {
        [h, m, s] => (number(h, 1, 3)?, number(m, 1, 2)?, number(s, 1, 2)?),
        [m, s] => (0, number(m, 1, 2)?, number(s, 1, 2)?),
        _ => return None,
    };
    let ms = match fraction.len() {
        1 => number(fraction, 1, 1)? * 100,
        2 => number(fraction, 2, 2)? * 10,
        3 => number(fraction, 3, 3)?,
        _ => return None,
    };
    clock_ms(h, m, s, ms)
}

/// `text` as a number of `min` to `max` ASCII digits.
fn number(text: &str, min: usize, max: usize) -> Option<i64> {
    if !(min..=max).contains(&text.len()) || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn clock_ms(h: i64, m: i64, s: i64, ms: i64) -> Option<i64> {
    if m >= 60 || s >= 60 {
        return None;
    }
    let total = ((h * 60 + m) * 60 + s) * 1000 + ms;
    (total <= MAX_MS).then_some(total)
}

/// ASS and SSA: the `Dialogue:` lines of `[Events]`, their times in the
/// fields its `Format:` line names `Start` and `End` (the second and third
/// when it names none).
fn ass_cues(bytes: &[u8]) -> Result<Vec<Cue>, SyncError> {
    let mut cues = Vec::new();
    let (mut start_field, mut end_field) = (1, 2);
    let mut in_events = false;
    for (at, line) in lines(bytes) {
        let trimmed_line = &line[trimmed(line)];
        if trimmed_line.starts_with(b"[") {
            in_events = trimmed_line.eq_ignore_ascii_case(b"[events]");
            continue;
        }
        if !in_events {
            continue;
        }
        if let Some(format) = strip_key(trimmed_line, b"format:") {
            let names: Vec<String> = format
                .split(|&b| b == b',')
                .map(|f| String::from_utf8_lossy(&f[trimmed(f)]).to_ascii_lowercase())
                .collect();
            if let (Some(s), Some(e)) = (
                names.iter().position(|n| n == "start"),
                names.iter().position(|n| n == "end"),
            ) {
                (start_field, end_field) = (s, e);
            }
            continue;
        }
        // Where the line's fields start: just after `Dialogue:`.
        let lead = trimmed(line).start;
        if !line[lead..]
            .get(..9)
            .is_some_and(|k| k.eq_ignore_ascii_case(b"dialogue:"))
        {
            continue;
        }
        let fields_at = lead + 9;
        // The text, the last field, may hold commas: only the fields
        // before it are split, and the times are never last.
        let mut ranges = Vec::new();
        let mut from = fields_at;
        for (i, &b) in line[fields_at..].iter().enumerate() {
            if b == b',' {
                ranges.push(from..fields_at + i);
                from = fields_at + i + 1;
                if ranges.len() > start_field.max(end_field) {
                    break;
                }
            }
        }
        let (Some(start), Some(end)) = (ranges.get(start_field), ranges.get(end_field)) else {
            continue;
        };
        let narrow = |r: &Range<usize>| {
            let t = trimmed(&line[r.clone()]);
            r.start + t.start..r.start + t.end
        };
        let (start, end) = (narrow(start), narrow(end));
        let (Some(start_ms), Some(end_ms)) =
            (ass_time(&line[start.clone()]), ass_time(&line[end.clone()]))
        else {
            continue;
        };
        if cues.len() == MAX_CUES {
            return Err(too_many());
        }
        cues.push(Cue {
            start_ms,
            end_ms,
            start_at: at + start.start..at + start.end,
            end_at: at + end.start..at + end.end,
        });
    }
    Ok(cues)
}

fn strip_key<'a>(line: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    line.get(..key.len())
        .filter(|k| k.eq_ignore_ascii_case(key))
        .map(|_| &line[key.len()..])
}

/// `h:mm:ss.cc` in ms.
fn ass_time(text: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(text).ok()?;
    let (clock, fraction) = text.split_once('.')?;
    let parts: Vec<&str> = clock.split(':').collect();
    let [h, m, s] = parts.as_slice() else {
        return None;
    };
    let ms = match fraction.len() {
        1 => number(fraction, 1, 1)? * 100,
        2 => number(fraction, 2, 2)? * 10,
        3 => number(fraction, 3, 3)?,
        _ => return None,
    };
    clock_ms(number(h, 1, 2)?, number(m, 1, 2)?, number(s, 1, 2)?, ms)
}

/// How each cue's times move: `t * factor + delta`, the delta its own.
#[derive(Debug, Clone, PartialEq)]
pub struct Retiming {
    pub factor: f64,
    /// One for each cue, in the file's order; ms.
    pub deltas: Vec<i64>,
}

impl Retiming {
    /// Every cue of `count` moved by `delta_ms`.
    pub fn shift(count: usize, delta_ms: i64) -> Self {
        Self {
            factor: 1.0,
            deltas: vec![delta_ms; count],
        }
    }

    /// Where time `ms` of cue `i` goes; never before 0.
    pub fn apply(&self, i: usize, ms: i64) -> i64 {
        let delta = self.deltas.get(i).copied().unwrap_or(0);
        ((ms as f64 * self.factor).round() as i64 + delta).clamp(0, MAX_MS)
    }
}

/// `bytes`, whose cues are `cues`, with their times moved by `retiming`:
/// only the timestamps change, written as `format` writes them.
pub fn retime(bytes: &[u8], format: TextFormat, cues: &[Cue], retiming: &Retiming) -> Vec<u8> {
    let mut edits: Vec<(Range<usize>, String)> = Vec::with_capacity(cues.len() * 2);
    for (i, cue) in cues.iter().enumerate() {
        let start = retiming.apply(i, cue.start_ms);
        // An end before its start would hide the cue.
        let end = retiming.apply(i, cue.end_ms).max(start);
        edits.push((cue.start_at.clone(), write_time(format, start)));
        edits.push((cue.end_at.clone(), write_time(format, end)));
    }
    edits.sort_by_key(|(at, _)| at.start);
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 8);
    let mut copied = 0;
    for (at, text) in edits {
        out.extend_from_slice(&bytes[copied..at.start]);
        out.extend_from_slice(text.as_bytes());
        copied = at.end;
    }
    out.extend_from_slice(&bytes[copied..]);
    out
}

/// `bytes`, a subtitle in `format`, with every cue moved by `delta_ms`.
pub fn shift(bytes: &[u8], format: TextFormat, delta_ms: i64) -> Result<Vec<u8>, SyncError> {
    let cues = cues(bytes, format)?;
    Ok(retime(
        bytes,
        format,
        &cues,
        &Retiming::shift(cues.len(), delta_ms),
    ))
}

fn write_time(format: TextFormat, ms: i64) -> String {
    let (h, m, s, frac) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    match format {
        TextFormat::Srt => format!("{h:02}:{m:02}:{s:02},{frac:03}"),
        TextFormat::WebVtt => format!("{h:02}:{m:02}:{s:02}.{frac:03}"),
        TextFormat::Ass => format!("{h}:{m:02}:{s:02}.{:02}", frac / 10),
    }
}
