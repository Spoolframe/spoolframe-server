//! The track store with real ffmpeg and ffprobe (PX-37, the user's
//! decision 2026-10-20): a 4K-style film (HEVC, with DTS and AC-3), its
//! copies' tracks, and copies muxed whole ([`super::super::mux`]).

use sha2::{Digest, Sha256};

use super::*;
use crate::optimized::mux::{MuxInputs, MuxPicks, mux_args};

/// 2 s of HEVC at 25 fps with a stereo DTS track, a stereo AC-3 track, a
/// SubRip subtitle and a chapter; the DTS starting `late` s after the
/// video (0 for none).
fn hevc_film(dir: &Path, late: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let srt = dir.join("hevc.srt");
    std::fs::write(&srt, "1\n00:00:00,500 --> 00:00:01,500\nHello\n").unwrap();
    let chapters = dir.join("chapters.txt");
    std::fs::write(
        &chapters,
        ";FFMETADATA1\ntitle=Alien\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=2000\ntitle=Opening\n",
    )
    .unwrap();
    let (video, dts) = (dir.join("video.mkv"), dir.join("dts.mkv"));
    run_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x180:rate=25",
        "-t",
        "2",
        "-c:v",
        "libx265",
        "-x265-params",
        "log-level=error",
        "-pix_fmt",
        "yuv420p",
        &video.display().to_string(),
    ]);
    run_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000",
        "-t",
        "1.5",
        "-c:a",
        "dca",
        "-strict",
        "-2",
        "-ac",
        "2",
        &dts.display().to_string(),
    ]);
    let path = dir.join(format!("hevc-{late}.mkv"));
    run_ffmpeg(&[
        "-i",
        &video.display().to_string(),
        "-itsoffset",
        late,
        "-i",
        &dts.display().to_string(),
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=880:sample_rate=48000:duration=2",
        "-i",
        &srt.display().to_string(),
        "-i",
        &chapters.display().to_string(),
        "-map",
        "0:v",
        "-map",
        "1:a",
        "-map",
        "2:a",
        "-map",
        "3:s",
        "-map_metadata",
        "4",
        "-map_chapters",
        "4",
        "-c:v",
        "copy",
        "-c:a:0",
        "copy",
        "-c:a:1",
        "ac3",
        "-ac:a:1",
        "2",
        "-c:s",
        "srt",
        "-metadata:s:a:0",
        "language=eng",
        "-metadata:s:a:1",
        "language=fre",
        &path.display().to_string(),
    ]);
    path
}

/// A pool running real ffmpeg, with the encoders it finds, over a film
/// [`hevc_film`] made, its DTS `late`.
async fn real(test: &str, late: &str) -> (Setup, PathBuf) {
    let s = setup_with(test, PathBuf::from("ffmpeg"), 1, 1 << 30).await;
    s.transcoding.detect().await;
    let film = hevc_film(&s.media.parent().unwrap().join(test), late);
    (s, film)
}

/// `copy` asked for and made, or the test fails saying why.
async fn made(s: &Setup, copy: NewCopy) -> CopyRow {
    let row = s.pool.request(copy).await.unwrap();
    eventually("the copy is made or failed", || async {
        matches!(
            s.state(&row.id).await,
            Some(CopyState::Ready | CopyState::Failed)
        )
    })
    .await;
    let row = s.store.optimized_copy(&row.id).await.unwrap().unwrap();
    assert_eq!(row.state, CopyState::Ready, "{:?}", row.failure);
    row
}

/// A download's Apple copy of `film`, its video the film's own.
async fn apple(film: &Path) -> NewCopy {
    asked_for_apple(film, 180).await
}

/// `Mobile`'s copy of `film`.
async fn mobile(film: &Path) -> NewCopy {
    asked(film, "Mobile").await
}

/// Each of `file`'s streams' codec, in order.
fn codecs(file: &Path) -> Vec<String> {
    probe(file, "stream=codec_name")
}

/// What ffprobe says of `entries` in `file`, a line each.
fn probe(file: &Path, entries: &str) -> Vec<String> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", entries, "-of", "csv=p=0"])
        .arg(file)
        .output()
        .expect("ffprobe runs");
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Each stream's start, in seconds.
fn starts(file: &Path) -> Vec<f64> {
    probe(file, "stream=start_time")
        .iter()
        .map(|s| s.trim_end_matches(',').parse().unwrap())
        .collect()
}

/// The MD5 of stream `stream` of `file`'s packets, as they are.
fn packets_md5(file: &Path, stream: &str) -> String {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-loglevel", "error", "-i"])
        .arg(file)
        .args(["-map", stream, "-c", "copy", "-f", "md5", "-"])
        .output()
        .expect("ffmpeg runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn sha256(file: &Path) -> String {
    let bytes = std::fs::read(file).unwrap();
    format!("{:x}", Sha256::digest(&bytes))
}

/// Copy `row` of `film` muxed whole into `out` from its lease.
async fn muxed(s: &Setup, row: &CopyRow, film: &Path, out: &Path) {
    let lease = s.pool.lease(&row.id).await.unwrap().unwrap();
    let descriptor = s.pool.inner.prober.describe(film).await.unwrap();
    let args = mux_args(
        &descriptor,
        &MuxInputs::new(film, lease.files()),
        &MuxPicks::ALL,
        out,
    );
    let ran = Command::new("ffmpeg").args(&args).output().unwrap();
    assert!(
        ran.status.success(),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );
}

#[tokio::test]
async fn an_apple_copy_of_a_dts_film_stores_only_its_e_ac_3() {
    let (s, film) = real("real-apple", "0").await;
    let copy = made(&s, apple(&film).await).await;
    let tracks = s.tracks(&copy.id).await;
    let files: Vec<Vec<String>> = tracks
        .iter()
        .map(|t| codecs(&track_file(&s.dir, &t.id)))
        .collect();
    assert_eq!(files, [["eac3"]]);
}

#[tokio::test]
async fn an_apple_copy_is_much_smaller_than_its_film() {
    let (s, film) = real("real-apple-size", "0").await;
    let copy = made(&s, apple(&film).await).await;
    let film_size = std::fs::metadata(&film).unwrap().len();
    assert!(
        copy.size_bytes.unwrap() * 2 < film_size,
        "{copy:?} of {film_size}"
    );
}

/// One converted track, two copies: the Apple copy's E-AC-3 is the one
/// `Mobile`'s copy plays, made once.
#[tokio::test]
async fn a_mobile_copy_stores_its_video_and_reuses_the_apple_copy_s_e_ac_3() {
    let (s, film) = real("real-shared", "0").await;
    let apple = made(&s, apple(&film).await).await;
    let eac3 = s.tracks(&apple.id).await[0].clone();
    let mobile = made(&s, mobile(&film).await).await;
    let tracks = s.tracks(&mobile.id).await;
    let kinds: Vec<(Vec<String>, bool)> = tracks
        .iter()
        .map(|t| (codecs(&track_file(&s.dir, &t.id)), t.id == eac3.id))
        .collect();
    let made_at = s
        .store
        .optimized_track(&eac3.id)
        .await
        .unwrap()
        .unwrap()
        .made_at;
    assert_eq!(
        (kinds, made_at),
        (
            vec![
                (vec!["h264".to_owned()], false),
                (vec!["eac3".to_owned()], true)
            ],
            eac3.made_at
        )
    );
}

#[tokio::test]
async fn a_muxed_copy_has_the_film_s_streams_with_its_dts_as_e_ac_3() {
    let (s, film) = real("real-mux-streams", "0").await;
    let copy = made(&s, apple(&film).await).await;
    let out = s.dir.join("muxed.mkv");
    muxed(&s, &copy, &film, &out).await;
    assert_eq!(codecs(&out), ["hevc", "eac3", "ac3", "subrip"]);
}

#[tokio::test]
async fn a_muxed_copy_s_video_is_the_film_s_bit_for_bit() {
    let (s, film) = real("real-mux-video", "0").await;
    let copy = made(&s, apple(&film).await).await;
    let out = s.dir.join("muxed.mkv");
    muxed(&s, &copy, &film, &out).await;
    assert_eq!(packets_md5(&out, "0:v:0"), packets_md5(&film, "0:v:0"));
}

#[tokio::test]
async fn a_muxed_copy_keeps_the_film_s_languages_and_chapters() {
    let (s, film) = real("real-mux-tags", "0").await;
    let copy = made(&s, apple(&film).await).await;
    let out = s.dir.join("muxed.mkv");
    muxed(&s, &copy, &film, &out).await;
    assert_eq!(
        (
            probe(&out, "stream_tags=language"),
            probe(&out, "chapter=id").len()
        ),
        (
            vec![
                "".to_owned(),
                "eng".to_owned(),
                "fre".to_owned(),
                "".to_owned()
            ],
            1
        )
    );
}

/// One frame at 25 fps.
const FRAME: f64 = 0.04;

/// Each stream of `out` starts where `film`'s does, within a frame.
fn lined_up(film: &Path, out: &Path) {
    let (film_starts, out_starts) = (starts(film), starts(out));
    assert_eq!(film_starts.len(), out_starts.len());
    for (a, b) in film_starts.iter().zip(&out_starts) {
        assert!(
            (a - b).abs() <= FRAME,
            "the film starts at {film_starts:?}, the copy at {out_starts:?}"
        );
    }
}

#[tokio::test]
async fn a_muxed_copy_s_streams_start_where_the_film_s_do() {
    let (s, film) = real("real-mux-sync", "0").await;
    let copy = made(&s, apple(&film).await).await;
    let out = s.dir.join("muxed.mkv");
    muxed(&s, &copy, &film, &out).await;
    lined_up(&film, &out);
}

/// Audio that starts half a second in keeps its lead in its track, and
/// in the copy muxed (`encode::late_audio`'s case).
#[tokio::test]
async fn a_late_audio_track_keeps_its_lead_in_a_muxed_copy() {
    let (s, film) = real("real-mux-late", "0.5").await;
    let copy = made(&s, apple(&film).await).await;
    let out = s.dir.join("muxed.mkv");
    muxed(&s, &copy, &film, &out).await;
    lined_up(&film, &out);
}

/// A track evicted and made again is the same bytes: a download written
/// from it keeps its SHA-256 (PX-38).
#[tokio::test]
async fn a_track_evicted_and_made_again_is_the_same_bytes() {
    let (s, film) = real("real-remade", "0").await;
    let first = made(&s, apple(&film).await).await;
    let track = s.tracks(&first.id).await[0].id.clone();
    let before = sha256(&track_file(&s.dir, &track));
    // Now, or a moment later when the worker's own sweep took it first.
    assert_ne!(s.pool.delete(&first.id).await.unwrap(), Deleted::NotFound);
    eventually("the track is gone", || async {
        !track_file(&s.dir, &track).exists()
    })
    .await;
    let again = made(&s, apple(&film).await).await;
    let remade = s.tracks(&again.id).await[0].id.clone();
    assert_ne!(remade, track, "the track was not made again");
    assert_eq!(sha256(&track_file(&s.dir, &remade)), before);
}
