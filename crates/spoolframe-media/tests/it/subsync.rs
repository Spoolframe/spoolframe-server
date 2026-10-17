//! Subtitle sync's runs of ffmpeg (PX-03): the audio decoded confined and
//! at idle priority, and an embedded text subtitle read out as the
//! reference.

use std::path::Path;

use spoolframe_media::sandbox::Tool;
use spoolframe_media::subsync::{self, Span, SyncError};

use crate::sandbox::confined;

async fn scratch(test: &str) -> spoolframe_test_dir::TestDir {
    spoolframe_test_dir::TestDir::new(&format!("spoolframe-subsync-{test}")).await
}

fn ffmpeg(args: &[&str]) {
    let out = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-v", "error", "-y"])
        .args(args)
        .output()
        .expect("ffmpeg runs");
    assert!(
        out.status.success(),
        "ffmpeg {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Five seconds of silence with a subtitle track of two cues, as
/// Matroska.
fn film(dir: &Path) -> std::path::PathBuf {
    let srt = dir.join("in.srt");
    std::fs::write(
        &srt,
        "1\n00:00:01,000 --> 00:00:02,000\nHola\n\n2\n00:00:03,500 --> 00:00:04,250\nAdios\n",
    )
    .unwrap();
    let path = dir.join("film.mkv");
    let (srt, out) = (srt.display().to_string(), path.display().to_string());
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "anullsrc=r=48000:cl=stereo:d=5",
        "-i",
        &srt,
        "-c:a",
        "aac",
        "-c:s",
        "srt",
        &out,
    ]);
    path
}

#[tokio::test]
async fn an_embedded_text_subtitle_is_read_out_as_the_reference() {
    let dir = scratch("reference").await;
    let film = film(dir.path());
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    // Stream 1: the audio is 0.
    let spans = subsync::subtitle_reference(&confined("ffmpeg"), &film, 1, &work)
        .await
        .unwrap();
    assert_eq!(
        spans,
        [
            Span {
                start_ms: 1000,
                end_ms: 2000
            },
            Span {
                start_ms: 3500,
                end_ms: 4250
            }
        ]
    );
}

#[tokio::test]
async fn silence_is_no_reference() {
    let dir = scratch("silence").await;
    let film = film(dir.path());
    let found = subsync::speech(&confined("ffmpeg"), &film, 0).await;
    assert_eq!(found, Err(SyncError::NoReference));
}

/// The decode runs at nice 19, as a preview's does, so a playback's encode
/// alongside keeps its speed; and the samples it writes reach the
/// detector.
#[tokio::test]
async fn the_audio_is_decoded_at_idle_priority() {
    let dir = scratch("nice").await;
    let film = dir.path().join("film.mkv");
    std::fs::write(&film, b"not read").unwrap();
    let nice = dir.path().join("nice");
    let stand_in = dir.path().join("ffmpeg");
    std::fs::write(
        &stand_in,
        format!(
            "#!/bin/sh\ncut -d' ' -f19 /proc/self/stat > '{}'\nhead -c 64000 /dev/zero\n",
            nice.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&stand_in, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Two seconds of silence: no speech, and no error from the pipe.
    let found = subsync::speech(&Tool::unconfined(&stand_in), &film, 0).await;
    assert_eq!(found, Err(SyncError::NoReference));
    assert_eq!(std::fs::read_to_string(&nice).unwrap().trim(), "19");
}

#[tokio::test]
async fn a_failed_decode_says_why() {
    let dir = scratch("failed").await;
    let film = dir.path().join("film.mkv");
    std::fs::write(&film, b"not a film").unwrap();
    match subsync::speech(&confined("ffmpeg"), &film, 0).await {
        Err(SyncError::Tool(why)) => assert!(why.contains("failed"), "{why}"),
        other => panic!("{other:?}"),
    }
}
