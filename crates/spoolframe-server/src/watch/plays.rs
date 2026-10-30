//! Play history (PX-52): a row per play, tracks included. [`Plays`]
//! keeps them as an observer of every playback
//! ([`crate::playback::Observer`]): a play opens as its playback starts,
//! moves on with each progress report, and closes as the playback ends,
//! however it ends (its client stops it, it expires, an admin stops it,
//! its session ends). What counts as a play is
//! `spoolframe_policy::plays::counts`.
//!
//! Time played is measured here, not taken from the position: between two
//! reports, what the position moved by, never more than the time that
//! passed on the server's clock, nor than [`MOST_PER_REPORT`]. Pausing
//! adds nothing, nor does a seek, forward or back.
//!
//! Play history never fails or holds up a playback, nor a request: each
//! hook works out the play in memory at once and writes it in the
//! background, and a write that fails is logged. A play whose row cannot
//! be closed stays open in the store, and the next start of the server
//! closes it as it stood ([`Watch::close_open_plays`]).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use spoolframe_policy::plays::{PlayKind, PlayProgress, counts};
use spoolframe_protocol::playlists::Kinds;
use spoolframe_types::{Item, MediaId, MediaKind};

use super::{Watch, WatchError, ms, playback_mut};
use crate::playback::{Observer, Opened};
use crate::principal::Principal;
use crate::store::plays::{NewPlay, PlayUpdate, PlayedKind};

/// The most one progress report can add to a play's time played.
///
/// A player reports every 10 s while it plays (spoolframe-linux's
/// `PROGRESS_EVERY`, and the watch throttle's own 10 s; PROTOCOL.md asks
/// for it), and once a minute while paused; the HTTP playback port
/// reports nothing, its app does. So between two reports of a playing
/// item, 10 s have passed, give or take a timer's slack. Three times that
/// lets two reports be lost and still credits all of it; past it, the
/// gap is a pause, a backgrounded app or a dropped connection, and a
/// position that moved across it is a seek, not play. Such a gap credits
/// at most this much: what the player may have played before it went
/// quiet.
pub const MOST_PER_REPORT: Duration = Duration::from_secs(30);

/// Records every play: an [`Observer`] of the server's playbacks,
/// registered once beside its other playback hooks.
pub struct Plays {
    watch: Arc<Watch>,
    /// The plays open now, by their playback's handle.
    open: parking_lot::Mutex<HashMap<String, Arc<Play>>>,
}

/// A play open now.
struct Play {
    /// Its row, once opened; `None` if it could not be. Held by the task
    /// opening it until then, so every write waits for the row.
    row: Arc<tokio::sync::Mutex<Option<i64>>>,
    /// As far as it has got.
    progress: parking_lot::Mutex<Progress>,
}

/// A play as far as it has got.
#[derive(Debug, Clone, Copy)]
struct Progress {
    kind: PlayKind,
    from: Duration,
    position: Duration,
    /// When the position was last reported (or the play opened).
    at: Instant,
    played: Duration,
    duration: Option<Duration>,
    credits: Option<Duration>,
    counted: bool,
    over_http: bool,
    /// When it was last written.
    written: Instant,
}

impl Progress {
    fn update(&self, close: bool) -> PlayUpdate {
        PlayUpdate {
            played_ms: ms(self.played),
            counted: self.counted,
            over_http: self.over_http,
            close,
        }
    }
}

/// How a play of `kind` is kept: its row's kind, and the rule it counts by.
fn kind_of(kind: MediaKind) -> Option<(PlayedKind, PlayKind)> {
    let played = PlayedKind::of(kind)?;
    let rule = match played {
        PlayedKind::Movie | PlayedKind::Episode => PlayKind::Video,
        PlayedKind::Track => PlayKind::Track,
    };
    Some((played, rule))
}

impl std::fmt::Debug for Plays {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plays")
            .field("open", &self.open.lock().len())
            .finish_non_exhaustive()
    }
}

impl Plays {
    pub fn new(watch: Arc<Watch>) -> Self {
        Self {
            watch,
            open: parking_lot::Mutex::default(),
        }
    }

    /// Write `play` as it stands now, closing it if `close`, in the
    /// background, once its row is open. Each write takes the play as it
    /// is when it runs, so writes that run out of order still leave the
    /// latest; one after the close changes nothing (a closed row is never
    /// rewritten).
    fn write(&self, play: Arc<Play>, close: bool) {
        let store = self.watch.store.clone();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!("a play was moved on outside the runtime; not written");
            return;
        };
        runtime.spawn(async move {
            let Some(row) = *play.row.lock().await else {
                return;
            };
            let update = play.progress.lock().update(close);
            if let Err(e) = store.update_play(row, update).await {
                tracing::warn!(play = row, close, error = %e, "a play could not be written");
            }
        });
    }
}

impl Observer for Plays {
    /// Open the play of `opened`'s item (`Plays::open`).
    fn started(&self, handle: &str, opened: &Opened) {
        // A paired server's user keeps their history at home (P8-03): its
        // server records the play, not this one.
        if matches!(opened.played.principal, Principal::RemoteUser { .. }) {
            return;
        }
        self.open(
            handle,
            &opened.played,
            Where {
                endpoint_id: opened.endpoint.clone(),
                device_name: opened.viewer.device_name.clone(),
                playback_id: Some(opened.trace.id.clone()),
            },
        );
    }

    /// The playback `handle` reported `position`: its play moves on.
    /// Written at most once per throttle interval, and always when it
    /// comes to count; the close writes what is left.
    fn progressed(&self, handle: &str, position: Duration, over_http: bool) {
        self.moved_on(handle, position, over_http);
    }

    /// `handle`'s playback ended, however it ended: its play is closed as
    /// far as it got.
    fn ended(&self, handle: &str, over_http: bool) {
        self.close(handle, over_http);
    }

    /// A paired server's item's play with no report for `idle` is closed
    /// as far as it got, as a playback idle as long is ended (L8).
    fn swept(&self, idle: Duration) {
        let idle: Vec<String> = self
            .open
            .lock()
            .iter()
            .filter(|(handle, play)| {
                handle.starts_with(REMOTE) && self.watch.since(play.progress.lock().at) >= idle
            })
            .map(|(handle, _)| handle.clone())
            .collect();
        for handle in idle {
            self.close(&handle, false);
        }
    }
}

/// Where a play is played from: its device, and its playback's trace,
/// when it has a playback here.
struct Where {
    endpoint_id: String,
    device_name: Option<String>,
    playback_id: Option<String>,
}

/// What the handles of a paired server's items' plays start with: they
/// have no playback here.
const REMOTE: &str = "remote:";

impl Plays {
    /// A paired server's item, `played` from that server (P8-03), as its
    /// user reported being at `position` (`UpdateWatchProgress`) from the
    /// device `endpoint_id`: its play is kept as a playback's is. The
    /// first report opens it, from where it is; each after moves it on;
    /// with no report for as long as a playback may idle, the playbacks'
    /// sweep closes it, and the next report opens another. The server's
    /// next start closes one still open. Nothing of it is the paired
    /// server's: this server is the user's home.
    pub fn remote_progressed(
        &self,
        played: &crate::playback::Played,
        position: Duration,
        endpoint_id: String,
        device_name: Option<String>,
    ) {
        let handle = format!("{REMOTE}{}:{}", played.principal, played.item.id());
        if self.open.lock().contains_key(&handle) {
            self.moved_on(&handle, position, false);
            return;
        }
        let from = crate::playback::Played {
            from: position,
            ..played.clone()
        };
        let at = Where {
            endpoint_id,
            device_name,
            playback_id: None,
        };
        self.open(&handle, &from, at);
    }

    /// Open the play of `played`'s item under `handle`: anything but a
    /// film, an episode or a track has none. In memory now, so whatever
    /// ends it finds it; its row in the background, after the item's
    /// state under an earlier key is brought over ([`Watch::relink`]): the
    /// row takes the item's external ids for its key, and the earlier
    /// key's plays would not be found by them after.
    fn open(&self, handle: &str, played: &crate::playback::Played, at: Where) {
        let Where {
            endpoint_id,
            device_name,
            playback_id,
        } = at;
        let Some((kind, rule)) = kind_of(played.item.id().kind()) else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(item = %played.item.id(), "a play started outside the runtime; not kept");
            return;
        };
        let row = Arc::new(tokio::sync::Mutex::new(None));
        let mut opening = row.clone().try_lock_owned().expect("a new lock is free");
        let now = self.watch.now();
        self.open.lock().insert(
            handle.to_owned(),
            Arc::new(Play {
                row,
                progress: parking_lot::Mutex::new(Progress {
                    kind: rule,
                    from: played.from,
                    position: played.from,
                    at: now,
                    played: Duration::ZERO,
                    duration: played.duration,
                    credits: played.credits,
                    counted: false,
                    over_http: false,
                    written: now,
                }),
            }),
        );
        let watch = self.watch.clone();
        let principal = played.principal;
        let item = played.item.clone();
        let title = played.title_key.clone();
        let duration_ms = played.duration.map(ms);
        runtime.spawn(async move {
            let id = item.id();
            if let Err(e) = watch.relink(principal, id, &title).await {
                tracing::warn!(%id, error = %e, "its state could not be re-linked");
            }
            let media_id = id.to_string();
            let (show_id, album_id, artist_id) = match &item {
                Item::Episode(e) => (Some(e.show_id.to_string()), None, None),
                Item::Track(t) => (
                    None,
                    Some(t.album_id.to_string()),
                    Some(t.artist_id.to_string()),
                ),
                _ => (None, None, None),
            };
            let new = NewPlay {
                media_id: &media_id,
                kind,
                show_id: show_id.as_deref(),
                album_id: album_id.as_deref(),
                artist_id: artist_id.as_deref(),
                external_ids: &title,
                duration_ms,
                endpoint_id: Some(&endpoint_id),
                device_name: device_name.as_deref(),
                playback_id: playback_id.as_deref(),
            };
            match watch.store.open_play(principal, new).await {
                Ok(row) => *opening = Some(row),
                Err(e) => tracing::warn!(%id, error = %e, "its play could not be opened"),
            }
        });
    }

    /// The play `handle` got to `position`: it moves on. Written at most
    /// once per throttle interval, and always when it comes to count; the
    /// close writes what is left.
    fn moved_on(&self, handle: &str, position: Duration, over_http: bool) {
        let Some(play) = self.open.lock().get(handle).cloned() else {
            return;
        };
        let now = self.watch.now();
        let rules = self.watch.rules();
        {
            let mut p = play.progress.lock();
            if position > p.position {
                let moved = position - p.position;
                let passed = now.saturating_duration_since(p.at);
                p.played += moved.min(passed).min(MOST_PER_REPORT);
            }
            p.position = position;
            p.at = now;
            p.over_http |= over_http;
            let was = p.counted;
            p.counted = was
                || counts(
                    &rules,
                    p.kind,
                    &PlayProgress {
                        played: p.played,
                        from: p.from,
                        position,
                        duration: p.duration,
                        credits: p.credits,
                    },
                );
            let due = now.saturating_duration_since(p.written) >= rules.throttle;
            if !due && p.counted == was {
                return;
            }
            p.written = now;
        }
        self.write(play, false);
    }

    /// The play `handle` ended: it is closed as far as it got.
    fn close(&self, handle: &str, over_http: bool) {
        let Some(play) = self.open.lock().remove(handle) else {
            return;
        };
        play.progress.lock().over_http |= over_http;
        self.write(play, true);
    }
}

/// A track played on a device with no server (PX-51).
#[derive(Debug, Clone, Copy)]
pub struct OfflinePlay<'a> {
    /// When it ended, by the device's clock (Unix seconds).
    pub at: i64,
    /// The device's clock now, as it sent this (Unix seconds).
    pub device_now: i64,
    pub played: Duration,
    /// The device: its endpoint id, and the name it gave.
    pub endpoint_id: Option<&'a str>,
    pub device_name: Option<&'a str>,
}

impl Watch {
    /// Record `principal`'s play of track `id` on a device with no server
    /// (PX-51): a closed play, counted as the rules count a track's from
    /// what was played of it, ending at `play.at` with the device's clock
    /// set right as [`Watch::offline`] sets it. Recorded once per device
    /// and device time: sent again, it is `Applied` and nothing more is
    /// written. Anything but a track, or a track not on the server, is
    /// `NotTracked`.
    pub async fn offline_play(
        &self,
        principal: Principal,
        id: &MediaId,
        play: OfflinePlay<'_>,
    ) -> Result<super::Synced, WatchError> {
        use super::Synced;
        if id.kind() != MediaKind::Track {
            return Ok(Synced::NotTracked);
        }
        let Some(found) = self.served_item(id).await? else {
            return Ok(Synced::NotTracked);
        };
        let Item::Track(track) = &found.item else {
            return Ok(Synced::NotTracked);
        };
        let ended = self.store.server_time_of(play.at, play.device_now);
        let played_secs = i64::try_from(play.played.as_secs()).unwrap_or(i64::MAX);
        let duration = self.duration(id, &found.item).await;
        let counted = counts(
            &self.rules(),
            PlayKind::Track,
            &PlayProgress {
                played: play.played,
                from: Duration::ZERO,
                position: play.played,
                duration,
                credits: None,
            },
        );
        let media_id = id.to_string();
        let album_id = track.album_id.to_string();
        let artist_id = track.artist_id.to_string();
        let title = self.title_key(&found).await;
        // What makes it one play: the device's own time of it, which a
        // second send has unchanged (the clock fix may move).
        let key = format!("offline:{}", play.at);
        self.store
            .record_offline_play(
                principal,
                NewPlay {
                    media_id: &media_id,
                    kind: PlayedKind::Track,
                    show_id: None,
                    album_id: Some(&album_id),
                    artist_id: Some(&artist_id),
                    external_ids: &title,
                    duration_ms: duration.map(ms),
                    endpoint_id: play.endpoint_id,
                    device_name: play.device_name,
                    playback_id: Some(&key),
                },
                (ended.saturating_sub(played_secs), ended),
                ms(play.played),
                counted,
            )
            .await?;
        Ok(Synced::Applied)
    }

    /// Close the plays a server that stopped left open, as far as they
    /// had got: at start, before any playback.
    pub async fn close_open_plays(&self) -> Result<u64, WatchError> {
        Ok(self.store.close_open_plays().await?)
    }

    /// Set `play_count` and `last_played_at` on the films, episodes and
    /// tracks in `items`, from `principal`'s counted plays, with one
    /// lookup. Asked for only where a reply carries them.
    pub async fn fill_plays(
        &self,
        principal: Principal,
        items: &mut [&mut Item],
    ) -> Result<(), WatchError> {
        let ids: Vec<String> = items
            .iter()
            .filter(|i| PlayedKind::of(i.id().kind()).is_some())
            .map(|i| i.id().to_string())
            .collect();
        let plays = self.store.play_summaries(principal, &ids).await?;
        for item in items.iter_mut() {
            let summary = plays.get(&item.id().to_string());
            let count = summary.map_or(0, |s| s.count);
            let last = summary.map(|s| s.last);
            let Some(state) = playback_mut(item) else {
                continue;
            };
            state.play_count = count;
            state.last_played_at = last.map(spoolframe_types::serde_time::unix_seconds::from_wire);
        }
        Ok(())
    }

    /// A page of what `principal` played lately: each item once, by its
    /// latest counted play, newest first, of `kinds` (all, for `None`),
    /// with their watch state and plays; and how many there are. An item
    /// gone from the catalog, not served now, or on a Plex server that is
    /// down is left out of the page. A paired server's items only with
    /// `federated` (a client that named the experimental `federation`
    /// feature, P8-02), those no longer available kept and named in the
    /// third list (P8-03).
    pub async fn recently_played(
        &self,
        principal: Principal,
        kinds: Option<Kinds>,
        offset: u32,
        limit: u32,
        federated: bool,
    ) -> Result<(Vec<Item>, u32, Vec<MediaId>), WatchError> {
        let kinds: Vec<PlayedKind> = match kinds {
            None => Vec::new(),
            // Kinds that tick none ask for nothing.
            Some(k) if k.is_empty() => return Ok((Vec::new(), 0, Vec::new())),
            Some(k) => [
                (k.movies, PlayedKind::Movie),
                (k.episodes, PlayedKind::Episode),
                (k.tracks, PlayedKind::Track),
            ]
            .into_iter()
            .filter_map(|(on, kind)| on.then_some(kind))
            .collect(),
        };
        let (ids, total) = self
            .store
            .recently_played(principal, &kinds, offset, limit, federated)
            .await?;
        let (mut items, unavailable) = self.look_up_kept(ids, "Recently played", federated).await?;
        let mut all: Vec<&mut Item> = items.iter_mut().collect();
        self.fill(principal, &mut all).await?;
        self.fill_plays(principal, &mut all).await?;
        Ok((items, total, unavailable))
    }
}
