//! A downloaded subtitle (PX-02): a file from a subtitle site, in any
//! encoding, made UTF-8 and checked in the server's own process before it
//! is kept (`spoolframe-media` subtitle_file.rs). What is kept is UTF-8
//! and reads back with as many cues.
#![no_main]

use libfuzzer_sys::fuzz_target;
use spoolframe_media::{subsync, subtitle_file};

fuzz_target!(|data: &[u8]| {
    let Ok(kept) = subtitle_file::keep(data) else {
        return;
    };
    assert!(std::str::from_utf8(&kept.bytes).is_ok());
    let again = subsync::cues(&kept.bytes, kept.format).expect("a kept file reads");
    assert_eq!(again.len(), kept.cues);
});
