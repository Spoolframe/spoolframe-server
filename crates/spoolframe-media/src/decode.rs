//! Bitmap subtitles decoded in a child process (finding 45): PGS, DVB and
//! VobSub are parsed from files anyone who can write to a library folder
//! made, and a few bytes of them can ask for gigabytes of pictures. So the
//! server never decodes them itself. It runs its own binary again as
//! [`COMMAND`], confined as ffmpeg is ([`Limits::DECODE`]), and gives it
//! the track on its standard input: a `.sup` file it opened, or ffprobe's
//! JSON of the track's packets. The child decodes within
//! [`Budget::DEFAULT`], writes each cue's picture as `{i}.pgm` into the
//! one directory it may write, and prints an index of the cues' times.
//! The server reads only the index, and gives the pictures to tesseract.
//!
//! A decoder that panics, runs out of memory or time, or is refused by
//! its budget fails the child alone, and the track is not read.

use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::bitmap::{Budget, Cue};
use crate::ocr::{self, Feed, OcrError};
use crate::sandbox::{Jail, Limits, Tool};
use crate::{dvb, pgs, vobsub};

/// The server's hidden subcommand that decodes a track.
pub const COMMAND: &str = "decode-subtitle";

/// The longest a child may take over one track. A film's track decodes in
/// seconds.
pub const TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// One line of the index at most: two `u64`s, a space and a newline.
const MAX_LINE: usize = 2 * 20 + 2;

/// A bitmap subtitle format, as [`COMMAND`] names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// A raw `.sup` stream.
    Pgs,
    /// ffprobe's JSON of a DVB track's packets.
    Dvb,
    /// ffprobe's JSON of a VobSub track's packets and header.
    VobSub,
}

impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pgs => "pgs",
            Self::Dvb => "dvb",
            Self::VobSub => "vobsub",
        }
    }

    /// The most of the input read: a `.sup` as the server read it before,
    /// or ffprobe's JSON as it is read from ffprobe.
    fn max_input(self) -> usize {
        match self {
            Self::Pgs => ocr::MAX_SUP_BYTES,
            Self::Dvb | Self::VobSub => ocr::MAX_TRACK_JSON,
        }
    }
}

impl std::str::FromStr for Format {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pgs" => Ok(Self::Pgs),
            "dvb" => Ok(Self::Dvb),
            "vobsub" => Ok(Self::VobSub),
            other => Err(format!("{other:?}: one of \"pgs\", \"dvb\" or \"vobsub\"")),
        }
    }
}

/// When a cue shows; its picture is `{i}.pgm`, `i` its place in the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CueTime {
    pub start_ms: u64,
    pub end_ms: u64,
}

/// The child's work: the track in `format` read from `input`, decoded
/// within `budget`, each cue's picture written into `pictures`. The
/// cues' times, for the index; or why the track cannot be read.
pub fn decode(
    format: Format,
    from_file_start: bool,
    input: impl Read,
    pictures: &Path,
    budget: &Budget,
) -> Result<Vec<CueTime>, String> {
    let limit = format.max_input();
    let mut bytes = Vec::new();
    input
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read the track: {e}"))?;
    if bytes.len() > limit {
        return Err(format!(
            "the {} track is larger than {limit} bytes",
            format.as_str()
        ));
    }
    let cues = match format {
        Format::Pgs => pgs::cues_within(&bytes, budget).map_err(|e| e.to_string())?,
        Format::Dvb | Format::VobSub => {
            let track = ocr::track(&bytes, from_file_start)?;
            drop(bytes);
            match format {
                Format::Dvb => {
                    dvb::cues_within(&track.packets, budget).map_err(|e| e.to_string())?
                }
                _ => vobsub::cues_within(&track.extradata, &track.packets, budget)
                    .map_err(|e| e.to_string())?,
            }
        }
    };
    write_pictures(&cues, pictures).map_err(|e| format!("cannot write a picture: {e}"))
}

/// Each cue's picture as `{i}.pgm` in `dir`, which must hold none yet:
/// a file there already is an error, not written through.
pub fn write_pictures(cues: &[Cue], dir: &Path) -> std::io::Result<Vec<CueTime>> {
    for (i, cue) in cues.iter().enumerate() {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(picture(dir, i))
            .and_then(|mut file| std::io::Write::write_all(&mut file, &cue.image.to_pgm()))?;
    }
    Ok(cues
        .iter()
        .map(|c| CueTime {
            start_ms: c.start_ms,
            end_ms: c.end_ms,
        })
        .collect())
}

/// Cue `i`'s picture in `dir`.
pub fn picture(dir: &Path, i: usize) -> PathBuf {
    dir.join(format!("{i}.pgm"))
}

/// The index the child prints: a line per cue, its start and end in
/// milliseconds.
pub fn index(times: &[CueTime]) -> String {
    let mut out = String::new();
    for t in times {
        let _ = writeln!(out, "{} {}", t.start_ms, t.end_ms);
    }
    out
}

/// The index a child printed, read strictly: at most `max_cues` lines,
/// each two decimal numbers and a newline, each cue ending after it
/// starts. Anything else is an error, not a cue left out.
pub fn parse_index(text: &[u8], max_cues: usize) -> Result<Vec<CueTime>, String> {
    let Some(body) = text.strip_suffix(b"\n") else {
        return match text {
            [] => Ok(Vec::new()),
            _ => Err("the index does not end with a newline".into()),
        };
    };
    let mut times = Vec::new();
    for (n, line) in body.split(|&b| b == b'\n').enumerate() {
        if n >= max_cues {
            return Err(format!("the index has more than {max_cues} cues"));
        }
        let number = |part: &[u8]| -> Option<u64> {
            if part.is_empty() || part.len() > 20 || !part.iter().all(u8::is_ascii_digit) {
                return None;
            }
            std::str::from_utf8(part).ok()?.parse().ok()
        };
        let mut parts = line.split(|&b| b == b' ');
        let (Some(start), Some(end), None) = (parts.next(), parts.next(), parts.next()) else {
            return Err(format!("line {} of the index is not two numbers", n + 1));
        };
        let (Some(start_ms), Some(end_ms)) = (number(start), number(end)) else {
            return Err(format!("line {} of the index is not two numbers", n + 1));
        };
        if end_ms <= start_ms {
            return Err(format!("cue {} of the index ends before it starts", n + 1));
        }
        times.push(CueTime { start_ms, end_ms });
    }
    Ok(times)
}

/// A PGS track of one cue, a pixel shown from 0 to 1 s, for [`check`]: as
/// `pgs::fixture::sup` writes it.
const CHECK_SUP: &[u8] = &[
    80, 71, 0, 0, 0, 0, 0, 0, 0, 0, 22, 0, 19, 7, 128, 4, 56, 16, 0, 0, 128, 0, 0, 1, 0, 0, 0, 0,
    0, 0, 0, 0, 80, 71, 0, 0, 0, 0, 0, 0, 0, 0, 23, 0, 10, 1, 0, 0, 0, 0, 0, 0, 1, 0, 1, 80, 71, 0,
    0, 0, 0, 0, 0, 0, 0, 20, 0, 12, 0, 0, 1, 235, 128, 128, 255, 2, 16, 128, 128, 255, 80, 71, 0,
    0, 0, 0, 0, 0, 0, 0, 21, 0, 14, 0, 0, 0, 192, 0, 0, 7, 0, 1, 0, 1, 1, 0, 0, 80, 71, 0, 0, 0, 0,
    0, 0, 0, 0, 128, 0, 0, 80, 71, 0, 1, 95, 144, 0, 0, 0, 0, 22, 0, 11, 7, 128, 4, 56, 16, 0, 1,
    0, 0, 0, 0, 80, 71, 0, 1, 95, 144, 0, 0, 0, 0, 23, 0, 10, 1, 0, 0, 0, 0, 0, 0, 1, 0, 1, 80, 71,
    0, 1, 95, 144, 0, 0, 0, 0, 128, 0, 0,
];

/// Shows the decoder runs here, as the server will run it: a one-cue track
/// decoded in `scratch`, which is made and removed. When `decoder` is
/// confined, also that it may not write `secret` (the state directory):
/// told to put its picture there, it must be refused. What it showed, for
/// the log; or what went wrong.
pub async fn check(decoder: &Tool, scratch: &Path, secret: &Path) -> Result<String, String> {
    let _ = tokio::fs::remove_dir_all(scratch).await;
    tokio::fs::create_dir_all(scratch)
        .await
        .map_err(|e| format!("cannot make {}: {e}", scratch.display()))?;
    let checked = check_in(decoder, scratch, secret).await;
    let _ = tokio::fs::remove_dir_all(scratch).await;
    checked
}

async fn check_in(decoder: &Tool, scratch: &Path, secret: &Path) -> Result<String, String> {
    let program = decoder.program().display();
    let one = || Feed::Bytes(CHECK_SUP.to_vec());
    match spawn(decoder, Format::Pgs, false, one(), scratch, scratch).await {
        Ok(times) if times.len() == 1 => {}
        Ok(times) => return Err(format!("{program} decoded {} cues of 1", times.len())),
        Err(e) => return Err(format!("{program} cannot decode subtitles: {e}")),
    }
    if !decoder.sandbox().confines() {
        return Ok(format!(
            "the subtitle decoder {program} runs, not confined (media.sandbox)"
        ));
    }
    match spawn(decoder, Format::Pgs, false, one(), scratch, secret).await {
        Err(OcrError::Failed { reason, .. }) if reason.contains("Permission denied") => {
            Ok(format!(
                "a confined subtitle decoder {program} decoded a track and could not write {}",
                secret.display()
            ))
        }
        Err(e) => Err(format!(
            "{program} failed to write {} for another reason than its confinement: {e}",
            secret.display()
        )),
        Ok(_) => {
            // It made this picture itself (it creates, never overwrites).
            let _ = tokio::fs::remove_file(picture(secret, 0)).await;
            Err(format!(
                "a confined subtitle decoder {program} could write {}",
                secret.display()
            ))
        }
    }
}

/// What the child is given on its standard input.
pub enum Input {
    /// A `.sup` the server opened: the child reads it, never its path.
    File(std::fs::File),
    /// ffprobe's JSON of the track, written to a pipe.
    Bytes(Vec<u8>),
}

/// The track in `format` decoded by `decoder` (the server's own binary)
/// from `input`, its pictures written into `pictures`, which the child
/// may write and nothing else. The cues' times; the pictures are
/// [`picture`]`(pictures, i)`.
pub async fn run(
    decoder: &Tool,
    format: Format,
    from_file_start: bool,
    input: Input,
    pictures: &Path,
) -> Result<Vec<CueTime>, OcrError> {
    let feed = match input {
        Input::File(file) => Feed::File(file),
        Input::Bytes(bytes) => Feed::Bytes(bytes),
    };
    spawn(decoder, format, from_file_start, feed, pictures, pictures).await
}

/// [`run`], told to write its pictures into `pictures` while its jail
/// lets it write `writable`: the same but in [`check`].
async fn spawn(
    decoder: &Tool,
    format: Format,
    from_file_start: bool,
    feed: Feed,
    writable: &Path,
    pictures: &Path,
) -> Result<Vec<CueTime>, OcrError> {
    let program = "subtitle decoder";
    let mut command = decoder
        .command(&Jail::new(Limits::DECODE).writes(writable))
        .map_err(|source| OcrError::Spawn {
            program: program.into(),
            source,
        })?;
    command
        .arg(COMMAND)
        .args(["--format", format.as_str(), "--pictures"])
        .arg(pictures);
    if from_file_start {
        command.arg("--from-file-start");
    }
    let max_cues = Budget::DEFAULT.cues;
    let output = ocr::run(command, program, feed, max_cues * MAX_LINE, TIMEOUT).await?;
    if !output.status.success() {
        // Killed: past its memory or CPU limit, say.
        let said = ocr::last_line(&output.stderr);
        let reason = match std::os::unix::process::ExitStatusExt::signal(&output.status) {
            Some(signal) => format!("ended by signal {signal}: {said}"),
            None => said,
        };
        return Err(OcrError::Failed {
            program: program.into(),
            reason,
        });
    }
    parse_index(&output.stdout, max_cues).map_err(|reason| OcrError::Failed {
        program: program.into(),
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitmap::OverBudget;
    use crate::pgs::fixture;

    #[test]
    fn an_index_reads_back_as_its_times() {
        let times = [
            CueTime {
                start_ms: 0,
                end_ms: 1,
            },
            CueTime {
                start_ms: 3_725_042,
                end_ms: u64::MAX,
            },
        ];
        assert_eq!(parse_index(index(&times).as_bytes(), 2), Ok(times.to_vec()));
        assert_eq!(parse_index(b"", 2), Ok(Vec::new()));
    }

    #[test]
    fn an_index_that_is_not_exactly_lines_of_two_numbers_is_refused() {
        for bad in [
            &b"1 2"[..],
            b"1 2\n\n",
            b"1  2\n",
            b" 1 2\n",
            b"1 2 3\n",
            b"1\n",
            b"+1 2\n",
            b"-1 2\n",
            b"1 2\r\n",
            b"a b\n",
            b"18446744073709551616 18446744073709551617\n",
            b"00000000000000000000001 2\n",
            b"5 5\n",
            b"6 5\n",
        ] {
            assert!(
                parse_index(bad, 10).is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn an_index_of_more_cues_than_the_budget_is_refused() {
        assert_eq!(parse_index(b"1 2\n3 4\n", 2).map(|t| t.len()), Ok(2));
        assert!(parse_index(b"1 2\n3 4\n5 6\n", 2).is_err());
    }

    async fn scratch() -> (spoolframe_test_dir::TestDir, PathBuf) {
        let tmp = spoolframe_test_dir::TestDir::new("spoolframe-decode").await;
        let dir = tmp.as_ref().to_path_buf();
        (tmp, dir)
    }

    #[tokio::test]
    async fn a_track_s_pictures_are_written_and_its_times_returned() {
        let (_tmp, dir) = scratch().await;
        let shown = |start_ms, end_ms| fixture::Cue {
            start_ms,
            end_ms,
            x: 100,
            y: 900,
            width: 4,
            height: 2,
            mask: vec![1; 8],
        };
        let sup = fixture::sup(&[shown(1_000, 2_000), shown(5_000, 6_500)], 65_000);
        let times = decode(Format::Pgs, false, &sup[..], &dir, &Budget::DEFAULT).unwrap();
        assert_eq!(
            times,
            [
                CueTime {
                    start_ms: 1_000,
                    end_ms: 2_000
                },
                CueTime {
                    start_ms: 5_000,
                    end_ms: 6_500
                }
            ]
        );
        let pgm = std::fs::read(picture(&dir, 1)).unwrap();
        assert!(pgm.starts_with(b"P5\n36 34\n255\n"), "{pgm:?}");
        assert!(!picture(&dir, 2).exists());
    }

    #[tokio::test]
    async fn a_track_past_its_budget_writes_no_picture() {
        let (_tmp, dir) = scratch().await;
        let budget = Budget {
            cues: 1,
            ..Budget::DEFAULT
        };
        let err = decode(Format::Pgs, false, &fixture::bomb(2, 8)[..], &dir, &budget).unwrap_err();
        assert_eq!(err, OverBudget::Cues(1).to_string());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    /// A picture already there (a symlink the child did not make, say) is
    /// not written through.
    #[tokio::test]
    async fn a_picture_already_there_is_an_error() {
        let (_tmp, dir) = scratch().await;
        let elsewhere = dir.join("elsewhere");
        std::fs::write(&elsewhere, b"keep").unwrap();
        std::os::unix::fs::symlink(&elsewhere, picture(&dir, 0)).unwrap();
        let sup = fixture::sup(
            &[fixture::Cue {
                start_ms: 0,
                end_ms: 1_000,
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                mask: vec![1],
            }],
            65_000,
        );
        assert!(decode(Format::Pgs, false, &sup[..], &dir, &Budget::DEFAULT).is_err());
        assert_eq!(std::fs::read(&elsewhere).unwrap(), b"keep");
    }

    #[test]
    fn the_check_s_track_is_the_fixture_s_one_cue() {
        let sup = fixture::sup(
            &[fixture::Cue {
                start_ms: 0,
                end_ms: 1_000,
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                mask: vec![1],
            }],
            65_000,
        );
        assert_eq!(CHECK_SUP, sup);
    }

    #[test]
    fn a_format_is_one_of_three_words() {
        for format in [Format::Pgs, Format::Dvb, Format::VobSub] {
            assert_eq!(format.as_str().parse(), Ok(format));
        }
        assert!("sup".parse::<Format>().is_err());
    }
}
