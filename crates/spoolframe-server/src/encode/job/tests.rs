//! The jobs' tests (P2-03).

use std::sync::atomic::Ordering;

use super::process::out_time_us;
use super::supervisor::*;
use super::*;

/// PX-37: the encoded time is read from `out_time_us` alone; ffmpeg's
/// `N/A` before the first frame, and any other key, say nothing.
#[test]
fn the_encoded_time_is_read_from_out_time_us() {
    assert_eq!(out_time_us("out_time_us=1500000"), Some(1_500_000));
    assert_eq!(out_time_us("out_time_us=42 "), Some(42));
    assert_eq!(out_time_us("out_time_us=N/A"), None);
    assert_eq!(out_time_us("out_time_ms=1500000"), None);
    assert_eq!(out_time_us("progress=continue"), None);
}

/// PX-37: a hold takes a place as a start does, keeps it until dropped,
/// and a hold refused is not counted as a refused start.
#[tokio::test]
async fn a_hold_takes_a_place_until_dropped() {
    let (jobs, _dir) = jobs("hold", "exit 0", 1, Duration::from_secs(60)).await;
    let held = jobs.hold(Load::Software, None).unwrap();
    assert_eq!(held.load(), Load::Software);
    assert!(matches!(
        jobs.hold(Load::Software, None),
        Err(JobError::Busy)
    ));
    assert_eq!(jobs.counts().refused, 0);
    assert_eq!(jobs.budget().used, Units::jobs(1));
    drop(held);
    assert_eq!(jobs.budget().used, Units::quarters(0));
    jobs.hold(Load::Software, None).unwrap();
}

/// PX-37: a decode failure remembered is said once, and the film
/// decodes in software from then on.
#[tokio::test]
async fn a_remembered_decode_failure_holds_for_the_film() {
    let (jobs, _dir) = jobs("remembered", "exit 0", 1, Duration::from_secs(60)).await;
    let film = Path::new("/films/a.mkv");
    assert!(!jobs.hardware_decode_failed(film));
    assert!(jobs.remember_decode_failure(film));
    assert!(!jobs.remember_decode_failure(film), "said once");
    assert!(jobs.hardware_decode_failed(film));
    assert!(!jobs.hardware_decode_failed(Path::new("/films/b.mkv")));
}

/// Whether `hold` has yielded its place: within a second for one that
/// has, and a short wait for one that has not.
async fn has_yielded(hold: &Yielding, wait: Duration) -> bool {
    tokio::time::timeout(wait, hold.yielded()).await.is_ok()
}

/// PX-37: live admission with the budget full takes the place of the
/// newest yielding hold, which hears so; an older one keeps its place.
/// Both places count until the newest hold is dropped.
#[tokio::test]
async fn live_playback_takes_the_newest_yielding_hold_s_place() {
    let (jobs, _dir) = jobs("yield-newest", "exit 0", 2, Duration::from_secs(60)).await;
    let older = jobs.hold_yielding(Load::Software, None).unwrap();
    let newer = jobs.hold_yielding(Load::Software, None).unwrap();
    let live = jobs.hold(Load::Software, None).unwrap();
    assert!(has_yielded(&newer, Duration::from_secs(1)).await);
    assert!(!has_yielded(&older, Duration::from_millis(50)).await);
    assert_eq!(jobs.counts().yielded, 1);
    assert_eq!(jobs.budget().used, Units::jobs(3), "until the copy lets go");
    drop(newer);
    assert_eq!(jobs.budget().used, Units::jobs(2));
    // The newer one taken, the older one is the next to go.
    let again = jobs.hold(Load::Software, None).unwrap();
    assert!(has_yielded(&older, Duration::from_secs(1)).await);
    drop((live, again));
}

/// PX-37: a job's start is live playback too: it runs in a yielding
/// hold's place rather than being refused.
#[tokio::test]
async fn a_job_s_start_takes_a_yielding_hold_s_place() {
    let (jobs, _dir) = jobs("yield-start", "exec sleep 30", 1, Duration::from_secs(60)).await;
    let copy = jobs.hold_yielding(Load::Software, None).unwrap();
    let scratch = jobs.dir().to_path_buf();
    let job = jobs
        .start(
            scratch.join("a/0"),
            Vec::new(),
            Written::Hls,
            Load::Software,
            0,
            0,
            0,
            None,
        )
        .unwrap();
    assert!(has_yielded(&copy, Duration::from_secs(1)).await);
    assert_eq!(jobs.counts().refused, 0);
    job.stop();
}

/// PX-37: a yielding hold takes only a free place: never a running
/// one's, nor another yielding hold's.
#[tokio::test]
async fn a_yielding_hold_never_takes_another_s_place() {
    let (jobs, _dir) = jobs("yield-never", "exit 0", 2, Duration::from_secs(60)).await;
    let first = jobs.hold_yielding(Load::Software, None).unwrap();
    let live = jobs.hold(Load::Software, None).unwrap();
    assert!(matches!(
        jobs.hold_yielding(Load::Software, None),
        Err(JobError::Busy)
    ));
    assert!(!has_yielded(&first, Duration::from_millis(50)).await);
    assert_eq!(jobs.counts().yielded, 0);
    drop(live);
    let second = jobs.hold_yielding(Load::Software, None).unwrap();
    assert!(matches!(
        jobs.hold_yielding(Load::Software, None),
        Err(JobError::Busy)
    ));
    assert!(!has_yielded(&first, Duration::from_millis(50)).await);
    assert!(!has_yielded(&second, Duration::from_millis(50)).await);
}

/// PX-37: with no yielding hold, a full budget refuses live admission as
/// before; a dropped hold is none to take, its place simply free again.
#[tokio::test]
async fn a_dropped_yielding_hold_is_not_taken_and_a_full_budget_is_busy() {
    let (jobs, _dir) = jobs("yield-dropped", "exit 0", 1, Duration::from_secs(60)).await;
    let copy = jobs.hold_yielding(Load::Software, None).unwrap();
    drop(copy);
    assert_eq!(jobs.budget().used, Units::quarters(0));
    let live = jobs.hold(Load::Software, None).unwrap();
    assert!(matches!(
        jobs.hold(Load::Software, None),
        Err(JobError::Busy)
    ));
    assert_eq!(jobs.counts().yielded, 0);
    assert_eq!(jobs.budget().used, Units::jobs(1));
    drop(live);
}

#[tokio::test]
async fn a_note_is_new_once() {
    let (jobs, _dir) = jobs("notes", "exit 0", 1, Duration::from_secs(60)).await;
    assert!(jobs.first_note("the GPU's driver does not tone map"));
    assert!(!jobs.first_note("the GPU's driver does not tone map"));
    assert!(jobs.first_note("text subtitles are burned in on the CPU"));
}

#[test]
fn a_segment_is_finished_once_ffmpeg_lists_it() {
    let playlist = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-MAP:URI=\"init.mp4\"\n\
                    #EXTINF:6.000000,\nseg4.m4s\n#EXTINF:6.000000,\nseg5.m4s\n";
    assert_eq!(last_of(&listed_segments(playlist)), Some(5));
    assert_eq!(
        last_of(&listed_segments("#EXTM3U\n#EXT-X-VERSION:7\n")),
        None
    );
}

/// PX-24: the measured bitrate is the finished segments' bytes over
/// their listed durations, each counted once as the playlist lists it,
/// though a run from 0 lists only its last.
#[tokio::test]
async fn the_measured_bitrate_is_segment_bytes_over_their_durations() {
    let dir = scratch("measured").await;
    let dir = dir.as_ref();
    // Segment 1: 2 s, 500 kB; 2: 2 s, 500 kB; 3: 4 s, 1 MB.
    for (n, bytes) in [(1, 500_000), (2, 500_000), (3, 1_000_000)] {
        std::fs::write(dir.join(format!("seg{n}.m4s")), vec![0u8; bytes]).unwrap();
    }
    let listing = |n: u32, secs: &str| {
        format!(
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-MAP:URI=\"init.mp4\"\n\
             #EXTINF:{secs},\nseg{n}.m4s\n"
        )
    };
    let mut huge = Measured::default();
    huge.count_segments(dir, &listed_segments(&listing(1, "1e300")));
    huge.count_segments(dir, &listed_segments(&listing(2, "1e300")));
    assert_eq!(huge.ms, u64::MAX, "a huge duration saturates");
    let mut output = Measured::default();
    assert_eq!(output.bitrate(), None, "nothing written yet");
    output.count_segments(dir, &listed_segments(&listing(1, "2.000000")));
    output.count_segments(dir, &listed_segments(&listing(1, "2.000000")));
    assert_eq!((output.bytes, output.ms), (500_000, 2000), "counted once");
    assert_eq!(output.bitrate(), Some(2_000_000));
    output.count_segments(dir, &listed_segments(&listing(2, "2.000000")));
    output.count_segments(dir, &listed_segments(&listing(3, "4.000000")));
    // 2 MB over 8 s.
    assert_eq!(output.through, Some(3));
    assert_eq!(output.bitrate(), Some(2_000_000));
    // One with every segment listed counts only the ones after.
    std::fs::write(dir.join("seg4.m4s"), vec![0u8; 3_000_000]).unwrap();
    let all = format!(
        "{}#EXTINF:2.000000,\nseg2.m4s\n#EXTINF:4.000000,\nseg3.m4s\n\
         #EXTINF:4.000000,\nseg4.m4s\n",
        listing(1, "2.000000")
    );
    output.count_segments(dir, &listed_segments(&all));
    // 5 MB over 12 s.
    assert_eq!((output.bytes, output.ms), (5_000_000, 12_000));
    assert_eq!(output.bitrate(), Some(3_333_333));
}

/// A directory of the test's own, removed when dropped.
async fn scratch(test: &str) -> spoolframe_test_dir::TestDir {
    spoolframe_test_dir::TestDir::new(&format!("spoolframe-jobs-{test}")).await
}

/// Writes an executable script. The text goes to a plain file first and
/// `install` makes the script from it, so this process never holds the
/// script open for writing: a child another test forks at that moment
/// would inherit the handle, and running the script would then fail with
/// "Text file busy" (ETXTBSY).
fn write_script(path: &Path, text: &str) {
    let source = path.with_extension("txt");
    std::fs::write(&source, text).unwrap();
    let status = std::process::Command::new("install")
        .args(["-m", "755"])
        .arg(&source)
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "install {} failed", path.display());
}

/// A stand-in for ffmpeg: a shell script.
pub(crate) fn ffmpeg(dir: &Path, body: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("ffmpeg");
    write_script(&path, &format!("#!/bin/sh\n{body}\n"));
    path
}

/// Jobs running `body` as ffmpeg, in a directory kept for the test.
async fn jobs(
    test: &str,
    body: &str,
    max_jobs: usize,
    read_ahead: Duration,
) -> (Jobs, spoolframe_test_dir::TestDir) {
    jobs_stalled_after(test, body, max_jobs, read_ahead, STALLED_AFTER).await
}

/// The same, with an encode ended as stalled after `stalled_after`
/// without progress.
async fn jobs_stalled_after(
    test: &str,
    body: &str,
    max_jobs: usize,
    read_ahead: Duration,
    stalled_after: Duration,
) -> (Jobs, spoolframe_test_dir::TestDir) {
    let tmp = scratch(test).await;
    let root = tmp.as_ref().to_path_buf();
    let bin = ffmpeg(&root.join("bin"), body);
    let limits = JobLimits {
        budget: Units::jobs(max_jobs as u32),
        max_remux: 32,
        max_gpu_jobs: 3,
        scratch_dir: root.join("scratch"),
        scratch_limit_bytes: 1 << 30,
        read_ahead,
        copy_segment_secs: 2,
        stalled_after,
        gpu_filters: false,
        tone_map: spoolframe_media::encoders::ToneMapSetting::Auto,
        video_codecs: crate::config::DEFAULT_VIDEO_CODECS.to_vec(),
    };
    (Jobs::new(limits, Tool::unconfined(bin)).unwrap(), tmp)
}

async fn done(job: &EncodeJob) -> Result<(), String> {
    let mut progress = job.progress();
    let ended = progress
        .wait_for(|p| p.done.is_some())
        .await
        .unwrap()
        .clone();
    ended.done.unwrap().map_err(|f| f.reason)
}

fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
        && std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .is_ok_and(|stat| !stat.contains(") Z "))
}

fn stopped(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| stat.contains(") T "))
}

/// A run killed by a signal (the OOM killer) is not a GPU decode
/// failure; one that exits non-zero, or stalls, may be.
#[test]
fn a_run_killed_by_a_signal_is_not_a_decode_failure() {
    use std::os::unix::process::ExitStatusExt;
    let killed = std::process::ExitStatus::from_raw(9);
    let failed = std::process::ExitStatus::from_raw(1 << 8);
    assert!(!decode_failed(&Ended::Exited(Ok(killed))));
    assert!(decode_failed(&Ended::Exited(Ok(failed))));
    assert!(decode_failed(&Ended::Stalled));
    assert!(!decode_failed(&Ended::Exited(Ok(
        std::process::ExitStatus::from_raw(0)
    ))));
}

/// A run that failed, still held by its playback, gives up its slot:
/// the only one, here, which the next job then takes.
#[tokio::test]
async fn a_failed_encode_gives_up_its_slot() {
    let (jobs, _dir) = jobs("fail-slot", "exit 1", 1, Duration::from_secs(60)).await;
    let scratch = jobs.dir().to_path_buf();
    let job = jobs
        .start(
            scratch.join("a/0"),
            Vec::new(),
            Written::Hls,
            Load::Software,
            0,
            0,
            0,
            None,
        )
        .unwrap();
    assert!(done(&job).await.is_err());
    assert!(job.slot().is_none());
    jobs.start(
        scratch.join("b/0"),
        Vec::new(),
        Written::Hls,
        Load::Software,
        0,
        0,
        0,
        None,
    )
    .unwrap();
}

#[tokio::test]
async fn a_failed_encode_reports_ffmpeg_s_last_words() {
    let (jobs, _dir) = jobs(
        "fail",
        "echo 'out.mkv: Invalid data found when processing input' >&2; exit 1",
        2,
        Duration::from_secs(60),
    )
    .await;
    let dir = jobs.dir().join("h/0");
    let job = jobs
        .start(dir, Vec::new(), Written::Hls, Load::Remux, 0, 0, 0, None)
        .unwrap();
    assert_eq!(
        done(&job).await.unwrap_err(),
        "out.mkv: Invalid data found when processing input"
    );
}

/// A run's speed is observed as it ends, by its kind: the media time
/// it encoded over the time it ran. One killed as it started is not.
#[tokio::test]
async fn a_run_s_speed_is_observed_at_its_end_and_a_short_run_s_is_not() {
    let (jobs, _dir) = jobs(
        "speed",
        "printf 'abc' > \"$1\"; echo out_time_us=3000000; sleep 1.5",
        2,
        Duration::from_secs(60),
    )
    .await;
    let histograms = Arc::new(Histograms::default());
    jobs.record_into(histograms.clone());
    let run = |n: u32| {
        let dir = jobs.dir().join(format!("h/{n}"));
        let file = dir.join("out.mkv");
        jobs.start(
            dir,
            vec![file.display().to_string()],
            Written::Progressive { file },
            Load::Remux,
            n,
            0,
            0,
            None,
        )
        .unwrap()
    };
    let killed = run(0);
    killed.stop_because("seek");
    let started = std::time::Instant::now();
    let finished = run(1);
    done(&finished).await.unwrap();
    // The run is timed from just after ffmpeg is spawned to its exit,
    // so within what the test waited for it.
    let waited = started.elapsed().as_secs_f64();
    let family = histograms.family(&crate::metrics::histogram::CONVERSION_SPEED);
    let [sample] = family.samples.as_slice() else {
        panic!("{family:?}");
    };
    assert_eq!(sample.labels, [("kind", "remux".to_owned())]);
    let crate::metrics::Value::Histogram(h) = &sample.value else {
        panic!("{sample:?}");
    };
    assert_eq!(h.count, 1);
    // 3 s of media over the time it ran: no faster than the 1.5 s
    // sleep allows, less 5 % for ffmpeg starting just before its timer
    // does, and no slower than over all the time the test waited (a
    // loaded machine stretches the sleep).
    let fastest = 3.0 / (1.5 * 0.95);
    let slowest = 3.0 / waited;
    assert!(
        (slowest..=fastest).contains(&h.sum),
        "{} not within {slowest:.3}..={fastest:.3}",
        h.sum
    );
}

#[tokio::test]
async fn what_a_job_writes_is_measured() {
    let (jobs, _dir) = jobs(
        "write",
        "printf 'abcdefghij' > \"$1\"; echo out_time_us=4000000",
        2,
        Duration::from_secs(60),
    )
    .await;
    let dir = jobs.dir().join("h/0");
    let file = dir.join("out.mkv");
    let job = jobs
        .start(
            dir,
            vec![file.display().to_string()],
            Written::Progressive { file },
            Load::Remux,
            0,
            0,
            0,
            None,
        )
        .unwrap();
    done(&job).await.unwrap();
    let progress = job.progress().borrow().clone();
    assert_eq!(progress.bytes, 10);
    assert_eq!(progress.encoded_ms, 4000);
    let counts = jobs.counts();
    assert_eq!(counts.output_seconds, 4.0);
    assert_eq!(counts.running, 0);
}

/// A run started at a seek reports the film's own time: only what it
/// encoded from there is output (P3-14).
#[tokio::test]
async fn output_is_counted_from_where_the_run_started() {
    let (jobs, _dir) = jobs(
        "output-from-seek",
        "echo out_time_us=61000000; echo out_time_us=63500000",
        2,
        Duration::from_secs(60),
    )
    .await;
    let job = jobs
        .start(
            jobs.dir().join("h/0"),
            Vec::new(),
            Written::Hls,
            Load::Remux,
            0,
            0,
            60_000,
            None,
        )
        .unwrap();
    done(&job).await.unwrap();
    assert_eq!(jobs.counts().output_seconds, 3.5);
}

#[tokio::test]
async fn one_job_too_many_is_busy_until_one_stops() {
    let (jobs, _dir) = jobs("busy", "exec sleep 30", 1, Duration::from_secs(60)).await;
    let scratch = jobs.dir().to_path_buf();
    let first = jobs
        .start(
            scratch.join("a/0"),
            Vec::new(),
            Written::Hls,
            Load::Software,
            0,
            0,
            0,
            None,
        )
        .unwrap();
    assert!(matches!(
        jobs.start(
            scratch.join("b/0"),
            Vec::new(),
            Written::Hls,
            Load::Software,
            0,
            0,
            0,
            None
        ),
        Err(JobError::Busy)
    ));
    let counts = jobs.counts();
    assert_eq!((counts.running, counts.refused), (1, 1));
    assert_eq!(counts.running_by_load, [0, 0, 0, 1, 0], "{counts:?}");
    first.stop();
    jobs.start(
        scratch.join("b/0"),
        Vec::new(),
        Written::Hls,
        Load::Software,
        0,
        0,
        0,
        None,
    )
    .unwrap();
}

#[tokio::test]
/// PX-25: a remux counts as an encode does.
async fn a_conversion_counts_while_ffmpeg_runs_a_remux_too() {
    let (some, _dir) = jobs("encoding", "exec sleep 30", 3, Duration::from_secs(60)).await;
    let encoding = some.encoding();
    let scratch = some.dir().to_path_buf();
    let copy = some
        .start(
            scratch.join("c/0"),
            Vec::new(),
            Written::Hls,
            Load::Remux,
            0,
            0,
            0,
            None,
        )
        .unwrap();
    assert_eq!(encoding.running(), 1);
    let stopped = some
        .start(
            scratch.join("a/0"),
            Vec::new(),
            Written::Hls,
            Load::Software,
            0,
            0,
            0,
            None,
        )
        .unwrap();
    assert_eq!(encoding.running(), 2);
    stopped.stop();
    copy.stop();
    none_encoding(&encoding).await;
    // One whose ffmpeg exits by itself stops counting then, though
    // the job (its playback) lives on.
    let (quick, _dir) = jobs("encoding-exits", "exit 0", 1, Duration::from_secs(60)).await;
    let encoding = quick.encoding();
    let exited = quick
        .start(
            quick.dir().join("e/0"),
            Vec::new(),
            Written::Hls,
            Load::Software,
            0,
            0,
            0,
            None,
        )
        .unwrap();
    done(&exited).await.unwrap();
    none_encoding(&encoding).await;
}

async fn none_encoding(encoding: &Encoding) {
    tokio::time::timeout(
        Duration::from_secs(10),
        encoding.watch().wait_for(|n| *n == 0),
    )
    .await
    .expect("an encode still counts")
    .unwrap();
}

/// Waits for `ready`, checking every 20 ms. The 30 s only stops a test
/// that would hang: what is waited for comes far sooner, even on a
/// loaded machine.
pub(crate) async fn eventually(ready: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("never happened");
}

/// Playback debug (P3-15) shows what ffmpeg said last while it still
/// runs, not only once it has failed; only the last lines are kept.
#[tokio::test]
async fn ffmpeg_s_last_stderr_lines_are_kept_while_it_runs() {
    let (jobs, _dir) = jobs(
        "stderr-ring",
        "i=0; while [ $i -lt 100 ]; do echo \"line $i\" >&2; i=$((i+1)); done; exec sleep 30",
        2,
        Duration::from_secs(60),
    )
    .await;
    let job = jobs
        .start(
            jobs.dir().join("h/0"),
            Vec::new(),
            Written::Hls,
            Load::Remux,
            0,
            0,
            0,
            None,
        )
        .unwrap();
    eventually(|| job.stderr().last().is_some_and(|l| l == "line 99")).await;
    let kept = job.stderr();
    assert_eq!(kept.len(), STDERR_KEPT);
    assert_eq!(kept[0], format!("line {}", 100 - STDERR_KEPT));
}

/// A run decoding on the GPU, for `film`, as `jobs` gets it: `hw` as
/// its last argument, which the stand-in ffmpeg looks for, and no
/// `hw` in its software fallback.
fn gpu_run(file: &Path, film: &Path) -> Run {
    let file = file.display().to_string();
    Run {
        args: vec![file.clone(), "hw".into()],
        gpu: GpuUse::Vaapi,
        fallback: Some(Fallback {
            args: vec![file],
            gpu: GpuUse::None,
            file: film.to_owned(),
            made: Some(made_by(crate::encode::Decoder::Software)),
        }),
        made: Some(made_by(crate::encode::Decoder::Hardware(
            spoolframe_media::encoders::Hardware::Vaapi,
        ))),
        ..Run::default()
    }
}

/// A 1080p encode decoded by `decoder`, tone-mapped where it decodes.
fn made_by(decoder: crate::encode::Decoder) -> Made {
    use crate::encode::{Decoder, FilterOn};
    Made {
        decoder,
        encoder: None,
        tone_map: match decoder {
            Decoder::Hardware(_) => FilterOn::Gpu,
            _ => FilterOn::Cpu,
        },
        tone_mapper: None,
        burn_in: FilterOn::None,
        size: Some((1920, 1080)),
        target_bitrate: None,
    }
}

/// The stand-in: `on_gpu` when the last argument is `hw`, else writes
/// `abc` to the first and reports a second encoded.
fn decoding(on_gpu: &str) -> String {
    format!(
        "for a; do last=$a; done\n\
         if [ \"$last\" = hw ]; then {on_gpu}; fi\n\
         printf abc > \"$1\"; echo out_time_us=1000000"
    )
}

/// A GPU decode the driver refuses (10-bit H.264, an AV1 profile) fails
/// before ffmpeg writes anything: the same job runs again decoding in
/// software, and the film is remembered, so its next run decodes in
/// software from the start.
#[tokio::test]
async fn a_gpu_decode_that_fails_before_any_output_runs_again_in_software() {
    let (jobs, tmp) = jobs(
        "gpu-decode-fails",
        &decoding(
            "echo 'Impossible to convert between the formats supported by the filter' >&2; \
             exit 218",
        ),
        1,
        Duration::from_secs(60),
    )
    .await;
    let film = tmp.as_ref().join("film.mkv");
    let dir = jobs.dir().join("h/0");
    let file = dir.join("out.mkv");
    let job = jobs
        .start(
            dir,
            gpu_run(&file, &film),
            Written::Progressive { file: file.clone() },
            Load::Gpu,
            0,
            0,
            0,
            None,
        )
        .unwrap();
    assert_eq!(done(&job).await, Ok(()));
    assert_eq!(job.progress().borrow().bytes, 3);
    assert!(jobs.hardware_decode_failed(&film));
    assert!(!jobs.hardware_decode_failed(&tmp.as_ref().join("other.mkv")));
    assert!(!job.decodes_in_hardware());
    // PX-24: what playback debug says is made now is the run in
    // software's.
    assert_eq!(job.made(), Some(made_by(crate::encode::Decoder::Software)));
    // What ran last is what playback debug shows; what the GPU run
    // said is kept above it.
    assert_eq!(
        job.command(),
        [
            jobs.ffmpeg.program().display().to_string(),
            file.display().to_string()
        ]
    );
    assert!(
        job.stderr()
            .iter()
            .any(|l| l.starts_with("Impossible to convert")),
        "{:?}",
        job.stderr()
    );
}

/// One that fails after it wrote is not run again from the start: what
/// its reader has would change under it. It fails, and the film is
/// remembered, so the playback's next run (a seek, the next segment)
/// decodes in software.
#[tokio::test]
async fn a_gpu_decode_that_fails_after_its_first_output_fails_and_is_remembered() {
    let (jobs, tmp) = jobs(
        "gpu-decode-fails-late",
        &decoding("printf abc > \"$1\"; echo 'hardware decoding failed' >&2; exit 1"),
        1,
        Duration::from_secs(60),
    )
    .await;
    let film = tmp.as_ref().join("film.mkv");
    let dir = jobs.dir().join("h/0");
    let file = dir.join("out.mkv");
    let job = jobs
        .start(
            dir,
            gpu_run(&file, &film),
            Written::Progressive { file: file.clone() },
            Load::Gpu,
            0,
            0,
            0,
            None,
        )
        .unwrap();
    assert_eq!(done(&job).await.unwrap_err(), "hardware decoding failed");
    assert!(jobs.hardware_decode_failed(&film));
    assert!(job.decodes_in_hardware());
}

/// A GPU decode that hangs before its first output is ended as stalled
/// and run again in software, rather than holding the playback.
#[tokio::test]
async fn a_gpu_decode_that_stalls_before_any_output_runs_again_in_software() {
    let (jobs, tmp) = jobs_stalled_after(
        "gpu-decode-stalls",
        &decoding("exec sleep 30"),
        1,
        Duration::from_secs(60),
        Duration::from_millis(600),
    )
    .await;
    let film = tmp.as_ref().join("film.mkv");
    let dir = jobs.dir().join("h/0");
    let file = dir.join("out.mkv");
    let job = jobs
        .start(
            dir,
            gpu_run(&file, &film),
            Written::Progressive { file: file.clone() },
            Load::Gpu,
            0,
            0,
            0,
            None,
        )
        .unwrap();
    let hung = job.pid();
    assert_eq!(done(&job).await, Ok(()));
    assert_ne!(job.pid(), hung);
    assert!(!alive(hung), "the hung ffmpeg {hung} still runs");
    assert!(jobs.hardware_decode_failed(&film));
    // The hung run is counted as a stall all the same.
    assert_eq!(jobs.counts().stalled, 1);
}

/// The software run is the job's ffmpeg in every way: held back for its
/// reader, and killed with the job.
#[tokio::test]
async fn the_software_run_is_throttled_and_killed_as_the_first_would_be() {
    let (jobs, tmp) = jobs(
        "gpu-decode-fallback-supervised",
        "for a; do last=$a; done\n\
         if [ \"$last\" = hw ]; then exit 218; fi\n\
         head -c 100 /dev/zero > \"$1\"; echo out_time_us=60000000; exec sleep 30",
        1,
        Duration::from_secs(10),
    )
    .await;
    let film = tmp.as_ref().join("film.mkv");
    let dir = jobs.dir().join("h/0");
    let file = dir.join("out.mkv");
    let job = jobs
        .start(
            dir,
            gpu_run(&file, &film),
            Written::Progressive { file },
            Load::Gpu,
            0,
            0,
            0,
            None,
        )
        .unwrap();
    let first = job.pid();
    eventually(|| job.pid() != first && stopped(job.pid())).await;
    assert!(job.paused());
    let pid = job.pid();
    drop(job);
    eventually(|| !alive(pid)).await;
}

/// What the job runs, as it ran it: the program and its arguments.
#[tokio::test]
async fn a_job_knows_its_command_line() {
    let (jobs, _dir) = jobs("command", "exec sleep 30", 2, Duration::from_secs(60)).await;
    let args = vec!["-i".to_owned(), "/media/a b.mkv".to_owned()];
    let job = jobs
        .start(
            jobs.dir().join("h/0"),
            args.clone(),
            Written::Hls,
            Load::Remux,
            0,
            0,
            0,
            None,
        )
        .unwrap();
    let mut expected = vec![jobs.ffmpeg.program().display().to_string()];
    expected.extend(args);
    assert_eq!(job.command(), expected);
}

/// End conversion (P3-15): ffmpeg stops, the place is free at once, and
/// what it wrote stays readable as all there is, rather than a reader
/// waiting for more that never comes.
#[tokio::test]
async fn an_ended_encode_stops_and_what_it_wrote_is_the_whole() {
    let (jobs, _dir) = jobs(
        "end",
        "printf 'abcdefghij' > \"$1\"; echo out_time_us=4000000; exec sleep 30",
        1,
        Duration::from_secs(60),
    )
    .await;
    let dir = jobs.dir().join("h/0");
    let file = dir.join("out.mkv");
    let job = jobs
        .start(
            dir.clone(),
            vec![file.display().to_string()],
            Written::Progressive { file: file.clone() },
            Load::Software,
            0,
            0,
            0,
            None,
        )
        .unwrap();
    let mut progress = job.progress();
    progress.wait_for(|p| p.bytes == 10).await.unwrap();
    let pid = job.pid();
    job.end();
    let now = job.progress().borrow().clone();
    assert_eq!(now.done, Some(Ok(())));
    assert_eq!(now.bytes, 10);
    eventually(|| !alive(pid)).await;
    // Its files stay for the reader until the job is dropped.
    assert!(file.exists());
    // Its place is free: another job starts at the limit of one.
    let other = jobs.start(
        jobs.dir().join("i/0"),
        Vec::new(),
        Written::Hls,
        Load::Remux,
        0,
        0,
        0,
        None,
    );
    assert!(other.is_ok(), "the ended job still holds its place");
    // The ended job no longer counts for previews; the new remux does.
    eventually(|| jobs.encoding().running() == 1).await;
}

#[tokio::test]
async fn dropping_a_job_kills_ffmpeg_and_removes_its_files() {
    let (jobs, _dir) = jobs("drop", "exec sleep 30", 2, Duration::from_secs(60)).await;
    let dir = jobs.dir().join("h/0");
    let job = jobs
        .start(
            dir.clone(),
            Vec::new(),
            Written::Hls,
            Load::Remux,
            0,
            0,
            0,
            None,
        )
        .unwrap();
    let pid = job.pid();
    assert!(alive(pid));
    drop(job);
    // The kill and the cleanup run on their own task: waited for, not
    // given a fixed time a loaded machine may not keep to.
    eventually(|| !alive(pid)).await;
    eventually(|| !dir.exists()).await;
}

#[tokio::test]
async fn an_encode_far_ahead_of_its_reader_is_paused_and_resumed() {
    // Reports a minute of encoded time at once, writes 100 bytes, then
    // waits.
    let (jobs, _dir) = jobs(
        "throttle",
        "head -c 100 /dev/zero > \"$1\"; echo out_time_us=60000000; exec sleep 30",
        2,
        Duration::from_secs(10),
    )
    .await;
    let dir = jobs.dir().join("h/0");
    let file = dir.join("out.mkv");
    let job = jobs
        .start(
            dir,
            vec![file.display().to_string()],
            Written::Progressive { file },
            Load::Remux,
            0,
            0,
            0,
            None,
        )
        .unwrap();
    let pid = job.pid();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !stopped(pid) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("not paused");
    assert!(job.paused());
    let counts = jobs.counts();
    assert_eq!((counts.running, counts.paused), (1, 1));
    // The reader is at 95 of 100 bytes: 57 s of 60, 3 s behind.
    job.read_to(95);
    tokio::time::timeout(Duration::from_secs(5), async {
        while stopped(pid) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("not resumed");
    assert!(!job.paused());
}

/// ffmpeg keeps a Matroska cluster to itself until it is whole, so it
/// reports encoded time before it has written a byte. A job held back
/// then stayed so for ever: its reader had nothing to read, and only a
/// read lets a held job go on.
#[tokio::test]
async fn an_encode_that_has_written_nothing_is_not_held_back() {
    // Reports two seconds encoded, and writes them a second later.
    let (jobs, _dir) = jobs(
        "nothing-written",
        "echo out_time_us=2000000; sleep 1; head -c 100 /dev/zero > \"$1\"; exec sleep 30",
        2,
        Duration::from_millis(500),
    )
    .await;
    let dir = jobs.dir().join("h/0");
    let file = dir.join("out.mkv");
    let job = jobs
        .start(
            dir,
            vec![file.display().to_string()],
            Written::Progressive { file },
            Load::Remux,
            0,
            0,
            0,
            None,
        )
        .unwrap();
    let mut progress = job.progress();
    tokio::time::timeout(
        Duration::from_secs(5),
        progress.wait_for(|p| p.bytes == 100),
    )
    .await
    .expect("the encode writes on")
    .unwrap();
}

/// The same for HLS: the reader waits for a segment ffmpeg has not
/// finished, though it reports time past the segment's end.
#[tokio::test]
async fn an_encode_is_not_held_back_before_the_segment_its_reader_waits_for() {
    // Reports twenty seconds encoded, and lists segment 1 a second later.
    let (jobs, _dir) = jobs(
        "segment-awaited",
        &format!(
            "echo out_time_us=20000000; sleep 1; echo seg1.m4s > \"$1/{PLAYLIST}\"; exec sleep 30"
        ),
        2,
        Duration::from_millis(500),
    )
    .await;
    let dir = jobs.dir().join("h/0");
    let args = vec![dir.display().to_string()];
    let job = jobs
        .start(dir, args, Written::Hls, Load::Remux, 0, 1, 0, None)
        .unwrap();
    job.reader_at(1, 6_000);
    let mut progress = job.progress();
    let listed = progress.wait_for(|p| p.last_segment == Some(1));
    tokio::time::timeout(Duration::from_secs(5), listed)
        .await
        .expect("the encode finishes the segment")
        .unwrap();
}

/// `read_ahead_secs = 0` holds the encode whenever its reader is
/// behind. It once never let it go on: with the reader caught up the
/// encode was no longer ahead, but not under half of nothing either.
#[tokio::test]
async fn an_encode_with_no_read_ahead_goes_on_once_its_reader_has_caught_up() {
    let (jobs, _dir) = jobs(
        "no-read-ahead",
        "head -c 100 /dev/zero > \"$1\"; echo out_time_us=1000000; exec sleep 30",
        2,
        Duration::ZERO,
    )
    .await;
    let dir = jobs.dir().join("h/0");
    let file = dir.join("out.mkv");
    let job = jobs
        .start(
            dir,
            vec![file.display().to_string()],
            Written::Progressive { file },
            Load::Remux,
            0,
            0,
            0,
            None,
        )
        .unwrap();
    let pid = job.pid();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !stopped(pid) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("not paused");
    job.read_to(100);
    tokio::time::timeout(Duration::from_secs(5), async {
        while stopped(pid) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("not resumed");
    assert!(!job.paused());
}

/// Short, for the tests of a stalled encode, and over two looks at the
/// output long.
const STALLED_SOON: Duration = Duration::from_millis(500);

fn stalled(done: Result<(), String>) -> bool {
    done.is_err_and(|reason| reason.starts_with("the conversion stalled"))
}

/// ffmpeg can wait for ever with nothing wrong that it knows of (it
/// does when it lays a picture subtitle file of one cue over the
/// video and loses the file's end). The job once waited with it, in
/// its slot, and its reads waited too.
#[tokio::test]
async fn an_encode_that_makes_no_progress_is_ended_and_gives_up_its_slot() {
    let (jobs, _dir) = jobs_stalled_after(
        "stalled",
        "exec sleep 30",
        1,
        Duration::from_secs(60),
        STALLED_SOON,
    )
    .await;
    let scratch = jobs.dir().to_path_buf();
    let job = jobs
        .start(
            scratch.join("a/0"),
            Vec::new(),
            Written::Hls,
            Load::Software,
            0,
            0,
            0,
            None,
        )
        .unwrap();
    let pid = job.pid();
    let ended = tokio::time::timeout(Duration::from_secs(5), done(&job))
        .await
        .expect("the stalled encode is ended");
    assert_eq!(
        ended.unwrap_err(),
        "the conversion stalled: ffmpeg made no progress for 500ms"
    );
    assert!(!alive(pid), "ffmpeg {pid} still runs");
    let counts = jobs.counts();
    assert_eq!((counts.running, counts.stalled), (0, 1));
    // The job is still held, as by a playback not yet stopped: its
    // slot, the only one, is free.
    jobs.start(
        scratch.join("b/0"),
        Vec::new(),
        Written::Hls,
        Load::Software,
        0,
        0,
        0,
        None,
    )
    .unwrap();
}

/// An encode held back for its reader makes no progress on purpose,
/// for as long as the reader takes. The time it is held does not count:
/// let go on, it has the whole time again.
#[tokio::test]
async fn an_encode_held_back_for_its_reader_is_not_stalled() {
    // Reports a minute of encoded time, writes 100 bytes, then hangs.
    let (jobs, _dir) = jobs_stalled_after(
        "stalled-held",
        "head -c 100 /dev/zero > \"$1\"; echo out_time_us=60000000; exec sleep 30",
        2,
        Duration::from_secs(10),
        STALLED_SOON,
    )
    .await;
    let dir = jobs.dir().join("h/0");
    let file = dir.join("out.mkv");
    let job = jobs
        .start(
            dir,
            vec![file.display().to_string()],
            Written::Progressive { file },
            Load::Remux,
            0,
            0,
            0,
            None,
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !job.paused() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("not paused");
    // Held for six looks, 1.2 s: over twice the time a stall takes.
    looked(&job, 6).await;
    assert_eq!(job.progress().borrow().done, None);
    assert!(job.paused());
    // The reader catches up: the encode goes on, and now it hangs.
    job.read_to(100);
    let ended = tokio::time::timeout(Duration::from_secs(5), done(&job))
        .await
        .expect("the stalled encode is ended");
    assert!(stalled(ended));
}

/// A slow encode is not a stalled one: while ffmpeg reports more time
/// encoded, however little, the job lets it run.
#[tokio::test]
async fn an_encode_that_gets_on_slowly_is_not_stalled() {
    // Reports a tenth of a second more every tenth of a second, for
    // 1.5 s (three times the time a stall takes), says so, then hangs.
    let (jobs, _dir) = jobs_stalled_after(
        "stalled-slow",
        "i=0; while [ $i -lt 15 ]; do i=$((i+1)); echo out_time_us=${i}00000; sleep 0.1; done\n\
         touch \"$1\"; exec sleep 30",
        2,
        Duration::from_secs(60),
        STALLED_SOON,
    )
    .await;
    let dir = jobs.dir().join("h/0");
    let slow_part_over = jobs.dir().join("over");
    let args = vec![slow_part_over.display().to_string()];
    let job = jobs
        .start(dir, args, Written::Hls, Load::Remux, 0, 0, 0, None)
        .unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(10), done(&job))
        .await
        .expect("the stalled encode is ended");
    assert!(stalled(ended));
    assert!(
        slow_part_over.exists(),
        "ended while it still reported progress"
    );
}

/// Jobs running `body` as ffmpeg, which may write `scratch_limit`
/// bytes together.
async fn jobs_within(
    test: &str,
    body: &str,
    scratch_limit: u64,
) -> (Jobs, spoolframe_test_dir::TestDir) {
    let tmp = scratch(test).await;
    let root = tmp.as_ref().to_path_buf();
    let bin = ffmpeg(&root.join("bin"), body);
    let limits = JobLimits {
        budget: Units::jobs(2),
        max_remux: 32,
        max_gpu_jobs: 3,
        scratch_dir: root.join("scratch"),
        scratch_limit_bytes: scratch_limit,
        read_ahead: Duration::from_secs(60),
        copy_segment_secs: 2,
        stalled_after: STALLED_AFTER,
        gpu_filters: false,
        tone_map: spoolframe_media::encoders::ToneMapSetting::Auto,
        video_codecs: crate::config::DEFAULT_VIDEO_CODECS.to_vec(),
    };
    (Jobs::new(limits, Tool::unconfined(bin)).unwrap(), tmp)
}

const FULL: &str = "the server's scratch space for conversions is full";

/// Waits until `job` has looked at its output `times` more times. A
/// job tells its progress at every look, changed or not, and only
/// then looks at the scratch space: with the next look told, the one
/// before is over.
async fn looked(job: &EncodeJob, times: usize) {
    let mut progress = job.progress();
    progress.borrow_and_update();
    for _ in 0..times {
        tokio::time::timeout(Duration::from_secs(5), progress.changed())
            .await
            .expect("the job looks at its output")
            .unwrap();
    }
}

#[tokio::test]
async fn the_job_that_fills_the_scratch_space_fails_and_no_other() {
    // `quiet` writes 100 bytes and waits; the other writes on and on.
    let (jobs, _dir) = jobs_within(
        "full",
        "if [ \"$2\" = quiet ]; then head -c 100 /dev/zero > \"$1\"; exec sleep 30; fi\n\
         while :; do head -c 1000 /dev/zero >> \"$1\"; sleep 0.01; done",
        20_000,
    )
    .await;
    let start = |name: &str, mode: &str| {
        let dir = jobs.dir().join(name).join("0");
        let file = dir.join("out.mkv");
        let args = vec![file.display().to_string(), mode.to_owned()];
        let written = Written::Progressive { file: file.clone() };
        (
            jobs.start(dir, args, written, Load::Remux, 0, 0, 0, None)
                .unwrap(),
            file,
        )
    };
    let (quiet, kept) = start("a", "quiet");
    let mut progress = quiet.progress();
    progress.wait_for(|p| p.bytes == 100).await.unwrap();
    // It has seen its 100 bytes, in a space with room.
    looked(&quiet, 1).await;
    // Something else fills the space, and it stays full: the quiet job
    // looks at its output twice more, so once at least with the space
    // full and that look over.
    let other = jobs.dir().join("c/0");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("out.mkv"), vec![0; 30_000]).unwrap();
    looked(&quiet, 2).await;
    assert_eq!(quiet.progress().borrow().done, None);
    // The job that writes into the full space is the one that fails.
    let (greedy, removed) = start("b", "greedy");
    assert_eq!(done(&greedy).await.unwrap_err(), FULL);
    // What it wrote is gone.
    assert!(!removed.exists());
    assert_eq!(quiet.progress().borrow().done, None);
    assert!(alive(quiet.pid()));
    assert!(kept.exists());
}

/// The segment muxer writes a run's `init.mp4` as it starts, where the
/// HLS muxer kept everything in memory until its first segment was
/// done. A run restarted beside the one it replaces (kept, stopped,
/// with its segments) was ended for a full space before it had a
/// segment to give its reader. Before its first segment it is not.
#[tokio::test]
async fn an_hls_run_is_not_ended_for_a_full_space_before_its_first_segment() {
    let (jobs, _dir) = jobs_within(
        "full-before-first",
        &format!(
            "while :; do head -c 100 /dev/zero >> \"$1/init.mp4\"\n\
             [ -e \"$1/go\" ] && printf 'seg1.m4s\\n' > \"$1/{PLAYLIST}\"\n\
             sleep 0.01; done"
        ),
        20_000,
    )
    .await;
    let other = jobs.dir().join("c/0");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("out.mkv"), vec![0; 30_000]).unwrap();
    let dir = jobs.dir().join("a").join("0");
    let args = vec![dir.display().to_string()];
    let job = jobs
        .start(dir.clone(), args, Written::Hls, Load::Remux, 0, 1, 0, None)
        .unwrap();
    looked(&job, 3).await;
    assert_eq!(job.progress().borrow().done, None);
    // Its first segment done, the space is still full and nothing is
    // behind its reader: it is the one that fails.
    std::fs::write(dir.join("go"), b"").unwrap();
    assert_eq!(done(&job).await.unwrap_err(), FULL);
}

/// Writes segments 1 to 8 of 1000 bytes into `dir`, as ffmpeg would,
/// and waits until `job` has made room for them and that look at the
/// scratch space is over (or the job has ended). The job sees the
/// eight files at once: its task runs on the test's thread, only while
/// the test waits. It has finished the look that removed a segment
/// when it counts a byte written after.
async fn eight_segments(job: &EncodeJob, dir: &Path) {
    for n in 1..=8 {
        std::fs::write(dir.join(format!("seg{n}.m4s")), [0; 1000]).unwrap();
    }
    let mut progress = job.progress();
    tokio::time::timeout(Duration::from_secs(5), async {
        while job.first_kept() == 1 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        std::fs::write(dir.join("mark"), [0]).unwrap();
        let counted = |p: &Progress| p.bytes % 1000 == 1 || p.done.is_some();
        progress.wait_for(counted).await.unwrap();
    })
    .await
    .expect("no segment removed");
}

/// Which of segments 1 to 8 are in `dir`.
fn segments_left(dir: &Path) -> Vec<bool> {
    (1..=8)
        .map(|n| dir.join(format!("seg{n}.m4s")).exists())
        .collect()
}

/// What a job writes is counted for the scratch space as it is
/// measured, and uncounted when the job goes: what decides whether a
/// job walks every job's files, which each did on every look. Review
/// 2026-10-11 H2.
#[tokio::test]
async fn a_job_s_bytes_are_counted_while_it_lives() {
    let script = "head -c 3000 /dev/zero > \"$1/a\"; echo out_time_us=1000000; exec sleep 30";
    let (jobs, _dir) = jobs_within("counted", script, 1 << 30).await;
    let dir = jobs.dir().join("h/0");
    let args = vec![dir.display().to_string()];
    let job = jobs
        .start(dir.clone(), args, Written::Hls, Load::Remux, 0, 1, 0, None)
        .unwrap();
    let used = || jobs.scratch_used.load(Ordering::Relaxed);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while used() < 3000 {
        assert!(std::time::Instant::now() < deadline, "counted {}", used());
        looked(&job, 1).await;
    }
    assert_eq!(used(), 3000);
    drop(job);
    assert_eq!(used(), 0);
}

#[tokio::test]
async fn a_full_scratch_space_takes_the_segments_behind_the_reader_first() {
    let (jobs, _dir) = jobs_within("trim", "exec sleep 30", 5_500).await;
    let dir = jobs.dir().join("h/0");
    let job = jobs
        .start(
            dir.clone(),
            Vec::new(),
            Written::Hls,
            Load::Remux,
            0,
            1,
            0,
            None,
        )
        .unwrap();
    // The reader asked for segment 7: 6 stays too, 1 to 5 may go.
    job.reader_at(7, 42_000);
    eight_segments(&job, &dir).await;
    // 8000 bytes, 5500 allowed: the three oldest went, and no more.
    assert_eq!(job.first_kept(), 4);
    assert_eq!(
        segments_left(&dir),
        [false, false, false, true, true, true, true, true]
    );
    assert_eq!(job.progress().borrow().done, None);
}

/// A player that seeks back to a segment still on disk reads there
/// again. The job once kept only the furthest segment ever asked for,
/// so it took the segments the player was now reading for ones behind
/// it, and removed them for room.
#[tokio::test]
async fn a_reader_that_went_back_keeps_the_segments_it_now_reads() {
    // Says it has encoded two minutes and lists segments 1 to 8, in a
    // playlist as large as a segment, then waits.
    let script = format!(
        "printf 'seg%s.m4s\\n' 1 2 3 4 5 6 7 8 > \"$1/{PLAYLIST}\"; \
         truncate -s 1000 \"$1/{PLAYLIST}\"; echo out_time_us=120000000; exec sleep 30"
    );
    let (jobs, _dir) = jobs_within("back", &script, 5_500).await;
    let dir = jobs.dir().join("h/0");
    let args = vec![dir.display().to_string()];
    let job = jobs
        .start(dir.clone(), args, Written::Hls, Load::Remux, 0, 1, 0, None)
        .unwrap();
    // The reader got to segment 7, then went back to 4.
    job.reader_at(7, 42_000);
    job.reader_at(4, 24_000);
    // Far behind what is encoded: the job is held back for it.
    tokio::time::timeout(Duration::from_secs(5), async {
        while !job.paused() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("not paused");
    eight_segments(&job, &dir).await;
    // 9000 bytes with the playlist, 5500 allowed. Segments 1 and 2 are
    // behind the reader and went; 3 is the one before the reader's and
    // stays, with all after it, though that leaves 7000.
    assert_eq!(job.first_kept(), 3);
    assert_eq!(
        segments_left(&dir),
        [false, false, true, true, true, true, true, true]
    );
    // The job is held back and writes no more: it is not failed for
    // the space it cannot free yet.
    assert_eq!(job.progress().borrow().done, None);
}

#[tokio::test]
async fn leftovers_from_a_previous_run_are_removed() {
    let tmp = scratch("leftovers").await;
    let root = tmp.as_ref();
    let old = root.join("scratch").join(JOBS_DIR).join("oldhandle/0");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("out.mkv"), b"stale").unwrap();
    let limits = JobLimits {
        budget: Units::jobs(1),
        max_remux: 32,
        max_gpu_jobs: 3,
        scratch_dir: root.join("scratch"),
        scratch_limit_bytes: 1 << 30,
        read_ahead: Duration::from_secs(60),
        copy_segment_secs: 2,
        stalled_after: STALLED_AFTER,
        gpu_filters: false,
        tone_map: spoolframe_media::encoders::ToneMapSetting::Auto,
        video_codecs: crate::config::DEFAULT_VIDEO_CODECS.to_vec(),
    };
    let jobs = Jobs::new(limits, Tool::unconfined("ffmpeg")).unwrap();
    assert!(!jobs.dir().join("oldhandle").exists());
    assert!(jobs.dir().is_dir());
}

/// `transcode.scratch_dir` may name a directory other things are in:
/// the state directory, the parent of the subtitle cache, a mount
/// other programs use. Once, all of it was removed at every start.
#[tokio::test]
async fn what_else_is_in_the_scratch_directory_is_left_alone() {
    let tmp = scratch("not-ours").await;
    let root = tmp.as_ref();
    let key = root.join("state/endpoint.key");
    let text = root.join("state/subtitles/film.vtt");
    std::fs::create_dir_all(text.parent().unwrap()).unwrap();
    std::fs::write(&key, b"secret").unwrap();
    std::fs::write(&text, b"WEBVTT").unwrap();
    let limits = JobLimits {
        budget: Units::jobs(1),
        max_remux: 32,
        max_gpu_jobs: 3,
        scratch_dir: root.join("state"),
        scratch_limit_bytes: 1 << 30,
        read_ahead: Duration::from_secs(60),
        copy_segment_secs: 2,
        stalled_after: STALLED_AFTER,
        gpu_filters: false,
        tone_map: spoolframe_media::encoders::ToneMapSetting::Auto,
        video_codecs: crate::config::DEFAULT_VIDEO_CODECS.to_vec(),
    };
    Jobs::new(limits, Tool::unconfined("ffmpeg")).unwrap();
    assert_eq!(std::fs::read(&key).unwrap(), b"secret");
    assert_eq!(std::fs::read(&text).unwrap(), b"WEBVTT");
}
