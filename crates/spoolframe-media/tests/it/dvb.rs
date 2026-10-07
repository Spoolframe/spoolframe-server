//! DVB subtitle tracks as ffmpeg's encoder writes them, read back cue by
//! cue: in Matroska, and in MPEG-TS, whose clock does not start at 0.

use spoolframe_media::{dvb, ocr};

use crate::samples::{self, Shown};

const CUES: [Shown; 2] = [
    ("Hello there", 1_000, 3_500),
    ("General Kenobi", 5_000, 7_250),
];

async fn scratch(test: &str) -> spoolframe_test_dir::TestDir {
    spoolframe_test_dir::TestDir::new(&format!("spoolframe-dvb-{test}")).await
}

async fn assert_reads_back(name: &str) {
    let tmp = scratch(name).await;
    let movie = samples::movie(tmp.as_ref(), name, "dvbsub", &CUES);
    let track = ocr::read_track(&crate::sandbox::confined("ffprobe"), &movie, 1, true)
        .await
        .unwrap();
    let cues = dvb::cues(&track.packets).unwrap();
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
async fn a_track_in_matroska_reads_back_at_its_times_with_its_text() {
    assert_reads_back("movie.mkv").await;
}

#[tokio::test]
async fn a_recording_s_track_reads_back_at_times_from_its_start() {
    // ffmpeg starts an MPEG-TS clock at 1.4 s, and the picture later.
    assert_reads_back("movie.ts").await;
}
