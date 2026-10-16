//! Uploaded pictures (PX-53) decoded by real ffmpeg, confined as the
//! server runs it: each format to its centre square, scaled down, and a
//! picture that hits the decoder's limits stopped there.

use std::path::{Path, PathBuf};
use std::time::Duration;

use spoolframe_media::picture::{DecodeLimits, Format, PictureError, check, decode_square};

use crate::sandbox::confined;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pictures")
        .join(name)
}

async fn scratch(test: &str) -> spoolframe_test_dir::TestDir {
    spoolframe_test_dir::TestDir::new(&format!("spoolframe-picture-{test}")).await
}

#[tokio::test]
async fn each_format_decodes_to_its_centre_square() {
    for (name, format, side) in [
        // iPhone photos: HEVC in HEIF.
        ("photo.heic", Format::Heif, 64),
        ("lossy.webp", Format::Webp, 64),
        ("lossless.webp", Format::Webp, 60),
        ("tall.png", Format::Png, 60),
        ("exif-gps.jpg", Format::Jpeg, 64),
    ] {
        let path = fixture(name);
        let header = check(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(header.format, format);
        let square = decode_square(
            &confined("ffmpeg"),
            &path,
            format,
            2048,
            DecodeLimits::DEFAULT,
        )
        .await
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(square.side, side, "{name}");
        assert_eq!(square.rgb.len(), (side * side * 3) as usize, "{name}");
    }
}

/// A picture larger than the most asked for is scaled down to it.
#[tokio::test]
async fn a_large_picture_is_scaled_down_to_the_most_asked_for() {
    let dir = scratch("scaled").await;
    let path = dir.join("wide.png");
    make(&path, "3000x2200", "rgb24");
    let square = decode_square(
        &confined("ffmpeg"),
        &path,
        Format::Png,
        2048,
        DecodeLimits::DEFAULT,
    )
    .await
    .unwrap();
    assert_eq!(square.side, 2048);
}

/// A bomb that says what it is within the pixel limit (6000 × 6000 of
/// 16-bit RGBA, 275 MiB decoded, from a file of a few hundred KB) is
/// stopped by the decode's memory limit: an error, not a server out of
/// memory.
#[tokio::test]
async fn a_bomb_within_the_pixel_limit_is_stopped_by_the_memory_limit() {
    let dir = scratch("bomb").await;
    let path = dir.join("bomb.png");
    make(&path, "6000x6000", "rgba64be");
    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.len() < 2 << 20, "{} bytes", bytes.len());
    check(&bytes).unwrap();
    // ffmpeg's own threads' stacks take about 350 MiB of it before any
    // picture: 512 MiB is room for a photo, and not for the bomb.
    let small = DecodeLimits {
        memory: 512 << 20,
        ..DecodeLimits::DEFAULT
    };
    let stopped = decode_square(&confined("ffmpeg"), &path, Format::Png, 2048, small)
        .await
        .unwrap_err();
    assert!(matches!(stopped, PictureError::Failed(_)), "{stopped}");
    // The same limit takes an ordinary photo.
    decode_square(
        &confined("ffmpeg"),
        &fixture("photo.heic"),
        Format::Heif,
        2048,
        small,
    )
    .await
    .unwrap();
}

/// A decode that runs past its time is killed.
#[tokio::test]
async fn a_decode_past_its_time_is_stopped() {
    let dir = scratch("slow").await;
    let script = dir.join("ffmpeg");
    let source = dir.join("ffmpeg.txt");
    std::fs::write(&source, "#!/bin/sh\nexec sleep 30\n").unwrap();
    let installed = std::process::Command::new("install")
        .args(["-m", "755"])
        .arg(&source)
        .arg(&script)
        .status()
        .unwrap();
    assert!(installed.success());
    let began = std::time::Instant::now();
    let stopped = decode_square(
        &spoolframe_media::sandbox::Tool::unconfined(&script),
        &fixture("tall.png"),
        Format::Png,
        2048,
        DecodeLimits {
            wall: Duration::from_millis(300),
            ..DecodeLimits::DEFAULT
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(stopped, PictureError::Failed(_)), "{stopped}");
    assert!(began.elapsed() < Duration::from_secs(10));
}

/// A file that says it is a PNG and is not one fails to decode.
#[tokio::test]
async fn a_broken_picture_fails_to_decode() {
    let dir = scratch("broken").await;
    let path = dir.join("broken.png");
    let mut bytes = std::fs::read(fixture("tall.png")).unwrap();
    bytes.truncate(40);
    std::fs::write(&path, &bytes).unwrap();
    let failed = decode_square(
        &confined("ffmpeg"),
        &path,
        Format::Png,
        2048,
        DecodeLimits::DEFAULT,
    )
    .await
    .unwrap_err();
    assert!(matches!(failed, PictureError::Failed(_)), "{failed}");
}

/// A PNG of `size` in `pixels`, all black: a few hundred KB however
/// large.
fn make(path: &Path, size: &str, pixels: &str) {
    let status = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("color=c=black:s={size}"))
        .args(["-frames:v", "1", "-pix_fmt", pixels])
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}
