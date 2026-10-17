//! A subtitle file from outside the server (PX-02: a download from a
//! subtitle site), made into one the server keeps: UTF-8, of a format it
//! reads, with cues it can read. The file is untrusted: it is capped in
//! size, decoded by `encoding_rs` (never a C library), and its cues are
//! read by [`crate::subsync`]'s parser, the one a sync reads it with.
//! Nothing the site said about it (its name, its format) is believed:
//! the format is read from the text itself.
//!
//! A site may send it zipped (SubDL does): [`unpack`] takes the one
//! subtitle out of the archive, in memory, never by a name in it, with
//! the archive, its entries and what is read capped.

use crate::subsync::{self, TextFormat};

/// The largest downloaded subtitle taken: a film's SRT is some 100 KB, an
/// ASS with heavy styling a few MB.
pub const MAX_DOWNLOAD_BYTES: usize = 8 << 20;

/// The largest archive taken: SubDL's are a few hundred KB.
pub const MAX_ARCHIVE_BYTES: usize = 8 << 20;

/// The most entries an archive may list: a film's subtitle and a note or
/// two; a season's pack some thirty.
pub const MAX_ARCHIVE_ENTRIES: usize = 64;

/// A downloaded subtitle as the server keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    /// The text, UTF-8, without a byte order mark.
    pub bytes: Vec<u8>,
    pub format: TextFormat,
    /// The encoding it came in, by its WHATWG name: `UTF-8`,
    /// `windows-1251`.
    pub encoding: &'static str,
    /// How many cues it has.
    pub cues: usize,
}

/// Why a downloaded file is not kept.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FileError {
    #[error("the subtitle is larger than {MAX_DOWNLOAD_BYTES} bytes")]
    TooLarge,
    /// It has NUL characters once decoded: a binary file, not text.
    #[error("the file is not text")]
    NotText,
    /// No cue of it could be read, in the format its text is.
    #[error("the file is not a subtitle the server reads (SRT, WebVTT or ASS): {0}")]
    NotSubtitle(String),
    /// A zip archive that cannot be read, that lists too many entries,
    /// one whose name leads out of it, or one that is encrypted.
    #[error("the archive is not read: {0}")]
    BadArchive(String),
    /// A readable archive with no SRT, WebVTT or ASS file in it.
    #[error("the archive has no subtitle the server reads (SRT, WebVTT or ASS)")]
    NoSubtitleInArchive,
    /// An archive of several subtitles (a season's pack) none of whose
    /// names says the episode asked for.
    #[error("the archive has several subtitles and none of their names says this episode")]
    EpisodeNotInArchive,
}

impl Kept {
    /// The extension its file is kept under: `srt`, `vtt` or `ass`.
    pub fn extension(&self) -> &'static str {
        extension(self.format)
    }

    /// The codec name a track of it has, as ffprobe would name it.
    pub fn codec(&self) -> &'static str {
        match self.format {
            TextFormat::Srt => "subrip",
            TextFormat::WebVtt => "webvtt",
            TextFormat::Ass => "ass",
        }
    }
}

/// The extension a file of `format` is kept under.
pub fn extension(format: TextFormat) -> &'static str {
    match format {
        TextFormat::Srt => "srt",
        TextFormat::WebVtt => "vtt",
        TextFormat::Ass => "ass",
    }
}

/// `bytes`, a downloaded subtitle, as the server keeps it: decoded from
/// the encoding its byte order mark says, else UTF-8 when it is valid
/// UTF-8, else the encoding its text most likely is (CP-1251, CP-1252
/// and the like); its format read from its text; its cues read.
pub fn keep(bytes: &[u8]) -> Result<Kept, FileError> {
    if bytes.len() > MAX_DOWNLOAD_BYTES {
        return Err(FileError::TooLarge);
    }
    let (text, encoding) = decode(bytes);
    if text.contains('\0') {
        return Err(FileError::NotText);
    }
    let format = format_of(&text);
    let bytes = text.into_bytes();
    // UTF-8 may grow a file a little (CP-1251's letters are two bytes
    // each); what the sync reads is capped again there.
    let cues = subsync::cues(&bytes, format)
        .map_err(|e| FileError::NotSubtitle(e.to_string()))?
        .len();
    Ok(Kept {
        bytes,
        format,
        encoding: encoding.name(),
        cues,
    })
}

/// Whether `bytes` is a zip archive, by its first local header.
pub fn is_archive(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04")
}

/// The subtitle in the zip archive `bytes`, as it is in it (for [`keep`]
/// to check): its one SRT, WebVTT or ASS file. Of several, for a film,
/// the largest (two discs, an SDH beside it); for episode `episode`
/// (season, episode), the one whose name says it ([`episode_match`]),
/// the largest of those that say it as well; and none when no name does:
/// another episode's lines would be every user's. Nothing is written anywhere: an entry is
/// only read, by its place, so its name cannot lead out of anywhere; an
/// archive with a name that would is refused all the same, as made to
/// harm. Entries in `__MACOSX/` are Finder's, not files.
pub fn unpack(bytes: &[u8], episode: Option<(u32, u32)>) -> Result<Vec<u8>, FileError> {
    use std::io::Read;
    if bytes.len() > MAX_ARCHIVE_BYTES {
        return Err(FileError::TooLarge);
    }
    let bad = |e: zip::result::ZipError| FileError::BadArchive(e.to_string());
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(bad)?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(FileError::BadArchive(format!(
            "it lists {} entries, more than {MAX_ARCHIVE_ENTRIES}",
            archive.len()
        )));
    }
    // (place, name, size) of each subtitle in it.
    let mut subtitles: Vec<(usize, String, u64)> = Vec::new();
    for at in 0..archive.len() {
        let entry = archive.by_index_raw(at).map_err(bad)?;
        let name = entry.name().replace('\\', "/");
        if entry.enclosed_name().is_none() || leads_out(&name) {
            return Err(FileError::BadArchive(
                "an entry's name leads out of the archive".into(),
            ));
        }
        if entry.is_dir() || name.starts_with("__MACOSX/") || !is_subtitle_name(&name) {
            continue;
        }
        if entry.encrypted() {
            return Err(FileError::BadArchive("its subtitle is encrypted".into()));
        }
        subtitles.push((at, name, entry.size()));
    }
    if let Some((season, number)) = episode
        && subtitles.len() > 1
    {
        let matched: Vec<(u8, (usize, String, u64))> = subtitles
            .iter()
            .filter_map(|entry| Some((episode_match(&entry.1, season, number)?, entry.clone())))
            .collect();
        let best = matched
            .iter()
            .map(|(tier, _)| *tier)
            .min()
            .ok_or(FileError::EpisodeNotInArchive)?;
        let of_it: Vec<_> = matched
            .into_iter()
            .filter(|(tier, _)| *tier == best)
            .map(|(_, entry)| entry)
            .collect();
        // A bare `105` is taken only when one name has it.
        if best == 4 && of_it.len() > 1 {
            return Err(FileError::EpisodeNotInArchive);
        }
        subtitles = of_it;
    }
    // The largest; of equal ones, the first by name.
    let (at, _, _) = subtitles
        .into_iter()
        .min_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)))
        .ok_or(FileError::NoSubtitleInArchive)?;
    let entry = archive.by_index(at).map_err(bad)?;
    // What it says its size is is not believed: read one byte past the
    // cap, to tell a file at the cap from a larger one. Read to its end,
    // its checksum is checked.
    let mut out = Vec::new();
    entry
        .take(MAX_DOWNLOAD_BYTES as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| FileError::BadArchive(e.to_string()))?;
    if out.len() > MAX_DOWNLOAD_BYTES {
        return Err(FileError::TooLarge);
    }
    Ok(out)
}

/// Whether an entry's name (`/` for `\\`) is absolute, a drive's, or
/// climbs up out of the archive.
fn leads_out(name: &str) -> bool {
    name.starts_with('/')
        || name.as_bytes().get(1) == Some(&b':')
        || name.split('/').any(|part| part == "..")
}

/// Whether an archive entry's name is a subtitle the server reads, by
/// its extension.
fn is_subtitle_name(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, ext)| {
        ["srt", "vtt", "ass", "ssa"]
            .iter()
            .any(|known| ext.eq_ignore_ascii_case(known))
    })
}

/// How surely `name` says season `season`'s episode `number`, the
/// numbers never part of larger ones; `None` when it does not:
/// 1, `S01E05`, `s1e5`, `1x05`; 2, `E05`, `EP05`, `Episode 5`; 3, a
/// two-digit `05` on its own (`Show - 05`, `Show_05`), or any number
/// after `- `; 4, `105`, three digits on their own, season then episode.
pub fn episode_match(name: &str, season: u32, number: u32) -> Option<u8> {
    let name = name.to_ascii_lowercase();
    let b = name.as_bytes();
    // The number at `from` (at most 4 digits) and where it ends.
    let digits = |from: usize| -> (u32, usize) {
        let mut end = from;
        let mut value: u32 = 0;
        while end < b.len() && b[end].is_ascii_digit() && end - from < 4 {
            value = value * 10 + u32::from(b[end] - b'0');
            end += 1;
        }
        (value, end)
    };
    let alnum = |at: usize| b.get(at).is_some_and(u8::is_ascii_alphanumeric);
    let digit = |at: usize| b.get(at).is_some_and(u8::is_ascii_digit);
    let mut best: Option<u8> = None;
    let mut found = |tier: u8| best = Some(best.map_or(tier, |b| b.min(tier)));
    for at in 0..b.len() {
        let starts = at == 0 || !alnum(at - 1);
        if !starts {
            continue;
        }
        // `s01e05`
        if b[at] == b's' {
            let (s, end) = digits(at + 1);
            if end > at + 1 && b.get(end) == Some(&b'e') {
                let (e, stop) = digits(end + 1);
                if stop > end + 1 && !digit(stop) && (s, e) == (season, number) {
                    found(1);
                }
            }
        }
        if b[at].is_ascii_digit() {
            let (s, end) = digits(at);
            // `1x05`
            if b.get(end) == Some(&b'x') {
                let (e, stop) = digits(end + 1);
                if stop > end + 1 && !digit(stop) && (s, e) == (season, number) {
                    found(1);
                }
            } else if !alnum(end) {
                let len = end - at;
                let after_dash = at >= 2 && &b[at - 2..at] == b"- ";
                // `05`, or `- 5`
                if (len == 2 || after_dash) && s == number {
                    found(3);
                }
                // `105`
                if len == 3 && season < 10 && s == season * 100 + number {
                    found(4);
                }
            }
        }
        // `e05`, `ep05`, `episode 5`, `episode.05`
        for word in ["episode", "ep", "e"] {
            let Some(rest) = name[at..].strip_prefix(word) else {
                continue;
            };
            let from = b.len() - rest.len();
            let from = if word == "episode" && matches!(b.get(from), Some(b' ' | b'.' | b'_')) {
                from + 1
            } else {
                from
            };
            let (e, stop) = digits(from);
            if stop > from && !digit(stop) && e == number {
                found(2);
            }
            break;
        }
    }
    best
}

/// The text of `bytes`, without a byte order mark, and the encoding it was
/// in. Bytes that do not decode become U+FFFD, as a browser shows them.
fn decode(bytes: &[u8]) -> (String, &'static encoding_rs::Encoding) {
    if let Some((encoding, bom)) = encoding_rs::Encoding::for_bom(bytes) {
        let (text, _) = encoding.decode_without_bom_handling(&bytes[bom..]);
        return (text.into_owned(), encoding);
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return (text.to_owned(), encoding_rs::UTF_8);
    }
    let mut detector = chardetng::EncodingDetector::new();
    detector.feed(bytes, true);
    let encoding = detector.guess(None, true);
    let (text, _) = encoding.decode_without_bom_handling(bytes);
    (text.into_owned(), encoding)
}

/// The format `text` is in, by how it begins: WebVTT says so on its first
/// line, ASS and SSA have a `[Script Info]` section; anything else is
/// read as SRT.
fn format_of(text: &str) -> TextFormat {
    let start = text.trim_start();
    if start.starts_with("WEBVTT") {
        return TextFormat::WebVtt;
    }
    let head: String = start.chars().take(64).collect();
    if head.to_ascii_lowercase().starts_with("[script info]") {
        return TextFormat::Ass;
    }
    TextFormat::Srt
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRT: &str = "1\r\n00:00:01,000 --> 00:00:02,500\r\nПривет, мир\r\n\r\n\
                       2\r\n00:00:03,000 --> 00:00:04,000\r\nКак дела?\r\n";

    #[test]
    fn a_cp1251_srt_is_kept_as_utf8() {
        let (cp1251, _, unmappable) = encoding_rs::WINDOWS_1251.encode(SRT);
        assert!(!unmappable);
        assert!(std::str::from_utf8(&cp1251).is_err());
        let kept = keep(&cp1251).unwrap();
        assert_eq!(std::str::from_utf8(&kept.bytes).unwrap(), SRT);
        assert_eq!(kept.encoding, "windows-1251");
        assert_eq!(kept.format, TextFormat::Srt);
        assert_eq!(kept.cues, 2);
    }

    #[test]
    fn a_utf8_file_is_kept_as_it_came_without_its_bom() {
        let with_bom = [b"\xef\xbb\xbf".as_slice(), SRT.as_bytes()].concat();
        let kept = keep(&with_bom).unwrap();
        assert_eq!(kept.bytes, SRT.as_bytes());
        assert_eq!(kept.encoding, "UTF-8");
    }

    #[test]
    fn a_utf16_file_is_decoded_by_its_bom() {
        let mut utf16 = vec![0xff, 0xfe];
        for unit in SRT.encode_utf16() {
            utf16.extend(unit.to_le_bytes());
        }
        let kept = keep(&utf16).unwrap();
        assert_eq!(kept.bytes, SRT.as_bytes());
        assert_eq!(kept.encoding, "UTF-16LE");
    }

    /// A zip of `entries` (name, bytes), deflated.
    fn zipped(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in entries {
            zip.start_file(*name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn a_zipped_subtitle_is_taken_out_and_kept() {
        let (cp1251, _, _) = encoding_rs::WINDOWS_1251.encode(SRT);
        let archive = zipped(&[
            ("Readme.txt", b"made by someone"),
            ("Film.2019.1080p/Film.2019.1080p.srt", &cp1251),
        ]);
        assert!(is_archive(&archive));
        assert!(!is_archive(SRT.as_bytes()));
        let taken = unpack(&archive, None).unwrap();
        assert_eq!(taken, cp1251.as_ref());
        assert_eq!(
            std::str::from_utf8(&keep(&taken).unwrap().bytes).unwrap(),
            SRT
        );
    }

    /// Of a season's pack, the episode's; of several for a film (two
    /// discs, an SDH), the largest; of a pack none of whose names says
    /// the episode, none: another episode's lines must not be kept.
    #[test]
    fn of_several_the_episode_s_or_for_a_film_the_largest() {
        let one = SRT.as_bytes();
        let longer = [SRT, SRT].concat();
        let pack = zipped(&[
            ("Show.S01E01.srt", longer.as_bytes()),
            ("Show.S01E02.srt", one),
            ("Show.S01E12.srt", longer.as_bytes()),
            ("__MACOSX/._Show.S01E02.srt", longer.as_bytes()),
        ]);
        assert_eq!(unpack(&pack, Some((1, 2))).unwrap(), one);
        assert_eq!(
            unpack(
                &zipped(&[("show.1x02.ass", one), ("show.1x03.ass", longer.as_bytes())]),
                Some((1, 2))
            )
            .unwrap(),
            one
        );
        // An episode the pack does not have: refused.
        assert_eq!(
            unpack(&pack, Some((2, 1))),
            Err(FileError::EpisodeNotInArchive)
        );
        // A film: the largest.
        assert_eq!(unpack(&pack, None).unwrap(), longer.as_bytes());
        // One subtitle alone is taken, whatever its name says.
        assert_eq!(
            unpack(&zipped(&[("Show.S01E09.srt", one)]), Some((1, 2))).unwrap(),
            one
        );
    }

    #[test]
    fn an_episode_is_read_from_the_names_packs_use() {
        for name in [
            "Show.S01E05.srt",
            "show s1e5.srt",
            "Show.1x05.srt",
            "Show.E05.srt",
            "Show EP05.srt",
            "Show Episode 5.srt",
            "Show - 05.srt",
            "Show_05.srt",
            "Show - 5.srt",
            "Show.105.srt",
        ] {
            assert!(episode_match(name, 1, 5).is_some(), "{name}");
        }
        for name in [
            "Show.S01E15.srt",
            "Show.S02E05.srt",
            "Show.S01E051.srt",
            "Cars2x05.srt",
            "Show.720p.x265.srt",
            "Show.DD5.1.srt",
            "Show.1080p.srt",
            "Show.2019.srt",
            "Show.E15.srt",
        ] {
            assert_eq!(episode_match(name, 1, 5), None, "{name}");
        }
        // The surer form wins.
        assert_eq!(episode_match("Show.S01E05.srt", 1, 5), Some(1));
        assert_eq!(episode_match("Show - 05.srt", 1, 5), Some(3));
        let pack = zipped(&[
            ("Show - 05.srt", SRT.as_bytes()),
            ("Show.S01E05.srt", [SRT, SRT].concat().as_bytes()),
            ("Show - 06.srt", SRT.as_bytes()),
        ]);
        assert_eq!(
            unpack(&pack, Some((1, 5))).unwrap(),
            [SRT, SRT].concat().as_bytes()
        );
        // `105` only when one name has it.
        let ambiguous = zipped(&[("A.105.srt", SRT.as_bytes()), ("B.105.srt", SRT.as_bytes())]);
        assert_eq!(
            unpack(&ambiguous, Some((1, 5))),
            Err(FileError::EpisodeNotInArchive)
        );
    }

    #[test]
    fn a_hostile_or_useless_archive_is_refused() {
        // A name that leads out of it.
        assert!(matches!(
            unpack(&zipped(&[("../../etc/cron.d/x.srt", SRT.as_bytes())]), None),
            Err(FileError::BadArchive(_))
        ));
        assert!(matches!(
            unpack(&zipped(&[("/abs/x.srt", SRT.as_bytes())]), None),
            Err(FileError::BadArchive(_))
        ));
        // Too many entries.
        let names: Vec<String> = (0..=MAX_ARCHIVE_ENTRIES)
            .map(|n| format!("{n}.srt"))
            .collect();
        let many: Vec<(&str, &[u8])> = names
            .iter()
            .map(|n| (n.as_str(), b"x".as_slice()))
            .collect();
        assert!(matches!(
            unpack(&zipped(&many), None),
            Err(FileError::BadArchive(_))
        ));
        // No subtitle in it.
        assert_eq!(
            unpack(
                &zipped(&[("film.sub", b"{1}{2}Hi"), ("film.nfo", b"x")]),
                None
            ),
            Err(FileError::NoSubtitleInArchive)
        );
        // Not an archive at all.
        assert!(matches!(
            unpack(b"PK\x03\x04 nothing", None),
            Err(FileError::BadArchive(_))
        ));
        // A bomb: small zipped, past the cap unzipped.
        let bomb = vec![b'a'; MAX_DOWNLOAD_BYTES + 1];
        let zipped_bomb = zipped(&[("big.srt", &bomb)]);
        assert!(zipped_bomb.len() < 64 << 10);
        assert_eq!(unpack(&zipped_bomb, None), Err(FileError::TooLarge));
        assert_eq!(
            unpack(&vec![0; MAX_ARCHIVE_BYTES + 1], None),
            Err(FileError::TooLarge)
        );
    }

    #[test]
    fn the_format_is_read_from_the_text() {
        let vtt = "WEBVTT\n\n00:01.000 --> 00:02.000\nHello\n";
        assert_eq!(keep(vtt.as_bytes()).unwrap().format, TextFormat::WebVtt);
        let ass = "[Script Info]\nScriptType: v4.00+\n\n[Events]\n\
                   Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
                   Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,Hello\n";
        let kept = keep(ass.as_bytes()).unwrap();
        assert_eq!(
            (kept.format, kept.extension(), kept.codec()),
            (TextFormat::Ass, "ass", "ass")
        );
    }

    #[test]
    fn what_is_no_subtitle_is_refused() {
        assert!(matches!(
            keep(b"<html><body>Not found</body></html>"),
            Err(FileError::NotSubtitle(_))
        ));
        assert_eq!(
            keep(b"1\n00:00:01,000 --> 00:00:02,000\n\0\0"),
            Err(FileError::NotText)
        );
        assert_eq!(
            keep(&vec![b'a'; MAX_DOWNLOAD_BYTES + 1]),
            Err(FileError::TooLarge)
        );
    }
}
