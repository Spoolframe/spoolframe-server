//! The previews worker with real ffmpeg, on films made at test time
//! whose picture says what time it is: grey level `16 + (s mod 20) × 10`
//! for the 10-second span `s` it is in.

use parking_lot::Mutex;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use spoolframe_catalog::{
    Found, Image, ItemPage, ItemWithChildren, Page, PreviewFrames, SearchQuery,
};
use spoolframe_media::sandbox::Tool;
use spoolframe_types::{Item, Library, LibraryId, PersonId};

use super::*;

/// Every film's file is the one in `files` under its key; a film in
/// `indexed` has the catalog's own previews, and one in `broken` an index
/// that cannot be read.
#[derive(Debug, Default)]
struct Files {
    files: Mutex<HashMap<String, PathBuf>>,
    indexed: Mutex<HashMap<String, PreviewFrames>>,
    broken: Mutex<HashSet<String>>,
    /// How many times a file's path was asked for.
    asked: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl Catalog for Files {
    async fn libraries(&self) -> Result<Vec<Library>, CatalogError> {
        unreachable!()
    }
    async fn items(&self, _: &LibraryId, _: Page) -> Result<ItemPage, CatalogError> {
        unreachable!()
    }
    async fn item(&self, _: &MediaId) -> Result<ItemWithChildren, CatalogError> {
        unreachable!()
    }
    async fn external_ids(&self, _: &MediaId) -> Result<Vec<String>, CatalogError> {
        unreachable!()
    }
    async fn search(&self, _: &SearchQuery) -> Result<Found, CatalogError> {
        unreachable!()
    }
    async fn person_items(&self, _: &PersonId) -> Result<Vec<Item>, CatalogError> {
        unreachable!()
    }
    async fn media_path(&self, id: &MediaId) -> Result<PathBuf, CatalogError> {
        self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.files
            .lock()
            .get(id.key())
            .cloned()
            .ok_or_else(|| CatalogError::NotFound(id.to_string()))
    }
    async fn recently_added(&self, _: u32) -> Result<Vec<Item>, CatalogError> {
        unreachable!()
    }
    async fn authorize_artwork(
        &self,
        _id: &spoolframe_types::ArtworkId,
    ) -> Result<(), CatalogError> {
        Ok(())
    }

    async fn artwork(&self, _: &ArtworkId) -> Result<Image, CatalogError> {
        unreachable!()
    }
    async fn preview_frames(&self, id: &MediaId) -> Result<Option<PreviewFrames>, CatalogError> {
        if self.broken.lock().contains(id.key()) {
            return Err(CatalogError::Backend {
                url: "http://pms".into(),
                call: "GET /library/parts/1/indexes/sd".into(),
                item: None,
                message: "unreadable preview index: not a BIF".into(),
            });
        }
        Ok(self.indexed.lock().get(id.key()).cloned())
    }
}

struct Setup {
    previews: Arc<Previews>,
    files: Arc<Files>,
    /// The server's encodes: a test begins one as a conversion would.
    encoding: Encoding,
    tmp: spoolframe_test_dir::TestDir,
}

async fn setup(test: &str, settings: PreviewSettings) -> Setup {
    setup_with(test, settings, "ffmpeg").await
}

/// [`setup`], making previews with `ffmpeg`.
async fn setup_with(test: &str, settings: PreviewSettings, ffmpeg: &str) -> Setup {
    setup_making(test, settings, ffmpeg, None).await
}

async fn setup_making(
    test: &str,
    settings: PreviewSettings,
    ffmpeg: &str,
    maker: Option<Arc<dyn make::MakeTile>>,
) -> Setup {
    let tmp = spoolframe_test_dir::TestDir::new(&format!("spoolframe-previews-{test}")).await;
    let files = Arc::new(Files::default());
    let encoding = Encoding::default();
    let sources = PreviewSources {
        catalog: files.clone(),
        prober: Arc::new(Prober::new(
            Tool::unconfined("ffprobe"),
            Prober::DEFAULT_TIMEOUT,
        )),
        transcoding: Arc::new(Transcoding::new(
            Tool::unconfined("ffmpeg"),
            Vec::new(),
            tmp.as_ref().join("encoder-test"),
        )),
        ffmpeg: Tool::unconfined(ffmpeg),
        encoding: encoding.clone(),
    };
    let dir = tmp.as_ref().join("previews");
    let previews = match maker {
        Some(maker) => Previews::start_with(&dir, settings, sources, maker).await,
        None => Previews::start(&dir, settings, sources).await,
    }
    .unwrap();
    Setup {
        previews,
        files,
        encoding,
        tmp,
    }
}

fn settings() -> PreviewSettings {
    PreviewSettings {
        when: When::OnPlay,
        interval: Duration::from_secs(10),
        cache_limit_bytes: 1 << 30,
        memory_limit_bytes: 64 << 20,
        workers: 1,
    }
}

fn film(key: &str) -> MediaId {
    MediaId::new(Namespace::Plex, MediaKind::Movie, key).unwrap()
}

impl Setup {
    /// A film of `secs` seconds as `key`, a keyframe every 4 s, its grey
    /// level `offset` above the clock's.
    fn film(&self, key: &str, secs: u32, offset: u32) -> MediaId {
        let path = self.tmp.as_ref().join(format!("{key}.mkv"));
        let status = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg(format!(
                "color=c=black:s=64x36:r=1:d={secs},\
                 geq=lum='16+{offset}+mod(floor(T/10),20)*10':cb=128:cr=128"
            ))
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-g", "4"])
            .args(["-pix_fmt", "yuv420p"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        self.files.files.lock().insert(key.into(), path);
        film(key)
    }

    async fn ready(&self, id: &MediaId) -> (String, SetMeta) {
        // Only a guard against a set that never finishes: 510 tiles, one
        // ffmpeg each on one worker at idle priority, took 141 s on a CI
        // runner shared with two other runs.
        let deadline = std::time::Instant::now() + Duration::from_secs(600);
        loop {
            match self.previews.state(id).await.unwrap() {
                PreviewState::Ready { key, meta } => return (key, meta),
                PreviewState::Making { .. } => {}
                PreviewState::None => panic!("{id} has no previews"),
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{id}: not made in time"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The grey level of tile `i` of the set under `key`, as 0–255.
    async fn grey(&self, key: &str, meta: &SetMeta, i: u32) -> f64 {
        let per_sheet = meta.columns * meta.rows;
        let sheet = self.previews.sheet(key, i / per_sheet).await.unwrap();
        let sheet = image::load_from_memory(&sheet).unwrap().to_rgb8();
        let at = i % per_sheet;
        let (x, y) = (
            (at % meta.columns) * meta.tile_width,
            (at / meta.columns) * meta.tile_height,
        );
        let mut sum = 0u64;
        // The middle of the tile, away from JPEG's edges.
        for dy in meta.tile_height / 4..meta.tile_height * 3 / 4 {
            for dx in meta.tile_width / 4..meta.tile_width * 3 / 4 {
                sum += u64::from(sheet.get_pixel(x + dx, y + dy).0[0]);
            }
        }
        sum as f64 / f64::from(meta.tile_width / 2 * (meta.tile_height / 2))
    }
}

/// The grey a tile of span `s` shows, full range: video's 16–235 as
/// JPEG's 0–255.
fn grey_of_span(s: u32, offset: u32) -> f64 {
    f64::from((s % 20) * 10 + offset) * 255.0 / 219.0
}

#[tokio::test]
async fn a_film_gets_a_tile_every_interval_and_1_23_40_shows_that_moment() {
    let setup = setup("moment", settings()).await;
    // 85 minutes: past 1:23:40.
    let id = setup.film("long", 5100, 0);
    let (key, meta) = setup.ready(&id).await;
    assert_eq!(meta.count, 510);
    assert_eq!(meta.interval_ms, 10_000);
    assert_eq!((meta.columns, meta.rows), (10, 10));
    assert_eq!(meta.sheets, 6);
    assert_eq!((meta.tile_width, meta.tile_height), (320, 180));
    // 1:23:40 is 5020 s: tile 502 of the set, the 3rd of sheet 5. The
    // picture there is span 502's, or, within one interval, the one before.
    let i = (5_020_000 / meta.interval_ms) as u32;
    let grey = setup.grey(&key, &meta, i).await;
    let near = [501, 502].map(|s| grey_of_span(s, 0));
    assert!(
        near.iter().any(|g| (g - grey).abs() < 5.0),
        "tile {i} is grey {grey:.1}, not one of {near:?}"
    );
    // And not a tile far from it.
    assert!((grey_of_span(510, 0) - grey).abs() > 5.0);
}

#[tokio::test]
async fn a_replaced_file_gets_new_previews() {
    let setup = setup("replaced", settings()).await;
    let id = setup.film("film", 30, 0);
    let (first, meta) = setup.ready(&id).await;
    assert!((setup.grey(&first, &meta, 0).await - grey_of_span(0, 0)).abs() < 5.0);
    // A new rip under the same name: brighter, and longer.
    std::thread::sleep(Duration::from_millis(20));
    setup.film("film", 40, 100);
    assert!(matches!(
        setup.previews.state(&id).await.unwrap(),
        PreviewState::Making { .. }
    ));
    let (second, meta) = setup.ready(&id).await;
    assert_ne!(first, second);
    assert_eq!(meta.count, 4);
    assert!((setup.grey(&second, &meta, 0).await - grey_of_span(0, 100)).abs() < 5.0);
}

#[tokio::test]
async fn music_has_no_previews() {
    let setup = setup("music", settings()).await;
    let track = MediaId::new(Namespace::Plex, MediaKind::Track, "1").unwrap();
    assert_eq!(
        setup.previews.state(&track).await.unwrap(),
        PreviewState::None
    );
}

#[tokio::test]
async fn never_makes_none() {
    let setup = setup(
        "never",
        PreviewSettings {
            when: When::Never,
            ..settings()
        },
    )
    .await;
    let id = setup.film("film", 20, 0);
    setup.previews.played(&id);
    assert_eq!(setup.previews.state(&id).await.unwrap(), PreviewState::None);
    // Turned on while running: made from the next ask.
    setup.previews.set_settings(settings());
    setup.ready(&id).await;
}

#[tokio::test]
async fn a_file_without_pictures_fails_once_and_is_not_made_again() {
    let setup = setup("no-video", settings()).await;
    let path = setup.tmp.as_ref().join("sound.mka");
    let status = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "sine=d=5"])
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());
    setup
        .previews
        .sources
        .catalog
        .media_path(&film("x"))
        .await
        .unwrap_err();
    setup.files.files.lock().insert("sound".into(), path);
    let id = film("sound");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while setup.previews.state(&id).await.unwrap() != PreviewState::None {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let counts = setup.previews.counts();
    assert_eq!((counts.done, counts.failed), (0, 1));
}

/// What is made, and what waits, is counted (P3-14).
#[tokio::test]
async fn a_set_made_is_counted_done() {
    let setup = setup("counted", settings()).await;
    let id = setup.film("film", 20, 0);
    assert_eq!(setup.previews.counts(), PreviewCounts::default());
    setup.ready(&id).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while setup.previews.counts().done == 0 {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let counts = setup.previews.counts();
    assert_eq!((counts.done, counts.failed, counts.queued), (1, 0, 0));
}

#[tokio::test]
async fn previews_wait_for_nothing_but_the_one_being_made() {
    // Two films asked for at once: one is made while the other waits,
    // then the other.
    let setup = setup("queue", settings()).await;
    let a = setup.film("a", 600, 0);
    let b = setup.film("b", 20, 0);
    setup.previews.played(&a);
    setup.previews.played(&b);
    assert!(matches!(
        setup.previews.state(&b).await.unwrap(),
        PreviewState::Making { done_percent: 0 }
    ));
    setup.ready(&a).await;
    setup.ready(&b).await;
}

#[test]
fn a_sheet_s_id_names_its_set_and_number_and_nothing_else() {
    let key = "0123456789abcdef0123456789abcdef";
    let id = sheet_id(key, 3);
    assert_eq!(id.to_string(), format!("preview:{key}/3"));
    assert_eq!(sheet_of(&id), Some((key, 3)));
    for other in [
        "preview:../../etc/0",
        "preview:0123456789abcdef0123456789abcdeF/0",
        "preview:0123456789abcdef0123456789abcde/0",
        "preview:0123456789abcdef0123456789abcdef/x",
        "preview:0123456789abcdef0123456789abcdef",
        "plex:0123456789abcdef0123456789abcdef/0",
    ] {
        assert_eq!(sheet_of(&other.parse().unwrap()), None, "{other}");
    }
}

/// The ffmpeg processes reading `path`.
fn ffmpeg_reading(path: &Path) -> usize {
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.parse::<u32>().is_ok())
        })
        .filter(|e| {
            std::fs::read(e.path().join("cmdline"))
                .is_ok_and(|cmd| String::from_utf8_lossy(&cmd).contains(&*path.to_string_lossy()))
        })
        .count()
}

#[tokio::test]
async fn a_stopping_server_gives_up_the_film_being_made() {
    let setup = setup("stop", settings()).await;
    let id = setup.film("long", 7200, 0);
    let path = setup.files.files.lock()["long"].clone();
    setup.previews.played(&id);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let PreviewState::Making { done_percent } = setup.previews.state(&id).await.unwrap()
            && done_percent > 0
        {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    setup.previews.stop();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while ffmpeg_reading(&path) > 0 || !setup.previews.lock_queue().making.is_empty() {
        assert!(std::time::Instant::now() < deadline, "still making");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Given up, not failed, and nothing is left of its work.
    assert!(setup.previews.failed.lock().is_empty());
    assert_eq!(
        std::fs::read_dir(setup.tmp.as_ref().join("previews").join(WORK))
            .map(|d| d.count())
            .unwrap_or(0),
        0
    );
    assert_eq!(setup.previews.cache_total(), 0);
}

#[tokio::test]
async fn sound_that_runs_on_past_the_picture_adds_no_tiles() {
    let setup = setup("longer-sound", settings()).await;
    // 30 s of picture and 33 s of sound: a 33 s file.
    let path = setup.tmp.as_ref().join("film.mkv");
    let status = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc2=size=64x36:rate=5:d=30"])
        .args(["-f", "lavfi", "-i", "sine=d=33"])
        .args(["-c:v", "libx264", "-g", "10", "-c:a", "aac"])
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());
    setup.files.files.lock().insert("film".into(), path);
    let (_, meta) = setup.ready(&film("film")).await;
    assert_eq!(meta.count, 3);
}

/// Frames every 2 s for `secs`, each of grey `at_s × 4`, as Plex's index
/// has them.
fn plex_frames(secs: u64) -> PreviewFrames {
    PreviewFrames {
        frames: (0..=secs / 2)
            .map(|i| PreviewFrame {
                at_ms: i * 2000,
                jpeg: sheets::tests::tile(320, 132, (i * 8) as u8),
            })
            .collect(),
    }
}

#[tokio::test]
async fn the_catalog_s_own_index_is_used_and_the_file_is_never_decoded() {
    // No ffmpeg at all: the set can only come from the index.
    let setup = setup_with("plex-index", settings(), "/nonexistent/ffmpeg").await;
    let path = setup.tmp.as_ref().join("film.mkv");
    std::fs::write(&path, b"a film").unwrap();
    setup.files.files.lock().insert("film".into(), path);
    setup
        .files
        .indexed
        .lock()
        .insert("film".into(), plex_frames(60));
    let (key, meta) = setup.ready(&film("film")).await;
    // 0, 10, … 60 s.
    assert_eq!(meta.count, 7);
    assert_eq!((meta.tile_width, meta.tile_height), (320, 132));
    for i in 0..7 {
        let grey = setup.grey(&key, &meta, i).await;
        let want = f64::from(i * 5 * 8);
        assert!((grey - want).abs() < 4.0, "tile {i}: {grey:.1}, not {want}");
    }
}

#[tokio::test]
async fn an_index_that_cannot_be_read_is_made_from_the_file() {
    let setup = setup("plex-index-broken", settings()).await;
    let id = setup.film("film", 30, 0);
    setup.files.broken.lock().insert("film".into());
    let (_, meta) = setup.ready(&id).await;
    assert_eq!((meta.count, meta.tile_height), (3, 180));
}

#[test]
fn a_tile_from_an_index_is_its_last_frame_at_or_before_the_tile_s_time() {
    let frame = |at_ms, n| PreviewFrame {
        at_ms,
        jpeg: vec![n],
    };
    let frames = [frame(0, 0), frame(2000, 1), frame(4000, 2), frame(7000, 3)];
    let tiles = pick(&frames, Duration::from_secs(3)).unwrap();
    // 0 s, 3 s, 6 s: frames 0, 1 (at 2 s) and 2 (at 4 s).
    let bytes: Vec<&[u8]> = tiles.iter().map(|t| &t[..]).collect();
    assert_eq!(bytes, [[0], [1], [2]]);
    assert!(pick(&[], Duration::from_secs(3)).unwrap().is_empty());
}

/// Finding 52 (G-3): the last frame's time sizes the tiles, a JPEG copy
/// each. Past the longest film the decode path makes tiles of, the
/// catalog's index is refused, not made into tiles.
#[test]
fn an_index_past_the_longest_film_makes_no_tiles() {
    let last = spoolframe_media::MAX_DURATION.as_millis() as u64 + 1000;
    let frames = [PreviewFrame {
        at_ms: last,
        jpeg: vec![0],
    }];
    assert!(pick(&frames, Duration::from_secs(10)).is_err());
}

#[test]
fn the_newest_go_ahead_of_what_was_queued_ahead_and_behind_what_is_played() {
    let mut queue = Queue::default();
    queue.ahead_of_the_rest([film("1"), film("2"), film("3")].into_iter());
    queue.push(&film("9"), true);
    // A later sweep: 3 is newest now, 4 is new.
    queue.ahead_of_the_rest([film("3"), film("4")].into_iter());
    let urgent: Vec<_> = std::iter::from_fn(|| queue.pop_urgent()).collect();
    assert_eq!(keys(&urgent), ["9"]);
    let ahead: Vec<_> = std::iter::from_fn(|| queue.pop_ahead()).collect();
    assert_eq!(keys(&ahead), ["3", "4", "1", "2"]);
}

/// PX-09 finding 15 (B-8): every `GetPreviews` for another item queued
/// it as played, without end. The played queue keeps the newest
/// [`MAX_URGENT`]: one asked for longer ago is dropped, and made again
/// only if asked for again.
#[test]
fn the_played_queue_keeps_only_the_newest() {
    let mut queue = Queue::default();
    let asked: Vec<_> = (0..100).map(|n| film(&n.to_string())).collect();
    for id in &asked {
        queue.push(id, true);
    }
    let urgent: Vec<_> = std::iter::from_fn(|| queue.pop_urgent()).collect();
    assert_eq!(urgent, asked[asked.len() - MAX_URGENT..]);
    assert!(!queue.queued.contains(&asked[0]));
}

#[test]
fn a_file_being_made_ahead_that_is_played_is_made_at_once_where_it_is() {
    let mut queue = Queue::default();
    queue.ahead_of_the_rest([film("1"), film("2")].into_iter());
    let one = queue.pop_ahead().unwrap();
    queue.push(&one, true);
    // Not queued to be made again: on where it is, as played.
    assert!(queue.urgent.is_empty());
    assert!(queue.making[&one].urgent);
    // One still waiting is moved to the played ones.
    queue.push(&film("2"), true);
    assert_eq!(
        keys(&queue.urgent.iter().cloned().collect::<Vec<_>>()),
        ["2"]
    );
}

#[tokio::test]
async fn previews_made_ahead_stop_when_the_cache_is_full_and_a_play_still_gets_its_own() {
    let setup = setup(
        "ahead-full",
        PreviewSettings {
            when: When::Ahead,
            // Room for one set, and not a byte more.
            cache_limit_bytes: 1,
            ..settings()
        },
    )
    .await;
    let a = setup.film("a", 20, 0);
    let b = setup.film("b", 20, 0);
    setup.previews.ahead([a.clone(), b.clone()]);
    let idle = |previews: &Previews| {
        let queue = previews.lock_queue();
        queue.making.is_empty() && queue.ahead.is_empty()
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !idle(&setup.previews) {
        assert!(
            std::time::Instant::now() < deadline,
            "the queue never emptied"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let set = |name: &str| {
        let path = setup.files.files.lock()[name].clone();
        async move { key(&path, Duration::from_secs(10)).await.unwrap() }
    };
    let (key_a, key_b) = (set("a").await, set("b").await);
    assert!(
        setup.previews.cache.get(&key_a).await.is_some(),
        "a was made ahead"
    );
    assert!(
        setup.previews.cache.get(&key_b).await.is_none(),
        "b was made ahead, evicting a"
    );
    // b is played: made, though it evicts a.
    setup.previews.played(&b);
    setup.ready(&b).await;
}

fn keys(ids: &[MediaId]) -> Vec<&str> {
    ids.iter().map(MediaId::key).collect()
}

/// Tiles made only when the test lets them: each tile asked for is told
/// on `asked` (the film's key and the tile's number), and then waits for
/// a permit.
struct Gated {
    asked: tokio::sync::mpsc::UnboundedSender<(String, u64)>,
    permits: Arc<tokio::sync::Semaphore>,
}

#[async_trait]
impl make::MakeTile for Gated {
    async fn tile(
        &self,
        source: &make::Source,
        _: bool,
        at: Duration,
        _: &Path,
    ) -> Result<Option<Vec<u8>>, String> {
        let key = source
            .input
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        self.asked.send((key, at.as_secs() / 10)).unwrap();
        self.permits.acquire().await.unwrap().forget();
        Ok(Some(sheets::tests::tile(320, 180, 128)))
    }
}

/// A [`Setup`] whose tiles are [`Gated`]: the tiles asked for, and the
/// permits that let them be made.
struct GatedSetup {
    setup: Setup,
    asked: tokio::sync::mpsc::UnboundedReceiver<(String, u64)>,
    permits: Arc<tokio::sync::Semaphore>,
}

async fn gated(test: &str, settings: PreviewSettings) -> GatedSetup {
    let (tx, asked) = tokio::sync::mpsc::unbounded_channel();
    let permits = Arc::new(tokio::sync::Semaphore::new(0));
    let maker = Arc::new(Gated {
        asked: tx,
        permits: permits.clone(),
    });
    let setup = setup_making(test, settings, "ffmpeg", Some(maker)).await;
    GatedSetup {
        setup,
        asked,
        permits,
    }
}

impl GatedSetup {
    /// The next tile asked for.
    async fn next_asked(&mut self) -> (String, u64) {
        tokio::time::timeout(Duration::from_secs(30), self.asked.recv())
            .await
            .expect("no tile was asked for")
            .unwrap()
    }

    /// Whether `id` is held back before its next tile.
    async fn until_paused(&self, id: &MediaId) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !self
            .setup
            .previews
            .lock_queue()
            .making
            .get(id)
            .is_some_and(|m| m.paused)
        {
            assert!(std::time::Instant::now() < deadline, "{id} never paused");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

fn ahead_settings(workers: usize) -> PreviewSettings {
    PreviewSettings {
        when: When::Ahead,
        workers,
        ..settings()
    }
}

#[tokio::test]
async fn previews_made_ahead_wait_while_an_encode_runs_and_go_on_where_they_were() {
    let mut gated = gated("ahead-pause", ahead_settings(1)).await;
    let a = gated.setup.film("a", 30, 0);
    gated.setup.previews.ahead([a.clone()]);
    assert_eq!(gated.next_asked().await, ("a".into(), 0));
    // A conversion starts while tile 0 is being made: that one is
    // finished, and the next waits.
    let encode = gated.setup.encoding.begin();
    gated.permits.add_permits(1);
    gated.until_paused(&a).await;
    assert!(
        gated.asked.try_recv().is_err(),
        "a tile was made beside the encode"
    );
    // It ends: on from tile 1, tile 0 kept.
    drop(encode);
    assert_eq!(gated.next_asked().await, ("a".into(), 1));
    gated.permits.add_permits(2);
    let (_, meta) = gated.setup.ready(&a).await;
    assert_eq!(meta.count, 3);
}

#[tokio::test]
async fn nothing_is_started_ahead_while_an_encode_runs_but_a_played_film_is_made_at_once() {
    let mut gated = gated("on-play-encoding", ahead_settings(4)).await;
    let a = gated.setup.film("a", 30, 0);
    let b = gated.setup.film("b", 30, 0);
    let encode = gated.setup.encoding.begin();
    gated.setup.previews.ahead([a.clone()]);
    gated.setup.previews.played(&b);
    // b, though the encode runs; a, made ahead, is not started.
    assert_eq!(gated.next_asked().await, ("b".into(), 0));
    gated.permits.add_permits(3);
    assert_eq!(gated.next_asked().await, ("b".into(), 1));
    assert_eq!(gated.next_asked().await, ("b".into(), 2));
    gated.setup.ready(&b).await;
    assert!(
        gated.asked.try_recv().is_err(),
        "a was started beside the encode"
    );
    assert!(gated.setup.previews.lock_queue().ahead.contains(&a));
    drop(encode);
    assert_eq!(gated.next_asked().await, ("a".into(), 0));
}

#[tokio::test]
async fn a_film_held_back_by_an_encode_goes_on_at_once_when_it_is_played() {
    let mut gated = gated("promoted", ahead_settings(1)).await;
    let a = gated.setup.film("a", 30, 0);
    gated.setup.previews.ahead([a.clone()]);
    assert_eq!(gated.next_asked().await, ("a".into(), 0));
    let _encode = gated.setup.encoding.begin();
    gated.permits.add_permits(1);
    gated.until_paused(&a).await;
    // Played: on from tile 1 in the worker it was in, the encode still
    // running.
    gated.setup.previews.played(&a);
    assert_eq!(gated.next_asked().await, ("a".into(), 1));
    gated.permits.add_permits(2);
    gated.setup.ready(&a).await;
}

#[tokio::test]
async fn n_workers_make_n_files_ahead_at_once() {
    let mut gated = gated("workers", ahead_settings(2)).await;
    let films = ["a", "b", "c"].map(|k| gated.setup.film(k, 30, 0));
    gated.setup.previews.ahead(films.clone());
    let mut first = [gated.next_asked().await, gated.next_asked().await];
    first.sort();
    assert_eq!(first, [("a".into(), 0), ("b".into(), 0)]);
    {
        let queue = gated.setup.previews.lock_queue();
        assert_eq!(queue.making.len(), 2);
        assert_eq!(
            keys(&queue.ahead.iter().cloned().collect::<Vec<_>>()),
            ["c"]
        );
    }
    // a and b finish; c follows.
    gated.permits.add_permits(6);
    let mut seen = HashSet::new();
    while !seen.contains("c") {
        seen.insert(gated.next_asked().await.0);
    }
    gated.permits.add_permits(3);
    for id in &films {
        let path = gated.setup.files.files.lock()[id.key()].clone();
        let key = key(&path, Duration::from_secs(10)).await.unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while gated.setup.previews.cache.get(&key).await.is_none() {
            assert!(std::time::Instant::now() < deadline, "{id} not made");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

/// With no room in the cache, a set made ahead is not made, and Plex is
/// not asked where its file is first: a full cache asked once per item
/// of the library every sweep. A set for a play is still made. Review
/// 2026-10-11 M13.
#[tokio::test]
async fn a_full_cache_asks_plex_nothing_for_previews_made_ahead() {
    let full = PreviewSettings {
        cache_limit_bytes: 0,
        ..settings()
    };
    let setup = setup("full-cache", full).await;
    let asked = || setup.files.asked.load(std::sync::atomic::Ordering::SeqCst);
    let id = film("not-here");
    setup.previews.make(&id, false).await.unwrap();
    assert_eq!(asked(), 0);
    // A play's set is made whatever the room: Plex is asked.
    setup.previews.make(&id, true).await.unwrap();
    assert_eq!(asked(), 1);
}

/// A scrub's sheet fetches ask Plex once a minute, not per sheet, and a
/// sheet scaled for a size is kept and served again (review 2026-10-11
/// M16).
#[tokio::test]
async fn sheets_fetched_again_ask_the_catalog_once_and_scale_once() {
    let setup = setup_with("sheet-again", settings(), "/nonexistent/ffmpeg").await;
    let path = setup.tmp.as_ref().join("film.mkv");
    std::fs::write(&path, b"a film").unwrap();
    setup.files.files.lock().insert("film".into(), path);
    setup
        .files
        .indexed
        .lock()
        .insert("film".into(), plex_frames(60));
    let (key, _) = setup.ready(&film("film")).await;
    let asked = || setup.files.asked.load(std::sync::atomic::Ordering::SeqCst);
    let before = asked();
    for _ in 0..5 {
        assert!(setup.previews.sheet(&key, 0).await.is_some());
    }
    assert_eq!(asked() - before, 1, "one check for five fetches");
    let small = setup.previews.sheet_at(&key, 0, 160).await.unwrap();
    let again = setup.previews.sheet_at(&key, 0, 160).await.unwrap();
    assert!(Arc::ptr_eq(&small, &again), "scaled once, then kept");
    let decoded = image::load_from_memory(&small).unwrap();
    assert!(decoded.width() <= 160);
}

/// A slow film holds up only itself: a second film played after it has
/// its previews started while the first is still being made (review
/// 2026-10-11 M17).
#[tokio::test]
async fn a_slow_film_does_not_hold_up_the_next_played() {
    let mut gated = gated("two-urgent", settings()).await;
    let a = gated.setup.film("a", 30, 0);
    let b = gated.setup.film("b", 30, 0);
    gated.setup.previews.played(&a);
    gated.setup.previews.played(&b);
    let mut started = std::collections::HashSet::new();
    for _ in 0..2 {
        let (film, _) = tokio::time::timeout(Duration::from_secs(10), gated.asked.recv())
            .await
            .expect("only one film is being made")
            .unwrap();
        started.insert(film);
    }
    assert_eq!(started.len(), 2, "{started:?}");
    gated.permits.add_permits(1000);
    gated.setup.ready(&a).await;
    gated.setup.ready(&b).await;
}

/// Two items of one file (the film in two libraries) make its set once:
/// the second waits for the first and finds it made. Review 2026-10-11
/// L20.
#[tokio::test]
async fn two_items_of_one_file_make_its_previews_once() {
    let mut gated = gated("one-file-twice", settings()).await;
    let a = gated.setup.film("a", 30, 0);
    let path = gated.setup.files.files.lock()["a"].clone();
    gated.setup.files.files.lock().insert("twin".into(), path);
    let twin = film("twin");
    gated.setup.previews.played(&a);
    gated.setup.previews.played(&twin);
    gated.permits.add_permits(1000);
    let (one, _) = gated.setup.ready(&a).await;
    let (other, _) = gated.setup.ready(&twin).await;
    assert_eq!(one, other);
    let mut asked = 0;
    while gated.asked.try_recv().is_ok() {
        asked += 1;
    }
    assert_eq!(asked, 3, "30 s at 10 s is 3 tiles, made once");
}

/// A tile that fails for the moment (a timeout) fails the set, but the next
/// play tries again rather than finding it failed until a restart. Review
/// 2026-10-11 L21.
#[tokio::test]
async fn a_set_that_timed_out_is_made_again_at_the_next_play() {
    struct TimesOutOnce(std::sync::atomic::AtomicBool);
    #[async_trait]
    impl make::MakeTile for TimesOutOnce {
        async fn tile(
            &self,
            _: &make::Source,
            _: bool,
            _: Duration,
            _: &Path,
        ) -> Result<Option<Vec<u8>>, String> {
            if !self.0.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return Err("ffmpeg took over 120s for the tile at 0s".into());
            }
            Ok(Some(sheets::tests::tile(320, 180, 128)))
        }
    }
    let maker = Arc::new(TimesOutOnce(Default::default()));
    let setup = setup_making("timed-out", settings(), "ffmpeg", Some(maker)).await;
    let id = setup.film("a", 30, 0);
    setup.previews.played(&id);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while setup.previews.counts().failed == 0 {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    setup.previews.played(&id);
    setup.ready(&id).await;
}

/// A frame a sparse index repeats over many tiles is one copy, shared.
/// Review 2026-10-11 L23.
#[test]
fn tiles_that_show_one_frame_share_its_bytes() {
    let frames = [
        PreviewFrame {
            at_ms: 0,
            jpeg: vec![7; 1000],
        },
        PreviewFrame {
            at_ms: 60_000,
            jpeg: vec![8; 1000],
        },
    ];
    let tiles = pick(&frames, Duration::from_secs(10)).unwrap();
    assert_eq!(tiles.len(), 7);
    assert!(tiles[1..6].iter().all(|t| Arc::ptr_eq(t, &tiles[0])));
}
