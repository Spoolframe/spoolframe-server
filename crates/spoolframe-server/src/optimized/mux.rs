//! A copy read whole (PX-37's track store): its media file and its
//! stored tracks muxed into one file, a remux with no encode. PX-38's
//! download writer builds on this.
//!
//! Input 0 is always the media file, then the stored video, then each
//! stored audio track; every `-map` names its input by [`MuxInputs`],
//! which numbers them in that one place. The media file gives what a copy
//! never stores: the tracks kept as they are, the subtitles, the
//! chapters, the attachments and the metadata. A stored track's language,
//! title and flags are its source stream's. Every input keeps its own
//! times (`-copyts`): a track was written with the file's, so the two
//! line up as they did in the file.

use std::path::{Path, PathBuf};

use spoolframe_media::{MediaDescriptor, SubtitleSource};

use super::CopyFiles;
use crate::encode::args::{QUIET_ARGS, s};

/// Text subtitles that are not text ffmpeg can write again: teletext
/// decodes to pictures or needs a library builds often leave out, and
/// closed captions ride in the video. Left out of a mux, so it is made at
/// all.
pub(crate) const UNWRITABLE_TEXT: [&str; 2] = ["dvb_teletext", "eia_608"];

/// What one ffmpeg input of a mux is read for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Read {
    /// The media file.
    Source,
    /// The stored video.
    Video,
    /// The stored audio track standing in for this source stream.
    Audio(u32),
}

/// A mux's inputs in their `-i` order, each with its number: the media
/// file, the stored video, the stored audio tracks by stream.
#[derive(Debug, Clone)]
pub struct MuxInputs {
    inputs: Vec<(Read, PathBuf)>,
}

impl MuxInputs {
    /// `source` (the media file, or the run's link to it) and `files`,
    /// the copy's stored tracks (or the run's links to them).
    pub fn new(source: &Path, files: &CopyFiles) -> Self {
        let mut inputs = vec![(Read::Source, source.to_path_buf())];
        if let Some(video) = &files.video {
            inputs.push((Read::Video, video.clone()));
        }
        for (&stream, path) in &files.audio {
            inputs.push((Read::Audio(stream), path.clone()));
        }
        Self { inputs }
    }

    fn index(&self, read: &Read) -> Option<usize> {
        self.inputs.iter().position(|(r, _)| r == read)
    }

    /// `-map`'s name of the video: the stored one, else the file's
    /// stream `stream`.
    fn video(&self, stream: u32) -> (String, bool) {
        match self.index(&Read::Video) {
            Some(at) => (format!("{at}:0"), true),
            None => (format!("0:{stream}"), false),
        }
    }

    /// `-map`'s name of the file's audio stream `stream`: its stored
    /// track, else the file's own; whether it is stored.
    fn audio(&self, stream: u32) -> (String, bool) {
        match self.index(&Read::Audio(stream)) {
            Some(at) => (format!("{at}:0"), true),
            None => (format!("0:{stream}"), false),
        }
    }

    /// ffmpeg's `-i` arguments, each input keeping its own times.
    fn args(&self) -> Vec<String> {
        let mut args = Vec::new();
        for (_, path) in &self.inputs {
            args.extend([s("-i"), s(path.display())]);
        }
        args
    }
}

/// What a mux writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuxPicks {
    /// The audio tracks, by their position in the descriptor; every one
    /// when `None`.
    pub audio: Option<Vec<u32>>,
    /// The embedded subtitles, by their position in the descriptor; every
    /// one ffmpeg can write when `None`.
    pub subtitles: Option<Vec<u32>>,
}

impl MuxPicks {
    /// Every track.
    pub const ALL: Self = Self {
        audio: None,
        subtitles: None,
    };
}

/// ffmpeg's arguments for a Matroska file of `descriptor`'s media file
/// read through `inputs`, with `picks`' tracks, written to `out`. Every
/// stream is copied; nothing is encoded. The output is bit-exact: the
/// same inputs make the same bytes.
pub fn mux_args(
    descriptor: &MediaDescriptor,
    inputs: &MuxInputs,
    picks: &MuxPicks,
    out: &Path,
) -> Vec<String> {
    let mut args: Vec<String> = QUIET_ARGS.map(s).to_vec();
    args.extend(inputs.args());
    args.push(s("-copyts"));
    // Output streams so far: a stored track's language, title and flags
    // are said for it, its source stream's (ffmpeg copies the others' from
    // the media file by itself; a `-map_metadata:s` would stop that for
    // every stream, attachments' file names too).
    let mut at = 0usize;
    if let Some(video) = &descriptor.video {
        let (map, stored) = inputs.video(video.index);
        args.extend([s("-map"), map]);
        if stored {
            args.extend([format!("-disposition:{at}"), s("default")]);
        }
        at += 1;
    }
    for (position, track) in descriptor.audio.iter().enumerate() {
        if picks
            .audio
            .as_ref()
            .is_some_and(|p| !p.contains(&(position as u32)))
        {
            continue;
        }
        let (map, stored) = inputs.audio(track.index);
        args.extend([s("-map"), map]);
        if stored {
            let tags = [("language", &track.language), ("title", &track.title)];
            for (key, value) in tags {
                if let Some(value) = value {
                    args.extend([format!("-metadata:s:{at}"), format!("{key}={value}")]);
                }
            }
            args.extend([
                format!("-disposition:{at}"),
                s(if track.default { "default" } else { "0" }),
            ]);
        }
        at += 1;
    }
    for (position, track) in descriptor.subtitles.iter().enumerate() {
        let SubtitleSource::Embedded { index } = track.source else {
            continue;
        };
        let picked = match &picks.subtitles {
            Some(p) => p.contains(&(position as u32)),
            None => !UNWRITABLE_TEXT.contains(&track.codec.as_str()),
        };
        if picked {
            args.extend([s("-map"), format!("0:{index}")]);
        }
    }
    // Fonts for ASS, and whatever else the file carries.
    args.extend(["-map", "0:t?"].map(s));
    args.extend(["-map_metadata", "0", "-map_chapters", "0", "-c", "copy"].map(s));
    // Some records the Matroska muxer writes only with it (a Dolby
    // Vision configuration, PGPU-06).
    args.extend(["-strict", "unofficial", "-fflags", "+bitexact"].map(s));
    args.extend([s("-f"), s("matroska"), s(out.display())]);
    args
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use spoolframe_media::{AudioStream, SubtitleKind, SubtitleStream, VideoStream};
    use spoolframe_types::{AudioRole, SubtitleRole};

    use super::*;

    /// An H.264 film: stream 0 video, 1 DTS (English), 2 AC-3, 3 SubRip,
    /// 4 teletext.
    fn film() -> MediaDescriptor {
        let audio = |index, codec: &str, language: &str| AudioStream {
            index,
            codec: codec.into(),
            profile: None,
            channels: Some(2),
            channel_layout: None,
            language: Some(language.into()),
            title: None,
            default: index == 1,
            role: AudioRole::Main,
        };
        let sub = |index, codec: &str| SubtitleStream {
            source: SubtitleSource::Embedded { index },
            kind: SubtitleKind::of_codec(codec),
            codec: codec.into(),
            language: None,
            title: None,
            forced: false,
            default: false,
            role: SubtitleRole::Full,
        };
        MediaDescriptor {
            container: "matroska,webm".into(),
            duration: Some(Duration::from_secs(60)),
            bit_rate: None,
            video: Some(VideoStream {
                index: 0,
                codec: "h264".into(),
                profile: None,
                level: None,
                width: 1920,
                height: 1080,
                pixel_format: None,
                color_transfer: None,
                frame_rate_millis: None,
                bit_depth: 8,
                hdr: None,
                dolby_vision: None,
                bit_rate: None,
                parameter_sets_in_record: true,
            }),
            audio: vec![audio(1, "dts", "eng"), audio(2, "ac3", "fre")],
            subtitles: vec![sub(3, "subrip"), sub(4, "dvb_teletext")],
        }
    }

    /// The film's DTS stored, and its video.
    fn files() -> CopyFiles {
        CopyFiles {
            video: Some("/optimized/v.mkv".into()),
            audio: [(1, PathBuf::from("/optimized/a.mkv"))].into(),
        }
    }

    fn args(picks: &MuxPicks) -> Vec<String> {
        let inputs = MuxInputs::new(Path::new("/media/film.mkv"), &files());
        mux_args(&film(), &inputs, picks, Path::new("/out.mkv"))
    }

    fn has(args: &[String], run: &[&str]) -> bool {
        args.windows(run.len()).any(|w| w == run)
    }

    #[test]
    fn the_film_is_input_0_then_the_video_then_the_audio() {
        assert!(has(
            &args(&MuxPicks::ALL),
            &[
                "-i",
                "/media/film.mkv",
                "-i",
                "/optimized/v.mkv",
                "-i",
                "/optimized/a.mkv"
            ]
        ));
    }

    #[test]
    fn each_stream_is_mapped_from_its_input() {
        let args = args(&MuxPicks::ALL);
        let maps: Vec<&str> = args
            .windows(2)
            .filter(|w| w[0] == "-map")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(maps, ["1:0", "2:0", "0:2", "0:3", "0:t?"]);
    }

    #[test]
    fn a_stored_track_is_named_as_its_source_stream() {
        assert!(has(
            &args(&MuxPicks::ALL),
            &["-metadata:s:1", "language=eng"]
        ));
    }

    #[test]
    fn every_input_keeps_its_own_times() {
        assert!(args(&MuxPicks::ALL).iter().any(|a| a == "-copyts"));
    }

    #[test]
    fn chapters_and_metadata_come_from_the_film() {
        assert!(has(
            &args(&MuxPicks::ALL),
            &["-map_metadata", "0", "-map_chapters", "0"]
        ));
    }

    #[test]
    fn picked_tracks_alone_are_mapped() {
        let picks = MuxPicks {
            audio: Some(vec![1]),
            subtitles: Some(Vec::new()),
        };
        let args = args(&picks);
        let maps: Vec<&str> = args
            .windows(2)
            .filter(|w| w[0] == "-map")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(maps, ["1:0", "0:2", "0:t?"]);
    }

    #[test]
    fn nothing_is_encoded() {
        assert!(has(&args(&MuxPicks::ALL), &["-c", "copy"]));
    }
}
