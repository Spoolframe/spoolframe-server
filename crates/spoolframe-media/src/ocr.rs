//! Bitmap subtitles to text (P2-07, P3-08): a PGS, VobSub or DVB track's
//! cues ([`crate::bitmap`]) read by tesseract, and written out as SubRip or
//! WebVTT.
//!
//! tesseract runs as a program, as ffmpeg does: a few processes side by
//! side, each given a list of cue pictures and asked for one page of text
//! per picture. One process per cue would spend more time loading the
//! language model than reading.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::sync::Semaphore;

use spoolframe_types::SubtitleFormat;

use crate::bitmap::{Packet, Track};
use crate::decode::CueTime;
use crate::sandbox::{Command, Jail, Limits, Tool};

/// Cues per tesseract process: enough that loading the model is a small
/// part of each run, few enough that the work spreads over every thread
/// and progress moves.
const CHUNK: usize = 40;

/// The longest one tesseract run over a [`CHUNK`] of cues may take
/// (PX-09 B-3): seconds is usual.
pub const OCR_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// The longest an ffmpeg or ffprobe pass over a whole film may take:
/// copying a track out, reading its packets, converting a text track. A
/// large file on slow storage takes minutes.
pub const PASS_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// The most ffprobe's JSON of a track's packets is read to: its hex dump is
/// about 4.5 bytes a byte, and a film's bitmap track is tens of MB.
pub const MAX_TRACK_JSON: usize = 512 << 20;

/// The most of a PGS track (`.sup`) read: a film's is tens of MB.
pub const MAX_SUP_BYTES: usize = 256 << 20;

/// The most of tesseract's text read, for one run.
const MAX_TEXT: usize = 16 << 20;

/// What a program writes to stderr that is kept, for its last line.
const MAX_STDERR: usize = 64 << 10;

/// A cue as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextCue {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

#[derive(Debug, thiserror::Error)]
pub enum OcrError {
    #[error("cannot run {program}: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("{program} failed: {reason}")]
    Failed { program: String, reason: String },
    #[error("tesseract read {got} pictures of {expected}")]
    PageCount { expected: usize, got: usize },
    #[error("{program} took longer than {} s", after.as_secs())]
    TimedOut { program: String, after: Duration },
    #[error("{program} wrote more than {limit} bytes")]
    TooMuch { program: String, limit: usize },
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// tesseract, and the languages it has data for.
#[derive(Debug, Clone)]
pub struct Ocr {
    tesseract: Tool,
    languages: BTreeSet<String>,
}

impl Ocr {
    /// No OCR: the planner never offers it.
    pub fn disabled() -> Self {
        Self {
            tesseract: Tool::unconfined(PathBuf::new()),
            languages: BTreeSet::new(),
        }
    }

    /// `tesseract` with data for `languages`, as [`Ocr::detect`] would find
    /// them.
    pub fn new(tesseract: Tool, languages: impl IntoIterator<Item = String>) -> Self {
        Self {
            tesseract,
            languages: languages.into_iter().collect(),
        }
    }

    /// Asks `tesseract` which languages it has. If it cannot be run, OCR
    /// is off, and the log says why.
    pub async fn detect(tesseract: &Tool) -> Self {
        let output = match tesseract.command(&Jail::new(Limits::OCR)) {
            Ok(mut command) => {
                command.arg("--list-langs");
                run(
                    command,
                    "tesseract",
                    Feed::Nothing,
                    MAX_TEXT,
                    Duration::from_secs(30),
                )
                .await
            }
            Err(source) => Err(OcrError::Spawn {
                program: "tesseract".into(),
                source,
            }),
        };
        let languages = match output {
            Ok(out) if out.status.success() => list_langs(&String::from_utf8_lossy(&out.stdout)),
            Ok(out) => {
                tracing::warn!(
                    tesseract = %tesseract.program().display(),
                    stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                    "tesseract cannot list its languages; subtitle OCR is off"
                );
                BTreeSet::new()
            }
            Err(e) => {
                tracing::warn!(
                    tesseract = %tesseract.program().display(),
                    error = %e,
                    "cannot run tesseract; subtitle OCR is off"
                );
                BTreeSet::new()
            }
        };
        if !languages.is_empty() {
            tracing::info!(?languages, "subtitle OCR available");
        }
        Self {
            tesseract: tesseract.to_owned(),
            languages,
        }
    }

    pub fn languages(&self) -> &BTreeSet<String> {
        &self.languages
    }

    /// The tesseract language for a track tagged `tag` (ISO 639-2, as
    /// Matroska and ffprobe give it). An untagged track is read as
    /// English. A tagged one is read only with its own language's data:
    /// English data reads French badly and Russian not at all, and wrong
    /// text is worse than none.
    pub fn language_for(&self, tag: Option<&str>) -> Option<String> {
        let wanted = match tag.map(str::to_ascii_lowercase).as_deref() {
            None | Some("" | "und" | "mul" | "zxx") => "eng".to_owned(),
            Some(tag) => tesseract_code(tag).to_owned(),
        };
        self.languages.contains(&wanted).then_some(wanted)
    }

    /// The text of each of `count` cue pictures, in order: `{i}.pgm` in
    /// `pictures` ([`crate::decode::picture`]), as the subtitle decoder
    /// wrote them. They are read with `threads` tesseract processes side
    /// by side, which may read `pictures` and `work`. The lists of
    /// pictures go in `work`, which the caller owns and removes, and which
    /// the decoder may not write. `progress` is told how many cues are
    /// done.
    pub async fn recognise(
        &self,
        pictures: &Path,
        count: usize,
        work: &Path,
        language: &str,
        threads: usize,
        progress: &(dyn Fn(usize) + Sync),
    ) -> Result<Vec<String>, OcrError> {
        tokio::fs::create_dir_all(work).await?;
        let permits = Arc::new(Semaphore::new(threads.max(1)));
        let mut runs = tokio::task::JoinSet::new();
        for (n, first) in (0..count).step_by(CHUNK).enumerate() {
            let listed: Vec<PathBuf> = (first..(first + CHUNK).min(count))
                .map(|i| crate::decode::picture(pictures, i))
                .collect();
            let list = work.join(format!("chunk{n}.txt"));
            let mut text = String::new();
            for p in &listed {
                let _ = writeln!(text, "{}", p.display());
            }
            tokio::fs::write(&list, text).await?;
            let (tesseract, language) = (self.tesseract.clone(), language.to_owned());
            let permits = permits.clone();
            let pictures = pictures.to_owned();
            runs.spawn(async move {
                let _permit = permits.acquire_owned().await.expect("never closed");
                let pages =
                    read_pages(&tesseract, &list, &pictures, &language, listed.len()).await?;
                Ok::<_, OcrError>((first, pages))
            });
        }
        let mut texts = vec![String::new(); count];
        let mut done = 0;
        while let Some(run) = runs.join_next().await {
            let (first, pages) = run.expect("an OCR run never panics")?;
            done += pages.len();
            for (i, page) in pages.into_iter().enumerate() {
                texts[first + i] = page;
            }
            progress(done);
        }
        Ok(texts)
    }
}

/// One tesseract run over a list of pictures (in `pictures`): one page
/// of text each.
async fn read_pages(
    tesseract: &Tool,
    list: &Path,
    pictures: &Path,
    language: &str,
    expected: usize,
) -> Result<Vec<String>, OcrError> {
    let mut jail = Jail::new(Limits::OCR).reads(pictures);
    if let Some(dir) = list.parent() {
        jail = jail.reads(dir);
    }
    let mut command = spawnable(tesseract, jail).await?;
    command
        .arg(list)
        .arg("stdout")
        .args(["-l", language, "--psm", "6"])
        // One thread each: the parallelism is the processes, and
        // tesseract's own threads only fight them for cores.
        .env("OMP_THREAD_LIMIT", "1");
    let output = succeeded(
        run(command, "tesseract", Feed::Nothing, MAX_TEXT, OCR_TIMEOUT).await?,
        "tesseract",
    )?;
    // A form feed between pages, none after the last.
    let text = String::from_utf8_lossy(&output.stdout);
    let pages: Vec<String> = text.split('\x0c').map(tidy).collect();
    if pages.len() != expected {
        return Err(OcrError::PageCount {
            expected,
            got: pages.len(),
        });
    }
    Ok(pages)
}

/// Copies subtitle stream `index` (ffprobe's stream index) of `input` into
/// a raw `.sup` file, without decoding it.
pub async fn extract_sup(
    ffmpeg: &Tool,
    input: &Path,
    index: u32,
    out: &Path,
) -> Result<(), OcrError> {
    let jail = Jail::new(Limits::PASS).input(input);
    copy_stream(
        ffmpeg,
        jail,
        input,
        index,
        &["-c", "copy", "-f", "sup"],
        out,
    )
    .await
}

/// Subtitle stream `index` (ffprobe's stream index) of `input` as its
/// packets, read with ffprobe: the way into VobSub and DVB tracks, which
/// ffmpeg has no raw format to copy them out to. A `.idx` is read the same
/// way, with ffmpeg pairing it with its `.sub`.
///
/// With `from_file_start`, times are from the start of the file, as a
/// player shows them: an MPEG-TS recording's clock starts anywhere. A
/// subtitle file of its own already counts from the film's start.
pub async fn read_track(
    ffprobe: &Tool,
    input: &Path,
    index: u32,
    from_file_start: bool,
) -> Result<Track, OcrError> {
    let json = read_track_json(ffprobe, input, index).await?;
    track(&json, from_file_start).map_err(|reason| OcrError::Failed {
        program: "ffprobe".into(),
        reason,
    })
}

/// ffprobe's JSON of [`read_track`], not parsed: what the subtitle
/// decoder reads ([`crate::decode`]).
pub async fn read_track_json(
    ffprobe: &Tool,
    input: &Path,
    index: u32,
) -> Result<Vec<u8>, OcrError> {
    let mut command = spawnable(ffprobe, Jail::new(Limits::PROBE).media(input)).await?;
    command
        .args(["-v", "error", "-select_streams", &index.to_string()])
        .args(["-show_data", "-show_entries"])
        .arg("packet=pts,duration,data:stream=time_base,extradata:format=start_time")
        .args(["-of", "json"])
        .arg(input);
    let output = run(
        command,
        "ffprobe",
        Feed::Nothing,
        MAX_TRACK_JSON,
        PASS_TIMEOUT,
    )
    .await?;
    Ok(succeeded(output, "ffprobe")?.stdout)
}

/// ffprobe's JSON for one stream's packets, with `-show_data`.
#[derive(serde::Deserialize)]
struct Probed {
    #[serde(default)]
    packets: Vec<ProbedPacket>,
    #[serde(default)]
    streams: Vec<ProbedStream>,
    format: Option<ProbedFormat>,
}

#[derive(serde::Deserialize)]
struct ProbedPacket {
    pts: Option<i64>,
    duration: Option<i64>,
    data: Option<String>,
}

#[derive(serde::Deserialize)]
struct ProbedStream {
    time_base: String,
    extradata: Option<String>,
}

#[derive(serde::Deserialize)]
struct ProbedFormat {
    start_time: Option<String>,
}

pub(crate) fn track(json: &[u8], from_file_start: bool) -> Result<Track, String> {
    let probed: Probed = serde_json::from_slice(json).map_err(|e| e.to_string())?;
    let stream = probed.streams.first().ok_or("no such stream")?;
    let (num, den) = stream
        .time_base
        .split_once('/')
        .and_then(|(n, d)| Some((n.parse::<u32>().ok()?, d.parse::<u32>().ok()?)))
        .filter(|&(n, d)| n > 0 && d > 0)
        .ok_or_else(|| format!("a time base of {}", stream.time_base))?;
    // The time base is the file's: held to u32 parts (ffmpeg's own are
    // ints), so ticks × num × 1000 stays well inside i128 (review
    // 2026-10-11 L67).
    let (num, den) = (i128::from(num), i128::from(den));
    let ms = |ticks: i64| i128::from(ticks) * num * 1000 / den;
    let start = match probed.format.and_then(|f| f.start_time) {
        Some(secs) if from_file_start => {
            (secs.parse::<f64>().map_err(|e| e.to_string())? * 1000.0).round() as i128
        }
        _ => 0,
    };
    let packets = probed
        .packets
        .into_iter()
        // A packet with no time cannot be placed.
        // One whose time is past what a u64 of milliseconds holds is left
        // out, not wrapped (review 2026-10-11 L67).
        .filter_map(|p| {
            Some(Packet {
                pts_ms: u64::try_from((ms(p.pts?) - start).max(0)).ok()?,
                duration_ms: p
                    .duration
                    .filter(|&d| d > 0)
                    .and_then(|d| u64::try_from(ms(d)).ok()),
                data: hexdump(p.data.as_deref().unwrap_or_default()),
            })
        })
        .collect();
    Ok(Track {
        extradata: hexdump(stream.extradata.as_deref().unwrap_or_default()),
        packets,
    })
}

/// The bytes of ffprobe's hex dump: a line per 16 bytes, an offset, the
/// bytes in hex in the 41 columns after it, then the same as text.
fn hexdump(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text
        .lines()
        .filter_map(|line| line.get(10..).map(|rest| rest.get(..41).unwrap_or(rest)))
        .flat_map(|hex| hex.chars().filter_map(|c| c.to_digit(16)))
        .map(|d| d as u8)
        .collect();
    digits
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[high, low]| (high << 4) | low)
        .collect()
}

/// Converts text subtitle stream `index` of `input` (ffprobe's stream
/// index; 0 for a subtitle file of its own) to `to`, SubRip or WebVTT,
/// with ffmpeg. Not OCR, but the other way a track becomes the text a
/// client takes: ASS, `mov_text` or SubRip to WebVTT, say. ASS styling is
/// lost.
pub async fn convert_text(
    ffmpeg: &Tool,
    input: &Path,
    index: u32,
    to: SubtitleFormat,
    out: &Path,
) -> Result<(), OcrError> {
    let (codec, format) = text_codec(to);
    let jail = Jail::new(Limits::PASS).input(input);
    copy_stream(
        ffmpeg,
        jail,
        input,
        index,
        &["-c:s", codec, "-f", format],
        out,
    )
    .await
}

/// [`convert_text`] of a subtitle file of the server's own, in its
/// hidden state directory, which a run may not open as an input: a synced
/// subtitle (PX-03). The run may read the file's directory instead.
pub async fn convert_own_text(
    ffmpeg: &Tool,
    input: &Path,
    to: SubtitleFormat,
    out: &Path,
) -> Result<(), OcrError> {
    let (codec, format) = text_codec(to);
    let jail = match input.parent() {
        Some(dir) => Jail::new(Limits::PASS).reads(dir),
        None => Jail::new(Limits::PASS),
    };
    copy_stream(ffmpeg, jail, input, 0, &["-c:s", codec, "-f", format], out).await
}

fn text_codec(to: SubtitleFormat) -> (&'static str, &'static str) {
    match to {
        SubtitleFormat::WebVtt => ("webvtt", "webvtt"),
        _ => ("srt", "srt"),
    }
}

/// Subtitle stream `index` of `input` written to `out` by ffmpeg in
/// `jail`, with `codec_args` saying how: what [`extract_sup`] and
/// [`convert_text`] both run.
async fn copy_stream(
    ffmpeg: &Tool,
    jail: Jail,
    input: &Path,
    index: u32,
    codec_args: &[&str],
    out: &Path,
) -> Result<(), OcrError> {
    let mut command = spawnable(ffmpeg, writing(jail, out)).await?;
    command
        .args(["-hide_banner", "-nostdin", "-y", "-v", "error", "-i"])
        .arg(input)
        .args(["-map", &format!("0:{index}")])
        .args(codec_args)
        .arg(out);
    let output = run(command, "ffmpeg", Feed::Nothing, MAX_STDERR, PASS_TIMEOUT).await?;
    succeeded(output, "ffmpeg")?;
    Ok(())
}

/// The cues with their text; cues tesseract found no text in are left out.
pub fn text_cues(cues: &[CueTime], texts: Vec<String>) -> Vec<TextCue> {
    cues.iter()
        .zip(texts)
        .filter(|(_, text)| !text.is_empty())
        .map(|(cue, text)| TextCue {
            start_ms: cue.start_ms,
            end_ms: cue.end_ms,
            text,
        })
        .collect()
}

/// As SubRip.
pub fn srt(cues: &[TextCue]) -> String {
    let mut out = String::new();
    for (n, cue) in cues.iter().enumerate() {
        let _ = write!(
            out,
            "{}\n{} --> {}\n{}\n\n",
            n + 1,
            timestamp(cue.start_ms, ','),
            timestamp(cue.end_ms, ','),
            cue.text
        );
    }
    out
}

/// As WebVTT.
pub fn webvtt(cues: &[TextCue]) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for cue in cues {
        let _ = write!(
            out,
            "{} --> {}\n{}\n\n",
            timestamp(cue.start_ms, '.'),
            timestamp(cue.end_ms, '.'),
            // A blank line would end the cue early; "-->" would start one.
            cue.text.replace("-->", "->")
        );
    }
    out
}

fn timestamp(ms: u64, separator: char) -> String {
    format!(
        "{:02}:{:02}:{:02}{separator}{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

/// A page as subtitle text: its non-blank lines, trimmed. tesseract reads
/// a capital I on its own as a bar, about a hundred times a film; a
/// subtitle never has a bar.
fn tidy(page: &str) -> String {
    page.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
        .replace('|', "I")
}

/// `tesseract --list-langs`: a heading, then a code a line. `osd` is
/// orientation detection, not a language.
fn list_langs(text: &str) -> BTreeSet<String> {
    text.lines()
        .skip(1)
        .map(str::trim)
        .filter(|l| !l.is_empty() && *l != "osd")
        .map(str::to_owned)
        .collect()
}

/// tesseract names languages by ISO 639-2/T; Matroska tags often use
/// 639-2/B, which differs for these, and a subtitle file's name (and a
/// `.idx`) ISO 639-1, two letters.
fn tesseract_code(tag: &str) -> &str {
    match tag {
        "ar" => "ara",
        "bg" => "bul",
        "ca" => "cat",
        "cs" => "ces",
        "da" => "dan",
        "de" => "deu",
        "el" => "ell",
        "en" => "eng",
        "es" => "spa",
        "et" => "est",
        "eu" => "eus",
        "fa" => "fas",
        "fi" => "fin",
        "fr" => "fra",
        "ga" => "gle",
        "gl" => "glg",
        "he" => "heb",
        "hi" => "hin",
        "hr" => "hrv",
        "hu" => "hun",
        "id" => "ind",
        "is" => "isl",
        "it" => "ita",
        "ja" => "jpn",
        "ko" => "kor",
        "lt" => "lit",
        "lv" => "lav",
        "ms" => "msa",
        "nl" => "nld",
        "no" | "nb" => "nor",
        "pl" => "pol",
        "pt" => "por",
        "ro" => "ron",
        "ru" => "rus",
        "sk" => "slk",
        "sl" => "slv",
        "sr" => "srp",
        "sv" => "swe",
        "th" => "tha",
        "tr" => "tur",
        "uk" => "ukr",
        "vi" => "vie",
        "zh" => "chi_sim",
        "alb" => "sqi",
        "arm" => "hye",
        "baq" => "eus",
        "bur" => "mya",
        "chi" | "zho" => "chi_sim",
        "cze" => "ces",
        "dut" => "nld",
        "fre" => "fra",
        "geo" => "kat",
        "ger" => "deu",
        "gre" => "ell",
        "ice" => "isl",
        "mac" => "mkd",
        "may" => "msa",
        "per" => "fas",
        "rum" => "ron",
        "slo" => "slk",
        "tib" => "bod",
        "wel" => "cym",
        other => other,
    }
}

/// `tool`'s command in `jail`, or why it cannot run; made off the async
/// workers ([`Tool::command_off_thread`]): a media input on a network
/// mount can block.
async fn spawnable(tool: &Tool, jail: Jail) -> Result<Command, OcrError> {
    tool.command_off_thread(jail)
        .await
        .map_err(|source| OcrError::Spawn {
            program: tool.program().display().to_string(),
            source,
        })
}

/// `jail`, writing to `out`'s directory.
fn writing(jail: Jail, out: &Path) -> Jail {
    match out.parent() {
        Some(dir) => jail.writes(dir),
        None => jail,
    }
}

/// What a run reads on its standard input.
pub(crate) enum Feed {
    Nothing,
    /// A file the server opened.
    File(std::fs::File),
    /// Bytes written to a pipe while the run reads them.
    Bytes(Vec<u8>),
}

/// What a run wrote, and how it ended.
pub(crate) struct Ran {
    pub(crate) status: std::process::ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

/// `command` run to its end, as `program` in errors (PX-09 B-3): killed
/// if it runs past `timeout`, or writes more than `max_stdout` bytes to
/// stdout, which is what a crafted file can make it do. Only the first
/// [`MAX_STDERR`] bytes of stderr are kept; the rest is read and dropped,
/// so a chatty program does not stall on a full pipe. It reads `feed`.
pub(crate) async fn run(
    mut command: Command,
    program: &str,
    feed: Feed,
    max_stdout: usize,
    timeout: Duration,
) -> Result<Ran, OcrError> {
    let (stdin, bytes) = match feed {
        Feed::Nothing => (Stdio::null(), None),
        Feed::File(file) => (Stdio::from(file), None),
        Feed::Bytes(bytes) => (Stdio::piped(), Some(bytes)),
    };
    let mut child = command
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| OcrError::Spawn {
            program: program.to_owned(),
            source,
        })?;
    let mut stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    // Written beside the reads, so neither side waits on a full pipe. A
    // run that stops reading (it failed, or was killed) ends the write.
    if let (Some(bytes), Some(mut pipe)) = (bytes, child.stdin.take()) {
        tokio::spawn(async move {
            let _ = tokio::io::AsyncWriteExt::write_all(&mut pipe, &bytes).await;
        });
    }
    let errors = tokio::spawn(crate::capped::read_some(Some(stderr), MAX_STDERR));
    let ran = async {
        let mut out = Vec::new();
        (&mut stdout)
            .take(max_stdout as u64 + 1)
            .read_to_end(&mut out)
            .await?;
        if out.len() > max_stdout {
            let _ = child.kill().await;
            return Err(OcrError::TooMuch {
                program: program.to_owned(),
                limit: max_stdout,
            });
        }
        let status = child.wait().await?;
        Ok((status, out))
    };
    // Past the time, the future and the child in it are dropped, which
    // kills it.
    let (status, stdout) =
        tokio::time::timeout(timeout, ran)
            .await
            .map_err(|_| OcrError::TimedOut {
                program: program.to_owned(),
                after: timeout,
            })??;
    let stderr = errors.await.unwrap_or_default();
    Ok(Ran {
        status,
        stdout,
        stderr,
    })
}

/// `output`, if `program` succeeded; why not, from its last line, if not.
fn succeeded(output: Ran, program: &str) -> Result<Ran, OcrError> {
    if output.status.success() {
        Ok(output)
    } else {
        Err(OcrError::Failed {
            program: program.into(),
            reason: last_line(&output.stderr),
        })
    }
}

pub(crate) fn last_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("no message")
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Tool::unconfined("sh")
            .command(&Jail::new(Limits::OCR))
            .unwrap();
        command.args(["-c", script]);
        command
    }

    /// PX-09 B-3: a program that never ends is killed at its time.
    #[tokio::test]
    async fn a_program_past_its_time_is_killed() {
        let started = std::time::Instant::now();
        let ran = run(
            sh("sleep 30"),
            "sleep",
            Feed::Nothing,
            1024,
            Duration::from_millis(200),
        )
        .await;
        assert!(
            matches!(ran, Err(OcrError::TimedOut { .. })),
            "{:?}",
            ran.err()
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// One that writes more than its cap is stopped there, not buffered.
    #[tokio::test]
    async fn a_program_writing_past_its_cap_is_stopped() {
        let ran = run(
            sh("head -c 1000000 /dev/zero"),
            "head",
            Feed::Nothing,
            1000,
            Duration::from_secs(30),
        )
        .await;
        assert!(
            matches!(ran, Err(OcrError::TooMuch { limit: 1000, .. })),
            "{:?}",
            ran.err()
        );
    }

    /// Much on stderr is read and dropped, so the program does not stall
    /// on a full pipe, and its stdout comes back whole.
    #[tokio::test]
    async fn a_chatty_program_runs_to_its_end() {
        let ran = run(
            sh("head -c 1000000 /dev/zero >&2; echo done"),
            "sh",
            Feed::Nothing,
            1000,
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        assert!(ran.status.success());
        assert_eq!(ran.stdout, b"done\n");
        assert_eq!(ran.stderr.len(), MAX_STDERR);
    }

    /// Bytes fed to a run arrive whole, written while it reads: more
    /// than a pipe holds.
    #[tokio::test]
    async fn a_program_reads_the_bytes_it_is_fed() {
        let ran = run(
            sh("wc -c"),
            "wc",
            Feed::Bytes(vec![b'x'; 1_000_000]),
            1000,
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        assert_eq!(String::from_utf8_lossy(&ran.stdout).trim(), "1000000");
    }

    fn ocr(languages: &[&str]) -> Ocr {
        Ocr {
            tesseract: Tool::unconfined("tesseract"),
            languages: languages.iter().map(|l| (*l).to_owned()).collect(),
        }
    }

    #[test]
    fn the_language_list_is_the_codes_without_osd() {
        let listed =
            "List of available languages in \"/nix/store/x/share/tessdata/\" (3):\neng\nfra\nosd\n";
        assert_eq!(
            list_langs(listed),
            BTreeSet::from(["eng".into(), "fra".into()])
        );
    }

    #[test]
    fn a_track_is_read_in_its_own_language_or_not_at_all() {
        let ocr = ocr(&["eng", "fra", "deu"]);
        assert_eq!(ocr.language_for(None).as_deref(), Some("eng"));
        assert_eq!(ocr.language_for(Some("und")).as_deref(), Some("eng"));
        // Matroska's bibliographic codes.
        assert_eq!(ocr.language_for(Some("fre")).as_deref(), Some("fra"));
        assert_eq!(ocr.language_for(Some("GER")).as_deref(), Some("deu"));
        assert_eq!(ocr.language_for(Some("fra")).as_deref(), Some("fra"));
        // A file's name: `Movie.en.idx`, `Movie.fr.sup`.
        assert_eq!(ocr.language_for(Some("en")).as_deref(), Some("eng"));
        assert_eq!(ocr.language_for(Some("fr")).as_deref(), Some("fra"));
        // Russian data is not installed: no guess with English.
        assert_eq!(ocr.language_for(Some("rus")), None);
    }

    #[test]
    fn no_english_data_means_no_untagged_tracks() {
        assert_eq!(ocr(&["fra"]).language_for(None), None);
        assert_eq!(Ocr::disabled().language_for(Some("eng")), None);
    }

    fn cues() -> Vec<TextCue> {
        vec![
            TextCue {
                start_ms: 1_000,
                end_ms: 3_500,
                text: "Hello there".into(),
            },
            TextCue {
                start_ms: 3_725_042,
                end_ms: 3_727_000,
                text: "General Kenobi\n--> you are a bold one".into(),
            },
        ]
    }

    #[test]
    fn subrip_numbers_its_cues_with_comma_milliseconds() {
        assert_eq!(
            srt(&cues()),
            "1\n00:00:01,000 --> 00:00:03,500\nHello there\n\n\
             2\n01:02:05,042 --> 01:02:07,000\nGeneral Kenobi\n--> you are a bold one\n\n"
        );
    }

    #[test]
    fn webvtt_has_its_header_and_no_stray_arrows() {
        let vtt = webvtt(&cues());
        assert!(vtt.starts_with("WEBVTT\n\n00:00:01.000 --> 00:00:03.500\nHello there\n\n"));
        assert!(
            vtt.contains("01:02:05.042 --> 01:02:07.000\nGeneral Kenobi\n-> you are a bold one\n")
        );
    }

    /// ffprobe's answer for a DVB track in an MPEG-TS whose clock starts at
    /// 1.48 s: packets in 90 kHz ticks, data as hex dumps.
    const PROBED: &str = r#"{
        "packets": [
            {"pts": 223200, "data": "\n00000000: 0f10 0001 0002 1e0b 0f80 0001 0000 6162  ..............ab\n00000010: 6364                                     cd\n"},
            {"pts": 448200, "duration": 900, "data": "\n00000000: ff                                       .\n"},
            {"data": "\n00000000: 00                                       .\n"}
        ],
        "streams": [{"time_base": "1/90000", "extradata": "\n00000000: 7061 6c65 7474 653a                      palette:\n"}],
        "format": {"start_time": "1.480000"}
    }"#;

    /// A file's time base or times past what the arithmetic holds are
    /// refused or left out, not overflowed or wrapped. Review 2026-10-11
    /// L67.
    #[test]
    fn times_past_what_fits_are_refused_not_wrapped() {
        let huge_base = r#"{"packets": [{"pts": 1000000000000, "data": ""}],
            "streams": [{"time_base": "99999999999999999999999999999/1"}]}"#;
        assert!(track(huge_base.as_bytes(), false).is_err());
        let far = r#"{"packets": [{"pts": 9223372036854775807, "data": ""},
                                {"pts": 5, "duration": 9223372036854775807, "data": ""}],
            "streams": [{"time_base": "1/1"}]}"#;
        let read = track(far.as_bytes(), false).unwrap();
        let packets: Vec<_> = read
            .packets
            .iter()
            .map(|p| (p.pts_ms, p.duration_ms))
            .collect();
        assert_eq!(packets, [(5_000, None)]);
    }

    #[test]
    fn a_probed_track_counts_from_the_file_s_start_and_its_dumps_are_bytes() {
        let read = track(PROBED.as_bytes(), true).unwrap();
        assert_eq!(read.extradata, b"palette:");
        let packets: Vec<_> = read
            .packets
            .iter()
            .map(|p| (p.pts_ms, p.duration_ms, p.data.clone()))
            .collect();
        // The packet without a time is left out; the text column is not
        // read as hex, though "abcd" would pass for it.
        assert_eq!(
            packets,
            [
                (
                    1_000,
                    None,
                    vec![
                        0x0f, 0x10, 0, 1, 0, 2, 0x1e, 0x0b, 0x0f, 0x80, 0, 1, 0, 0, 0x61, 0x62,
                        0x63, 0x64
                    ]
                ),
                (3_500, Some(10), vec![0xff]),
            ]
        );
    }

    #[test]
    fn a_subtitle_file_keeps_its_own_times() {
        let read = track(PROBED.as_bytes(), false).unwrap();
        assert_eq!(read.packets[0].pts_ms, 2_480);
    }

    #[test]
    fn blank_lines_go_and_empty_cues_are_dropped() {
        assert_eq!(tidy("  Hello \n\n there\n\n"), "Hello\nthere");
        assert_eq!(tidy("| said, | know.\n"), "I said, I know.");
        let cue = |start_ms| CueTime {
            start_ms,
            end_ms: start_ms + 1,
        };
        let kept = text_cues(&[cue(0), cue(10)], vec![String::new(), "hi".into()]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].start_ms, 10);
    }
}
