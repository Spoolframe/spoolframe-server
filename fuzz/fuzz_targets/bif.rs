//! A PMS's preview index (BIF) read into frames (`spoolframe-plex`
//! bif.rs).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = spoolframe_plex::fuzzing::bif(data);
});
