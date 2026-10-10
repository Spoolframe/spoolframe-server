//! The encoders this server can use (P2-02): detected at start and again
//! on SIGHUP, read by the planner.

use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::Arc;

use spoolframe_media::encoders::{self, Encoders, Ffmpeg, Hardware, TranscodeSettings};

pub struct Transcoding {
    ffmpeg: Box<dyn Ffmpeg>,
    settings: TranscodeSettings,
    /// Where detection writes its test samples.
    scratch: PathBuf,
    current: RwLock<Arc<Encoders>>,
}

impl std::fmt::Debug for Transcoding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transcoding")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl Transcoding {
    /// Software only until [`Transcoding::detect`] has run.
    pub fn new(ffmpeg: Box<dyn Ffmpeg>, settings: TranscodeSettings, scratch: PathBuf) -> Self {
        Self {
            ffmpeg,
            settings,
            scratch,
            current: RwLock::new(Arc::new(Encoders::software_baseline())),
        }
    }

    pub fn settings(&self) -> &TranscodeSettings {
        &self.settings
    }

    pub fn encoders(&self) -> Arc<Encoders> {
        self.current.read().clone()
    }

    /// Find what works now, and use it from here on.
    pub async fn detect(&self) {
        let found = encoders::detect(self.ffmpeg.as_ref(), &self.settings, &self.scratch).await;
        tracing::info!(encoders = %found.summary(), "transcoding");
        for hardware in self.settings.hardware.named() {
            log_named(&found, *hardware);
        }
        *self.current.write() = Arc::new(found);
    }
}

/// What detection found of a family `transcode.hardware` names (PX-28):
/// a warning when none of its encoders works, with the first failed
/// test's stderr, the operator having asked for a GPU that is not
/// there; else, if it lacks something, one line saying what: a GTX 1060
/// lacking AV1 is how it is, and the job goes to a family that has it.
/// Each failed test's stderr is at debug (`encoders::detect`).
fn log_named(found: &Encoders, hardware: Hardware) {
    let first = found.failed.iter().find(|f| f.hardware == hardware);
    if found.unusable.contains(&hardware) {
        tracing::warn!(
            ?hardware,
            test = first.map(|f| f.test.as_str()),
            stderr = first.map(|f| f.stderr.as_str()),
            "transcode.hardware names an encoder family that does not work here"
        );
        return;
    }
    let lacks = found.lacks(hardware);
    if !lacks.is_empty() {
        tracing::info!(
            ?hardware,
            "{} lacks: {}",
            hardware.as_str(),
            lacks.join(", ")
        );
    }
}

/// Detect now, then again on every SIGHUP, in the background.
pub fn spawn_detection(transcoding: Arc<Transcoding>) {
    tokio::spawn(async move {
        transcoding.detect().await;
        let Ok(mut hangup) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        else {
            tracing::warn!("cannot listen for SIGHUP; encoders are detected only at start");
            return;
        };
        while hangup.recv().await.is_some() {
            tracing::info!("SIGHUP: detecting encoders again");
            transcoding.detect().await;
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use spoolframe_admin_api::{LogLevel, LogQuery};
    use spoolframe_media::encoders::{FfmpegCommand, HardwarePolicy};
    use spoolframe_media::sandbox::Tool;
    use tracing_subscriber::layer::SubscriberExt as _;

    use super::*;
    use crate::logs::{LogRing, RingLayer};

    /// A stand-in ffmpeg that lists NVENC and libx264, encodes with
    /// libx264, and fails with NVENC as it does without its driver.
    const NO_CUDA: &str = "#!/bin/sh\n\
         case \"$*\" in\n\
         *-encoders*) printf ' ------\\n V....D h264_nvenc NVIDIA\\n V....D libx264 x264\\n' ;;\n\
         *h264_nvenc*) echo 'Input #0, lavfi' >&2\n\
         \x20 echo '[h264_nvenc @ 0x1] Cannot load libcuda.so.1' >&2; exit 1 ;;\n\
         esac\n";

    /// A stand-in ffmpeg for a GTX 1060: NVENC encodes H.264, not AV1.
    const PASCAL: &str = "#!/bin/sh\n\
         case \"$*\" in\n\
         *-encoders*) printf ' ------\\n V....D h264_nvenc NVIDIA\\n V....D av1_nvenc NVIDIA\\n V....D libx264 x264\\n' ;;\n\
         *av1_nvenc*) echo '[av1_nvenc @ 0x1] No capable devices found' >&2; exit 1 ;;\n\
         esac\n";

    /// The lines at `level` or worse that mention `text`.
    fn lines(ring: &LogRing, level: LogLevel, text: &str) -> Vec<String> {
        ring.page(&LogQuery {
            level: Some(level),
            text: Some(text.into()),
            ..LogQuery::default()
        })
        .lines
        .into_iter()
        .map(|line| line.text)
        .collect()
    }

    /// The lines at `level` or worse that mention libcuda.
    fn about_cuda(ring: &LogRing, level: LogLevel) -> Vec<String> {
        lines(ring, level, "libcuda")
    }

    async fn detect_logging(name: &str, hardware: HardwarePolicy) -> Arc<LogRing> {
        detect_with(name, hardware, NO_CUDA).await
    }

    /// Detection with the stand-in ffmpeg `script`, and what it logged.
    async fn detect_with(name: &str, hardware: HardwarePolicy, script: &str) -> Arc<LogRing> {
        let dir = spoolframe_test_dir::TestDir::new(name).await;
        let ffmpeg_path = dir.join("ffmpeg");
        crate::test_support::write_script(&ffmpeg_path, script);
        let ffmpeg = FfmpegCommand::new(Tool::unconfined(ffmpeg_path));
        let ring = Arc::new(LogRing::default());
        let _logged = tracing::subscriber::set_default(
            tracing_subscriber::registry().with(RingLayer(ring.clone())),
        );
        let transcoding = Transcoding::new(
            Box::new(ffmpeg),
            TranscodeSettings {
                hardware,
                ..TranscodeSettings::default()
            },
            dir.join("scratch"),
        );
        transcoding.detect().await;
        ring
    }

    /// NVENC named in transcode.hardware with no encoder that works: one
    /// warning, with what its first failed test's ffmpeg said last.
    #[tokio::test]
    async fn a_named_family_s_failed_test_is_a_warning_with_ffmpeg_s_last_words() {
        let ring = detect_logging(
            "spoolframe-transcode-named",
            HardwarePolicy::Ordered(vec![Hardware::Nvenc]),
        )
        .await;
        let warned = about_cuda(&ring, LogLevel::Warn);
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert!(warned[0].contains("test=\"h264_nvenc\""), "{}", warned[0]);
        assert!(
            warned[0].contains(
                "exit status: 1: Input #0, lavfi / [h264_nvenc @ 0x1] Cannot load libcuda.so.1"
            ),
            "{}",
            warned[0]
        );
    }

    /// With `auto` the same failure is only for debugging: most hosts lack
    /// some family.
    #[tokio::test]
    async fn an_auto_family_s_failed_test_is_logged_at_debug() {
        let ring = detect_logging("spoolframe-transcode-auto", HardwarePolicy::Auto).await;
        assert!(about_cuda(&ring, LogLevel::Warn).is_empty());
        let debug = about_cuda(&ring, LogLevel::Debug);
        assert_eq!(debug.len(), 1, "{debug:?}");
        assert!(
            debug[0].contains("Cannot load libcuda.so.1"),
            "{}",
            debug[0]
        );
    }

    /// PX-28: a named family that works but lacks a codec is not a
    /// warning: one line says what it lacks, and each failed test's
    /// stderr is at debug.
    #[tokio::test]
    async fn a_named_family_lacking_a_codec_says_so_once_not_as_a_warning() {
        let ring = detect_with(
            "spoolframe-transcode-lacks",
            HardwarePolicy::Ordered(vec![Hardware::Nvenc]),
            PASCAL,
        )
        .await;
        assert_eq!(lines(&ring, LogLevel::Warn, ""), Vec::<String>::new());
        let lacks = lines(&ring, LogLevel::Info, "lacks");
        assert_eq!(lacks.len(), 1, "{lacks:?}");
        assert!(lacks[0].contains("nvenc lacks: av1 encode"), "{}", lacks[0]);
        let why = lines(&ring, LogLevel::Debug, "No capable devices");
        assert_eq!(why.len(), 1, "{why:?}");
        assert!(why[0].contains("test=\"av1_nvenc\""), "{}", why[0]);
    }

    /// Under `auto`, what a family lacks stays at debug.
    #[tokio::test]
    async fn an_auto_family_lacking_a_codec_says_nothing_above_debug() {
        let ring = detect_with(
            "spoolframe-transcode-lacks-auto",
            HardwarePolicy::Auto,
            PASCAL,
        )
        .await;
        assert_eq!(lines(&ring, LogLevel::Info, "lacks"), Vec::<String>::new());
        assert_eq!(lines(&ring, LogLevel::Debug, "No capable devices").len(), 1);
    }
}
