//! ffprobe's JSON for a file, read into a descriptor as the prober does
//! (`spoolframe-media` probe.rs and descriptor.rs). The file is the
//! user's, and ffprobe passes much of it through: tags, names, rates.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(probe) = serde_json::from_slice::<easy_ffprobe::FfProbe>(data) {
        let _ = spoolframe_media::MediaDescriptor::from_probe(&probe);
    }
});
