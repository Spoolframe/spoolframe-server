//! Bitmap subtitle samples, made when a test runs: nothing is committed.
//! Text is drawn by ffmpeg's drawtext, white with a black outline, written
//! as a PGS stream (`pgs::fixture`), then encoded by ffmpeg's own `dvdsub`
//! and `dvbsub` encoders, so the parsers read what a real encoder writes.
//! By hand, from a `subs.sup`:
//!
//! ```text
//! # VobSub in Matroska (stream 1); DVB in Matroska or MPEG-TS
//! ffmpeg -f lavfi -i testsrc2=s=320x180:d=10 -copyts -fix_sub_duration -i subs.sup \
//!     -map 0 -map 1 -c:v libx264 -c:s dvdsub movie.mkv      # or dvbsub, movie.ts
//! # A VobSub `.sub`; the `.idx` beside it is written by `vobsub_pair`
//! ffmpeg -copyts -fix_sub_duration -i subs.sup -c:s dvdsub -f vob movie.sub
//! ```
//!
//! `-copyts` keeps the cues' own times (without it ffmpeg moves the track
//! back by its first cue's time); `-fix_sub_duration` gives each cue the
//! length PGS says with a packet of its own.

use std::path::{Path, PathBuf};
use std::process::Command;

use spoolframe_media::pgs::fixture;

/// A cue's text and time.
pub type Shown<'a> = (&'a str, u64, u64);

/// DVD-sized text: at 26 points, about as tall as a DVD's subtitles on
/// its 720×480 picture.
pub const WIDTH: u16 = 400;
pub const HEIGHT: u16 = 40;

pub fn run(cmd: &mut Command) -> Vec<u8> {
    let out = cmd.output().expect("the program runs");
    assert!(
        out.status.success(),
        "{cmd:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// `text` as a mask: 1 where drawtext lights a pixel, 2 for the two-pixel
/// black outline around it, 0 for nothing.
pub fn outlined(text: &str) -> Vec<u8> {
    // The dev shell names a font file: fontconfig has none to find there.
    let font =
        std::env::var("SPOOLFRAME_TEST_FONT").expect("the dev shell sets SPOOLFRAME_TEST_FONT");
    let grey = run(Command::new("ffmpeg")
        .args(["-v", "error", "-f", "lavfi", "-i"])
        .arg(format!("color=black:s={WIDTH}x{HEIGHT}:d=1"))
        .arg("-vf")
        .arg(format!(
            "drawtext=fontfile={font}:text='{text}':fontcolor=white:fontsize=26:x=8:y=6"
        ))
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "gray", "-"]));
    let (w, h) = (usize::from(WIDTH), usize::from(HEIGHT));
    assert_eq!(grey.len(), w * h);
    let text: Vec<bool> = grey.iter().map(|&p| p > 128).collect();
    (0..w * h)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            if text[i] {
                return 1;
            }
            let near = (y.saturating_sub(2)..(y + 3).min(h))
                .any(|ny| (x.saturating_sub(2)..(x + 3).min(w)).any(|nx| text[ny * w + nx]));
            if near { 2 } else { 0 }
        })
        .collect()
}

/// The cues as a PGS stream, each at (160, 400).
pub fn sup(cues: &[Shown]) -> Vec<u8> {
    let shown: Vec<fixture::Cue> = cues
        .iter()
        .map(|&(text, start_ms, end_ms)| fixture::Cue {
            start_ms,
            end_ms,
            x: 160,
            y: 400,
            width: WIDTH,
            height: HEIGHT,
            mask: outlined(text),
        })
        .collect();
    fixture::sup(&shown, 65_000)
}

/// A film with the cues as stream 1, encoded by ffmpeg's `codec`
/// (`dvdsub`, `dvbsub`), in `name`'s container. The picture runs past the
/// last cue: ffmpeg's MPEG-TS muxer gives a subtitle after the picture's
/// end a time of its own invention.
pub fn movie(dir: &Path, name: &str, codec: &str, cues: &[Shown]) -> PathBuf {
    let sup = dir.join(format!("{name}.sup"));
    std::fs::write(&sup, self::sup(cues)).unwrap();
    let movie = dir.join(name);
    let secs = cues
        .iter()
        .map(|&(_, _, end)| end / 1000 + 2)
        .max()
        .unwrap_or(0);
    run(Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi"])
        .arg("-i")
        .arg(format!("testsrc2=s=320x180:d={}", secs.max(10)))
        .args(["-copyts", "-fix_sub_duration", "-i"])
        .arg(&sup)
        .args(["-map", "0", "-map", "1", "-c:v", "libx264", "-c:s", codec])
        .args(["-metadata:s:s:0", "language=eng"])
        .arg(&movie));
    movie
}

/// The palette ffmpeg's `dvdsub` encoder uses unless told otherwise,
/// which the `.idx` repeats.
const PALETTE: [&str; 16] = [
    "000000", "0000ff", "00ff00", "ff0000", "ffff00", "ff00ff", "00ffff", "ffffff", "808000",
    "8080ff", "800080", "80ff80", "008080", "ff8080", "555555", "aaaaaa",
];

/// The cues as a VobSub pair, `movie.idx` and `movie.sub` in `dir`; the
/// `.idx`'s path. ffmpeg has no VobSub muxer: it encodes the pictures into
/// an MPEG program stream, which is what a `.sub` is, and the `.idx` (the
/// palette, and each cue's time and place in the `.sub`) is written here.
pub fn vobsub_pair(dir: &Path, cues: &[Shown]) -> PathBuf {
    let sup = dir.join("pair.sup");
    std::fs::write(&sup, self::sup(cues)).unwrap();
    let sub = dir.join("movie.sub");
    run(Command::new("ffmpeg")
        .args(["-v", "fatal", "-y", "-copyts", "-fix_sub_duration", "-i"])
        .arg(&sup)
        .args(["-c:s", "dvdsub", "-f", "vob"])
        .arg(&sub));
    let starts = unit_starts(&std::fs::read(&sub).unwrap());
    assert_eq!(starts.len(), cues.len(), "one unit per cue");
    let mut idx = format!(
        "# VobSub index file, v7 (do not modify this line!)\n\
         size: 720x480\n\
         palette: {}\n\
         langidx: 0\n\
         id: en, index: 0\n",
        PALETTE.join(", ")
    );
    for (&(_, start_ms, _), filepos) in cues.iter().zip(starts) {
        idx.push_str(&format!(
            "timestamp: {:02}:{:02}:{:02}:{:03}, filepos: {filepos:09x}\n",
            start_ms / 3_600_000,
            start_ms / 60_000 % 60,
            start_ms / 1000 % 60,
            start_ms % 1000,
        ));
    }
    let path = dir.join("movie.idx");
    std::fs::write(&path, idx).unwrap();
    path
}

/// Where each subtitle unit starts in a `.sub`: the 2048-byte packs whose
/// private stream packet has a time, which only a unit's first has.
fn unit_starts(sub: &[u8]) -> Vec<usize> {
    (0..sub.len())
        .step_by(2048)
        .filter(|&pack| {
            let data = &sub[pack..(pack + 2048).min(sub.len())];
            data.windows(4)
                .position(|w| w == [0, 0, 1, 0xbd])
                .is_some_and(|pes| data.get(pes + 7).is_some_and(|flags| flags & 0x80 != 0))
        })
        .collect()
}

/// A picture's ink: true where it is dark.
pub fn ink(image: &spoolframe_media::bitmap::Image) -> Vec<bool> {
    image.pixels.iter().map(|&p| p < 128).collect()
}

/// `text`'s own pixels (not its outline), cut to the box around them, with
/// the 16-pixel margin a cue's picture has.
pub fn expected_ink(text: &str) -> (usize, usize, Vec<bool>) {
    let mask = outlined(text);
    let (w, h) = (usize::from(WIDTH), usize::from(HEIGHT));
    let lit = |x: usize, y: usize| mask[y * w + x] == 1;
    let rows: Vec<usize> = (0..h).filter(|&y| (0..w).any(|x| lit(x, y))).collect();
    let cols: Vec<usize> = (0..w).filter(|&x| (0..h).any(|y| lit(x, y))).collect();
    let (top, bottom) = (rows[0], *rows.last().unwrap());
    let (left, right) = (cols[0], *cols.last().unwrap());
    let (width, height) = (right - left + 1 + 32, bottom - top + 1 + 32);
    let mut out = vec![false; width * height];
    for y in top..=bottom {
        for x in left..=right {
            out[(y - top + 16) * width + (x - left + 16)] = lit(x, y);
        }
    }
    (width, height, out)
}
