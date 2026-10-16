//! An uploaded playlist cover's header (PX-53): its format and size,
//! read in the server's own process before anything is decoded
//! (`spoolframe-media` picture.rs).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = spoolframe_media::picture::check(data);
});
