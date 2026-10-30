//! Playing an item: planned on the server and opened with the SDK's
//! `ManagedPlayback`, as a [`SpoolPlayback`] the app holds. A player that
//! only takes a URL (AVPlayer, AirPlay) has it served on the loopback
//! bridge (`play`, `SpoolPlayback::url`; the `hls-bridge` feature,
//! `bridge.rs`): `/<token>/master.m3u8` (HLS) or `/<token>/file`
//! (direct), the paths the player and the system subtitle menu depend
//! on. A player that reads the bytes itself (Media3's `DataSource`) opens
//! it with `open_playback` and reads it from the object, with no bridge.
//!
//! Inside, each plan of a playback is registered under a token, the
//! bridge's URL path, unguessable. The object follows its playback from
//! one token to the next: a re-plan the SDK made behind the player, or
//! one the app asked for (quality, subtitles, a slow link).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use spoolframe_client_sdk::types::{Delivery, Marker, MediaId, SubtitleFormat};
use spoolframe_client_sdk::{
    self as sdk, ChosenTracks, HttpPlayback, PlaybackPlan, PlaybackQuality, Selection,
};
use spoolframe_hls_bridge::{Kind, MenuEntry, SdkSource, Stream};

use crate::error::SpoolError;
use crate::events::PlaybackShareWatch;
use crate::reader::PlaybackReader;
use crate::resources::{PlaybackResource, bridge};
use crate::runtime::{block, run};
use crate::session::SpoolSession;
use crate::values::ScreenCapabilities;

/// A playback as it plays now, for the player.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PlaybackStart {
    /// What a URL player opens, on the loopback bridge; `None` for a
    /// playback only opened (`open_playback`), whose bytes the app reads.
    pub url: Option<String>,
    /// The server converts it to HLS; else the file plays as it is.
    pub hls: bool,
    /// How the server plays it, for the diagnostics.
    pub plan: PlaybackPlan,
    /// The audio and subtitle the server picked, when none were asked
    /// for.
    pub chosen: Option<ChosenTracks>,
    /// The file's size in bytes, for direct play; 0 when converted.
    pub size: u64,
    /// A paired server's item, played through the user's own server: this
    /// device didn't reach the paired one.
    pub proxied: bool,
    /// The item's intro and credits, by the item's own times wherever the
    /// playback started.
    pub markers: Vec<Marker>,
    /// The item's running time, when the server says it (an older one
    /// doesn't).
    pub duration: Option<Duration>,
}

/// A subtitle track as the system player's menu names it.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SubtitleEntry {
    /// Its index in the item's subtitle tracks.
    pub index: u32,
    /// As the menu shows it.
    pub name: String,
    /// BCP 47 ("en"), when known.
    pub language: Option<String>,
    pub forced: bool,
}

impl From<SubtitleEntry> for MenuEntry {
    fn from(entry: SubtitleEntry) -> Self {
        Self {
            index: entry.index,
            name: entry.name,
            language: entry.language,
            forced: entry.forced,
        }
    }
}

/// A subtitle file, whole: text in `format`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PlaybackSubtitle {
    pub format: SubtitleFormat,
    pub bytes: Vec<u8>,
}

/// One plan of a playback, open under a token: the crate's one registry
/// of them, which the bridge and the reads by name share.
pub(crate) struct Registered {
    /// What it is open under: the bridge's URL path, unguessable.
    pub(crate) token: String,
    pub(crate) managed: sdk::ManagedPlayback,
    /// The playback's generation then ([`sdk::ManagedPlayback::generation`]):
    /// once it moves on, the player moves to a new plan.
    generation: u64,
    /// The subtitle the server plays, if any: the one the viewer picked
    /// last, even where the plan stayed the same (a direct play's).
    pub(crate) subtitle: Mutex<Option<u32>>,
    /// What happened to it behind the player, for the playback log.
    notes: Arc<Mutex<Vec<String>>>,
    /// It as the bridge reads it, when its plan is one the bridge serves
    /// (the file, or HLS): by name (`resources.rs`), and over HTTP once
    /// served (`bridge.rs`). Its token is this one's, and its reads flag
    /// it stopped or expired.
    pub(crate) stream: Option<Arc<Stream<SdkSource>>>,
    /// A read of its bytes (`reader.rs`) found the server's admin had
    /// stopped it.
    pub(crate) stopped: AtomicBool,
    /// A read of its bytes found the server had ended it.
    pub(crate) expired: AtomicBool,
    /// The HLS resources read last, whole, for the ranges read from them,
    /// least recently used first (`resources.rs`).
    pub(crate) resources: Mutex<Vec<Arc<crate::resources::Fetched>>>,
}

static PLAYBACKS: LazyLock<Mutex<HashMap<String, Arc<Registered>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// `token`'s playback, while it is open.
pub(crate) fn registered(token: &str) -> Option<Arc<Registered>> {
    PLAYBACKS
        .lock()
        .expect("never poisoned")
        .get(token)
        .cloned()
}

impl Registered {
    /// A read found the server's admin had stopped it.
    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed) || self.stream.as_ref().is_some_and(|s| s.stopped())
    }

    /// A read found the server had ended it.
    fn expired(&self) -> bool {
        self.expired.load(Ordering::Relaxed) || self.stream.as_ref().is_some_and(|s| s.expired())
    }

    /// The playback moved on behind the player ([`sdk::moves_player`]):
    /// planned again, another encode, or started again.
    pub(crate) fn moved(&self) -> bool {
        self.managed.generation() != self.generation
    }

    /// The playback's generation as registered: what the player was given.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
}

/// Open `managed`, planned as `playback` in `generation`, under a new
/// token, unguessable:
/// the bridge's URL path too. One the bridge reads is registered with it
/// under the same token, its subtitle menu `menu`.
fn register(
    managed: sdk::ManagedPlayback,
    generation: u64,
    playback: &sdk::Playback,
    subtitle: Option<u32>,
    notes: Arc<Mutex<Vec<String>>>,
    menu: Option<Vec<MenuEntry>>,
) -> Arc<Registered> {
    let stream = Kind::of(playback).map(|kind| {
        let source = SdkSource::new(managed.clone(), playback.plan.clone());
        let stream = Stream::new(source, kind, subtitle, notes.clone());
        if let Some(menu) = menu {
            stream.set_menu(menu);
        }
        Arc::new(stream)
    });
    let token = match &stream {
        Some(stream) => bridge().register_shared(stream.clone()),
        None => spoolframe_hls_bridge::token(),
    };
    let playback = Arc::new(Registered {
        token: token.clone(),
        managed,
        generation,
        subtitle: Mutex::new(subtitle),
        notes,
        stream,
        stopped: AtomicBool::new(false),
        expired: AtomicBool::new(false),
        resources: Mutex::new(Vec::new()),
    });
    PLAYBACKS
        .lock()
        .expect("never poisoned")
        .insert(token, playback.clone());
    playback
}

/// `token` is done: no more reads, and the bridge serves it no more.
pub(crate) fn unregister(token: &str) -> Option<Arc<Registered>> {
    let playback = PLAYBACKS.lock().expect("never poisoned").remove(token)?;
    if playback.stream.is_some() {
        bridge().unregister(token);
    }
    Some(playback)
}

fn hls(plan: &PlaybackPlan) -> bool {
    matches!(
        plan,
        PlaybackPlan::Converted {
            delivery: Delivery::Hls,
            ..
        }
    )
}

/// A plan of a playback just opened under its token.
pub(crate) struct Opened {
    pub(crate) playback: Arc<Registered>,
    pub(crate) start: PlaybackStart,
}

/// Own a token until it is handed to a [`SpoolPlayback`]. Dropping a
/// cancelled runtime task's completed result closes the token (and its
/// URL) too.
pub(crate) struct RegisteredPlayback(pub(crate) Option<Opened>);

impl RegisteredPlayback {
    pub(crate) fn take(mut self) -> Opened {
        self.0.take().expect("owned result")
    }
}

impl Drop for RegisteredPlayback {
    fn drop(&mut self) {
        if let Some(opened) = self.0.take() {
            unregister(&opened.playback.token);
        }
    }
}

/// One item playing. Its reads (`read`, `open_resource`,
/// `read_resource`) block their thread until the bytes are here: call
/// them from the player's loading threads. It follows the playback
/// through every new plan: a re-plan the SDK made behind the player, and
/// the changes the app asks for. `finish` or `stop` ends it; dropped, it
/// is closed.
#[derive(uniffi::Object)]
pub struct SpoolPlayback {
    current: Mutex<Current>,
    /// Held while the player moves to a new plan (`follow`, `replan`):
    /// one move at a time, so two callers that both find the plan stale
    /// make one new plan, and the second finds the player already on it.
    moving: tokio::sync::Mutex<()>,
    /// Served on the loopback bridge: each plan it moves to is served
    /// there too. Set before `url` serves, so a move meanwhile serves its
    /// new plan.
    #[cfg(feature = "hls-bridge")]
    pub(crate) served: AtomicBool,
}

/// The plan a read is of, and its reader (or why there is none).
type Reading = (Arc<Registered>, Result<Arc<PlaybackReader>, SpoolError>);

/// The plan the player is on.
struct Current {
    /// Its registration: a read reaches its playback with no lookup.
    plan: Arc<Registered>,
    /// How it plays, shared with `info()`'s callers until it changes.
    start: Arc<PlaybackStart>,
    /// Its bytes, made on the first read.
    reader: Option<Arc<PlaybackReader>>,
    /// As `set_flushing` said last: a reader made now starts so.
    flushing: bool,
    /// `finish` or `stop` ended it: a move still under way closes the
    /// plan it made rather than moving there.
    ended: bool,
}

impl SpoolPlayback {
    pub(crate) fn new(opened: RegisteredPlayback) -> Arc<Self> {
        let Opened { playback, start } = opened.take();
        #[cfg(feature = "hls-bridge")]
        let served = AtomicBool::new(start.url.is_some());
        Arc::new(Self {
            current: Mutex::new(Current {
                plan: playback,
                start: Arc::new(start),
                reader: None,
                flushing: false,
                ended: false,
            }),
            moving: tokio::sync::Mutex::new(()),
            #[cfg(feature = "hls-bridge")]
            served,
        })
    }

    /// Served on the loopback bridge: its moves are served there too.
    fn served(&self) -> bool {
        #[cfg(feature = "hls-bridge")]
        return self.served.load(Ordering::Relaxed);
        #[cfg(not(feature = "hls-bridge"))]
        false
    }

    /// The plan's token: the bridge's URL path.
    #[cfg(any(feature = "hls-bridge", test))]
    pub(crate) fn token(&self) -> String {
        self.current
            .lock()
            .expect("never poisoned")
            .plan
            .token
            .clone()
    }

    /// The plan the player is on, while it is open.
    pub(crate) fn registered(&self) -> Result<Arc<Registered>, SpoolError> {
        let current = self.current.lock().expect("never poisoned");
        match current.ended {
            true => Err(SpoolError::PlaybackExpired),
            false => Ok(current.plan.clone()),
        }
    }

    fn managed(&self) -> Result<sdk::ManagedPlayback, SpoolError> {
        Ok(self.registered()?.managed.clone())
    }

    /// Move the player from the plan `old` to `opened`, the playback's
    /// new plan, and only then close `old`: a call from another thread
    /// meanwhile finds one or the other open, never neither.
    fn switch(&self, old: &Registered, opened: RegisteredPlayback) -> PlaybackStart {
        let Opened { playback, start } = opened.take();
        let ended = {
            let mut current = self.current.lock().expect("never poisoned");
            if !current.ended {
                current.plan = playback.clone();
                current.start = Arc::new(start.clone());
                current.reader = None;
            }
            current.ended
        };
        if ended {
            unregister(&playback.token);
        }
        unregister(&old.token);
        start
    }

    /// Whether the player is still on the plan `seen`.
    fn on(&self, seen: &Arc<Registered>) -> bool {
        Arc::ptr_eq(&self.current.lock().expect("never poisoned").plan, seen)
    }

    /// What a call on the plan `seen` that failed with `error` tells the
    /// player. A stale plan's read moves the player to the new plan and
    /// says `Replanned`; so does a plan closed because another caller
    /// moved the player on.
    fn failed(self: &Arc<Self>, seen: Arc<Registered>, error: SpoolError) -> SpoolError {
        match error {
            SpoolError::Replanned => self.replanned(seen),
            SpoolError::PlaybackExpired if !self.on(&seen) => SpoolError::Replanned,
            error => error,
        }
    }

    /// The URL the plan `token` is served at, while the player is on it.
    #[cfg(feature = "hls-bridge")]
    pub(crate) fn set_url(&self, token: &str, url: String) {
        let mut current = self.current.lock().expect("never poisoned");
        if current.plan.token == token {
            let start = PlaybackStart {
                url: Some(url),
                ..(*current.start).clone()
            };
            current.start = Arc::new(start);
        }
    }

    /// Follow a plan the SDK made behind the player, which was on the
    /// plan `seen`: the plan it is on now, where it moved (here, or by
    /// another caller first), or `None` when there is no new plan. On a
    /// failure the player stays on `seen`, still open.
    async fn follow(
        self: Arc<Self>,
        seen: Arc<Registered>,
    ) -> Result<Option<PlaybackStart>, SpoolError> {
        let _moving = self.moving.lock().await;
        if !self.on(&seen) {
            return Ok(Some(self.info()));
        }
        let old = self.registered()?;
        if !old.moved() {
            return Ok(None);
        }
        let served = self.served();
        let opened = run(async move { reopen(&old, None, served).await }).await?;
        Ok(Some(self.switch(&seen, opened)))
    }

    /// A read of the plan `seen` found the playback planned again: the
    /// player is moved to the new plan, and told so (`Replanned`) to
    /// build its media again from `info()`.
    fn replanned(self: &Arc<Self>, seen: Arc<Registered>) -> SpoolError {
        match block(self.clone().follow(seen)) {
            Ok(_) => SpoolError::Replanned,
            Err(error) => error,
        }
    }

    /// The plan the player is on, and its reader, made on first use (or
    /// why not: the plan moved on): one lock, no lookup.
    fn reader(&self) -> Result<Reading, SpoolError> {
        let mut current = self.current.lock().expect("never poisoned");
        if current.ended {
            return Err(SpoolError::PlaybackExpired);
        }
        let plan = current.plan.clone();
        if let Some(reader) = &current.reader {
            return Ok((plan, Ok(reader.clone())));
        }
        let reader = crate::reader::reader(plan.clone(), current.start.duration).map(Arc::new);
        if let Ok(reader) = &reader {
            reader.set_flushing(current.flushing);
            current.reader = Some(reader.clone());
        }
        Ok((plan, reader))
    }

    /// Plan the playback again with `change`. One that comes back with
    /// another plan or encode moves the player there, its subtitle
    /// `subtitle` where given: the new plan, or `None` when nothing
    /// changed.
    async fn replan<F, Fut>(
        self: Arc<Self>,
        subtitle: Option<Option<u32>>,
        change: F,
    ) -> Result<Option<PlaybackStart>, SpoolError>
    where
        F: FnOnce(sdk::ManagedPlayback) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<sdk::Playback, sdk::SdkError>> + Send,
    {
        let _moving = self.moving.lock().await;
        let registered = self.registered()?;
        let old = registered.clone();
        let served = self.served();
        let opened = run(async move {
            let playback = registered.managed.clone();
            let before = playback.generation();
            change(playback).await?;
            // The player moves where the playback did ([`sdk::moves_player`]).
            if registered.managed.generation() == before {
                // The same bytes, a subtitle picked all the same (a
                // direct play's): kept for the plans after this one.
                if let Some(subtitle) = subtitle {
                    *registered.subtitle.lock().expect("never poisoned") = subtitle;
                }
                return Ok(None);
            }
            reopen(&registered, subtitle, served).await.map(Some)
        })
        .await?;
        Ok(opened.map(|opened| self.switch(&old, opened)))
    }

    /// Done: no more reads, and the bridge serves it no more.
    fn end(&self) -> Option<sdk::ManagedPlayback> {
        let mut current = self.current.lock().expect("never poisoned");
        current.reader = None;
        current.ended = true;
        unregister(&current.plan.token).map(|p| p.managed.clone())
    }
}

impl Drop for SpoolPlayback {
    fn drop(&mut self) {
        if let Some(managed) = self.end() {
            crate::runtime::spawn(async move {
                let _ = managed.close().await;
            });
        }
    }
}

#[uniffi::export]
impl SpoolPlayback {
    /// How it plays now: after a change, or a re-plan the SDK made behind
    /// the player, another plan.
    pub fn info(&self) -> PlaybackStart {
        let start = self.current.lock().expect("never poisoned").start.clone();
        (*start).clone()
    }

    /// Up to `length` bytes from `offset` of a direct play's file or a
    /// progressive conversion's output; none at the end. Blocks until
    /// they are here. `Cancelled` while `set_flushing(true)` holds. One
    /// the admin stopped is `PlaybackStopped`. One that moved on (planned
    /// again, or started again by the SDK after the server ended it) is
    /// `Replanned`, the player already moved to the playback as it is:
    /// build the media again from `info()`, where the viewer was. An HLS
    /// conversion is read by name (`open_resource`).
    pub fn read(self: Arc<Self>, offset: u64, length: u64) -> Result<Vec<u8>, SpoolError> {
        let (seen, reader) = self.reader()?;
        reader
            .and_then(|reader| reader.read(offset, length))
            .map_err(|error| self.failed(seen, error))
    }

    /// Waiting reads return `Cancelled` until called with `false`: a
    /// `DataSource` closed, or a seek.
    pub fn set_flushing(&self, flushing: bool) {
        let mut current = self.current.lock().expect("never poisoned");
        current.flushing = flushing;
        if let Some(reader) = &current.reader {
            reader.set_flushing(flushing);
        }
    }

    /// The size in bytes of a direct play's file; `None` for a
    /// conversion's output, which grows.
    pub fn size(&self) -> Option<u64> {
        let start = &self.current.lock().expect("never poisoned").start;
        matches!(start.plan, PlaybackPlan::Direct { .. }).then_some(start.size)
    }

    /// The type a player reads it as: the file's (`video/mp4`), a
    /// progressive conversion's Matroska, or HLS's playlist type.
    pub fn content_type(&self) -> String {
        let start = &self.current.lock().expect("never poisoned").start;
        if start.hls {
            "application/vnd.apple.mpegurl".to_owned()
        } else {
            crate::reader::content_type(&start.plan).to_owned()
        }
    }

    /// Open `path` of an HLS conversion (`master.m3u8`, or a name a
    /// playlist gives): read whole now, as a `DataSource`'s open does,
    /// and kept for `read_resource`'s ranges: the last few opened, each
    /// by its path. A playlist or subtitle opened again is read again; a
    /// media segment kept is not. Blocks. `Replanned` as `read`.
    pub fn open_resource(self: Arc<Self>, path: String) -> Result<PlaybackResource, SpoolError> {
        let seen = self.registered()?;
        crate::resources::open(&seen, path)
            .map(|fetched| fetched.resource())
            .map_err(|error| self.failed(seen, error))
    }

    /// Up to `length` bytes of `path` from `offset`, none past its end:
    /// from the body `open_resource` kept for `path`, so a run of ranges
    /// reads it once. Blocks. `Replanned` as `read`.
    pub fn read_resource(
        self: Arc<Self>,
        path: String,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, SpoolError> {
        let seen = self.registered()?;
        crate::resources::read_range(&seen, path, offset, length)
            .map_err(|error| self.failed(seen, error))
    }

    /// The SDK planned the playback again behind the player (an Auto
    /// playback on a slow link, or its share of the server's uplink
    /// moved): the player is moved to the new plan, returned to switch
    /// to where it was; `None` when it wasn't. For a URL player, whose
    /// reads the bridge answers: ask when it stalls or now and then.
    pub async fn follow_replan(self: Arc<Self>) -> Result<Option<PlaybackStart>, SpoolError> {
        let seen = self.registered()?;
        self.follow(seen).await
    }

    /// The viewer picked `quality` at `position`: the server plans it
    /// again. The new plan (or a new encode) to switch to, or `None` when
    /// nothing changed.
    pub async fn change_quality(
        self: Arc<Self>,
        quality: PlaybackQuality,
        position: Duration,
    ) -> Result<Option<PlaybackStart>, SpoolError> {
        self.replan(None, move |playback| async move {
            playback.change_quality(quality, position).await
        })
        .await
    }

    /// The viewer picked subtitle `subtitle` (its index in the item's
    /// subtitle tracks; `None`: none) at `position`. A conversion is
    /// encoded again from there: the new plan (or encode) to switch to,
    /// or `None` when the player keeps what it has.
    pub async fn change_subtitle(
        self: Arc<Self>,
        subtitle: Option<u32>,
        position: Duration,
    ) -> Result<Option<PlaybackStart>, SpoolError> {
        self.replan(Some(subtitle), move |playback| async move {
            playback
                .change_subtitle(subtitle, sdk::BitmapSubtitles::Auto, position)
                .await
        })
        .await
    }

    /// The direct playback stalls on a link carrying only `link_bitrate`:
    /// the server plans it again for that. The new plan (a conversion,
    /// where the viewer last was) to switch to, or `None` when the plan
    /// already fits.
    pub async fn limit(
        self: Arc<Self>,
        link_bitrate: u64,
    ) -> Result<Option<PlaybackStart>, SpoolError> {
        self.replan(None, move |playback| async move {
            playback.limit(link_bitrate).await
        })
        .await
    }

    /// The subtitle that comes as a file (an external one, or one the
    /// server reads as text), whole. One still being read as text is
    /// waited for, up to 30 s (the SDK's `SUBTITLE_WAIT`): then
    /// `SubtitleNotReady`, with how far the server got.
    pub async fn read_subtitle(&self) -> Result<PlaybackSubtitle, SpoolError> {
        let managed = self.managed()?;
        let file = run(async move {
            managed
                .subtitle_file_within(sdk::ManagedPlayback::SUBTITLE_WAIT)
                .await
        })
        .await?;
        Ok(PlaybackSubtitle {
            format: file.format,
            bytes: file.bytes,
        })
    }

    /// Where the player is: on every tick, as often as it likes. Sent in
    /// the background, a few at a time; `flush` sends the last now.
    pub fn report_position(&self, position: Duration) -> Result<(), SpoolError> {
        Ok(self.managed()?.report_position(position)?)
    }

    /// The player's seek to `target` begins: its own scrubber, or a seek
    /// the app asked of it. Until it lands its positions are the old
    /// place's, so `report_position` saves `target` meanwhile. Call
    /// `seek_done` from the player's own seek-complete signal (Media3's
    /// position discontinuity, AVPlayer's seek completion handler).
    /// Without it the seek is over once a position lands within 2 s of
    /// the target, or after 5 s. Nothing once the playback is over.
    pub fn seek_started(&self, target: Duration) {
        if let Ok(managed) = self.managed() {
            managed.seek_started(target);
        }
    }

    /// The player's seek is done: its positions count again.
    pub fn seek_done(&self) {
        if let Ok(managed) = self.managed() {
            managed.seek_done();
        }
    }

    /// Send the position reported last now: on pause.
    pub async fn flush(&self) -> Result<(), SpoolError> {
        let managed = self.managed()?;
        Ok(run(async move { managed.flush().await }).await?)
    }

    /// Report `position` and end the playback: the place is kept for
    /// Continue watching.
    pub async fn finish(&self, position: Duration) -> Result<(), SpoolError> {
        let Some(managed) = self.end() else {
            return Err(SpoolError::PlaybackExpired);
        };
        Ok(run(async move { managed.finish(position).await }).await?)
    }

    /// End the playback without a position: no more reads, the bridge
    /// stops serving it, and the server stops its encode.
    pub async fn stop(&self) {
        if let Some(managed) = self.end() {
            run(async move {
                let _ = managed.close().await;
            })
            .await
        }
    }

    /// What happened to the playback behind the player since last asked,
    /// for the playback log: plan changes, share events, errors.
    pub fn notes(&self) -> Vec<String> {
        self.registered()
            .map(|p| std::mem::take(&mut *p.notes.lock().expect("never poisoned")))
            .unwrap_or_default()
    }

    /// Have an HLS playback's master playlist name every subtitle track
    /// of its item, so the system player's menu lists them, in this
    /// order. Call before the player opens the URL; kept through re-plans.
    pub fn subtitle_menu(&self, entries: Vec<SubtitleEntry>) {
        if let Some(stream) = self.registered().ok().and_then(|p| p.stream.clone()) {
            stream.set_menu(entries.into_iter().map(Into::into).collect());
        }
    }

    /// The server's admin stopped the playback: the player says it ended
    /// there, not that something failed.
    pub fn stopped(&self) -> bool {
        self.registered().is_ok_and(|p| p.stopped())
    }

    /// The server ended the playback under the player (it expired, or a
    /// reconnect left it behind): start a new one at the same place.
    pub fn expired(&self) -> bool {
        self.registered().is_ok_and(|p| p.expired())
    }

    /// The playback over the server's own HTTP port, for an AirPlay
    /// receiver, which can't reach the loopback bridge: its URL, or why
    /// not. It's the playback's handle now: one the SDK started again
    /// needs a new one (the old answers 404).
    pub async fn http_playback_url(&self) -> Result<HttpPlayback, SpoolError> {
        let managed = self.managed()?;
        Ok(run(async move { managed.http_playback_url().await }).await?)
    }

    /// Its share of the server's uplink as the server moves it; `None`
    /// with no server to say.
    pub fn watch_share(&self) -> Option<Arc<PlaybackShareWatch>> {
        let playback = self.managed().ok()?;
        Some(Arc::new(PlaybackShareWatch::new(
            playback.downgrade(),
            playback.watch_share()?,
        )))
    }
}

#[uniffi::export]
impl SpoolSession {
    /// How the server would play `id` here with `tracks` (or, for `None`,
    /// the tracks it picks from the user's preferences). `display` is the
    /// screen for this plan only; `None` sends `SpoolClient.set_display`'s.
    pub async fn plan(
        &self,
        id: MediaId,
        tracks: Option<Selection>,
        display: Option<ScreenCapabilities>,
    ) -> Result<sdk::Plan, SpoolError> {
        let display = display.map(Into::into);
        self.call(|s| async move {
            s.playback_plan_on(&id, tracks, sdk::BitmapSubtitles::Auto, display)
                .await
        })
        .await
    }

    /// Keep `tracks` for the rest of `id`'s series (an episode) or for
    /// `id`.
    pub async fn keep_tracks(&self, id: MediaId, tracks: Selection) -> Result<(), SpoolError> {
        self.call(|s| async move { s.set_track_override(&id, tracks).await })
            .await
    }

    /// Open `id` from `start` (or the start), with `tracks` or, for
    /// `None`, the tracks the server picks from the user's preferences,
    /// for a player that reads it itself: no URL. `display`: the screen for
    /// this playback, kept for its re-plans; `None` sends
    /// `SpoolClient.set_display`'s.
    pub async fn open_playback(
        &self,
        id: MediaId,
        start: Option<Duration>,
        tracks: Option<Selection>,
        quality: PlaybackQuality,
        display: Option<ScreenCapabilities>,
    ) -> Result<Arc<SpoolPlayback>, SpoolError> {
        let opening = open(self.session.clone(), id, start, tracks, quality, display);
        let opened = run(opening).await?;
        Ok(SpoolPlayback::new(opened))
    }
}

/// Open `id` on `session` under a new token, not served.
pub(crate) async fn open(
    session: sdk::Session,
    id: MediaId,
    start: Option<Duration>,
    tracks: Option<Selection>,
    quality: PlaybackQuality,
    display: Option<ScreenCapabilities>,
) -> Result<RegisteredPlayback, SpoolError> {
    let options = sdk::PlaybackOptions {
        start,
        quality,
        display: display.map(Into::into),
        ..Default::default()
    };
    // A paired server's item plays from that server or through the
    // user's own, and its progress goes to the user's own.
    let managed = session.open_any_playback(&id, tracks, options).await?;
    let notes = Arc::new(Mutex::new(Vec::new()));
    tokio::spawn(watch(&managed, managed.shares(), notes.clone()));
    let subtitle = tracks.and_then(|t| t.subtitle);
    Ok(open_under_token(managed, subtitle, notes, None))
}

/// Open `managed` under a new token, not served. `subtitle`: the one
/// asked for, when the server chose none.
pub(crate) fn open_under_token(
    managed: sdk::ManagedPlayback,
    subtitle: Option<u32>,
    notes: Arc<Mutex<Vec<String>>>,
    menu: Option<Vec<MenuEntry>>,
) -> RegisteredPlayback {
    let (playback, generation) = managed.snapshot_with_generation();
    let subtitle = playback
        .chosen
        .as_ref()
        .map_or(subtitle, |c| c.selection.subtitle);
    let proxied = managed.proxied();
    let registered = register(managed, generation, &playback, subtitle, notes, menu);
    RegisteredPlayback(Some(Opened {
        playback: registered,
        start: PlaybackStart {
            url: None,
            hls: hls(&playback.plan),
            plan: playback.plan,
            chosen: playback.chosen,
            size: playback.size,
            proxied,
            markers: playback.markers,
            duration: playback.duration,
        },
    }))
}

/// `old`'s playback, as it is planned now, under a new token, with its
/// subtitle menu, and its subtitle `subtitle` where given; served on the
/// bridge where the player is (`served`).
async fn reopen(
    old: &Registered,
    subtitle: Option<Option<u32>>,
    served: bool,
) -> Result<RegisteredPlayback, SpoolError> {
    let menu = old.stream.as_ref().and_then(|s| s.menu());
    let subtitle = subtitle.unwrap_or(*old.subtitle.lock().expect("never poisoned"));
    let opened = open_under_token(old.managed.clone(), subtitle, old.notes.clone(), menu);
    // Served where `old` was. A failure closes only the new plan's token:
    // `old` and the server's playback stay open, the player on them.
    #[cfg(feature = "hls-bridge")]
    if served {
        return crate::bridge::serve_token(opened).await;
    }
    #[cfg(not(feature = "hls-bridge"))]
    let _ = served;
    Ok(opened)
}
/// Note what happens to `managed` behind the player, for the app's
/// playback log, until it's dropped: its plan or epoch changing, the
/// server moving its share of the uplink, errors in the background.
fn watch(
    managed: &sdk::ManagedPlayback,
    shares: Option<tokio::sync::broadcast::Receiver<sdk::PlaybackShare>>,
    notes: Arc<Mutex<Vec<String>>>,
) -> impl std::future::Future<Output = ()> + Send + 'static {
    // Receivers only: holding the playback would keep it open.
    let mut changes = managed.changes();
    let mut errors = managed.errors();
    let mut shares = shares;
    let note = move |line: String| {
        let mut notes = notes.lock().expect("never poisoned");
        if notes.len() < 200 {
            notes.push(line);
        }
    };
    async move {
        loop {
            let share = async {
                match shares.as_mut() {
                    Some(shares) => shares.recv().await.ok(),
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                changed = changes.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let line = {
                        let now = changes.borrow_and_update();
                        format!("sdk: playback now epoch {} plan {:?}", now.epoch, now.plan)
                    };
                    note(line);
                }
                error = errors.recv() => match error {
                    Ok(error) => note(format!("sdk: in the background: {error}")),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                },
                Some(share) = share => {
                    if share.handle == changes.borrow().handle {
                        let cap = share.max_bitrate.map_or("no limit".to_owned(), |b| {
                            format!("{:.2} Mbit/s", b as f64 / 1e6)
                        });
                        note(format!("server: this playback's share of the uplink moved: {cap}"));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "playback_tests.rs"]
mod tests;
