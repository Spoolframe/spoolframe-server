//! The copy pool's tests (PX-37): a stand-in ffmpeg where only what the
//! pool does with it matters, and real ffmpeg and ffprobe on a film made
//! at test time for what a copy holds.

use std::future::Future;
use std::process::Command;

use spoolframe_types::VideoCodec;

use super::*;
use crate::encode::JobLimits;
use crate::encode::job::{Load, Units, tests::ffmpeg};
use crate::store::optimized::CopyTarget;
use crate::store::tests::{TestStore, test_store};

/// What a test works with: the pool, and what it stands on.
struct Setup {
    pool: Pool,
    jobs: Arc<Jobs>,
    store: TestStore,
    dir: PathBuf,
    media: PathBuf,
    _tmp: spoolframe_test_dir::TestDir,
}

/// 2 s of H.264 with an AC-3 track, an MP2 track and a SubRip subtitle,
/// in Matroska.
fn film(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let srt = dir.join("film.srt");
    std::fs::write(&srt, "1\n00:00:00,500 --> 00:00:01,500\nHello\n").unwrap();
    let path = dir.join("film.mkv");
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc2=size=160x90:rate=25"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
        .args(["-f", "lavfi", "-i", "sine=frequency=880:sample_rate=48000"])
        .arg("-i")
        .arg(&srt)
        .args([
            "-t", "2", "-map", "0", "-map", "1", "-map", "2", "-map", "3",
        ])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
        .args(["-c:a:0", "ac3", "-c:a:1", "mp2", "-c:s", "srt"])
        .arg(&path)
        .output()
        .expect("ffmpeg runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
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
    let pool = Pool::start_with(
        PoolSources {
            store: store.store.clone(),
            jobs: jobs.clone(),
            transcoding,
            prober,
        },
        &OptimizedSettings {
            dir: dir.clone(),
            max_size_bytes: max_size,
        },
        // Nothing frees a place on its own in these tests.
        Duration::from_secs(600),
    )
    .await
    .unwrap();
    Setup {
        pool,
        jobs,
        store,
        dir,
        media: film(&root.join("media")),
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
};

/// A copy of `media` in profile `profile`.
async fn asked(media: &Path, profile: &str) -> NewCopy {
    NewCopy {
        media_id: "plex:movie:1".into(),
        media_path: media.display().to_string(),
        source_stamp: crate::files::stamp_text(media).await.unwrap(),
        target: CopyTarget::Profile(profile.into()),
        video: MOBILE,
    }
}

/// Until `check` holds, for at most 30 s.
async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let until = Instant::now() + Duration::from_secs(30);
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

    fn part(&self, id: &str) -> PathBuf {
        self.dir.join(MAKING_DIR).join(format!("{id}.mkv.part"))
    }

    /// A copy made already, `size` bytes, its file there; in profile
    /// `profile`, used at the store's clock now.
    async fn made(&self, profile: &str, size: u64) -> CopyRow {
        let (row, _) = self
            .store
            .request_optimized_copy(&asked(&self.media, profile).await)
            .await
            .unwrap();
        self.store
            .optimized_copy_ready(&row.id, size)
            .await
            .unwrap();
        std::fs::write(copy_file(&self.dir, &row.id), vec![0; size as usize]).unwrap();
        row
    }
}

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
    assert_eq!(ran, 1);
}

#[tokio::test]
async fn a_made_copy_is_renamed_from_its_part() {
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
    assert_eq!(
        std::fs::read_to_string(copy_file(&s.dir, &copy.id)).unwrap(),
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
async fn a_copy_live_playback_preempts_waits_again_at_the_head_without_its_part() {
    let body = "for last; do :; done\necho partial > \"$last\"\nexec sleep 30";
    let s = setup("preempted", body, 1, 1 << 30).await;
    let first = s
        .pool
        .request(asked(&s.media, "Mobile").await)
        .await
        .unwrap();
    let second = s.pool.request(asked(&s.media, "TV").await).await.unwrap();
    eventually("the first copy is being written", || async {
        s.part(&first.id).exists()
    })
    .await;
    let live = s.jobs.hold(Load::Software, None).unwrap();
    eventually("the first copy waits again", || async {
        s.state(&first.id).await == Some(CopyState::Waiting)
    })
    .await;
    assert_eq!(
        (s.part(&first.id).exists(), s.pool.queued()),
        (false, vec![first.id.clone(), second.id.clone()])
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
    s.pool.sweep().await.unwrap();
    assert_eq!(
        (
            copy_file(&s.dir, &older.id).exists(),
            copy_file(&s.dir, &newer.id).exists()
        ),
        (true, false)
    );
    drop(lease);
}

#[tokio::test]
async fn over_budget_the_least_recently_used_copy_goes() {
    let s = setup("lru", "exit 1", 1, 150).await;
    let older = s.made("Mobile", 100).await;
    s.store.clock().advance(Duration::from_secs(60));
    let newer = s.made("TV", 100).await;
    s.pool.sweep().await.unwrap();
    assert_eq!(
        (
            copy_file(&s.dir, &older.id).exists(),
            copy_file(&s.dir, &newer.id).exists()
        ),
        (false, true)
    );
}

#[tokio::test]
async fn over_budget_an_outdated_copy_goes_before_the_least_recently_used() {
    let s = setup("outdated-first", "exit 1", 1, 150).await;
    let older = s.made("Mobile", 100).await;
    s.store.clock().advance(Duration::from_secs(60));
    let outdated = s.made("TV", 100).await;
    s.store.delete_optimized_profile("TV").await.unwrap();
    s.pool.sweep().await.unwrap();
    assert_eq!(
        (
            copy_file(&s.dir, &older.id).exists(),
            copy_file(&s.dir, &outdated.id).exists()
        ),
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
    let lease = s.pool.lease(&copy.id).await.unwrap().unwrap();
    assert_eq!(s.pool.delete(&copy.id).await.unwrap(), Deleted::Deferred);
    assert!(lease.path().exists(), "kept while read");
    drop(lease);
    eventually("the copy is gone", || async {
        s.state(&copy.id).await.is_none()
    })
    .await;
}

#[tokio::test]
async fn a_copy_deleted_unread_goes_at_once() {
    let s = setup("delete-now", "exit 1", 1, 1 << 30).await;
    let copy = s.made("Mobile", 100).await;
    assert_eq!(s.pool.delete(&copy.id).await.unwrap(), Deleted::Now);
    assert!(!copy_file(&s.dir, &copy.id).exists());
}

/// A pool restarted over `s`'s directory and store, with no place in
/// the budget: what its start did stays as it left it.
async fn restarted(s: &Setup, test: &str) -> Pool {
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
        },
        &OptimizedSettings {
            dir: s.dir.clone(),
            max_size_bytes: 1 << 30,
        },
        Duration::from_secs(600),
    )
    .await
    .unwrap()
}

/// A copy a stopped server was making: its row `making` and a part.
async fn left_making(s: &Setup) -> CopyRow {
    s.pool.stop();
    let (row, _) = s
        .store
        .request_optimized_copy(&asked(&s.media, "Mobile").await)
        .await
        .unwrap();
    s.store.optimized_copy_making(&row.id).await.unwrap();
    std::fs::write(s.part(&row.id), "half").unwrap();
    row
}

#[tokio::test]
async fn a_copy_a_stopped_server_was_making_waits_again_at_start() {
    let s = setup("restart-requeue", "exit 1", 0, 1 << 30).await;
    let copy = left_making(&s).await;
    let pool = restarted(&s, "restart-requeue").await;
    assert_eq!(
        (s.state(&copy.id).await, pool.queued()),
        (Some(CopyState::Waiting), vec![copy.id.clone()])
    );
}

#[tokio::test]
async fn a_part_a_stopped_server_left_is_removed_at_start() {
    let s = setup("restart-part", "exit 1", 0, 1 << 30).await;
    let copy = left_making(&s).await;
    let _pool = restarted(&s, "restart-part").await;
    assert!(!s.part(&copy.id).exists());
}

/// Real ffmpeg's copy of the test film in `Mobile`, made: the source, and
/// the copy's file.
async fn real_copy(test: &str) -> (Setup, PathBuf) {
    let s = setup_with(test, PathBuf::from("ffmpeg"), 1, 1 << 30).await;
    let copy = s
        .pool
        .request(asked(&s.media, "Mobile").await)
        .await
        .unwrap();
    eventually("the copy is made or failed", || async {
        matches!(
            s.state(&copy.id).await,
            Some(CopyState::Ready | CopyState::Failed)
        )
    })
    .await;
    let row = s.store.optimized_copy(&copy.id).await.unwrap().unwrap();
    assert_eq!(row.state, CopyState::Ready, "{:?}", row.failure);
    let file = copy_file(&s.dir, &copy.id);
    (s, file)
}

/// The MD5 of stream `stream` of `file`'s packets, as they are.
fn packets_md5(file: &Path, stream: &str) -> String {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-loglevel", "error", "-i"])
        .arg(file)
        .args(["-map", stream, "-c", "copy", "-f", "md5", "-"])
        .output()
        .expect("ffmpeg runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Each of `file`'s streams' codec, in order.
fn codecs(file: &Path) -> Vec<String> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "csv=p=0",
        ])
        .arg(file)
        .output()
        .expect("ffprobe runs");
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn a_copied_audio_track_is_bit_identical() {
    let (s, copy) = real_copy("bit-identical").await;
    assert_eq!(packets_md5(&copy, "0:a:0"), packets_md5(&s.media, "0:a:0"));
}

#[tokio::test]
async fn a_copy_holds_every_track_of_its_file() {
    let (_s, copy) = real_copy("every-track").await;
    assert_eq!(codecs(&copy), ["h264", "ac3", "aac", "subrip"]);
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
    eventually("the copy is being written", || async {
        s.part(&copy.id).exists()
    })
    .await;
    s.pool.stop();
    eventually("its part is removed", || async {
        !s.part(&copy.id).exists()
    })
    .await;
    assert_eq!(s.state(&copy.id).await, Some(CopyState::Making));
}

/// An `ahead` copy of `media`, made as [`MOBILE`].
async fn asked_ahead(media: &Path) -> NewCopy {
    NewCopy {
        target: CopyTarget::Ahead,
        ..asked(media, "Mobile").await
    }
}

/// A stand-in ffmpeg that writes its part and runs on until killed.
const RUNS_ON: &str = "for last; do :; done\necho partial > \"$last\"\nexec sleep 30";

#[tokio::test]
async fn a_profile_copy_asked_for_after_an_ahead_copy_waits_before_it() {
    let s = setup("ahead-behind", "exit 1", 0, 1 << 30).await;
    let ahead = s
        .pool
        .request_ahead("alice", "show", asked_ahead(&s.media).await)
        .await
        .unwrap();
    let profile = s
        .pool
        .request(asked(&s.media, "Mobile").await)
        .await
        .unwrap();
    assert_eq!(s.pool.queued(), [profile.id, ahead.id]);
}

#[tokio::test]
async fn a_profile_copy_asked_for_after_an_ahead_copy_is_made_first() {
    let runs =
        std::env::temp_dir().join(format!("spoolframe-runs-{}", uuid::Uuid::new_v4().simple()));
    let body = format!(
        "for last; do :; done\necho \"$last\" >> {}\necho copy > \"$last\"",
        runs.display()
    );
    let s = setup("ahead-made-after", &body, 1, 1 << 30).await;
    // Live playback holds the budget while both are asked for.
    let live = s.jobs.hold(Load::Software, None).unwrap();
    let ahead = s
        .pool
        .request_ahead("alice", "show", asked_ahead(&s.media).await)
        .await
        .unwrap();
    let profile = s
        .pool
        .request(asked(&s.media, "Mobile").await)
        .await
        .unwrap();
    drop(live);
    s.pool.inner.wake.notify_one();
    eventually("both copies are made", || async {
        s.state(&ahead.id).await == Some(CopyState::Ready)
            && s.state(&profile.id).await == Some(CopyState::Ready)
    })
    .await;
    let ran = std::fs::read_to_string(&runs).unwrap();
    let _ = std::fs::remove_file(&runs);
    let order: Vec<bool> = ran.lines().map(|l| l.contains(&profile.id)).collect();
    assert_eq!(order, [true, false]);
}

#[tokio::test]
async fn an_ahead_copy_live_playback_preempts_waits_again() {
    let s = setup("ahead-preempted", RUNS_ON, 1, 1 << 30).await;
    let ahead = s
        .pool
        .request_ahead("alice", "show", asked_ahead(&s.media).await)
        .await
        .unwrap();
    eventually("the ahead copy is being written", || async {
        s.part(&ahead.id).exists()
    })
    .await;
    let live = s.jobs.hold(Load::Software, None).unwrap();
    eventually("the ahead copy waits again", || async {
        s.state(&ahead.id).await == Some(CopyState::Waiting)
    })
    .await;
    assert_eq!(
        (s.part(&ahead.id).exists(), s.pool.queued()),
        (false, vec![ahead.id.clone()])
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
    eventually("the ahead copy is being written", || async {
        s.part(&ahead.id).exists()
    })
    .await;
    let profile = s
        .pool
        .request(asked(&s.media, "Mobile").await)
        .await
        .unwrap();
    let live = s.jobs.hold(Load::Software, None).unwrap();
    eventually("the ahead copy waits again", || async {
        s.state(&ahead.id).await == Some(CopyState::Waiting)
    })
    .await;
    assert_eq!(s.pool.queued(), [profile.id.clone(), ahead.id.clone()]);
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
    eventually("the ahead copy is being written", || async {
        s.part(&ahead.id).exists()
    })
    .await;
    s.store.clock().advance(UNPLAYED + Duration::from_secs(60));
    s.pool.cancel_unplayed().await.unwrap();
    eventually("the copy and its part are gone", || async {
        s.state(&ahead.id).await.is_none() && !s.part(&ahead.id).exists()
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
        .pool
        .request_ahead("alice", "show", asked_ahead(&s.media).await)
        .await
        .unwrap();
    s.store.optimized_copy_ready(&ahead.id, 1).await.unwrap();
    std::fs::write(copy_file(&s.dir, &ahead.id), "x").unwrap();
    s.store.clock().advance(UNPLAYED + Duration::from_secs(60));
    s.pool.cancel_unplayed().await.unwrap();
    assert_eq!(s.state(&ahead.id).await, Some(CopyState::Ready));
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
