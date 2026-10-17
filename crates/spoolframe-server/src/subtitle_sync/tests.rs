use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use spoolframe_media::{AudioStream, SubtitleStream};
use spoolframe_types::{AudioRole, SubtitleRole};

use super::*;
use crate::store::tests::{TestStore, test_store};

/// A small deterministic generator.
struct Rng(u64);

impl Rng {
    fn range(&mut self, low: i64, high: i64) -> i64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        low + (self.0 % (high - low) as u64) as i64
    }
}

/// Twelve minutes of a film's dialogue.
fn film_cues(seed: u64) -> Vec<Span> {
    let mut rng = Rng(seed);
    let (mut at, mut cues) = (4_000, Vec::new());
    while at < 12 * 60 * 1000 {
        let length = rng.range(800, 4_500);
        cues.push(Span {
            start_ms: at,
            end_ms: at + length,
        });
        at += length + rng.range(200, 5_000);
    }
    cues
}

fn srt(cues: &[Span], shift_ms: i64) -> String {
    let time = |ms: i64| {
        format!(
            "{:02}:{:02}:{:02},{:03}",
            ms / 3_600_000,
            ms / 60_000 % 60,
            ms / 1000 % 60,
            ms % 1000
        )
    };
    cues.iter()
        .enumerate()
        .map(|(i, c)| {
            format!(
                "{}\n{} --> {}\nLine {i}\n\n",
                i + 1,
                time(c.start_ms + shift_ms),
                time(c.end_ms + shift_ms)
            )
        })
        .collect()
}

fn times(text: &[u8]) -> Vec<(i64, i64)> {
    subsync::cues(text, TextFormat::Srt)
        .unwrap()
        .iter()
        .map(|c| (c.start_ms, c.end_ms))
        .collect()
}

/// Speech found where the cues are, counting how often it is asked.
struct FakeSpeech {
    spans: Vec<Span>,
    asked: AtomicUsize,
    running: AtomicUsize,
    most: AtomicUsize,
    takes: Duration,
}

#[async_trait::async_trait]
impl SpeechSource for FakeSpeech {
    async fn speech(&self, _media: &Path, _index: u32) -> Result<Vec<Span>, SyncError> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(self.takes).await;
        self.running.fetch_sub(1, Ordering::SeqCst);
        Ok(self.spans.clone())
    }
}

fn speech(spans: Vec<Span>, takes: Duration) -> Arc<FakeSpeech> {
    Arc::new(FakeSpeech {
        spans,
        asked: AtomicUsize::new(0),
        running: AtomicUsize::new(0),
        most: AtomicUsize::new(0),
        takes,
    })
}

struct Fixture {
    sync: Arc<SubtitleTimings>,
    dir: PathBuf,
    _tmp: spoolframe_test_dir::TestDir,
    _store: TestStore,
}

async fn fixture(test: &str, ffmpeg: Option<&str>, source: Arc<FakeSpeech>) -> Fixture {
    let tmp = spoolframe_test_dir::TestDir::new(&format!("spoolframe-subsync-{test}")).await;
    let dir = tmp.path().to_path_buf();
    let store = test_store(&format!("subsync-{test}")).await;
    let ffmpeg = match ffmpeg {
        Some(script) => {
            let path = dir.join("ffmpeg");
            crate::test_support::write_script(&path, script);
            Tool::unconfined(path)
        }
        None => Tool::unconfined("/nonexistent/ffmpeg"),
    };
    let sync = Arc::new(
        SubtitleTimings::new(
            store.clone(),
            ffmpeg,
            source,
            &SyncSettings {
                at_once: 1,
                dir: dir.join("state").join("subtitles"),
                speech_dir: dir.join("state").join("speech"),
            },
        )
        .unwrap(),
    );
    Fixture {
        sync,
        dir,
        _tmp: tmp,
        _store: store,
    }
}

fn sidecar(path: PathBuf) -> SubtitleStream {
    SubtitleStream {
        source: SubtitleSource::External { path },
        codec: "subrip".into(),
        kind: SubtitleKind::Text,
        language: Some("eng".into()),
        title: None,
        forced: false,
        default: false,
        role: SubtitleRole::Full,
    }
}

fn embedded(index: u32) -> SubtitleStream {
    SubtitleStream {
        source: SubtitleSource::Embedded { index },
        codec: "subrip".into(),
        kind: SubtitleKind::Text,
        language: Some("spa".into()),
        title: None,
        forced: false,
        default: false,
        role: SubtitleRole::Full,
    }
}

/// A film at `name` in `dir`, with `subtitles`, and its one audio track.
fn track(dir: &Path, name: &str, subtitles: Vec<SubtitleStream>, index: u32) -> TrackRef {
    let media = dir.join(name);
    if !media.exists() {
        std::fs::write(&media, name).unwrap();
    }
    TrackRef {
        id: format!("plex:movie:{}", name.len()).parse().unwrap(),
        media,
        descriptor: Arc::new(MediaDescriptor {
            container: "matroska,webm".into(),
            duration: None,
            bit_rate: None,
            video: None,
            audio: vec![AudioStream {
                index: 1,
                codec: "aac".into(),
                profile: None,
                channels: Some(2),
                channel_layout: None,
                language: None,
                title: None,
                default: true,
                role: AudioRole::Main,
            }],
            subtitles,
        }),
        index,
    }
}

/// A subtitle file beside the film, `text`.
fn write_sidecar(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

/// The timing news of `track` once its sync is done.
async fn synced(news: &mut broadcast::Receiver<TimingNews>, track: u32) -> SubtitleTiming {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let told = news.recv().await.unwrap();
            if told.track == track && told.timing.sync != SubtitleSync::Syncing {
                return told.timing;
            }
        }
    })
    .await
    .expect("the sync ends")
}

async fn served(sync: &SubtitleTimings, sidecar: &Path) -> Vec<u8> {
    std::fs::read(sync.resolve(sidecar).await).unwrap()
}

fn assert_within(got: &[(i64, i64)], want: &[Span], ms: i64) {
    assert_eq!(got.len(), want.len());
    for (i, ((start, end), w)) in got.iter().zip(want).enumerate() {
        assert!(
            (start - w.start_ms).abs() <= ms && (end - w.end_ms).abs() <= ms,
            "cue {i}: {start}..{end}, not {w:?}"
        );
    }
}

/// Without a subtitle in the file, the speech is the reference; a second
/// subtitle of the same film is synced with the speech found for the
/// first, and its audio is not decoded again.
#[tokio::test]
async fn speech_is_the_reference_and_is_found_once_per_film() {
    let cues = film_cues(1);
    let source = speech(cues.clone(), Duration::ZERO);
    let f = fixture("speech", None, source.clone()).await;
    let en = write_sidecar(&f.dir, "Film.en.srt", &srt(&cues, 3_200));
    let fr = write_sidecar(&f.dir, "Film.fr.srt", &srt(&cues, -2_000));
    let subtitles = vec![sidecar(en.clone()), sidecar(fr.clone())];
    let mut news = f.sync.changes();
    for index in 0..2 {
        let t = track(&f.dir, "Film.mkv", subtitles.clone(), index);
        assert_eq!(f.sync.sync(t).await.unwrap().sync, SubtitleSync::Syncing);
        let done = synced(&mut news, index).await;
        assert!(matches!(done.sync, SubtitleSync::Synced { .. }), "{done:?}");
    }
    assert_eq!(source.asked.load(Ordering::SeqCst), 1);
    assert_within(&times(&served(&f.sync, &en).await), &cues, 200);
    assert_within(&times(&served(&f.sync, &fr).await), &cues, 200);
}

/// With a subtitle in the file, in another language, it is the reference:
/// read out by ffmpeg, and the speech is never decoded.
#[tokio::test]
async fn an_embedded_subtitle_is_the_reference() {
    let cues = film_cues(2);
    let tmp = spoolframe_test_dir::TestDir::new("spoolframe-subsync-embedded-ref").await;
    let reference = tmp.path().join("reference.srt");
    std::fs::write(&reference, srt(&cues, 0)).unwrap();
    let runs = tmp.path().join("runs");
    // ffmpeg's stand-in writes the reference to its output, its last
    // argument.
    let script = format!(
        "#!/bin/sh\nfor out; do :; done\necho run >> '{}'\ncp '{}' \"$out\"\n",
        runs.display(),
        reference.display()
    );
    let source = speech(Vec::new(), Duration::ZERO);
    let f = fixture("embedded", Some(&script), source.clone()).await;
    let en = write_sidecar(&f.dir, "Film.en.srt", &srt(&cues, 3_200));
    let t = track(
        &f.dir,
        "Film.mkv",
        vec![embedded(2), sidecar(en.clone())],
        1,
    );
    let mut news = f.sync.changes();
    f.sync.sync(t).await.unwrap();
    let done = synced(&mut news, 1).await;
    match done.sync {
        SubtitleSync::Synced {
            offset_ms,
            factor_ppm,
            splits,
            score,
        } => {
            assert!((offset_ms + 3_200).abs() <= 200, "{offset_ms}");
            assert_eq!((factor_ppm, splits), (1_000_000, 0));
            assert!(score >= 50, "{score}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(source.asked.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read_to_string(&runs).unwrap().lines().count(), 1);
    assert_within(&times(&served(&f.sync, &en).await), &cues, 200);
}

/// A subtitle of another film scores too low: it says so, and is served
/// as it came.
#[tokio::test]
async fn another_film_s_subtitle_is_uncertain_and_keeps_its_timing() {
    let source = speech(film_cues(3), Duration::ZERO);
    let f = fixture("uncertain", None, source).await;
    let text = srt(&film_cues(77), 0);
    let en = write_sidecar(&f.dir, "Film.en.srt", &text);
    let mut news = f.sync.changes();
    f.sync
        .sync(track(&f.dir, "Film.mkv", vec![sidecar(en.clone())], 0))
        .await
        .unwrap();
    let done = synced(&mut news, 0).await;
    assert!(
        matches!(done.sync, SubtitleSync::Uncertain { .. }),
        "{done:?}"
    );
    assert_eq!(served(&f.sync, &en).await, text.as_bytes());
}

/// The original is never written; "Use original timing" serves it again,
/// for everyone, from the same served file, so a reader is told it
/// changed.
#[tokio::test]
async fn use_original_timing_serves_the_untouched_original() {
    let cues = film_cues(4);
    let f = fixture("original", None, speech(cues.clone(), Duration::ZERO)).await;
    let text = srt(&cues, 3_200);
    let en = write_sidecar(&f.dir, "Film.en.srt", &text);
    let t = track(&f.dir, "Film.mkv", vec![sidecar(en.clone())], 0);
    // Read before the track has any timing: the file itself.
    assert_eq!(f.sync.resolve(&en).await, en);
    let mut news = f.sync.changes();
    f.sync.sync(t.clone()).await.unwrap();
    synced(&mut news, 0).await;
    let served_at = f.sync.resolve(&en).await;
    assert_ne!(served_at, en);
    assert_ne!(std::fs::read(&served_at).unwrap(), text.as_bytes());
    assert_eq!(
        std::fs::read_to_string(&en).unwrap(),
        text,
        "the original was written"
    );
    let timing = f.sync.use_original(t).await.unwrap();
    assert_eq!(timing.sync, SubtitleSync::Original);
    assert_eq!(f.sync.resolve(&en).await, served_at, "one served file");
    assert_eq!(std::fs::read(&served_at).unwrap(), text.as_bytes());
}

/// An offset set by one user is what everyone gets, on top of the timing,
/// with who set it; Reset (0) brings the timing back.
#[tokio::test]
async fn an_offset_is_the_track_s_for_everyone() {
    let cues = film_cues(5);
    let f = fixture("offset", None, speech(Vec::new(), Duration::ZERO)).await;
    let text = srt(&cues, 0);
    let en = write_sidecar(&f.dir, "Film.en.srt", &text);
    let t = track(&f.dir, "Film.mkv", vec![sidecar(en.clone())], 0);
    let timing = f
        .sync
        .set_offset(t.clone(), 1_500, "local:alice", "Alice")
        .await
        .unwrap();
    assert_eq!(timing.offset_ms, 1_500);
    assert_eq!(timing.offset_by.as_deref(), Some("Alice"));
    // What any other user's playback, or its Tracks, gets.
    let later: Vec<Span> = cues
        .iter()
        .map(|c| Span {
            start_ms: c.start_ms + 1_500,
            end_ms: c.end_ms + 1_500,
        })
        .collect();
    assert_within(&times(&served(&f.sync, &en).await), &later, 0);
    let shown = f.sync.timings(&t.descriptor).await;
    assert_eq!(shown[0].as_ref().unwrap().offset_ms, 1_500);
    f.sync.set_offset(t, 0, "local:bob", "Bob").await.unwrap();
    assert_eq!(served(&f.sync, &en).await, text.as_bytes());
}

/// A subtitle file edited since its timing was set is served as it is
/// now: the timing was of the file that is gone.
#[tokio::test]
async fn an_edited_subtitle_drops_its_timing() {
    let cues = film_cues(6);
    let f = fixture("edited", None, speech(Vec::new(), Duration::ZERO)).await;
    let en = write_sidecar(&f.dir, "Film.en.srt", &srt(&cues, 0));
    let t = track(&f.dir, "Film.mkv", vec![sidecar(en.clone())], 0);
    f.sync
        .set_offset(t.clone(), 900, "local:a", "A")
        .await
        .unwrap();
    std::fs::write(&en, srt(&cues, 100)).unwrap();
    assert_eq!(f.sync.resolve(&en).await, en);
    let timing = f.sync.timings(&t.descriptor).await[0].clone().unwrap();
    assert_eq!((timing.sync, timing.offset_ms), (SubtitleSync::Original, 0));
}

/// Syncs run one at a time by default: the rest wait their turn, and all
/// are done.
#[tokio::test]
async fn syncs_run_one_at_a_time() {
    let cues = film_cues(7);
    let source = speech(cues.clone(), Duration::from_millis(300));
    let f = fixture("one-at-a-time", None, source.clone()).await;
    let mut news = f.sync.changes();
    // Three films, so none shares another's speech.
    for n in 0..3u32 {
        let film = format!("Film{n}.mkv");
        let en = write_sidecar(&f.dir, &format!("Film{n}.en.srt"), &srt(&cues, 1_000));
        f.sync
            .sync(track(&f.dir, &film, vec![sidecar(en)], 0))
            .await
            .unwrap();
    }
    for _ in 0..3 {
        synced(&mut news, 0).await;
    }
    assert_eq!(source.asked.load(Ordering::SeqCst), 3);
    assert_eq!(source.most.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn only_an_external_text_subtitle_is_retimed() {
    let f = fixture("refused", None, speech(Vec::new(), Duration::ZERO)).await;
    let en = write_sidecar(&f.dir, "Film.en.srt", &srt(&film_cues(8), 0));
    let mut pgs = sidecar(f.dir.join("Film.en.sup"));
    pgs.codec = "hdmv_pgs_subtitle".into();
    pgs.kind = SubtitleKind::Bitmap;
    let subtitles = vec![embedded(2), pgs, sidecar(en)];
    for index in [0, 1] {
        let refused = f
            .sync
            .set_offset(
                track(&f.dir, "Film.mkv", subtitles.clone(), index),
                100,
                "a",
                "A",
            )
            .await;
        assert!(
            matches!(refused, Err(TimingError::NotExternalText)),
            "{refused:?}"
        );
    }
    let refused = f
        .sync
        .sync(track(&f.dir, "Film.mkv", subtitles.clone(), 9))
        .await;
    assert!(
        matches!(refused, Err(TimingError::NoSuchTrack(9))),
        "{refused:?}"
    );
    let refused = f
        .sync
        .set_offset(
            track(&f.dir, "Film.mkv", subtitles.clone(), 2),
            MAX_OFFSET_MS + 1,
            "a",
            "A",
        )
        .await;
    assert!(
        matches!(refused, Err(TimingError::OffsetTooLarge)),
        "{refused:?}"
    );
    let shown = f
        .sync
        .timings(&track(&f.dir, "Film.mkv", subtitles, 0).descriptor)
        .await;
    assert_eq!(
        shown.iter().map(Option::is_some).collect::<Vec<_>>(),
        [false, false, true]
    );
}
