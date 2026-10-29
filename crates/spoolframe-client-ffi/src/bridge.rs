//! The loopback bridge a URL player reads from (the `hls-bridge`
//! feature): an HTTP/1.1 server on 127.0.0.1 that answers each request
//! with SDK reads. The serving is `spoolframe-hls-bridge` (the same crate
//! the server serves AirPlay from); this keeps the app's side: one bridge
//! for the app, its listener on 127.0.0.1, and a new listener when the
//! system takes a suspended app's socket back.
//!
//! The playbacks it serves are the registry in `resources.rs`, which
//! reads them by name without HTTP too. An open playback is served under
//! its own token,
//! `http://127.0.0.1:<port>/<token>/…`: `file` for direct play, or the
//! server's HLS names (`master.m3u8`, `media.m3u8`, segments, subtitles).
//! Only this machine can connect, and only with a playback's token.

use std::sync::atomic::Ordering;
use std::time::Duration;

use spoolframe_client_sdk::types::MediaId;
use spoolframe_client_sdk::{PlaybackQuality, Selection};
use tokio::net::TcpListener;

use crate::error::SpoolError;
use crate::playback::{PlaybackStart, RegisteredPlayback, open, registered};
use crate::resources::bridge;
use crate::runtime::run;
use crate::session::SpoolSession;
use crate::values::ScreenCapabilities;

/// The listening socket's port, and its accept loop.
static LISTENER: tokio::sync::Mutex<Option<(u16, tokio::task::AbortHandle)>> =
    tokio::sync::Mutex::const_new(None);

#[uniffi::export]
impl SpoolSession {
    /// Start playing `id` for a URL player: `open_playback`, served on
    /// the loopback bridge. `end_playback` with its token when done.
    pub async fn play(
        &self,
        id: MediaId,
        start: Option<Duration>,
        tracks: Option<Selection>,
        quality: PlaybackQuality,
        display: Option<ScreenCapabilities>,
    ) -> Result<PlaybackStart, SpoolError> {
        let session = self.session.clone();
        run(async move {
            let opened = open(session, id, start, tracks, quality, display).await?;
            serve_opened(opened).await
        })
        .await
        .map(RegisteredPlayback::take)
    }

    /// The URL the open playback `token` plays at on the loopback bridge,
    /// serving it there first if it isn't yet.
    pub async fn playback_url(&self, token: String) -> Result<String, SpoolError> {
        run(async move { serve(&token).await }).await
    }
}

/// `opened` served on the bridge, its URL set; on failure its playback
/// is closed, and dropping it closes the token.
pub(crate) async fn serve_opened(
    mut opened: RegisteredPlayback,
) -> Result<RegisteredPlayback, SpoolError> {
    let start = opened.0.as_mut().expect("owned result");
    match serve(&start.token).await {
        Ok(url) => {
            start.url = Some(url);
            Ok(opened)
        }
        Err(e) => {
            if let Some(playback) = registered(&start.token) {
                let _ = playback.managed.close().await;
            }
            Err(e)
        }
    }
}

/// Serve the open playback `token` on the bridge, as its plan says (the
/// file, or the server's HLS): its URL.
async fn serve(token: &str) -> Result<String, SpoolError> {
    let Some(playback) = registered(token) else {
        return Err(SpoolError::PlaybackExpired);
    };
    let Some(stream) = &playback.stream else {
        return Err(SpoolError::CannotPlay {
            reasons: vec![format!(
                "this device can't take the server's plan: {:?}",
                playback.plan
            )],
        });
    };
    let base = base().await.map_err(|e| SpoolError::Bridge {
        message: e.to_string(),
    })?;
    // Registered with the bridge when it was opened: served now.
    playback.served.store(true, Ordering::Relaxed);
    let entry = stream.kind().entry();
    Ok(format!("{base}/{token}/{entry}"))
}

/// The bridge's base URL, starting it on first use. Runs on the core's
/// runtime. iOS takes a suspended app's listening sockets back (the
/// screen locked while paused, another app in front): one that no longer
/// answers is replaced by a new one, on a new port, serving the same
/// playbacks.
pub(crate) async fn base() -> std::io::Result<String> {
    let mut listening = LISTENER.lock().await;
    if let Some((port, task)) = listening.as_ref() {
        if !task.is_finished() && answers(*port).await {
            return Ok(format!("http://127.0.0.1:{port}"));
        }
        tracing::info!(port, "bridge stopped listening: starting it again");
        task.abort();
    }
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let task = tokio::spawn(async move { bridge().serve(listener).await }).abort_handle();
    *listening = Some((port, task));
    Ok(format!("http://127.0.0.1:{port}"))
}

/// Whether the bridge's socket on `port` still takes connections.
async fn answers(port: u16) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_millis(500),
            tokio::net::TcpStream::connect(("127.0.0.1", port))
        )
        .await,
        Ok(Ok(_))
    )
}
