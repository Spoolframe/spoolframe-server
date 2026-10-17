//! A subtitle file from outside the server (PX-02: a download from a
//! subtitle site), made into one the server keeps: UTF-8, of a format it
//! reads, with cues it can read. The file is untrusted: it is capped in
//! size, decoded by `encoding_rs` (never a C library), and its cues are
//! read by [`crate::subsync`]'s parser, the one a sync reads it with.
//! Nothing the site said about it (its name, its format) is believed:
//! the format is read from the text itself.

use crate::subsync::{self, TextFormat};

/// The largest downloaded subtitle taken: a film's SRT is some 100 KB, an
/// ASS with heavy styling a few MB.
pub const MAX_DOWNLOAD_BYTES: usize = 8 << 20;

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
