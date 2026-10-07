//! The encoders this server can use (P2-02): detected at start and again
//! on SIGHUP, read by the planner.

use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::Arc;

use spoolframe_media::encoders::{self, Encoders, Ffmpeg, TranscodeSettings};

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
            if found.unusable.contains(hardware) {
                tracing::warn!(
                    ?hardware,
                    "transcode.hardware names an encoder family that does not work here"
                );
            }
        }
        *self.current.write() = Arc::new(found);
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
