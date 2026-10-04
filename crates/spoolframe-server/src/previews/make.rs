//! A film's tiles from its file: one ffmpeg run per tile, each seeking to
//! the tile's time and decoding the one keyframe there.
//!
//! One pass over the file with `-skip_frame nokey` decodes as little, but
//! it reads every byte: 46 GB for a 4K remux, from the disk a playback
//! reads too. A seek reads only around the keyframe it lands on: on the
//! dev machine a 4K HEVC remux makes a tile in 0.45 s in software and
//! 0.25 s with VA-API, so a two-hour film's 720 tiles in a few minutes,
//! reading well under 1 GB.
//!
//! Every run is at the lowest CPU and I/O priority (nice 19, the idle I/O
//! class), so a playback, or its conversion, never waits on it.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use spoolframe_media::encoders::Hardware;

use super::sheets::TILE_WIDTH;
use crate::encode::args::TONE_MAP;

/// How long one tile may take before it counts as failed: a seek in a
/// file on a network mount, at the idle I/O priority, behind a busy disk.
const TILE_TIMEOUT: Duration = Duration::from_secs(120);

/// What ffmpeg says when its one frame never came.
const NOTHING_WRITTEN: &str = "Nothing was written into output file";

/// The file to make tiles of.
#[derive(Debug, Clone)]
pub struct Source {
    pub input: PathBuf,
    /// The video stream's index in the file.
    pub stream: u32,
    pub duration: Duration,
    /// PQ or HLG: tone-mapped, or the tiles are grey and washed out.
    pub hdr: bool,
    /// Decode on this hardware (P2-02 found a decoder for the codec),
    /// and the VA-API render node.
    pub hardware: Option<(Hardware, PathBuf)>,
}

/// The times of the tiles: every `interval` from 0, up to the end.
pub fn times(duration: Duration, interval: Duration) -> Vec<Duration> {
    let interval = interval.as_millis().max(1) as u64;
    let end = duration.as_millis() as u64;
    (0..end.div_ceil(interval).max(1))
        .map(|i| Duration::from_millis(i * interval))
        .collect()
}

/// ffmpeg's arguments for the tile at `at`, written to `out` as a JPEG.
pub fn tile_args(source: &Source, hardware: bool, at: Duration, out: &Path) -> Vec<String> {
    let mut args: Vec<String> = ["-hide_banner", "-nostdin", "-loglevel", "error", "-y"]
        .map(String::from)
        .to_vec();
    if let (true, Some((family, device))) = (hardware, &source.hardware) {
        // Without an output format the frames come back to memory, where
        // the scale below runs; a decoder that cannot fails, and the tile
        // is made again in software.
        match family {
            Hardware::Vaapi => args.extend([
                "-hwaccel".into(),
                "vaapi".into(),
                "-hwaccel_device".into(),
                device.display().to_string(),
            ]),
            Hardware::Nvenc => args.extend(["-hwaccel".into(), "cuda".into()]),
            Hardware::Qsv => args.extend(["-hwaccel".into(), "qsv".into()]),
        }
    }
    // Keyframes only, and the one the seek lands on (at or before `at`),
    // not decoded on to `at` exactly: within a GOP is close enough for a
    // picture under a pointer.
    args.extend(["-skip_frame", "nokey", "-noaccurate_seek", "-ss"].map(String::from));
    args.push(format!("{:.3}", at.as_secs_f64()));
    args.push("-i".into());
    args.push(source.input.display().to_string());
    args.extend([
        "-map".into(),
        format!("0:{}", source.stream),
        "-an".into(),
        "-sn".into(),
        "-dn".into(),
        "-frames:v".into(),
        "1".into(),
    ]);
    // Scaled first, so a 4K frame is tone-mapped at 320 px wide; the
    // display aspect ratio, so an anamorphic DVD is not squeezed.
    let mut chain = vec![format!("scale={TILE_WIDTH}:trunc(ow/dar/2)*2")];
    if source.hdr {
        chain.push(TONE_MAP.to_owned());
    }
    chain.push("setsar=1".into());
    args.extend(["-vf".into(), chain.join(","), "-q:v".into(), "2".into()]);
    args.push(out.display().to_string());
    args
}

/// Run `command` at the lowest CPU and I/O priority there is.
pub fn low_priority(command: &mut tokio::process::Command) {
    // SAFETY: only setpriority(2) and ioprio_set(2), both async-signal-safe
    // syscalls, run between fork and exec; neither allocates or takes a
    // lock. Set there, before exec, every thread ffmpeg starts inherits
    // them. A failure (a kernel without ioprio) leaves the default, and
    // the tile is still made.
    unsafe {
        command.pre_exec(|| {
            libc::setpriority(libc::PRIO_PROCESS, 0, 19);
            libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, IOPRIO_IDLE);
            Ok(())
        });
    }
}

/// `ioprio_set(2)`: the calling process.
const IOPRIO_WHO_PROCESS: libc::c_int = 1;
/// The idle class (3), in the class bits: I/O only when no one else
/// wants the disk.
const IOPRIO_IDLE: libc::c_int = 3 << 13;

/// Make every tile of `source`, telling `done` how many are made, until
/// `stop` is set. A tile whose seek finds no frame repeats the one before
/// it, and at the end is left out; a failure with none before is the
/// film's.
pub async fn tiles(
    ffmpeg: &Path,
    source: &Source,
    interval: Duration,
    work: &Path,
    stop: &AtomicBool,
    done: &(dyn Fn(usize, usize) + Send + Sync),
) -> Result<Vec<Vec<u8>>, String> {
    let times = times(source.duration, interval);
    let mut hardware = source.hardware.is_some();
    let mut tiles: Vec<Vec<u8>> = Vec::with_capacity(times.len());
    // How many tiles there are up to the last real picture.
    let mut real = 0;
    for (i, at) in times.iter().enumerate() {
        if stop.load(Ordering::SeqCst) {
            return Err("stopped".into());
        }
        let out = work.join(format!("{i}.jpg"));
        let mut made = tile(ffmpeg, source, hardware, *at, &out).await;
        if made.is_err() && hardware {
            tracing::info!(
                input = %source.input.display(),
                error = made.as_ref().err().map(String::as_str),
                "hardware decoding fails for previews; going on in software"
            );
            hardware = false;
            made = tile(ffmpeg, source, false, *at, &out).await;
        }
        match (made, tiles.last()) {
            (Ok(Some(bytes)), _) => {
                tiles.push(bytes);
                real = tiles.len();
            }
            (Ok(None), Some(before)) => tiles.push(before.clone()),
            (Ok(None), None) => return Err(format!("no frame at {at:?}")),
            (Err(e), _) => return Err(e),
        }
        let _ = tokio::fs::remove_file(&out).await;
        done(i + 1, times.len());
    }
    // No picture at the end (the sound goes on past it) is no tile: a
    // time past the last one shows the last.
    tiles.truncate(real);
    Ok(tiles)
}

/// One tile: its JPEG, or `None` when ffmpeg found no frame there.
async fn tile(
    ffmpeg: &Path,
    source: &Source,
    hardware: bool,
    at: Duration,
    out: &Path,
) -> Result<Option<Vec<u8>>, String> {
    let _ = tokio::fs::remove_file(out).await;
    let mut command = tokio::process::Command::new(ffmpeg);
    command
        .args(tile_args(source, hardware, at, out))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    low_priority(&mut command);
    let child = command
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", ffmpeg.display()))?;
    let output = tokio::time::timeout(TILE_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| format!("ffmpeg took over {TILE_TIMEOUT:?} for the tile at {at:?}"))?
        .map_err(|e| e.to_string())?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    // A seek past the last frame (the sound runs on a little after the
    // picture, so the file is longer than its video) is not a failure:
    // there is just no picture there.
    if stderr.contains(NOTHING_WRITTEN) {
        return Ok(None);
    }
    if !output.status.success() {
        let last = stderr.lines().last().unwrap_or("no message").trim();
        return Err(format!(
            "ffmpeg failed at {at:?} ({}): {last}",
            output.status
        ));
    }
    match tokio::fs::read(out).await {
        Ok(bytes) if !bytes.is_empty() => Ok(Some(bytes)),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(hdr: bool, hardware: Option<Hardware>) -> Source {
        Source {
            input: "/films/a.mkv".into(),
            stream: 0,
            duration: Duration::from_secs(60),
            hdr,
            hardware: hardware.map(|h| (h, PathBuf::from("/dev/dri/renderD129"))),
        }
    }

    fn after<'a>(args: &'a [String], flag: &str) -> &'a str {
        let i = args.iter().position(|a| a == flag).unwrap();
        &args[i + 1]
    }

    #[test]
    fn a_tile_every_interval_up_to_the_end() {
        let s = Duration::from_secs;
        assert_eq!(times(s(30), s(10)), [s(0), s(10), s(20)]);
        assert_eq!(times(s(31), s(10)).len(), 4);
        assert_eq!(times(Duration::from_millis(5_100_000), s(10)).len(), 510);
        // Even a film shorter than the interval has its first picture.
        assert_eq!(times(s(3), s(10)), [s(0)]);
    }

    #[test]
    fn a_tile_seeks_and_decodes_one_keyframe() {
        let args = tile_args(
            &source(false, None),
            false,
            Duration::from_millis(5_020_000),
            Path::new("/w/1.jpg"),
        );
        assert_eq!(after(&args, "-skip_frame"), "nokey");
        assert!(args.contains(&"-noaccurate_seek".into()));
        assert_eq!(after(&args, "-ss"), "5020.000");
        // Input options come before the input.
        let input = args.iter().position(|a| a == "-i").unwrap();
        assert!(args.iter().position(|a| a == "-ss").unwrap() < input);
        assert_eq!(after(&args, "-frames:v"), "1");
        assert_eq!(after(&args, "-vf"), "scale=320:trunc(ow/dar/2)*2,setsar=1");
        assert_eq!(args.last().unwrap(), "/w/1.jpg");
        assert!(!args.contains(&"-hwaccel".into()));
    }

    #[test]
    fn hdr_is_tone_mapped_after_scaling_down() {
        let args = tile_args(&source(true, None), false, Duration::ZERO, Path::new("/o"));
        let chain = after(&args, "-vf");
        assert!(chain.starts_with("scale=320:"), "{chain}");
        assert!(chain.contains("tonemap=tonemap=hable"), "{chain}");
    }

    #[test]
    fn a_hardware_decoder_is_used_where_one_was_found() {
        let source = source(false, Some(Hardware::Vaapi));
        let args = tile_args(&source, true, Duration::ZERO, Path::new("/o"));
        assert_eq!(after(&args, "-hwaccel"), "vaapi");
        assert_eq!(after(&args, "-hwaccel_device"), "/dev/dri/renderD129");
        assert!(
            args.iter().position(|a| a == "-hwaccel").unwrap()
                < args.iter().position(|a| a == "-i").unwrap()
        );
        // Turned off after it failed once.
        let args = tile_args(&source, false, Duration::ZERO, Path::new("/o"));
        assert!(!args.contains(&"-hwaccel".into()));
    }

    #[tokio::test]
    async fn ffmpeg_runs_at_the_lowest_cpu_and_io_priority() {
        let mut command = tokio::process::Command::new("sleep");
        command.arg("5").kill_on_drop(true);
        low_priority(&mut command);
        let child = command.spawn().unwrap();
        let pid = child.id().unwrap();
        // SAFETY: reads of a live child's priorities.
        let (nice, io) = unsafe {
            *libc::__errno_location() = 0;
            let nice = libc::getpriority(libc::PRIO_PROCESS, pid);
            let io = libc::syscall(libc::SYS_ioprio_get, IOPRIO_WHO_PROCESS, pid);
            (nice, io)
        };
        assert_eq!(nice, 19);
        assert_eq!(io as libc::c_int >> 13, 3, "the idle I/O class");
        drop(child);
    }
}
