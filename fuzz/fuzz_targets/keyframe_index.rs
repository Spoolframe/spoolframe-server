//! A media file's own keyframe index, Matroska `Cues` or MP4 `stss`, read
//! without the rest of the file (`spoolframe-media` keyframe_index.rs,
//! PX-25). The input is the file.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut file = std::io::Cursor::new(data);
    let _ = spoolframe_media::keyframe_index::read(&mut file, data.len() as u64);
});
