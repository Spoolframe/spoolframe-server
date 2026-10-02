//! ffprobe's names → the shared codec vocabulary (`spoolframe-types`).
//!
//! A name without an entry is `Unknown`, which no client supports: an
//! unlisted format makes a plan say "not supported" instead of guessing.

use spoolframe_types::{AudioCodec, Container, SubtitleFormat, VideoCodec};

pub fn video_codec(ffprobe: &str) -> VideoCodec {
    match ffprobe {
        "h264" => VideoCodec::H264,
        "hevc" => VideoCodec::Hevc,
        "av1" => VideoCodec::Av1,
        "vp9" => VideoCodec::Vp9,
        "vp8" => VideoCodec::Vp8,
        "mpeg2video" => VideoCodec::Mpeg2,
        "mpeg4" => VideoCodec::Mpeg4,
        "vc1" => VideoCodec::Vc1,
        _ => VideoCodec::Unknown,
    }
}

pub fn audio_codec(ffprobe: &str) -> AudioCodec {
    match ffprobe {
        "aac" => AudioCodec::Aac,
        "ac3" => AudioCodec::Ac3,
        "eac3" => AudioCodec::Eac3,
        "dts" => AudioCodec::Dts,
        "truehd" => AudioCodec::TrueHd,
        "flac" => AudioCodec::Flac,
        "alac" => AudioCodec::Alac,
        "opus" => AudioCodec::Opus,
        "vorbis" => AudioCodec::Vorbis,
        "mp3" => AudioCodec::Mp3,
        pcm if pcm.starts_with("pcm_") => AudioCodec::Pcm,
        _ => AudioCodec::Unknown,
    }
}

/// ffprobe reports a container as a list of the demuxer's names
/// (`matroska,webm`, `mov,mp4,m4a,3gp,3g2,mj2`).
pub fn container(ffprobe: &str) -> Container {
    match ffprobe {
        "matroska,webm" | "matroska" | "webm" => Container::Matroska,
        "mov,mp4,m4a,3gp,3g2,mj2" | "mp4" | "mov" => Container::Mp4,
        "mpegts" => Container::MpegTs,
        "avi" => Container::Avi,
        "flac" => Container::Flac,
        "mp3" => Container::Mp3,
        "ogg" => Container::Ogg,
        _ => Container::Unknown,
    }
}

pub fn subtitle_format(ffprobe: &str) -> SubtitleFormat {
    match ffprobe {
        "subrip" | "srt" => SubtitleFormat::Srt,
        "ass" | "ssa" => SubtitleFormat::Ass,
        "webvtt" => SubtitleFormat::WebVtt,
        "mov_text" => SubtitleFormat::MovText,
        "hdmv_pgs_subtitle" => SubtitleFormat::Pgs,
        "dvd_subtitle" => SubtitleFormat::VobSub,
        "dvb_subtitle" => SubtitleFormat::DvbSub,
        _ => SubtitleFormat::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MediaDescriptor;

    fn fixtures() -> Vec<(String, MediaDescriptor)> {
        let dir = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let probe = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            out.push((
                path.file_name().unwrap().to_string_lossy().into_owned(),
                MediaDescriptor::from_probe(&probe),
            ));
        }
        out
    }

    #[test]
    fn every_codec_in_the_recorded_media_is_known() {
        for (name, d) in fixtures() {
            if let Some(v) = &d.video {
                assert_ne!(
                    video_codec(&v.codec),
                    VideoCodec::Unknown,
                    "{name}: {}",
                    v.codec
                );
            }
            for a in &d.audio {
                assert_ne!(
                    audio_codec(&a.codec),
                    AudioCodec::Unknown,
                    "{name}: {}",
                    a.codec
                );
            }
            for s in &d.subtitles {
                assert_ne!(
                    subtitle_format(&s.codec),
                    SubtitleFormat::Unknown,
                    "{name}: {}",
                    s.codec
                );
            }
        }
    }

    #[test]
    fn every_media_container_in_the_recordings_is_known() {
        for (name, d) in fixtures() {
            // Standalone subtitle files are not media containers.
            if name.starts_with("external-") {
                continue;
            }
            assert_ne!(container(&d.container), Container::Unknown, "{name}");
        }
    }

    #[test]
    fn an_unlisted_name_is_unknown() {
        assert_eq!(video_codec("prores"), VideoCodec::Unknown);
        assert_eq!(audio_codec("wmav2"), AudioCodec::Unknown);
        assert_eq!(container("asf"), Container::Unknown);
        assert_eq!(subtitle_format("eia_608"), SubtitleFormat::Unknown);
    }

    #[test]
    fn every_pcm_flavour_is_pcm() {
        assert_eq!(audio_codec("pcm_s16be"), AudioCodec::Pcm);
        assert_eq!(audio_codec("pcm_s24le"), AudioCodec::Pcm);
    }
}
