//! Whether a video's codec record holds its parameter sets, read through
//! real ffprobe from files ffmpeg writes when the test runs.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use spoolframe_media::Prober;
use spoolframe_media::sandbox::Tool;

async fn scratch(test: &str) -> spoolframe_test_dir::TestDir {
    spoolframe_test_dir::TestDir::new(&format!("spoolframe-records-{test}")).await
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

/// Two seconds of a test pattern in `encoder` (with `params`), into `name`.
fn film(dir: &Path, name: &str, encoder: &str, params: &[&str]) -> PathBuf {
    let path = dir.join(name);
    let mut args = vec![
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=160x90:rate=25",
        "-t",
        "2",
        "-pix_fmt",
        "yuv420p",
        "-c:v",
        encoder,
    ];
    args.extend_from_slice(params);
    args.push(path.to_str().unwrap());
    ffmpeg(&args);
    path
}

async fn in_record(path: &Path) -> bool {
    let prober = Prober::new(Tool::unconfined("ffprobe"), Duration::from_secs(30));
    let described = prober.describe(path).await.unwrap();
    described.video.as_ref().unwrap().parameter_sets_in_record
}

#[tokio::test]
async fn h264_and_hevc_as_ffmpeg_writes_them_hold_their_sets_in_the_record() {
    let tmp = scratch("whole").await;
    let h264 = film(tmp.as_ref(), "a.mkv", "libx264", &[]);
    assert!(in_record(&h264).await);
    let hevc = film(
        tmp.as_ref(),
        "b.mp4",
        "libx265",
        &["-x265-params", "log-level=error", "-tag:v", "hvc1"],
    );
    assert!(in_record(&hevc).await);
}

/// Some 4K WEB-DLs: the record (`CodecPrivate`) an `hvcC` with no arrays,
/// the sets before each keyframe in the stream. ffmpeg never writes one
/// like that, so its array count is set to 0 here; ffmpeg still plays it.
#[tokio::test]
async fn an_hevc_record_with_no_arrays_does_not() {
    let tmp = scratch("empty-hvcc").await;
    let path = film(
        tmp.as_ref(),
        "c.mkv",
        "libx265",
        &["-x265-params", "log-level=error:repeat-headers=1"],
    );
    let mut file = std::fs::read(&path).unwrap();
    // CodecPrivate (0x63A2), its size as a one-byte… or longer EBML
    // number, then the record: its array count at byte 22.
    let at = file
        .windows(2)
        .position(|w| w == [0x63, 0xa2])
        .expect("a CodecPrivate");
    let width = file[at + 2].leading_zeros() as usize + 1;
    let record = at + 2 + width;
    assert_eq!(file[record], 1, "an hvcC");
    file[record + 22] = 0;
    std::fs::write(&path, file).unwrap();
    ffmpeg(&["-i", path.to_str().unwrap(), "-f", "null", "-"]);
    assert!(!in_record(&path).await);
}

/// `hev1`: the sets may change in the stream, whatever the record holds.
#[tokio::test]
async fn an_hev1_track_does_not() {
    let tmp = scratch("hev1").await;
    let path = film(
        tmp.as_ref(),
        "d.mp4",
        "libx265",
        &["-x265-params", "log-level=error", "-tag:v", "hev1"],
    );
    assert!(!in_record(&path).await);
}
