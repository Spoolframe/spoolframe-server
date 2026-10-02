//! Bitmap subtitles to text (P2-07): a PGS track's cues ([`crate::pgs`])
//! read by tesseract, and written out as SubRip or WebVTT.
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
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::Semaphore;

use spoolframe_types::SubtitleFormat;

use crate::pgs;

/// Cues per tesseract process: enough that loading the model is a small
/// part of each run, few enough that the work spreads over every thread
/// and progress moves.
const CHUNK: usize = 40;

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
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// tesseract, and the languages it has data for.
#[derive(Debug, Clone)]
pub struct Ocr {
    tesseract: PathBuf,
    languages: BTreeSet<String>,
}

impl Ocr {
    /// No OCR: the planner never offers it.
    pub fn disabled() -> Self {
        Self {
            tesseract: PathBuf::new(),
            languages: BTreeSet::new(),
        }
    }

    /// `tesseract` with data for `languages`, as [`Ocr::detect`] would find
    /// them.
    pub fn new(tesseract: PathBuf, languages: impl IntoIterator<Item = String>) -> Self {
        Self {
            tesseract,
            languages: languages.into_iter().collect(),
        }
    }

    /// Asks `tesseract` which languages it has. If it cannot be run, OCR
    /// is off, and the log says why.
    pub async fn detect(tesseract: &Path) -> Self {
        let output = tokio::process::Command::new(tesseract)
            .arg("--list-langs")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await;
        let languages = match output {
            Ok(out) if out.status.success() => list_langs(&String::from_utf8_lossy(&out.stdout)),
            Ok(out) => {
                tracing::warn!(
                    tesseract = %tesseract.display(),
                    stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                    "tesseract cannot list its languages; subtitle OCR is off"
                );
                BTreeSet::new()
            }
            Err(e) => {
                tracing::warn!(
                    tesseract = %tesseract.display(),
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

    /// The text of each cue, in order, read with `threads` tesseract
    /// processes side by side. The pictures go in `work`, which the caller
    /// owns and removes. `progress` is told how many cues are done.
    pub async fn recognise(
        &self,
        cues: &[pgs::Cue],
        language: &str,
        work: &Path,
        threads: usize,
        progress: &(dyn Fn(usize) + Sync),
    ) -> Result<Vec<String>, OcrError> {
        tokio::fs::create_dir_all(work).await?;
        for (i, cue) in cues.iter().enumerate() {
            tokio::fs::write(work.join(format!("{i}.pgm")), cue.image.to_pgm()).await?;
        }
        let permits = Arc::new(Semaphore::new(threads.max(1)));
        let done = Arc::new(AtomicUsize::new(0));
        let mut runs = tokio::task::JoinSet::new();
        for (n, first) in (0..cues.len()).step_by(CHUNK).enumerate() {
            let pictures: Vec<PathBuf> = (first..(first + CHUNK).min(cues.len()))
                .map(|i| work.join(format!("{i}.pgm")))
                .collect();
            let list = work.join(format!("chunk{n}.txt"));
            let mut text = String::new();
            for p in &pictures {
                let _ = writeln!(text, "{}", p.display());
            }
            tokio::fs::write(&list, text).await?;
            let (tesseract, language) = (self.tesseract.clone(), language.to_owned());
            let (permits, done) = (permits.clone(), done.clone());
            runs.spawn(async move {
                let _permit = permits.acquire_owned().await.expect("never closed");
                let pages = read_pages(&tesseract, &list, &language, pictures.len()).await?;
                done.fetch_add(pages.len(), Ordering::Relaxed);
                Ok::<_, OcrError>((first, pages))
            });
        }
        let mut texts = vec![String::new(); cues.len()];
        while let Some(run) = runs.join_next().await {
            let (first, pages) = run.expect("an OCR run never panics")?;
            for (i, page) in pages.into_iter().enumerate() {
                texts[first + i] = page;
            }
            progress(done.load(Ordering::Relaxed));
        }
        Ok(texts)
    }
}

/// One tesseract run over a list of pictures: one page of text each.
async fn read_pages(
    tesseract: &Path,
    list: &Path,
    language: &str,
    expected: usize,
) -> Result<Vec<String>, OcrError> {
    let output = tokio::process::Command::new(tesseract)
        .arg(list)
        .arg("stdout")
        .args(["-l", language, "--psm", "6"])
        // One thread each: the parallelism is the processes, and
        // tesseract's own threads only fight them for cores.
        .env("OMP_THREAD_LIMIT", "1")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|source| OcrError::Spawn {
            program: tesseract.display().to_string(),
            source,
        })?;
    if !output.status.success() {
        return Err(OcrError::Failed {
            program: "tesseract".into(),
            reason: last_line(&output.stderr),
        });
    }
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
    ffmpeg: &Path,
    input: &Path,
    index: u32,
    out: &Path,
) -> Result<(), OcrError> {
    let output = tokio::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-nostdin", "-y", "-v", "error", "-i"])
        .arg(input)
        .args(["-map", &format!("0:{index}"), "-c", "copy", "-f", "sup"])
        .arg(out)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|source| OcrError::Spawn {
            program: ffmpeg.display().to_string(),
            source,
        })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(OcrError::Failed {
            program: "ffmpeg".into(),
            reason: last_line(&output.stderr),
        })
    }
}

/// Converts text subtitle stream `index` of `input` (ffprobe's stream
/// index; 0 for a subtitle file of its own) to `to`, SubRip or WebVTT,
/// with ffmpeg. Not OCR, but the other way a track becomes the text a
/// client takes: ASS, `mov_text` or SubRip to WebVTT, say. ASS styling is
/// lost.
pub async fn convert_text(
    ffmpeg: &Path,
    input: &Path,
    index: u32,
    to: SubtitleFormat,
    out: &Path,
) -> Result<(), OcrError> {
    let (codec, format) = match to {
        SubtitleFormat::WebVtt => ("webvtt", "webvtt"),
        _ => ("srt", "srt"),
    };
    let output = tokio::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-nostdin", "-y", "-v", "error", "-i"])
        .arg(input)
        .args(["-map", &format!("0:{index}"), "-c:s", codec, "-f", format])
        .arg(out)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|source| OcrError::Spawn {
            program: ffmpeg.display().to_string(),
            source,
        })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(OcrError::Failed {
            program: "ffmpeg".into(),
            reason: last_line(&output.stderr),
        })
    }
}

/// The cues with their text; cues tesseract found no text in are left out.
pub fn text_cues(cues: &[pgs::Cue], texts: Vec<String>) -> Vec<TextCue> {
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

/// A page as subtitle text: its non-blank lines, trimmed.
fn tidy(page: &str) -> String {
    page.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
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
/// 639-2/B, which differs for these.
fn tesseract_code(tag: &str) -> &str {
    match tag {
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

fn last_line(stderr: &[u8]) -> String {
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

    fn ocr(languages: &[&str]) -> Ocr {
        Ocr {
            tesseract: PathBuf::from("tesseract"),
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

    #[test]
    fn blank_lines_go_and_empty_cues_are_dropped() {
        assert_eq!(tidy("  Hello \n\n there\n\n"), "Hello\nthere");
        let image = pgs::Image {
            width: 1,
            height: 1,
            pixels: vec![255],
        };
        let cue = |start_ms| pgs::Cue {
            start_ms,
            end_ms: start_ms + 1,
            image: image.clone(),
        };
        let kept = text_cues(&[cue(0), cue(10)], vec![String::new(), "hi".into()]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].start_ms, 10);
    }
}
