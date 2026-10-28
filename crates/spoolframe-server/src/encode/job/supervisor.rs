//! The task that watches one job's ffmpeg (P2-03): its progress, its
//! reader, the scratch space, a stall, and a run again in software.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::watch;

use spoolframe_media::encoders::VideoEncoder;
use spoolframe_media::sandbox::{Jail, Tool};

use crate::metrics::Histograms;

use super::process::{exit_blames_gpu, out_time_us, signal};
use super::*;

/// What the reader side and the supervisor tell each other.
#[derive(Debug)]
pub(super) struct Shared {
    /// ffmpeg's: its process group's. Another once it runs again
    /// decoding in software.
    pub(super) pid: AtomicU32,
    /// The program and its arguments, as run.
    pub(super) command: parking_lot::Mutex<Vec<String>>,
    /// ffmpeg decodes on the GPU.
    pub(super) hardware_decode: AtomicBool,
    /// How the ffmpeg running now makes its video (PX-24): the fallback's
    /// once it runs.
    pub(super) made: parking_lot::Mutex<Option<Made>>,
    pub(super) furthest_bytes: AtomicU64,
    /// For HLS: the media time of the segment asked for last.
    pub(super) reader_ms: AtomicU64,
    /// For HLS: the number of the segment asked for last. Not the furthest:
    /// a reader that seeks back to a segment still on disk reads on from
    /// there.
    pub(super) reader_segment: AtomicU32,
    /// For HLS: the first segment still on disk. Those before it were
    /// never written by this run, or removed to make room.
    pub(super) first_kept: AtomicU32,
    /// Held back for its reader: a watch, so binge-ahead (PX-37) hears
    /// when it catches up ([`EncodeJob::caught_up_signal`]).
    pub(super) paused: watch::Sender<bool>,
    /// What it has counted into [`Jobs::scratch_used`].
    pub(super) counted: AtomicU64,
    /// ffmpeg's last [`STDERR_KEPT`] lines of stderr.
    pub(super) stderr: parking_lot::Mutex<VecDeque<String>>,
    /// Its `conversion` span, while ffmpeg runs.
    pub(super) span: RunSpan,
    /// When it was spawned.
    pub(super) began: std::time::Instant,
    /// Since when it is held back for its reader, and for how long it was
    /// before: its speed is over the time it ran.
    pub(super) paused_for: parking_lot::Mutex<(Option<std::time::Instant>, Duration)>,
    /// It has written its first segment (or its first bytes).
    pub(super) first_ready: AtomicBool,
    pub(super) load: Load,
    /// Where its speed and its readers' waits go, if anywhere.
    pub(super) histograms: Option<Arc<Histograms>>,
    /// [`Shared::end`] has run.
    pub(super) ended: AtomicBool,
    /// The [`OtherEncoder::key`] its encode moved to, once it has.
    pub(super) moved_to: parking_lot::Mutex<Option<String>>,
}

impl Shared {
    /// Held back for its reader from now, or not.
    pub(super) fn set_paused(&self, paused: bool) {
        self.paused
            .send_if_modified(|was| std::mem::replace(was, paused) != paused);
        let mut paused_for = self.paused_for.lock();
        match (paused, paused_for.0) {
            (true, None) => paused_for.0 = Some(std::time::Instant::now()),
            (false, Some(since)) => *paused_for = (None, paused_for.1 + since.elapsed()),
            _ => {}
        }
    }

    /// The run ended, `outcome`, with `more` said of it: its span ends,
    /// with what it did, and its speed is observed; once, whoever ends it
    /// first (its supervisor, or a kill).
    ///
    /// Its speed is the media time it encoded from `start_ms` to
    /// `encoded_ms` over the time it ran, paused time left out: 2.0 is
    /// twice real time.
    pub(super) fn end(
        &self,
        outcome: &'static str,
        start_ms: u64,
        encoded_ms: u64,
        more: Option<opentelemetry::KeyValue>,
    ) {
        use opentelemetry::KeyValue;
        if self.ended.swap(true, Ordering::Relaxed) {
            return;
        }
        let paused = {
            let paused_for = self.paused_for.lock();
            paused_for.1 + paused_for.0.map_or(Duration::ZERO, |since| since.elapsed())
        };
        let running = self.began.elapsed().saturating_sub(paused);
        let encoded = encoded_ms.saturating_sub(start_ms);
        let mut ran = vec![
            KeyValue::new("conversion.encoded_ms", encoded as i64),
            KeyValue::new("conversion.running_ms", running.as_millis() as i64),
            KeyValue::new("conversion.paused_ms", paused.as_millis() as i64),
        ];
        if !running.is_zero() {
            let speed = encoded as f64 / running.as_millis().max(1) as f64;
            ran.push(KeyValue::new("conversion.speed", speed));
            // A run killed as it started (a seek straight after another)
            // says nothing of how fast it goes.
            if let Some(histograms) = &self.histograms
                && running >= Duration::from_secs(1)
            {
                histograms.observe(
                    &crate::metrics::histogram::CONVERSION_SPEED,
                    speed,
                    &[("kind", self.load.as_str())],
                );
            }
        }
        ran.extend(more);
        self.span.end(outcome, ran);
    }

    fn stderr_line(&self, line: String) {
        let mut kept = self.stderr.lock();
        if kept.len() == STDERR_KEPT {
            kept.pop_front();
        }
        kept.push_back(line);
    }

    /// The last [`STDERR_LINES`], for a failure's report.
    fn stderr_tail(&self) -> Vec<String> {
        let kept = self.stderr.lock();
        kept.iter()
            .skip(kept.len().saturating_sub(STDERR_LINES))
            .cloned()
            .collect()
    }
}

/// A run's `conversion` span: from ffmpeg's spawn to its exit, or its
/// kill. Ended there, whoever holds the job on: a playback holds its last
/// run until it ends, and Tempo gets a span only when it ends.
#[derive(Debug, Default)]
pub(super) struct RunSpan(pub(super) parking_lot::Mutex<Option<tracing::Span>>);

impl RunSpan {
    pub(super) fn event(&self, name: &'static str, attributes: Vec<opentelemetry::KeyValue>) {
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        if let Some(span) = &*self.0.lock() {
            span.add_event(name, attributes);
        }
    }

    fn set(&self, key: &'static str, value: impl Into<opentelemetry::Value>) {
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        if let Some(span) = &*self.0.lock() {
            span.set_attribute(key, value);
        }
    }

    /// End it, `outcome`, with `attributes`; once.
    fn end(&self, outcome: &'static str, attributes: Vec<opentelemetry::KeyValue>) {
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        let Some(span) = self.0.lock().take() else {
            return;
        };
        for attribute in attributes {
            span.set_attribute(attribute.key, attribute.value);
        }
        span.set_attribute("outcome", outcome);
    }
}

pub(super) struct Supervisor {
    /// What ran ffmpeg, and its jail but for the GPU: for the run in
    /// software.
    pub(super) ffmpeg: Tool,
    pub(super) jail: Jail,
    /// The run decoding in software, while it has not run.
    pub(super) fallback: Option<Fallback>,
    /// The runs on other encoders of its codec, each with its ffmpeg,
    /// while they have not run.
    pub(super) others: VecDeque<(Tool, OtherEncoder)>,
    /// Where a run on another card takes its place.
    pub(super) budget: Arc<parking_lot::Mutex<BudgetUse>>,
    pub(super) released: Arc<tokio::sync::Notify>,
    pub(super) decode_failures: Arc<DecodeFailures>,
    /// Where the run started in the film, ms.
    pub(super) start_ms: u64,
    pub(super) dir: PathBuf,
    pub(super) written: Written,
    /// The jobs' directory: what all jobs have written.
    pub(super) scratch: PathBuf,
    pub(super) scratch_limit: u64,
    pub(super) scratch_check: Arc<tokio::sync::Mutex<()>>,
    pub(super) scratch_used: Arc<AtomicU64>,
    pub(super) read_ahead: Duration,
    pub(super) stalled_after: Duration,
    pub(super) shared: Arc<Shared>,
    /// The job's slot, let go of when a stalled ffmpeg is ended.
    pub(super) slot: Arc<parking_lot::Mutex<Option<Slot>>>,
    pub(super) progress: Arc<watch::Sender<Progress>>,
    pub(super) counters: Arc<JobCounters>,
    /// Counted in [`Encoding`] for as long as the supervisor runs: until
    /// ffmpeg exits, is ended as stalled, or the job is stopped (which
    /// aborts the supervisor).
    pub(super) _encoding: EncodingGuard,
}

/// How one ffmpeg ended.
pub(super) enum Ended {
    Exited(std::io::Result<std::process::ExitStatus>),
    /// No progress for `stalled_after`: killed.
    Stalled,
    /// The scratch directory is full: killed.
    ScratchFull,
    /// ffmpeg could not be run again, decoding in software.
    NotRelaunched,
}

/// Whether a run that `ended` so may have failed for its GPU encoder: as
/// [`decode_failed`], or ffmpeg ended by its own abort or a fault, as a
/// driver ends it when the video engine hangs (amdgpu's "the context is
/// lost").
pub(super) fn encode_failed(ended: &Ended) -> bool {
    use std::os::unix::process::ExitStatusExt;
    decode_failed(ended)
        || matches!(ended, Ended::Exited(Ok(status))
            if matches!(status.signal(), Some(libc::SIGABRT | libc::SIGSEGV | libc::SIGBUS)))
}

/// Whether a run that `ended` so may have failed for decoding on the GPU:
/// it exited non-zero, or stalled. A run ended by a signal (the kernel's
/// OOM killer, an operator, or the server stopping it) says nothing about
/// the GPU, and is not remembered against the file.
pub(super) fn decode_failed(ended: &Ended) -> bool {
    match ended {
        Ended::Exited(status) => exit_blames_gpu(status),
        Ended::Stalled => true,
        _ => false,
    }
}

impl Supervisor {
    pub(super) async fn run(
        mut self,
        child: tokio::process::Child,
        stdout: tokio::process::ChildStdout,
        stderr: tokio::process::ChildStderr,
    ) {
        // The furthest media time counted as output (P3-14): from where the
        // run started, never twice, whichever ffmpeg encoded it.
        let mut counted_ms = self.start_ms;
        let mut ended = self.attempt(child, stdout, stderr, &mut counted_ms).await;
        if decode_failed(&ended) && self.shared.hardware_decode.load(Ordering::Relaxed) {
            if let Some(fallback) = &self.fallback
                && self.decode_failures.remember(&fallback.file)
            {
                tracing::warn!(
                    file = %fallback.file.display(),
                    stderr = ?self.shared.stderr_tail(),
                    "decoding or filtering on the GPU failed; this file is decoded and \
                     filtered on the CPU from now on"
                );
            }
            // Before the first output nothing a reader has changes: the job
            // runs again from the start, decoding in software.
            if self.nothing_written().await
                && let Some(fallback) = self.fallback.take()
            {
                match self.relaunch(&fallback) {
                    Ok((child, stdout, stderr)) => {
                        ended = self.attempt(child, stdout, stderr, &mut counted_ms).await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "cannot run ffmpeg again, decoding in software");
                        ended = Ended::NotRelaunched;
                    }
                }
            }
        }
        // A GPU encode that failed before the first output moves to the
        // next encoder of its codec: another card's, then software's. A
        // driver's abort says nothing of the film, so it is not
        // remembered against it. Each runs once, then the job fails.
        while encode_failed(&ended)
            && let Some(from) = self.gpu_encoder()
            && self.nothing_written().await
            && let Some((ffmpeg, other, slot)) = self.next_encoder()
        {
            let to = other.made.as_ref().and_then(|m| m.encoder.as_ref());
            tracing::warn!(
                pid = self.pid(),
                from = from.name,
                to = to.map(|e| e.name.as_str()),
                stderr = ?self.shared.stderr_tail(),
                "a GPU encode failed: running again on another encoder"
            );
            // Its place in the budget is the new run's: on its card, of
            // its weight.
            *self.slot.lock() = Some(slot);
            match self.move_to(&ffmpeg, &other) {
                Ok((child, stdout, stderr)) => {
                    ended = self.attempt(child, stdout, stderr, &mut counted_ms).await;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "cannot run ffmpeg again, on another encoder");
                    ended = Ended::NotRelaunched;
                }
            }
        }
        self.conclude(ended);
    }

    /// The GPU video encoder the ffmpeg running now encodes with, if any.
    fn gpu_encoder(&self) -> Option<VideoEncoder> {
        self.shared
            .made
            .lock()
            .as_ref()
            .and_then(|m| m.encoder.clone())
            .filter(|e| e.hardware.is_some())
    }

    /// The next of [`Supervisor::others`] with a place in the budget, and
    /// that place (the failed run's counted as free); one with none is
    /// passed over, as a full card is when a conversion starts.
    fn next_encoder(&mut self) -> Option<(Tool, OtherEncoder, Slot)> {
        while let Some((ffmpeg, other)) = self.others.pop_front() {
            let replacing = self.slot.lock().clone();
            let place = Place::take(
                &self.budget,
                &self.released,
                other.load,
                other.card.clone(),
                replacing.as_ref(),
            );
            match place {
                Some(place) => {
                    let slot = Slot {
                        place: Arc::new(place),
                    };
                    return Some((ffmpeg, other, slot));
                }
                None => tracing::warn!(
                    to = other.key,
                    "no room to move a failed GPU encode to this encoder: passed over"
                ),
            }
        }
        None
    }

    /// Run ffmpeg again with `fallback`'s arguments, in its place.
    fn relaunch(
        &self,
        fallback: &Fallback,
    ) -> std::io::Result<(
        tokio::process::Child,
        tokio::process::ChildStdout,
        tokio::process::ChildStderr,
    )> {
        let launched = self.launch_again(
            &self.ffmpeg,
            &self.jail.clone().gpu(fallback.gpu),
            &fallback.args,
            false,
            fallback.made.clone(),
            "(the GPU failed: running again, decoding and filtering on the CPU)",
        )?;
        self.shared.span.event("software decode", Vec::new());
        self.shared.span.set("conversion.decode", "sw");
        tracing::debug!(pid = self.pid(), dir = %self.dir.display(), "encode running again, decoding in software");
        Ok(launched)
    }

    /// Run ffmpeg again as `other`, on its encoder, in its place.
    fn move_to(
        &self,
        ffmpeg: &Tool,
        other: &OtherEncoder,
    ) -> std::io::Result<(
        tokio::process::Child,
        tokio::process::ChildStdout,
        tokio::process::ChildStderr,
    )> {
        use crate::encode::Decoder;
        let made = other.made.as_ref();
        let encoder = made.and_then(|m| m.encoder.as_ref());
        let launched = self.launch_again(
            ffmpeg,
            &self
                .jail
                .clone()
                .gpu(other.gpu)
                .vulkan(other.vulkan.clone()),
            &other.args,
            made.is_some_and(|m| matches!(m.decoder, Decoder::Hardware(_) | Decoder::Vulkan)),
            other.made.clone(),
            &format!(
                "(the GPU encode failed: running again on {})",
                encoder.map_or("another encoder", |e| e.name.as_str())
            ),
        )?;
        *self.shared.moved_to.lock() = Some(other.key.clone());
        self.shared.span.event(
            "encoder moved",
            vec![opentelemetry::KeyValue::new("to", other.key.clone())],
        );
        if let Some(encoder) = encoder {
            self.shared
                .span
                .set("conversion.encoder", encoder.name.clone());
            let hardware = if encoder.hardware.is_some() {
                "gpu"
            } else {
                "cpu"
            };
            self.shared.span.set("conversion.hardware", hardware);
        }
        Ok(launched)
    }

    /// ffmpeg run again with `args` in `jail`, in place of the one that
    /// failed: the job's pid, command, and what it makes are the new
    /// run's, and `said` goes in its stderr between the two.
    fn launch_again(
        &self,
        ffmpeg: &Tool,
        jail: &Jail,
        args: &[String],
        hardware_decode: bool,
        made: Option<Made>,
        said: &str,
    ) -> std::io::Result<(
        tokio::process::Child,
        tokio::process::ChildStdout,
        tokio::process::ChildStderr,
    )> {
        let mut child = launch(ffmpeg, jail, args)?;
        let pid = child.id().unwrap_or(0);
        self.shared.pid.store(pid, Ordering::Relaxed);
        self.shared
            .hardware_decode
            .store(hardware_decode, Ordering::Relaxed);
        *self.shared.made.lock() = made;
        self.shared.set_paused(false);
        let mut command = vec![ffmpeg.program().display().to_string()];
        command.extend(args.iter().cloned());
        *self.shared.command.lock() = command;
        self.shared.stderr_line(said.into());
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        Ok((child, stdout, stderr))
    }

    /// Whether the run has written nothing a reader could have: no
    /// finished segment, or not a byte of the file.
    async fn nothing_written(&self) -> bool {
        match self.measure().await {
            (_, Some(_)) => false,
            (bytes, None) => self.written == Written::Hls || bytes == 0,
        }
    }

    /// How the job ended, said once ffmpeg will not run again; its span
    /// ends with it.
    fn conclude(&self, ended: Ended) {
        let outcome = match &ended {
            Ended::Exited(Ok(status)) => {
                use std::os::unix::process::ExitStatusExt;
                let mut exited = Vec::new();
                if let Some(code) = status.code() {
                    exited.push(opentelemetry::KeyValue::new("exit_code", i64::from(code)));
                    self.shared
                        .span
                        .set("conversion.exit_code", i64::from(code));
                }
                if let Some(signal) = status.signal() {
                    exited.push(opentelemetry::KeyValue::new("signal", i64::from(signal)));
                }
                self.shared.span.event("exited", exited);
                if status.success() { "ok" } else { "failed" }
            }
            Ended::Exited(Err(_)) => "failed",
            Ended::Stalled => "stalled",
            Ended::ScratchFull => "scratch_full",
            Ended::NotRelaunched => "not_relaunched",
        };
        let encoded_ms = self.progress.borrow().encoded_ms;
        self.shared.end(outcome, self.start_ms, encoded_ms, None);
        match ended {
            Ended::Exited(Ok(status)) if status.success() => self.finish(Ok(())),
            Ended::Exited(Ok(status)) => {
                // A failed run holds nothing: its place in the budget goes
                // to the next job, not to the playback until it is dropped.
                self.slot.lock().take();
                let tail = self.shared.stderr_tail();
                let reason = tail
                    .iter()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .cloned()
                    .unwrap_or_else(|| format!("ffmpeg exited with {status}"));
                tracing::warn!(pid = self.pid(), %status, stderr = ?tail, "an encode failed");
                self.finish(Err(Failure {
                    reason,
                    from_ffmpeg: true,
                }));
            }
            Ended::Exited(Err(e)) => {
                self.slot.lock().take();
                self.finish(Err(Failure::server(format!("cannot wait for ffmpeg: {e}"))))
            }
            Ended::NotRelaunched => {
                self.slot.lock().take();
                self.finish(Err(Failure::server(SPAWN_FAILED)));
            }
            Ended::Stalled => {
                self.slot.lock().take();
                self.finish(Err(Failure::server(format!(
                    "the conversion stalled: ffmpeg made no progress for {:?}",
                    self.stalled_after
                ))));
            }
            Ended::ScratchFull => {
                self.slot.lock().take();
                // Of no use now, and in the way of the other jobs for as
                // long as the playback lives.
                if let Err(e) = std::fs::remove_dir_all(&self.dir) {
                    tracing::warn!(dir = %self.dir.display(), error = %e, "cannot remove an encode's files");
                }
                self.finish(Err(Failure::server(
                    "the server's scratch space for conversions is full",
                )));
            }
        }
    }

    fn pid(&self) -> u32 {
        self.shared.pid.load(Ordering::Relaxed)
    }

    /// Say on the span, once, when the run has something for its reader:
    /// its first segment finished, or its first bytes.
    fn first_ready(&self, bytes: u64, last_segment: Option<u32>) {
        let ready = match self.written {
            Written::Hls => last_segment.is_some(),
            Written::Progressive { .. } => bytes > 0,
        };
        if ready && !self.shared.first_ready.swap(true, Ordering::Relaxed) {
            self.shared.span.event(
                "first segment ready",
                vec![opentelemetry::KeyValue::new(
                    "after_ms",
                    self.shared.began.elapsed().as_millis() as i64,
                )],
            );
        }
    }

    /// Watch one ffmpeg until it ends, or is ended.
    async fn attempt(
        &self,
        mut child: tokio::process::Child,
        stdout: tokio::process::ChildStdout,
        stderr: tokio::process::ChildStderr,
        counted_ms: &mut u64,
    ) -> Ended {
        let mut out = BufReader::new(stdout).lines();
        let mut err = BufReader::new(stderr).lines();
        let mut encoded_ms = 0u64;
        // What the job had written when it last looked.
        let mut seen = 0u64;
        // When it last walked the scratch directory: not yet, so its first
        // write does, as every write did before (H2).
        let mut walked = tokio::time::Instant::now()
            .checked_sub(WALK_EVERY)
            .unwrap_or_else(tokio::time::Instant::now);
        // What it had written and encoded when it last got further, and
        // when that was.
        let mut reached = (0u64, 0u64);
        let mut got_further = tokio::time::Instant::now();
        // With `-copyts` a run started at a seek reports the film's own
        // time, from where it started.
        let base_ms = self.start_ms;
        let mut tick = tokio::time::interval(POLL);
        let mut out_open = true;
        let mut err_open = true;
        let status = loop {
            tokio::select! {
                line = out.next_line(), if out_open => match line {
                    Ok(Some(line)) => self.progress_line(&line, &mut encoded_ms, counted_ms),
                    _ => out_open = false,
                },
                line = err.next_line(), if err_open => match line {
                    Ok(Some(line)) => self.shared.stderr_line(line),
                    _ => err_open = false,
                },
                status = child.wait() => break status,
                _ = tick.tick() => {
                    let (bytes, last_segment, output) = self.measure_and_count(encoded_ms).await;
                    self.count(bytes);
                    self.first_ready(bytes, last_segment);
                    self.throttle(encoded_ms, base_ms, bytes, last_segment);
                    self.progress.send_modify(|p| {
                        p.bytes = bytes;
                        p.encoded_ms = encoded_ms;
                        p.last_segment = last_segment;
                        p.output = output;
                    });
                    // ffmpeg can wait for ever with nothing wrong that it
                    // knows of. One held back for its reader is not
                    // waiting: its time starts again when it goes on.
                    if reached != (bytes, encoded_ms) || *self.shared.paused.borrow() {
                        reached = (bytes, encoded_ms);
                        got_further = tokio::time::Instant::now();
                    } else if got_further.elapsed() >= self.stalled_after {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        tracing::warn!(
                            pid = self.pid(),
                            after = ?self.stalled_after,
                            bytes,
                            encoded_ms,
                            stderr = ?self.shared.stderr_tail(),
                            "an encode stalled"
                        );
                        self.counters.stalled.inc();
                        self.shared.span.event(
                            "stalled",
                            vec![opentelemetry::KeyValue::new(
                                "after_ms",
                                self.stalled_after.as_millis() as i64,
                            )],
                        );
                        return Ended::Stalled;
                    }
                    // Only a job that wrote since it last looked can be the
                    // one that fills the scratch directory: a paused one
                    // is left alone, whoever fills it.
                    // The whole directory is walked only when what the
                    // jobs count says it may be full, or now and then to
                    // see what they do not count (H2): it was walked by
                    // every job that wrote, every tick.
                    if bytes > seen
                        && (self.scratch_used.load(Ordering::Relaxed) > self.scratch_limit
                            || walked.elapsed() >= WALK_EVERY)
                    {
                        let _one_job_at_a_time = self.scratch_check.lock().await;
                        walked = tokio::time::Instant::now();
                        // A job just held back for its reader writes no
                        // more: what it cannot free now it can once the
                        // reader has moved on and it writes again.
                        // Nor is an HLS run before its first segment: it has
                        // nothing yet for its reader, whose wait is what
                        // frees the space (the segment muxer writes its
                        // init at once; the HLS muxer wrote nothing until
                        // then).
                        let begun = matches!(self.written, Written::Progressive { .. })
                            || last_segment.is_some();
                        if !self.make_room().await
                            && !*self.shared.paused.borrow()
                            && begun
                        {
                            let _ = child.start_kill();
                            let _ = child.wait().await;
                            return Ended::ScratchFull;
                        }
                        let measured = self.measure().await.0;
                        self.count(measured);
                        seen = measured;
                    } else if bytes > seen {
                        seen = bytes;
                    }
                }
            }
        };
        // What is left of its progress (a run that ends at once may have
        // written all of it unread), and of stderr for the report.
        while out_open && let Ok(Some(line)) = out.next_line().await {
            self.progress_line(&line, &mut encoded_ms, counted_ms);
        }
        while let Ok(Some(line)) = err.next_line().await {
            self.shared.stderr_line(line);
        }
        let encoded_ms = self.progress.borrow().encoded_ms.max(encoded_ms);
        let (bytes, last_segment, output) = self.measure_and_count(encoded_ms).await;
        self.count(bytes);
        // A run that wrote all it had before it was first looked at.
        self.first_ready(bytes, last_segment);
        self.progress.send_modify(|p| {
            p.bytes = bytes;
            p.last_segment = last_segment;
            p.encoded_ms = encoded_ms;
            p.output = output;
        });
        Ended::Exited(status)
    }

    /// A line of ffmpeg's `-progress`: key=value; out_time_us is the
    /// encoded media time (out_time_ms is µs too), counted as output past
    /// `counted_ms` (P3-14).
    fn progress_line(&self, line: &str, encoded_ms: &mut u64, counted_ms: &mut u64) {
        if let Some(us) = out_time_us(line) {
            *encoded_ms = us / 1000;
            if *encoded_ms > *counted_ms {
                self.counters.output_ms.add(*encoded_ms - *counted_ms);
                *counted_ms = *encoded_ms;
            }
        }
    }

    fn finish(&self, done: Result<(), Failure>) {
        self.shared.set_paused(false);
        self.progress.send_modify(|p| p.done = Some(done));
    }

    /// Whether the scratch directory is within its limit. When it is not,
    /// an HLS job first removes its own segments behind its reader, the
    /// oldest first, until it is (P2-03). The segment before the one
    /// asked for last stays: a reader may still be fetching it.
    ///
    /// On the blocking pool: it walks every job's files (H2).
    async fn make_room(&self) -> bool {
        let (scratch, limit, hls, shared, dir) = (
            self.scratch.clone(),
            self.scratch_limit,
            self.written == Written::Hls,
            self.shared.clone(),
            self.dir.clone(),
        );
        tokio::task::spawn_blocking(move || make_room(&scratch, limit, hls, &shared, &dir))
            .await
            .unwrap_or(true)
    }

    /// Bytes written, and for HLS the highest segment on disk: read on the
    /// blocking pool, not this worker (H2).
    async fn measure(&self) -> (u64, Option<u32>) {
        let (written, dir) = (self.written.clone(), self.dir.clone());
        tokio::task::spawn_blocking(move || measure(&written, &dir))
            .await
            .unwrap_or((0, None))
    }

    /// `bytes`, just measured, counted into [`Jobs::scratch_used`] in place
    /// of what it counted before.
    fn count(&self, bytes: u64) {
        let before = self.shared.counted.swap(bytes, Ordering::Relaxed);
        if bytes >= before {
            self.scratch_used
                .fetch_add(bytes - before, Ordering::Relaxed);
        } else {
            uncount(&self.scratch_used, before - bytes);
        }
    }

    /// [`Self::measure`], and [`Progress::output`] now with `encoded_ms`
    /// reported: for HLS, the segments finished since it last counted
    /// added in. One read of the playlist on the blocking pool serves both
    /// (H2): the segment count and the measure come from the same listing.
    async fn measure_and_count(&self, encoded_ms: u64) -> (u64, Option<u32>, Measured) {
        let mut output = self.progress.borrow().output;
        let (written, dir, start_ms) = (self.written.clone(), self.dir.clone(), self.start_ms);
        tokio::task::spawn_blocking(move || {
            let (bytes, listed) = measure_listed(&written, &dir);
            let last = last_of(&listed);
            match written {
                Written::Progressive { .. } => {
                    output.bytes = bytes;
                    output.ms = encoded_ms.saturating_sub(start_ms);
                }
                Written::Hls => {
                    if last > output.through {
                        output.count_segments(&dir, &listed);
                    }
                }
            }
            (bytes, last, output)
        })
        .await
        .unwrap_or_else(|_| (0, None, self.progress.borrow().output))
    }

    /// Pause ffmpeg once it is `read_ahead` past its reader; resume it at
    /// half that. Never while the reader has nothing it can read: a held
    /// encode goes on only when its reader moves on, and a reader that
    /// waits for the encode cannot.
    fn throttle(&self, encoded_ms: u64, base_ms: u64, bytes: u64, last_segment: Option<u32>) {
        let reader_ms = reader_ms(
            &self.written,
            &self.shared,
            base_ms,
            encoded_ms,
            bytes,
            last_segment,
        );
        let ahead = Duration::from_millis(encoded_ms.saturating_sub(reader_ms));
        let paused = *self.shared.paused.borrow();
        if !paused && ahead > self.read_ahead {
            signal(self.pid(), libc::SIGSTOP);
            self.shared.set_paused(true);
            tracing::debug!(pid = self.pid(), ?ahead, "encode paused for its reader");
        } else if paused && ahead <= self.read_ahead / 2 {
            signal(self.pid(), libc::SIGCONT);
            self.shared.set_paused(false);
            tracing::debug!(pid = self.pid(), ?ahead, "encode resumed");
        }
    }
}

/// Where a job's reader is in the film, ms: what the throttle holds the
/// encode back against, and what playback debug (P3-15) shows as read.
pub(super) fn reader_ms(
    written: &Written,
    shared: &Shared,
    base_ms: u64,
    encoded_ms: u64,
    bytes: u64,
    last_segment: Option<u32>,
) -> u64 {
    match written {
        Written::Progressive { .. } => {
            let read = shared.furthest_bytes.load(Ordering::Relaxed);
            if read >= bytes {
                // It has all that is written, which is nothing until ffmpeg
                // has its first cluster whole: it is where the encode is.
                encoded_ms
            } else {
                // A byte offset as a share of what this run encoded: close
                // enough for a stream of roughly even bitrate.
                let run = encoded_ms.saturating_sub(base_ms);
                base_ms + (u128::from(run) * u128::from(read) / u128::from(bytes)) as u64
            }
        }
        Written::Hls => {
            let asked = shared.reader_segment.load(Ordering::Relaxed);
            if asked > 0 && last_segment.is_none_or(|last| last < asked) {
                // It waits for a segment ffmpeg has not finished, whatever
                // time ffmpeg reports.
                encoded_ms
            } else {
                shared.reader_ms.load(Ordering::Relaxed).max(base_ms)
            }
        }
    }
}

/// `bytes` taken off what the jobs counted, never below nothing.
pub(super) fn uncount(used: &AtomicU64, bytes: u64) {
    let _ = used.try_update(Ordering::Relaxed, Ordering::Relaxed, |now| {
        Some(now.saturating_sub(bytes))
    });
}

/// How often a job that writes walks the whole scratch directory though
/// what the jobs count says it has room: to see what they do not count
/// (the inputs' links, kept inits) and catch a count gone wrong.
const WALK_EVERY: Duration = Duration::from_secs(30);

/// Whether the scratch directory is within `limit`. When it is not, an
/// HLS job (`hls`) first removes its own segments in `dir` behind its
/// reader, the oldest first, until it is (P2-03). The segment before the
/// one asked for last stays: a reader may still be fetching it.
fn make_room(scratch: &Path, limit: u64, hls: bool, shared: &Shared, dir: &Path) -> bool {
    let mut total = dir_size(scratch);
    if total > limit && hls {
        let behind = shared
            .reader_segment
            .load(Ordering::Relaxed)
            .saturating_sub(1);
        let mut n = shared.first_kept.load(Ordering::Relaxed);
        while total > limit && n < behind {
            // Said first, so no reader takes the missing file for an
            // empty segment.
            shared.first_kept.store(n + 1, Ordering::Relaxed);
            let file = dir.join(format!("seg{n}.m4s"));
            let size = std::fs::metadata(&file).map_or(0, |m| m.len());
            if std::fs::remove_file(&file).is_ok() {
                total = total.saturating_sub(size);
            }
            n += 1;
        }
        tracing::debug!(
            pid = shared.pid.load(Ordering::Relaxed),
            first_kept = n,
            "segments behind the reader removed for room"
        );
    }
    total <= limit
}

/// Bytes `written` in `dir`, and for HLS the highest segment finished.
pub(super) fn measure(written: &Written, dir: &Path) -> (u64, Option<u32>) {
    let (bytes, listed) = measure_listed(written, dir);
    (bytes, last_of(&listed))
}

/// Bytes `written` in `dir`, and for HLS each segment ffmpeg's playlist
/// lists as finished, with its duration (ms).
fn measure_listed(written: &Written, dir: &Path) -> (u64, Vec<(u32, u64)>) {
    match written {
        Written::Progressive { file } => {
            (std::fs::metadata(file).map_or(0, |m| m.len()), Vec::new())
        }
        Written::Hls => {
            let bytes = std::fs::read_dir(dir).map_or(0, |entries| {
                entries
                    .flatten()
                    .map(|e| e.metadata().map_or(0, |m| m.len()))
                    .sum()
            });
            // Not the segment files: the HLS muxer writes a segment's file
            // only as it closes it, the segment muxer as it goes, so a
            // file that exists may still be half written. Either's list is
            // replaced (written aside, then renamed) right after each
            // segment is closed.
            let listed = std::fs::read_to_string(dir.join(PLAYLIST))
                .map(|text| listed_segments(&text))
                .unwrap_or_default();
            (bytes, listed)
        }
    }
}

/// The bytes under `dir`, one level of subdirectories deep per handle and
/// epoch: `<jobs>/<handle>/<epoch>/…`.
fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_size(&e.path()),
            Ok(m) if m.is_file() => m.len(),
            _ => 0,
        })
        .sum()
}

/// Each `seg<n>.m4s` an ffmpeg HLS playlist lists, with the duration its
/// `#EXTINF` gives it, ms.
pub(in crate::encode) fn listed_segments(playlist: &str) -> Vec<(u32, u64)> {
    let mut listed = Vec::new();
    let mut duration = None;
    for line in playlist.lines() {
        if let Some(info) = line.strip_prefix("#EXTINF:") {
            duration = info
                .split(',')
                .next()
                .and_then(|secs| secs.trim().parse::<f64>().ok())
                .map(|secs| (secs * 1000.0).round() as u64);
        } else if let Some(n) = line
            .strip_prefix("seg")
            .and_then(|rest| rest.strip_suffix(".m4s"))
            .and_then(|n| n.parse().ok())
        {
            listed.push((n, duration.take().unwrap_or(0)));
        }
    }
    listed
}

/// The highest segment of those `listed`.
pub(super) fn last_of(listed: &[(u32, u64)]) -> Option<u32> {
    listed.iter().map(|&(n, _)| n).max()
}
