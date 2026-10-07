//! HLS for a converted playback (P2-05): where its segments are cut, and
//! the playlists that list them.
//!
//! The media playlist lists the whole file from the start (VOD), before
//! most of it is encoded, so a player knows the length and can seek. The
//! cuts are therefore decided here, and the encode is made to cut at them,
//! across restarts too:
//!
//! - An encoded video gets a keyframe forced every 6 s from the first, so
//!   segments are a 6 s grid. A restart begins on the grid, and ffmpeg's
//!   HLS muxer, which cuts at `start + 6 s × n`, stays on it.
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

use super::args::SEGMENT_SECS;

/// The shortest last segment kept on its own.
const TAIL_MS: u64 = 500;

/// The segments of a converted playback: segment `n` (from 1) starts at
/// `starts_ms[n - 1]` and ends where the next starts, or at `total_ms`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segments {
    pub starts_ms: Vec<u64>,
    pub total_ms: u64,
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
    /// A grid of [`SEGMENT_SECS`].
    pub fn grid(total_ms: u64) -> Self {
        let step = u64::from(SEGMENT_SECS) * 1000;
        let count = total_ms.div_ceil(step).max(1);
        Self::tidy((0..count).map(|i| i * step).collect(), total_ms)
    }

    /// A cut at every keyframe but the first: the first segment starts at 0
    /// whatever the first keyframe's time, and holds the first GOP whole,
    /// as ffmpeg writes it.
    pub fn at_keyframes(keyframes: &[u64], total_ms: u64) -> Self {
        let mut starts_ms = vec![0];
        let later = keyframes.iter().skip(1).copied();
        starts_ms.extend(later.filter(|&t| t > 0 && t < total_ms));
        Self::tidy(starts_ms, total_ms)
    }

    /// A last segment shorter than [`TAIL_MS`] is folded into the one
    /// before: the file's duration often runs a few milliseconds past its
    /// last video frame (the audio), and ffmpeg does not cut there.
    fn tidy(mut starts_ms: Vec<u64>, total_ms: u64) -> Self {
        if starts_ms.len() > 1
            && let Some(&last) = starts_ms.last()
            && total_ms.saturating_sub(last) < TAIL_MS
        {
            starts_ms.pop();
        }
        Self {
            starts_ms,
            total_ms,
        }
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
            peak: average.saturating_mul(3) / 2,
            average,
        }
    }

    /// A video encode capped at `cap` (`-maxrate cap -bufsize 2×cap`): a
    /// segment holds at most its time at the cap plus the full buffer.
    fn capped(cap: u64) -> Self {
        Self {
            peak: cap.saturating_add(cap.saturating_mul(2) / u64::from(SEGMENT_SECS)),
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
/// with `subtitle` (the picked track) as its subtitle rendition.
pub fn multivariant_playlist(
    descriptor: &MediaDescriptor,
    video: Option<ConvertedVideo>,
    audio: Option<ConvertedAudio>,
    subtitle: Option<&SubtitleStream>,
) -> String {
    let mut codecs = Vec::new();
    let mut resolution = None;
    let mut picture = None;
    let mut rate = Rate::default();
    if let (Some(plan), Some(source)) = (video, &descriptor.video) {
        // A copied stream is the source's; an encode's level is not known
        // before it runs.
        let (codec, height, cap, level) = match plan {
            ConvertedVideo::Copy { codec } => (codec, source.height, None, source.level),
            ConvertedVideo::Encode {
                codec,
                height,
                max_bitrate,
                ..
            } => (codec, height, max_bitrate, None),
            ConvertedVideo::Unknown => (VideoCodec::Unknown, source.height, None, None),
        };
        codecs.push(video_codec_string(codec, source.bit_depth, height, level));
        let width =
            (u64::from(source.width) * u64::from(height) / u64::from(source.height.max(1))) as u32;
        resolution = Some((width & !1, height));
        // What the output is: a tone-mapped encode is SDR, anything else
        // keeps the source's transfer. No conversion changes the rate.
        let range = match plan {
            ConvertedVideo::Encode { tone_map: true, .. } => VideoRange::Sdr,
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
    let mut text = String::from("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n");
    if let Some(track) = subtitle {
        let name = track
            .title
            .clone()
            .or_else(|| track.language.clone())
            .unwrap_or_else(|| "Subtitles".into())
            .replace('"', "'");
        let _ = write!(
            text,
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"{name}\",DEFAULT=YES,\
             AUTOSELECT=YES,FORCED={},URI=\"subtitles.m3u8\"",
            if track.forced { "YES" } else { "NO" }
        );
        if let Some(language) = &track.language {
            let _ = write!(text, ",LANGUAGE=\"{}\"", language::bcp47(language));
        }
        text.push('\n');
    }
    let _ = write!(
        text,
        "#EXT-X-STREAM-INF:BANDWIDTH={},AVERAGE-BANDWIDTH={}",
        rate.peak, rate.average
    );
    // An empty CODECS is not valid HLS; none is.
    if !codecs.is_empty() {
        let _ = write!(text, ",CODECS=\"{}\"", codecs.join(","));
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
    if subtitle.is_some() {
        text.push_str(",SUBTITLES=\"subs\"");
    }
    text.push_str("\nmedia.m3u8\n");
    text
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
    let mut secs = 0u64;
    for part in clock.split(':') {
        secs = secs * 60 + part.parse::<u64>().ok()?;
    }
    Some(secs * 1000 + millis.parse::<u64>().ok()?)
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
        VideoCodec::Av1 => "av01.0.08M.08".into(),
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
            video_codec_string(VideoCodec::Hevc, 10, 2160, None),
            "hvc1.2.4.L153.B0"
        );
        assert_eq!(
            video_codec_string(VideoCodec::H264, 8, 720, None),
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
            }),
            audio: Vec::new(),
            subtitles: Vec::new(),
        }
    }

    const COPY_HEVC: Option<ConvertedVideo> = Some(ConvertedVideo::Copy {
        codec: VideoCodec::Hevc,
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
        let text = multivariant_playlist(&film("smpte2084"), COPY_HEVC, eac3, None);
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
        let text = multivariant_playlist(&flac_track(1_000_000), None, COPY_FLAC, None);
        let (peak, average) = bandwidths(&text);
        assert_eq!(average, Some(1_000_000));
        assert!(peak >= 1_500_000, "{text}");
    }

    /// A capped encode's segments can run over its cap by what the
    /// encoder's buffer holds, twice the cap, spread over one segment.
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
        let text = multivariant_playlist(&film("bt709"), video, aac, None);
        assert_eq!(bandwidths(&text), (8_000_000 + 220_000, Some(6_200_000)));
    }

    #[test]
    fn an_sdr_film_is_declared_sdr() {
        for transfer in ["bt709", "unknown"] {
            let text = multivariant_playlist(&film(transfer), COPY_HEVC, None, None);
            assert!(variant(&text).ends_with(",VIDEO-RANGE=SDR"), "{text}");
        }
        let mut untold = film("bt709");
        untold.video.as_mut().unwrap().color_transfer = None;
        let text = multivariant_playlist(&untold, COPY_HEVC, None, None);
        assert!(variant(&text).ends_with(",VIDEO-RANGE=SDR"), "{text}");
    }

    #[test]
    fn a_copied_hlg_film_is_declared_hlg() {
        let text = multivariant_playlist(&film("arib-std-b67"), COPY_HEVC, None, None);
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
        let mapped = multivariant_playlist(&pq, encode(VideoCodec::H264, true), None, None);
        assert!(variant(&mapped).ends_with(",VIDEO-RANGE=SDR"), "{mapped}");
        // Encoded as it is: still PQ.
        let kept = multivariant_playlist(&pq, encode(VideoCodec::Hevc, false), None, None);
        assert!(variant(&kept).ends_with(",VIDEO-RANGE=PQ"), "{kept}");
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
            let text = multivariant_playlist(&film, COPY_HEVC, None, None);
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
        let text = multivariant_playlist(&descriptor, None, None, Some(&track));
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
}
