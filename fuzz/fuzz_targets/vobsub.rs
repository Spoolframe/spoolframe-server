//! A VobSub track, its `.idx` header and its packets, read into cues
//! (`spoolframe-media` vobsub.rs). The input is as
//! `spoolframe_fuzz::vobsub` reads it.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Some((header, packets)) = spoolframe_fuzz::vobsub(data) {
        let _ = spoolframe_media::vobsub::cues(header, &packets);
    }
});
