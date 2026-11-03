//! The picture cue on screen at a seek. A seek lands on a keyframe, so a
//! burned-in cue that began before that keyframe is never read and stays
//! off the screen until the next. A second input for the track alone would
//! read it, but ffmpeg hangs once that input's cues run out. So the film
//! itself is read from the cue ([`EncodeSpec::cue_from`]).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use spoolframe_media::sandbox::{Jail, Limits, Tool};
use spoolframe_media::{SubtitleKind, SubtitleSource};
use tokio::runtime::{Handle, RuntimeFlavor};

use super::args::{EncodeSpec, burned};

/// How far before a seek a cue is looked for: each second of it can cost
/// a second of film decoded for nothing, and a cue is on screen for less.
const LOOK_BACK: Duration = Duration::from_secs(15);
/// A seek waits this long at most, and goes on without the cue.
const TAKES_AT_MOST: Duration = Duration::from_secs(2);
/// PGS ends a cue with a display set that draws nothing, 30 bytes as
/// muxers write it; the smallest picture is far over this.
const CLEAR_AT_MOST: u64 = 64;

/// Where to read the film from for the cue on screen at `spec`'s start, if
/// one is. A lookup that fails only leaves the cue off, as it was.
pub(super) fn cue_from(
    spec: &EncodeSpec<'_>,
    ffmpeg: &Tool,
    dir: &Path,
    read: &[PathBuf],
) -> Option<Duration> {
    let start = spec.start?;
    let track = burned(spec).ok()??;
    let SubtitleSource::Embedded { index } = track.source else {
        return None;
    };
    if track.kind != SubtitleKind::Bitmap {
        return None;
    }
    let from = start.saturating_sub(LOOK_BACK);
    let mut jail = Jail::new(Limits::TILE).reads(dir);
    for input in read {
        jail = jail.input(input.clone());
    }
    let mut command = ffmpeg
        .command(&jail)
        .inspect_err(|e| tracing::debug!(error = %e, "cannot look for the cue at a seek"))
        .ok()?;
    command
        .args(lookup_args(spec.inputs.film, index, from, start - from))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // Runs start from sync code. A single-threaded runtime, a test's,
    // cannot wait here: as if no cue were found.
    let handle = Handle::try_current()
        .ok()
        .filter(|h| h.runtime_flavor() == RuntimeFlavor::MultiThread)?;
    let output = tokio::task::block_in_place(|| {
        handle.block_on(tokio::time::timeout(TAKES_AT_MOST, command.output()))
    });
    match output {
        Ok(Ok(output)) => live_cue(&String::from_utf8_lossy(&output.stdout), start),
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "cannot look for the cue at a seek");
            None
        }
        Err(_) => {
            tracing::debug!("the cue at a seek took too long to find");
            None
        }
    }
}

/// ffmpeg's arguments to list track `index`'s packets, with the film's
/// picture and sound skipped: nothing is decoded.
fn lookup_args(film: &Path, index: u32, from: Duration, length: Duration) -> Vec<String> {
    [
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-discard:v",
        "all",
        "-discard:a",
        "all",
        "-ss",
        &format!("{:.3}", from.as_secs_f64()),
        "-copyts",
        "-t",
        &format!("{:.3}", length.as_secs_f64()),
        "-i",
        &film.display().to_string(),
        "-map",
        &format!("0:{index}"),
        "-c",
        "copy",
        "-f",
        "framecrc",
        "-",
    ]
    .map(str::to_owned)
    .to_vec()
}

/// When the cue on screen at `at` began, from a track's `framecrc`: the
/// last packet before `at`, unless that one clears the screen.
fn live_cue(framecrc: &str, at: Duration) -> Option<Duration> {
    let mut time_base = (1u64, 1000u64);
    let mut last = None;
    for line in framecrc.lines() {
        if let Some(base) = line.strip_prefix("#tb 0: ") {
            let (num, den) = base.split_once('/')?;
            time_base = (num.trim().parse().ok()?, den.trim().parse().ok()?);
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        let mut fields = line.split(',').map(str::trim);
        let pts: u64 = fields.nth(2)?.parse().ok()?;
        let size: u64 = fields.nth(1)?.parse().ok()?;
        let began = Duration::from_secs_f64(pts as f64 * time_base.0 as f64 / time_base.1 as f64);
        if began < at {
            last = Some((began, size));
        }
    }
    last.filter(|&(_, size)| size > CLEAR_AT_MOST)
        .map(|(began, _)| began)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT: Duration = Duration::from_millis(363_500);

    fn listing(packets: &[(u64, u64)]) -> String {
        let mut out = String::from("#tb 0: 1/1000\n#media_type 0: subtitle\n");
        for (pts, size) in packets {
            out.push_str(&format!("0, {pts}, {pts}, 2502, {size}, 0xdeadbeef\n"));
        }
        out
    }

    #[test]
    fn a_picture_still_up_at_the_seek_is_the_cue() {
        let packets = listing(&[(361_111, 30), (362_737, 30_622)]);
        assert_eq!(live_cue(&packets, AT), Some(Duration::from_millis(362_737)));
    }

    #[test]
    fn a_screen_already_cleared_has_no_cue() {
        let packets = listing(&[(360_000, 26_143), (361_111, 30)]);
        assert_eq!(live_cue(&packets, AT), None);
    }

    #[test]
    fn no_packet_is_no_cue() {
        assert_eq!(live_cue(&listing(&[]), AT), None);
    }

    #[test]
    fn a_packet_from_the_seek_on_is_not_on_screen_yet() {
        let packets = listing(&[(361_111, 30), (363_500, 30_622)]);
        assert_eq!(live_cue(&packets, AT), None);
    }

    #[test]
    fn a_track_keeps_time_in_its_own_units() {
        let packets = "#tb 0: 1/90000\n0, 32646330, 32646330, 0, 30622, 0xdeadbeef\n";
        let began = live_cue(packets, AT).unwrap();
        assert_eq!(began.as_millis(), 362_737);
    }
}
