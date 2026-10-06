//! Bitmap subtitles read back as text with real ffmpeg and tesseract: text
//! drawn by ffmpeg, written as a PGS track (`pgs::fixture`), muxed into
//! Matroska, then copied out, parsed and read as the server does it; and
//! the same as VobSub and DVB (`samples`), at a DVD's size.

use std::path::{Path, PathBuf};
use std::process::Command;

use spoolframe_media::ocr::{self, Ocr};
use spoolframe_media::pgs::{self, fixture};
use spoolframe_media::{dvb, vobsub};
use spoolframe_types::SubtitleFormat;

use crate::samples;

const WIDTH: u16 = 640;
const HEIGHT: u16 = 64;

/// A directory of the test's own under a random name, removed when
/// dropped, so runs at once never share one.
async fn scratch(test: &str) -> spoolframe_test_dir::TestDir {
    spoolframe_test_dir::TestDir::new(&format!("spoolframe-ocr-{test}")).await
}

fn run(cmd: &mut Command) -> Vec<u8> {
    let out = cmd.output().expect("the program runs");
    assert!(
        out.status.success(),
        "{cmd:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// `text` as subtitles draw it, white on nothing: a mask of the pixels
/// ffmpeg's drawtext lights.
fn drawn(text: &str) -> Vec<u8> {
    // The dev shell names a font file: fontconfig has none to find there.
    let font =
        std::env::var("SPOOLFRAME_TEST_FONT").expect("the dev shell sets SPOOLFRAME_TEST_FONT");
    let grey = run(Command::new("ffmpeg")
        .args(["-v", "error", "-f", "lavfi", "-i"])
        .arg(format!("color=black:s={WIDTH}x{HEIGHT}:d=1"))
        .arg("-vf")
        .arg(format!(
            "drawtext=fontfile={font}:text='{text}':fontcolor=white:fontsize=40:x=12:y=10"
        ))
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "gray", "-"]));
    assert_eq!(grey.len(), usize::from(WIDTH) * usize::from(HEIGHT));
    grey.into_iter().map(|p| u8::from(p > 128)).collect()
}

fn shown(start_ms: u64, end_ms: u64, text: &str) -> fixture::Cue {
    fixture::Cue {
        start_ms,
        end_ms,
        x: 640,
        y: 960,
        width: WIDTH,
        height: HEIGHT,
        mask: drawn(text),
    }
}

/// A 10 s Matroska file: a test picture, and the cues as its PGS track
/// (stream 1). `-copyts` keeps the cues' own times: without it ffmpeg
/// moves the track back by its first cue's time.
fn movie(dir: &Path, cues: &[fixture::Cue]) -> PathBuf {
    let sup = dir.join("subs.sup");
    std::fs::write(&sup, fixture::sup(cues, 65_000)).unwrap();
    let movie = dir.join("movie.mkv");
    run(Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=320x180:d=10",
            "-i",
        ])
        .arg(&sup)
        .args([
            "-map", "0", "-map", "1", "-c:v", "libx264", "-c:s", "copy", "-copyts",
        ])
        .arg(&movie));
    movie
}

#[tokio::test]
async fn a_pgs_track_reads_back_as_its_text_at_its_times() {
    let tmp = scratch("reads-back").await;
    let dir = tmp.as_ref();
    let movie = movie(
        dir,
        &[
            shown(1_000, 3_500, "Hello there"),
            shown(5_000, 7_250, "General Kenobi"),
        ],
    );
    let sup = dir.join("extracted.sup");
    ocr::extract_sup(&crate::sandbox::confined("ffmpeg"), &movie, 1, &sup)
        .await
        .unwrap();
    let cues = pgs::cues(&std::fs::read(&sup).unwrap()).unwrap();

    let ocr = Ocr::detect(&crate::sandbox::confined("tesseract")).await;
    let language = ocr
        .language_for(Some("eng"))
        .expect("the dev shell has English data");
    let done = std::sync::Mutex::new(Vec::new());
    let texts = ocr
        .recognise(&cues, &language, &dir.join("work"), 2, &|n| {
            done.lock().unwrap().push(n)
        })
        .await
        .unwrap();
    let read = ocr::text_cues(&cues, texts);
    let got: Vec<_> = read
        .iter()
        .map(|c| (c.start_ms, c.end_ms, c.text.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            (1_000, 3_500, "Hello there"),
            (5_000, 7_250, "General Kenobi")
        ]
    );
    assert_eq!(done.into_inner().unwrap().last(), Some(&2));
}

/// What VobSub and DVB samples show. VobSub's stop is in 1024/90000 s,
/// so the lengths are multiples of 512 ms, which DVB keeps as well.
const SHOWN: [samples::Shown; 2] = [
    ("Hello there", 1_000, 3_560),
    ("General Kenobi", 5_000, 7_048),
];

/// The text tesseract reads in `cues`, with their times.
async fn read(cues: &[spoolframe_media::bitmap::Cue], dir: &Path) -> Vec<(u64, u64, String)> {
    let ocr = Ocr::detect(&crate::sandbox::confined("tesseract")).await;
    let texts = ocr
        .recognise(cues, "eng", &dir.join("work"), 2, &|_| {})
        .await
        .unwrap();
    ocr::text_cues(cues, texts)
        .into_iter()
        .map(|c| (c.start_ms, c.end_ms, c.text))
        .collect()
}

fn expected() -> Vec<(u64, u64, String)> {
    SHOWN
        .iter()
        .map(|&(text, start, end)| (start, end, text.to_owned()))
        .collect()
}

#[tokio::test]
async fn vobsub_tracks_read_back_as_their_text_at_their_times() {
    let tmp = scratch("vobsub").await;
    let dir = tmp.as_ref();
    let movie = samples::movie(dir, "movie.mkv", "dvdsub", &SHOWN);
    let idx = samples::vobsub_pair(dir, &SHOWN);
    for (input, index, from_start) in [(&movie, 1, true), (&idx, 0, false)] {
        let track = ocr::read_track(
            &crate::sandbox::confined("ffprobe"),
            input,
            index,
            from_start,
        )
        .await
        .unwrap();
        let cues = vobsub::cues(&track.extradata, &track.packets).unwrap();
        assert_eq!(read(&cues, dir).await, expected(), "{}", input.display());
    }
}

#[tokio::test]
async fn dvb_tracks_read_back_as_their_text_at_their_times() {
    let tmp = scratch("dvb").await;
    let dir = tmp.as_ref();
    for name in ["movie.mkv", "movie.ts"] {
        let movie = samples::movie(dir, name, "dvbsub", &SHOWN);
        let track = ocr::read_track(&crate::sandbox::confined("ffprobe"), &movie, 1, true)
            .await
            .unwrap();
        let cues = dvb::cues(&track.packets);
        assert_eq!(read(&cues, dir).await, expected(), "{name}");
    }
}

#[tokio::test]
async fn more_cues_than_one_run_takes_come_back_in_order() {
    // 90 cues: three tesseract runs of up to 40, two at a time. Every
    // fourth is a dot with no text in it, which must not shift the text of
    // the cues after it.
    let tmp = scratch("in-order").await;
    let dir = tmp.as_ref();
    let words = ["alpha", "bravo", "charlie", ""];
    let fixtures: Vec<_> = (0..90u64)
        .map(|i| {
            let mut cue = shown(i * 1_000, i * 1_000 + 800, words[(i % 4) as usize]);
            if i % 4 == 3 {
                cue.mask[30 * usize::from(WIDTH) + 300] = 1;
            }
            cue
        })
        .collect();
    let cues = pgs::cues(&fixture::sup(&fixtures, 65_000)).unwrap();
    let ocr = Ocr::detect(&crate::sandbox::confined("tesseract")).await;
    let texts = ocr
        .recognise(&cues, "eng", &dir.join("work"), 2, &|_| {})
        .await
        .unwrap();
    let expected: Vec<_> = (0..90).map(|i| words[i % 4]).collect();
    assert_eq!(texts, expected);
}

#[tokio::test]
async fn without_tesseract_ocr_is_off() {
    let ocr = Ocr::detect(&crate::sandbox::confined("/nonexistent/tesseract")).await;
    assert!(ocr.languages().is_empty());
    assert_eq!(ocr.language_for(None), None);
}

#[tokio::test]
async fn a_language_without_data_fails_with_tesseract_s_reason() {
    let tmp = scratch("no-data").await;
    let dir = tmp.as_ref();
    let cues = pgs::cues(&fixture::sup(&[shown(0, 1_000, "hi")], 65_000)).unwrap();
    let ocr = Ocr::detect(&crate::sandbox::confined("tesseract")).await;
    let err = ocr
        .recognise(&cues, "xyz", &dir.join("work"), 1, &|_| {})
        .await
        .unwrap_err();
    assert!(matches!(err, ocr::OcrError::Failed { .. }), "{err}");
}

/// Cues in SubRip, at times a conversion must keep.
const SRT: &str = "1\n00:00:01,000 --> 00:00:03,500\nHello there\n\n\
                   2\n00:00:05,250 --> 00:00:07,000\nGeneral Kenobi\n\n";

/// The cue times of a WebVTT file, in order, in milliseconds. WebVTT may
/// leave out the hours (`01:02.500`), as ffmpeg does under an hour.
fn vtt_times(vtt: &str) -> Vec<(u64, u64)> {
    assert!(vtt.starts_with("WEBVTT"), "{vtt}");
    let ms = |t: &str| {
        let (clock, millis) = t.split_once('.').expect("milliseconds");
        let secs = clock
            .split(':')
            .fold(0u64, |acc, part| acc * 60 + part.parse::<u64>().unwrap());
        secs * 1000 + millis.parse::<u64>().unwrap()
    };
    vtt.lines()
        .filter_map(|l| l.split_once(" --> "))
        .map(|(start, end)| (ms(start), ms(end.split_whitespace().next().unwrap())))
        .collect()
}

#[tokio::test]
async fn subrip_ass_and_mov_text_become_webvtt_with_the_same_times() {
    let tmp = scratch("convert").await;
    let dir = tmp.as_ref();
    let srt = dir.join("subs.srt");
    std::fs::write(&srt, SRT).unwrap();
    // The same cues as ASS, and as mov_text inside an MP4 (stream 1).
    let ass = dir.join("subs.ass");
    run(Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&srt)
        .arg(&ass));
    let mp4 = dir.join("movie.mp4");
    run(Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=160x90:d=8",
            "-i",
        ])
        .arg(&srt)
        .args([
            "-map", "0", "-map", "1", "-c:v", "libx264", "-c:s", "mov_text",
        ])
        .arg(&mp4));

    let expected = [(1_000, 3_500), (5_250, 7_000)];
    for (input, index) in [(&srt, 0), (&ass, 0), (&mp4, 1)] {
        let out = dir.join("out.vtt");
        ocr::convert_text(
            &crate::sandbox::confined("ffmpeg"),
            input,
            index,
            SubtitleFormat::WebVtt,
            &out,
        )
        .await
        .unwrap();
        let vtt = std::fs::read_to_string(&out).unwrap();
        assert_eq!(vtt_times(&vtt), expected, "{}: {vtt}", input.display());
        assert!(vtt.contains("General Kenobi"), "{vtt}");
    }
}
