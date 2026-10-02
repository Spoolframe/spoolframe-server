//! `Prober` with stand-in ffprobe scripts, so timeouts, failures and the
//! cache can be tested without real media.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use spoolframe_media::{ProbeError, Prober, SubtitleSource};

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"))
}

/// A fresh media directory for one test. The stand-in ffprobe and its
/// call log live in its `bin` sibling, so they never change the media
/// directory the cache watches.
fn scratch(test: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("prober")
        .join(test);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::create_dir_all(root.join("media")).unwrap();
    root.join("media")
}

fn bin(dir: &Path) -> PathBuf {
    dir.parent().unwrap().join("bin")
}

/// A stand-in ffprobe: logs each path it is asked about to `calls`, then
/// runs `body` with the path in `$path`.
fn fake_ffprobe(dir: &Path, body: &str) -> PathBuf {
    let script = bin(dir).join("ffprobe");
    let calls = bin(dir).join("calls");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nfor path; do :; done\necho \"$path\" >> '{}'\n{body}\n",
            calls.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// Answers with the forced-subtitle MKV for media and the SRT fixture for
/// `.srt` files.
fn ffprobe_with_fixtures(dir: &Path) -> PathBuf {
    fake_ffprobe(
        dir,
        &format!(
            "case \"$path\" in *.srt) cat '{}' ;; *) cat '{}' ;; esac",
            fixture("external-srt"),
            fixture("mkv-forced-subs")
        ),
    )
}

fn calls(dir: &Path) -> usize {
    std::fs::read_to_string(bin(dir).join("calls"))
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

fn media(dir: &Path) -> PathBuf {
    let path = dir.join("Movie.mkv");
    std::fs::write(&path, b"not really a movie").unwrap();
    path
}

fn prober(ffprobe: PathBuf) -> Prober {
    Prober::new(ffprobe, Duration::from_secs(10))
}

#[tokio::test]
async fn a_subtitle_file_beside_the_media_is_added_with_its_name_s_language() {
    let dir = scratch("external");
    let movie = media(&dir);
    std::fs::write(dir.join("Movie.en.forced.srt"), b"").unwrap();
    let d = prober(ffprobe_with_fixtures(&dir))
        .describe(&movie)
        .await
        .unwrap();
    let last = d.subtitles.last().unwrap();
    assert_eq!(
        last.source,
        SubtitleSource::External {
            path: dir.join("Movie.en.forced.srt")
        }
    );
    assert_eq!(last.codec, "subrip");
    assert_eq!(last.language.as_deref(), Some("en"));
    assert!(last.forced);
    assert!(d.tracks().subtitles.last().unwrap().external);
}

#[tokio::test]
async fn an_unchanged_file_is_probed_once() {
    let dir = scratch("cache-hit");
    let movie = media(&dir);
    let prober = prober(ffprobe_with_fixtures(&dir));
    let first = prober.describe(&movie).await.unwrap();
    let second = prober.describe(&movie).await.unwrap();
    assert_eq!(calls(&dir), 1);
    assert_eq!(first, second);
}

#[tokio::test]
async fn a_changed_file_is_probed_again() {
    let dir = scratch("cache-size");
    let movie = media(&dir);
    let prober = prober(ffprobe_with_fixtures(&dir));
    prober.describe(&movie).await.unwrap();
    std::fs::write(&movie, b"a different, longer movie").unwrap();
    prober.describe(&movie).await.unwrap();
    assert_eq!(calls(&dir), 2);
}

#[tokio::test]
async fn a_new_subtitle_file_is_noticed() {
    let dir = scratch("cache-new-subtitle");
    let movie = media(&dir);
    let prober = prober(ffprobe_with_fixtures(&dir));
    let before = prober.describe(&movie).await.unwrap().subtitles.len();
    std::fs::write(dir.join("Movie.srt"), b"").unwrap();
    let after = prober.describe(&movie).await.unwrap().subtitles.len();
    assert_eq!(after, before + 1);
}

#[tokio::test]
async fn a_hung_ffprobe_is_stopped_and_named_by_path() {
    let dir = scratch("timeout");
    let movie = media(&dir);
    let prober = Prober::new(fake_ffprobe(&dir, "sleep 30"), Duration::from_millis(200));
    let err = prober.describe(&movie).await.unwrap_err();
    assert!(matches!(err, ProbeError::TimedOut { .. }), "{err}");
    assert!(err.to_string().contains(movie.to_str().unwrap()), "{err}");
}

#[tokio::test]
async fn a_failing_ffprobe_reports_its_stderr_and_the_path() {
    let dir = scratch("failure");
    let movie = media(&dir);
    let err = prober(fake_ffprobe(&dir, "echo 'Invalid data found' >&2; exit 1"))
        .describe(&movie)
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("Invalid data found"), "{text}");
    assert!(text.contains(movie.to_str().unwrap()), "{text}");
}

#[tokio::test]
async fn a_missing_ffprobe_is_named() {
    let dir = scratch("no-ffprobe");
    let movie = media(&dir);
    let err = prober(dir.join("no-such-ffprobe"))
        .describe(&movie)
        .await
        .unwrap_err();
    assert!(matches!(err, ProbeError::Spawn { .. }), "{err}");
    assert!(err.to_string().contains("no-such-ffprobe"), "{err}");
}

#[tokio::test]
async fn garbage_output_is_an_error_not_an_empty_descriptor() {
    let dir = scratch("garbage");
    let movie = media(&dir);
    let err = prober(fake_ffprobe(&dir, "echo 'not json'"))
        .describe(&movie)
        .await
        .unwrap_err();
    assert!(matches!(err, ProbeError::Unreadable { .. }), "{err}");
}

#[tokio::test]
async fn a_file_with_no_audio_or_video_is_an_error() {
    let dir = scratch("no-media");
    let movie = media(&dir);
    let err = prober(fake_ffprobe(
        &dir,
        &format!("cat '{}'", fixture("external-srt")),
    ))
    .describe(&movie)
    .await
    .unwrap_err();
    assert!(matches!(err, ProbeError::NoMedia { .. }), "{err}");
}

#[tokio::test]
async fn a_broken_subtitle_file_is_skipped() {
    let dir = scratch("broken-subtitle");
    let movie = media(&dir);
    std::fs::write(dir.join("Movie.srt"), b"").unwrap();
    let ffprobe = fake_ffprobe(
        &dir,
        &format!(
            "case \"$path\" in *.srt) exit 1 ;; *) cat '{}' ;; esac",
            fixture("mkv-forced-subs")
        ),
    );
    let d = prober(ffprobe).describe(&movie).await.unwrap();
    assert!(
        d.subtitles
            .iter()
            .all(|s| matches!(s.source, SubtitleSource::Embedded { .. }))
    );
}

#[tokio::test]
async fn a_missing_media_file_is_named() {
    let dir = scratch("missing-media");
    let err = prober(ffprobe_with_fixtures(&dir))
        .describe(&dir.join("Gone.mkv"))
        .await
        .unwrap_err();
    assert!(matches!(err, ProbeError::Stat { .. }), "{err}");
    assert!(err.to_string().contains("Gone.mkv"), "{err}");
}
