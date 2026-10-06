//! A PMS JSON response, decoded and converted into Spoolframe's items
//! (`spoolframe-plex` wire.rs and convert.rs).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    spoolframe_plex::fuzzing::json(data);
});
