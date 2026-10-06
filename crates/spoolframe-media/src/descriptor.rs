//! ffprobe output → [`MediaDescriptor`].

use std::path::PathBuf;
use std::time::Duration;

use easy_ffprobe::{FfProbe, Ratio, StreamKinds};
use serde::Serialize;
use spoolframe_types::{
    AudioRole, AudioTrack, HdrFormat, MediaTracks, SubtitleRole, SubtitleTrack, VideoTrack,
};

use crate::codecs;
use crate::roles::{self, Flags};

/// The longest a file may say it is for what is sized by its length:
/// frame previews and an HLS grid (PX-09 B-4). The length is ffprobe's,
/// from the file, so a crafted one can say anything; no film is two days
/// long.
pub const MAX_DURATION: Duration = Duration::from_secs(48 * 3600);

/// Everything the server knows about one media file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MediaDescriptor {
    /// ffprobe's container name, e.g. `matroska,webm` or `mov,mp4,…`.
    pub container: String,
    #[serde(serialize_with = "millis")]
    pub duration: Option<Duration>,
    /// Bits per second over the whole file.
    pub bit_rate: Option<u64>,
    /// The main picture. Cover art embedded as a video stream is not it.
    pub video: Option<VideoStream>,
    pub audio: Vec<AudioStream>,
    /// Embedded first, in file order; then external files.
    pub subtitles: Vec<SubtitleStream>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VideoStream {
    /// The stream index in the file.
    pub index: u32,
    pub codec: String,
    pub profile: Option<String>,
    /// The codec's level as ffprobe gives it: 153 for HEVC 5.1, 41 for
    /// H.264 4.1; `None` when the file does not say.
    pub level: Option<u32>,
    pub width: u32,
    pub height: u32,
    pub pixel_format: Option<String>,
    /// e.g. `smpte2084` (HDR10/PQ) or `arib-std-b67` (HLG).
    pub color_transfer: Option<String>,
    /// Frames per 1000 seconds (23 976 for 24000/1001), so it compares
    /// exactly; `None` when the file does not say.
    pub frame_rate_millis: Option<u32>,
    /// Bits per sample: 8, 10 or 12.
    pub bit_depth: u32,
    /// The HDR format, `None` for SDR.
    pub hdr: Option<HdrFormat>,
}

/// The transfer function of the picture, as HLS names it (`VIDEO-RANGE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoRange {
    Sdr,
    /// SMPTE ST 2084: HDR10, and what Dolby Vision profile 8.1 is under
    /// its own layer.
    Pq,
    /// ARIB STD-B67.
    Hlg,
}

impl VideoRange {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sdr => "SDR",
            Self::Pq => "PQ",
            Self::Hlg => "HLG",
        }
    }
}

impl VideoStream {
    /// From the transfer function, not from `hdr`: Dolby Vision is over
    /// either PQ or HLG.
    pub fn range(&self) -> VideoRange {
        match self.color_transfer.as_deref() {
            Some("smpte2084") => VideoRange::Pq,
            Some("arib-std-b67") => VideoRange::Hlg,
            _ => VideoRange::Sdr,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AudioStream {
    pub index: u32,
    pub codec: String,
    pub profile: Option<String>,
    pub channels: Option<u32>,
    pub channel_layout: Option<String>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
    pub role: AudioRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SubtitleStream {
    pub source: SubtitleSource,
    pub codec: String,
    pub kind: SubtitleKind,
    pub language: Option<String>,
    pub title: Option<String>,
    /// `role == Forced`.
    pub forced: bool,
    pub default: bool,
    pub role: SubtitleRole,
}

/// Where a subtitle track lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtitleSource {
    /// A stream inside the media file.
    Embedded { index: u32 },
    /// A separate file next to the media file.
    External { path: PathBuf },
}

/// Whether a subtitle is text, which a client can style or a server can
/// convert, or pictures, which can only be shown as they are or burned
/// in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtitleKind {
    Text,
    Bitmap,
}

impl SubtitleKind {
    /// By ffprobe codec name. The picture-based codecs are a short, known
    /// list; everything else ffprobe reports for subtitles is text.
    pub fn of_codec(codec: &str) -> Self {
        match codec {
            "hdmv_pgs_subtitle" | "dvd_subtitle" | "dvb_subtitle" | "xsub" => Self::Bitmap,
            _ => Self::Text,
        }
    }
}

impl MediaDescriptor {
    /// The descriptor of a media file from its ffprobe output. External
    /// subtitles are added separately.
    pub fn from_probe(probe: &FfProbe) -> Self {
        let mut video = None;
        let mut audio = Vec::new();
        let mut subtitles = Vec::new();
        for stream in &probe.streams {
            let index = u32::try_from(stream.index).unwrap_or(u32::MAX);
            let d = &stream.disposition;
            let flags = Flags {
                forced: flag(d.forced),
                hearing_impaired: flag(d.hearing_impaired),
                visual_impaired: flag(d.visual_impaired),
                descriptions: flag(d.descriptions),
                comment: flag(d.comment),
                other: flag(d.karaoke) || flag(d.clean_effects),
            };
            match &stream.stream {
                // Cover art is stored as a one-frame video stream; the
                // first real one is the picture.
                StreamKinds::Video(v) if video.is_none() && !flag(d.attached_pic) => {
                    video = Some(VideoStream {
                        index,
                        codec: v.codec_name.clone(),
                        profile: v.profile.clone(),
                        // ffprobe says -99 for a level it does not know.
                        level: u32::try_from(v.level).ok().filter(|&l| l > 0),
                        width: dimension(v.width),
                        height: dimension(v.height),
                        pixel_format: v.pix_fmt.clone(),
                        color_transfer: v.color_transfer.clone(),
                        frame_rate_millis: frame_rate(&stream.avg_frame_rate, &stream.r_frame_rate),
                        bit_depth: bit_depth(v.bits_per_raw_sample, v.pix_fmt.as_deref()),
                        hdr: hdr(
                            v.color_transfer.as_deref(),
                            stream.side_data_list.iter().any(|d| d.dv_profile.is_some()),
                        ),
                    });
                }
                StreamKinds::Audio(a) => {
                    let tags = a.tags.as_ref().map(|t| &t.tags);
                    let title = tags.and_then(|t| t.title.clone());
                    audio.push(AudioStream {
                        index,
                        codec: a.codec_name.clone(),
                        profile: a.profile.clone(),
                        channels: u32::try_from(a.channels).ok().filter(|&c| c > 0),
                        channel_layout: a.channel_layout.clone(),
                        language: language(tags.and_then(|t| t.language.as_deref())),
                        role: roles::audio_role(flags, title.as_deref()),
                        title,
                        default: flag(d.default),
                    });
                }
                StreamKinds::Subtitle(s) => {
                    let tags = s.tags.as_ref().map(|t| &t.tags);
                    let title = tags.and_then(|t| t.title.clone());
                    let role = roles::subtitle_role(flags, title.as_deref());
                    subtitles.push(SubtitleStream {
                        source: SubtitleSource::Embedded { index },
                        kind: SubtitleKind::of_codec(&s.codec_name),
                        codec: s.codec_name.clone(),
                        language: language(tags.and_then(|t| t.language.as_deref())),
                        title,
                        forced: role == SubtitleRole::Forced,
                        default: flag(d.default),
                        role,
                    });
                }
                _ => {}
            }
        }
        Self {
            container: probe.format.format_name.clone(),
            duration: probe.format.duration,
            bit_rate: probe.format.bit_rate.and_then(|b| u64::try_from(b).ok()),
            video,
            audio,
            subtitles,
        }
    }

    /// The part of the descriptor a client sees.
    pub fn tracks(&self) -> MediaTracks {
        MediaTracks {
            duration: self.duration,
            container: Some(codecs::container(&self.container)),
            video: self.video.as_ref().map(|v| VideoTrack {
                codec: codecs::video_codec(&v.codec),
                width: v.width,
                height: v.height,
                bit_depth: v.bit_depth,
                hdr: v.hdr,
            }),
            audio: self
                .audio
                .iter()
                .map(|a| AudioTrack {
                    language: a.language.clone(),
                    title: a.title.clone(),
                    channels: a.channels,
                    default: a.default,
                    role: a.role,
                })
                .collect(),
            subtitles: self
                .subtitles
                .iter()
                .map(|s| SubtitleTrack {
                    language: s.language.clone(),
                    title: s.title.clone(),
                    forced: s.forced,
                    default: s.default,
                    external: matches!(s.source, SubtitleSource::External { .. }),
                    role: s.role,
                })
                .collect(),
        }
    }
}

/// Whole milliseconds, as everywhere else Spoolframe prints a duration.
fn millis<S: serde::Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
    match d {
        Some(d) => s.serialize_some(&u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
        None => s.serialize_none(),
    }
}

/// The average frame rate, else the real one, in frames per 1000 s.
fn frame_rate(average: &Ratio, real: &Ratio) -> Option<u32> {
    [average, real].into_iter().find_map(|r| {
        let (n, d) = (r.numerator(), r.denominator());
        (n > 0 && d > 0).then(|| u32::try_from(n * 1000 / d).unwrap_or(u32::MAX))
    })
}

/// From the decoder's sample size, else from the pixel format's name
/// (`yuv420p10le`, `p010le`).
fn bit_depth(raw: Option<i64>, pixel_format: Option<&str>) -> u32 {
    if let Some(bits) = raw.and_then(|b| u32::try_from(b).ok()).filter(|&b| b > 0) {
        return bits;
    }
    match pixel_format {
        Some(f) if f.contains("12") => 12,
        Some(f) if f.contains("10") => 10,
        _ => 8,
    }
}

/// Dolby Vision is in the stream's side data; HDR10 and HLG are transfer
/// functions. A Dolby Vision file with an HDR10 base layer is Dolby
/// Vision: that is the richer format to show where it can be.
fn hdr(transfer: Option<&str>, dolby_vision: bool) -> Option<HdrFormat> {
    if dolby_vision {
        return Some(HdrFormat::DolbyVision);
    }
    match transfer {
        Some("smpte2084") => Some(HdrFormat::Hdr10),
        Some("arib-std-b67") => Some(HdrFormat::Hlg),
        _ => None,
    }
}

fn flag(value: i64) -> bool {
    value != 0
}

fn dimension(value: i64) -> u32 {
    u32::try_from(value).unwrap_or(0)
}

/// `und` (undetermined) is how files say "no language".
fn language(tag: Option<&str>) -> Option<String> {
    tag.filter(|l| !l.is_empty() && *l != "und")
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_types::{Container, VideoCodec};

    fn probe(name: &str) -> FfProbe {
        let path = format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        serde_json::from_slice(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn describe(name: &str) -> MediaDescriptor {
        MediaDescriptor::from_probe(&probe(name))
    }

    #[test]
    fn an_h264_aac_mp4() {
        let d = describe("mp4-h264-aac");
        assert_eq!(d.container, "mov,mp4,m4a,3gp,3g2,mj2");
        let video = d.video.unwrap();
        assert_eq!((video.codec.as_str(), video.width), ("h264", 1920));
        assert_eq!(d.audio.len(), 1);
        assert_eq!(d.audio[0].codec, "aac");
        assert_eq!(d.audio[0].language.as_deref(), Some("eng"));
        assert!(d.subtitles.is_empty());
        assert!(d.duration.unwrap() > Duration::from_secs(6600));
    }

    #[test]
    fn an_hevc_eac3_mkv_lists_every_audio_track() {
        let d = describe("mkv-hevc-eac3");
        assert_eq!(d.container, "matroska,webm");
        assert_eq!(d.video.unwrap().codec, "hevc");
        let codecs: Vec<_> = d.audio.iter().map(|a| a.codec.as_str()).collect();
        assert_eq!(codecs, ["eac3", "eac3", "ac3", "eac3"]);
        assert_eq!(d.audio[1].language.as_deref(), Some("chi"));
    }

    #[test]
    fn pgs_subtitles_are_bitmaps_and_subrip_is_text() {
        let d = describe("mkv-hevc-pgs");
        let pgs = d
            .subtitles
            .iter()
            .find(|s| s.codec == "hdmv_pgs_subtitle")
            .unwrap();
        assert_eq!(pgs.kind, SubtitleKind::Bitmap);
        let srt = d.subtitles.iter().find(|s| s.codec == "subrip").unwrap();
        assert_eq!(srt.kind, SubtitleKind::Text);
    }

    #[test]
    fn embedded_subtitles_keep_their_stream_index() {
        let d = describe("mkv-hevc-pgs");
        let SubtitleSource::Embedded { index } = d.subtitles[0].source else {
            panic!("not embedded")
        };
        assert!(index > 0);
    }

    #[test]
    fn hdr_bit_depth_and_frame_rate_are_read() {
        // A 4K HDR10 HEVC remux at 23.976 fps.
        let hdr = describe("mkv-cover-art").video.unwrap();
        assert_eq!(hdr.hdr, Some(HdrFormat::Hdr10));
        assert_eq!(hdr.bit_depth, 10);
        assert_eq!(hdr.frame_rate_millis, Some(23_976));
        // An SDR H.264 web release.
        let sdr = describe("b99-s07e01").video.unwrap();
        assert_eq!(sdr.hdr, None);
        assert_eq!(sdr.bit_depth, 8);
    }

    #[test]
    fn the_level_and_the_range_are_read() {
        // HEVC Main 10 at level 5.1, PQ.
        let hdr = describe("mkv-cover-art").video.unwrap();
        assert_eq!(hdr.level, Some(153));
        assert_eq!(hdr.range(), VideoRange::Pq);
        let mut sdr = describe("b99-s07e01").video.unwrap();
        assert_eq!(sdr.level, Some(40));
        assert_eq!(sdr.range(), VideoRange::Sdr);
        sdr.color_transfer = Some("arib-std-b67".into());
        assert_eq!(sdr.range(), VideoRange::Hlg);
    }

    #[test]
    fn dolby_vision_wins_over_its_hdr10_base_layer() {
        assert_eq!(hdr(Some("smpte2084"), true), Some(HdrFormat::DolbyVision));
        assert_eq!(hdr(Some("arib-std-b67"), false), Some(HdrFormat::Hlg));
        assert_eq!(hdr(Some("bt709"), false), None);
    }

    #[test]
    fn cover_art_is_not_the_video() {
        // The cover is a one-frame mjpeg stream at the end of the file.
        let d = describe("mkv-cover-art");
        assert_eq!(d.video.unwrap().codec, "hevc");
    }

    #[test]
    fn a_forced_subtitle_is_marked() {
        let d = describe("mkv-forced-subs");
        let forced: Vec<_> = d.subtitles.iter().filter(|s| s.forced).collect();
        assert_eq!(forced.len(), 1);
        assert_eq!(forced[0].language.as_deref(), Some("eng"));
    }

    #[test]
    fn a_flac_track_is_audio_only() {
        let d = describe("flac-track");
        assert_eq!(d.container, "flac");
        assert!(d.video.is_none());
        assert_eq!(d.audio[0].codec, "flac");
        assert_eq!(d.audio[0].channels, Some(2));
    }

    #[test]
    fn the_descriptor_prints_its_duration_in_milliseconds() {
        let json = serde_json::to_value(describe("flac-track")).unwrap();
        assert_eq!(json["duration"], 257_386);
    }

    #[test]
    fn an_undetermined_language_is_none() {
        assert_eq!(language(Some("und")), None);
        assert_eq!(language(Some("eng")).as_deref(), Some("eng"));
    }

    #[test]
    fn standalone_subtitle_files_parse() {
        // Upstream easy-ffprobe fails on these (no start_pts); the vendored
        // copy must not.
        for (name, codec) in [
            ("external-srt", "subrip"),
            ("external-vtt", "webvtt"),
            ("external-vobsub", "dvd_subtitle"),
        ] {
            assert_eq!(describe(name).subtitles[0].codec, codec, "{name}");
        }
    }

    #[test]
    fn brooklyn_nine_nine_s07_roles_come_from_titles() {
        // No disposition flags at all: the titles say it.
        let d = describe("b99-s07e01");
        let roles: Vec<_> = d.audio.iter().map(|a| a.role).collect();
        assert_eq!(roles[1], AudioRole::AudioDescription);
        assert!(
            roles
                .iter()
                .enumerate()
                .all(|(i, r)| i == 1 || *r == AudioRole::Main)
        );
        let sdh: Vec<_> = d
            .subtitles
            .iter()
            .filter(|s| s.role == SubtitleRole::Sdh)
            .collect();
        assert_eq!(sdh.len(), 1);
        assert_eq!(sdh[0].language.as_deref(), Some("eng"));
        let forced = d.subtitles.iter().filter(|s| s.forced).count();
        assert_eq!(forced, 9);
    }

    #[test]
    fn the_client_sees_the_file_s_container_and_picture() {
        let dv = describe("mkv-forced-subs").tracks();
        assert_eq!(dv.container, Some(Container::Matroska));
        assert_eq!(
            dv.video,
            Some(VideoTrack {
                codec: VideoCodec::Hevc,
                width: 3840,
                height: 1920,
                bit_depth: 10,
                hdr: Some(HdrFormat::DolbyVision),
            })
        );
        let hdr10 = describe("mkv-cover-art").tracks().video.unwrap();
        assert_eq!((hdr10.height, hdr10.hdr), (2160, Some(HdrFormat::Hdr10)));
        let sdr = describe("mp4-h264-aac").tracks();
        assert_eq!(sdr.container, Some(Container::Mp4));
        assert_eq!(
            sdr.video,
            Some(VideoTrack {
                codec: VideoCodec::H264,
                width: 1920,
                height: 1036,
                bit_depth: 8,
                hdr: None,
            })
        );
    }

    #[test]
    fn music_has_a_container_and_no_picture() {
        let flac = describe("flac-track").tracks();
        assert_eq!((flac.container, flac.video), (Some(Container::Flac), None));
    }

    #[test]
    fn the_client_sees_tracks_but_not_codecs() {
        let tracks = describe("mkv-forced-subs").tracks();
        assert_eq!(tracks.audio.len(), 1);
        assert_eq!(tracks.audio[0].channels, Some(6));
        assert!(tracks.subtitles.iter().any(|s| s.forced));
        assert!(tracks.subtitles.iter().all(|s| !s.external));
        let json = serde_json::to_string(&tracks).unwrap();
        assert!(!json.contains("eac3"), "{json}");
    }
}
