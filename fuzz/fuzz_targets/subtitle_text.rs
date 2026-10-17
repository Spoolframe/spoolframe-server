//! A text subtitle's cue times (PX-03): SRT, WebVTT and ASS read as bytes
//! in the server's own process to be synced, from a sidecar anyone who
//! can write to a library folder made, then rewritten with new times
//! (`spoolframe-media` subsync/text.rs). The first byte picks the format.
#![no_main]

use libfuzzer_sys::fuzz_target;
use spoolframe_media::subsync::{self, Retiming, TextFormat};

fuzz_target!(|data: &[u8]| {
    let Some((&pick, text)) = data.split_first() else {
        return;
    };
    let format = match pick % 3 {
        0 => TextFormat::Srt,
        1 => TextFormat::WebVtt,
        _ => TextFormat::Ass,
    };
    let Ok(cues) = subsync::cues(text, format) else {
        return;
    };
    // Moved, the file reads back with as many cues, each moved.
    let retiming = Retiming {
        factor: 25.0 / 23.976,
        deltas: (0..cues.len() as i64).map(|i| i * 37 - 5_000).collect(),
    };
    let moved = subsync::retime(text, format, &cues, &retiming);
    let again = subsync::cues(&moved, format).expect("a retimed file reads");
    assert_eq!(again.len(), cues.len());
});
