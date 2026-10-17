//! A zipped subtitle (PX-02): an archive SubDL sent, its one subtitle
//! taken out in the server's own process (`spoolframe-media`
//! subtitle_file.rs `unpack`), then kept as any download. The first byte
//! picks the episode looked for (none, or season 1's episode 1 to 255).
//! What is taken out is never past the cap, and an archive never makes it
//! read past it.
#![no_main]

use libfuzzer_sys::fuzz_target;
use spoolframe_media::subtitle_file;

fuzz_target!(|data: &[u8]| {
    let Some((&pick, archive)) = data.split_first() else {
        return;
    };
    let episode = (pick != 0).then_some((1, u32::from(pick)));
    let Ok(taken) = subtitle_file::unpack(archive, episode) else {
        return;
    };
    assert!(taken.len() <= subtitle_file::MAX_DOWNLOAD_BYTES);
    if let Ok(kept) = subtitle_file::keep(&taken) {
        assert!(std::str::from_utf8(&kept.bytes).is_ok());
    }
});
