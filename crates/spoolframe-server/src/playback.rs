//! Playback sessions (P1-08): what a `StartPlayback` leaves behind, so
//! that range reads need no database lookups.
//!
//! A handle is bound to the session and the endpoint that started it. It
//! lives in memory; it holds no open file (each read opens the file), so an
//! idle handle costs a map entry until it expires.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use password_hash::rand_core::{OsRng, RngCore};
use spoolframe_protocol::{PlaybackHandle, SessionId};
use spoolframe_types::SubtitleFormat;

/// How long a handle lives without reads.
pub const IDLE: Duration = Duration::from_secs(5 * 60);
/// How often, while a handle is used, its session is checked again, so a
/// revoked session or a disabled user stops playback promptly.
pub const RECHECK: Duration = Duration::from_secs(30);

/// A selected external subtitle: the file and what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sidecar {
    pub path: PathBuf,
    pub format: SubtitleFormat,
}

/// One playback.
#[derive(Debug, Clone)]
pub struct Playback {
    pub session: SessionId,
    /// Hex `EndpointId`.
    pub endpoint: String,
    pub path: PathBuf,
    pub size: u64,
    pub subtitle: Option<Sidecar>,
    last_used: Instant,
    last_checked: Instant,
}

/// Why a handle cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Unknown, or idle past [`IDLE`].
    Expired,
    /// From another session or another endpoint.
    NotYours,
}

/// What a use of a handle needs to do.
#[derive(Debug, Clone)]
pub struct Use {
    pub playback: Playback,
    /// The session must be checked again before serving.
    pub recheck: bool,
}

#[derive(Debug)]
pub struct Playbacks {
    idle: Duration,
    recheck: Duration,
    handles: Mutex<HashMap<String, Playback>>,
}

impl Default for Playbacks {
    fn default() -> Self {
        Self::new(IDLE, RECHECK)
    }
}

impl Playbacks {
    pub fn new(idle: Duration, recheck: Duration) -> Self {
        Self {
            idle,
            recheck,
            handles: Mutex::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Playback>> {
        self.handles.lock().expect("never held across a panic")
    }

    /// Open a playback; the session was checked by the caller just now.
    pub fn start(
        &self,
        session: SessionId,
        endpoint: String,
        path: PathBuf,
        size: u64,
        subtitle: Option<Sidecar>,
    ) -> PlaybackHandle {
        let mut bytes = [0u8; 16];
        OsRng.fill_bytes(&mut bytes);
        let handle: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let now = Instant::now();
        let mut handles = self.lock();
        // Forget idle handles here rather than on a timer.
        handles.retain(|_, p| now.duration_since(p.last_used) < self.idle);
        handles.insert(
            handle.clone(),
            Playback {
                session,
                endpoint,
                path,
                size,
                subtitle,
                last_used: now,
                last_checked: now,
            },
        );
        PlaybackHandle(handle)
    }

    /// Use `handle` from `session` on `endpoint`: refused if it is not
    /// theirs or has expired, and otherwise marked used.
    pub fn use_handle(
        &self,
        handle: &PlaybackHandle,
        session: &SessionId,
        endpoint: &str,
    ) -> Result<Use, Refusal> {
        let now = Instant::now();
        let mut handles = self.lock();
        let Some(playback) = handles.get_mut(&handle.0) else {
            return Err(Refusal::Expired);
        };
        if &playback.session != session || playback.endpoint != endpoint {
            return Err(Refusal::NotYours);
        }
        if now.duration_since(playback.last_used) >= self.idle {
            handles.remove(&handle.0);
            return Err(Refusal::Expired);
        }
        playback.last_used = now;
        let recheck = now.duration_since(playback.last_checked) >= self.recheck;
        if recheck {
            playback.last_checked = now;
        }
        Ok(Use {
            playback: playback.clone(),
            recheck,
        })
    }

    /// End a playback whose session failed its re-check.
    pub fn end(&self, handle: &PlaybackHandle) {
        self.lock().remove(&handle.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(p: &Playbacks) -> PlaybackHandle {
        p.start(
            SessionId("s".into()),
            "e".into(),
            "/media/a.mkv".into(),
            10,
            None,
        )
    }

    #[test]
    fn a_handle_is_usable_by_its_session_on_its_endpoint_only() {
        let p = Playbacks::default();
        let h = start(&p);
        assert!(p.use_handle(&h, &SessionId("s".into()), "e").is_ok());
        assert_eq!(
            p.use_handle(&h, &SessionId("other".into()), "e")
                .unwrap_err(),
            Refusal::NotYours
        );
        assert_eq!(
            p.use_handle(&h, &SessionId("s".into()), "x").unwrap_err(),
            Refusal::NotYours
        );
    }

    #[test]
    fn an_unknown_handle_is_expired() {
        let p = Playbacks::default();
        assert_eq!(
            p.use_handle(&PlaybackHandle("nope".into()), &SessionId("s".into()), "e")
                .unwrap_err(),
            Refusal::Expired
        );
    }

    #[test]
    fn an_idle_handle_expires() {
        let p = Playbacks::new(Duration::from_millis(20), RECHECK);
        let h = start(&p);
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(
            p.use_handle(&h, &SessionId("s".into()), "e").unwrap_err(),
            Refusal::Expired
        );
    }

    #[test]
    fn use_asks_for_a_recheck_once_per_interval() {
        let p = Playbacks::new(IDLE, Duration::from_millis(20));
        let h = start(&p);
        let s = SessionId("s".into());
        assert!(!p.use_handle(&h, &s, "e").unwrap().recheck);
        std::thread::sleep(Duration::from_millis(40));
        assert!(p.use_handle(&h, &s, "e").unwrap().recheck);
        assert!(!p.use_handle(&h, &s, "e").unwrap().recheck);
    }

    #[test]
    fn handles_are_unguessable_and_distinct() {
        let p = Playbacks::default();
        let a = start(&p);
        let b = start(&p);
        assert_ne!(a, b);
        assert_eq!(a.0.len(), 32);
    }
}
