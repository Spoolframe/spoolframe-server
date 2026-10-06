//! A DVB subtitle track's packets read into cues (`spoolframe-media`
//! dvb.rs). The input is packets as `spoolframe_fuzz::packets` reads them.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = spoolframe_media::dvb::cues(&spoolframe_fuzz::packets(data));
});
