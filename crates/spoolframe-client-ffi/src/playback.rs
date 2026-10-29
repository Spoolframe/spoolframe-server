//! Playing an item: planned on the server and opened with the SDK's
//! `ManagedPlayback`, under a token the other playback calls take. A
//! player that only takes a URL (AVPlayer, AirPlay) has it served on the
//! loopback bridge (`play`, `playback_url`; the `hls-bridge` feature,
//! `bridge.rs`): `/<token>/master.m3u8` (HLS) or `/<token>/file`
//! (direct), the paths the player and the system subtitle menu depend
//! on. A player that reads the bytes itself (Media3's `DataSource`) opens
//! it with `open_playback` and reads it with a `PlaybackReader`
//! (`reader.rs`), with no bridge.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use spoolframe_client_sdk::types::{Delivery, MediaId};
use spoolframe_client_sdk::{
    self as sdk, ChosenTracks, HttpPlayback, PlaybackPlan, PlaybackQuality, Selection,
};
use spoolframe_hls_bridge::{Kind, MenuEntry, SdkSource, Stream};

use crate::error::SpoolError;
use crate::events::PlaybackShareWatch;
use crate::resources::bridge;
use crate::runtime::run;
use crate::session::SpoolSession;
use crate::values::ScreenCapabilities;

/// A playback, ready for the player.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PlaybackStart {
    /// What a URL player opens, on the loopback bridge; `None` for a
    /// playback only opened (`open_playback`), whose bytes the app reads.
    pub url: Option<String>,
    /// What the other playback calls take.
    pub token: String,
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

/// A playback open under a token: the app's one registry of them.
pub(crate) struct Registered {
    pub(crate) managed: sdk::ManagedPlayback,
    /// The plan it was opened (or served) with, to tell a replan.
    pub(crate) plan: PlaybackPlan,
    /// The subtitle the server plays, if any.
    subtitle: Option<u32>,
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
    /// Served on the loopback bridge: a replan is served there too.
    #[cfg(feature = "hls-bridge")]
    pub(crate) served: AtomicBool,
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
}

/// Open `managed`, planned as `playback`, under a new token, unguessable:
/// the bridge's URL path too. One the bridge reads is registered with it
/// under the same token, its subtitle menu `menu`.
fn register(
    managed: sdk::ManagedPlayback,
    playback: &sdk::Playback,
    subtitle: Option<u32>,
    notes: Arc<Mutex<Vec<String>>>,
    menu: Option<Vec<MenuEntry>>,
) -> String {
    let stream = Kind::of(playback).map(|kind| {
        // The source keeps the plan it's read with, to tell a replan.
        let source = SdkSource::new(managed.clone());
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
    let playback = Registered {
        managed,
        plan: playback.plan.clone(),
        subtitle,
        notes,
        stream,
        stopped: AtomicBool::new(false),
        expired: AtomicBool::new(false),
        resources: Mutex::new(Vec::new()),
        #[cfg(feature = "hls-bridge")]
        served: AtomicBool::new(false),
    };
    PLAYBACKS
        .lock()
        .expect("never poisoned")
        .insert(token.clone(), Arc::new(playback));
    token
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

#[uniffi::export]
impl SpoolSession {
    /// How the server would play `id` here with `tracks` (or, for `None`,
    /// the tracks it picks from the user's preferences). `display` is the
    /// screen's HDR and bit depth now, over what the device said at start.
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
    /// for a player that reads it itself (`read_playback`): no URL.
    /// `end_playback` with its token when done. `display`: the screen's
    /// HDR and bit depth as it is now, kept for this playback's re-plans.
    pub async fn open_playback(
        &self,
        id: MediaId,
        start: Option<Duration>,
        tracks: Option<Selection>,
        quality: PlaybackQuality,
        display: Option<ScreenCapabilities>,
    ) -> Result<PlaybackStart, SpoolError> {
        let opening = open(self.session.clone(), id, start, tracks, quality, display);
        run(opening).await.map(RegisteredPlayback::take)
    }

    /// The direct playback `token` stalls on a link carrying only
    /// `link_bitrate`: the server plans it again for that. A new plan (a
    /// conversion, where the viewer last was) is opened under a new token
    /// (and URL, where `token` was served) for the player to switch to,
    /// and `token` is done; `None` when the plan already fits.
    pub async fn limit_playback(
        &self,
        token: String,
        link_bitrate: u64,
    ) -> Result<Option<PlaybackStart>, SpoolError> {
        replan(token, move |playback| async move {
            playback.limit(link_bitrate).await
        })
        .await
    }

    /// The viewer picked `quality` for the playback `token`, at
    /// `position`: the server plans it again. A new plan (or a new
    /// encode) is opened under a new token for the player to switch to,
    /// and `token` is done; `None` when nothing changed.
    pub async fn change_quality(
        &self,
        token: String,
        quality: PlaybackQuality,
        position: Duration,
    ) -> Result<Option<PlaybackStart>, SpoolError> {
        replan(token, move |playback| async move {
            playback.change_quality(quality, position).await
        })
        .await
    }

    /// Whether the SDK planned the playback `token` again behind the
    /// player (an Auto playback on a slow link, or its share of the
    /// server's uplink moved): `follow_replan` for the new token.
    pub fn playback_replanned(&self, token: String) -> bool {
        registered(&token).is_some_and(|p| !p.managed.is_planned(&p.plan))
    }

    /// The playback `token`, planned again behind the player, opened as
    /// it is now under a new token (and URL, where `token` was served)
    /// for the player to switch to, where it was; `token` is done.
    pub async fn follow_replan(&self, token: String) -> Result<PlaybackStart, SpoolError> {
        let Some(old) = unregister(&token) else {
            return Err(SpoolError::PlaybackExpired);
        };
        run(async move { reopen(&old).await })
            .await
            .map(RegisteredPlayback::take)
    }

    /// What happened to the playback `token` behind the player since last
    /// asked, for the playback log: plan changes, share events, errors.
    pub fn playback_notes(&self, token: String) -> Vec<String> {
        registered(&token)
            .map(|p| std::mem::take(&mut *p.notes.lock().expect("never poisoned")))
            .unwrap_or_default()
    }

    /// Have an HLS playback's master playlist name every subtitle track
    /// of its item, so the system player's menu lists them, in this
    /// order. Call before the player opens the URL; false for no such
    /// playback.
    pub fn subtitle_menu(&self, token: String, entries: Vec<SubtitleEntry>) -> bool {
        let Some(playback) = registered(&token) else {
            return false;
        };
        if let Some(stream) = &playback.stream {
            stream.set_menu(entries.into_iter().map(Into::into).collect());
        }
        true
    }

    /// End a playback: no more reads, the bridge stops serving it, and the
    /// server stops its encode.
    pub async fn end_playback(&self, token: String) {
        let Some(playback) = unregister(&token) else {
            return;
        };
        run(async move {
            let _ = playback.managed.close().await;
        })
        .await
    }

    /// The server's admin stopped the playback `token`: the player says
    /// it ended there, not that something failed.
    pub fn playback_stopped(&self, token: String) -> bool {
        registered(&token).is_some_and(|p| p.stopped())
    }

    /// The server ended the playback `token` under the player (it
    /// expired, or a reconnect left it behind): start a new one at the
    /// same place.
    pub fn playback_expired(&self, token: String) -> bool {
        registered(&token).is_some_and(|p| p.expired())
    }

    /// The playback `token` over the server's own HTTP port, for an
    /// AirPlay receiver, which can't reach the loopback bridge: its URL,
    /// or why not. It's the playback's handle now: one the SDK started
    /// again needs a new one (the old answers 404).
    pub async fn http_playback_url(&self, token: String) -> Result<HttpPlayback, SpoolError> {
        let Some(playback) = registered(&token) else {
            return Err(SpoolError::PlaybackExpired);
        };
        Ok(run(async move { playback.managed.http_playback_url().await }).await?)
    }

    /// Where the playback `token` has got to: now and then while playing,
    /// and on pause and stop.
    pub async fn update_progress(
        &self,
        token: String,
        position: Duration,
    ) -> Result<(), SpoolError> {
        let Some(playback) = registered(&token).map(|p| p.managed.clone()) else {
            return Err(SpoolError::PlaybackExpired);
        };
        // The SDK resumes a playback the server ended from here, too; a
        // paired server's item's progress goes to the user's own server.
        playback.report_position(position)?;
        Ok(run(async move { playback.flush().await }).await?)
    }

    /// The playback `token`'s share of the server's uplink as the server
    /// moves it; `None` for no such playback.
    pub fn watch_playback_share(&self, token: String) -> Option<Arc<PlaybackShareWatch>> {
        let playback = registered(&token)?.managed.clone();
        let shares = sdk::PlaybackBackend::shares(&self.session)?;
        Some(Arc::new(PlaybackShareWatch::new(playback, shares)))
    }
}

/// Own a token until the foreign caller actually receives it. Dropping a
/// cancelled runtime task's completed result closes the token (and its
/// URL) too.
pub(crate) struct RegisteredPlayback(pub(crate) Option<PlaybackStart>);

impl RegisteredPlayback {
    pub(crate) fn take(mut self) -> PlaybackStart {
        self.0.take().expect("owned result")
    }
}

impl Drop for RegisteredPlayback {
    fn drop(&mut self) {
        if let Some(start) = self.0.take() {
            unregister(&start.token);
        }
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
    tokio::spawn(watch(
        &managed,
        sdk::PlaybackBackend::shares(&session),
        notes.clone(),
    ));
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
    let playback = managed.snapshot();
    let subtitle = playback
        .chosen
        .as_ref()
        .map_or(subtitle, |c| c.selection.subtitle);
    let proxied = managed.proxied();
    let token = register(managed, &playback, subtitle, notes, menu);
    RegisteredPlayback(Some(PlaybackStart {
        url: None,
        hls: hls(&playback.plan),
        token,
        plan: playback.plan,
        chosen: playback.chosen,
        size: playback.size,
        proxied,
    }))
}

/// `old`'s playback, as it is planned now, under a new token, with its
/// subtitle menu; served on the bridge where `old` was.
async fn reopen(old: &Registered) -> Result<RegisteredPlayback, SpoolError> {
    let menu = old.stream.as_ref().and_then(|s| s.menu());
    let opened = open_under_token(old.managed.clone(), old.subtitle, old.notes.clone(), menu);
    #[cfg(feature = "hls-bridge")]
    if old.served.load(Ordering::Relaxed) {
        return crate::bridge::serve_opened(opened).await;
    }
    Ok(opened)
}

/// Plan `token`'s playback again with `change`: one that comes back with
/// another plan or encode is opened under a new token (this one's done),
/// for the player to switch to.
async fn replan<F, Fut>(token: String, change: F) -> Result<Option<PlaybackStart>, SpoolError>
where
    F: FnOnce(sdk::ManagedPlayback) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<sdk::Playback, sdk::SdkError>> + Send,
{
    let Some(playback) = registered(&token).map(|p| p.managed.clone()) else {
        return Err(SpoolError::PlaybackExpired);
    };
    run(async move {
        let before = playback.snapshot();
        let after = change(playback).await?;
        // Another plan, or another encode of it: the player switches.
        if after.plan == before.plan && after.epoch == before.epoch {
            return Ok(None);
        }
        let Some(registered) = unregister(&token) else {
            return Err(SpoolError::PlaybackExpired);
        };
        reopen(&registered).await.map(Some)
    })
    .await
    .map(|start| start.map(RegisteredPlayback::take))
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
