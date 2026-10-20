//! The copy pool's tests (PX-37): a stand-in ffmpeg where only what the
//! pool does with it matters, and real ffmpeg and ffprobe on films made
//! at test time for what a copy's tracks hold and how they mux.

use std::future::Future;
use std::process::Command;

use spoolframe_types::{AudioCodec, VideoCodec};

use super::*;
use crate::downloads::DeviceClass;
use crate::encode::JobLimits;
use crate::encode::job::{Load, Units, tests::ffmpeg};
use crate::store::optimized::{CopyTarget, CopyVideo};
use crate::store::tests::{TestStore, test_store};

/// What a test works with: the pool, and what it stands on.
struct Setup {
    pool: Pool,
    /// What on-demand requests' scopes hold: the fake show's episodes,
    /// each file the test film once [`Setup::write_episodes`] puts them.
    catalog: Arc<crate::test_support::FakeCatalog>,
    jobs: Arc<Jobs>,
    transcoding: Arc<Transcoding>,
    store: TestStore,
    dir: PathBuf,
    media: PathBuf,
    _tmp: spoolframe_test_dir::TestDir,
}

/// Run ffmpeg with `args`, which must succeed.
fn run_ffmpeg(args: &[&str]) {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .expect("ffmpeg runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// 2 s of H.264 with an AC-3 track, an MP2 track and a SubRip subtitle,
/// in Matroska.
fn film(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let srt = dir.join("film.srt");
    std::fs::write(&srt, "1\n00:00:00,500 --> 00:00:01,500\nHello\n").unwrap();
    let path = dir.join("film.mkv");
    run_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=160x90:rate=25",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=880:sample_rate=48000",
        "-i",
        &srt.display().to_string(),
        "-t",
        "2",
        "-map",
        "0",
        "-map",
        "1",
        "-map",
        "2",
        "-map",
        "3",
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        "-c:a:0",
        "ac3",
        "-c:a:1",
        "mp2",
        "-c:s",
        "srt",
        &path.display().to_string(),
    ]);
    path
}

/// A pool with a budget of `max_jobs` software encodes and `max_size`
/// bytes, its jobs running `ffmpeg` (a path, or `ffmpeg` itself); it
/// tries for a place every `retry`.
async fn setup_with(test: &str, ffmpeg: PathBuf, max_jobs: u32, max_size: u64) -> Setup {
    let tmp = spoolframe_test_dir::TestDir::new(&format!("spoolframe-optimized-{test}")).await;
    let root = tmp.as_ref().to_path_buf();
    let store = test_store(&format!("optimized-pool-{test}")).await;
    let limits = JobLimits {
        budget: Units::jobs(max_jobs),
        max_remux: 32,
        max_gpu_jobs: 3,
        scratch_dir: root.join("scratch"),
        scratch_limit_bytes: 1 << 30,
        read_ahead: Duration::from_secs(60),
        copy_segment_secs: 2,
        stalled_after: crate::encode::job::STALLED_AFTER,
        gpu_filters: false,
        tone_map: spoolframe_media::encoders::ToneMapSetting::Auto,
        video_codecs: crate::config::DEFAULT_VIDEO_CODECS.to_vec(),
    };
    let jobs = Arc::new(Jobs::new(limits, Tool::unconfined(&ffmpeg)).unwrap());
    let transcoding = Arc::new(Transcoding::new(
        Tool::unconfined(&ffmpeg),
        Vec::new(),
        root.join("encoder-test"),
    ));
    let prober = Arc::new(Prober::new(
        Tool::unconfined("ffprobe"),
        Prober::DEFAULT_TIMEOUT,
    ));
    let dir = root.join("optimized");
    let media = film(&root.join("media"));
    let catalog = Arc::new(crate::test_support::FakeCatalog::new(media.clone()));
    let pool = Pool::start_with(
        PoolSources {
            store: store.store.clone(),
            jobs: jobs.clone(),
            transcoding: transcoding.clone(),
            prober,
            catalog: catalog.clone(),
            catalog_changes: None,
        },
        &OptimizedSettings {
            dir: dir.clone(),
            max_size_bytes: max_size,
            binge_ahead: true,
        },
        // Nothing frees a place on its own in these tests.
        Duration::from_secs(600),
    )
    .await
    .unwrap();
    Setup {
        pool,
        catalog,
        jobs,
        transcoding,
        store,
        dir,
        media,
        _tmp: tmp,
    }
}

/// [`setup_with`] with a stand-in ffmpeg running `body`.
async fn setup(test: &str, body: &str, max_jobs: u32, max_size: u64) -> Setup {
    let bin = std::env::temp_dir().join(format!(
        "spoolframe-optimized-bin-{test}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    setup_with(test, ffmpeg(&bin, body), max_jobs, max_size).await
}

const MOBILE: CopyVideo = CopyVideo {
    codec: VideoCodec::H264,
    height: 1080,
    max_bitrate: Some(4_000_000),
    tone_map: false,
    copied: false,
};

/// Profile `profile`'s video: `Mobile`'s, or one of its own for any other,
/// so two profiles' copies share no video track.
fn video_of(profile: &str) -> CopyVideo {
    match profile {
        "Mobile" => MOBILE,
        "TV" => CopyVideo {
            height: 720,
            ..MOBILE
        },
        _ => CopyVideo {
            height: 480,
            ..MOBILE
        },
    }
}

/// A copy of `media` in profile `profile`.
async fn asked(media: &Path, profile: &str) -> NewCopy {
    NewCopy {
        media_id: "plex:movie:1".into(),
        media_path: media.display().to_string(),
        source_stamp: crate::files::stamp_text(media).await.unwrap(),
        target: CopyTarget::Profile(profile.into()),
        video: video_of(profile),
    }
}

/// A download's copy of `media` for an Apple device, its video the file's.
async fn asked_for_apple(media: &Path, height: u32) -> NewCopy {
    NewCopy {
        target: CopyTarget::Original {
            class: DeviceClass::Apple,
        },
        video: CopyVideo {
            codec: VideoCodec::Hevc,
            height,
            max_bitrate: None,
            tone_map: false,
            copied: true,
        },
        ..asked(media, "Mobile").await
    }
}

/// The E-AC-3 a stereo track at stream `stream` becomes.
fn eac3_of(stream: u32) -> Recipe {
    Recipe::Audio(crate::store::optimized::AudioRecipe {
        stream,
        codec: AudioCodec::Eac3,
        channels: 2,
        bitrate: crate::planner::encoded_audio_bitrate(AudioCodec::Eac3, 2),
    })
}

/// Until `check` holds, for at most 60 s.
async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let until = Instant::now() + Duration::from_secs(60);
    while !check().await {
        assert!(Instant::now() < until, "never: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

impl Setup {
    async fn state(&self, id: &str) -> Option<CopyState> {
        self.store
            .optimized_copy(id)
            .await
            .unwrap()
            .map(|c| c.state)
    }

    fn part(&self, track: &str) -> PathBuf {
        self.dir.join(MAKING_DIR).join(format!("{track}.mkv.part"))
    }

    /// Copy `id`'s tracks.
    async fn tracks(&self, id: &str) -> Vec<TrackRow> {
        self.store.optimized_copy_tracks(id).await.unwrap()
    }

    /// Copy `id`'s first track's id: its video, for a profile's copy.
    async fn first_track(&self, id: &str) -> String {
        self.tracks(id).await[0].id.clone()
    }

    /// A copy made already of `recipes`, each track `size` bytes, its file
    /// there; in profile `profile`, used at the store's clock now.
    async fn made_of(&self, copy: NewCopy, recipes: &[Recipe], size: u64) -> CopyRow {
        let (row, _) = self
            .store
            .request_optimized_copy(&copy, recipes)
            .await
            .unwrap();
        for track in self.tracks(&row.id).await {
            if track.state != CopyState::Ready {
                self.store
                    .optimized_track_ready(&track.id, size)
                    .await
                    .unwrap();
                std::fs::write(track_file(&self.dir, &track.id), vec![0; size as usize]).unwrap();
            }
        }
        row
    }

    /// A copy in profile `profile` made already: its video track alone,
    /// `size` bytes.
    async fn made(&self, profile: &str, size: u64) -> CopyRow {
        let copy = asked(&self.media, profile).await;
        self.made_of(copy, &[Recipe::Video(video_of(profile))], size)
            .await
    }

    /// Whether copy `id`'s first track's file is there.
    fn kept(&self, track: &str) -> bool {
        track_file(&self.dir, track).exists()
    }
}

/// Each track, by its file name in `ran`'s lines (a stand-in ffmpeg's
/// last argument), made once.
#[tokio::test]
async fn a_second_request_for_a_copy_starts_no_second_ffmpeg() {
    let runs =
        std::env::temp_dir().join(format!("spoolframe-runs-{}", uuid::Uuid::new_v4().simple()));
    let body = format!(
        "echo run >> {}\nfor last; do :; done\necho copy > \"$last\"",
        runs.display()
    );
    let s = setup("dedupe", &body, 1, 1 << 30).await;
    let first = s
        .pool
        .request(asked(&s.media, "Mobile").await)
        .await
        .unwrap();
    s.pool
        .request(asked(&s.media, "Mobile").await)
        .await
        .unwrap();
    eventually("the copy is made", || async {
        s.state(&first.id).await == Some(CopyState::Ready)
    })
    .await;
    s.pool
        .request(asked(&s.media, "Mobile").await)
        .await
        .unwrap();
    eventually("the pool is idle", || async {
        let state = s.pool.inner.state.lock();
        state.queue.head().is_none() && state.running.is_none()
    })
    .await;
    let ran = std::fs::read_to_string(&runs).unwrap().lines().count();
    let _ = std::fs::remove_file(&runs);
    assert_eq!(ran, s.tracks(&first.id).await.len());
}

/// The test film's copy in `Mobile`: its video, and its (mono) MP2
/// converted; its AC-3 is the file's own.
#[tokio::test]
async fn a_profile_copy_stores_its_video_and_the_audio_it_converts() {
    let s = setup("recipes", "exit 1", 0, 1 << 30).await;
    let copy = s
        .pool
        .request(asked(&s.media, "Mobile").await)
        .await
        .unwrap();
    let recipes: Vec<String> = s
        .tracks(&copy.id)
        .await
        .iter()
        .map(|t| t.recipe.key())
        .collect();
    assert_eq!(
        recipes,
        ["video:h264:1080:4000000:same", "audio:2:aac:1:192000"]
    );
}

#[tokio::test]
async fn a_made_track_is_renamed_from_its_part() {
    let s = setup(
        "renamed",
        "for last; do :; done\necho copy > \"$last\"",
        1,
        1 << 30,
    )
    .await;
    let copy = s
        .pool
        .request(asked(&s.media, "Mobile").await)
        .await
        .unwrap();
    eventually("the copy is made", || async {
        s.state(&copy.id).await == Some(CopyState::Ready)
    })
    .await;
    let video = s.first_track(&copy.id).await;
    assert_eq!(
        std::fs::read_to_string(track_file(&s.dir, &video)).unwrap(),
        "copy\n"
    );
}

#[tokio::test]
async fn a_copy_whose_ffmpeg_fails_says_ffmpeg_s_last_words() {
    let s = setup("fails", "echo 'Invalid data found' >&2\nexit 1", 1, 1 << 30).await;
    let copy = s
        .pool
        .request(asked(&s.media, "Mobile").await)
        .await
        .unwrap();
    eventually("the copy fails", || async {
        s.state(&copy.id).await == Some(CopyState::Failed)
    })
    .await;
    let failed = s.store.optimized_copy(&copy.id).await.unwrap().unwrap();
    assert_eq!(failed.failure.as_deref(), Some("Invalid data found"));
}

#[tokio::test]
async fn a_track_live_playback_preempts_waits_again_at_the_head_without_its_part() {
    let body = "for last; do :; done\necho partial > \"$last\"\nexec sleep 30";
    let s = setup("preempted", body, 1, 1 << 30).await;
    let first = s
        .pool
        .request(asked(&s.media, "Mobile").await)
        .await
        .unwrap();
    s.pool.request(asked(&s.media, "TV").await).await.unwrap();
    let video = s.first_track(&first.id).await;
    eventually("the first track is being written", || async {
        s.part(&video).exists()
    })
    .await;
    let live = s.jobs.hold(Load::Software, None).unwrap();
    eventually("the first track waits again", || async {
        s.store
            .optimized_track(&video)
            .await
            .unwrap()
            .unwrap()
            .state
            == CopyState::Waiting
    })
    .await;
    assert_eq!(
        (s.part(&video).exists(), s.pool.queued().first()),
        (false, Some(&video))
    );
    drop(live);
}

#[tokio::test]
async fn over_budget_a_leased_copy_stays_and_the_next_goes() {
    let s = setup("leased", "exit 1", 1, 150).await;
    let older = s.made("Mobile", 100).await;
    let lease = s.pool.lease(&older.id).await.unwrap().unwrap();
    s.store.clock().advance(Duration::from_secs(60));
    let newer = s.made("TV", 100).await;
    let (older_video, newer_video) = (
        s.first_track(&older.id).await,
        s.first_track(&newer.id).await,
    );
    s.pool.sweep().await.unwrap();
    assert_eq!((s.kept(&older_video), s.kept(&newer_video)), (true, false));
    drop(lease);
}

#[tokio::test]
async fn over_budget_the_least_recently_used_track_goes() {
    let s = setup("lru", "exit 1", 1, 150).await;
    let older = s.made("Mobile", 100).await;
    s.store.clock().advance(Duration::from_secs(60));
    let newer = s.made("TV", 100).await;
    let (older_video, newer_video) = (
        s.first_track(&older.id).await,
        s.first_track(&newer.id).await,
    );
    s.pool.sweep().await.unwrap();
    assert_eq!((s.kept(&older_video), s.kept(&newer_video)), (false, true));
}

#[tokio::test]
async fn an_evicted_track_takes_its_copy_with_it() {
    let s = setup("evicted-copy", "exit 1", 1, 150).await;
    let older = s.made("Mobile", 100).await;
    s.store.clock().advance(Duration::from_secs(60));
    s.made("TV", 100).await;
    s.pool.sweep().await.unwrap();
    assert_eq!(s.state(&older.id).await, None);
}

#[tokio::test]
async fn over_budget_an_outdated_track_goes_before_the_least_recently_used() {
    let s = setup("outdated-first", "exit 1", 1, 150).await;
    let older = s.made("Mobile", 100).await;
    s.store.clock().advance(Duration::from_secs(60));
    let outdated = s.made("TV", 100).await;
    let (older_video, outdated_video) = (
        s.first_track(&older.id).await,
        s.first_track(&outdated.id).await,
    );
    s.store.delete_optimized_profile("TV").await.unwrap();
    s.pool.sweep().await.unwrap();
    assert_eq!(
        (s.kept(&older_video), s.kept(&outdated_video)),
        (true, false)
    );
}

#[tokio::test]
async fn an_outdated_copy_is_removed_though_the_pool_has_room() {
    let s = setup("outdated-room", "exit 1", 1, 1 << 30).await;
    let outdated = s.made("TV", 100).await;
    s.store.delete_optimized_profile("TV").await.unwrap();
    s.pool.sweep().await.unwrap();
    assert_eq!(s.state(&outdated.id).await, None);
}

#[tokio::test]
async fn an_outdated_copy_is_not_lent() {
    let s = setup("outdated-lease", "exit 1", 1, 1 << 30).await;
    let outdated = s.made("TV", 100).await;
    s.store.delete_optimized_profile("TV").await.unwrap();
    assert!(s.pool.lease(&outdated.id).await.unwrap().is_none());
}

#[tokio::test]
async fn a_copy_deleted_while_read_goes_when_its_lease_ends() {
    let s = setup("delete-leased", "exit 1", 1, 1 << 30).await;
    let copy = s.made("Mobile", 100).await;
    let video = s.first_track(&copy.id).await;
    let lease = s.pool.lease(&copy.id).await.unwrap().unwrap();
    assert_eq!(s.pool.delete(&copy.id).await.unwrap(), Deleted::Deferred);
    assert!(
        lease.files().video.as_ref().is_some_and(|f| f.exists()),
        "kept while read"
    );
    drop(lease);
    eventually("the copy and its track are gone", || async {
        s.state(&copy.id).await.is_none() && !s.kept(&video)
    })
    .await;
}

#[tokio::test]
async fn a_copy_deleted_unread_goes_at_once() {
    let s = setup("delete-now", "exit 1", 1, 1 << 30).await;
    let copy = s.made("Mobile", 100).await;
    let video = s.first_track(&copy.id).await;
    assert_eq!(s.pool.delete(&copy.id).await.unwrap(), Deleted::Now);
    assert!(!s.kept(&video));
}

/// `Mobile`'s copy and a download's Apple copy of the same film, made,
/// sharing the film's E-AC-3 (stream 2, its MP2).
async fn sharing(s: &Setup) -> (CopyRow, CopyRow, String) {
    let mobile = s
        .made_of(
            asked(&s.media, "Mobile").await,
            &[Recipe::Video(MOBILE), eac3_of(2)],
            100,
        )
        .await;
    let apple = s
        .made_of(asked_for_apple(&s.media, 90).await, &[eac3_of(2)], 100)
        .await;
    let shared = s.tracks(&apple.id).await[0].id.clone();
    (mobile, apple, shared)
}

#[tokio::test]
async fn a_track_two_copies_use_is_one_file() {
    let s = setup("shared-one-file", "exit 1", 1, 1 << 30).await;
    let (mobile, _, shared) = sharing(&s).await;
    assert!(s.tracks(&mobile.id).await.iter().any(|t| t.id == shared));
}

#[tokio::test]
async fn a_shared_track_stays_when_one_of_its_copies_is_deleted() {
    let s = setup("shared-delete", "exit 1", 1, 1 << 30).await;
    let (_, apple, shared) = sharing(&s).await;
    s.pool.delete(&apple.id).await.unwrap();
    s.pool.sweep().await.unwrap();
    assert!(s.kept(&shared));
}

#[tokio::test]
async fn a_profile_edit_removes_its_video_track_and_keeps_its_audio() {
    let s = setup("edit-video-only", "exit 1", 1, 1 << 30).await;
    let (mobile, _, shared) = sharing(&s).await;
    let video = s.first_track(&mobile.id).await;
    s.store
        .put_optimized_profile(&crate::store::optimized::Profile {
            name: "Mobile".into(),
            video_codec: VideoCodec::H264,
            height: 1080,
            max_bitrate: 6_000_000,
        })
        .await
        .unwrap();
    s.pool.sweep().await.unwrap();
    assert_eq!((s.kept(&video), s.kept(&shared)), (false, true));
}

#[tokio::test]
async fn a_profile_edit_keeps_an_audio_track_no_other_copy_uses() {
    let s = setup("edit-keeps-audio", "exit 1", 1, 1 << 30).await;
    let mobile = s
        .made_of(
            asked(&s.media, "Mobile").await,
            &[Recipe::Video(MOBILE), eac3_of(2)],
            100,
        )
        .await;
    let audio = s.tracks(&mobile.id).await[1].id.clone();
    s.store.delete_optimized_profile("Mobile").await.unwrap();
    s.pool.sweep().await.unwrap();
    assert!(s.kept(&audio));
}

/// Over budget with a lease on `Mobile`'s copy: neither of its tracks
/// goes, though the Apple copy's shared one is the least recently used.
#[tokio::test]
async fn a_lease_holds_every_track_of_its_copy() {
    let s = setup("lease-every-track", "exit 1", 1, 50).await;
    let (mobile, _, shared) = sharing(&s).await;
    let video = s.first_track(&mobile.id).await;
    let lease = s.pool.lease(&mobile.id).await.unwrap().unwrap();
    s.pool.sweep().await.unwrap();
    assert_eq!((s.kept(&video), s.kept(&shared)), (true, true));
    drop(lease);
}

#[tokio::test]
async fn a_lease_names_each_stored_track_by_what_it_stands_in_for() {
    let s = setup("lease-files", "exit 1", 1, 1 << 30).await;
    let (mobile, _, shared) = sharing(&s).await;
    let video = s.first_track(&mobile.id).await;
    let lease = s.pool.lease(&mobile.id).await.unwrap().unwrap();
    assert_eq!(
        lease.files(),
        &CopyFiles {
            video: Some(track_file(&s.dir, &video)),
            audio: [(2, track_file(&s.dir, &shared))].into(),
        }
    );
}

/// A pool restarted over `s`'s directory and store, with no place in
/// the budget: what its start did stays as it left it.
async fn restarted(s: &Setup, test: &str) -> Pool {
    restarted_as(s, test, true).await
}

/// [`restarted`], with binge-ahead on or off.
async fn restarted_as(s: &Setup, test: &str, binge_ahead: bool) -> Pool {
    let bin = std::env::temp_dir().join(format!(
        "spoolframe-optimized-bin-{test}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let limits = JobLimits {
        budget: Units::jobs(0),
        ..s.jobs.limits()
    };
    let jobs = Arc::new(Jobs::new(limits, Tool::unconfined(ffmpeg(&bin, "exit 1"))).unwrap());
    Pool::start_with(
        PoolSources {
            store: s.store.store.clone(),
            jobs,
            transcoding: Arc::new(Transcoding::new(
                Tool::unconfined("ffmpeg"),
                Vec::new(),
                bin.join("encoder-test"),
            )),
            prober: Arc::new(Prober::new(
                Tool::unconfined("ffprobe"),
                Prober::DEFAULT_TIMEOUT,
            )),
            catalog: s.catalog.clone(),
            catalog_changes: None,
        },
        &OptimizedSettings {
            dir: s.dir.clone(),
            max_size_bytes: 1 << 30,
            binge_ahead,
        },
        Duration::from_secs(600),
    )
    .await
    .unwrap()
}

/// A track a stopped server was making: its row `making` and a part.
async fn left_making(s: &Setup) -> String {
    s.pool.stop();
    let (row, _) = s
        .store
        .request_optimized_copy(&asked(&s.media, "Mobile").await, &[Recipe::Video(MOBILE)])
        .await
        .unwrap();
    let track = s.first_track(&row.id).await;
    s.store.optimized_track_making(&track).await.unwrap();
    std::fs::write(s.part(&track), "half").unwrap();
    track
}

#[tokio::test]
async fn a_track_a_stopped_server_was_making_waits_again_at_start() {
    let s = setup("restart-requeue", "exit 1", 0, 1 << 30).await;
    let track = left_making(&s).await;
    let pool = restarted(&s, "restart-requeue").await;
    assert_eq!(
        (
            s.store
                .optimized_track(&track)
                .await
                .unwrap()
                .unwrap()
                .state,
            pool.queued()
        ),
        (CopyState::Waiting, vec![track.clone()])
    );
}

#[tokio::test]
async fn a_part_a_stopped_server_left_is_removed_at_start() {
    let s = setup("restart-part", "exit 1", 0, 1 << 30).await;
    let track = left_making(&s).await;
    let _pool = restarted(&s, "restart-part").await;
    assert!(!s.part(&track).exists());
}

/// 0.13's whole-file copies, named by their old ids: their rows were
/// dropped by migration 0039, and their files go at the next start.
#[tokio::test]
async fn a_whole_file_copy_0_13_left_is_removed_at_start_and_a_track_stays() {
    let s = setup("restart-strays", "exit 1", 0, 1 << 30).await;
    let copy = s.made("Mobile", 10).await;
    let track = s.first_track(&copy.id).await;
    let stray = s.dir.join("0123456789abcdef0123456789abcdef.mkv");
    std::fs::write(&stray, "a whole copy").unwrap();
    s.pool.stop();
    let _pool = restarted(&s, "restart-strays").await;
    assert_eq!((stray.exists(), s.kept(&track)), (false, true));
}

#[tokio::test]
async fn a_copy_the_server_stops_making_is_left_to_the_next_start() {
    let body = "for last; do :; done\necho partial > \"$last\"\nexec sleep 30";
    let s = setup("stopped", body, 1, 1 << 30).await;
    let copy = s
        .pool
        .request(asked(&s.media, "Mobile").await)
        .await
        .unwrap();
    let video = s.first_track(&copy.id).await;
    eventually("the track is being written", || async {
        s.part(&video).exists()
    })
    .await;
    s.pool.stop();
    eventually("its part is removed", || async { !s.part(&video).exists() }).await;
    assert_eq!(s.state(&copy.id).await, Some(CopyState::Making));
}

/// An `ahead` copy of `media`, made as [`MOBILE`].
async fn asked_ahead(media: &Path) -> NewCopy {
    NewCopy {
        target: CopyTarget::Ahead,
        ..asked(media, "Mobile").await
    }
}

/// An `ahead` copy a server with binge-ahead on left waiting.
async fn left_ahead(s: &Setup) -> CopyRow {
    let copy = s
        .pool
        .request_ahead("alice", "show", asked_ahead(&s.media).await)
        .await
        .unwrap();
    s.pool.stop();
    copy
}

/// PX-38: off, no binge-ahead copy is asked for, so one not yet made
/// would only take the place of a download's.
#[tokio::test]
async fn with_binge_ahead_off_an_ahead_copy_left_waiting_is_dropped_at_start() {
    let s = setup("ahead-off", "exit 1", 0, 1 << 30).await;
    let copy = left_ahead(&s).await;
    let _pool = restarted_as(&s, "ahead-off", false).await;
    assert_eq!(s.state(&copy.id).await, None);
}

#[tokio::test]
async fn with_binge_ahead_on_an_ahead_copy_left_waiting_waits_again() {
    let s = setup("ahead-on", "exit 1", 0, 1 << 30).await;
    let copy = left_ahead(&s).await;
    let _pool = restarted_as(&s, "ahead-on", true).await;
    assert_eq!(s.state(&copy.id).await, Some(CopyState::Waiting));
}

/// A stand-in ffmpeg that writes its part and runs on until killed.
const RUNS_ON: &str = "for last; do :; done\necho partial > \"$last\"\nexec sleep 30";

/// The `ahead` copy's video waits behind a profile's, its own, though
/// asked for first.
#[tokio::test]
async fn a_profile_copy_asked_for_after_an_ahead_copy_waits_before_it() {
    let s = setup("ahead-behind", "exit 1", 0, 1 << 30).await;
    let ahead = s
        .pool
        .request_ahead("alice", "show", asked_ahead(&s.media).await)
        .await
        .unwrap();
    let profile = s.pool.request(asked(&s.media, "TV").await).await.unwrap();
    let (ahead_video, profile_video) = (
        s.first_track(&ahead.id).await,
        s.first_track(&profile.id).await,
    );
    let queued = s.pool.queued();
    let at = |id: &str| queued.iter().position(|q| q == id).unwrap();
    assert!(at(&profile_video) < at(&ahead_video), "{queued:?}");
}

/// A track an `ahead` copy and a profile's both want waits with the
/// profile's.
#[tokio::test]
async fn a_track_a_profile_copy_wants_too_leaves_the_ahead_lane() {
    let s = setup("ahead-promoted", "exit 1", 0, 1 << 30).await;
    let ahead = s
        .pool
        .request_ahead("alice", "show", asked_ahead(&s.media).await)
        .await
        .unwrap();
    s.pool
        .request(asked(&s.media, "Mobile").await)
        .await
        .unwrap();
    let video = s.first_track(&ahead.id).await;
    assert_eq!(
        s.pool.inner.state.lock().queue.lane_of(&video),
        Some(Lane::Now)
    );
}

#[tokio::test]
async fn an_ahead_copy_live_playback_preempts_waits_again() {
    let s = setup("ahead-preempted", RUNS_ON, 1, 1 << 30).await;
    let ahead = s
        .pool
        .request_ahead("alice", "show", asked_ahead(&s.media).await)
        .await
        .unwrap();
    let video = s.first_track(&ahead.id).await;
    eventually("the ahead copy's video is being written", || async {
        s.part(&video).exists()
    })
    .await;
    let live = s.jobs.hold(Load::Software, None).unwrap();
    eventually("the ahead copy waits again", || async {
        s.state(&ahead.id).await == Some(CopyState::Waiting)
    })
    .await;
    assert_eq!(
        (s.part(&video).exists(), s.pool.queued().first()),
        (false, Some(&video))
    );
    drop(live);
}

#[tokio::test]
async fn an_ahead_copy_preempted_waits_behind_the_profile_copies() {
    let s = setup("ahead-preempted-behind", RUNS_ON, 1, 1 << 30).await;
    let ahead = s
        .pool
        .request_ahead("alice", "show", asked_ahead(&s.media).await)
        .await
        .unwrap();
    let video = s.first_track(&ahead.id).await;
    eventually("the ahead copy's video is being written", || async {
        s.part(&video).exists()
    })
    .await;
    let profile = s.pool.request(asked(&s.media, "TV").await).await.unwrap();
    let live = s.jobs.hold(Load::Software, None).unwrap();
    eventually("the ahead copy waits again", || async {
        s.state(&ahead.id).await == Some(CopyState::Waiting)
    })
    .await;
    let queued = s.pool.queued();
    let profile_video = s.first_track(&profile.id).await;
    let at = |id: &str| queued.iter().position(|q| q == id).unwrap();
    assert!(at(&profile_video) < at(&video), "{queued:?}");
    drop(live);
}

#[tokio::test]
async fn a_newer_ahead_copy_for_a_viewer_replaces_one_not_started() {
    let s = setup("ahead-replaced", "exit 1", 0, 1 << 30).await;
    let older = s
        .pool
        .request_ahead("alice", "show", asked_ahead(&s.media).await)
        .await
        .unwrap();
    let newer = NewCopy {
        video: CopyVideo {
            height: 720,
            ..MOBILE
        },
        ..asked_ahead(&s.media).await
    };
    s.pool.request_ahead("alice", "show", newer).await.unwrap();
    assert_eq!(s.state(&older.id).await, None);
}

#[tokio::test]
async fn an_ahead_copy_another_viewer_asked_for_too_is_not_replaced() {
    let s = setup("ahead-shared", "exit 1", 0, 1 << 30).await;
    let shared = s
        .pool
        .request_ahead("alice", "show", asked_ahead(&s.media).await)
        .await
        .unwrap();
    s.pool
        .request_ahead("bob", "show", asked_ahead(&s.media).await)
        .await
        .unwrap();
    let newer = NewCopy {
        video: CopyVideo {
            height: 720,
            ..MOBILE
        },
        ..asked_ahead(&s.media).await
    };
    s.pool.request_ahead("alice", "show", newer).await.unwrap();
    assert_eq!(s.state(&shared.id).await, Some(CopyState::Waiting));
}

#[tokio::test]
async fn an_ahead_copy_for_a_show_unplayed_for_a_day_is_dropped() {
    let s = setup("ahead-unplayed", "exit 1", 0, 1 << 30).await;
    let ahead = s
        .pool
        .request_ahead("alice", "show", asked_ahead(&s.media).await)
        .await
        .unwrap();
    s.store.clock().advance(UNPLAYED + Duration::from_secs(60));
    assert_eq!(s.pool.cancel_unplayed().await.unwrap(), 1);
    assert_eq!(
        (s.state(&ahead.id).await, s.pool.queued()),
        (None, Vec::new())
    );
}

#[tokio::test]
async fn an_ahead_copy_being_made_for_a_show_unplayed_for_a_day_is_stopped() {
    let s = setup("ahead-unplayed-making", RUNS_ON, 1, 1 << 30).await;
    let ahead = s
        .pool
        .request_ahead("alice", "show", asked_ahead(&s.media).await)
        .await
        .unwrap();
    let video = s.first_track(&ahead.id).await;
    eventually("the ahead copy's video is being written", || async {
        s.part(&video).exists()
    })
    .await;
    s.store.clock().advance(UNPLAYED + Duration::from_secs(60));
    s.pool.cancel_unplayed().await.unwrap();
    eventually("the copy and its part are gone", || async {
        s.state(&ahead.id).await.is_none() && !s.part(&video).exists()
    })
    .await;
}

#[tokio::test]
async fn an_ahead_copy_for_a_show_played_within_the_day_stays() {
    let s = setup("ahead-played", "exit 1", 0, 1 << 30).await;
    let ahead = s
        .pool
        .request_ahead("alice", "show", asked_ahead(&s.media).await)
        .await
        .unwrap();
    s.store.clock().advance(UNPLAYED - Duration::from_secs(60));
    s.pool.played("alice", "show");
    s.store.clock().advance(Duration::from_secs(120));
    assert_eq!(s.pool.cancel_unplayed().await.unwrap(), 0);
    assert_eq!(s.state(&ahead.id).await, Some(CopyState::Waiting));
}

#[tokio::test]
async fn a_made_ahead_copy_stays_though_its_show_is_unplayed() {
    let s = setup("ahead-made-stays", "exit 1", 0, 1 << 30).await;
    let ahead = s
        .made_of(asked_ahead(&s.media).await, &[Recipe::Video(MOBILE)], 1)
        .await;
    s.store.clock().advance(UNPLAYED + Duration::from_secs(60));
    s.pool.cancel_unplayed().await.unwrap();
    assert_eq!(s.state(&ahead.id).await, Some(CopyState::Ready));
}

mod real;

mod on_demand {
    //! On-demand requests ([`super::requests`]) over the fake catalog:
    //! its film (the test film) and its show's episodes 12 and 13 (season
    //! 11), each file the test film.

    use spoolframe_types::{MediaId, MediaKind, Namespace};

    use super::super::requests::{Asked, RequestError, Scope};
    use super::*;
    use crate::store::optimized::Profile;
    use crate::test_support::{episode_media, movie};

    fn media(kind: MediaKind, key: &str) -> MediaId {
        MediaId::new(Namespace::Plex, kind, key).unwrap()
    }

    fn asked(scope: Scope, profile: &str, pinned: bool) -> Asked {
        Asked {
            scope,
            profile: profile.into(),
            pinned,
            created_by: "admin".into(),
        }
    }

    fn the_film() -> Scope {
        Scope::Item(movie().id().clone())
    }

    impl Setup {
        /// Episodes 12 and 13's files, each the test film.
        fn write_episodes(&self) {
            for key in ["12", "13"] {
                let id = media(MediaKind::Episode, key);
                std::fs::copy(&self.media, episode_media(&self.media, &id)).unwrap();
            }
        }

        /// The copy request `request` made of `media_id`, once the asker
        /// has asked for it and queued its tracks; one other than `not`.
        async fn on_demand_copy(
            &self,
            request: &str,
            media_id: &MediaId,
            not: Option<&str>,
        ) -> CopyRow {
            let until = Instant::now() + Duration::from_secs(60);
            loop {
                let item = self
                    .store
                    .optimized_request_item(request, &media_id.to_string())
                    .await
                    .unwrap();
                if let Some((true, Some(copy))) = item
                    && Some(copy.as_str()) != not
                {
                    return self.store.optimized_copy(&copy).await.unwrap().unwrap();
                }
                assert!(
                    Instant::now() < until,
                    "never: the item's copy is asked for"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        /// Copy `copy`'s tracks made, each `size` bytes, its files there.
        async fn make_ready(&self, copy: &CopyRow, size: u64) {
            for track in self.tracks(&copy.id).await {
                self.store
                    .optimized_track_ready(&track.id, size)
                    .await
                    .unwrap();
                std::fs::write(track_file(&self.dir, &track.id), vec![0; size as usize]).unwrap();
            }
        }
    }

    /// Over budget, the on-demand copy a pinned request wants stays,
    /// though it is the least recently used, and the newer copy goes.
    #[tokio::test]
    async fn over_budget_a_pinned_copy_stays_and_an_unpinned_one_goes() {
        let s = setup("on-demand-pinned", "exit 1", 0, 250).await;
        let request = s
            .pool
            .put_request(asked(the_film(), "Mobile", false))
            .await
            .unwrap();
        let pinned = s.on_demand_copy(&request.row.id, movie().id(), None).await;
        // Pinned past the check (these tests' budget is bytes, not
        // films), before it is made: the pool's own sweep runs whenever
        // it is woken.
        s.store
            .set_optimized_request_pinned(&request.row.id, true)
            .await
            .unwrap();
        s.make_ready(&pinned, 100).await;
        s.store.clock().advance(Duration::from_secs(60));
        let newer = s.made("TV", 100).await;
        let (pinned_video, newer_video) = (
            s.first_track(&pinned.id).await,
            s.first_track(&newer.id).await,
        );
        s.pool.sweep().await.unwrap();
        assert_eq!((s.kept(&pinned_video), s.kept(&newer_video)), (true, false));
    }

    /// Deleting the request unpins its copy: over budget, it goes as any
    /// other.
    #[tokio::test]
    async fn a_copy_whose_pinned_request_is_deleted_is_evicted_again() {
        let s = setup("on-demand-unpinned", "exit 1", 0, 150).await;
        let request = s
            .pool
            .put_request(asked(the_film(), "Mobile", false))
            .await
            .unwrap();
        let copy = s.on_demand_copy(&request.row.id, movie().id(), None).await;
        s.store
            .set_optimized_request_pinned(&request.row.id, true)
            .await
            .unwrap();
        s.make_ready(&copy, 100).await;
        s.pool.sweep().await.unwrap();
        assert_eq!(
            s.state(&copy.id).await,
            Some(CopyState::Ready),
            "kept while pinned"
        );
        assert!(s.pool.delete_request(&request.row.id).await.unwrap());
        s.pool.sweep().await.unwrap();
        // Its two tracks are 200 bytes: one goes, and the copy with it.
        assert_eq!(s.state(&copy.id).await, None);
    }

    /// The film in `Mobile` is about 3.9 GB at its bitrate over the
    /// catalog's 117 minutes: a 1 GiB pool cannot hold it pinned.
    #[tokio::test]
    async fn a_pinned_request_the_budget_cannot_hold_is_refused() {
        let s = setup("on-demand-over-budget", "exit 1", 0, 1 << 30).await;
        let refused = s
            .pool
            .put_request(asked(the_film(), "Mobile", true))
            .await
            .unwrap_err();
        assert!(
            matches!(refused, RequestError::OverBudget { .. })
                && refused.to_string().contains("optimized.max_size_mb")
                && s.store.optimized_requests().await.unwrap().is_empty(),
            "{refused}"
        );
    }

    /// Unpinned, the same request is taken: it holds no place.
    #[tokio::test]
    async fn an_unpinned_request_the_budget_cannot_hold_is_taken() {
        let s = setup("on-demand-unpinned-big", "exit 1", 0, 1 << 30).await;
        let request = s
            .pool
            .put_request(asked(the_film(), "Mobile", false))
            .await
            .unwrap();
        assert_eq!(request.items, 1);
    }

    /// Pinning a request later is refused the same way.
    #[tokio::test]
    async fn pinning_a_request_the_budget_cannot_hold_is_refused() {
        let s = setup("on-demand-pin-later", "exit 1", 0, 1 << 30).await;
        let request = s
            .pool
            .put_request(asked(the_film(), "Mobile", false))
            .await
            .unwrap();
        let refused = s.pool.pin_request(&request.row.id, true).await;
        assert!(
            matches!(refused, Err(RequestError::OverBudget { .. })),
            "{refused:?}"
        );
    }

    /// After its profile's video changed, the film is asked for again: a
    /// new copy whose video track is made with the new bitrate.
    #[tokio::test]
    async fn a_profile_edit_asks_for_the_request_s_copy_again_with_the_new_video() {
        let s = setup("on-demand-remake", "exit 1", 0, 1 << 30).await;
        let request = s
            .pool
            .put_request(asked(the_film(), "Mobile", false))
            .await
            .unwrap();
        let old = s.on_demand_copy(&request.row.id, movie().id(), None).await;
        s.make_ready(&old, 100).await;
        let written = s
            .store
            .put_optimized_profile(&Profile {
                name: "Mobile".into(),
                video_codec: VideoCodec::H264,
                height: 1080,
                max_bitrate: 6_000_000,
            })
            .await
            .unwrap();
        assert!(written.changed);
        assert_eq!(s.pool.remake("Mobile").await.unwrap(), 1);
        let new = s
            .on_demand_copy(&request.row.id, movie().id(), Some(&old.id))
            .await;
        let video = s.tracks(&new.id).await[0].clone();
        assert_eq!(
            (
                new.id != old.id,
                video.recipe.key(),
                s.pool.queued().contains(&video.id)
            ),
            (true, "video:h264:90:6000000:same".to_owned(), true)
        );
    }

    /// A season is a kept rule: the episode its season gains after the
    /// request is asked for too.
    #[tokio::test]
    async fn a_season_rule_asks_for_an_episode_the_season_gains_later() {
        let s = setup("on-demand-follow", "exit 1", 0, 1 << 30).await;
        s.write_episodes();
        let later = media(MediaKind::Episode, "13");
        s.catalog.not_yet(&later);
        let request = s
            .pool
            .put_request(asked(
                Scope::Season(media(MediaKind::Season, "11")),
                "Mobile",
                false,
            ))
            .await
            .unwrap();
        assert_eq!(request.items, 1);
        s.catalog.gain(&later);
        assert_eq!(s.pool.follow_requests().await.unwrap(), 1);
        let copy = s.on_demand_copy(&request.row.id, &later, None).await;
        assert_eq!(copy.state, CopyState::Waiting);
    }

    /// An on-demand track waits behind a download's, though asked for
    /// first.
    #[tokio::test]
    async fn a_download_s_track_waits_before_an_on_demand_one() {
        let s = setup("on-demand-lane", "exit 1", 0, 1 << 30).await;
        let request = s
            .pool
            .put_request(asked(the_film(), "Mobile", false))
            .await
            .unwrap();
        let on_demand = s.on_demand_copy(&request.row.id, movie().id(), None).await;
        let last = s.first_track(&on_demand.id).await;
        let download = s
            .pool
            .request(super::asked(&s.media, "TV").await)
            .await
            .unwrap();
        let first = s.first_track(&download.id).await;
        let queued = s.pool.queued();
        let at = |id: &str| queued.iter().position(|q| q == id);
        assert!(at(&first) < at(&last), "{queued:?}");
    }

    #[tokio::test]
    async fn a_request_for_a_music_library_is_refused() {
        let s = setup("on-demand-music", "exit 1", 0, 1 << 30).await;
        s.catalog.serve_music();
        let refused = s
            .pool
            .put_request(asked(
                Scope::Library(crate::test_support::music_library().id),
                "Mobile",
                false,
            ))
            .await;
        assert!(
            matches!(refused, Err(RequestError::Scope(_))),
            "{refused:?}"
        );
    }
}

mod binge {
    //! The binge-ahead watcher ([`super::ahead::Watcher`]) over the fake
    //! catalog's show: episodes 12 (S1E1), 13 (S1E2) and 15 (S2E1), each
    //! file the test film.

    use spoolframe_protocol::{AudioAction, ConvertedAudio, ConvertedVideo, PlaybackPlan};
    use spoolframe_types::{AudioCodec, Container, Delivery, Item};

    use super::super::ahead::{Played, Seen, Watcher};
    use super::*;
    use crate::test_support::{FakeCatalog, episode, episode_media};

    /// A plan that encodes the video as `video` says.
    fn encoding(video: ConvertedVideo) -> PlaybackPlan {
        PlaybackPlan::Converted {
            delivery: Delivery::Progressive,
            container: Container::Matroska,
            video: Some(video),
            audio: None,
            subtitle: None,
            ladder: Vec::new(),
            reasons: Vec::new(),
        }
    }

    const ENCODE: ConvertedVideo = ConvertedVideo::Encode {
        codec: VideoCodec::H264,
        height: 720,
        max_bitrate: Some(3_000_000),
        tone_map: false,
    };

    /// A watcher of episode `key` (season `season`), for alice, with each
    /// episode's file in place: `None` when it has no next episode.
    async fn watcher(
        s: &Setup,
        key: &str,
        season_key: &str,
        season: u32,
        number: u32,
    ) -> Option<Watcher> {
        let catalog = Arc::new(FakeCatalog::new(s.media.clone()));
        let Item::Episode(watched) = episode(key, season_key, season, number) else {
            unreachable!()
        };
        for key in ["12", "13", "15"] {
            let Item::Episode(e) = episode(key, "11", 1, 1) else {
                unreachable!()
            };
            std::fs::copy(&s.media, episode_media(&s.media, &e.id)).unwrap();
        }
        Watcher::new(catalog, "alice".into(), &Played::from(&watched)).await
    }

    fn seen(plan: PlaybackPlan, caught_up: bool) -> Seen {
        Seen {
            plan,
            caught_up,
            copy: None,
            signal: None,
        }
    }

    /// A playback of a copy in the film's place: a remux of it.
    fn from_copy(copy: &CopyRow) -> Seen {
        Seen {
            plan: encoding(ConvertedVideo::Copy {
                codec: VideoCodec::H264,
                dolby_vision: false,
            }),
            caught_up: false,
            copy: Some(copy.id.clone()),
            signal: None,
        }
    }

    const ON_COPY: CopyVideo = CopyVideo {
        codec: VideoCodec::Hevc,
        height: 540,
        max_bitrate: Some(1_500_000),
        tone_map: true,
        copied: false,
    };

    #[tokio::test]
    async fn a_playback_from_an_ahead_copy_asks_for_the_next_with_that_copy_s_video() {
        let s = setup("binge-chain", "exit 1", 0, 1 << 30).await;
        let played = s
            .pool
            .request(NewCopy {
                video: ON_COPY,
                ..asked_ahead(&s.media).await
            })
            .await
            .unwrap();
        let mut w = watcher(&s, "13", "11", 1, 2).await.unwrap();
        let row = w.look(&s.pool, from_copy(&played)).await.unwrap();
        assert_eq!(
            (row.media_id.as_str(), row.target, row.video),
            ("plex:episode:15", CopyTarget::Ahead, ON_COPY)
        );
    }

    #[tokio::test]
    async fn a_playback_from_a_profile_copy_asks_for_nothing() {
        let s = setup("binge-profile-copy", "exit 1", 0, 1 << 30).await;
        let played = s.made("Mobile", 100).await;
        let mut w = watcher(&s, "12", "11", 1, 1).await.unwrap();
        w.look(&s.pool, from_copy(&played)).await;
        assert_eq!(s.pool.queued(), Vec::<String>::new());
    }

    #[tokio::test]
    async fn an_encode_not_caught_up_asks_for_no_copy() {
        let s = setup("binge-not-yet", "exit 1", 0, 1 << 30).await;
        let mut w = watcher(&s, "12", "11", 1, 1).await.unwrap();
        w.look(&s.pool, seen(encoding(ENCODE), false)).await;
        assert!(s.pool.queued().is_empty());
    }

    #[tokio::test]
    async fn a_caught_up_encode_asks_for_the_next_episode_s_copy_with_its_plan() {
        let s = setup("binge-asks", "exit 1", 0, 1 << 30).await;
        let mut w = watcher(&s, "12", "11", 1, 1).await.unwrap();
        w.look(&s.pool, seen(encoding(ENCODE), false)).await;
        let row = w.look(&s.pool, seen(encoding(ENCODE), true)).await.unwrap();
        assert_eq!(
            (row.media_id.as_str(), row.target, row.video),
            (
                "plex:episode:13",
                CopyTarget::Ahead,
                CopyVideo {
                    codec: VideoCodec::H264,
                    height: 720,
                    max_bitrate: Some(3_000_000),
                    tone_map: false,
                    copied: false,
                }
            )
        );
    }

    #[tokio::test]
    async fn the_last_episode_of_a_season_asks_for_the_next_season_s_first() {
        let s = setup("binge-season", "exit 1", 0, 1 << 30).await;
        let mut w = watcher(&s, "13", "11", 1, 2).await.unwrap();
        let row = w.look(&s.pool, seen(encoding(ENCODE), true)).await.unwrap();
        assert_eq!(row.media_id, "plex:episode:15");
    }

    #[tokio::test]
    async fn the_show_s_last_episode_asks_for_nothing() {
        let s = setup("binge-last", "exit 1", 0, 1 << 30).await;
        assert!(watcher(&s, "15", "14", 2, 1).await.is_none());
    }

    #[tokio::test]
    async fn a_remux_asks_for_no_copy() {
        let s = setup("binge-remux", "exit 1", 0, 1 << 30).await;
        let mut w = watcher(&s, "12", "11", 1, 1).await.unwrap();
        let remux = encoding(ConvertedVideo::Copy {
            codec: VideoCodec::H264,
            dolby_vision: false,
        });
        w.look(&s.pool, seen(remux, true)).await;
        assert!(s.pool.queued().is_empty());
    }

    #[tokio::test]
    async fn an_audio_only_conversion_asks_for_no_copy() {
        let s = setup("binge-audio", "exit 1", 0, 1 << 30).await;
        let mut w = watcher(&s, "12", "11", 1, 1).await.unwrap();
        let audio = PlaybackPlan::Converted {
            delivery: Delivery::Progressive,
            container: Container::Matroska,
            video: Some(ConvertedVideo::Copy {
                codec: VideoCodec::H264,
                dolby_vision: false,
            }),
            audio: Some(ConvertedAudio {
                index: 0,
                action: AudioAction::Encode {
                    codec: AudioCodec::Aac,
                    channels: 2,
                    bitrate: 192_000,
                },
            }),
            subtitle: None,
            ladder: Vec::new(),
            reasons: Vec::new(),
        };
        w.look(&s.pool, seen(audio, true)).await;
        assert!(s.pool.queued().is_empty());
    }

    #[tokio::test]
    async fn a_playback_asks_for_the_next_copy_once() {
        let s = setup("binge-once", "exit 1", 0, 1 << 30).await;
        let mut w = watcher(&s, "12", "11", 1, 1).await.unwrap();
        w.look(&s.pool, seen(encoding(ENCODE), true)).await;
        let again = w.look(&s.pool, seen(encoding(ENCODE), true)).await;
        assert!(again.is_none());
    }

    #[tokio::test]
    async fn watching_keeps_the_ahead_copy_wanted_past_a_day() {
        let s = setup("binge-keeps", "exit 1", 0, 1 << 30).await;
        let mut w = watcher(&s, "12", "11", 1, 1).await.unwrap();
        let row = w.look(&s.pool, seen(encoding(ENCODE), true)).await.unwrap();
        s.store.clock().advance(UNPLAYED - Duration::from_secs(60));
        w.look(&s.pool, seen(encoding(ENCODE), true)).await;
        s.store.clock().advance(Duration::from_secs(120));
        s.pool.cancel_unplayed().await.unwrap();
        assert_eq!(s.state(&row.id).await, Some(CopyState::Waiting));
    }
}
