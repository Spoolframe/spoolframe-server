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
//!   the same whatever a run started from.
//! - Audio alone cuts anywhere: a 6 s grid.

use std::fmt::Write as _;

use spoolframe_media::MediaDescriptor;
use spoolframe_protocol::{AudioAction, ConvertedAudio, ConvertedVideo};
use spoolframe_types::{AudioCodec, VideoCodec};

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

impl Segments {
    /// A grid of [`SEGMENT_SECS`].
    pub fn grid(total_ms: u64) -> Self {
        let step = u64::from(SEGMENT_SECS) * 1000;
        let count = total_ms.div_ceil(step).max(1);
        Self::tidy((0..count).map(|i| i * step).collect(), total_ms)
    }

    /// A cut at every keyframe. The first segment starts at 0 whatever the
    /// first keyframe's time.
    pub fn at_keyframes(keyframes: &[u64], total_ms: u64) -> Self {
        let mut starts_ms = vec![0];
        starts_ms.extend(keyframes.iter().copied().filter(|&t| t > 0 && t < total_ms));
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

    fn duration_ms(&self, i: usize) -> u64 {
        let end = self.starts_ms.get(i + 1).copied().unwrap_or(self.total_ms);
        end.saturating_sub(self.starts_ms[i])
    }

    /// The VOD media playlist. Segment URIs are relative names
    /// (`seg3.m4s`) that a client maps to a segment read.
    pub fn media_playlist(&self) -> String {
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
        text.push_str("#EXT-X-MAP:URI=\"init.mp4\"\n");
        for i in 0..self.starts_ms.len() {
            let _ = writeln!(
                text,
                "#EXTINF:{:.3},\nseg{}.m4s",
                self.duration_ms(i) as f64 / 1000.0,
                i + 1
            );
        }
        text.push_str("#EXT-X-ENDLIST\n");
        text
    }
}

/// The multivariant playlist: the one variant, described for the player.
pub fn multivariant_playlist(
    descriptor: &MediaDescriptor,
    video: Option<ConvertedVideo>,
    audio: Option<ConvertedAudio>,
) -> String {
    let mut codecs = Vec::new();
    let mut resolution = None;
    let mut video_bits = 0;
    if let (Some(plan), Some(source)) = (video, &descriptor.video) {
        let (codec, height, cap) = match plan {
            ConvertedVideo::Copy { codec } => (codec, source.height, None),
            ConvertedVideo::Encode {
                codec,
                height,
                max_bitrate,
                ..
            } => (codec, height, max_bitrate),
            ConvertedVideo::Unknown => (VideoCodec::Unknown, source.height, None),
        };
        codecs.push(video_codec_string(codec, source.bit_depth, height));
        let width =
            (u64::from(source.width) * u64::from(height) / u64::from(source.height.max(1))) as u32;
        resolution = Some((width & !1, height));
        video_bits = cap.or(descriptor.bit_rate).unwrap_or(8_000_000);
    }
    let mut audio_bits = 0;
    if let Some(audio) = audio {
        let (codec, bitrate) = match audio.action {
            AudioAction::Copy { codec } => (codec, 640_000),
            AudioAction::Encode { codec, bitrate, .. } => (codec, bitrate),
            AudioAction::Unknown => (AudioCodec::Unknown, 0),
        };
        if let Some(name) = audio_codec_string(codec) {
            codecs.push(name.to_owned());
        }
        audio_bits = bitrate;
    }
    let mut text = String::from("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n");
    let _ = write!(
        text,
        "#EXT-X-STREAM-INF:BANDWIDTH={},CODECS=\"{}\"",
        video_bits + audio_bits,
        codecs.join(",")
    );
    if let Some((w, h)) = resolution {
        let _ = write!(text, ",RESOLUTION={w}x{h}");
    }
    text.push_str("\nmedia.m3u8\n");
    text
}

/// RFC 6381 names, as Apple's HLS spec wants them. The levels are the
/// highest a stream of that height commonly needs, which players treat as
/// an upper bound.
fn video_codec_string(codec: VideoCodec, bit_depth: u32, height: u32) -> String {
    match codec {
        // High profile; level 4.0 to 1080p, 5.1 above.
        VideoCodec::H264 => {
            if height > 1080 {
                "avc1.640033".into()
            } else {
                "avc1.640028".into()
            }
        }
        // Main or Main 10, high tier off; level 4.0 to 1080p, 5.1 above.
        VideoCodec::Hevc => {
            let profile = if bit_depth > 8 { "2.4" } else { "1.6" };
            let level = if height > 1080 { "L153" } else { "L120" };
            format!("hvc1.{profile}.{level}.B0")
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
        let s = Segments::at_keyframes(&[40, 2000, 4000, 9000], 10_000);
        assert_eq!(s.starts_ms, [0, 40, 2000, 4000, 9000]);
        let playlist = s.media_playlist();
        // The longest is 5 s (4000 to 9000).
        assert!(playlist.contains("#EXT-X-TARGETDURATION:5\n"), "{playlist}");
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
            video_codec_string(VideoCodec::Hevc, 10, 2160),
            "hvc1.2.4.L153.B0"
        );
        assert_eq!(video_codec_string(VideoCodec::H264, 8, 720), "avc1.640028");
        assert_eq!(audio_codec_string(AudioCodec::Eac3), Some("ec-3"));
    }
}
