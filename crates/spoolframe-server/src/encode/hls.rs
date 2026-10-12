//! HLS for a converted playback (P2-05): where its segments are cut, and
//! the playlists that list them.
//!
//! The media playlist lists the whole file from the start (VOD), before
//! most of it is encoded, so a player knows the length and can seek. The
//! cuts are therefore decided here, and the encode is made to cut at them,
//! across restarts too:
//!
//! - An encoded video gets a keyframe forced at every cut, so segments are
//!   a grid: 2 s, 2 s and 4 s first (PX-26: the player has a segment to
//!   play two seconds into the encode, not six), then 6 s. ffmpeg's HLS
//!   muxer cuts at the first keyframe from `start + 6 s × n` on; for a run
//!   from 0, `hls_init_time` 2 s with a one-entry list makes that 2 s, 4 s,
//!   then 8 s and every 6 s on ([`Segments::encoded`]). The encoder adds
//!   no keyframe of its own (no scene cuts, a long GOP): the muxer cuts at
//!   the first keyframe past each threshold, and a restart's thresholds
//!   can come early, so one of the encoder's (VA-API's every 120 frames)
//!   would be cut at instead. A restart begins on the grid past the head,
//!   at `start + 6 s × n`, and stays on it; one in the head starts from 0.
//!   A copied audio track begins where the seek lands, at the source's
//!   keyframe before it, and is cut back to the start (`args`): the muxer
//!   measures from the first packet it gets.
//! - A copied video can only be cut at the source's keyframes. ffmpeg
//!   counts its 6 s from where a run starts, so a restart at a keyframe off
//!   the grid would cut elsewhere than a run from 0. Instead every keyframe
//!   is a cut: the segments are the source's GOPs (1 to 10 s in practice),
//!   the same whatever a run started from. Only the first keyframe is not
//!   one: ffmpeg never cuts at the first keyframe of a run, so the first
//!   segment runs from 0 to the second keyframe.
//! - Audio alone cuts anywhere: a 6 s grid.

use std::fmt::Write as _;

use spoolframe_media::{MAX_DURATION, MediaDescriptor, SubtitleStream, VideoRange};
use spoolframe_protocol::{AudioAction, ConvertedAudio, ConvertedVideo};
use spoolframe_types::{AudioCodec, VideoCodec, language};

use super::args::{FIRST_SEGMENT_SECS, SEGMENT_SECS};

/// The shortest last segment kept on its own.
const TAIL_MS: u64 = 500;

/// The segments of an encoded video that only a run from 0 cuts where
/// [`Segments::encoded`] says: 0–2 s, 2–4 s and 4–8 s.
const ENCODED_HEAD: u32 = 3;

/// The segments of a converted playback: segment `n` (from 1) starts at
/// `starts_ms[n - 1]` and ends where the next starts, or at `total_ms`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segments {
    pub starts_ms: Vec<u64>,
    pub total_ms: u64,
    /// Segments 1 to `head` are written only by a run from 0: a run for
    /// any of them starts at segment 1 ([`Segments::run_from`]).
    head: u32,
}

/// The length an HLS grid is cut over, in ms, from ffprobe's length of
/// the file: refused past [`MAX_DURATION`], which would make a grid (and
/// a playlist) as long as the file says.
pub fn hls_total_ms(duration: Option<std::time::Duration>) -> Result<u64, String> {
    match duration {
        Some(d) if d > MAX_DURATION => Err(format!(
            "the file says it is {} hours long; HLS is made only of files up to {} hours",
            d.as_secs() / 3600,
            MAX_DURATION.as_secs() / 3600
        )),
        Some(d) => Ok(d.as_millis() as u64),
        None => Ok(0),
    }
}

impl Segments {
    /// A grid of [`SEGMENT_SECS`]: audio alone.
    pub fn grid(total_ms: u64) -> Self {
        Self::tidy(Self::cuts(total_ms, 0, 0), total_ms, 0)
    }

    /// An encoded video's grid (PX-26): [`FIRST_SEGMENT_SECS`] twice, then
    /// what is left to [`FIRST_SEGMENT_SECS`] + [`SEGMENT_SECS`], then
    /// [`SEGMENT_SECS`]: 0, 2, 4, 8, 14, 20 s… These are the thresholds
    /// ffmpeg's HLS muxer cuts at with `hls_init_time` and a one-entry list
    /// (see `args`), and the times keyframes are forced at.
    pub fn encoded(total_ms: u64) -> Self {
        let first = u64::from(FIRST_SEGMENT_SECS) * 1000;
        let head = [0, first, 2 * first];
        let mut starts_ms: Vec<u64> = head
            .into_iter()
            .filter(|&t| t == 0 || t < total_ms)
            .collect();
        if starts_ms.len() == head.len() {
            starts_ms.extend(Self::cuts(total_ms, first, 1));
        }
        Self::tidy(starts_ms, total_ms, ENCODED_HEAD)
    }

    /// Every [`SEGMENT_SECS`] from `from_ms` up to `total_ms`, the first
    /// `skip` of them left out; 0 alone for an empty file.
    fn cuts(total_ms: u64, from_ms: u64, skip: usize) -> Vec<u64> {
        let step = u64::from(SEGMENT_SECS) * 1000;
        let count = total_ms.saturating_sub(from_ms).div_ceil(step).max(1);
        (0..count).map(|i| from_ms + i * step).skip(skip).collect()
    }

    /// A cut at every keyframe but the first: the first segment starts at 0
    /// whatever the first keyframe's time, and holds the first GOP whole,
    /// as ffmpeg writes it.
    pub fn at_keyframes(keyframes: &[u64], total_ms: u64) -> Self {
        let mut starts_ms = vec![0];
        let later = keyframes.iter().skip(1).copied();
        starts_ms.extend(later.filter(|&t| t > 0 && t < total_ms));
        Self::tidy(starts_ms, total_ms, 0)
    }

    /// A last segment shorter than [`TAIL_MS`] is folded into the one
    /// before: the file's duration often runs a few milliseconds past its
    /// last video frame (the audio), and ffmpeg does not cut there.
    fn tidy(mut starts_ms: Vec<u64>, total_ms: u64, head: u32) -> Self {
        if starts_ms.len() > 1
            && let Some(&last) = starts_ms.last()
            && total_ms.saturating_sub(last) < TAIL_MS
        {
            starts_ms.pop();
        }
        Self {
            starts_ms,
            total_ms,
            head,
        }
    }

    /// The segment a run that is to write segment `n` starts at: `n`
    /// itself, or 1 for a segment of the head, which a run started
    /// anywhere else cuts elsewhere.
    pub fn run_from(&self, n: u32) -> u32 {
        if n <= self.head { 1 } else { n }
    }

    pub fn count(&self) -> u32 {
        self.starts_ms.len() as u32
    }

    /// Where segment `n` (from 1) starts.
    pub fn start(&self, n: u32) -> Option<u64> {
        self.starts_ms.get(n.checked_sub(1)? as usize).copied()
    }

    /// Where segment `n` (from 1) ends.
    pub fn end(&self, n: u32) -> Option<u64> {
        self.start(n)?;
        Some(self.start(n + 1).unwrap_or(self.total_ms))
    }

    fn duration_ms(&self, i: usize) -> u64 {
        let end = self.starts_ms.get(i + 1).copied().unwrap_or(self.total_ms);
        end.saturating_sub(self.starts_ms[i])
    }

    /// The subtitle rendition's playlist (P2-07): WebVTT segments on the
    /// video's own cuts, so a cue is in the segment its picture is in.
    /// URIs (`subs3.vtt`) map to `ReadSubtitleSegment { n: 3 }`.
    pub fn subtitle_playlist(&self) -> String {
        self.playlist(None, "subs", "vtt")
    }

    /// The VOD media playlist. Segment URIs are relative names
    /// (`seg3.m4s`) that a client maps to a segment read.
    pub fn media_playlist(&self) -> String {
        self.playlist(Some("init.mp4"), "seg", "m4s")
    }

    fn playlist(&self, init: Option<&str>, name: &str, extension: &str) -> String {
        let longest = (0..self.starts_ms.len())
            .map(|i| self.duration_ms(i))
            .max()
            .unwrap_or(0);
        let mut text = String::from("#EXTM3U\n#EXT-X-VERSION:7\n");
        let _ = writeln!(
            text,
            "#EXT-X-TARGETDURATION:{}",
            longest.div_ceil(1000).max(1)
        );
        text.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n");
        if let Some(init) = init {
            let _ = writeln!(text, "#EXT-X-MAP:URI=\"{init}\"");
        }
        for i in 0..self.starts_ms.len() {
            let _ = writeln!(
                text,
                "#EXTINF:{:.3},\n{name}{}.{extension}",
                self.duration_ms(i) as f64 / 1000.0,
                i + 1
            );
        }
        text.push_str("#EXT-X-ENDLIST\n");
        text
    }
}

/// The most a copied stream averaging `average` bit/s carries over a
/// segment: half again over it, as variable rates run. What a variant
/// declares as its `BANDWIDTH`, and what a copy off the LAN must fit
/// under its cap (PX-46).
pub(crate) fn copy_peak(average: u64) -> u64 {
    average.saturating_mul(3) / 2
}

/// What a variant declares, in bit/s: `BANDWIDTH`, the peak, is what
/// AVPlayer holds every segment to, and it refuses one over it
/// (CoreMediaErrorDomain -12318); `AVERAGE-BANDWIDTH` is the mean. The
/// playlist is written before the encode runs, so no segment's size is
/// known: the peak is bounded from how each stream is made.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Rate {
    peak: u64,
    average: u64,
}

impl Rate {
    /// A copied stream, or an encode with no cap: its average, and a
    /// segment up to half again over it, as variable rates run.
    fn varying(average: u64) -> Self {
        Self {
            peak: copy_peak(average),
            average,
        }
    }

    /// A video encode capped at `cap` (`-maxrate cap -bufsize 2×cap`): a
    /// segment holds at most its time at the cap plus the full buffer, and
    /// the shortest, the first ([`FIRST_SEGMENT_SECS`], PX-26), has the
    /// most over its time: twice the cap at 2 s.
    fn capped(cap: u64) -> Self {
        Self {
            peak: cap.saturating_add(cap.saturating_mul(2) / u64::from(FIRST_SEGMENT_SECS)),
            average: cap,
        }
    }

    /// An audio encode at `bitrate`, which ffmpeg's encoders keep near
    /// it: a tenth more for what it varies by and the container.
    fn encoded(bitrate: u64) -> Self {
        Self {
            peak: bitrate.saturating_add(bitrate / 10),
            average: bitrate,
        }
    }

    fn plus(self, other: Self) -> Self {
        Self {
            peak: self.peak.saturating_add(other.peak),
            average: self.average.saturating_add(other.average),
        }
    }
}

/// The multivariant playlist: the one variant, described for the player,
/// with `subtitle` (the picked track) as its subtitle rendition. `size`
/// is the picture the encode writes (`args::written_size`), where known.
pub fn multivariant_playlist(
    descriptor: &MediaDescriptor,
    video: Option<ConvertedVideo>,
    size: Option<(u32, u32)>,
    audio: Option<ConvertedAudio>,
    subtitle: Option<&SubtitleStream>,
) -> String {
    let mut text = header(subtitle);
    text.push_str(&stream_inf(
        descriptor,
        video,
        size,
        audio,
        subtitle.is_some(),
    ));
    text.push_str("\nmedia.m3u8\n");
    text
}

/// A variant of a ladder's playlist: its rung, its video, and the
/// picture its encode writes, where known.
pub type LadderVariant = (u32, ConvertedVideo, Option<(u32, u32)>);

/// The multivariant playlist of a ladder (PX-21): one variant per rung,
/// `rungs` as `(n, video)` in the order the player is to see them (it
/// starts on the first), each at `v{n}/media.m3u8`, all with the same
/// audio and subtitle rendition: every rung is on the same grid. Each
/// rung's size is the picture its encode writes, where known.
pub fn ladder_playlist(
    descriptor: &MediaDescriptor,
    rungs: &[LadderVariant],
    audio: Option<ConvertedAudio>,
    subtitle: Option<&SubtitleStream>,
) -> String {
    let mut text = header(subtitle);
    for (n, video, size) in rungs {
        text.push_str(&stream_inf(
            descriptor,
            Some(*video),
            *size,
            audio,
            subtitle.is_some(),
        ));
        let _ = write!(text, "\nv{n}/media.m3u8\n");
    }
    text
}

/// The playlist's head, with `subtitle` as the rendition its variants
/// share.
fn header(subtitle: Option<&SubtitleStream>) -> String {
    let mut text = String::from("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n");
    if let Some(track) = subtitle {
        let name = track
            .title
            .as_deref()
            .or(track.language.as_deref())
            .map(quoted_string)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "Subtitles".into());
        let _ = write!(
            text,
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"{name}\",DEFAULT=YES,\
             AUTOSELECT=YES,FORCED={},URI=\"subtitles.m3u8\"",
            if track.forced { "YES" } else { "NO" }
        );
        if let Some(language) = track.language.as_deref().and_then(language_tag) {
            let _ = write!(text, ",LANGUAGE=\"{language}\"");
        }
        text.push('\n');
    }
    text
}

/// One variant's `#EXT-X-STREAM-INF` line, without its URI. `size` is
/// the picture written; without it, the source's scaled to the plan's
/// height as `scale=-2` scales it.
fn stream_inf(
    descriptor: &MediaDescriptor,
    video: Option<ConvertedVideo>,
    size: Option<(u32, u32)>,
    audio: Option<ConvertedAudio>,
    subtitles: bool,
) -> String {
    let mut codecs = Vec::new();
    let mut supplemental = None;
    let mut resolution = None;
    let mut picture = None;
    let mut rate = Rate::default();
    if let (Some(plan), Some(source)) = (video, &descriptor.video) {
        // A copied stream is the source's; an encode's level is not known
        // before it runs.
        // What is written: a copy is the source; an encode keeps its depth
        // only in HEVC or AV1 not tone mapped (PGPU-06), H.264 and a tone
        // map being 8-bit.
        let (codec, height, cap, level, depth) = match plan {
            ConvertedVideo::Copy { codec, .. } => {
                (codec, source.height, None, source.level, source.bit_depth)
            }
            ConvertedVideo::Encode {
                codec,
                height,
                max_bitrate,
                tone_map,
            } => {
                let depth = if tone_map || codec == VideoCodec::H264 {
                    8
                } else {
                    source.bit_depth
                };
                (codec, height, max_bitrate, None, depth)
            }
            ConvertedVideo::Unknown => (VideoCodec::Unknown, source.height, None, None, 8),
        };
        let fast = source.frame_rate_millis.is_some_and(|rate| rate > 30_000);
        // A Dolby Vision copy the client shows as such (PGPU-06): profile
        // 5 is `dvh1` itself; profile 8 is HEVC with Dolby Vision beside,
        // its base layer's brand saying what it is without it.
        let dolby_vision = match plan {
            ConvertedVideo::Copy {
                dolby_vision: true, ..
            } => source.dolby_vision,
            _ => None,
        };
        match dolby_vision {
            Some(dv) if dv.profile == 5 => codecs.push(format!("dvh1.05.{:02}", dv.level)),
            Some(dv) => {
                codecs.push(video_codec_string(codec, depth, height, level, fast));
                let brand = match dv.compatibility {
                    4 => "db4h",
                    2 => "db2g",
                    _ => "db1p",
                };
                supplemental = Some(format!("dvh1.{:02}.{:02}/{brand}", dv.profile, dv.level));
            }
            None => codecs.push(video_codec_string(codec, depth, height, level, fast)),
        }
        resolution = Some(size.unwrap_or_else(|| {
            let width = (u64::from(source.width) * u64::from(height)
                / u64::from(source.height.max(1))) as u32;
            (width & !1, height)
        }));
        // What the output is: a tone-mapped encode is SDR, anything else
        // keeps the source's transfer. No conversion changes the rate.
        let range = match plan {
            ConvertedVideo::Encode { tone_map: true, .. } => VideoRange::Sdr,
            // Dolby Vision profile 5, labelled or made HDR10: PQ, whatever
            // its own transfer says.
            _ if source.needs_rpu() => VideoRange::Pq,
            _ => source.range(),
        };
        picture = Some((source.frame_rate_millis, range));
        rate = rate.plus(match cap {
            Some(cap) => Rate::capped(cap),
            // A copy is the source's own; an uncapped encode's rate is
            // not known before it runs, and the source's is a fair guess.
            None => Rate::varying(descriptor.bit_rate.unwrap_or(8_000_000)),
        });
    }
    if let Some(audio) = audio {
        let (codec, audio_rate) = match audio.action {
            // Alone, the file is the sound (a music track): its rate is
            // the sound's. Beside a picture it is not known.
            AudioAction::Copy { codec } => {
                let alone = descriptor.bit_rate.filter(|_| video.is_none());
                (codec, Rate::varying(alone.unwrap_or(640_000)))
            }
            AudioAction::Encode { codec, bitrate, .. } => (codec, Rate::encoded(bitrate)),
            AudioAction::Unknown => (AudioCodec::Unknown, Rate::default()),
        };
        if let Some(name) = audio_codec_string(codec) {
            codecs.push(name.to_owned());
        }
        rate = rate.plus(audio_rate);
    }
    let mut text = format!(
        "#EXT-X-STREAM-INF:BANDWIDTH={},AVERAGE-BANDWIDTH={}",
        rate.peak, rate.average
    );
    // An empty CODECS is not valid HLS; none is.
    if !codecs.is_empty() {
        let _ = write!(text, ",CODECS=\"{}\"", codecs.join(","));
    }
    if let Some(supplemental) = supplemental {
        let _ = write!(text, ",SUPPLEMENTAL-CODECS=\"{supplemental}\"");
    }
    if let Some((w, h)) = resolution {
        let _ = write!(text, ",RESOLUTION={w}x{h}");
    }
    if let Some((rate, range)) = picture {
        if let Some(rate) = rate {
            let _ = write!(text, ",FRAME-RATE={:.3}", f64::from(rate) / 1000.0);
        }
        // Apple's players want it said for HDR, and read none as SDR.
        let _ = write!(text, ",VIDEO-RANGE={}", range.as_str());
    }
    if subtitles {
        text.push_str(",SUBTITLES=\"subs\"");
    }
    text
}

/// `text` from the file (a track's title or language), as the inside of
/// an HLS quoted-string: no `"`, CR or LF (RFC 8216 4.2), and no other
/// control character either. A line break in it would end the tag, and
/// the next line would be read as a tag or a URI the player fetches
/// (finding 53).
fn quoted_string(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '"' => '\'',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

/// `code` (from the file) as HLS's `LANGUAGE`, a BCP 47 tag: letters,
/// digits and `-` only. Anything else is not a tag, and is left out
/// (finding 53).
fn language_tag(code: &str) -> Option<String> {
    let tag = language::bcp47(code);
    let valid = !tag.is_empty()
        && tag.len() <= 35
        && tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    valid.then_some(tag)
}

/// The cues of WebVTT `vtt` that show during `start_ms..end_ms`, as one
/// WebVTT segment. A cue across a cut is in both segments; players show it
/// once. `X-TIMESTAMP-MAP` ties WebVTT time 0 to media time 0: the video
/// segments store the film's own timestamps (P2-05, `frag_discont` in
/// `args`), in every run, and so do the cues. A run from 0 stores them a
/// few frames late (ffmpeg moves it forward until no decode time is
/// negative); a run from a seek stores them as they are.
pub fn subtitle_segment(vtt: &str, start_ms: u64, end_ms: u64) -> String {
    let mut out = String::from("WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000\n\n");
    for cue in vtt_cues(vtt) {
        if cue.start_ms < end_ms && cue.end_ms > start_ms {
            let _ = write!(
                out,
                "{} --> {}\n{}\n\n",
                vtt_time(cue.start_ms),
                vtt_time(cue.end_ms),
                cue.text
            );
        }
    }
    out
}

/// One WebVTT cue: its times and its text, settings dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
struct VttCue {
    start_ms: u64,
    end_ms: u64,
    text: String,
}

/// The cues of a WebVTT file, as ffmpeg and the OCR write them: blocks
/// separated by blank lines, each with a timing line (`a --> b`, settings
/// after), optionally an identifier before it, and text after. NOTE, STYLE
/// and REGION blocks have no timing line and are skipped.
fn vtt_cues(vtt: &str) -> Vec<VttCue> {
    let normalized = vtt.replace("\r\n", "\n");
    let mut cues = Vec::new();
    for block in normalized.split("\n\n") {
        let mut lines = block.lines().skip_while(|l| !l.contains("-->"));
        let Some(timing) = lines.next() else {
            continue;
        };
        let Some((start, rest)) = timing.split_once("-->") else {
            continue;
        };
        let end = rest.split_whitespace().next().unwrap_or_default();
        let (Some(start_ms), Some(end_ms)) = (parse_time(start.trim()), parse_time(end)) else {
            continue;
        };
        let text = lines.collect::<Vec<_>>().join("\n");
        if !text.trim().is_empty() {
            cues.push(VttCue {
                start_ms,
                end_ms,
                text,
            });
        }
    }
    cues
}

/// `hh:mm:ss.ttt` or `mm:ss.ttt`.
fn parse_time(t: &str) -> Option<u64> {
    let (clock, millis) = t.split_once('.')?;
    // Parsed text: a time too large for a u64 of milliseconds is no time.
    let mut secs = 0u64;
    for part in clock.split(':') {
        secs = secs
            .checked_mul(60)?
            .checked_add(part.parse::<u64>().ok()?)?;
    }
    secs.checked_mul(1000)?
        .checked_add(millis.parse::<u64>().ok()?)
}

fn vtt_time(ms: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

/// RFC 6381 names, as Apple's HLS spec wants them. An HEVC stream's own
/// `level` is used where it is known (a copied one). Otherwise the levels
/// are the highest a stream of that height commonly needs, which players
/// treat as an upper bound.
fn video_codec_string(
    codec: VideoCodec,
    bit_depth: u32,
    height: u32,
    level: Option<u32>,
    fast: bool,
) -> String {
    match codec {
        // High profile; level 4.0 to 1080p, 5.1 above.
        VideoCodec::H264 => {
            if height > 1080 {
                "avc1.640033".into()
            } else {
                "avc1.640028".into()
            }
        }
        // Main or Main 10, always as main tier (the probe does not say
        // the tier); level 4.0 to 1080p, 5.1 above.
        VideoCodec::Hevc => {
            let profile = if bit_depth > 8 { "2.4" } else { "1.6" };
            let level = level.unwrap_or(if height > 1080 { 153 } else { 120 });
            format!("hvc1.{profile}.L{level}.B0")
        }
        // Main profile, main tier, 8 or 10 bits (PGPU-06); level 4.0
        // (`08`) to 1080p, above it 5.0 (`12`), 5.1 (`13`) over 30 frames
        // a second: `seq_level_idx`, as RFC 6381's AV1 form writes it.
        VideoCodec::Av1 => {
            let level = match (height > 1080, fast) {
                (false, _) => 8,
                (true, false) => 12,
                (true, true) => 13,
            };
            let depth = if bit_depth > 8 { 10 } else { 8 };
            format!("av01.0.{level:02}M.{depth:02}")
        }
        other => other.as_str().into(),
    }
}

fn audio_codec_string(codec: AudioCodec) -> Option<&'static str> {
    Some(match codec {
        AudioCodec::Aac => "mp4a.40.2",
        AudioCodec::Ac3 => "ac-3",
        AudioCodec::Eac3 => "ec-3",
        AudioCodec::Opus => "Opus",
        AudioCodec::Flac => "fLaC",
        AudioCodec::Mp3 => "mp4a.40.34",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_vtt_time_too_large_is_no_time() {
        assert_eq!(super::parse_time("01:02:03.004"), Some(3_723_004));
        assert_eq!(super::parse_time("99999999999999999:00.000"), None);
        assert_eq!(super::parse_time("18446744073709551.616"), None);
    }

    use super::*;

    /// PX-09 B-4: the grid's size comes from ffprobe's length of the file;
    /// one said to be longer than 48 hours is refused, with why.
    #[test]
    fn a_file_said_to_be_longer_than_48_hours_has_no_hls() {
        let hours = |h: u64| Some(std::time::Duration::from_secs(h * 3600));
        assert_eq!(hls_total_ms(hours(48)), Ok(48 * 3600 * 1000));
        let refused = hls_total_ms(hours(49)).unwrap_err();
        assert!(refused.contains("48 hours"), "{refused}");
        assert!(hls_total_ms(Some(std::time::Duration::MAX)).is_err());
        assert_eq!(hls_total_ms(None), Ok(0));
    }

    #[test]
    fn a_grid_covers_the_file_with_a_short_last_segment() {
        let s = Segments::grid(20_000);
        assert_eq!(s.starts_ms, [0, 6000, 12_000, 18_000]);
        assert_eq!(s.start(4), Some(18_000));
        assert_eq!(s.start(0), None);
        let playlist = s.media_playlist();
        assert!(playlist.contains("#EXT-X-TARGETDURATION:6\n"), "{playlist}");
        assert!(
            playlist.contains("#EXTINF:2.000,\nseg4.m4s\n"),
            "{playlist}"
        );
        assert!(playlist.ends_with("#EXT-X-ENDLIST\n"));
    }

    /// PX-26: an encoded video's first segment is 2 s, so the player has
    /// one to play two seconds into the encode; the second 2 s and the
    /// third 4 s, where ffmpeg's muxer cuts, then 6 s.
    #[test]
    fn an_encoded_grid_starts_with_two_second_segments() {
        let s = Segments::encoded(30_000);
        assert_eq!(s.starts_ms, [0, 2000, 4000, 8000, 14_000, 20_000, 26_000]);
        assert_eq!(s.end(1), Some(2000));
        assert_eq!(s.start(4), Some(8000));
        assert_eq!(s.end(7), Some(30_000));
    }

    #[test]
    fn a_short_encoded_film_keeps_what_of_the_head_it_reaches() {
        assert_eq!(Segments::encoded(0).starts_ms, [0]);
        assert_eq!(Segments::encoded(1_500).starts_ms, [0]);
        assert_eq!(Segments::encoded(3_000).starts_ms, [0, 2000]);
        assert_eq!(Segments::encoded(8_000).starts_ms, [0, 2000, 4000]);
        assert_eq!(Segments::encoded(8_600).starts_ms, [0, 2000, 4000, 8000]);
        // A sliver past the head joins its last segment.
        assert_eq!(Segments::encoded(8_200).starts_ms, [0, 2000, 4000]);
    }

    #[test]
    fn an_encoded_playlist_lists_the_short_head_and_the_longest_as_its_target() {
        let playlist = Segments::encoded(20_000).media_playlist();
        let extinf: Vec<&str> = playlist
            .lines()
            .filter(|l| l.starts_with("#EXTINF:"))
            .collect();
        assert_eq!(
            extinf,
            [
                "#EXTINF:2.000,",
                "#EXTINF:2.000,",
                "#EXTINF:4.000,",
                "#EXTINF:6.000,",
                "#EXTINF:6.000,"
            ]
        );
        assert!(playlist.contains("#EXT-X-TARGETDURATION:6\n"), "{playlist}");
        assert!(
            playlist.contains("#EXTINF:2.000,\nseg1.m4s\n"),
            "{playlist}"
        );
    }

    /// A run started in the head would cut on the 6 s grid from where it
    /// started: a segment of the head is written by a run from 0.
    #[test]
    fn a_run_for_a_segment_of_the_head_starts_at_the_first() {
        let s = Segments::encoded(60_000);
        assert_eq!([1, 2, 3].map(|n| s.run_from(n)), [1, 1, 1]);
        assert_eq!(s.run_from(4), 4);
        assert_eq!(s.run_from(9), 9);
        // The audio grid and keyframe cuts start a run anywhere.
        assert_eq!(Segments::grid(60_000).run_from(2), 2);
        assert_eq!(
            Segments::at_keyframes(&[0, 2000, 4000], 6000).run_from(2),
            2
        );
    }

    #[test]
    fn a_sliver_at_the_end_joins_the_last_segment() {
        // 30.005 s: the audio runs 5 ms past the last frame.
        assert_eq!(Segments::grid(30_005).starts_ms.len(), 5);
        assert_eq!(
            Segments::at_keyframes(&[0, 2000, 4000], 4_010).starts_ms,
            [0, 2000]
        );
    }

    #[test]
    fn keyframes_are_the_cuts_and_the_first_segment_starts_at_zero() {
        let s = Segments::at_keyframes(&[0, 2000, 4000, 9000], 10_000);
        assert_eq!(s.starts_ms, [0, 2000, 4000, 9000]);
        let playlist = s.media_playlist();
        // The longest is 5 s (4000 to 9000).
        assert!(playlist.contains("#EXT-X-TARGETDURATION:5\n"), "{playlist}");
    }

    /// ffmpeg's HLS muxer never cuts at the first keyframe it sees: its
    /// first segment runs to the second one.
    #[test]
    fn a_late_first_keyframe_is_not_a_cut() {
        let s = Segments::at_keyframes(&[520, 2520, 4520, 6520, 8520], 10_000);
        assert_eq!(s.starts_ms, [0, 2520, 4520, 6520, 8520]);
    }

    #[test]
    fn the_playlists_parse_as_hls() {
        let media = Segments::grid(30_000).media_playlist();
        match m3u8_rs::parse_playlist_res(media.as_bytes()).unwrap() {
            m3u8_rs::Playlist::MediaPlaylist(p) => {
                assert_eq!(p.segments.len(), 5);
                assert!(p.end_list);
                let total: f32 = p.segments.iter().map(|s| s.duration).sum();
                assert!((total - 30.0).abs() < 0.01);
            }
            other => panic!("not a media playlist: {other:?}"),
        }
    }

    #[test]
    fn hevc_main_10_at_4k_is_named_for_the_player() {
        assert_eq!(
            video_codec_string(VideoCodec::Hevc, 10, 2160, None, false),
            "hvc1.2.4.L153.B0"
        );
        assert_eq!(
            video_codec_string(VideoCodec::H264, 8, 720, None, false),
            "avc1.640028"
        );
        assert_eq!(audio_codec_string(AudioCodec::Eac3), Some("ec-3"));
    }

    /// The film that did not start on an iPhone: 4K HEVC Main 10 at level
    /// 5.1, HDR10 (PQ), 23.976 fps; its sound is not what is tested.
    fn film(transfer: &str) -> MediaDescriptor {
        MediaDescriptor {
            container: "matroska,webm".into(),
            duration: None,
            bit_rate: Some(60_000_000),
            video: Some(spoolframe_media::VideoStream {
                index: 0,
                codec: "hevc".into(),
                profile: Some("Main 10".into()),
                level: Some(153),
                width: 3840,
                height: 1600,
                pixel_format: Some("yuv420p10le".into()),
                color_transfer: Some(transfer.into()),
                frame_rate_millis: Some(23_976),
                bit_depth: 10,
                hdr: None,
                dolby_vision: None,
                bit_rate: None,
            }),
            audio: Vec::new(),
            subtitles: Vec::new(),
        }
    }

    const COPY_HEVC: Option<ConvertedVideo> = Some(ConvertedVideo::Copy {
        codec: VideoCodec::Hevc,
        dolby_vision: false,
    });

    /// The `EXT-X-STREAM-INF` line.
    fn variant(text: &str) -> &str {
        text.lines()
            .find(|l| l.starts_with("#EXT-X-STREAM-INF:"))
            .unwrap_or_else(|| panic!("no variant: {text}"))
    }

    #[test]
    fn a_copied_pq_film_is_declared_pq_with_its_size_and_frame_rate() {
        let eac3 = Some(ConvertedAudio {
            index: 0,
            action: AudioAction::Encode {
                codec: AudioCodec::Eac3,
                channels: 6,
                bitrate: 640_000,
            },
        });
        let text = multivariant_playlist(&film("smpte2084"), COPY_HEVC, None, eac3, None);
        assert_eq!(
            variant(&text),
            "#EXT-X-STREAM-INF:BANDWIDTH=90704000,AVERAGE-BANDWIDTH=60640000,\
             CODECS=\"hvc1.2.4.L153.B0,ec-3\",\
             RESOLUTION=3840x1600,FRAME-RATE=23.976,VIDEO-RANGE=PQ"
        );
        assert!(
            matches!(
                m3u8_rs::parse_playlist_res(text.as_bytes()),
                Ok(m3u8_rs::Playlist::MasterPlaylist(_))
            ),
            "{text}"
        );
    }

    /// The variant's `(BANDWIDTH, AVERAGE-BANDWIDTH)`.
    fn bandwidths(text: &str) -> (u64, Option<u64>) {
        match m3u8_rs::parse_playlist_res(text.as_bytes()) {
            Ok(m3u8_rs::Playlist::MasterPlaylist(p)) => {
                (p.variants[0].bandwidth, p.variants[0].average_bandwidth)
            }
            other => panic!("not a multivariant playlist: {other:?}"),
        }
    }

    /// A track, CD-quality FLAC at its average rate, with no picture.
    fn flac_track(bit_rate: u64) -> MediaDescriptor {
        MediaDescriptor {
            container: "flac".into(),
            duration: None,
            bit_rate: Some(bit_rate),
            video: None,
            audio: Vec::new(),
            subtitles: Vec::new(),
        }
    }

    const COPY_FLAC: Option<ConvertedAudio> = Some(ConvertedAudio {
        index: 0,
        action: AudioAction::Copy {
            codec: AudioCodec::Flac,
        },
    });

    /// AVPlayer refuses a segment over BANDWIDTH (-12318). Copied FLAC
    /// was declared at 640 kbit/s, whatever its rate.
    #[test]
    fn copied_flac_declares_its_own_rate_and_a_peak_above_it() {
        let text = multivariant_playlist(&flac_track(1_000_000), None, None, COPY_FLAC, None);
        let (peak, average) = bandwidths(&text);
        assert_eq!(average, Some(1_000_000));
        assert!(peak >= 1_500_000, "{text}");
    }

    /// A ladder (PX-21) lists a variant per rung, in the order given,
    /// each at its own size and cap, at `v{n}/media.m3u8`.
    #[test]
    fn a_ladder_lists_each_rung_with_its_own_size_and_rate() {
        let rung = |height, cap| ConvertedVideo::Encode {
            codec: VideoCodec::H264,
            height,
            max_bitrate: Some(cap),
            tone_map: false,
        };
        let rungs = [
            (2, rung(720, 3_000_000), None),
            (0, rung(1080, 8_000_000), None),
            (1, rung(1080, 5_000_000), None),
        ];
        let text = ladder_playlist(&film("bt709"), &rungs, None, None);
        let Ok(m3u8_rs::Playlist::MasterPlaylist(p)) = m3u8_rs::parse_playlist_res(text.as_bytes())
        else {
            panic!("not a multivariant playlist: {text}");
        };
        let listed: Vec<(&str, Option<u64>, Option<u64>)> = p
            .variants
            .iter()
            .map(|v| {
                (
                    v.uri.as_str(),
                    v.resolution.map(|r| r.height),
                    v.average_bandwidth,
                )
            })
            .collect();
        assert_eq!(
            listed,
            [
                ("v2/media.m3u8", Some(720), Some(3_000_000)),
                ("v0/media.m3u8", Some(1080), Some(8_000_000)),
                ("v1/media.m3u8", Some(1080), Some(5_000_000)),
            ]
        );
    }

    /// A remux ladder (PX-21): the copy, at the source's size, codec and
    /// rate (its peak in BANDWIDTH, as for one copied variant), beside the
    /// encodes under it at theirs.
    #[test]
    fn a_remux_ladder_lists_the_copy_with_the_source_s_codec_and_rate() {
        let copy = ConvertedVideo::Copy {
            codec: VideoCodec::Hevc,
            dolby_vision: false,
        };
        let encode = ConvertedVideo::Encode {
            codec: VideoCodec::Hevc,
            height: 720,
            max_bitrate: Some(4_000_000),
            tone_map: false,
        };
        let film = film("smpte2084");
        let text = ladder_playlist(&film, &[(1, encode, None), (0, copy, None)], None, None);
        let Ok(m3u8_rs::Playlist::MasterPlaylist(p)) = m3u8_rs::parse_playlist_res(text.as_bytes())
        else {
            panic!("not a multivariant playlist: {text}");
        };
        let alone = multivariant_playlist(&film, Some(copy), None, None, None);
        let (peak, average) = bandwidths(&alone);
        let top = &p.variants[1];
        assert_eq!(top.uri, "v0/media.m3u8");
        assert_eq!(top.resolution.map(|r| r.height), Some(1600));
        assert_eq!((top.bandwidth, top.average_bandwidth), (peak, average));
        assert_eq!(p.variants[0].resolution.map(|r| r.height), Some(720));
        assert!(text.contains("#EXT-X-INDEPENDENT-SEGMENTS"), "{text}");
    }

    /// A capped encode's segments can run over its cap by what the
    /// encoder's buffer holds, twice the cap, spread over the shortest
    /// segment: the first, 2 s (PX-26).
    #[test]
    fn a_capped_encode_declares_its_cap_and_its_buffer() {
        let video = Some(ConvertedVideo::Encode {
            codec: VideoCodec::H264,
            height: 1080,
            max_bitrate: Some(6_000_000),
            tone_map: false,
        });
        let aac = Some(ConvertedAudio {
            index: 0,
            action: AudioAction::Encode {
                codec: AudioCodec::Aac,
                channels: 2,
                bitrate: 200_000,
            },
        });
        let text = multivariant_playlist(&film("bt709"), video, None, aac, None);
        assert_eq!(bandwidths(&text), (12_000_000 + 220_000, Some(6_200_000)));
    }

    #[test]
    fn an_sdr_film_is_declared_sdr() {
        for transfer in ["bt709", "unknown"] {
            let text = multivariant_playlist(&film(transfer), COPY_HEVC, None, None, None);
            assert!(variant(&text).ends_with(",VIDEO-RANGE=SDR"), "{text}");
        }
        let mut untold = film("bt709");
        untold.video.as_mut().unwrap().color_transfer = None;
        let text = multivariant_playlist(&untold, COPY_HEVC, None, None, None);
        assert!(variant(&text).ends_with(",VIDEO-RANGE=SDR"), "{text}");
    }

    #[test]
    fn a_copied_hlg_film_is_declared_hlg() {
        let text = multivariant_playlist(&film("arib-std-b67"), COPY_HEVC, None, None, None);
        assert!(variant(&text).ends_with(",VIDEO-RANGE=HLG"), "{text}");
    }

    #[test]
    fn a_tone_mapped_encode_of_a_pq_film_is_declared_sdr() {
        let encode = |codec, tone_map| {
            Some(ConvertedVideo::Encode {
                codec,
                height: 1080,
                max_bitrate: None,
                tone_map,
            })
        };
        let pq = film("smpte2084");
        let mapped = multivariant_playlist(&pq, encode(VideoCodec::H264, true), None, None, None);
        assert!(variant(&mapped).ends_with(",VIDEO-RANGE=SDR"), "{mapped}");
        // Encoded as it is: still PQ.
        let kept = multivariant_playlist(&pq, encode(VideoCodec::Hevc, false), None, None, None);
        assert!(variant(&kept).ends_with(",VIDEO-RANGE=PQ"), "{kept}");
    }

    /// PGPU-06: a Dolby Vision copy labelled so is named so: profile 8.1
    /// as HEVC with `dvh1.08.LL/db1p` beside it (8.4's `db4h`, HLG),
    /// profile 5 as `dvh1.05.LL`, PQ; copied as its base layer, plain HEVC.
    #[test]
    fn a_dolby_vision_copy_is_named_for_the_player() {
        let dv = |profile, compatibility, transfer| {
            let mut film = film(transfer);
            let video = film.video.as_mut().unwrap();
            video.hdr = Some(spoolframe_types::HdrFormat::DolbyVision);
            video.dolby_vision = Some(spoolframe_media::DolbyVision {
                profile,
                level: 6,
                compatibility,
            });
            film
        };
        let copy = |dolby_vision| {
            Some(ConvertedVideo::Copy {
                codec: VideoCodec::Hevc,
                dolby_vision,
            })
        };
        let p81 = multivariant_playlist(&dv(8, 1, "smpte2084"), copy(true), None, None, None);
        let line = variant(&p81);
        assert!(line.contains("CODECS=\"hvc1.2.4."), "{p81}");
        assert!(
            line.contains(",SUPPLEMENTAL-CODECS=\"dvh1.08.06/db1p\""),
            "{p81}"
        );
        assert!(line.ends_with(",VIDEO-RANGE=PQ"), "{p81}");
        let p84 = multivariant_playlist(&dv(8, 4, "arib-std-b67"), copy(true), None, None, None);
        assert!(variant(&p84).contains("dvh1.08.06/db4h"), "{p84}");
        assert!(variant(&p84).ends_with(",VIDEO-RANGE=HLG"), "{p84}");
        // Profile 5's transfer as a player reads it says nothing: PQ.
        let p5 = multivariant_playlist(&dv(5, 0, "unknown"), copy(true), None, None, None);
        assert!(variant(&p5).contains("CODECS=\"dvh1.05.06"), "{p5}");
        assert!(!variant(&p5).contains("SUPPLEMENTAL"), "{p5}");
        assert!(variant(&p5).ends_with(",VIDEO-RANGE=PQ"), "{p5}");
        // The base layer alone: HEVC, nothing beside.
        let base = multivariant_playlist(&dv(8, 1, "smpte2084"), copy(false), None, None, None);
        assert!(!variant(&base).contains("dvh1"), "{base}");
    }

    /// PGPU-06: an encode names what it writes. HEVC or AV1 keeping HDR
    /// is 10-bit (Main 10, `av01….10`) and PQ; tone mapped, 8-bit and SDR.
    #[test]
    fn an_encode_is_named_for_the_depth_it_writes() {
        let encode = |codec, tone_map| {
            Some(ConvertedVideo::Encode {
                codec,
                height: 1080,
                max_bitrate: None,
                tone_map,
            })
        };
        let pq = film("smpte2084");
        for (codec, tone_map, name, range) in [
            (VideoCodec::Hevc, false, "hvc1.2.4.L120.B0", "PQ"),
            (VideoCodec::Hevc, true, "hvc1.1.6.L120.B0", "SDR"),
            (VideoCodec::Av1, false, "av01.0.08M.10", "PQ"),
            (VideoCodec::Av1, true, "av01.0.08M.08", "SDR"),
        ] {
            let text = multivariant_playlist(&pq, encode(codec, tone_map), None, None, None);
            assert!(text.contains(&format!("CODECS=\"{name}")), "{text}");
            assert!(
                variant(&text).ends_with(&format!(",VIDEO-RANGE={range}")),
                "{text}"
            );
        }
        // AV1 above 1080p: level 5.0, and 5.1 over 30 frames a second.
        assert_eq!(
            video_codec_string(VideoCodec::Av1, 10, 2160, None, false),
            "av01.0.12M.10"
        );
        assert_eq!(
            video_codec_string(VideoCodec::Av1, 10, 2160, None, true),
            "av01.0.13M.10"
        );
    }

    #[test]
    fn a_copied_hevc_stream_is_named_with_its_own_level() {
        // 1080p at level 4.1, and 4K at 60 fps, level 5.2: neither is the
        // level its height alone suggests.
        for (height, level, name) in [
            (1080, 123, "hvc1.2.4.L123.B0"),
            (2160, 156, "hvc1.2.4.L156.B0"),
        ] {
            let mut film = film("smpte2084");
            let video = film.video.as_mut().unwrap();
            (video.height, video.level) = (height, Some(level));
            let text = multivariant_playlist(&film, COPY_HEVC, None, None, None);
            assert!(text.contains(&format!("CODECS=\"{name}\"")), "{text}");
        }
    }

    const VTT: &str = "WEBVTT\n\n\
                       1\n00:09:58.000 --> 00:10:01.500 line:90%\nAt ten minutes\n\n\
                       NOTE a comment, not a cue\n\n\
                       10:05.000 --> 10:06.000\nFive seconds after\n\n";

    #[test]
    fn a_cue_is_in_the_segment_that_holds_its_time() {
        let grid = Segments::grid(7_200_000);
        // 10:00 is the start of segment 101 (6 s each, from 1).
        assert_eq!(grid.start(101), Some(600_000));
        let at_ten = subtitle_segment(VTT, 600_000, grid.end(101).unwrap());
        assert!(at_ten.starts_with("WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000\n"));
        assert!(
            at_ten.contains("00:09:58.000 --> 00:10:01.500\nAt ten minutes"),
            "{at_ten}"
        );
        assert!(
            at_ten.contains("00:10:05.000 --> 00:10:06.000\nFive seconds after"),
            "{at_ten}"
        );
        // Across the cut at 10:00, the first cue is in the segment before
        // too; neither is in the one after.
        let before = subtitle_segment(VTT, 594_000, 600_000);
        assert!(before.contains("At ten minutes") && !before.contains("Five seconds"));
        let after = subtitle_segment(VTT, 606_000, 612_000);
        assert!(!after.contains("-->"), "{after}");
    }

    #[test]
    fn the_subtitle_playlist_is_on_the_video_s_cuts() {
        let s = Segments::at_keyframes(&[0, 2000, 4000, 9000], 10_000);
        let subs = s.subtitle_playlist();
        assert!(!subs.contains("EXT-X-MAP"), "{subs}");
        match m3u8_rs::parse_playlist_res(subs.as_bytes()).unwrap() {
            m3u8_rs::Playlist::MediaPlaylist(p) => {
                let uris: Vec<_> = p.segments.iter().map(|s| s.uri.as_str()).collect();
                assert_eq!(uris, ["subs1.vtt", "subs2.vtt", "subs3.vtt", "subs4.vtt"]);
                let durations: Vec<_> = p.segments.iter().map(|s| s.duration).collect();
                assert_eq!(durations, [2.0, 2.0, 5.0, 1.0]);
            }
            other => panic!("not a media playlist: {other:?}"),
        }
    }

    #[test]
    fn the_multivariant_playlist_names_the_subtitle_rendition() {
        let descriptor = MediaDescriptor {
            container: "matroska,webm".into(),
            duration: None,
            bit_rate: None,
            video: None,
            audio: Vec::new(),
            subtitles: Vec::new(),
        };
        let track = SubtitleStream {
            source: spoolframe_media::SubtitleSource::Embedded { index: 3 },
            codec: "subrip".into(),
            kind: spoolframe_media::SubtitleKind::Text,
            language: Some("eng".into()),
            title: None,
            forced: false,
            default: false,
            role: spoolframe_types::SubtitleRole::Full,
        };
        let text = multivariant_playlist(&descriptor, None, None, None, Some(&track));
        match m3u8_rs::parse_playlist_res(text.as_bytes()).unwrap() {
            m3u8_rs::Playlist::MasterPlaylist(p) => {
                let subs = &p.alternatives[0];
                assert_eq!(subs.media_type, m3u8_rs::AlternativeMediaType::Subtitles);
                assert_eq!(subs.language.as_deref(), Some("en"));
                assert_eq!(subs.uri.as_deref(), Some("subtitles.m3u8"));
                assert_eq!(p.variants[0].subtitles.as_deref(), Some("subs"));
            }
            other => panic!("not a multivariant playlist: {other:?}"),
        }
    }

    /// The multivariant playlist of a subtitle rendition of `language`
    /// and `title`, and nothing else.
    fn with_subtitle(language: Option<&str>, title: Option<&str>) -> String {
        let descriptor = MediaDescriptor {
            container: "matroska,webm".into(),
            duration: None,
            bit_rate: None,
            video: None,
            audio: Vec::new(),
            subtitles: Vec::new(),
        };
        let track = SubtitleStream {
            source: spoolframe_media::SubtitleSource::Embedded { index: 3 },
            codec: "subrip".into(),
            kind: spoolframe_media::SubtitleKind::Text,
            language: language.map(Into::into),
            title: title.map(Into::into),
            forced: false,
            default: false,
            role: spoolframe_types::SubtitleRole::Full,
        };
        multivariant_playlist(&descriptor, None, None, None, Some(&track))
    }

    /// The lines of `playlist` that are neither tags nor blank: the URIs a
    /// player fetches.
    fn uris(playlist: &str) -> Vec<&str> {
        playlist
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect()
    }

    const INJECTED: &str = "Forced\n#EXT-X-STREAM-INF:BANDWIDTH=1\nhttp://attacker/x";

    /// Finding 53 (G-4): a track title from the file is written into the
    /// playlist; a line break in it must not start a line of its own.
    #[test]
    fn a_line_break_in_a_subtitle_title_adds_no_line() {
        let text = with_subtitle(Some("eng"), Some(INJECTED));
        assert_eq!(uris(&text), ["media.m3u8"], "{text}");
    }

    #[test]
    fn a_line_break_in_a_subtitle_language_adds_no_line() {
        let text = with_subtitle(Some(INJECTED), Some("English"));
        assert_eq!(uris(&text), ["media.m3u8"], "{text}");
    }

    /// The language's name is the NAME when there is no title.
    #[test]
    fn a_line_break_in_an_untitled_subtitle_s_language_adds_no_line() {
        let text = with_subtitle(Some(INJECTED), None);
        assert_eq!(uris(&text), ["media.m3u8"], "{text}");
    }

    /// LANGUAGE is a BCP 47 tag: letters, digits and `-`. Anything else
    /// is not one, and the attribute is left out.
    #[test]
    fn a_language_that_is_not_a_tag_is_left_out() {
        let text = with_subtitle(Some("en\"x"), Some("English"));
        assert!(!text.contains("LANGUAGE="), "{text}");
    }

    #[test]
    fn a_tag_with_a_region_is_kept() {
        let text = with_subtitle(Some("pt-BR"), None);
        assert!(text.contains(",LANGUAGE=\"pt-br\""), "{text}");
    }
}
