//! Where `Prober::keyframes` reads a file's keyframes from (PX-25), how
//! long that takes, and, with `--scan`, whether they equal every packet's
//! and how long reading those takes.
//!
//! ```text
//! cargo run --release -p spoolframe-media --example keyframes -- [--scan] FILE...
//! ```
//!
//! ffprobe runs unconfined, from `PATH`.

use std::time::{Duration, Instant};

use spoolframe_media::Prober;
use spoolframe_media::sandbox::Tool;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut scan = false;
    let mut files = Vec::new();
    for arg in std::env::args_os().skip(1) {
        if arg == "--scan" {
            scan = true;
        } else {
            files.push(std::path::PathBuf::from(arg));
        }
    }
    let timeout = Duration::from_secs(600);
    for file in files {
        let prober = Prober::new(Tool::unconfined("ffprobe"), timeout);
        let started = Instant::now();
        let (times, from) = match prober.keyframes_from(&file).await {
            Ok(found) => found,
            Err(e) => {
                println!("{}: {e}", file.display());
                continue;
            }
        };
        println!(
            "{}: {} keyframes in {:.2} s from {from:?}",
            file.display(),
            times.len(),
            started.elapsed().as_secs_f64()
        );
        if scan {
            let started = Instant::now();
            match prober.scan_keyframes(&file).await {
                Ok(all) => println!(
                    "  every packet: {} keyframes in {:.2} s, {}",
                    all.len(),
                    started.elapsed().as_secs_f64(),
                    if all == times {
                        "the same"
                    } else {
                        "DIFFERENT"
                    }
                ),
                Err(e) => println!("  every packet: {e}"),
            }
        }
    }
}
