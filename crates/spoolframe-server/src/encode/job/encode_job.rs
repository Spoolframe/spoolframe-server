//! A running (or finished) ffmpeg, as its readers see it (P2-03).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::process::{remove_run_dir, signal};
use super::supervisor::{measure, reader_ms, uncount};
use super::*;

/// A running (or finished) ffmpeg.
#[derive(Debug)]
pub struct EncodeJob {
    pub epoch: u32,
    /// For HLS, the number of the first segment this run writes.
    pub first_segment: u32,
    /// See [`Run::aligned_until`]: the first of its segments not cut
    /// where the playlist says, `None` for all of them.
    pub aligned_until: Option<u32>,
    /// What it weighs against the encode budget.
    pub load: Load,
    pub(super) dir: PathBuf,
    pub(super) written: Written,
    /// Where the run started in the film, ms.
    pub(super) start_ms: u64,
    pub(super) progress: watch::Receiver<Progress>,
    /// The supervisor's, while it runs: what [`EncodeJob::end`] says the
    /// job ended with.
    pub(super) sender: std::sync::Weak<watch::Sender<Progress>>,
    pub(super) shared: Arc<Shared>,
    pub(super) task: JoinHandle<()>,
    /// Held while ffmpeg may run; released by [`EncodeJob::stop`], or by
    /// the job itself when it ends a stalled ffmpeg.
    pub(super) slot: Arc<parking_lot::Mutex<Option<Slot>>>,
    /// [`Jobs::scratch_used`]: what it counted there goes with its files.
    pub(super) scratch_used: Arc<AtomicU64>,
}

impl EncodeJob {
    /// Kill ffmpeg now and let go of its slot, though readers may still
    /// hold the job: one replaced at a seek must not encode on.
    pub fn stop(&self) {
        self.stop_because("ended");
    }

    /// [`EncodeJob::stop`], for `why` (`seek`, `server_stopping`): its
    /// span ends there, killed, if ffmpeg still ran.
    pub fn stop_because(&self, why: &'static str) {
        self.task.abort();
        let running = self.progress.borrow().done.is_none();
        if running {
            let pid = self.pid();
            if *self.shared.paused.borrow() {
                signal(pid, libc::SIGCONT);
            }
            signal(pid, libc::SIGKILL);
        }
        self.slot.lock().take();
        if running {
            let encoded_ms = self.progress.borrow().encoded_ms;
            self.shared.end(
                "killed",
                self.start_ms,
                encoded_ms,
                Some(opentelemetry::KeyValue::new("conversion.killed_by", why)),
            );
        }
    }

    /// A read waited `waited` for the encode to get where it asked, or
    /// found it there already (`None`): for the metrics, where a read
    /// that did not wait is 0.
    pub fn read_waited(&self, waited: Option<Duration>) {
        if let Some(histograms) = &self.shared.histograms {
            histograms.observe(
                &crate::metrics::histogram::SEGMENT_READ_WAIT,
                waited.map_or(0.0, |w| w.as_secs_f64()),
                &[],
            );
        }
    }

    /// Replaced by a run started `why` (`seek`, `switch`, `restart`) at
    /// `at_ms` in the film: said on its span, then stopped.
    pub fn replaced(&self, why: &'static str, at_ms: u64) {
        self.shared.span.event(
            "replaced",
            vec![
                opentelemetry::KeyValue::new("conversion.reason", why),
                opentelemetry::KeyValue::new("position_ms", at_ms as i64),
            ],
        );
        self.stop_because(why);
    }

    /// End the encode where it is (P3-15's End conversion): ffmpeg is
    /// killed and its slot let go of, and what it wrote is all there is,
    /// so a reader gets that and then the end, rather than waiting for
    /// more. Its files stay until the job is dropped. One that has ended
    /// already keeps how it ended.
    pub fn end(&self) {
        // Held across the abort, so the channel is still open to say it.
        let sender = self.sender.upgrade();
        self.stop();
        if let Some(sender) = sender {
            let (bytes, last_segment) = measure(&self.written, &self.dir);
            sender.send_if_modified(|p| {
                if p.done.is_some() {
                    return false;
                }
                p.bytes = p.bytes.max(bytes);
                p.last_segment = p.last_segment.max(last_segment);
                p.done = Some(Ok(()));
                true
            });
        }
        self.shared.set_paused(false);
    }

    /// Where its reader is in the film, ms, as the throttle reckons it.
    pub fn reader_ms(&self) -> u64 {
        let p = self.progress.borrow();
        reader_ms(
            &self.written,
            &self.shared,
            self.start_ms,
            p.encoded_ms,
            p.bytes,
            p.last_segment,
        )
    }

    /// The program and its arguments, as run (paths and all).
    pub fn command(&self) -> Vec<String> {
        self.shared.command.lock().clone()
    }

    /// ffmpeg's last lines of stderr, oldest first, at most
    /// [`STDERR_KEPT`].
    pub fn stderr(&self) -> Vec<String> {
        self.shared.stderr.lock().iter().cloned().collect()
    }

    /// The job's slot, for the job that replaces it; none once stopped.
    pub fn slot(&self) -> Option<Slot> {
        self.slot.lock().clone()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn pid(&self) -> u32 {
        self.shared.pid.load(Ordering::Relaxed)
    }

    pub fn progress(&self) -> watch::Receiver<Progress> {
        self.progress.clone()
    }

    /// How its ffmpeg makes the video (PX-24): the run decoding in
    /// software once it fell back to it.
    pub fn made(&self) -> Option<Made> {
        self.shared.made.lock().clone()
    }

    /// The [`OtherEncoder::key`] its encode moved to after its GPU encode
    /// failed, if it did: where its conversion's next runs go.
    pub fn moved_to(&self) -> Option<String> {
        self.shared.moved_to.lock().clone()
    }

    /// Whether its ffmpeg decodes on the GPU: false once it fell back to
    /// software.
    pub fn decodes_in_hardware(&self) -> bool {
        self.shared.hardware_decode.load(Ordering::Relaxed)
    }

    /// Whether the job is held back for its reader to catch up.
    pub fn paused(&self) -> bool {
        *self.shared.paused.borrow()
    }

    /// What tells when it catches up with its reader, without holding it.
    pub fn caught_up_signal(&self) -> CaughtUp {
        CaughtUp {
            paused: self.shared.paused.subscribe(),
            progress: self.progress(),
        }
    }

    /// A reader got as far as `bytes` into the progressive output.
    pub fn read_to(&self, bytes: u64) {
        self.shared
            .furthest_bytes
            .fetch_max(bytes, Ordering::Relaxed);
    }

    /// A reader asked for HLS segment `n`, which ends at `ms` (media
    /// time): that is where the reader is now.
    pub fn reader_at(&self, n: u32, ms: u64) {
        self.shared.reader_segment.store(n, Ordering::Relaxed);
        self.shared.reader_ms.store(ms, Ordering::Relaxed);
    }

    /// A reader waits for HLS segment `n`, which ends at `ms`, to be
    /// finished, without reading it (the init segment is ready with the
    /// run's first): the reader is there if it was not further already.
    pub fn reader_at_least(&self, n: u32, ms: u64) {
        self.shared.reader_segment.fetch_max(n, Ordering::Relaxed);
        self.shared.reader_ms.fetch_max(ms, Ordering::Relaxed);
    }

    /// For HLS, the segment its reader asked for last (0: none yet).
    pub fn reader_segment(&self) -> u32 {
        self.shared.reader_segment.load(Ordering::Relaxed)
    }

    /// For HLS, the last segment it has written, if any.
    pub fn written(&self) -> Option<u32> {
        self.progress.borrow().last_segment
    }

    /// For HLS, whether it has segment `n` on disk now: one it wrote and
    /// has not removed for the scratch space.
    pub fn has_segment(&self, n: u32) -> bool {
        n >= self.first_segment.max(self.first_kept()) && self.written().is_some_and(|w| n <= w)
    }

    /// For HLS, the first segment this run still has: the one it started
    /// at, or a later one once a full scratch directory made it remove
    /// those behind its reader.
    pub fn first_kept(&self) -> u32 {
        self.shared.first_kept.load(Ordering::Relaxed)
    }
}

impl Drop for EncodeJob {
    fn drop(&mut self) {
        // The task owns the child, which is killed when dropped; the signal
        // makes it now rather than when the runtime gets to the task.
        self.stop();
        // A run's directory holds up to about 1200 files, and a job is
        // dropped on async workers, often under a conversion's lock: the
        // removal runs on the blocking pool, not there (review 2026-10-11
        // M3). Outside a runtime (a test's end, shutdown) it runs here.
        let dir = std::mem::take(&mut self.dir);
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(move || remove_run_dir(&dir));
            }
            Err(_) => remove_run_dir(&dir),
        }
        uncount(
            &self.scratch_used,
            self.shared.counted.swap(0, Ordering::Relaxed),
        );
        tracing::debug!(pid = self.pid(), "encode ended");
    }
}

/// When a run catches up with its reader (PX-37's binge-ahead): it is
/// held back for it, or has written to the film's end. It does not keep
/// the run alive.
#[derive(Debug, Clone)]
pub struct CaughtUp {
    paused: watch::Receiver<bool>,
    progress: watch::Receiver<Progress>,
}

impl CaughtUp {
    /// Until the run has caught up: true; false once it is gone (stopped,
    /// replaced by a seek's) or failed.
    pub async fn wait(mut self) -> bool {
        tokio::select! {
            paused = self.paused.wait_for(|paused| *paused) => paused.is_ok(),
            done = self.progress.wait_for(|p| p.done.is_some()) => {
                done.is_ok_and(|p| matches!(p.done, Some(Ok(()))))
            }
        }
    }
}
