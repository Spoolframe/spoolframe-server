//! VobSub tracks as ffmpeg's encoder writes them, read back cue by cue:
//! embedded in Matroska, and as a `.idx` and `.sub` pair.

use std::path::Path;

use spoolframe_media::bitmap::Cue;
use spoolframe_media::{ocr, vobsub};

use crate::samples::{self, Shown};

/// A unit's stop is in 1024/90000 s, so these lengths (2,560 and 2,048
/// ms, multiples of 512) are whole milliseconds.
const CUES: [Shown; 2] = [
    ("Hello there", 1_000, 3_560),
    ("General Kenobi", 5_000, 7_048),
];

async fn scratch(test: &str) -> spoolframe_test_dir::TestDir {
    spoolframe_test_dir::TestDir::new(&format!("spoolframe-vobsub-{test}")).await
}

/// The cues are `CUES`: their times, and pictures inked where the text's
/// own pixels are, not its outline.
fn assert_read_back(cues: &[Cue]) {
    let times: Vec<_> = cues.iter().map(|c| (c.start_ms, c.end_ms)).collect();
    assert_eq!(times, CUES.map(|(_, start, end)| (start, end)));
    for (cue, (text, ..)) in cues.iter().zip(CUES) {
        let (width, height, ink) = samples::expected_ink(text);
        assert_eq!(
            (cue.image.width, cue.image.height),
            (width, height),
            "{text}"
        );
        assert!(
            samples::ink(&cue.image) == ink,
            "{text}: other pixels inked"
        );
    }
}

#[tokio::test]
async fn an_embedded_track_reads_back_at_its_times_with_its_text() {
    let tmp = scratch("embedded").await;
    let movie = samples::movie(tmp.as_ref(), "movie.mkv", "dvdsub", &CUES);
    let track = ocr::read_track(Path::new("ffprobe"), &movie, 1, true)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&track.extradata).contains("palette:"),
        "the codec private data is the .idx header"
    );
    assert_read_back(&vobsub::cues(&track.extradata, &track.packets).unwrap());
}

#[tokio::test]
async fn a_pair_beside_the_film_reads_back_at_its_times_with_its_text() {
    let tmp = scratch("pair").await;
    let idx = samples::vobsub_pair(tmp.as_ref(), &CUES);
    let track = ocr::read_track(Path::new("ffprobe"), &idx, 0, false)
        .await
        .unwrap();
    assert_read_back(&vobsub::cues(&track.extradata, &track.packets).unwrap());
}
