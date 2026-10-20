//! Downloads on disk (PX-38): one directory per stored file,
//! `<dir>/<server>/<user>/<kind>/<key>/`, holding `manifest.json`, the
//! media (`media.part` while it arrives, then `media.mp4` or `media.mkv`)
//! and the artwork (`art/*.jpg`). Everything about an entry is in its
//! manifest, so the list needs no server.
//!
//! Entries are keyed by their item's kind ([`EntryKind`]): films and
//! episodes now; music (PX-51) adds tracks beside them. A stored file is
//! kept while anything holds it ([`Hold`]): a film's download is its one
//! hold, while a track may be held by an album, a playlist and the cache
//! of what was kept from listening, and goes only when none is left.
//!
//! A manifest is written whole to a file beside it and renamed over it:
//! a crash leaves the old one or the new one, never half of either. A
//! directory without a manifest is a download that never started, and
//! is not listed; neither is one of a newer version or a kind this build
//! does not know, which is left as it is.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use serde::{Deserialize, Serialize};
use spoolframe_protocol::downloads::{DownloadFile, DownloadSource, DownloadTracks, OfflineWatch};
use spoolframe_types::{ArtworkKind, Container, Item, Marker, MediaId, MediaTracks};

use super::{DownloadPhase, LocalWatch};

/// This layout's version, in each manifest.
const VERSION: u32 = 1;
const MANIFEST: &str = "manifest.json";
/// Where a manifest is written before it is renamed over the old one.
const MANIFEST_NEW: &str = "manifest.json.new";
const PART: &str = "media.part";
const ART: &str = "art";
/// Offline watch entries of downloads deleted before they were sent,
/// per server and user: sent with the rest on the next sync.
const CARRIED: &str = "watch.json";

/// What kind of item an entry stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EntryKind {
    Film,
    Episode,
    /// A kind a newer build stores (a track): never listed, moved or
    /// removed here.
    #[serde(other)]
    Unknown,
}

impl EntryKind {
    /// The kind of `id`'s entry; `None` for one nothing here stores.
    pub fn of(id: &MediaId) -> Option<Self> {
        match id.kind() {
            spoolframe_types::MediaKind::Movie => Some(Self::Film),
            spoolframe_types::MediaKind::Episode => Some(Self::Episode),
            _ => None,
        }
    }

    /// Its directory under a user's.
    fn dir(self) -> &'static str {
        match self {
            Self::Film => "film",
            Self::Episode => "episode",
            Self::Unknown => "unknown",
        }
    }
}

/// What keeps a stored file: `by` what (`download`, the user's download
/// of the item; PX-51 adds `album`, `playlist`, `listening`), and which
/// one when there can be several (an album's id). Plain text, so a build
/// that does not know a newer hold still counts it, and writes it back
/// as it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Hold {
    pub by: String,
    pub id: Option<String>,
}

impl Hold {
    /// The user's download of the item itself.
    pub fn download() -> Self {
        Self {
            by: "download".into(),
            id: None,
        }
    }
}

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
    pub sha256: Option<String>,
    /// How much of `media.part` is known to be on disk (synced).
    pub offset: u64,
    pub phase: DownloadPhase,
    pub watch: LocalWatch,
    /// Why it is not moving, while it waits to try again.
    pub note: Option<String>,
    /// Unix seconds, by this device's clock.
    pub created_at: i64,
}

/// A picture kept with a download: `file` in its `art` directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredArtwork {
    /// The item it is of: the download's, or its show's or season's.
    pub item: MediaId,
    pub kind: ArtworkKind,
    pub file: String,
}

/// The key of a download: the same for the same server, user, kind,
/// item, source and tracks, so starting one twice finds the first. Order
/// and repeats of chosen tracks do not count, as on the server.
pub(crate) fn key_of(
    server: &str,
    user_id: &str,
    kind: EntryKind,
    id: &MediaId,
    source: &DownloadSource,
    tracks: &DownloadTracks,
) -> String {
    let tracks = tracks.normalized();
    key_hash(&[
        "download:1",
        server,
        user_id,
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

/// The extension of a finished file in `container`.
pub(crate) fn extension(container: Container) -> &'static str {
    match container {
        Container::Mp4 => "mp4",
        _ => "mkv",
    }
}

/// The downloads directory.
#[derive(Debug, Clone)]
pub(crate) struct Store {
    root: PathBuf,
    /// The downloads under way, as `(created_at, key)`: read from the
    /// disk at first use, then kept by each write and removal here, so
    /// the manager need not read every manifest to find the next.
    active: Arc<Mutex<Option<Active>>>,
}

/// Downloads by `(created_at, key)`: oldest first.
type Active = BTreeSet<(i64, String)>;

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            active: Arc::default(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where `server`'s downloads for `user_id` are.
    pub fn user_dir(&self, server: &str, user_id: &str) -> PathBuf {
        self.root.join(dir_name(server)).join(dir_name(user_id))
    }

    /// Where download `m` is.
    pub fn dir(&self, m: &Manifest) -> PathBuf {
        self.user_dir(&m.server, &m.user_id)
            .join(m.kind.dir())
            .join(&m.key)
    }

    pub fn part(&self, m: &Manifest) -> PathBuf {
        self.dir(m).join(PART)
    }

    /// The finished file, once the container is known.
    pub fn media(&self, m: &Manifest) -> Option<PathBuf> {
        let container = m.file.as_ref()?.container;
        Some(self.dir(m).join(format!("media.{}", extension(container))))
    }

    pub fn art_dir(&self, m: &Manifest) -> PathBuf {
        self.dir(m).join(ART)
    }

    /// Write `m`, whole or not at all.
    pub fn write(&self, m: &Manifest) -> std::io::Result<()> {
        let dir = self.dir(m);
        std::fs::create_dir_all(&dir)?;
        let new = dir.join(MANIFEST_NEW);
        let mut file = std::fs::File::create(&new)?;
        file.write_all(&serde_json::to_vec_pretty(m).map_err(std::io::Error::other)?)?;
        file.sync_all()?;
        std::fs::rename(&new, dir.join(MANIFEST))?;
        self.note(m, m.phase.is_active());
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
                .map(|m| (m.created_at, m.key))
                .collect()
        });
        active.iter().map(|(_, key)| key.clone()).collect()
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
            active.retain(|(_, k)| k != key);
        }
    }

    /// `m` is under way, or no longer.
    fn note(&self, m: &Manifest, under_way: bool) {
        if let Some(active) = self.active.lock().as_mut() {
            let entry = (m.created_at, m.key.clone());
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
            holds: vec![Hold::download()],
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
            sha256: None,
            offset: 0,
            phase: DownloadPhase::Preparing {
                server: spoolframe_protocol::downloads::DownloadState::Waiting,
            },
            watch,
            note: None,
            created_at,
        }
    }

    /// Every download, oldest first. One whose manifest cannot be read is
    /// left out, and said so in the log.
    pub fn scan(&self) -> Vec<Manifest> {
        let mut found = Vec::new();
        for server in read_dir(&self.root) {
            for user in read_dir(&server) {
                for kind in read_dir(&user) {
                    for key in read_dir(&kind) {
                        let path = key.join(MANIFEST);
                        match read_manifest(&path) {
                            Ok(Some(m)) => found.push(m),
                            Ok(None) => {}
                            Err(e) => {
                                tracing::warn!(path = %path.display(), error = %e, "a download's manifest cannot be read; left out")
                            }
                        }
                    }
                }
            }
        }
        found.sort_by(|a, b| (a.created_at, &a.key).cmp(&(b.created_at, &b.key)));
        found
    }

    /// Download `key`, if there is one.
    pub fn find(&self, key: &str) -> Option<Manifest> {
        // A key is a hash: never a path of its own.
        if !key.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        for server in read_dir(&self.root) {
            for user in read_dir(&server) {
                for kind in read_dir(&user) {
                    if let Ok(Some(m)) = read_manifest(&kind.join(key).join(MANIFEST)) {
                        return Some(m);
                    }
                }
            }
        }
        None
    }

    /// Let go of `hold` on `m`: with nothing left holding it, it is taken
    /// off the disk ([`Store::remove`]) and this is true; else the
    /// manifest is written with the holds left.
    pub fn release(&self, m: &mut Manifest, hold: &Hold) -> std::io::Result<bool> {
        m.holds.retain(|h| h != hold);
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
        if m.watch.pending
            && let Some(entry) = m.watch.entry(&m.item)
        {
            let mut carried = self.carried(&m.server, &m.user_id);
            carried.retain(|c| c.id != entry.id);
            carried.push(entry);
            self.set_carried(&m.server, &m.user_id, &carried)?;
        }
        match std::fs::remove_dir_all(self.dir(m)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        self.note(m, false);
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
        let dir = self.user_dir(server, user_id);
        let path = dir.join(CARRIED);
        if entries.is_empty() {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        std::fs::create_dir_all(&dir)?;
        let new = dir.join(format!("{CARRIED}.new"));
        let mut file = std::fs::File::create(&new)?;
        file.write_all(&serde_json::to_vec(entries).map_err(std::io::Error::other)?)?;
        file.sync_all()?;
        std::fs::rename(&new, path)
    }

    /// The servers and users with offline watch entries of deleted
    /// downloads.
    pub fn carried_users(&self) -> Vec<(String, String)> {
        let mut users = Vec::new();
        for m in self.scan() {
            if m.watch.pending {
                users.push((m.server.clone(), m.user_id.clone()));
            }
        }
        for server in read_dir(&self.root) {
            for user in read_dir(&server) {
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

    /// The bytes every download takes on disk.
    pub fn used(&self) -> u64 {
        fn walk(dir: &Path) -> u64 {
            read_dir(dir)
                .into_iter()
                .map(|path| match std::fs::symlink_metadata(&path) {
                    Ok(meta) if meta.is_dir() => walk(&path),
                    Ok(meta) => meta.len(),
                    Err(_) => 0,
                })
                .sum()
        }
        walk(&self.root)
    }
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
    read.map(Some).map_err(std::io::Error::other)
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
pub(crate) fn sha256_of(path: &Path) -> std::io::Result<String> {
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

    #[test]
    fn the_same_request_has_the_same_key_whatever_the_order_of_its_tracks() {
        let chosen = |audio: Vec<u32>| DownloadTracks::Chosen {
            audio,
            subtitles: vec![2],
        };
        let key = |tracks| {
            key_of(
                "s",
                "u",
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
    fn another_user_server_or_source_has_another_key() {
        let all = DownloadTracks::All;
        let original = DownloadSource::Original;
        let film = EntryKind::Film;
        let base = key_of("s", "u", film, &id(), &original, &all);
        assert_ne!(base, key_of("s", "v", film, &id(), &original, &all));
        assert_ne!(base, key_of("t", "u", film, &id(), &original, &all));
        assert_ne!(
            base,
            key_of("s", "u", EntryKind::Episode, &id(), &original, &all)
        );
        let mobile = DownloadSource::Profile {
            name: "Mobile".into(),
        };
        assert_ne!(base, key_of("s", "u", film, &id(), &mobile, &all));
        assert_eq!(base.len(), 32);
    }

    /// A key names a directory already on disk: the same inputs give the
    /// same bytes, build after build.
    #[test]
    fn a_download_s_key_does_not_change() {
        let key = key_of(
            "s",
            "u",
            EntryKind::Film,
            &id(),
            &DownloadSource::Original,
            &DownloadTracks::All,
        );
        assert_eq!(key, "d36a1f2bce4c77f44634c05b7a6a0f7a");
    }

    #[test]
    fn a_download_lives_under_its_server_and_user() {
        let store = store("layout");
        let m = manifest("ab12", 1);
        assert_eq!(
            store.dir(&m),
            store
                .root()
                .join("server-key")
                .join("local%3A0f3a")
                .join("film")
                .join("ab12")
        );
        assert_eq!(undo_dir_name("local%3A0f3a").as_deref(), Some("local:0f3a"));
        assert_eq!(dir_name(".."), "%2E%2E");
    }

    #[test]
    fn a_manifest_written_reads_back_and_a_half_written_one_is_not_listed() {
        let store = store("manifest");
        let m = manifest("ab12", 1);
        store.write(&m).unwrap();
        assert_eq!(store.find("ab12"), Some(m.clone()));
        // A crash while writing the next leaves its new file beside it:
        // the last one whole is what is read.
        std::fs::write(store.dir(&m).join(MANIFEST_NEW), b"{\"version\":").unwrap();
        assert_eq!(store.scan(), vec![m.clone()]);
        // A directory with no manifest is a download that never started.
        std::fs::create_dir_all(
            store
                .user_dir("server-key", "local:0f3a")
                .join("film")
                .join("cd34"),
        )
        .unwrap();
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
        };
        let mut m = manifest("ab12", 1);
        m.holds.push(album.clone());
        store.write(&m).unwrap();
        assert!(!store.release(&mut m, &Hold::download()).unwrap());
        assert_eq!(
            store.find("ab12").map(|m| m.holds),
            Some(vec![album.clone()])
        );
        assert!(store.release(&mut m, &album).unwrap());
        assert_eq!(store.find("ab12"), None);
        assert!(!store.dir(&m).exists());
    }

    /// A newer build's entry (a track, or a later layout) is not listed,
    /// and its directory is left as it is.
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
        let track = store
            .user_dir("server-key", "local:0f3a")
            .join("track")
            .join("cd34");
        std::fs::create_dir_all(&track).unwrap();
        std::fs::write(
            track.join(MANIFEST),
            r#"{"version":1,"kind":"track","whatever":true}"#,
        )
        .unwrap();
        assert!(store.scan().is_empty());
        assert_eq!(store.find("cd34"), None);
        assert!(path.exists() && track.join(MANIFEST).exists());
    }

    /// A newer build's manifest this one cannot read whole is left alone;
    /// one of this build's that cannot be read is broken.
    #[test]
    fn a_manifest_this_build_cannot_read_is_newer_or_broken() {
        let store = store("unreadable");
        std::fs::create_dir_all(store.root()).unwrap();
        let path = store.root().join(MANIFEST);
        std::fs::write(&path, r#"{"version":2,"kind":"film","holds":"new"}"#).unwrap();
        assert!(matches!(read_manifest(&path), Ok(None)));
        std::fs::write(&path, r#"{"version":1,"kind":"film","holds":"new"}"#).unwrap();
        assert!(read_manifest(&path).is_err());
        std::fs::write(&path, b"{").unwrap();
        assert!(read_manifest(&path).is_err());
    }

    #[test]
    fn a_deleted_download_s_unsent_watch_is_kept_for_the_next_sync() {
        let store = store("carried");
        let mut m = manifest("ab12", 1);
        m.watch = LocalWatch {
            position_ms: Some(90_000),
            watched: None,
            at: 1_700_000_000,
            pending: true,
        };
        store.write(&m).unwrap();
        store.remove(&m).unwrap();
        assert!(store.scan().is_empty());
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
        sent.watch.pending = false;
        sent.watch.position_ms = Some(5);
        store.write(&sent).unwrap();
        store.remove(&sent).unwrap();
        assert_eq!(store.carried("server-key", "local:0f3a").len(), 1);
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
