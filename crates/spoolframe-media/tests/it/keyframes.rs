//! PX-25: a file's keyframes from its index equal every packet's, on
//! files ffmpeg writes when the test runs; where the index is missing or
//! lists only some, every packet is read.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use spoolframe_media::keyframe_index::{self, IndexKind};
use spoolframe_media::sandbox::Tool;
use spoolframe_media::{KeyframesFrom, Prober};

async fn scratch(test: &str) -> spoolframe_test_dir::TestDir {
    spoolframe_test_dir::TestDir::new(&format!("spoolframe-keyframes-{test}")).await
}

fn prober() -> Prober {
    Prober::new(Tool::unconfined("ffprobe"), Duration::from_secs(30))
}

fn ffmpeg(args: &[&str]) {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .expect("ffmpeg runs");
    assert!(
        out.status.success(),
        "ffmpeg {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// 30 s of a test pattern with sound: H.264 with B-frames, keyframes at
/// uneven times (every 1.7 s, and every 37 frames), into `name`.
fn film(dir: &Path, name: &str, extra: &[&str]) -> PathBuf {
    let path = dir.join(name);
    let mut args = vec![
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=160x90:rate=25",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000",
        "-t",
        "30",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-bf",
        "2",
        "-g",
        "37",
        "-force_key_frames",
        "expr:gte(t,n_forced*1.7)",
        "-c:a",
        "aac",
    ];
    args.extend_from_slice(extra);
    args.push(path.to_str().unwrap());
    ffmpeg(&args);
    path
}

/// What the prober gives, against every packet's keyframes.
async fn from_and_scan(path: &Path) -> (Vec<u64>, KeyframesFrom, Vec<u64>) {
    let prober = prober();
    let (times, from) = prober.keyframes_from(path).await.unwrap();
    let all = prober.scan_keyframes(path).await.unwrap();
    (times.to_vec(), from, all.to_vec())
}

fn index_of(path: &Path) -> Result<keyframe_index::Indexed, keyframe_index::NoIndex> {
    let mut file = std::fs::File::open(path).unwrap();
    let len = file.metadata().unwrap().len();
    keyframe_index::read(&mut file, len)
}

#[tokio::test]
async fn matroska_cues_are_every_keyframe() {
    let dir = scratch("mkv").await;
    let path = film(dir.as_ref(), "film.mkv", &[]);
    let (times, from, all) = from_and_scan(&path).await;
    assert_eq!(from, KeyframesFrom::Index(IndexKind::Cues));
    assert_eq!(times, all);
    assert!(all.len() > 20, "{all:?}");
}

#[tokio::test]
async fn mp4_sync_samples_are_every_keyframe() {
    let dir = scratch("mp4").await;
    let path = film(dir.as_ref(), "film.mp4", &[]);
    let (times, from, all) = from_and_scan(&path).await;
    assert_eq!(from, KeyframesFrom::Index(IndexKind::SyncSamples));
    assert_eq!(times, all);
}

/// Cut with stream copy just after a keyframe: the file starts at that
/// keyframe, before its time 0, through an edit list.
#[tokio::test]
async fn an_mp4_cut_after_a_keyframe_lists_it_at_zero() {
    let dir = scratch("mp4-cut").await;
    let whole = film(dir.as_ref(), "whole.mp4", &[]);
    let cut = dir.as_ref().join("cut.mp4");
    let (whole, cut_path) = (whole.to_str().unwrap(), cut.to_str().unwrap());
    ffmpeg(&["-ss", "3.44", "-i", whole, "-c", "copy", cut_path]);
    let (times, from, all) = from_and_scan(&cut).await;
    assert_eq!(from, KeyframesFrom::Index(IndexKind::SyncSamples));
    assert_eq!(times, all);
    assert_eq!(times[0], 0);
}

/// The first keyframe's own time, as ffprobe reads its first packet, ms.
fn first_keyframe_ms(path: &Path) -> i64 {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "packet=pts_time,flags", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    let first = text
        .lines()
        .find(|l| l.contains(",K"))
        .and_then(|l| l.split(',').next())
        .and_then(|t| t.parse::<f64>().ok())
        .unwrap();
    (first * 1000.0).round() as i64
}

/// A copy from such a file's start begins at that keyframe, before 0:
/// its time is kept, from the index and from every packet alike, though
/// the list has it at 0.
#[tokio::test]
async fn an_mp4_cut_after_a_keyframe_keeps_where_that_keyframe_is() {
    let dir = scratch("mp4-cut-first").await;
    let whole = film(dir.as_ref(), "whole.mp4", &[]);
    let cut = dir.as_ref().join("cut.mp4");
    let (whole_path, cut_path) = (whole.to_str().unwrap(), cut.to_str().unwrap());
    ffmpeg(&["-ss", "3.44", "-i", whole_path, "-c", "copy", cut_path]);
    let expected = first_keyframe_ms(&cut);
    assert!(expected < 0, "the cut starts at {expected} ms");
    let prober = prober();
    let (indexed, from) = prober.keyframes_from(&cut).await.unwrap();
    assert_eq!(from, KeyframesFrom::Index(IndexKind::SyncSamples));
    assert_eq!(indexed.first_ms, expected);
    assert_eq!(
        prober.scan_keyframes(&cut).await.unwrap().first_ms,
        expected
    );
    // A file from 0 starts at 0.
    let (uncut, _) = prober.keyframes_from(&whole).await.unwrap();
    assert_eq!(uncut.first_ms, 0);
}

/// Keyed every 2 s and cut 40 ms past one, the MP4 keeps an earlier
/// keyframe too, which ffmpeg drops: the copy starts at the last one
/// before 0, at -0.04 s, not at the first in the file.
#[tokio::test]
async fn an_mp4_cut_keeps_the_keyframe_a_copy_starts_at_not_one_before_it() {
    let dir = scratch("mp4-cut-two").await;
    let whole = dir.as_ref().join("whole.mkv");
    let cut = dir.as_ref().join("cut.mp4");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=160x90:rate=25",
        "-t",
        "12",
        "-pix_fmt",
        "yuv420p",
        "-g",
        "50",
        "-sc_threshold",
        "0",
        "-c:v",
        "libx264",
        whole.to_str().unwrap(),
    ]);
    let (whole_path, cut_path) = (whole.to_str().unwrap(), cut.to_str().unwrap());
    ffmpeg(&[
        "-ss", "2.04", "-i", whole_path, "-c", "copy", "-f", "mp4", cut_path,
    ]);
    let expected = first_keyframe_ms(&cut);
    assert_eq!(expected, -40);
    let prober = prober();
    let (indexed, from) = prober.keyframes_from(&cut).await.unwrap();
    assert_eq!(from, KeyframesFrom::Index(IndexKind::SyncSamples));
    assert_eq!(indexed.first_ms, expected);
    assert_eq!(
        prober.scan_keyframes(&cut).await.unwrap().first_ms,
        expected
    );
}

/// Written to a pipe, the Matroska muxer cannot go back for an index.
#[tokio::test]
async fn a_matroska_file_without_cues_is_read_whole() {
    let dir = scratch("mkv-no-cues").await;
    let path = film(dir.as_ref(), "film.mkv", &["-live", "1"]);
    assert!(index_of(&path).is_err(), "{:?}", index_of(&path));
    let (times, from, all) = from_and_scan(&path).await;
    assert!(matches!(from, KeyframesFrom::Scan { .. }), "{from:?}");
    assert_eq!(times, all);
}

/// Every other `CuePoint` (ID 0xBB) made a `Void` (0xEC) of the same
/// size: an index of half the keyframes, as a muxer that indexes some of
/// them writes.
fn drop_every_other_cue(path: &Path) {
    let mut bytes = std::fs::read(path).unwrap();
    let cues = bytes
        .windows(4)
        .rposition(|w| w == [0x1C, 0x53, 0xBB, 0x6B])
        .expect("Cues");
    // The Cues' size, then its CuePoints one after another.
    let (size_len, size) = vint(&bytes[cues + 4..]);
    let (start, end) = (cues + 4 + size_len, cues + 4 + size_len + size as usize);
    let mut at = start;
    let mut n = 0;
    while at < end {
        // A CuePoint, or ffmpeg's CRC-32 (0xBF) first: both one-byte IDs.
        let (len, size) = vint(&bytes[at + 1..]);
        if bytes[at] == 0xBB {
            if n % 2 == 1 {
                bytes[at] = 0xEC;
            }
            n += 1;
        }
        at += 1 + len + size as usize;
    }
    assert!(n > 10, "{n} CuePoints");
    std::fs::write(path, bytes).unwrap();
}

/// An EBML size: its length and value.
fn vint(bytes: &[u8]) -> (usize, u64) {
    let len = bytes[0].leading_zeros() as usize + 1;
    let mut value = u64::from(bytes[0]) & (0xFF >> len);
    for &b in &bytes[1..len] {
        value = (value << 8) | u64::from(b);
    }
    (len, value)
}

#[tokio::test]
async fn cues_on_only_some_keyframes_are_not_used() {
    let dir = scratch("mkv-some-cues").await;
    let path = film(dir.as_ref(), "film.mkv", &[]);
    drop_every_other_cue(&path);
    let index = index_of(&path).unwrap();
    let (times, from, all) = from_and_scan(&path).await;
    assert!(index.times_ms.len() < all.len(), "{index:?}");
    // Read, and found wanting by the check.
    assert!(
        matches!(&from, KeyframesFrom::Scan { why } if why.contains("the packets between")),
        "{from:?}"
    );
    assert_eq!(times, all);
}

/// Two callers asking for one file's keyframes at once share one read of
/// it: a scan reads the whole file, minutes on a network mount, and a
/// second start of the same film ran another. Review 2026-10-11 M33.
#[tokio::test]
async fn two_callers_at_once_share_one_scan() {
    let tmp = scratch("one-scan").await;
    let dir = tmp.as_ref();
    let scans = dir.join("scans");
    let ffprobe = dir.join("ffprobe");
    std::fs::write(
        &ffprobe,
        format!(
            "#!/bin/sh\necho scan >> '{}'\nsleep 0.5\necho 0.000000,K__\necho 2.000000,K__\n",
            scans.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        &ffprobe,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    // No index the reader follows: every packet is read.
    let film = dir.join("film.ts");
    std::fs::write(&film, b"not a container with an index").unwrap();
    let prober = Prober::new(Tool::unconfined(&ffprobe), Duration::from_secs(30));
    let (a, b) = tokio::join!(prober.keyframes(&film), prober.keyframes(&film));
    assert_eq!(a.unwrap().times_ms, [0, 2000]);
    assert_eq!(b.unwrap().times_ms, [0, 2000]);
    let ran = std::fs::read_to_string(&scans).unwrap();
    assert_eq!(ran.lines().count(), 1, "{ran}");
}
