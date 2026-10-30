//! `hevc_vaapi`'s `hvcC` test (`encoders::detect`) on a real GPU: real
//! detection, with the ffmpeg this repo packages it keeps the encoder, and
//! with stock ffmpeg it refuses it. Ignored: CI has no GPU, and the patch
//! in `nix/ffmpeg.nix` is only ever seen working on one. On a host with
//! Mesa's encoder:
//!
//! ```text
//! SPOOLFRAME_HVCC_VAAPI=/dev/dri/renderD128 devenv shell -- cargo test \
//!     -p spoolframe-media --test it hvcc -- --ignored --nocapture
//! ```
//!
//! With `SPOOLFRAME_HVCC_STOCK` set to a stock ffmpeg, it is refused too.

use std::path::PathBuf;

use spoolframe_media::encoders::{self, FfmpegCommand, TranscodeSettings};
use spoolframe_media::sandbox::Tool;

async fn hevc_kept(ffmpeg: PathBuf, device: PathBuf) -> bool {
    let scratch = spoolframe_test_dir::TestDir::new("spoolframe-media-hvcc").await;
    let settings = TranscodeSettings {
        vaapi_device: device,
        ..TranscodeSettings::default()
    };
    let ffmpeg = FfmpegCommand::new(Tool::unconfined(ffmpeg));
    let found = encoders::detect(&ffmpeg, &settings, scratch.as_ref()).await;
    found.video.iter().any(|e| e.name == "hevc_vaapi")
}

#[tokio::test]
#[ignore = "needs a GPU: SPOOLFRAME_HVCC_VAAPI"]
async fn the_packaged_ffmpeg_keeps_hevc_vaapi_and_a_stock_one_loses_it() {
    let device = PathBuf::from(std::env::var("SPOOLFRAME_HVCC_VAAPI").unwrap());
    assert!(hevc_kept("ffmpeg".into(), device.clone()).await);
    if let Ok(stock) = std::env::var("SPOOLFRAME_HVCC_STOCK") {
        assert!(!hevc_kept(stock.into(), device).await);
    }
}
