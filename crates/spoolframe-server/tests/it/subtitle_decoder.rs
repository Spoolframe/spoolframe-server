//! Bitmap subtitles decoded by the server's own binary in a confined
//! child (finding 45): a track that decodes past its budget fails in the
//! child, and the child may write only its pictures directory.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use spoolframe_media::decode::{self, CueTime};
use spoolframe_media::ocr::Ocr;
use spoolframe_media::pgs::fixture;
use spoolframe_media::sandbox::{Jail, Limits, Tool};
use spoolframe_media::{SubtitleKind, SubtitleSource, SubtitleStream};
use spoolframe_server::subtitles::{OcrSettings, Subtitles, TextSource, TextState};
use spoolframe_types::{SubtitleFormat, SubtitleRole};

use crate::support::{self, fresh_dir};

/// Subtitles in `dir` whose decoder is the real one, confined as in a
/// server; tesseract is a name that is never run.
fn subtitles(dir: &Path) -> Arc<Subtitles> {
    let sandbox = support::media_sandbox(dir);
    Arc::new(
        Subtitles::new(
            Ocr::new(Tool::unconfined("/nonexistent/tesseract"), ["eng".into()]),
            Tool::unconfined("/nonexistent/ffmpeg"),
            Tool::unconfined("/nonexistent/ffprobe"),
            support::decoder(&sandbox),
            &OcrSettings {
                enabled: true,
                threads: 1,
                cache_dir: dir.join("cache").join("subtitles"),
            },
        )
        .unwrap(),
    )
}

/// What becomes of `sup`, an English PGS file beside a film in `dir`,
/// read as text.
async fn read(dir: &Path, sup: &[u8]) -> TextState {
    let film = dir.join("Film.mkv");
    std::fs::write(&film, b"a film").unwrap();
    let path = dir.join("Film.en.sup");
    std::fs::write(&path, sup).unwrap();
    let stream = SubtitleStream {
        source: SubtitleSource::External { path },
        codec: "hdmv_pgs_subtitle".into(),
        kind: SubtitleKind::Bitmap,
        language: Some("eng".into()),
        title: None,
        forced: false,
        default: false,
        role: SubtitleRole::Full,
    };
    let mut state = subtitles(dir)
        .text(TextSource {
            path: &film,
            stream: &stream,
            to: SubtitleFormat::Srt,
        })
        .await;
    // The pixel bomb is 1 Gi pixels of real work, about 30 s of one core
    // in a debug build here and past 60 s under llvm-cov in CI's 4-CPU
    // container. A wall-clock deadline guessed at that is a race with the
    // machine's load; the bound that holds is the child's own: it is
    // killed after Limits::DECODE's CPU seconds. Twice that in wall time
    // is a hang, not a slow machine.
    let hung = Duration::from_secs(2 * Limits::DECODE.cpu_secs);
    tokio::time::timeout(hung, state.wait_for(TextState::finished))
        .await
        .expect("made or failed in time")
        .unwrap()
        .clone()
}

/// Finding 45's bomb: 5,293 bytes of PGS that are 64 pictures of 4128 ×
/// 4128, past the budget's 1 Gi pixels at the 64th. The child refuses it;
/// the server holds none of it.
#[tokio::test]
async fn a_track_past_the_pixel_budget_fails_in_the_child() {
    let dir = fresh_dir("pixel-bomb").await;
    let bomb = fixture::bomb(64, 4096);
    assert_eq!(bomb.len(), 5_293);
    match read(dir.as_ref(), &bomb).await {
        TextState::Failed(reason) => assert!(
            reason.contains("pictures come to more than 1073741824 pixels"),
            "{reason}"
        ),
        other => panic!("{other:?}"),
    }
}

/// And one of more cues than the budget keeps, each a tiny picture.
#[tokio::test]
async fn a_track_past_the_cue_budget_fails_in_the_child() {
    let dir = fresh_dir("cue-bomb").await;
    match read(dir.as_ref(), &fixture::bomb(20_001, 1)).await {
        TextState::Failed(reason) => {
            assert!(reason.contains("more than 20000 cues"), "{reason}")
        }
        other => panic!("{other:?}"),
    }
}

/// A short track, as `pgs::fixture` writes one.
fn small_sup() -> Vec<u8> {
    fixture::sup(
        &[fixture::Cue {
            start_ms: 1_000,
            end_ms: 2_500,
            x: 100,
            y: 900,
            width: 4,
            height: 2,
            mask: vec![1; 8],
        }],
        65_000,
    )
}

/// The decoder run as the server runs it, but told to write its pictures
/// into `pictures` while its jail lets it write `writable` only.
async fn decode_into(dir: &Path, writable: &Path, pictures: &Path) -> std::process::Output {
    let input = dir.join("small.sup");
    std::fs::write(&input, small_sup()).unwrap();
    let mut command = support::decoder(&support::media_sandbox(dir))
        .command(&Jail::new(Limits::DECODE).writes(writable))
        .unwrap();
    command
        .arg(decode::COMMAND)
        .args(["--format", "pgs", "--pictures"])
        .arg(pictures)
        .stdin(std::fs::File::open(&input).unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(60), command.output())
        .await
        .expect("the decoder ends")
        .unwrap()
}

/// The child is confined as the tools are: Landlock lets it write its
/// pictures directory and nothing else, so not the state directory. It is
/// given no path to read: its input is the file the server opened.
#[tokio::test]
async fn the_decoder_writes_only_its_own_directory() {
    let dir = fresh_dir("confined").await;
    let dir = dir.as_ref();
    let work = dir.join("work");
    std::fs::create_dir_all(&work).unwrap();

    let allowed = decode_into(dir, &work, &work).await;
    assert!(
        allowed.status.success(),
        "{}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    assert_eq!(
        decode::parse_index(&allowed.stdout, 10),
        Ok(vec![CueTime {
            start_ms: 1_000,
            end_ms: 2_500
        }])
    );
    assert!(decode::picture(&work, 0).exists());

    let refused = decode_into(dir, &work, dir).await;
    let said = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success());
    assert!(said.contains("Permission denied"), "{said}");
    assert!(!decode::picture(dir, 0).exists());
}

/// The server's start-up check of the decoder: it decodes, and confined
/// it may not write the state directory.
#[tokio::test]
async fn the_start_up_check_shows_the_decoder_confined() {
    let dir = fresh_dir("check").await;
    let dir = dir.as_ref();
    let decoder = support::decoder(&support::media_sandbox(dir));
    let shown = decode::check(&decoder, &dir.join("cache").join("decoder-check"), dir)
        .await
        .unwrap();
    assert!(
        shown.contains(&format!("refused writing {}", dir.display())),
        "{shown}"
    );
    assert!(!dir.join("cache").join("decoder-check").exists());
    assert!(!decode::picture(dir, 0).exists());
}

/// Not confined (`media.sandbox = "off"`), the check says so, and still
/// shows the decoder runs.
#[tokio::test]
async fn unconfined_the_start_up_check_says_so() {
    let dir = fresh_dir("check-off").await;
    let dir = dir.as_ref();
    let decoder = support::decoder(&spoolframe_media::sandbox::Sandbox::off());
    let shown = decode::check(&decoder, &dir.join("decoder-check"), dir)
        .await
        .unwrap();
    assert!(shown.contains("not confined"), "{shown}");
}
