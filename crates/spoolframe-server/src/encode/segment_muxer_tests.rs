//! A copied video cut by ffmpeg's segment muxer at the conversion's own
//! list (review 2026-10-11 M2), with real ffmpeg and ffprobe, on films
//! made at test time: each segment ffmpeg writes holds the source frames
//! the playlist says, from the start and from a resume alike.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use spoolframe_media::encoders::{Encoders, ToneMapSetting};
use spoolframe_media::sandbox::Tool;
use spoolframe_media::{KeyframeTimes, MediaDescriptor, Prober};
use spoolframe_protocol::{AudioAction, ConvertedAudio, ConvertedVideo};
use spoolframe_types::{AudioCodec, VideoCodec};

use super::args::{EncodeSpec, Output, ffmpeg_args};
use super::hls::Segments;
use super::job::listed_segments;
use super::{header_first, keyframe_cuts, run_at};

const COPY: ConvertedVideo = ConvertedVideo::Copy {
    codec: VideoCodec::H264,
    dolby_vision: false,
};

const AAC: ConvertedAudio = ConvertedAudio {
    index: 0,
    action: AudioAction::Copy {
        codec: AudioCodec::Aac,
    },
};

fn ffmpeg(args: &[&str]) {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .expect("ffmpeg runs");
    assert!(
        out.status.success(),
        "ffmpeg {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `secs` of a 23.976 fps test pattern and a tone (AAC) in Matroska, the
/// picture in libx264 with `video` (its keyframes).
fn film(dir: &Path, name: &str, secs: u32, video: &[&str]) -> PathBuf {
    let path = dir.join(name);
    let secs = secs.to_string();
    let mut args = vec![
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=160x90:rate=24000/1001",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000",
        "-t",
        &secs,
        "-pix_fmt",
        "yuv420p",
        "-c:v",
        "libx264",
    ];
    args.extend_from_slice(video);
    args.extend(["-c:a", "aac", "-b:a", "64k", path.to_str().unwrap()]);
    ffmpeg(&args);
    path
}

/// Every keyframe, then every 41.7 ms frame one: all-intra.
fn all_intra(dir: &Path, secs: u32) -> PathBuf {
    film(dir, "intra.mkv", secs, &["-preset", "ultrafast", "-g", "1"])
}

/// The keyframes that showed the HLS muxer's cuts drift on a resume: at
/// 0, 0.5, 3.5, 3.708, 4.125, 4.333, 5.208, 6.083 and 9 s, as near as
/// frames fall; with B-frames, as `medium` makes them, if `b_frames`.
fn uneven(dir: &Path, b_frames: bool) -> PathBuf {
    let preset = if b_frames { "medium" } else { "ultrafast" };
    film(
        dir,
        "uneven.mkv",
        12,
        &[
            "-preset",
            preset,
            "-x264-params",
            "keyint=1000:min-keyint=1000:scenecut=0",
            "-force_key_frames",
            "0,0.5,3.5,3.708,4.125,4.333,5.208,6.083,9",
        ],
    )
}

/// The film as a conversion starting now sees it: its descriptor and
/// keyframes, read by real ffprobe.
async fn read(path: &Path) -> (MediaDescriptor, KeyframeTimes) {
    let prober = Prober::new(Tool::unconfined("ffprobe"), Duration::from_secs(30));
    let descriptor = prober.describe(path).await.unwrap();
    let keyframes = prober.scan_keyframes(path).await.unwrap();
    ((*descriptor).clone(), (*keyframes).clone())
}

/// The film's video packets, by their own time (ms): its frames.
fn frames(path: &Path) -> Vec<(i64, u64)> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "packet=pts_time,size", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    let mut frames: Vec<(i64, u64)> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let (time, size) = line.split_once(',')?;
            let ms = (time.parse::<f64>().ok()? * 1000.0).round() as i64;
            Some((ms, size.parse().ok()?))
        })
        .collect();
    frames.sort_unstable();
    frames
}

/// What each segment holds: its frames' sizes, in order of time.
type Held = Vec<Vec<u64>>;

/// The source's frames as `segments` cut them, from segment `from` on.
fn expected(source: &[(i64, u64)], segments: &Segments, from: u32) -> Held {
    (from..=segments.count())
        .map(|n| {
            let start = if n == 1 {
                i64::MIN
            } else {
                segments.start(n).unwrap() as i64
            };
            let end = segments.start(n + 1).map_or(i64::MAX, |ms| ms as i64);
            source
                .iter()
                .filter(|(t, _)| (start..end).contains(t))
                .map(|&(_, size)| size)
                .collect()
        })
        .collect()
}

/// Run the conversion's run from segment `n` (from 1) of `segments` with
/// real ffmpeg, as `spawn` makes it, into `dir`: the segments it lists as
/// finished, by number, and what each holds.
fn run(
    film: &Path,
    descriptor: &MediaDescriptor,
    segments: &Segments,
    n: u32,
    dir: &Path,
) -> (Vec<u32>, Held) {
    run_as(film, descriptor, segments, n, dir, COPY)
}

/// [`run`] of a rung whose video is `video`: the copy, or an encode under
/// it (a remux ladder's lower rung, PX-21).
fn run_as(
    film: &Path,
    descriptor: &MediaDescriptor,
    segments: &Segments,
    n: u32,
    dir: &Path,
    video: ConvertedVideo,
) -> (Vec<u32>, Held) {
    std::fs::create_dir_all(dir).unwrap();
    let (start, from) = run_at(segments, n, Some(video));
    let encoders = Encoders::software_baseline();
    let spec = EncodeSpec {
        input: film,
        sidecar: None,
        descriptor,
        video: Some(video),
        audio: Some(AAC),
        subtitle: None,
        encoders: &encoders,
        vaapi_device: Path::new(""),
        hardware_decode: false,
        gpu_filters: false,
        tone_map: ToneMapSetting::Auto,
        start,
        output: Output::Hls {
            dir: dir.to_path_buf(),
            start_number: from,
            cuts: keyframe_cuts(Some(COPY), Some(video), Some(segments)),
        },
    };
    let args = ffmpeg_args(&spec).unwrap();
    let muxer = if video == COPY { "segment" } else { "hls" };
    assert!(args.contains(&muxer.to_owned()), "{args:?}");
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    ffmpeg(&refs);
    let list = std::fs::read_to_string(dir.join(super::args::PLAYLIST)).unwrap();
    let numbers: Vec<u32> = listed_segments(&list).iter().map(|&(n, _)| n).collect();
    let init = std::fs::read(dir.join("init.mp4")).unwrap();
    let held = numbers
        .iter()
        .map(|n| {
            let whole = dir.join("whole.mp4");
            let segment = std::fs::read(dir.join(format!("seg{n}.m4s"))).unwrap();
            std::fs::write(&whole, [init.clone(), segment].concat()).unwrap();
            frames(&whole).into_iter().map(|(_, size)| size).collect()
        })
        .collect();
    (numbers, held)
}

/// Whether init + every segment of `numbers` in `dir`, as a player reads
/// them, decodes without one decoder or demuxer error (no `styp`, no identity `elst`: what
/// the segment muxer leaves out against the HLS muxer).
fn decodes(dir: &Path, numbers: &[u32]) -> bool {
    let mut stream = std::fs::read(dir.join("init.mp4")).unwrap();
    for n in numbers {
        stream.extend(std::fs::read(dir.join(format!("seg{n}.m4s"))).unwrap());
    }
    let whole = dir.join("stream.mp4");
    std::fs::write(&whole, stream).unwrap();
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-xerror", "-i"])
        .arg(&whole)
        .args(["-f", "null", "-"])
        .output()
        .unwrap();
    // The null muxer's own complaints about the timestamps it is handed
    // (the HLS muxer's output draws them too) are not the stream's.
    let errors = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|line| !line.starts_with("[null @"))
        .count();
    out.status.success() && errors == 0
}

async fn scratch(test: &str) -> spoolframe_test_dir::TestDir {
    spoolframe_test_dir::TestDir::new(&format!("spoolframe-segment-muxer-{test}")).await
}

/// The segments' lengths, ms.
fn lengths(segments: &Segments) -> Vec<u64> {
    (1..=segments.count())
        .map(|n| segments.end(n).unwrap() - segments.start(n).unwrap())
        .collect()
}

/// An all-intra film at 2 s, the default: ten segments of 48 frames,
/// 2.002 s each, not one per frame; each as ffmpeg wrote it.
#[tokio::test]
async fn an_all_intra_film_is_cut_every_two_seconds() {
    let tmp = scratch("intra-2").await;
    let path = all_intra(tmp.as_ref(), 20);
    let (descriptor, keyframes) = read(&path).await;
    assert!(header_first(&descriptor, Some(AAC)));
    let segments = Segments::copied(&keyframes, 20_000, 2);
    assert_eq!(segments.count(), 10, "{:?}", segments.starts_ms());
    assert!(lengths(&segments)[..9].iter().all(|&ms| ms == 2002));
    let (numbers, held) = run(&path, &descriptor, &segments, 1, &tmp.as_ref().join("run"));
    assert_eq!(numbers, (1..=10).collect::<Vec<_>>());
    assert_eq!(held, expected(&frames(&path), &segments, 1));
    assert!(held[..9].iter().all(|frames| frames.len() == 48));
    assert!(decodes(&tmp.as_ref().join("run"), &numbers));
}

/// At 1 s: twenty segments of 24 frames, another list, also as written.
#[tokio::test]
async fn an_all_intra_film_is_cut_every_second_at_one() {
    let tmp = scratch("intra-1").await;
    let path = all_intra(tmp.as_ref(), 20);
    let (descriptor, keyframes) = read(&path).await;
    let one = Segments::copied(&keyframes, 20_000, 1);
    assert_ne!(
        one.starts_ms(),
        Segments::copied(&keyframes, 20_000, 2).starts_ms()
    );
    assert_eq!(one.count(), 20, "{:?}", one.starts_ms());
    let (numbers, held) = run(&path, &descriptor, &one, 1, &tmp.as_ref().join("run"));
    assert_eq!(numbers, (1..=20).collect::<Vec<_>>());
    assert_eq!(held, expected(&frames(&path), &one, 1));
    assert!(held[..19].iter().all(|frames| frames.len() == 24));
}

/// Uneven keyframes: cut at the first one at least 1 s, or 2 s, past the
/// last cut, as ffmpeg writes them.
#[tokio::test]
async fn uneven_keyframes_are_cut_where_the_list_says() {
    let tmp = scratch("uneven").await;
    let path = uneven(tmp.as_ref(), false);
    let (descriptor, keyframes) = read(&path).await;
    for (secs, cuts) in [(1, 4), (2, 4)] {
        let segments = Segments::copied(&keyframes, 12_000, secs);
        assert_eq!(segments.count(), cuts, "{:?}", segments.starts_ms());
        let dir = tmp.as_ref().join(format!("run-{secs}"));
        let (numbers, held) = run(&path, &descriptor, &segments, 1, &dir);
        assert_eq!(numbers, (1..=cuts).collect::<Vec<_>>(), "{secs} s");
        assert_eq!(held, expected(&frames(&path), &segments, 1), "{secs} s");
        assert!(decodes(&dir, &numbers), "{secs} s");
    }
}

/// Review 2026-10-11 M2: a run resumed at a cut in the middle writes the
/// same segments as the run from the start, frame for frame and number
/// for number: on an all-intra film, and on uneven keyframes with
/// B-frames, where ffmpeg seeks 3/23 s before where it is aimed.
#[tokio::test]
async fn a_resumed_run_cuts_as_the_run_from_the_start() {
    let tmp = scratch("resume").await;
    for (name, path) in [
        ("intra", all_intra(tmp.as_ref(), 20)),
        ("uneven", uneven(tmp.as_ref(), true)),
    ] {
        let (descriptor, keyframes) = read(&path).await;
        for secs in [1, 2] {
            let total = descriptor.duration.unwrap().as_millis() as u64;
            let segments = Segments::copied(&keyframes, total, secs);
            let dir = tmp.as_ref().join(format!("{name}-{secs}"));
            let (all, whole) = run(&path, &descriptor, &segments, 1, &dir.join("from"));
            assert_eq!(all, (1..=segments.count()).collect::<Vec<_>>());
            let mid = segments.count() / 2 + 1;
            let (resumed, held) = run(&path, &descriptor, &segments, mid, &dir.join("mid"));
            assert_eq!(
                resumed,
                (mid..=segments.count()).collect::<Vec<_>>(),
                "{name} at {secs} s, from segment {mid}"
            );
            assert_eq!(
                held,
                whole[mid as usize - 1..],
                "{name} at {secs} s, from segment {mid}: {:?}",
                segments.starts_ms()
            );
        }
    }
}

/// A remux ladder's lower rung (PX-21) is encoded with a keyframe forced
/// at each of the copy's cuts: a player switching to it at a segment in
/// the middle gets the frames the copy's segment of that number holds.
#[tokio::test]
async fn a_rung_switched_to_mid_film_cuts_where_the_copy_does() {
    let tmp = scratch("rung").await;
    let path = uneven(tmp.as_ref(), false);
    let (descriptor, keyframes) = read(&path).await;
    let segments = Segments::copied(&keyframes, 12_000, 1);
    let (_, copy) = run(&path, &descriptor, &segments, 1, &tmp.as_ref().join("copy"));
    let encode = ConvertedVideo::Encode {
        codec: VideoCodec::H264,
        height: 90,
        max_bitrate: None,
        tone_map: false,
    };
    let mid = segments.count() / 2 + 1;
    let dir = tmp.as_ref().join("rung");
    let (numbers, held) = run_as(&path, &descriptor, &segments, mid, &dir, encode);
    assert_eq!(numbers, (mid..=segments.count()).collect::<Vec<_>>());
    let frames_in = |held: &[Vec<u64>]| held.iter().map(Vec::len).collect::<Vec<_>>();
    assert_eq!(frames_in(&held), frames_in(&copy[mid as usize - 1..]));
}
