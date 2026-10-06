//! A PGS (`.sup`) stream, from a file's track or one beside it, read into
//! cues (`spoolframe-media` pgs.rs).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = spoolframe_media::pgs::cues(data);
});
