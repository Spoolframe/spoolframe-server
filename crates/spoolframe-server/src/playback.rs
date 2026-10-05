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

use std::sync::Arc;

use password_hash::rand_core::{OsRng, RngCore};
use spoolframe_protocol::{PlaybackHandle, Selection, SessionId};
use spoolframe_types::{MediaId, SubtitleFormat};
use tokio::sync::watch;

use crate::encode::Conversion;
use crate::metrics::Counter;
use crate::subtitles::TextState;

/// How long a handle lives without reads.
pub const IDLE: Duration = Duration::from_secs(5 * 60);
/// How long a device's playbacks outlive its last connection: long enough
/// for a reconnect (a network change, an app brought back), short of
/// holding an encode slot for [`IDLE`] for an app that was killed.
pub const DISCONNECT_GRACE: Duration = Duration::from_secs(30);
/// How often, while a handle is used, its session is checked again, so a
/// revoked session or a disabled user stops playback promptly.
pub const RECHECK: Duration = Duration::from_secs(30);

/// A selected subtitle the client fetches whole.
#[derive(Debug, Clone)]
pub enum Sidecar {
    /// A file next to the media file (P1-08).
    File {
        path: PathBuf,
        format: SubtitleFormat,
    },
    /// Text the server makes from a track (P2-07): a bitmap track read as
    /// text, or a text track converted; there once it is made.
    Made {
        state: watch::Receiver<TextState>,
        format: SubtitleFormat,
    },
}

/// One playback.
#[derive(Debug, Clone)]
pub struct Playback {
    pub revision: u64,
    pub progress_sequence: u64,
    progress_superseded: bool,
    pub session: SessionId,
    /// Hex `EndpointId`.
    pub endpoint: String,
    /// What is played, and with which tracks: a subtitle change re-plans
    /// it (P2-07).
    pub item: MediaId,
    pub selection: Selection,
    pub path: PathBuf,
    pub size: u64,
    pub subtitle: Option<Sidecar>,
    /// For a converted playback, its encode (P2-03). Ending the handle
    /// drops it, which stops ffmpeg and removes its files.
    pub conversion: Option<Arc<Conversion>>,
    last_used: Instant,
    last_checked: Instant,
}

impl Playback {
    /// How it is served.
    pub fn plan(&self) -> Plan {
        match &self.conversion {
            None => Plan::Direct,
            Some(c) if c.encodes() => Plan::Transcode,
            Some(_) => Plan::Remux,
        }
    }
}

/// A playback to open.
#[derive(Debug)]
pub struct Opened {
    pub session: SessionId,
    pub endpoint: String,
    pub item: MediaId,
    pub selection: Selection,
    pub path: PathBuf,
    pub size: u64,
    pub subtitle: Option<Sidecar>,
    pub conversion: Option<Arc<Conversion>>,
}

/// What a playback becomes when its subtitle changes.
#[derive(Debug)]
pub struct Changed {
    pub selection: Selection,
    pub size: u64,
    pub subtitle: Option<Sidecar>,
    pub conversion: Option<Arc<Conversion>>,
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
    disconnect_grace: Duration,
    handles: Mutex<HashMap<String, Playback>>,
    controls: crate::keyed_locks::KeyedLocks,
    progress_controls: crate::keyed_locks::KeyedLocks,
    /// Opened since the server started (P3-14).
    started: Counter,
    /// Ended since the server started, however they ended.
    ended: Counter,
}

/// How a playback is served (P3-14): the file as it is, a conversion that
/// only copies its streams into another container, or one that encodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Plan {
    Direct,
    Remux,
    Transcode,
}

impl Plan {
    pub const ALL: [Plan; 3] = [Plan::Direct, Plan::Remux, Plan::Transcode];

    pub fn as_str(self) -> &'static str {
        match self {
            Plan::Direct => "direct",
            Plan::Remux => "remux",
            Plan::Transcode => "transcode",
        }
    }
}

/// The playbacks open now, by plan, and how many opened and ended since
/// the server started.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaybackCounts {
    pub direct: u64,
    pub remux: u64,
    pub transcode: u64,
    pub started: u64,
    pub ended: u64,
}

impl PlaybackCounts {
    pub fn active(&self, plan: Plan) -> u64 {
        match plan {
            Plan::Direct => self.direct,
            Plan::Remux => self.remux,
            Plan::Transcode => self.transcode,
        }
    }
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
            disconnect_grace: DISCONNECT_GRACE,
            handles: Mutex::default(),
            controls: Default::default(),
            progress_controls: Default::default(),
            started: Counter::default(),
            ended: Counter::default(),
        }
    }

    /// What is open now, and what opened and ended (P3-14). A playback
    /// idle past the limit counts as open until the next sweep ends it.
    pub fn counts(&self) -> PlaybackCounts {
        let mut counts = PlaybackCounts {
            started: self.started.get(),
            ended: self.ended.get(),
            ..PlaybackCounts::default()
        };
        for playback in self.lock().values() {
            match playback.plan() {
                Plan::Direct => counts.direct += 1,
                Plan::Remux => counts.remux += 1,
                Plan::Transcode => counts.transcode += 1,
            }
        }
        counts
    }

    /// Playbacks end `grace` after their device's last connection closes
    /// ([`DISCONNECT_GRACE`]; tests shorten it).
    pub fn with_disconnect_grace(mut self, grace: Duration) -> Self {
        self.disconnect_grace = grace;
        self
    }

    pub fn disconnect_grace(&self) -> Duration {
        self.disconnect_grace
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Playback>> {
        self.handles.lock().expect("never held across a panic")
    }

    /// Serialize changes to this handle. The caller must look up the
    /// playback again after acquiring this guard: a preceding operation
    /// may have replaced its conversion or stopped it entirely.
    pub(crate) async fn control(
        &self,
        handle: &PlaybackHandle,
        session: &SessionId,
        endpoint: &str,
    ) -> Result<crate::keyed_locks::Guard<'_>, Refusal> {
        {
            let handles = self.lock();
            let playback = handles.get(&handle.0).ok_or(Refusal::Expired)?;
            if &playback.session != session || playback.endpoint != endpoint {
                return Err(Refusal::NotYours);
            }
        }
        Ok(self.controls.lock(&handle.0).await)
    }

    /// A new, unguessable handle, before its playback is opened: a
    /// conversion names its files after it.
    pub fn new_handle() -> PlaybackHandle {
        let mut bytes = [0u8; 16];
        OsRng.fill_bytes(&mut bytes);
        PlaybackHandle(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// Open `opened` as `handle`; the session was checked by the caller
    /// just now.
    pub fn start(&self, handle: PlaybackHandle, opened: Opened) -> PlaybackHandle {
        let now = Instant::now();
        let mut handles = self.lock();
        // Forget idle handles here rather than on a timer.
        let before = handles.len();
        handles.retain(|_, p| now.duration_since(p.last_used) < self.idle);
        self.ended.add((before - handles.len()) as u64);
        self.started.inc();
        for previous in handles.values_mut() {
            if previous.session == opened.session && previous.item == opened.item {
                previous.progress_superseded = true;
            }
        }
        handles.insert(
            handle.0.clone(),
            Playback {
                revision: 0,
                progress_sequence: 0,
                progress_superseded: false,
                session: opened.session,
                endpoint: opened.endpoint,
                item: opened.item,
                selection: opened.selection,
                path: opened.path,
                size: opened.size,
                subtitle: opened.subtitle,
                conversion: opened.conversion,
                last_used: now,
                last_checked: now,
            },
        );
        handle
    }

    /// Play `handle` as `changed` from now (a subtitle change, P2-07). The
    /// conversion it replaces is returned, to be dropped outside the lock:
    /// dropping it stops its encode and removes its files.
    pub fn change(&self, handle: &PlaybackHandle, changed: Changed) -> Option<Arc<Conversion>> {
        let mut handles = self.lock();
        let playback = handles.get_mut(&handle.0)?;
        playback.revision += 1;
        playback.selection = changed.selection;
        playback.size = changed.size;
        playback.subtitle = changed.subtitle;
        std::mem::replace(&mut playback.conversion, changed.conversion)
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
            self.ended.inc();
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

    /// What `handle` plays, if it is open: for the log, which never names
    /// the handle itself. Not a use of it.
    pub fn item(&self, handle: &PlaybackHandle) -> Option<MediaId> {
        self.lock().get(&handle.0).map(|p| p.item.clone())
    }

    /// Mark `handle` used now, as a read that is still waiting for an
    /// encode does, so the playback does not idle out under it.
    pub fn touch(&self, handle: &PlaybackHandle) {
        if let Some(p) = self.lock().get_mut(&handle.0) {
            p.last_used = Instant::now();
        }
    }

    pub fn revised(&self, handle: &PlaybackHandle) {
        if let Some(playback) = self.lock().get_mut(&handle.0) {
            playback.revision += 1;
        }
    }

    pub(crate) async fn progress_control(
        &self,
        session: &SessionId,
        item: &MediaId,
    ) -> crate::keyed_locks::Guard<'_> {
        self.progress_controls
            .lock(&format!("{}:{item}", session.0))
            .await
    }

    pub(crate) fn check_progress(
        &self,
        handle: &PlaybackHandle,
        sequence: u64,
    ) -> Result<(), spoolframe_protocol::Error> {
        let handles = self.lock();
        let playback = handles
            .get(&handle.0)
            .ok_or(spoolframe_protocol::Error::PlaybackExpired)?;
        if playback.progress_superseded || sequence <= playback.progress_sequence {
            return Err(spoolframe_protocol::Error::StaleProgress);
        }
        Ok(())
    }

    pub(crate) fn record_progress(&self, handle: &PlaybackHandle, sequence: u64) {
        if let Some(playback) = self.lock().get_mut(&handle.0) {
            playback.progress_sequence = sequence;
        }
    }

    /// End a playback: its session failed its re-check, or the client
    /// stopped it.
    pub fn end(&self, handle: &PlaybackHandle) {
        // Dropped outside the lock: ending a conversion removes files.
        let ended = self.lock().remove(&handle.0);
        if ended.is_some() {
            self.ended.inc();
        }
        drop(ended);
    }

    /// End `endpoint`'s converted playbacks: it is starting another, and a
    /// device plays one thing at a time, so their encodes would only hold
    /// slots (an app killed mid-film and opened again). Returns how many.
    pub fn end_conversions_on(&self, endpoint: &str) -> usize {
        self.end_where(|p| p.endpoint == endpoint && p.conversion.is_some())
    }

    /// End every playback of `endpoint`: it has been gone longer than the
    /// grace. Returns how many.
    pub fn end_device(&self, endpoint: &str) -> usize {
        self.end_where(|p| p.endpoint == endpoint)
    }

    fn end_where(&self, ends: impl Fn(&Playback) -> bool) -> usize {
        let mut ended = Vec::new();
        self.lock().retain(|_, p| {
            let end = ends(p);
            if end {
                ended.push(p.conversion.take());
            }
            !end
        });
        // Dropped outside the lock: ending a conversion removes files.
        let count = ended.len();
        self.ended.add(count as u64);
        drop(ended);
        count
    }

    /// End every playback idle past the limit. Run now and then, so a
    /// conversion does not outlive its handle by long.
    pub fn sweep(&self) {
        let now = Instant::now();
        let mut ended = Vec::new();
        self.lock().retain(|_, p| {
            let alive = now.duration_since(p.last_used) < self.idle;
            if !alive {
                ended.push(p.conversion.take());
            }
            alive
        });
        self.ended.add(ended.len() as u64);
        drop(ended);
    }

    /// How often [`Playbacks::sweep`] should run.
    pub fn sweep_every(&self) -> Duration {
        (self.idle / 2).min(Duration::from_secs(30))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(p: &Playbacks) -> PlaybackHandle {
        start_on(p, "e")
    }

    fn start_on(p: &Playbacks, endpoint: &str) -> PlaybackHandle {
        p.start(
            Playbacks::new_handle(),
            Opened {
                session: SessionId("s".into()),
                endpoint: endpoint.into(),
                item: "plex:movie:1".parse().unwrap(),
                selection: Selection::default(),
                path: "/media/a.mkv".into(),
                size: 10,
                subtitle: None,
                conversion: None,
            },
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

    /// Each playback ends once in the counts, however it ends (P3-14).
    #[test]
    fn playbacks_are_counted_open_and_ended_once_each() {
        let p = Playbacks::new(Duration::from_millis(20), RECHECK);
        let stopped = start(&p);
        let idle = start(&p);
        let _device = start_on(&p, "tv");
        let counts = p.counts();
        assert_eq!((counts.direct, counts.started, counts.ended), (3, 3, 0));
        p.end(&stopped);
        p.end(&stopped);
        assert_eq!(p.end_device("tv"), 1);
        std::thread::sleep(Duration::from_millis(40));
        // Found idle when used, then forgotten.
        assert!(p.use_handle(&idle, &SessionId("s".into()), "e").is_err());
        p.sweep();
        let counts = p.counts();
        assert_eq!(
            (counts.active(Plan::Direct), counts.started, counts.ended),
            (0, 3, 3)
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
    fn a_gone_device_s_playbacks_end_and_no_one_else_s() {
        let p = Playbacks::default();
        let tv = start_on(&p, "tv");
        let phone = start_on(&p, "phone");
        assert_eq!(p.end_device("tv"), 1);
        let s = SessionId("s".into());
        assert_eq!(p.use_handle(&tv, &s, "tv").unwrap_err(), Refusal::Expired);
        assert!(p.use_handle(&phone, &s, "phone").is_ok());
    }

    #[test]
    fn a_new_playback_ends_only_converted_ones() {
        let p = Playbacks::default();
        let direct = start_on(&p, "tv");
        // Direct play holds no slot: it stays.
        assert_eq!(p.end_conversions_on("tv"), 0);
        assert!(p.use_handle(&direct, &SessionId("s".into()), "tv").is_ok());
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
