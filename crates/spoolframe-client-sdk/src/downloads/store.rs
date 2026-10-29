//! Downloads on disk: one directory per stored file,
//! `<dir>/<server>/%files/<kind>/<key>/`, holding `manifest.json`, the
//! media (`media.part` while it arrives, then `media.mp4` or `media.mkv`)
//! and the artwork (`art/*.jpg`). Everything about an entry is in its
//! manifest, so the list needs no server.
//!
//! A file is the device's, not a user's: two profiles that download the
//! same item share one file, each holding it ([`Hold::user`]), each with
//! its own watch state. What is per user (offline watch of deleted
//! downloads, music followed, plays not sent) is under
//! `<dir>/<server>/<user>/`. Files found in the older per-user layout,
//! `<dir>/<server>/<user>/<kind>/<key>/`, are moved by [`Store::open`].
//!
//! Entries are keyed by their item's kind ([`EntryKind`]): films,
//! episodes and tracks. A stored file is
//! kept while anything holds it ([`Hold`]): a film's download is its one
//! hold, while a track may be held by an album, a playlist and the cache
//! of what was kept from listening, and goes only when none is left.
//!
//! A manifest is written whole to a file beside it and renamed over it:
//! a crash leaves the old one or the new one, never half of either. A
//! directory without a manifest is a download that never started, and
//! is not listed; neither is one of a newer version or a kind this build
//! does not know, which is left as it is.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use serde::{Deserialize, Serialize};
use spoolframe_protocol::downloads::{DownloadFile, DownloadSource, DownloadTracks, OfflineWatch};
use spoolframe_types::{ArtworkKind, Container, Item, Marker, MediaId, MediaTracks};

use super::{DownloadPhase, LocalWatch};

/// This layout's version, in each manifest: 2 since files are shared by
/// the device's users.
const VERSION: u32 = 2;
/// The directory of a server's files, beside its users': `dir_name`
/// never makes a name with a `%` not followed by two hex digits.
const FILES: &str = "%files";
const MANIFEST: &str = "manifest.json";
const PART: &str = "media.part";
const ART: &str = "art";
/// Offline watch entries of downloads deleted before they were sent,
/// per server and user: sent with the rest on the next sync.
const CARRIED: &str = "watch.json";
/// What a user's music downloads follow on a server.
const MUSIC: &str = "music.json";
/// Plays and likes made with no server, per server and user, not sent
/// yet.
const OFFLINE_PLAYS: &str = "plays.json";
/// This device's download settings, at the top.
const SETTINGS: &str = "settings.json";
/// Tracks being kept as they play, at the top: none is whole, so all are
/// removed at start.
const LISTENING_PARTS: &str = "listening";

/// What kind of item an entry stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EntryKind {
    Film,
    Episode,
    /// A track: downloaded, or kept from listening.
    Track,
    /// A kind a newer build stores: never listed, moved or removed here.
    #[serde(other)]
    Unknown,
}

impl EntryKind {
    /// The kind of `id`'s entry; `None` for one nothing here stores.
    pub fn of(id: &MediaId) -> Option<Self> {
        match id.kind() {
            spoolframe_types::MediaKind::Movie => Some(Self::Film),
            spoolframe_types::MediaKind::Episode => Some(Self::Episode),
            spoolframe_types::MediaKind::Track => Some(Self::Track),
            _ => None,
        }
    }

    /// Its directory under a user's.
    fn dir(self) -> &'static str {
        match self {
            Self::Film => "film",
            Self::Episode => "episode",
            Self::Track => "track",
            Self::Unknown => "unknown",
        }
    }
}

/// What keeps a stored file: `by` what (`download`, the user's download
/// of the item, or `album`, `playlist`, `listening`), and which
/// one when there can be several (an album's id). Plain text, so a build
/// that does not know a newer hold still counts it, and writes it back
/// as it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Hold {
    pub by: String,
    pub id: Option<String>,
    /// Whose: the user whose download, album or playlist it is. `None`
    /// for the device's own, the cache of what was kept from listening,
    /// which every profile may play.
    #[serde(default)]
    pub user: Option<String>,
}

impl Hold {
    /// `user_id`'s download of the item itself.
    pub fn download(user_id: &str) -> Self {
        Self {
            by: "download".into(),
            id: None,
            user: Some(user_id.to_owned()),
        }
    }

    /// The cache of tracks kept from listening: the device's.
    pub fn listening() -> Self {
        Self {
            by: LISTENING.into(),
            id: None,
            user: None,
        }
    }

    /// Held for `user_id`, or for the device (every profile).
    pub fn is_for(&self, user_id: &str) -> bool {
        self.user.as_deref().is_none_or(|u| u == user_id)
    }

    pub fn is_listening(&self) -> bool {
        self.by == LISTENING
    }
}

const LISTENING: &str = "listening";

/// What every manifest starts with: read first, so one this build cannot
/// read is left alone rather than logged as broken.
#[derive(Deserialize)]
struct Header {
    version: u32,
    kind: EntryKind,
}

/// One stored file, as kept on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub version: u32,
    pub kind: EntryKind,
    pub key: String,
    /// What keeps it; it goes when none is left.
    pub holds: Vec<Hold>,
    /// The server's key.
    pub server: String,
    pub user_id: String,
    pub username: String,
    pub item: Item,
    /// An episode's show and season, as the server had them.
    pub show: Option<Item>,
    pub season: Option<Item>,
    pub markers: Vec<Marker>,
    pub artwork: Vec<StoredArtwork>,
    /// The item's tracks, as `Tracks` gave them: what the file's tracks
    /// are labelled with offline.
    pub tracks: MediaTracks,
    pub source: DownloadSource,
    pub chosen: DownloadTracks,
    /// The server's id for the download; `None` until prepared, and once
    /// the server no longer knows it.
    pub id: Option<String>,
    pub file: Option<DownloadFile>,
    pub size: Option<u64>,
    /// About how large the file will be, as the server said when it was
    /// prepared.
    #[serde(default)]
    pub expected_size: Option<u64>,
    pub sha256: Option<String>,
    /// How much of `media.part` is known to be on disk (synced).
    pub offset: u64,
    pub phase: DownloadPhase,
    /// Each user's watch state of it, by user id.
    #[serde(default)]
    pub watches: BTreeMap<String, LocalWatch>,
    /// The one watch state of the older per-user layout, of `user_id`:
    /// read into `watches`.
    #[serde(default, skip_serializing)]
    watch: Option<LocalWatch>,
    /// Why it is not moving, while it waits to try again.
    pub note: Option<String>,
    /// Unix seconds, by this device's clock.
    pub created_at: i64,
    /// Its place among what was started with it, in the same second: an
    /// album's tracks in the album's order. They move in that order.
    #[serde(default)]
    pub place: u32,
    /// A track's album and artist, as the server had them.
    #[serde(default)]
    pub album: Option<Item>,
    #[serde(default)]
    pub artist: Option<Item>,
    /// When a track was last played here, Unix seconds by this device's
    /// clock: the cache lets the least recently played go first.
    #[serde(default)]
    pub last_played_at: Option<i64>,
}

impl Manifest {
    /// Held by `hold` too, once.
    pub fn add_hold(&mut self, hold: &Hold) {
        if !self.holds.contains(hold) {
            self.holds.push(hold.clone());
        }
    }

    /// Kept from listening only: in the cache, under its cap.
    pub fn cached_only(&self) -> bool {
        !self.holds.is_empty() && self.holds.iter().all(Hold::is_listening)
    }

    /// Held by `user_id`, or for every profile (kept from listening).
    pub fn is_for(&self, user_id: &str) -> bool {
        self.holds.iter().any(|h| h.is_for(user_id))
    }

    /// Held by `user_id` themselves: a download of theirs.
    pub fn held_by(&self, user_id: &str) -> bool {
        self.holds
            .iter()
            .any(|h| h.user.as_deref() == Some(user_id))
    }

    /// `user_id`'s watch state of it: none yet for a user who has not
    /// watched it here, or downloaded it (`item` is as another user saw
    /// it).
    pub fn watch_of(&self, user_id: &str) -> LocalWatch {
        self.watches.get(user_id).cloned().unwrap_or_default()
    }

    /// `user_id`'s watch state, to change.
    pub fn watch_mut(&mut self, user_id: &str) -> &mut LocalWatch {
        self.watches.entry(user_id.to_owned()).or_default()
    }

    /// A manifest of the older per-user layout: its one watch state is its
    /// user's.
    fn upgrade(&mut self) {
        if let Some(watch) = self.watch.take() {
            self.watches.entry(self.user_id.clone()).or_insert(watch);
        }
        for hold in &mut self.holds {
            if hold.user.is_none() && !hold.is_listening() {
                hold.user = Some(self.user_id.clone());
            }
        }
        self.version = VERSION;
    }
}

/// A picture kept with a download: `file` in its `art` directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredArtwork {
    /// The item it is of: the download's, or its show's or season's.
    pub item: MediaId,
    pub kind: ArtworkKind,
    pub file: String,
}

/// The key of a download: the same for the same server, kind, item,
/// source and tracks, whoever asks, so starting one twice, or as another
/// profile, finds the first. Order and repeats of chosen tracks do not
/// count, as on the server.
pub(crate) fn key_of(
    server: &str,
    kind: EntryKind,
    id: &MediaId,
    source: &DownloadSource,
    tracks: &DownloadTracks,
) -> String {
    let tracks = tracks.normalized();
    key_hash(&[
        "download:2",
        server,
        kind.dir(),
        &id.to_string(),
        &serde_json::to_string(source).expect("a source serializes"),
        &serde_json::to_string(&tracks).expect("tracks serialize"),
    ])
}

/// A key made of `parts`: their BLAKE3, each part ended by a 0 byte, as
/// 32 hex digits.
pub(crate) fn key_hash(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex()[..32].to_owned()
}

/// The key of the file of track `id` kept from listening: one per track
/// and device, whatever was streamed and whoever played it.
pub(crate) fn kept_key(server: &str, id: &MediaId) -> String {
    key_hash(&["listening:2", server, &id.to_string()])
}

/// A user id as a directory name: every byte but ASCII letters, digits,
/// `.`, `_` and `-` as `%XX`, so `local:<uuid>` is one name on every
/// file system and no id is another's.
fn dir_name(id: &str) -> String {
    let mut name = String::with_capacity(id.len());
    for byte in id.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') {
            name.push(byte as char);
        } else {
            name.push_str(&format!("%{byte:02X}"));
        }
    }
    // `.` and `..` are not names of their own.
    if name.chars().all(|c| c == '.') {
        name = name.replace('.', "%2E");
    }
    name
}

/// The extension of a finished file of `kind` in `container`.
pub(crate) fn extension(kind: EntryKind, container: Container) -> &'static str {
    match (kind, container) {
        (EntryKind::Track, Container::Mp4) => "m4a",
        (EntryKind::Track, Container::Flac) => "flac",
        (EntryKind::Track, Container::Mp3) => "mp3",
        (EntryKind::Track, Container::Ogg) => "ogg",
        (EntryKind::Track, _) => "mka",
        (_, Container::Mp4) => "mp4",
        _ => "mkv",
    }
}

/// The downloads directory.
#[derive(Debug, Clone)]
pub(crate) struct Store {
    root: PathBuf,
    /// The downloads under way, as `(created_at, place, key)`: read from the
    /// disk at first use, then kept by each write and removal here, so
    /// the manager need not read every manifest to find the next.
    active: Arc<Mutex<Option<Active>>>,
    /// Where each item's downloads are, by the item's id and, for an
    /// episode, its show's: read from the disk at first use, then kept
    /// by each write and removal here, so finding one item's downloads
    /// reads only theirs. Read again when another process adds or removes
    /// a download (its [`Index::stamp`] moved).
    items: Arc<Mutex<Option<Index>>>,
}

/// [`Items`], with what tells that the disk moved under it.
#[derive(Debug)]
struct Index {
    items: Items,
    /// The listing directories' modification times when it was read:
    /// the root, each server's files directory and each kind's under it.
    /// Adding or removing a download (here or in another process: an iOS
    /// background session, a share extension, Android's WorkManager)
    /// moves its kind directory's, so one that differs is read again.
    stamp: Stamp,
    /// Download directories that had no manifest to read then: one being
    /// made, whose manifest comes after its directory. One that has it
    /// now is read again.
    unread: Vec<PathBuf>,
}

/// Each listing directory and its modification time, in path order.
type Stamp = Vec<(PathBuf, Option<std::time::SystemTime>)>;

/// Downloads by `(created_at, place, key)`: oldest first.
type Active = BTreeSet<(i64, u32, String)>;

/// The directories of the downloads of each item, and of each show's
/// episodes. Tracks are not listed.
type Items = HashMap<MediaId, BTreeSet<PathBuf>>;

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            active: Arc::default(),
            items: Arc::default(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Move the files of the older per-user layout, under
    /// `<server>/<user>/<kind>/<key>/`, to where they are now, under the
    /// key they have now: a file two users both had becomes one, held by
    /// both, with each one's watch state. Run before anything else reads
    /// the directory; one that cannot be moved is left where it was, and
    /// logged.
    pub fn migrate(&self) {
        self.rescan();
        for server in read_dir(&self.root) {
            for user in user_dirs(&server) {
                for kind in read_dir(&user) {
                    if !kind
                        .file_name()
                        .is_some_and(|n| ["film", "episode", "track"].iter().any(|k| n == *k))
                    {
                        continue;
                    }
                    for old in read_dir(&kind) {
                        if let Err(e) = self.migrate_one(&old) {
                            tracing::warn!(path = %old.display(), error = %e, "a 0.14.0 download cannot be moved; left where it was");
                        }
                    }
                    let _ = std::fs::remove_dir(&kind);
                }
            }
        }
    }

    /// Forget what was read of the directory: the next read reads it
    /// again, whole. For a download another process wrote, which the
    /// index finds by itself too ([`Index::stamp`]), and the downloads
    /// under way, which it does not.
    pub fn rescan(&self) {
        *self.items.lock() = None;
        *self.active.lock() = None;
    }

    /// The listing directories as they are now ([`Index::stamp`]).
    fn stamp(&self) -> Stamp {
        let mut dirs = vec![self.root.clone()];
        for server in read_dir(&self.root) {
            let files = server.join(FILES);
            dirs.extend(read_dir(&files));
            dirs.push(files);
        }
        dirs.sort();
        dirs.into_iter()
            .map(|dir| {
                let modified = std::fs::metadata(&dir).and_then(|m| m.modified()).ok();
                (dir, modified)
            })
            .collect()
    }

    /// Every download's directory, with its manifest when it can be read.
    fn downloads(&self) -> Vec<(PathBuf, Option<Manifest>)> {
        let mut found = Vec::new();
        for server in read_dir(&self.root) {
            for kind in read_dir(&server.join(FILES)) {
                for key in read_dir(&kind) {
                    let path = key.join(MANIFEST);
                    let m = match read_manifest(&path) {
                        Ok(m) => m,
                        Err(e) => {
                            tracing::warn!(path = %path.display(), error = %e, "a download's manifest cannot be read; left out");
                            None
                        }
                    };
                    found.push((key, m));
                }
            }
        }
        found
    }

    /// Move one file of the older per-user layout from `old`.
    fn migrate_one(&self, old: &Path) -> std::io::Result<()> {
        let Some(mut m) = read_manifest(&old.join(MANIFEST))? else {
            return Ok(());
        };
        let id = m.item.id().clone();
        // 0.14.0's key of a track kept from listening.
        let kept = m.key == key_hash(&["listening:1", &m.server, &m.user_id, &id.to_string()]);
        m.key = if kept {
            kept_key(&m.server, &id)
        } else {
            key_of(&m.server, m.kind, &id, &m.source, &m.chosen)
        };
        let to = self.dir(&m);
        match read_manifest(&to.join(MANIFEST))? {
            None => {
                std::fs::create_dir_all(to.parent().expect("a file's directory has a parent"))?;
                std::fs::rename(old, &to)?;
                self.write(&m)
            }
            Some(mut there) => {
                // The same file twice: the one further along stays, held
                // by both, and the other goes.
                if further(&m, &there) {
                    std::mem::swap(&mut m, &mut there);
                    std::fs::remove_dir_all(&to)?;
                    std::fs::rename(old, &to)?;
                } else {
                    std::fs::remove_dir_all(old)?;
                }
                for hold in &m.holds {
                    there.add_hold(hold);
                }
                for (user_id, watch) in m.watches {
                    there.watches.entry(user_id).or_insert(watch);
                }
                there.last_played_at = there.last_played_at.max(m.last_played_at);
                self.write(&there)
            }
        }
    }

    /// Where `server`'s downloads for `user_id` are.
    pub fn user_dir(&self, server: &str, user_id: &str) -> PathBuf {
        self.root.join(dir_name(server)).join(dir_name(user_id))
    }

    /// Where `server`'s files are.
    fn files_dir(&self, server: &str) -> PathBuf {
        self.root.join(dir_name(server)).join(FILES)
    }

    /// Where download `m` is.
    pub fn dir(&self, m: &Manifest) -> PathBuf {
        self.files_dir(&m.server).join(m.kind.dir()).join(&m.key)
    }

    pub fn part(&self, m: &Manifest) -> PathBuf {
        self.dir(m).join(PART)
    }

    /// The finished file, once the container is known.
    pub fn media(&self, m: &Manifest) -> Option<PathBuf> {
        let container = m.file.as_ref()?.container;
        Some(
            self.dir(m)
                .join(format!("media.{}", extension(m.kind, container))),
        )
    }

    pub fn art_dir(&self, m: &Manifest) -> PathBuf {
        self.dir(m).join(ART)
    }

    /// Write `m`, whole or not at all.
    pub fn write(&self, m: &Manifest) -> std::io::Result<()> {
        write_json(&self.dir(m).join(MANIFEST), m)?;
        self.note(m, m.phase.is_active());
        self.list(m, true);
        Ok(())
    }

    /// The keys of the downloads under way, oldest first.
    pub fn active(&self) -> Vec<String> {
        let mut active = self.active.lock();
        // Read while the lock is held: a write meanwhile notes itself
        // after this, not before.
        let active = active.get_or_insert_with(|| {
            self.scan()
                .into_iter()
                .filter(|m| m.phase.is_active())
                .map(|m| (m.created_at, m.place, m.key))
                .collect()
        });
        active.iter().map(|(_, _, key)| key.clone()).collect()
    }

    /// Download `key` was found not under way: changed or removed by
    /// another process using this directory, it is let go of unless the
    /// disk says it is under way.
    pub fn recheck(&self, key: &str) {
        let mut active = self.active.lock();
        // Read while the lock is held: a write meanwhile notes itself
        // after this.
        if let Some(active) = active.as_mut()
            && !self.find(key).is_some_and(|m| m.phase.is_active())
        {
            active.retain(|(_, _, k)| k != key);
        }
    }

    /// The ids `m` is listed under in [`Items`].
    fn listed_as(m: &Manifest) -> Vec<MediaId> {
        if m.kind == EntryKind::Track {
            return Vec::new();
        }
        let mut ids = vec![m.item.id().clone()];
        if let Item::Episode(e) = &m.item {
            ids.push(e.show_id.clone());
        }
        ids
    }

    /// The downloads of item `id` and, for a show, of its episodes;
    /// tracks are not among them.
    pub fn of_item(&self, id: &MediaId) -> Vec<Manifest> {
        let dirs: Vec<PathBuf> = {
            let mut index = self.items.lock();
            // Taken before the read: a change during it moves the stamp
            // after this, and the next call reads again.
            let stamp = self.stamp();
            let current = index.as_ref().is_some_and(|i| {
                i.stamp == stamp && !i.unread.iter().any(|d| d.join(MANIFEST).exists())
            });
            if !current {
                let mut items = Items::new();
                let mut unread = Vec::new();
                for (dir, m) in self.downloads() {
                    match m {
                        Some(m) => {
                            for id in Self::listed_as(&m) {
                                items.entry(id).or_default().insert(dir.clone());
                            }
                        }
                        // Not one being made: a newer build's, or one
                        // that can't be read, which reading again won't
                        // change.
                        None if dir.join(MANIFEST).exists() => {}
                        None => unread.push(dir),
                    }
                }
                *index = Some(Index {
                    items,
                    stamp,
                    unread,
                });
            }
            let items = &index.as_ref().expect("read above").items;
            items.get(id).into_iter().flatten().cloned().collect()
        };
        dirs.iter()
            .filter_map(|dir| read_manifest(&dir.join(MANIFEST)).ok().flatten())
            .collect()
    }

    /// `m` is on the disk, or no longer: [`Items`] kept in step.
    fn list(&self, m: &Manifest, there: bool) {
        if let Some(index) = self.items.lock().as_mut() {
            for id in Self::listed_as(m) {
                let dirs = index.items.entry(id).or_default();
                if there {
                    dirs.insert(self.dir(m));
                } else {
                    dirs.remove(&self.dir(m));
                }
            }
        }
    }

    /// `m` is under way, or no longer.
    fn note(&self, m: &Manifest, under_way: bool) {
        if let Some(active) = self.active.lock().as_mut() {
            let entry = (m.created_at, m.place, m.key.clone());
            if under_way {
                active.insert(entry);
            } else {
                active.remove(&entry);
            }
        }
    }

    /// A new manifest for these.
    #[allow(clippy::too_many_arguments)]
    pub fn manifest(
        kind: EntryKind,
        key: String,
        server: String,
        user_id: String,
        username: String,
        item: Item,
        tracks: MediaTracks,
        source: DownloadSource,
        chosen: DownloadTracks,
        watch: LocalWatch,
        created_at: i64,
    ) -> Manifest {
        Manifest {
            version: VERSION,
            kind,
            key,
            holds: vec![Hold::download(&user_id)],
            watches: BTreeMap::from([(user_id.clone(), watch)]),
            watch: None,
            server,
            user_id,
            username,
            item,
            show: None,
            season: None,
            markers: Vec::new(),
            artwork: Vec::new(),
            tracks,
            source,
            chosen,
            id: None,
            file: None,
            size: None,
            expected_size: None,
            sha256: None,
            offset: 0,
            phase: DownloadPhase::Preparing {
                server: spoolframe_protocol::downloads::DownloadState::Waiting,
            },
            note: None,
            created_at,
            place: 0,
            album: None,
            artist: None,
            last_played_at: None,
        }
    }

    /// Every download, oldest first. One whose manifest cannot be read is
    /// left out, and said so in the log.
    pub fn scan(&self) -> Vec<Manifest> {
        let mut found: Vec<Manifest> = self
            .downloads()
            .into_iter()
            .filter_map(|(_, m)| m)
            .collect();
        found.sort_by(|a, b| (a.created_at, a.place, &a.key).cmp(&(b.created_at, b.place, &b.key)));
        found
    }

    /// Download `key`, if there is one.
    pub fn find(&self, key: &str) -> Option<Manifest> {
        // A key is a hash: never a path of its own.
        if !key.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        for server in read_dir(&self.root) {
            for kind in read_dir(&server.join(FILES)) {
                if let Ok(Some(m)) = read_manifest(&kind.join(key).join(MANIFEST)) {
                    return Some(m);
                }
            }
        }
        None
    }

    /// `server`'s track `key`, if there is one: read from where it would
    /// be, with no other directory looked in.
    pub fn find_track(&self, server: &str, key: &str) -> Option<Manifest> {
        if !key.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let path = self
            .files_dir(server)
            .join(EntryKind::Track.dir())
            .join(key)
            .join(MANIFEST);
        read_manifest(&path).ok().flatten()
    }

    /// Let go of `hold` on `m`: with nothing left holding it, it is taken
    /// off the disk ([`Store::remove`]) and this is true; else the
    /// manifest is written with the holds left.
    pub fn release(&self, m: &mut Manifest, hold: &Hold) -> std::io::Result<bool> {
        m.holds.retain(|h| h != hold);
        // The user whose session fetches it let go, and others hold it
        // still: one of them fetches what is left, from the start of
        // their own preparation (the server's download is per user).
        if !m.holds.is_empty()
            && !m.held_by(&m.user_id)
            && !matches!(m.phase, DownloadPhase::Done)
            && let Some(next) = m.holds.iter().find_map(|h| h.user.clone())
        {
            m.user_id = next;
            m.id = None;
            m.phase = DownloadPhase::Preparing {
                server: spoolframe_protocol::downloads::DownloadState::Waiting,
            };
        }
        if m.holds.is_empty() {
            self.remove(m)?;
            Ok(true)
        } else {
            self.write(m)?;
            Ok(false)
        }
    }

    /// Take download `m` off the disk. Its offline watch entry, if not
    /// sent yet, is kept for the next sync.
    pub fn remove(&self, m: &Manifest) -> std::io::Result<()> {
        for (user_id, watch) in &m.watches {
            if watch.pending
                && let Some(entry) = watch.entry(&m.item)
            {
                let mut carried = self.carried(&m.server, user_id);
                carried.retain(|c| c.id != entry.id);
                carried.push(entry);
                self.set_carried(&m.server, user_id, &carried)?;
            }
        }
        match std::fs::remove_dir_all(self.dir(m)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        self.note(m, false);
        self.list(m, false);
        Ok(())
    }

    /// The offline watch entries of deleted downloads not sent yet.
    pub fn carried(&self, server: &str, user_id: &str) -> Vec<OfflineWatch> {
        let path = self.user_dir(server, user_id).join(CARRIED);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "offline watch entries cannot be read; dropped");
                Vec::new()
            }),
            Err(_) => Vec::new(),
        }
    }

    pub fn set_carried(
        &self,
        server: &str,
        user_id: &str,
        entries: &[OfflineWatch],
    ) -> std::io::Result<()> {
        let path = self.user_dir(server, user_id).join(CARRIED);
        if entries.is_empty() {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        write_json_compact(&path, &entries)
    }

    /// The servers and users with offline watch entries of deleted
    /// downloads.
    pub fn carried_users(&self) -> Vec<(String, String)> {
        let mut users = Vec::new();
        for m in self.scan() {
            for (user_id, watch) in &m.watches {
                if watch.pending {
                    users.push((m.server.clone(), user_id.clone()));
                }
            }
        }
        for server in read_dir(&self.root) {
            for user in user_dirs(&server) {
                let Ok(bytes) = std::fs::read(user.join(CARRIED)) else {
                    continue;
                };
                // The names are encoded: the ids are in the entries'
                // download dirs, or the carried file's own siblings.
                if let Ok(entries) = serde_json::from_slice::<Vec<OfflineWatch>>(&bytes)
                    && !entries.is_empty()
                    && let (Some(s), Some(u)) = (
                        server.file_name().and_then(|n| n.to_str()),
                        user.file_name().and_then(|n| n.to_str()),
                    )
                    && let (Some(s), Some(u)) = (undo_dir_name(s), undo_dir_name(u))
                {
                    users.push((s, u));
                }
            }
        }
        users.sort();
        users.dedup();
        users
    }

    /// What `server`'s `user_id`'s music downloads follow.
    pub fn music_path(&self, server: &str, user_id: &str) -> PathBuf {
        self.user_dir(server, user_id).join(MUSIC)
    }

    /// `server`'s `user_id`'s plays and likes not sent yet.
    pub fn offline_plays_path(&self, server: &str, user_id: &str) -> PathBuf {
        self.user_dir(server, user_id).join(OFFLINE_PLAYS)
    }

    pub fn settings_path(&self) -> PathBuf {
        self.root.join(SETTINGS)
    }

    /// Where tracks being kept as they play are written.
    pub fn listening_parts(&self) -> PathBuf {
        self.root.join(LISTENING_PARTS)
    }

    /// The servers and users with a file `name` (music, plays) of their
    /// own.
    pub fn users_with(&self, name: &str) -> Vec<(String, String)> {
        let mut users = Vec::new();
        for server in read_dir(&self.root) {
            for user in user_dirs(&server) {
                if user.join(name).exists()
                    && let (Some(s), Some(u)) = (
                        server.file_name().and_then(|n| n.to_str()),
                        user.file_name().and_then(|n| n.to_str()),
                    )
                    && let (Some(s), Some(u)) = (undo_dir_name(s), undo_dir_name(u))
                {
                    users.push((s, u));
                }
            }
        }
        users
    }

    /// The bytes every download takes on disk.
    pub fn used(&self) -> u64 {
        // Every entry, files too: `read_dir` gives directories alone.
        fn walk(dir: &Path) -> u64 {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return 0;
            };
            entries
                .filter_map(Result::ok)
                .map(|e| match e.metadata() {
                    Ok(meta) if meta.is_dir() => walk(&e.path()),
                    Ok(meta) => meta.len(),
                    Err(_) => 0,
                })
                .sum()
        }
        walk(&self.root)
    }
}

pub(crate) const MUSIC_FILE: &str = MUSIC;
pub(crate) const OFFLINE_PLAYS_FILE: &str = OFFLINE_PLAYS;

/// The JSON at `path`; the default if there is none, or it cannot be
/// read (said so in the log).
pub(crate) fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            tracing::warn!(path = %path.display(), error = %e, "a downloads file cannot be read; started over");
            T::default()
        }),
        Err(_) => T::default(),
    }
}

/// Write `value` to `path` as JSON, whole or not at all.
pub(crate) fn write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    write_whole(
        path,
        &serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?,
    )
}

/// [`write_json`] on one line, with no spaces.
fn write_json_compact<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    write_whole(
        path,
        &serde_json::to_vec(value).map_err(std::io::Error::other)?,
    )
}

/// Write `bytes` to `path` whole or not at all: to a file beside it,
/// renamed over it.
fn write_whole(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().expect("a file in a directory");
    std::fs::create_dir_all(dir)?;
    let mut new = path.as_os_str().to_owned();
    new.push(".new");
    let new = PathBuf::from(new);
    let mut file = std::fs::File::create(&new)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&new, path)
}

/// [`dir_name`] undone; `None` for a name it would not make.
fn undo_dir_name(name: &str) -> Option<String> {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `a` is further along than `b`: finished, or with more on disk.
fn further(a: &Manifest, b: &Manifest) -> bool {
    let done = |m: &Manifest| matches!(m.phase, DownloadPhase::Done);
    (done(a), a.offset) > (done(b), b.offset)
}

/// A server's users' directories: every one but its files'.
fn user_dirs(server: &Path) -> Vec<PathBuf> {
    read_dir(server)
        .into_iter()
        .filter(|d| d.file_name().is_none_or(|n| n != FILES))
        .collect()
}

/// The directories in `dir`; none if it cannot be read.
fn read_dir(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect()
}

/// The manifest at `path`; `None` if there is none, or it is one this
/// build does not read (a newer version, an unknown kind).
fn read_manifest(path: &Path) -> std::io::Result<Option<Manifest>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    // Read whole at once; the header alone only when that fails, to tell
    // a newer build's entry from a broken one.
    let (version, kind, read) = match serde_json::from_slice::<Manifest>(&bytes) {
        Ok(m) => (m.version, m.kind, Ok(m)),
        Err(e) => {
            let header: Header = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
            (header.version, header.kind, Err(e))
        }
    };
    if version > VERSION || kind == EntryKind::Unknown {
        tracing::debug!(path = %path.display(), version, "an entry of a newer build; left as it is");
        return Ok(None);
    }
    let mut m = read.map_err(std::io::Error::other)?;
    if m.version < VERSION {
        m.upgrade();
    }
    Ok(Some(m))
}

/// `path`'s SHA-256, as 64 hex digits, read on a blocking thread: a film
/// is gigabytes.
pub(crate) async fn sha256_file(path: &Path) -> std::io::Result<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || sha256_of(&path))
        .await
        .map_err(std::io::Error::other)?
}

/// `path`'s SHA-256, as 64 hex digits.
fn sha256_of(path: &Path) -> std::io::Result<String> {
    use sha2::Digest;
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_types::{MediaKind, Movie, Namespace, PlaybackState};

    fn movie() -> Item {
        Item::Movie(Movie {
            id: MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
            title: "Alien".into(),
            year: Some(1979),
            release_date: None,
            summary: None,
            content_rating: None,
            duration: None,
            cast: Vec::new(),
            artwork: Vec::new(),
            playback: PlaybackState::default(),
        })
    }

    fn manifest(key: &str, created_at: i64) -> Manifest {
        Store::manifest(
            EntryKind::Film,
            key.into(),
            "server-key".into(),
            "local:0f3a".into(),
            "alice".into(),
            movie(),
            MediaTracks::default(),
            DownloadSource::Original,
            DownloadTracks::All,
            LocalWatch::default(),
            created_at,
        )
    }

    fn store(test: &str) -> Store {
        let dir =
            std::env::temp_dir().join(format!("sf-sdk-downloads-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Store::new(dir)
    }

    fn id() -> MediaId {
        MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap()
    }

    /// An item's downloads, from the index: one written after it was
    /// read is found, and one removed is not.
    #[test]
    fn an_item_s_downloads_are_found_as_they_are_written_and_removed() {
        let store = store("of-item");
        let first = manifest("aa", 1);
        store.write(&first).unwrap();
        assert_eq!(keys(store.of_item(&id())), ["aa"]);
        store.write(&manifest("bb", 2)).unwrap();
        store.remove(&first).unwrap();
        let other = MediaId::new(Namespace::Plex, MediaKind::Movie, "2").unwrap();
        assert_eq!(
            (keys(store.of_item(&id())), keys(store.of_item(&other))),
            (vec!["bb".to_owned()], Vec::new())
        );
    }

    /// Another process writes into the same directory (an iOS background
    /// session, a share extension, Android's WorkManager): its download
    /// of an item this one has read is found, with no rescan.
    #[test]
    fn another_process_s_download_of_an_item_is_found() {
        let here = store("other-process");
        here.write(&manifest("aa", 1)).unwrap();
        assert_eq!(keys(here.of_item(&id())), ["aa"]);
        let there = Store::new(here.root().to_owned());
        there.write(&manifest("bb", 2)).unwrap();
        let mut found = keys(here.of_item(&id()));
        found.sort();
        assert_eq!(found, ["aa", "bb"]);
    }

    /// A download read while its directory was there but its manifest not
    /// yet is found once the manifest is, though the listing didn't move.
    #[test]
    fn a_download_whose_manifest_came_after_its_directory_is_found() {
        let here = store("manifest-after");
        here.write(&manifest("aa", 1)).unwrap();
        let late = manifest("bb", 2);
        std::fs::create_dir_all(here.dir(&late)).unwrap();
        assert_eq!(keys(here.of_item(&id())), ["aa"]);
        let there = Store::new(here.root().to_owned());
        there.write(&late).unwrap();
        let mut found = keys(here.of_item(&id()));
        found.sort();
        assert_eq!(found, ["aa", "bb"]);
    }

    /// A rescan finds a download under way that another process started.
    #[test]
    fn a_rescan_finds_another_process_s_download_under_way() {
        let here = store("rescan");
        assert!(here.active().is_empty());
        let there = Store::new(here.root().to_owned());
        there.write(&manifest("aa", 1)).unwrap();
        here.rescan();
        assert_eq!(here.active(), ["aa"]);
    }

    fn keys(found: Vec<Manifest>) -> Vec<String> {
        found.into_iter().map(|m| m.key).collect()
    }

    #[test]
    fn the_same_request_has_the_same_key_whatever_the_order_of_its_tracks() {
        let chosen = |audio: Vec<u32>| DownloadTracks::Chosen {
            audio,
            subtitles: vec![2],
        };
        let key = |tracks| {
            key_of(
                "s",
                EntryKind::Film,
                &id(),
                &DownloadSource::Original,
                &tracks,
            )
        };
        assert_eq!(key(chosen(vec![1, 0])), key(chosen(vec![0, 1, 1])));
        assert_ne!(key(chosen(vec![0])), key(chosen(vec![0, 1])));
        assert_ne!(key(chosen(vec![0])), key(DownloadTracks::All));
    }

    #[test]
    fn another_server_kind_or_source_has_another_key() {
        let all = DownloadTracks::All;
        let original = DownloadSource::Original;
        let film = EntryKind::Film;
        let base = key_of("s", film, &id(), &original, &all);
        assert_ne!(base, key_of("t", film, &id(), &original, &all));
        assert_ne!(
            base,
            key_of("s", EntryKind::Episode, &id(), &original, &all)
        );
        let mobile = DownloadSource::Profile {
            name: "Mobile".into(),
        };
        assert_ne!(base, key_of("s", film, &id(), &mobile, &all));
        assert_eq!(base.len(), 32);
    }

    /// A key names a directory already on disk: the same inputs give the
    /// same bytes, build after build.
    #[test]
    fn a_download_s_key_does_not_change() {
        let key = key_of(
            "s",
            EntryKind::Film,
            &id(),
            &DownloadSource::Original,
            &DownloadTracks::All,
        );
        assert_eq!(key, "ab616441e5cf44e6d44aea3cbbe01e32");
        let track = MediaId::new(Namespace::Plex, MediaKind::Track, "32").unwrap();
        assert_eq!(kept_key("s", &track), "5846f3e3807adad07ba3f7df6d6aeadc");
    }

    /// A track kept from listening and the same track downloaded are two
    /// entries; each music container has the extension players expect.
    #[test]
    fn a_kept_track_has_a_key_of_its_own_and_music_its_own_extensions() {
        let track = MediaId::new(Namespace::Plex, MediaKind::Track, "32").unwrap();
        let downloaded = key_of(
            "s",
            EntryKind::Track,
            &track,
            &DownloadSource::Original,
            &DownloadTracks::All,
        );
        assert_ne!(kept_key("s", &track), downloaded);
        assert_ne!(kept_key("s", &track), kept_key("t", &track));
        assert_eq!(
            [
                Container::Mp4,
                Container::Flac,
                Container::Mp3,
                Container::Ogg,
                Container::Matroska
            ]
            .map(|c| extension(EntryKind::Track, c)),
            ["m4a", "flac", "mp3", "ogg", "mka"]
        );
        assert_eq!(extension(EntryKind::Film, Container::Mp4), "mp4");
    }

    /// A file is the server's on this device, not a user's: beside the
    /// users' directories, under a name none of theirs can have.
    #[test]
    fn a_download_lives_under_its_server_s_files() {
        let store = store("layout");
        let m = manifest("ab12", 1);
        assert_eq!(
            store.dir(&m),
            store
                .root()
                .join("server-key")
                .join("%files")
                .join("film")
                .join("ab12")
        );
        assert_eq!(
            store.user_dir("server-key", "local:0f3a"),
            store.root().join("server-key").join("local%3A0f3a")
        );
        assert_ne!(dir_name("%files"), FILES);
        assert_eq!(undo_dir_name("local%3A0f3a").as_deref(), Some("local:0f3a"));
        assert_eq!(dir_name(".."), "%2E%2E");
    }

    #[test]
    fn a_manifest_written_reads_back_and_a_half_written_one_is_not_listed() {
        let store = store("manifest");
        let m = manifest("ab12", 1);
        store.write(&m).unwrap();
        assert_eq!(store.find("ab12"), Some(m.clone()));
        // Pretty, as builds before wrote it.
        let written = std::fs::read(store.dir(&m).join(MANIFEST)).unwrap();
        assert_eq!(written, serde_json::to_vec_pretty(&m).unwrap());
        // A crash while writing the next leaves its new file beside it:
        // the last one whole is what is read.
        std::fs::write(
            store.dir(&m).join(format!("{MANIFEST}.new")),
            b"{\"version\":",
        )
        .unwrap();
        assert_eq!(store.scan(), vec![m.clone()]);
        // A directory with no manifest is a download that never started.
        std::fs::create_dir_all(store.files_dir("server-key").join("film").join("cd34")).unwrap();
        assert_eq!(store.scan(), vec![m]);
    }

    #[test]
    fn downloads_are_listed_oldest_first() {
        let store = store("order");
        store.write(&manifest("bb", 20)).unwrap();
        store.write(&manifest("aa", 10)).unwrap();
        let keys: Vec<String> = store.scan().into_iter().map(|m| m.key).collect();
        assert_eq!(keys, ["aa", "bb"]);
    }

    /// An album's tracks, started in the same second, move in the album's
    /// order, not their keys'.
    #[test]
    fn what_was_started_together_moves_in_its_own_order() {
        let store = store("place");
        let mut first = manifest("ff", 10);
        let mut second = manifest("aa", 10);
        (first.place, second.place) = (0, 1);
        store.write(&second).unwrap();
        store.write(&first).unwrap();
        assert_eq!(store.active(), ["ff", "aa"]);
        let fresh = Store::new(store.root().to_path_buf());
        assert_eq!(fresh.active(), ["ff", "aa"]);
        let keys: Vec<String> = store.scan().into_iter().map(|m| m.key).collect();
        assert_eq!(keys, ["ff", "aa"]);
    }

    /// What is under way is kept as each download is written and removed,
    /// and is what a fresh read of the disk finds.
    #[test]
    fn the_downloads_under_way_follow_each_write() {
        let store = store("active");
        store.write(&manifest("bb", 20)).unwrap();
        assert_eq!(store.active(), ["bb"]);
        let older = manifest("aa", 10);
        store.write(&older).unwrap();
        let mut done = manifest("cc", 5);
        store.write(&done).unwrap();
        assert_eq!(store.active(), ["cc", "aa", "bb"]);
        done.phase = DownloadPhase::Done;
        store.write(&done).unwrap();
        store.remove(&older).unwrap();
        assert_eq!(store.active(), ["bb"]);
        let other = Store::new(store.root().to_path_buf());
        assert_eq!(other.active(), ["bb"]);
        // Another process finished it.
        store.recheck("bb");
        assert_eq!(store.active(), ["bb"]);
        let mut bb = manifest("bb", 20);
        bb.phase = DownloadPhase::Done;
        other.write(&bb).unwrap();
        store.recheck("bb");
        assert!(store.active().is_empty());
    }

    #[test]
    fn a_key_that_is_not_a_hash_finds_nothing() {
        let store = store("find");
        store.write(&manifest("ab12", 1)).unwrap();
        assert_eq!(store.find("../ab12"), None);
        assert_eq!(store.find("ab12").map(|m| m.key), Some("ab12".into()));
    }

    /// A file held twice (PX-51: an album and a playlist) stays when one
    /// lets go, and goes with the last.
    #[test]
    fn a_file_goes_only_when_nothing_holds_it() {
        let store = store("holds");
        let album = Hold {
            by: "album".into(),
            id: Some("plex:album:3".into()),
            user: Some("local:0f3a".into()),
        };
        let mut m = manifest("ab12", 1);
        m.add_hold(&album);
        m.add_hold(&album);
        assert_eq!(m.holds, [Hold::download("local:0f3a"), album.clone()]);
        store.write(&m).unwrap();
        assert!(
            !store
                .release(&mut m, &Hold::download("local:0f3a"))
                .unwrap()
        );
        assert_eq!(
            store.find("ab12").map(|m| m.holds),
            Some(vec![album.clone()])
        );
        assert!(store.release(&mut m, &album).unwrap());
        assert_eq!(store.find("ab12"), None);
        assert!(!store.dir(&m).exists());
    }

    /// A track is read from its own directory: the same one `find`
    /// finds, and none of another server's.
    #[test]
    fn a_track_is_found_where_it_is() {
        let store = store("track");
        let mut m = manifest("ab12", 1);
        m.kind = EntryKind::Track;
        store.write(&m).unwrap();
        assert_eq!(store.find_track("server-key", "ab12"), store.find("ab12"));
        assert!(store.find("ab12").is_some());
        assert_eq!(store.find_track("other", "ab12"), None);
        assert_eq!(store.find_track("server-key", "../ab12"), None);
    }

    /// The user fetching a file lets go of it while another still holds
    /// it: the other fetches it, prepared again as theirs.
    #[test]
    fn a_file_another_user_holds_is_handed_to_them() {
        let store = store("handoff");
        let mut m = manifest("ab12", 1);
        m.id = Some("alice's download".into());
        m.phase = DownloadPhase::Transferring;
        m.add_hold(&Hold::download("local:bob"));
        store.write(&m).unwrap();
        assert!(
            !store
                .release(&mut m, &Hold::download("local:0f3a"))
                .unwrap()
        );
        let m = store.find("ab12").unwrap();
        assert_eq!(m.user_id, "local:bob");
        assert_eq!(m.id, None);
        assert!(matches!(m.phase, DownloadPhase::Preparing { .. }));
    }

    /// A finished file stays as it is when its fetcher lets go.
    #[test]
    fn a_finished_file_is_not_fetched_again_for_another_user() {
        let store = store("handoff-done");
        let mut m = manifest("ab12", 1);
        m.phase = DownloadPhase::Done;
        m.add_hold(&Hold::download("local:bob"));
        store.write(&m).unwrap();
        assert!(
            !store
                .release(&mut m, &Hold::download("local:0f3a"))
                .unwrap()
        );
        let m = store.find("ab12").unwrap();
        assert_eq!(m.phase, DownloadPhase::Done);
        assert_eq!(m.user_id, "local:0f3a");
    }

    /// Who may see a file: whoever holds it, and every profile for one
    /// kept from listening.
    #[test]
    fn a_file_is_each_holder_s_and_a_kept_one_everyone_s() {
        let mut m = manifest("ab12", 1);
        assert!(m.is_for("local:0f3a"));
        assert!(!m.is_for("local:bob"));
        m.add_hold(&Hold::listening());
        assert!(m.is_for("local:bob"));
        assert!(!m.held_by("local:bob"));
    }

    /// 0.14.0's layout: each user's files under their directory. Moved
    /// under the new keys, the same film twice becomes one file held by
    /// both with each one's watch state, a kept track moves, and what is
    /// the user's own (their music, the watch of deleted downloads) stays.
    #[test]
    fn the_per_user_layout_of_0_14_0_is_moved_and_merged() {
        let store = store("migrate");
        let old = |user: &str, kind: &str, key: &str| {
            store.user_dir("server-key", user).join(kind).join(key)
        };
        let write_old = |user: &str, m: serde_json::Value, done: bool| {
            let dir = old(
                user,
                m["kind"].as_str().unwrap(),
                m["key"].as_str().unwrap(),
            );
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(MANIFEST), m.to_string()).unwrap();
            if done {
                std::fs::write(dir.join("media.mkv"), user).unwrap();
            }
        };
        // As 0.14.0 wrote them: version 1, one `watch`, holds with no user.
        let v1 = |user: &str, key: &str, position: u64, done: bool| {
            let mut m = serde_json::to_value(manifest(key, 1)).unwrap();
            m["version"] = 1.into();
            m["user_id"] = user.into();
            m["holds"] = serde_json::json!([{"by": "download", "id": null}]);
            m.as_object_mut().unwrap().remove("watches");
            m["watch"] = serde_json::to_value(LocalWatch {
                position_ms: Some(position),
                watched: None,
                at: 5,
                pending: true,
            })
            .unwrap();
            if done {
                m["phase"] = serde_json::to_value(DownloadPhase::Done).unwrap();
                m["file"] = serde_json::json!(null);
            }
            m
        };
        write_old("local:0f3a", v1("local:0f3a", "aaaa", 10, true), true);
        write_old("local:bob", v1("local:bob", "bbbb", 20, false), false);
        let track = MediaId::new(Namespace::Plex, MediaKind::Track, "32").unwrap();
        let old_kept = key_hash(&["listening:1", "server-key", "local:bob", &track.to_string()]);
        let mut kept = v1("local:bob", &old_kept, 0, true);
        kept["kind"] = "track".into();
        kept["item"]["id"] = serde_json::to_value(&track).unwrap();
        kept["holds"] = serde_json::json!([{"by": "listening", "id": null}]);
        write_old("local:bob", kept, true);
        let music = store.music_path("server-key", "local:bob");
        std::fs::write(&music, "{}").unwrap();

        store.migrate();

        let key = key_of(
            "server-key",
            EntryKind::Film,
            &id(),
            &DownloadSource::Original,
            &DownloadTracks::All,
        );
        let films: Vec<Manifest> = store
            .scan()
            .into_iter()
            .filter(|m| m.kind == EntryKind::Film)
            .collect();
        assert_eq!(films.len(), 1);
        let film = &films[0];
        assert_eq!(film.key, key);
        assert_eq!(film.version, VERSION);
        assert_eq!(
            film.holds,
            [Hold::download("local:0f3a"), Hold::download("local:bob")]
        );
        assert_eq!(film.watch_of("local:0f3a").position_ms, Some(10));
        assert_eq!(film.watch_of("local:bob").position_ms, Some(20));
        // The finished one stayed, with its file.
        assert_eq!(film.phase, DownloadPhase::Done);
        assert_eq!(
            std::fs::read_to_string(store.dir(film).join("media.mkv")).unwrap(),
            "local:0f3a"
        );
        let moved = store
            .find_track("server-key", &kept_key("server-key", &track))
            .expect("the kept track moved");
        assert!(moved.cached_only());
        assert!(moved.is_for("local:0f3a"));
        assert!(!old("local:0f3a", "film", "aaaa").exists());
        assert!(
            !store
                .user_dir("server-key", "local:bob")
                .join("film")
                .exists()
        );
        assert!(
            !store
                .user_dir("server-key", "local:bob")
                .join("track")
                .exists()
        );
        assert!(music.exists());
        // Run again: nothing more to do.
        store.migrate();
        assert_eq!(store.scan().len(), 2);
    }

    /// A manifest written before a failure had a cause still loads: its
    /// failure's cause is `other`.
    #[test]
    fn a_failure_kept_before_causes_loads_as_other() {
        let store = store("old-failure");
        let m = manifest("ab12", 1);
        let mut json = serde_json::to_value(&m).unwrap();
        json["phase"] = serde_json::json!({"type": "failed", "reason": "the conversion failed"});
        let path = store.dir(&m).join(MANIFEST);
        std::fs::create_dir_all(store.dir(&m)).unwrap();
        std::fs::write(&path, json.to_string()).unwrap();
        assert_eq!(
            store.find("ab12").map(|m| m.phase),
            Some(DownloadPhase::Failed {
                reason: "the conversion failed".into(),
                cause: spoolframe_protocol::downloads::DownloadFailure::Other,
            })
        );
    }

    /// A manifest written before the expected size was kept still loads,
    /// with none.
    #[test]
    fn a_manifest_without_an_expected_size_loads_with_none() {
        let store = store("old-expected");
        let mut m = manifest("ab12", 1);
        m.expected_size = Some(5);
        let mut json = serde_json::to_value(&m).unwrap();
        json.as_object_mut().unwrap().remove("expected_size");
        std::fs::create_dir_all(store.dir(&m)).unwrap();
        std::fs::write(store.dir(&m).join(MANIFEST), json.to_string()).unwrap();
        assert_eq!(store.find("ab12").map(|m| m.expected_size), Some(None));
    }

    /// A newer build's entry (a kind this one does not know, or a later
    /// layout) is not listed, and its directory is left as it is.
    #[test]
    fn an_entry_of_a_newer_build_is_left_alone() {
        let store = store("newer");
        let m = manifest("ab12", 1);
        store.write(&m).unwrap();
        let path = store.dir(&m).join(MANIFEST);
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        json["version"] = (VERSION + 1).into();
        std::fs::write(&path, json.to_string()).unwrap();
        let photo = store.files_dir("server-key").join("photo").join("cd34");
        std::fs::create_dir_all(&photo).unwrap();
        std::fs::write(
            photo.join(MANIFEST),
            r#"{"version":1,"kind":"photo","whatever":true}"#,
        )
        .unwrap();
        assert!(store.scan().is_empty());
        assert_eq!(store.find("cd34"), None);
        assert!(path.exists() && photo.join(MANIFEST).exists());
    }

    /// A newer build's manifest this one cannot read whole is left alone;
    /// one of this build's that cannot be read is broken.
    #[test]
    fn a_manifest_this_build_cannot_read_is_newer_or_broken() {
        let store = store("unreadable");
        std::fs::create_dir_all(store.root()).unwrap();
        let path = store.root().join(MANIFEST);
        std::fs::write(&path, r#"{"version":3,"kind":"film","holds":"new"}"#).unwrap();
        assert!(matches!(read_manifest(&path), Ok(None)));
        std::fs::write(&path, r#"{"version":2,"kind":"film","holds":"new"}"#).unwrap();
        assert!(read_manifest(&path).is_err());
        std::fs::write(&path, b"{").unwrap();
        assert!(read_manifest(&path).is_err());
    }

    #[test]
    fn a_deleted_download_s_unsent_watch_is_kept_for_the_next_sync() {
        let store = store("carried");
        let mut m = manifest("ab12", 1);
        *m.watch_mut("local:0f3a") = LocalWatch {
            position_ms: Some(90_000),
            watched: None,
            at: 1_700_000_000,
            pending: true,
        };
        store.write(&m).unwrap();
        store.remove(&m).unwrap();
        assert!(store.scan().is_empty());
        // On one line, as builds before wrote it.
        let written =
            std::fs::read(store.user_dir("server-key", "local:0f3a").join(CARRIED)).unwrap();
        assert_eq!(
            written,
            serde_json::to_vec(&store.carried("server-key", "local:0f3a")).unwrap()
        );
        assert_eq!(
            store.carried("server-key", "local:0f3a"),
            vec![OfflineWatch {
                id: id(),
                position_ms: Some(90_000),
                watched: None,
                at: 1_700_000_000,
            }]
        );
        assert_eq!(
            store.carried_users(),
            vec![("server-key".to_owned(), "local:0f3a".to_owned())]
        );
        // A sent one is not kept.
        let mut sent = manifest("cd34", 2);
        sent.watch_mut("local:0f3a").pending = false;
        sent.watch_mut("local:0f3a").position_ms = Some(5);
        store.write(&sent).unwrap();
        store.remove(&sent).unwrap();
        assert_eq!(store.carried("server-key", "local:0f3a").len(), 1);
    }

    /// What the downloads take is every file's bytes, however deep.
    #[test]
    fn the_space_used_counts_every_file() {
        let store = store("used");
        let m = manifest("ab12", 1);
        store.write(&m).unwrap();
        let manifest_len = std::fs::metadata(store.dir(&m).join(MANIFEST))
            .unwrap()
            .len();
        std::fs::write(store.dir(&m).join("media.mkv"), vec![0u8; 1000]).unwrap();
        assert_eq!(store.used(), manifest_len + 1000);
    }

    #[tokio::test]
    async fn a_file_s_sha256_is_its_hex_digest() {
        let store = store("sha");
        std::fs::create_dir_all(store.root()).unwrap();
        let path = store.root().join("f");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_file(&path).await.unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
