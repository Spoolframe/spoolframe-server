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
