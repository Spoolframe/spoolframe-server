//! Music offline (PX-51, feature `spoolframe:music-downloads`): tracks
//! downloaded on purpose, and tracks kept from listening.
//!
//! A download is of a track, an album, an artist, a playlist or Liked
//! ([`MusicTarget`]). Each track is one stored file per quality, held by
//! every download that covers it ([`super::store::Hold`]): an album and a
//! playlist with the same track keep one file, which goes only when
//! neither holds it. An artist, a playlist and Liked are followed: each
//! time a session of the user on that server is made or found, the device
//! asks what they hold now (`MusicDownloadChanges`) and compares it with
//! what it keeps, fetching what was added and letting go of what was
//! removed. The server answers each with its whole set; the device does
//! the diff.
//!
//! While connected, it follows edits too ([`follow_while_connected`]): a
//! `PlaylistChanged` or `PlaylistGone` of a playlist downloaded, or a
//! `LikedChanged`, asks about that one [`FOLLOW_DEBOUNCE`] later, so a
//! burst of edits is one ask. An artist's new album has no event: every
//! followed download is asked about every [`FOLLOW_EVERY`] while the
//! session's connection is up.
//!
//! Tracks kept from listening are in a cache with a cap
//! ([`DownloadSettings::cache_max_bytes`]): when it is over, the least
//! recently played go first. Downloads never count against it. A track
//! downloaded in the quality it was kept in moves from the cache to the
//! downloads with nothing fetched: the server's file has the kept file's
//! size and SHA-256.
//!
//! Plays and likes made with no server ([`Downloads::record_play`],
//! [`Downloads::record_like`]) are sent with `SyncOfflinePlays` when a
//! session is made or found, before the followed downloads are asked
//! about.
//!
//! Each album a download reads (an album's own, or a track's album while
//! its details are fetched) has its tracklist kept in `music.json` while
//! a track of it is here: [`Downloads::offline_music`] shows the album's
//! songs not on the device beside those that are.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Duration;

use iroh::EndpointId;
use tokio::time::Instant;

use serde::{Deserialize, Serialize};
use spoolframe_protocol::downloads::{
    DownloadSource, DownloadTracks, Followed, FollowedTracks, MAX_ASKED, MAX_FOLLOWED, OfflineLike,
    OfflinePlay, OfflineWatchOutcome,
};
use spoolframe_protocol::features::{LIKED, MUSIC_DOWNLOADS};
use spoolframe_protocol::{Reply, Request};
use spoolframe_types::{Item, MediaId, MediaKind, MediaTracks};

use super::store::{
    self, EntryKind, Hold, Manifest, Store, kept_key, key_of, read_json, write_json,
};
use super::{Downloads, LocalArtwork, LocalDownload, LocalWatch, Shared, disk, manager, now};
use crate::client::{Client, Inner};
use crate::{SdkError, Session};

/// How long after a followed playlist's or Liked's change it is asked
/// about: the edits of a burst are asked about once.
const FOLLOW_DEBOUNCE: Duration = Duration::from_secs(2);
/// How often every followed download is asked about while connected: an
/// artist's new album has no event. The cursor makes an unchanged set
/// cost nothing.
const FOLLOW_EVERY: Duration = Duration::from_secs(15 * 60);

/// The quality music is downloaded in: a setting of this device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MusicQuality {
    /// The source file when this device plays it as it is; else the
    /// closest it plays (lossless stays lossless).
    #[default]
    Original,
    /// AAC at 256 kbit/s: smaller, for a device short of space.
    Aac256,
}

impl MusicQuality {
    /// Every quality: a track's entries are found by these, so a new one
    /// is added here too.
    const ALL: [Self; 2] = [Self::Original, Self::Aac256];

    fn source(self) -> DownloadSource {
        match self {
            Self::Original => DownloadSource::Original,
            Self::Aac256 => DownloadSource::Aac256,
        }
    }
}

/// How this device reaches the network now, as the app tells it
/// ([`Downloads::set_network`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NetworkKind {
    /// Wi-Fi or wired: downloads move.
    #[default]
    Unmetered,
    /// A mobile network: downloads wait, unless
    /// [`DownloadSettings::allow_cellular`].
    Cellular,
}

/// This device's download settings, kept in the downloads directory
/// ([`Downloads::settings`], [`Downloads::set_settings`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DownloadSettings {
    /// The quality music downloads are made in. A change applies to
    /// downloads started after it.
    pub music_quality: MusicQuality,
    /// Downloads move on a mobile network too. Off by default: on one,
    /// every download waits until the device is on Wi-Fi or wired again.
    pub allow_cellular: bool,
    /// The most bytes tracks kept from listening take; the least recently
    /// played go first past it. Downloads never count.
    pub cache_max_bytes: u64,
}

/// The cache's cap by default: 2 GiB.
pub const DEFAULT_CACHE_MAX_BYTES: u64 = 2 << 30;

impl Default for DownloadSettings {
    fn default() -> Self {
        Self {
            music_quality: MusicQuality::Original,
            allow_cellular: false,
            cache_max_bytes: DEFAULT_CACHE_MAX_BYTES,
        }
    }
}

/// What a music download is of.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MusicTarget {
    Track {
        id: MediaId,
    },
    Album {
        id: MediaId,
    },
    /// Every album of the artist, and the ones the library gains later.
    Artist {
        id: MediaId,
    },
    /// A playlist (PX-40), in sync: tracks added come down, tracks
    /// removed go unless another download holds them.
    Playlist {
        id: String,
    },
    /// The user's Liked (PX-43), in sync as a playlist.
    Liked,
}

impl MusicTarget {
    /// The download whose hold `hold` is, on `track`; `None` for the
    /// cache of what was kept from listening, and for a hold this build
    /// does not know.
    fn of_hold(hold: &Hold, track: &MediaId) -> Option<Self> {
        let media = || hold.id.as_deref().and_then(|id| id.parse::<MediaId>().ok());
        match hold.by.as_str() {
            "download" => Some(Self::Track { id: track.clone() }),
            "album" => media().map(|id| Self::Album { id }),
            "artist" => media().map(|id| Self::Artist { id }),
            "playlist" => hold.id.clone().map(|id| Self::Playlist { id }),
            "liked" => Some(Self::Liked),
            _ => None,
        }
    }

    /// The hold its tracks get.
    fn hold(&self, user_id: &str) -> Hold {
        let (by, id) = match self {
            Self::Track { .. } => return Hold::download(user_id),
            Self::Album { id } => ("album", Some(id.to_string())),
            Self::Artist { id } => ("artist", Some(id.to_string())),
            Self::Playlist { id } => ("playlist", Some(id.clone())),
            Self::Liked => ("liked", None),
        };
        Hold {
            by: by.into(),
            id,
            user: Some(user_id.to_owned()),
        }
    }

    /// A track's, an album's or an artist's id, and the kind it must be.
    fn media_kind(&self) -> Option<(&MediaId, MediaKind)> {
        match self {
            Self::Track { id } => Some((id, MediaKind::Track)),
            Self::Album { id } => Some((id, MediaKind::Album)),
            Self::Artist { id } => Some((id, MediaKind::Artist)),
            Self::Playlist { .. } | Self::Liked => None,
        }
    }

    /// What the server is asked about, for one it follows.
    fn followed(&self) -> Option<Followed> {
        match self {
            Self::Artist { id } => Some(Followed::Artist { id: id.clone() }),
            Self::Playlist { id } => Some(Followed::Playlist { id: id.clone() }),
            Self::Liked => Some(Followed::Liked),
            Self::Track { .. } | Self::Album { .. } => None,
        }
    }
}

/// [`SdkError::InvalidInput`] unless `id` is of `kind`.
fn expect_kind(id: &MediaId, kind: MediaKind) -> Result<(), SdkError> {
    if id.kind() == kind {
        Ok(())
    } else {
        Err(SdkError::InvalidInput(format!("{id} is not a {kind:?}")))
    }
}

/// A music download, as the Downloads page shows it
/// ([`Downloads::music_downloads`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MusicDownload {
    /// Its key here: what [`Downloads::remove_music`] takes.
    pub key: String,
    pub server: String,
    pub user_id: String,
    pub target: MusicTarget,
    /// The track's or album's title, the artist's or playlist's name, or
    /// "Liked".
    pub name: String,
    pub quality: MusicQuality,
    /// Its tracks now, and how many are on this device.
    pub tracks: u32,
    pub done: u32,
    /// Bytes on this device, and bytes in all where the server has said
    /// (the tracks not prepared yet are not counted).
    pub received: u64,
    pub size: u64,
    /// Unix seconds, by this device's clock.
    pub created_at: i64,
}

/// How a track came to be on this device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfflineMark {
    /// Downloaded: it stays until the user removes it.
    Download,
    /// Kept from listening: in the cache, which may let it go.
    Kept,
}

/// A track to play with no server ([`Downloads::offline_music`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineTrack {
    /// Its entry's key: what [`Downloads::record_play`] and
    /// [`Downloads::record_like`] take.
    pub key: String,
    /// The track, as the server had it.
    pub item: Item,
    pub mark: OfflineMark,
    pub file_path: PathBuf,
    /// `file_path` as a `file://` URL.
    pub file_url: Option<String>,
    /// Its cover (the album's) and its artist's picture, kept beside it.
    pub artwork: Vec<LocalArtwork>,
    pub size: u64,
    /// When it was last played here, Unix seconds by this device's clock.
    pub last_played_at: Option<i64>,
    /// The user's downloads that hold it: [`MusicTarget::Track`] for the
    /// track downloaded itself, else its album, an artist, a playlist or
    /// Liked. Empty for one only kept from listening.
    pub held_by: Vec<MusicTarget>,
}

/// An album with the tracks of it on this device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineAlbum {
    pub id: MediaId,
    /// As the server had it, when it was asked; else only `title`.
    pub album: Option<Item>,
    pub title: String,
    /// By disc, then by place on it.
    pub tracks: Vec<OfflineTrack>,
    /// How many tracks the album has in all, here or not ("3 of 12
    /// songs"): its tracklist's, else as the server counted them; `None`
    /// when neither is known.
    pub total_tracks: Option<u32>,
    /// Every track of the album, in its order, each marked whether it is
    /// on this device: to show the others dimmed. Only those here, when
    /// no download read its tracklist (an album, an artist, a playlist or
    /// a track downloaded reads it; a track kept from listening does
    /// not).
    pub tracklist: Vec<AlbumTrack>,
}

impl OfflineAlbum {
    /// Its tracklist and count, its `tracks` here sorted: `kept`, the
    /// tracklist a download read, each marked whether it is here, then
    /// any track here it does not list (the album changed since); else
    /// the tracks here alone. The count is the tracklist's when one was
    /// kept, else the album's own, else what its tracks said.
    fn fill_tracklist(&mut self, kept: Option<&[Item]>) {
        let here: HashSet<&MediaId> = self.tracks.iter().map(|t| t.item.id()).collect();
        let mut tracklist: Vec<AlbumTrack> = kept
            .unwrap_or_default()
            .iter()
            .map(|item| AlbumTrack {
                on_device: here.contains(item.id()),
                item: item.clone(),
            })
            .collect();
        let listed: HashSet<MediaId> = tracklist.iter().map(|t| t.item.id().clone()).collect();
        tracklist.extend(
            self.tracks
                .iter()
                .filter(|t| !listed.contains(t.item.id()))
                .map(|t| AlbumTrack {
                    item: t.item.clone(),
                    on_device: true,
                }),
        );
        let counted = match &self.album {
            Some(Item::Album(album)) => album.track_count,
            _ => None,
        };
        self.total_tracks = match kept {
            Some(_) => u32::try_from(tracklist.len()).ok(),
            None => counted.or(self.total_tracks),
        };
        self.tracklist = tracklist;
    }
}

/// One track of an album's tracklist ([`OfflineAlbum::tracklist`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumTrack {
    /// The track, as the server had it.
    pub item: Item,
    /// It is in [`OfflineAlbum::tracks`]: it plays here.
    pub on_device: bool,
}

/// An artist with the albums of theirs on this device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineArtist {
    pub id: MediaId,
    pub artist: Option<Item>,
    pub name: String,
    /// Oldest first.
    pub albums: Vec<OfflineAlbum>,
}

/// A playlist or Liked downloaded, with its tracks on this device, in
/// its order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflinePlaylist {
    /// [`MusicTarget::Playlist`] or [`MusicTarget::Liked`].
    pub target: MusicTarget,
    pub name: String,
    pub tracks: Vec<OfflineTrack>,
}

/// A user's music on this device, made up from what is here, with no
/// server ([`Downloads::offline_music`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OfflineMusic {
    /// By name.
    pub artists: Vec<OfflineArtist>,
    /// The playlists and Liked downloaded.
    pub playlists: Vec<OfflinePlaylist>,
}

/// What became of one play or like sent ([`Downloads::sync_music`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncedPlay {
    pub track: MediaId,
    /// `Unknown`: kept, and sent again next time.
    pub outcome: OfflineWatchOutcome,
}

/// What [`Downloads::sync_music`] sent and found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MusicSynced {
    pub plays: Vec<SyncedPlay>,
    pub liked: Vec<SyncedPlay>,
}

/// How full the cache of tracks kept from listening is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheSpace {
    pub used: u64,
    /// [`DownloadSettings::cache_max_bytes`].
    pub max: u64,
}

/// A music download, as kept in `music.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredMusic {
    key: String,
    target: MusicTarget,
    name: String,
    quality: MusicQuality,
    /// Its tracks as last known, in order.
    tracks: Vec<MediaId>,
    created_at: i64,
}

/// A user's music downloads on a server.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct MusicState {
    /// The last `MusicDownloadChanges` cursor: unchanged sets cost nothing.
    cursor: Option<String>,
    downloads: Vec<StoredMusic>,
    /// The tracklists of the albums with tracks here, as the server had
    /// them when a download last read them: for showing the songs not on
    /// the device. None in a `music.json` from before.
    #[serde(default)]
    albums: Vec<StoredAlbum>,
}

/// An album's tracklist, as kept in `music.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredAlbum {
    id: MediaId,
    /// Every track of it, in the server's order.
    tracks: Vec<Item>,
}

/// Plays and likes made here, not sent yet.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PendingPlays {
    plays: Vec<OfflinePlay>,
    liked: Vec<OfflineLike>,
}

impl Session {
    /// [`SdkError::Unsupported`] unless the session has music downloads.
    pub(crate) fn music_offered(&self) -> Result<(), SdkError> {
        if self.features.downloads && self.features.music_downloads {
            Ok(())
        } else {
            Err(SdkError::Unsupported {
                feature: MUSIC_DOWNLOADS.into(),
            })
        }
    }
}

impl Downloads {
    /// Download `target` as `session`'s user, in
    /// [`DownloadSettings::music_quality`]: its tracks are listed now
    /// (so a refusal is this call's error), each with its details and
    /// artwork, and the manager prepares and moves them. Started again,
    /// the same download, with any track it gained.
    pub async fn start_music(
        &self,
        session: &Session,
        target: MusicTarget,
    ) -> Result<MusicDownload, SdkError> {
        let shared = self.shared();
        let store = shared.store()?;
        session.music_offered()?;
        if let Some((id, kind)) = target.media_kind() {
            expect_kind(id, kind)?;
        }
        if target == MusicTarget::Liked && !session.features.liked {
            return Err(SdkError::Unsupported {
                feature: LIKED.into(),
            });
        }
        shared.session_made(session);
        let server = session.server();
        let user_id = session.user().user_id.clone();
        let quality = shared.settings().music_quality;
        let mut parents = Parents::default();
        let (name, tracks) = listed(session, &target, &mut parents).await?;
        let key = music_key(&server, &user_id, &target);
        let mut dropped = Vec::new();
        let stored = {
            let _writing = shared.manifests.lock().await;
            let path = store.music_path(&server, &user_id);
            let mut state: MusicState = read_json(&path);
            let ids: Vec<MediaId> = tracks.iter().map(|(id, _)| id.clone()).collect();
            let stored = match state.downloads.iter_mut().find(|d| d.key == key) {
                Some(stored) => {
                    // Started again: what it no longer has is let go of.
                    let now_held: HashSet<&MediaId> = ids.iter().collect();
                    dropped = stored
                        .tracks
                        .iter()
                        .filter(|id| !now_held.contains(id))
                        .cloned()
                        .collect();
                    stored.tracks = ids;
                    stored.name = name;
                    stored.clone()
                }
                None => {
                    let stored = StoredMusic {
                        key: key.clone(),
                        target: target.clone(),
                        name,
                        quality,
                        tracks: ids,
                        created_at: now(),
                    };
                    state.downloads.push(stored.clone());
                    stored
                }
            };
            write_json(&path, &state).map_err(disk)?;
            stored
        };
        let hold = target.hold(&user_id);
        let source = stored.quality.source();
        self.release_tracks(&server, &source, &dropped, &hold)
            .await?;
        for (place, (id, item)) in (0..).zip(tracks) {
            if let Err(e) = hold_track(
                shared,
                session,
                &id,
                item,
                &source,
                &hold,
                place,
                &mut parents,
            )
            .await
            {
                tracing::info!(%id, error = %e, "a track of a music download is left out");
            }
        }
        // The download stands without them: only the songs not here go
        // unshown.
        if let Err(e) = record_albums(shared, &server, &user_id, parents.albums).await {
            tracing::warn!(error = %e, "the tracklists of a music download's albums not kept");
        }
        shared.wake.notify_one();
        Ok(shared.music_view(store, &server, &user_id, &stored))
    }

    /// Take music download `key` off this device: each of its tracks no
    /// other download holds goes (unfinished ones are cancelled on the
    /// server, if it can be reached now). Needs no server.
    pub async fn remove_music(&self, key: &str) -> Result<(), SdkError> {
        let shared = self.shared();
        let store = shared.store()?;
        let Some((server, user_id, stored)) = shared.find_music(store, key) else {
            return Err(SdkError::NotFound(format!("music download {key}")));
        };
        {
            let _writing = shared.manifests.lock().await;
            let path = store.music_path(&server, &user_id);
            let mut state: MusicState = read_json(&path);
            state.downloads.retain(|d| d.key != key);
            write_json(&path, &state).map_err(disk)?;
        }
        let source = stored.quality.source();
        self.release_tracks(
            &server,
            &source,
            &stored.tracks,
            &stored.target.hold(&user_id),
        )
        .await
    }

    /// Let go of `hold` on the tracks of `ids` here in `source`.
    async fn release_tracks(
        &self,
        server: &str,
        source: &DownloadSource,
        ids: &[MediaId],
        hold: &Hold,
    ) -> Result<(), SdkError> {
        let store = self.shared().store()?;
        for id in ids {
            let key = track_key(server, id, source);
            if let Some(m) = store.find_track(server, &key) {
                self.release(m, hold).await?;
            }
        }
        Ok(())
    }

    /// Every music download on this device, oldest first, from disk
    /// alone.
    pub fn music_downloads(&self) -> Vec<MusicDownload> {
        let shared = self.shared();
        let Some(store) = &shared.store else {
            return Vec::new();
        };
        let mut all = Vec::new();
        let active = shared.locks.active();
        for (server, user_id) in store.users_with(store::MUSIC_FILE) {
            if active
                .as_ref()
                .is_some_and(|a| *a != (server.clone(), user_id.clone()))
                || shared.locks.is_locked(&server, &user_id)
            {
                continue;
            }
            let state: MusicState = read_json(&store.music_path(&server, &user_id));
            for stored in &state.downloads {
                all.push(shared.music_view(store, &server, &user_id, stored));
            }
        }
        all.sort_by(|a, b| (a.created_at, &a.key).cmp(&(b.created_at, &b.key)));
        all
    }

    /// Every track on this device, downloaded or kept from listening, in
    /// any phase: each as a download.
    pub fn music_tracks(&self) -> Vec<LocalDownload> {
        let shared = self.shared();
        match &shared.store {
            Some(store) => store
                .scan()
                .iter()
                .filter(|m| m.kind == EntryKind::Track && shared.shown(m).is_ok())
                .map(|m| shared.view(m))
                .collect(),
            None => Vec::new(),
        }
    }

    /// `user_id`'s music on `server` that can be played here now, made up
    /// from what is on this device: artists, their albums and tracks, and
    /// the playlists and Liked downloaded. Each track is marked a
    /// download or kept. Needs no server.
    pub fn offline_music(&self, server: &str, user_id: &str) -> OfflineMusic {
        self.shared().offline_music(server, user_id)
    }

    /// This device's download settings.
    pub fn settings(&self) -> DownloadSettings {
        self.shared().settings()
    }

    /// Change this device's download settings. A smaller cache cap lets
    /// the least recently played go now; cellular allowed or not takes
    /// effect at once.
    pub async fn set_settings(&self, settings: DownloadSettings) -> Result<(), SdkError> {
        let shared = self.shared();
        let store = shared.store()?;
        write_json(&store.settings_path(), &settings).map_err(disk)?;
        *shared.settings.lock() = Some(settings);
        shared.network_changed();
        evict(shared, None).await
    }

    /// How this device reaches the network now: on a mobile network,
    /// downloads wait unless [`DownloadSettings::allow_cellular`], and go
    /// on when it is back on Wi-Fi or wired.
    pub fn set_network(&self, network: NetworkKind) {
        let shared = self.shared();
        *shared.network.lock() = network;
        shared.network_changed();
    }

    /// Bytes the tracks kept from listening take, and the cap.
    pub fn cache(&self) -> CacheSpace {
        let shared = self.shared();
        let used = shared
            .store
            .as_ref()
            .map_or(0, |store| cached(store).iter().map(entry_size).sum());
        CacheSpace {
            used,
            max: shared.settings().cache_max_bytes,
        }
    }

    /// Empty the cache of tracks kept from listening; downloads stay.
    pub async fn clear_cache(&self) -> Result<(), SdkError> {
        let shared = self.shared();
        let store = shared.store()?;
        for m in store.scan() {
            if m.holds.iter().any(Hold::is_listening) {
                self.release(m, &Hold::listening()).await?;
            }
        }
        Ok(())
    }

    /// The user played track `key` here, from its file, for `played`: the
    /// play is sent the next time the server is reached (play counts,
    /// last played), and the track counts as just played for the cache.
    pub async fn record_play(&self, key: &str, played: Duration) -> Result<(), SdkError> {
        let shared = self.shared();
        let store = shared.store()?;
        let m = store
            .find(key)
            .ok_or_else(|| SdkError::NotFound(format!("track {key}")))?;
        shared.shown(&m)?;
        let user_id = shared.acting(&m);
        let m = shared
            .update(key, |m| {
                if m.kind != EntryKind::Track {
                    return Err(SdkError::InvalidInput(format!("{key} is not a track")));
                }
                m.last_played_at = Some(now());
                Ok(())
            })
            .await?;
        let play = OfflinePlay {
            track: m.item.id().clone(),
            at: now(),
            played_ms: played.as_millis() as u64,
        };
        shared
            .pending(store, &m.server, &user_id, |p| p.plays.push(play))
            .await
    }

    /// The user liked track `key` here, or unliked it: sent the next time
    /// the server is reached, where the newest change wins.
    pub async fn record_like(&self, key: &str, liked: bool) -> Result<(), SdkError> {
        let shared = self.shared();
        let store = shared.store()?;
        let m = store
            .find(key)
            .filter(|m| m.kind == EntryKind::Track)
            .ok_or_else(|| SdkError::NotFound(format!("track {key}")))?;
        shared.shown(&m)?;
        let like = OfflineLike {
            track: m.item.id().clone(),
            liked,
            at: now(),
        };
        shared
            .pending(store, &m.server, &shared.acting(&m), |p| {
                p.liked.retain(|l| l.track != like.track);
                p.liked.push(like);
            })
            .await
    }

    /// Send what `session`'s user played and liked here with no server,
    /// then ask what the artists, playlists and Liked they downloaded hold
    /// now, fetching what was added and letting go of what was removed.
    /// The manager does this by itself when a session is made or found.
    pub async fn sync_music(&self, session: &Session) -> Result<MusicSynced, SdkError> {
        session.music_offered()?;
        sync(self.shared(), session).await
    }
}

/// `target`'s name and tracks now: the track itself, an album's tracks
/// with their details, or what the server says an artist, a playlist or
/// Liked holds (their details fetched later).
async fn listed(
    session: &Session,
    target: &MusicTarget,
    parents: &mut Parents,
) -> Result<(String, Vec<(MediaId, Option<Item>)>), SdkError> {
    match target {
        MusicTarget::Track { id } => {
            let detail = session.item(id).await?;
            let Item::Track(t) = &detail.item else {
                return Err(SdkError::InvalidInput(format!("{id} is not a track")));
            };
            Ok((t.title.clone(), vec![(id.clone(), Some(detail.item))]))
        }
        MusicTarget::Album { id } => {
            let detail = session.item(id).await?;
            let Item::Album(album) = &detail.item else {
                return Err(SdkError::InvalidInput(format!("{id} is not an album")));
            };
            let name = album.title.clone();
            parents.items.insert(id.clone(), Some(detail.item.clone()));
            parents.album_read(&detail.item, detail.children);
            let tracks = parents.albums[id]
                .iter()
                .map(|c| (c.id().clone(), Some(c.clone())))
                .collect();
            Ok((name, tracks))
        }
        MusicTarget::Artist { id } => {
            let detail = session.item(id).await?;
            let Item::Artist(artist) = &detail.item else {
                return Err(SdkError::InvalidInput(format!("{id} is not an artist")));
            };
            let name = artist.name.clone();
            parents.items.insert(id.clone(), Some(detail.item.clone()));
            Ok((name, held(session, target).await?))
        }
        MusicTarget::Playlist { id } => {
            let page = session.playlist(id, 0, 1).await?;
            Ok((page.playlist.name, held(session, target).await?))
        }
        MusicTarget::Liked => Ok(("Liked".to_owned(), held(session, target).await?)),
    }
}

/// What followed `target` holds now, by `MusicDownloadChanges` with no
/// cursor.
async fn held(
    session: &Session,
    target: &MusicTarget,
) -> Result<Vec<(MediaId, Option<Item>)>, SdkError> {
    let followed = target.followed().expect("a followed target");
    let (mut changes, _) = changes(session, std::slice::from_ref(&followed), None).await?;
    match changes.pop().map(|c| c.tracks) {
        Some(FollowedTracks::Tracks { tracks }) => {
            Ok(tracks.into_iter().map(|id| (id, None)).collect())
        }
        Some(FollowedTracks::Gone) => Err(SdkError::NotFound(format!("{followed:?}"))),
        other => Err(SdkError::Protocol(format!(
            "unexpected tracks of {followed:?}: {other:?}"
        ))),
    }
}

async fn changes(
    session: &Session,
    followed: &[Followed],
    since: Option<String>,
) -> Result<(Vec<spoolframe_protocol::downloads::FollowedChange>, String), SdkError> {
    let mut artists = Vec::new();
    let mut playlists = Vec::new();
    let mut liked = false;
    for f in followed {
        match f {
            Followed::Artist { id } => artists.push(id.clone()),
            Followed::Playlist { id } => playlists.push(id.clone()),
            Followed::Liked => liked = true,
            Followed::Unknown => {}
        }
    }
    let request = Request::MusicDownloadChanges {
        artists,
        playlists,
        liked,
        since,
    };
    match session.call(request).await? {
        Reply::MusicDownloadChanges { changes, cursor } => Ok((changes, cursor)),
        other => Err(SdkError::Protocol(format!("unexpected reply {other:?}"))),
    }
}

/// Albums, artists and their pictures fetched once per sync or start:
/// many tracks share one.
#[derive(Default)]
struct Parents {
    /// `None` for one that could not be read.
    items: HashMap<MediaId, Option<Item>>,
    /// The tracklists of the albums read, by album: kept in `music.json`
    /// once the tracks are held ([`record_albums`]).
    albums: HashMap<MediaId, Vec<Item>>,
    art: manager::KeptArt,
}

impl Parents {
    async fn get(&mut self, session: &Session, id: &MediaId) -> Option<Item> {
        if !self.items.contains_key(id) {
            let item = match session.item(id).await {
                Ok(detail) => {
                    self.album_read(&detail.item, detail.children);
                    Some(detail.item)
                }
                Err(_) => None,
            };
            self.items.insert(id.clone(), item);
        }
        self.items[id].clone()
    }

    /// `item` was read with `children`: an album's are its tracklist.
    fn album_read(&mut self, item: &Item, children: Vec<Item>) {
        if matches!(item, Item::Album(_)) {
            let tracks = children
                .into_iter()
                .filter(|c| matches!(c, Item::Track(_)))
                .collect();
            self.albums.insert(item.id().clone(), tracks);
        }
    }
}

/// Track `id` in `source` held by `hold`, at `place` in what holds it:
/// added to its entry if there is one, else a new entry with its details
/// (`item`, or fetched), its album
/// and artist and their artwork, for the manager to prepare.
#[allow(clippy::too_many_arguments)]
async fn hold_track(
    shared: &Shared,
    session: &Session,
    id: &MediaId,
    item: Option<Item>,
    source: &DownloadSource,
    hold: &Hold,
    place: u32,
    parents: &mut Parents,
) -> Result<(), SdkError> {
    let store = shared.store()?;
    let server = session.server();
    let user = session.user().clone();
    let key = track_key(&server, id, source);
    if store.find_track(&server, &key).is_some() {
        shared
            .update(&key, |m| {
                m.add_hold(hold);
                Ok(())
            })
            .await?;
        return Ok(());
    }
    let item = match item {
        Some(item) => item,
        None => session.item(id).await?.item,
    };
    let Item::Track(track) = &item else {
        return Err(SdkError::InvalidInput(format!("{id} is not a track")));
    };
    let album = parents.get(session, &track.album_id).await;
    let artist = parents.get(session, &track.artist_id).await;
    let mut m = Store::manifest(
        EntryKind::Track,
        key,
        server,
        user.user_id,
        user.username,
        item,
        MediaTracks::default(),
        source.clone(),
        DownloadTracks::All,
        LocalWatch::default(),
        now(),
    );
    m.holds = vec![hold.clone()];
    m.place = place;
    m.album = album;
    m.artist = artist;
    m.artwork = manager::fetch_artwork(session, store, &m, &mut parents.art).await;
    {
        let _writing = shared.manifests.lock().await;
        // Made meanwhile by another download: held by both.
        if let Some(mut there) = store.find_track(&m.server, &m.key) {
            there.add_hold(hold);
            store.write(&there).map_err(disk)?;
            m = there;
        } else {
            store.write(&m).map_err(disk)?;
        }
    }
    shared.emit(&m);
    Ok(())
}

/// The key of track `id` downloaded in `source`.
fn track_key(server: &str, id: &MediaId, source: &DownloadSource) -> String {
    key_of(server, EntryKind::Track, id, source, &DownloadTracks::All)
}

/// The key of a music download: one per server, user and target.
fn music_key(server: &str, user_id: &str, target: &MusicTarget) -> String {
    store::key_hash(&[
        "music:1",
        server,
        user_id,
        &serde_json::to_string(target).expect("a target serializes"),
    ])
}

/// The entries kept from listening alone: the cache.
fn cached(store: &Store) -> Vec<Manifest> {
    store
        .scan()
        .into_iter()
        .filter(Manifest::cached_only)
        .collect()
}

/// An entry's bytes: its file's once whole, else what has arrived.
fn entry_size(m: &Manifest) -> u64 {
    m.size.unwrap_or(m.offset)
}

/// Let the least recently played kept tracks go until the cache is under
/// its cap. `keeping` is the one just kept: it goes last, and only if it
/// alone is over the cap.
pub(super) async fn evict(shared: &Shared, keeping: Option<&str>) -> Result<(), SdkError> {
    let store = shared.store()?;
    let max = shared.settings().cache_max_bytes;
    let mut cache = cached(store);
    let mut used: u64 = cache.iter().map(entry_size).sum();
    // Oldest play first; the one just kept after every other.
    cache.sort_by_key(|m| {
        (
            Some(m.key.as_str()) == keeping,
            m.last_played_at.unwrap_or(m.created_at),
        )
    });
    for m in cache {
        if used <= max {
            break;
        }
        used -= entry_size(&m);
        let key = m.key.clone();
        let removed = {
            let _writing = shared.manifests.lock().await;
            let mut m = m;
            store.release(&mut m, &Hold::listening()).map_err(disk)?
        };
        if removed {
            tracing::debug!(key, "a kept track left the cache for room");
            let _ = shared.events.send(super::DownloadEvent::Removed { key });
        }
    }
    Ok(())
}

impl Shared {
    pub(super) fn settings(&self) -> DownloadSettings {
        let mut settings = self.settings.lock();
        if let Some(settings) = *settings {
            return settings;
        }
        let read = self
            .store
            .as_ref()
            .map(|store| read_json(&store.settings_path()))
            .unwrap_or_default();
        *settings = Some(read);
        read
    }

    /// Downloads wait now: on a mobile network, with it not allowed.
    pub(super) fn network_held(&self) -> bool {
        *self.network.lock() == NetworkKind::Cellular && !self.settings().allow_cellular
    }

    /// The network or what it allows changed: the manager lets go of a
    /// download it may not move, or picks one up again.
    fn network_changed(&self) {
        self.interrupt.send_modify(|n| *n += 1);
        self.wake.notify_one();
    }

    fn find_music(&self, store: &Store, key: &str) -> Option<(String, String, StoredMusic)> {
        for (server, user_id) in store.users_with(store::MUSIC_FILE) {
            let state: MusicState = read_json(&store.music_path(&server, &user_id));
            if let Some(stored) = state.downloads.into_iter().find(|d| d.key == key) {
                return Some((server, user_id, stored));
            }
        }
        None
    }

    fn music_view(
        &self,
        store: &Store,
        server: &str,
        user_id: &str,
        stored: &StoredMusic,
    ) -> MusicDownload {
        let source = stored.quality.source();
        let (mut done, mut received, mut size) = (0, 0, 0);
        for id in &stored.tracks {
            let key = track_key(server, id, &source);
            if let Some(m) = store.find_track(server, &key) {
                if m.phase == super::DownloadPhase::Done {
                    done += 1;
                }
                received += self.received_of(&m);
                size += m.size.unwrap_or(0);
            }
        }
        MusicDownload {
            key: stored.key.clone(),
            server: server.to_owned(),
            user_id: user_id.to_owned(),
            target: stored.target.clone(),
            name: stored.name.clone(),
            quality: stored.quality,
            tracks: stored.tracks.len() as u32,
            done,
            received,
            size,
            created_at: stored.created_at,
        }
    }

    /// [`Downloads::offline_music`].
    fn offline_music(&self, server: &str, user_id: &str) -> OfflineMusic {
        let Some(store) = &self.store else {
            return OfflineMusic::default();
        };
        if self.locks.is_locked(server, user_id) {
            return OfflineMusic::default();
        }
        let mut by_track: HashMap<MediaId, (Manifest, OfflineTrack)> = HashMap::new();
        // What holds each track, over all its entries (one per quality).
        let mut held_by: HashMap<MediaId, Vec<MusicTarget>> = HashMap::new();
        for m in store.scan() {
            if m.kind != EntryKind::Track || m.server != server || !m.is_for(user_id) {
                continue;
            }
            let Some(track) = self.offline_track(&m) else {
                continue;
            };
            let id = m.item.id().clone();
            let targets = held_by.entry(id.clone()).or_default();
            for hold in m
                .holds
                .iter()
                .filter(|h| h.user.as_deref() == Some(user_id))
            {
                if let Some(target) = MusicTarget::of_hold(hold, &id)
                    && !targets.contains(&target)
                {
                    targets.push(target);
                }
            }
            // One per track: a download before a kept copy.
            let better = by_track.get(&id).is_none_or(|(_, t)| {
                t.mark == OfflineMark::Kept && track.mark == OfflineMark::Download
            });
            if better {
                by_track.insert(id, (m, track));
            }
        }
        for (id, (_, track)) in &mut by_track {
            track.held_by = held_by.remove(id).unwrap_or_default();
        }
        let state: MusicState = read_json(&store.music_path(server, user_id));
        let mut artists: HashMap<MediaId, OfflineArtist> = HashMap::new();
        for (m, track) in by_track.values() {
            let Item::Track(t) = &m.item else {
                continue;
            };
            let artist = artists
                .entry(t.artist_id.clone())
                .or_insert_with(|| OfflineArtist {
                    id: t.artist_id.clone(),
                    artist: m.artist.clone(),
                    name: match &m.artist {
                        Some(Item::Artist(a)) => a.name.clone(),
                        _ => t.artist_name.clone().unwrap_or_default(),
                    },
                    albums: Vec::new(),
                });
            if artist.artist.is_none() {
                artist.artist = m.artist.clone();
            }
            let at = match artist.albums.iter().position(|a| a.id == t.album_id) {
                Some(at) => at,
                None => {
                    artist.albums.push(OfflineAlbum {
                        id: t.album_id.clone(),
                        album: m.album.clone(),
                        title: match &m.album {
                            Some(Item::Album(a)) => a.title.clone(),
                            _ => t.album_title.clone().unwrap_or_default(),
                        },
                        tracks: Vec::new(),
                        total_tracks: None,
                        tracklist: Vec::new(),
                    });
                    artist.albums.len() - 1
                }
            };
            let album = &mut artist.albums[at];
            if album.album.is_none() {
                album.album = m.album.clone();
            }
            // As the server counted them, should no tracklist be kept.
            album.total_tracks = album.total_tracks.or(t.album_track_count);
            album.tracks.push(track.clone());
        }
        let mut artists: Vec<OfflineArtist> = artists.into_values().collect();
        for artist in &mut artists {
            for album in &mut artist.albums {
                album.tracks.sort_by_key(|t| match &t.item {
                    Item::Track(t) => (t.disc.unwrap_or(0), t.track.unwrap_or(0), t.title.clone()),
                    _ => (0, 0, String::new()),
                });
                let kept = state.albums.iter().find(|a| a.id == album.id);
                album.fill_tracklist(kept.map(|a| a.tracks.as_slice()));
            }
            artist.albums.sort_by_key(|a| match &a.album {
                Some(Item::Album(album)) => (album.year.unwrap_or(0), a.title.clone()),
                _ => (0, a.title.clone()),
            });
        }
        artists.sort_by_key(|a| a.name.to_lowercase());
        let playlists = state
            .downloads
            .iter()
            .filter(|d| matches!(d.target, MusicTarget::Playlist { .. } | MusicTarget::Liked))
            .map(|d| OfflinePlaylist {
                target: d.target.clone(),
                name: d.name.clone(),
                tracks: d
                    .tracks
                    .iter()
                    .filter_map(|id| by_track.get(id).map(|(_, t)| t.clone()))
                    .collect(),
            })
            .collect();
        OfflineMusic { artists, playlists }
    }

    /// `m` as a track to play offline: `None` until its file is here.
    fn offline_track(&self, m: &Manifest) -> Option<OfflineTrack> {
        let view = self.view(m);
        let file_path = view.file_path.filter(|p| p.exists())?;
        Some(OfflineTrack {
            key: m.key.clone(),
            item: m.item.clone(),
            mark: if m.cached_only() {
                OfflineMark::Kept
            } else {
                OfflineMark::Download
            },
            file_path,
            file_url: view.file_url,
            artwork: view.artwork,
            size: m.size.unwrap_or(0),
            last_played_at: m.last_played_at,
            held_by: Vec::new(),
        })
    }

    /// Change `server`'s `user_id`'s plays and likes not sent yet.
    async fn pending(
        &self,
        store: &Store,
        server: &str,
        user_id: &str,
        change: impl FnOnce(&mut PendingPlays),
    ) -> Result<(), SdkError> {
        let _writing = self.manifests.lock().await;
        let path = store.offline_plays_path(server, user_id);
        let mut pending: PendingPlays = read_json(&path);
        change(&mut pending);
        write_json(&path, &pending).map_err(disk)
    }

    /// Track `id` of `server` was just played, streamed: its
    /// entries count it for the cache. Whether one is here whole,
    /// downloaded or kept already, so nothing more is to be kept of it.
    pub(super) async fn played(&self, server: &str, id: &MediaId) -> bool {
        let Some(store) = &self.store else {
            return false;
        };
        // Its entries: kept from listening, or downloaded in a quality.
        let mut keys = vec![kept_key(server, id)];
        keys.extend(MusicQuality::ALL.map(|q| track_key(server, id, &q.source())));
        let mut whole = false;
        for key in keys {
            let Some(m) = store.find_track(server, &key) else {
                continue;
            };
            whole |= m.phase == super::DownloadPhase::Done;
            let _ = self
                .update(&key, |m| {
                    m.last_played_at = Some(now());
                    Ok(())
                })
                .await;
        }
        whole
    }
}

/// The servers and users with music state or plays to send: synced at
/// the manager's start.
pub(super) fn users_to_sync(store: &Store) -> Vec<(String, String)> {
    let mut users = store.users_with(store::MUSIC_FILE);
    users.extend(store.users_with(store::OFFLINE_PLAYS_FILE));
    users.sort();
    users.dedup();
    users
}

/// Send `session`'s user's plays and likes, then bring their followed
/// downloads up to date.
pub(super) async fn sync(shared: &Shared, session: &Session) -> Result<MusicSynced, SdkError> {
    let synced = send_plays(shared, session).await?;
    refresh(shared, session, None).await?;
    Ok(synced)
}

async fn send_plays(shared: &Shared, session: &Session) -> Result<MusicSynced, SdkError> {
    let store = shared.store()?;
    let server = session.server();
    let user_id = session.user().user_id.clone();
    let path = store.offline_plays_path(&server, &user_id);
    let pending: PendingPlays = read_json(&path);
    // Likes wait for a server with Liked.
    let likes = if session.features.liked {
        pending.liked.clone()
    } else {
        Vec::new()
    };
    let mut synced = MusicSynced::default();
    let mut sent_plays: HashSet<(MediaId, i64)> = HashSet::new();
    let mut sent_likes: HashSet<(MediaId, i64)> = HashSet::new();
    let mut plays = pending.plays.as_slice();
    let mut liked = likes.as_slice();
    while !plays.is_empty() || !liked.is_empty() {
        let p = plays.len().min(MAX_ASKED);
        let l = liked.len().min(MAX_ASKED - p);
        let (these_plays, rest) = plays.split_at(p);
        plays = rest;
        let (these_likes, rest) = liked.split_at(l);
        liked = rest;
        let request = Request::SyncOfflinePlays {
            device_now: now(),
            plays: these_plays.to_vec(),
            liked: these_likes.to_vec(),
        };
        let (play_outcomes, like_outcomes) = match session.call(request).await? {
            Reply::OfflinePlaysSynced { plays, liked }
                if plays.len() == these_plays.len() && liked.len() == these_likes.len() =>
            {
                (plays, liked)
            }
            other => return Err(SdkError::Protocol(format!("unexpected reply {other:?}"))),
        };
        for (play, outcome) in these_plays.iter().zip(play_outcomes) {
            synced.plays.push(SyncedPlay {
                track: play.track.clone(),
                outcome,
            });
            // A play is recorded once whatever is said; one this build
            // does not understand is sent again.
            if outcome != OfflineWatchOutcome::Unknown {
                sent_plays.insert((play.track.clone(), play.at));
            }
        }
        for (like, outcome) in these_likes.iter().zip(like_outcomes) {
            synced.liked.push(SyncedPlay {
                track: like.track.clone(),
                outcome,
            });
            // Older: the server has a newer change, which stands.
            if outcome != OfflineWatchOutcome::Unknown {
                sent_likes.insert((like.track.clone(), like.at));
            }
        }
    }
    if !sent_plays.is_empty() || !sent_likes.is_empty() {
        shared
            .pending(store, &server, &user_id, |p| {
                p.plays
                    .retain(|play| !sent_plays.contains(&(play.track.clone(), play.at)));
                p.liked
                    .retain(|like| !sent_likes.contains(&(like.track.clone(), like.at)));
            })
            .await?;
    }
    Ok(synced)
}

/// Ask what `session`'s user's followed downloads hold now (those of
/// `only`, else all), and make what is kept match: tracks added are held
/// (and fetched), tracks removed let go of, a followed thing gone is
/// removed.
async fn refresh(
    shared: &Shared,
    session: &Session,
    only: Option<&[Followed]>,
) -> Result<(), SdkError> {
    let _following = shared.following.lock().await;
    let store = shared.store()?;
    let server = session.server();
    let user_id = session.user().user_id.clone();
    let path = store.music_path(&server, &user_id);
    let state: MusicState = read_json(&path);
    let followed: Vec<(Followed, StoredMusic)> = state
        .downloads
        .iter()
        .filter_map(|d| d.target.followed().map(|f| (f, d.clone())))
        .filter(|(f, _)| *f != Followed::Liked || session.features.liked)
        .filter(|(f, _)| only.is_none_or(|only| only.contains(f)))
        .collect();
    if followed.is_empty() {
        return Ok(());
    }
    let chunks: Vec<&[(Followed, StoredMusic)]> = followed.chunks(MAX_FOLLOWED).collect();
    // The server's cursor keeps every set's hash, those asked about now
    // put in their place: one ask's cursor is the next one's, whether it
    // asks about all, some (`only`), or the next chunk.
    let mut cursor = state.cursor.clone();
    let mut unknown = false;
    let mut parents = Parents::default();
    for chunk in &chunks {
        let asked: Vec<Followed> = chunk.iter().map(|(f, _)| f.clone()).collect();
        let (answers, next) = changes(session, &asked, cursor.clone()).await?;
        cursor = Some(next);
        for answer in answers {
            let Some((_, stored)) = chunk.iter().find(|(f, _)| *f == answer.followed) else {
                continue;
            };
            match answer.tracks {
                FollowedTracks::Unchanged => {}
                FollowedTracks::Tracks { tracks } => {
                    follow(shared, session, stored, tracks, &mut parents).await?;
                }
                FollowedTracks::Gone => {
                    tracing::info!(
                        key = stored.key,
                        "a followed music download is gone from the server"
                    );
                    let downloads = Downloads {
                        client: session.client.clone(),
                    };
                    downloads.remove_music(&stored.key).await?;
                }
                // Asked in full next time.
                FollowedTracks::Unknown => unknown = true,
            }
        }
    }
    let here = albums_here(store, &server, &user_id);
    let _writing = shared.manifests.lock().await;
    let mut state: MusicState = read_json(&path);
    state.cursor = if unknown { None } else { cursor };
    keep_albums(&mut state, parents.albums, &here);
    write_json(&path, &state).map_err(disk)
}

/// Keep the tracklists of the albums `read` in `server`'s `user_id`'s
/// `music.json`.
async fn record_albums(
    shared: &Shared,
    server: &str,
    user_id: &str,
    read: HashMap<MediaId, Vec<Item>>,
) -> Result<(), SdkError> {
    if read.is_empty() {
        return Ok(());
    }
    let store = shared.store()?;
    let here = albums_here(store, server, user_id);
    let _writing = shared.manifests.lock().await;
    let path = store.music_path(server, user_id);
    let mut state: MusicState = read_json(&path);
    keep_albums(&mut state, read, &here);
    write_json(&path, &state).map_err(disk)
}

/// The albums of the tracks `server`'s `user_id` has here.
fn albums_here(store: &Store, server: &str, user_id: &str) -> HashSet<MediaId> {
    store
        .scan()
        .into_iter()
        .filter(|m| m.kind == EntryKind::Track && m.server == server && m.is_for(user_id))
        .filter_map(|m| match m.item {
            Item::Track(t) => Some(t.album_id),
            _ => None,
        })
        .collect()
}

/// `state` keeps the tracklists `read`, the newest of each album, and
/// lets go of those of albums with no track `here` any more.
fn keep_albums(state: &mut MusicState, read: HashMap<MediaId, Vec<Item>>, here: &HashSet<MediaId>) {
    for (id, tracks) in read {
        match state.albums.iter_mut().find(|a| a.id == id) {
            Some(album) => album.tracks = tracks,
            None => state.albums.push(StoredAlbum { id, tracks }),
        }
    }
    state.albums.retain(|a| here.contains(&a.id));
}

/// A user on a server.
type ServerUser = (EndpointId, String);

/// Which followed downloads to ask about: `None`, every one.
type Asked = Option<Vec<Followed>>;

/// What the follower is to ask about, by server and user: when, and
/// which.
#[derive(Debug, Default)]
pub(super) struct FollowDue(HashMap<ServerUser, (Instant, Asked)>);

impl FollowDue {
    /// Ask about `followed` (`None`: every one) of `key`'s user at `at`, or
    /// with what is due already, if sooner: a burst is asked about once.
    fn mark(&mut self, key: ServerUser, followed: Option<Followed>, at: Instant) {
        let (when, asked) = self.0.entry(key).or_insert_with(|| (at, Some(Vec::new())));
        *when = (*when).min(at);
        match (asked.as_mut(), followed) {
            (Some(list), Some(f)) if !list.contains(&f) => list.push(f),
            (_, None) => *asked = None,
            _ => {}
        }
    }

    fn soonest(&self) -> Option<Instant> {
        self.0.values().map(|(at, _)| *at).min()
    }

    /// What is due by `now`, taken out.
    fn take(&mut self, now: Instant) -> Vec<(ServerUser, Asked)> {
        let due: Vec<ServerUser> = self
            .0
            .iter()
            .filter(|(_, (at, _))| *at <= now)
            .map(|(key, _)| key.clone())
            .collect();
        due.into_iter()
            .filter_map(|key| self.0.remove(&key).map(|(_, asked)| (key, asked)))
            .collect()
    }
}

impl Shared {
    /// Server `server` told that `followed` changed: each user with a
    /// session there who downloaded it has it asked about shortly.
    pub(crate) fn followed_changed(&self, server: EndpointId, followed: Followed) {
        let Some(store) = &self.store else {
            return;
        };
        let users: Vec<String> = self
            .sessions
            .lock()
            .keys()
            .filter(|(s, _)| *s == server)
            .map(|(_, user)| user.clone())
            .collect();
        let at = Instant::now() + FOLLOW_DEBOUNCE;
        for user in users {
            let state: MusicState = read_json(&store.music_path(&server.to_string(), &user));
            if state
                .downloads
                .iter()
                .any(|d| d.target.followed().as_ref() == Some(&followed))
            {
                self.follow_due
                    .lock()
                    .mark((server, user), Some(followed.clone()), at);
                self.follow_wake.notify_one();
            }
        }
    }
}

/// Until the client is closed or gone: ask about the followed downloads
/// edits were told of, and every one every [`FOLLOW_EVERY`], over the
/// connections that are up; it never dials. Holds the client only while
/// it asks.
pub(super) async fn follow_while_connected(client: Weak<Inner>, shared: Arc<Shared>) {
    let mut closed = shared.closed.subscribe();
    let mut every = tokio::time::interval_at(Instant::now() + FOLLOW_EVERY, FOLLOW_EVERY);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let soonest = shared.follow_due.lock().soonest();
        tokio::select! {
            () = shared.follow_wake.notified() => {}
            () = manager::sleep_until(soonest) => {}
            _ = every.tick() => {
                let now = Instant::now();
                let users: Vec<ServerUser> =
                    shared.sessions.lock().keys().cloned().collect();
                let mut due = shared.follow_due.lock();
                for key in users {
                    due.mark(key, None, now);
                }
            }
            _ = closed.wait_for(|closed| *closed) => return,
        }
        let ready = shared.follow_due.lock().take(Instant::now());
        for ((server, user), asked) in ready {
            if *shared.closed.borrow() {
                return;
            }
            let Some(inner) = client.upgrade() else {
                return;
            };
            let client = Client { inner };
            if client.link(server).open().is_none() {
                continue;
            }
            let parts = shared.sessions.lock().get(&(server, user.clone())).cloned();
            let Some(parts) = parts else {
                continue;
            };
            let session = parts.attach(client);
            if session.music_offered().is_err() {
                continue;
            }
            if let Err(e) = refresh(&shared, &session, asked.as_deref()).await {
                tracing::info!(%server, error = %e, "followed music not brought up to date; asked again later");
            }
        }
    }
}

/// `stored` holds `tracks` now: hold those it did not, let go of those it
/// no longer has.
async fn follow(
    shared: &Shared,
    session: &Session,
    stored: &StoredMusic,
    tracks: Vec<MediaId>,
    parents: &mut Parents,
) -> Result<(), SdkError> {
    let store = shared.store()?;
    let server = session.server();
    let user_id = session.user().user_id.clone();
    let before: HashSet<&MediaId> = stored.tracks.iter().collect();
    let now_held: HashSet<&MediaId> = tracks.iter().collect();
    let hold = stored.target.hold(&user_id);
    let source = stored.quality.source();
    for (place, id) in (0..)
        .zip(tracks.iter())
        .filter(|(_, id)| !before.contains(id))
    {
        if let Err(e) = hold_track(shared, session, id, None, &source, &hold, place, parents).await
        {
            tracing::info!(%id, error = %e, "a track a followed download gained is left out");
        }
    }
    let downloads = Downloads {
        client: session.client.clone(),
    };
    let gone: Vec<MediaId> = stored
        .tracks
        .iter()
        .filter(|id| !now_held.contains(id))
        .cloned()
        .collect();
    downloads
        .release_tracks(&server, &source, &gone, &hold)
        .await?;
    {
        let _writing = shared.manifests.lock().await;
        let path = store.music_path(&server, &user_id);
        let mut state: MusicState = read_json(&path);
        if let Some(d) = state.downloads.iter_mut().find(|d| d.key == stored.key) {
            d.tracks = tracks;
        }
        write_json(&path, &state).map_err(disk)?;
    }
    shared.wake.notify_one();
    Ok(())
}

/// Kept entry `m`, its file at `path`, is the file of SHA-256 `sha256`.
/// A file kept is hashed only here, once a download of its size comes.
pub(super) async fn kept_is(m: &Manifest, path: &std::path::Path, sha256: &str) -> bool {
    match &m.sha256 {
        Some(known) => known == sha256,
        None => store::sha256_file(path)
            .await
            .is_ok_and(|actual| actual == sha256),
    }
}

/// The entry of `id` kept from listening, if one is here whole.
pub(super) fn kept(store: &Store, server: &str, id: &MediaId) -> Option<Manifest> {
    store
        .find_track(server, &kept_key(server, id))
        .filter(|m| m.phase == super::DownloadPhase::Done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_types::{Namespace, PlaybackState, Track};
    use std::sync::Arc;

    fn track(key: &str) -> Item {
        Item::Track(Track {
            id: MediaId::new(Namespace::Plex, MediaKind::Track, key).unwrap(),
            album_id: MediaId::new(Namespace::Plex, MediaKind::Album, "31").unwrap(),
            artist_id: MediaId::new(Namespace::Plex, MediaKind::Artist, "30").unwrap(),
            artist_name: None,
            album_title: None,
            title: format!("Track {key}"),
            track: None,
            disc: None,
            album_track_count: None,
            codec: None,
            bit_depth: None,
            sample_rate: None,
            duration: None,
            artwork: Vec::new(),
            playback: PlaybackState::default(),
        })
    }

    /// A whole track entry of `size` bytes, held by `hold`, last played at
    /// `played`.
    fn entry(store: &Store, key: &str, hold: Hold, size: u64, played: i64) -> Manifest {
        let mut m = Store::manifest(
            EntryKind::Track,
            format!("{key:0>4}"),
            "server".into(),
            "user".into(),
            "alice".into(),
            track(key),
            MediaTracks::default(),
            DownloadSource::Original,
            DownloadTracks::All,
            LocalWatch::default(),
            1,
        );
        m.holds = vec![hold];
        m.size = Some(size);
        m.offset = size;
        m.phase = super::super::DownloadPhase::Done;
        m.last_played_at = Some(played);
        store.write(&m).unwrap();
        m
    }

    fn shared(test: &str, cap: u64) -> (Arc<Shared>, Store) {
        let dir = std::env::temp_dir().join(format!("sf-sdk-music-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let shared = Shared::new(Some(dir.clone()));
        *shared.settings.lock() = Some(DownloadSettings {
            cache_max_bytes: cap,
            ..DownloadSettings::default()
        });
        (shared, Store::new(dir))
    }

    fn keys(store: &Store) -> Vec<String> {
        store.scan().into_iter().map(|m| m.key).collect()
    }

    /// Over the cap, the least recently played kept track goes first; a
    /// download, however big, neither counts nor goes.
    #[tokio::test]
    async fn over_the_cap_the_least_recently_played_goes_and_never_a_download() {
        let (shared, store) = shared("evict", 150);
        entry(&store, "1", Hold::listening(), 100, 10);
        entry(&store, "2", Hold::listening(), 100, 20);
        entry(&store, "3", Hold::download("user"), 1000, 1);
        evict(&shared, None).await.unwrap();
        assert_eq!(keys(&store), ["0002", "0003"]);
    }

    /// The track just kept goes after every other, and only when it alone
    /// is over the cap.
    #[tokio::test]
    async fn the_track_just_kept_goes_last() {
        let (shared, store) = shared("keeping", 150);
        entry(&store, "1", Hold::listening(), 100, 30);
        let new = entry(&store, "2", Hold::listening(), 100, 5);
        evict(&shared, Some(&new.key)).await.unwrap();
        let kept = keys(&store);
        *shared.settings.lock() = Some(DownloadSettings {
            cache_max_bytes: 50,
            ..DownloadSettings::default()
        });
        evict(&shared, Some(&new.key)).await.unwrap();
        assert_eq!(
            (kept, keys(&store)),
            (vec!["0002".to_owned()], Vec::<String>::new())
        );
    }

    /// A track also in a download is not in the cache: kept from
    /// listening, and held by an album too.
    #[tokio::test]
    async fn a_kept_track_an_album_holds_too_is_not_in_the_cache() {
        let (shared, store) = shared("both", 10);
        let mut m = entry(&store, "1", Hold::listening(), 100, 1);
        m.holds.push(
            MusicTarget::Album {
                id: MediaId::new(Namespace::Plex, MediaKind::Album, "31").unwrap(),
            }
            .hold("user"),
        );
        store.write(&m).unwrap();
        evict(&shared, None).await.unwrap();
        assert_eq!(keys(&store), ["0001"]);
    }

    /// A track played counts for each of its entries, downloaded or kept,
    /// and is whole when one is.
    #[tokio::test]
    async fn a_track_played_marks_each_of_its_entries() {
        let (shared, store) = shared("played", 1);
        let id = MediaId::new(Namespace::Plex, MediaKind::Track, "7").unwrap();
        let put = |key: String, server: &str, done: bool| {
            let mut m = entry(&store, "7", Hold::download("user"), 10, 1);
            store.remove(&m).unwrap();
            (m.key, m.server) = (key, server.into());
            if !done {
                m.phase = super::super::DownloadPhase::Paused;
            }
            store.write(&m).unwrap();
            m.key
        };
        let aac = put(
            track_key("server", &id, &DownloadSource::Aac256),
            "server",
            false,
        );
        let kept = put(kept_key("server", &id), "server", true);
        let other = put(kept_key("other", &id), "other", true);
        let before = shared.played("server", &id).await;
        let played = |key: &str, server: &str| {
            store
                .find_track(server, key)
                .and_then(|m| m.last_played_at)
                .is_some_and(|at| at > 1)
        };
        assert_eq!(
            (
                before,
                played(&aac, "server"),
                played(&kept, "server"),
                played(&other, "other")
            ),
            (true, true, true, false)
        );
    }

    /// A kept file with no hash yet is hashed when asked; one hashed by an
    /// earlier build goes by its hash.
    #[tokio::test]
    async fn a_kept_file_is_hashed_only_when_asked() {
        let (_, store) = shared("kept-is", 1);
        let mut m = entry(&store, "1", Hold::listening(), 3, 1);
        let path = store.dir(&m).join("media.m4a");
        std::fs::write(&path, b"abc").unwrap();
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let lazy = (
            kept_is(&m, &path, abc).await,
            kept_is(&m, &path, &"0".repeat(64)).await,
        );
        m.sha256 = Some("0".repeat(64));
        assert_eq!(
            (lazy, kept_is(&m, &path, abc).await),
            ((true, false), false)
        );
    }

    #[test]
    fn on_cellular_downloads_wait_unless_allowed() {
        let (shared, _) = shared("cellular", 1);
        let unmetered = shared.network_held();
        *shared.network.lock() = NetworkKind::Cellular;
        let cellular = shared.network_held();
        *shared.settings.lock() = Some(DownloadSettings {
            allow_cellular: true,
            ..DownloadSettings::default()
        });
        assert_eq!(
            (unmetered, cellular, shared.network_held()),
            (false, true, false)
        );
    }

    /// Each download's hold names it back; the cache's names none.
    #[test]
    fn a_hold_names_the_download_that_holds() {
        let track = MediaId::new(Namespace::Plex, MediaKind::Track, "32").unwrap();
        let targets = [
            MusicTarget::Track { id: track.clone() },
            MusicTarget::Album {
                id: MediaId::new(Namespace::Plex, MediaKind::Album, "31").unwrap(),
            },
            MusicTarget::Artist {
                id: MediaId::new(Namespace::Plex, MediaKind::Artist, "30").unwrap(),
            },
            MusicTarget::Playlist { id: "p".into() },
            MusicTarget::Liked,
        ];
        let named: Vec<Option<MusicTarget>> = targets
            .iter()
            .map(|t| MusicTarget::of_hold(&t.hold("u"), &track))
            .collect();
        assert_eq!(
            (named, MusicTarget::of_hold(&Hold::listening(), &track)),
            (targets.map(Some).to_vec(), None)
        );
    }

    /// A whole track entry like [`entry`]'s, with its file on disk.
    fn playable(store: &Store, key: &str, holds: Vec<Hold>) -> Manifest {
        let mut m = entry(store, key, Hold::listening(), 3, 1);
        m.holds = holds;
        m.file = Some(spoolframe_protocol::downloads::DownloadFile {
            container: spoolframe_types::Container::Flac,
            tracks: Default::default(),
        });
        store.write(&m).unwrap();
        std::fs::write(store.media(&m).unwrap(), b"abc").unwrap();
        m
    }

    fn album_id() -> MediaId {
        MediaId::new(Namespace::Plex, MediaKind::Album, "31").unwrap()
    }

    /// Offline, an album has its whole tracklist, the songs not here
    /// marked so, and each track says which of the user's downloads
    /// hold it; a hold of another user's is not theirs to see.
    #[test]
    fn offline_an_album_has_its_tracklist_and_a_track_its_downloads() {
        let (shared, store) = shared("offline-album", 1 << 30);
        let album = MusicTarget::Album { id: album_id() };
        let playlist = MusicTarget::Playlist { id: "p".into() };
        playable(
            &store,
            "1",
            vec![
                album.hold("user"),
                playlist.hold("user"),
                Hold::download("bob"),
            ],
        );
        playable(&store, "3", vec![Hold::download("user"), Hold::listening()]);
        let state = MusicState {
            albums: vec![StoredAlbum {
                id: album_id(),
                tracks: vec![track("1"), track("2"), track("3")],
            }],
            ..MusicState::default()
        };
        write_json(&store.music_path("server", "user"), &state).unwrap();
        let music = shared.offline_music("server", "user");
        let album_here = &music.artists[0].albums[0];
        let three = MediaId::new(Namespace::Plex, MediaKind::Track, "3").unwrap();
        assert_eq!(
            (
                album_here.total_tracks,
                album_here
                    .tracklist
                    .iter()
                    .map(|t| (t.item.id().clone(), t.on_device))
                    .collect::<Vec<_>>(),
                album_here
                    .tracks
                    .iter()
                    .map(|t| t.held_by.clone())
                    .collect::<Vec<_>>(),
            ),
            (
                Some(3),
                vec![
                    (track("1").id().clone(), true),
                    (track("2").id().clone(), false),
                    (three.clone(), true),
                ],
                vec![
                    vec![album, playlist],
                    vec![MusicTarget::Track { id: three }]
                ],
            )
        );
    }

    /// With no tracklist kept (a track kept from listening), an album
    /// lists the tracks here, and counts as its tracks said.
    #[test]
    fn offline_an_album_with_no_tracklist_lists_what_is_here() {
        let (shared, store) = shared("offline-no-tracklist", 1 << 30);
        let mut m = playable(&store, "1", vec![Hold::listening()]);
        if let Item::Track(t) = &mut m.item {
            t.album_track_count = Some(9);
        }
        store.write(&m).unwrap();
        let music = shared.offline_music("server", "user");
        let album = &music.artists[0].albums[0];
        assert_eq!(
            (
                album.total_tracks,
                album
                    .tracklist
                    .iter()
                    .map(|t| t.on_device)
                    .collect::<Vec<_>>(),
                album.tracks[0].held_by.clone(),
            ),
            (Some(9), vec![true], Vec::new())
        );
    }

    /// A tracklist read again replaces the one kept, and an album with
    /// no track here any more is let go of.
    #[test]
    fn tracklists_are_kept_for_the_albums_here() {
        let other = MediaId::new(Namespace::Plex, MediaKind::Album, "40").unwrap();
        let mut state = MusicState {
            albums: vec![
                StoredAlbum {
                    id: album_id(),
                    tracks: vec![track("1")],
                },
                StoredAlbum {
                    id: other,
                    tracks: vec![track("9")],
                },
            ],
            ..MusicState::default()
        };
        keep_albums(
            &mut state,
            HashMap::from([(album_id(), vec![track("1"), track("2")])]),
            &HashSet::from([album_id()]),
        );
        assert_eq!(
            state
                .albums
                .iter()
                .map(|a| (a.id.clone(), a.tracks.len()))
                .collect::<Vec<_>>(),
            [(album_id(), 2)]
        );
    }

    /// A `music.json` written before tracklists were kept still reads,
    /// with none.
    #[test]
    fn music_state_from_before_tracklists_reads() {
        let state: MusicState =
            serde_json::from_str(r#"{"cursor": "c", "downloads": []}"#).unwrap();
        assert_eq!(
            (state.cursor.as_deref(), state.albums.len()),
            (Some("c"), 0)
        );
    }

    /// An album's id given as an artist is refused before the server is
    /// asked.
    #[test]
    fn a_target_s_id_must_be_of_its_kind() {
        let album = MediaId::new(Namespace::Plex, MediaKind::Album, "31").unwrap();
        let check = |target: MusicTarget| {
            target
                .media_kind()
                .map(|(id, kind)| expect_kind(id, kind).is_ok())
        };
        assert_eq!(
            [
                check(MusicTarget::Album { id: album.clone() }),
                check(MusicTarget::Artist { id: album }),
                check(MusicTarget::Liked),
            ],
            [Some(true), Some(false), None]
        );
    }

    #[test]
    fn each_target_holds_its_tracks_by_a_hold_of_its_own() {
        let album =
            MediaId::new(spoolframe_types::Namespace::Plex, MediaKind::Album, "31").unwrap();
        let holds = [
            MusicTarget::Album { id: album.clone() }.hold("u"),
            MusicTarget::Playlist { id: "p".into() }.hold("u"),
            MusicTarget::Liked.hold("u"),
            MusicTarget::Track {
                id: MediaId::new(spoolframe_types::Namespace::Plex, MediaKind::Track, "32")
                    .unwrap(),
            }
            .hold("u"),
        ];
        assert_eq!(
            holds.map(|h| (h.by, h.id)),
            [
                ("album".to_owned(), Some(album.to_string())),
                ("playlist".to_owned(), Some("p".to_owned())),
                ("liked".to_owned(), None),
                ("download".to_owned(), None),
            ]
        );
    }

    fn user() -> (EndpointId, String) {
        (
            iroh::SecretKey::from_bytes(&[7; 32]).public(),
            "user".into(),
        )
    }

    /// Edits told within the wait are asked about once, together, when the
    /// first one is due.
    #[tokio::test(start_paused = true)]
    async fn a_burst_of_edits_is_asked_about_once() {
        let mut due = FollowDue::default();
        let start = Instant::now();
        let playlist = Followed::Playlist { id: "p".into() };
        for (followed, after) in [
            (playlist.clone(), 0),
            (playlist.clone(), 500),
            (Followed::Liked, 1500),
        ] {
            let at = start + Duration::from_millis(after) + FOLLOW_DEBOUNCE;
            due.mark(user(), Some(followed), at);
        }
        let early = due.take(start + Duration::from_secs(1)).len();
        let asked = due.take(start + FOLLOW_DEBOUNCE);
        assert_eq!(
            (early, asked, due.soonest()),
            (
                0,
                vec![(user(), Some(vec![playlist, Followed::Liked]))],
                None
            )
        );
    }

    /// Every followed download asked about takes in one edit due with it.
    #[tokio::test(start_paused = true)]
    async fn asking_about_every_one_takes_in_an_edit_due() {
        let mut due = FollowDue::default();
        let start = Instant::now();
        due.mark(user(), Some(Followed::Liked), start + FOLLOW_DEBOUNCE);
        due.mark(user(), None, start);
        assert_eq!(due.take(start), vec![(user(), None)]);
    }

    /// A music download's key is kept in `music.json`: the same target
    /// gives the same bytes, build after build.
    #[test]
    fn a_music_download_s_key_does_not_change() {
        let album = MediaId::new(Namespace::Plex, MediaKind::Album, "31").unwrap();
        assert_eq!(
            music_key("s", "u", &MusicTarget::Album { id: album }),
            "9d6c9945d20fad11fb2489e6ba10833d"
        );
    }

    #[test]
    fn settings_default_to_original_no_cellular_and_a_2_gib_cache() {
        let settings: DownloadSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(
            settings,
            DownloadSettings {
                music_quality: MusicQuality::Original,
                allow_cellular: false,
                cache_max_bytes: 2 * 1024 * 1024 * 1024,
            }
        );
    }
}
