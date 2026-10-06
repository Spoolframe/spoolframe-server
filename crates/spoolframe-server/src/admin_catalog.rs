//! Catalog browsing for admins (P3-16): a library's rows, one item's
//! files and match, and each library's last path check, read through the
//! catalog. Read-only.
//!
//! A path check reads every media file of a library (P1-04), which for a
//! library of tens of thousands of episodes takes a while, so it is never
//! made per page. Each library's last one is kept here: opening a library
//! starts one if there is none, Server › Media paths' check fills it in
//! for the libraries it covers, and "Missing files" lists it. Rows on a
//! page look for their own files as they are listed; a show's or album's
//! count of missing files comes from the check. A library's check belongs
//! to the catalog it ran on: when Plex servers, libraries or mappings
//! change, the catalog is replaced and its checks no longer count.
//!
//! Server › Media paths' last check of every library (P3-18) is kept by
//! what it read instead: the mappings and the served libraries' folders
//! ([`spoolframe_plex::PlexCatalog::paths_basis`]). A reorder of the libraries replaces
//! the catalog but reads the same, so the check stands; it is kept in the
//! state directory, so it stands after a restart too.

use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use jsonrpsee::core::RpcResult;
use jsonrpsee::types::ErrorObjectOwned;
use spoolframe_admin_api::{
    ArtworkInfo, CatalogCrumb, CatalogFilter, CatalogItem, CatalogKind, CatalogLibrary,
    CatalogPage, CatalogQuery, CatalogRow, FileState, KindCount, MediaPart, MediaVersion,
    PathCheckRun, PathMappingEntry, PathsChecked, error_code,
};
use spoolframe_catalog::{
    Browse, Catalog, CatalogError, FileCheck, Inspect, LibraryCheck, Page, Row, RowFilter,
};
use spoolframe_types::{Item, Library, LibraryId, LibraryKind, MediaId, MediaKind};

use super::{AdminApi, error};
use crate::catalog::Composite;
use crate::catalog::ids;
use crate::principal::Principal;

/// The most rows one page has.
pub const MAX_ROWS: u32 = 200;

/// Where one library's check is.
#[derive(Debug, Clone)]
enum State {
    Running { since: i64 },
    Done { at: i64, check: Arc<LibraryCheck> },
    Failed { at: i64, error: String },
}

#[derive(Debug)]
struct Entry {
    /// The catalog it ran on.
    on: Weak<Composite>,
    state: State,
}

/// Where the last check of every library is kept, in the state
/// directory.
const LAST_RUN_FILE: &str = "last-path-check.json";

/// A check of every library, and what it read: a hash of its
/// [`spoolframe_plex::PlexCatalog::paths_basis`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct KeptRun {
    basis: String,
    run: PathCheckRun,
}

/// Each library's last path check, by library id.
#[derive(Debug, Default)]
pub struct PathChecks {
    entries: Mutex<HashMap<String, Entry>>,
    /// Server › Media paths' last check of every library (P3-18).
    last_run: Mutex<Option<KeptRun>>,
    /// Where it is kept; in memory only without one.
    file: Option<PathBuf>,
}

/// `basis` as kept: a hash, so the file holds no paths of its own.
fn basis_hash(basis: &str) -> String {
    blake3::hash(basis.as_bytes()).to_hex().to_string()
}

impl PathChecks {
    /// The checks, with the last check of every library kept in
    /// `state_dir` and read from there now. A file that cannot be read is
    /// logged and left for the next check to replace.
    pub fn kept_in(state_dir: &Path) -> Self {
        let file = state_dir.join(LAST_RUN_FILE);
        let last = match std::fs::read(&file) {
            Ok(bytes) => match serde_json::from_slice::<KeptRun>(&bytes) {
                Ok(kept) => Some(kept),
                Err(e) => {
                    tracing::warn!(path = %file.display(), error = %e, "cannot read the last path check");
                    None
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                tracing::warn!(path = %file.display(), error = %e, "cannot read the last path check");
                None
            }
        };
        Self {
            entries: Mutex::default(),
            last_run: Mutex::new(last),
            file: Some(file),
        }
    }

    /// Keep `run`, a check of every library that read `basis`, as the
    /// last one: in memory, and in the state directory if there is one.
    pub fn keep_run(&self, basis: &str, run: PathCheckRun) {
        let kept = KeptRun {
            basis: basis_hash(basis),
            run,
        };
        if let Some(file) = &self.file
            && let Err(e) = write_private(file, &kept)
        {
            tracing::warn!(path = %file.display(), error = %e, "cannot keep the last path check");
        }
        *self.last_run.lock().expect("never held across a panic") = Some(kept);
    }

    /// The last check of every library, while what it read is the same
    /// as `basis`: one that read other mappings or folders says what they
    /// found, not what these would.
    pub fn last_run(&self, basis: &str) -> Option<PathCheckRun> {
        let last = self.last_run.lock().expect("never held across a panic");
        let kept = last.as_ref()?;
        (kept.basis == basis_hash(basis)).then(|| kept.run.clone())
    }

    fn current(&self, on: &Arc<Composite>, library: &str) -> Option<State> {
        let entries = self.entries.lock().expect("never held across a panic");
        let entry = entries.get(library)?;
        let same = entry.on.upgrade().is_some_and(|c| Arc::ptr_eq(&c, on));
        same.then(|| entry.state.clone())
    }

    /// Library `library`'s check on `on`, and its result once done.
    pub fn state(
        &self,
        on: &Arc<Composite>,
        library: &str,
    ) -> (PathsChecked, Option<Arc<LibraryCheck>>) {
        match self.current(on, library) {
            None => (PathsChecked::NotChecked, None),
            Some(State::Running { since }) => (PathsChecked::Checking { since }, None),
            Some(State::Done { at, check }) => (
                PathsChecked::Checked {
                    at,
                    ok: check.ok,
                    missing: u64::try_from(check.missing.len()).unwrap_or(u64::MAX),
                },
                Some(check),
            ),
            Some(State::Failed { at, error }) => (PathsChecked::Failed { at, error }, None),
        }
    }

    /// Keep `check` as `library`'s on `on`, made just now.
    pub fn record(&self, on: &Arc<Composite>, library: &str, check: LibraryCheck) {
        self.entries
            .lock()
            .expect("never held across a panic")
            .insert(
                library.to_owned(),
                Entry {
                    on: Arc::downgrade(on),
                    state: State::Done {
                        at: crate::store::now(),
                        check: Arc::new(check),
                    },
                },
            );
    }

    /// Check `library` on `on`, unless a check of it is running; with
    /// `again`, also when one is done. What it is now.
    pub fn start(
        self: &Arc<Self>,
        on: &Arc<Composite>,
        library: &LibraryId,
        again: bool,
    ) -> PathsChecked {
        let key = library.to_string();
        let now = crate::store::now();
        {
            let mut entries = self.entries.lock().expect("never held across a panic");
            let current = entries
                .get(&key)
                .filter(|e| e.on.upgrade().is_some_and(|c| Arc::ptr_eq(&c, on)));
            match current.map(|e| &e.state) {
                Some(State::Running { .. }) => {}
                Some(State::Done { .. } | State::Failed { .. }) if !again => {}
                _ => {
                    entries.insert(
                        key.clone(),
                        Entry {
                            on: Arc::downgrade(on),
                            state: State::Running { since: now },
                        },
                    );
                    let checks = self.clone();
                    let on = on.clone();
                    let library = library.clone();
                    let key = key.clone();
                    tokio::spawn(async move {
                        let result = on.check_library(&library).await;
                        let state = match result {
                            Ok(check) => State::Done {
                                at: crate::store::now(),
                                check: Arc::new(check),
                            },
                            Err(e) => {
                                tracing::warn!(%library, error = %e, "library path check failed");
                                State::Failed {
                                    at: crate::store::now(),
                                    error: e.to_string(),
                                }
                            }
                        };
                        let mut entries = checks.entries.lock().expect("never held across a panic");
                        // Only into the entry it was started for: a newer
                        // catalog's check is not replaced by an older one's.
                        if let Some(entry) = entries.get_mut(&key)
                            && entry.on.upgrade().is_some_and(|c| Arc::ptr_eq(&c, &on))
                        {
                            entry.state = state;
                        }
                    });
                }
            }
        }
        self.state(on, &key).0
    }
}

/// Write `kept` to `file` whole or not at all: a file beside it first,
/// readable by the server's user only, then renamed over it.
fn write_private(file: &Path, kept: &KeptRun) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let dir = file.parent().unwrap_or(Path::new("."));
    let partial = dir.join(format!(".{LAST_RUN_FILE}.new"));
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&partial)?;
    out.write_all(&serde_json::to_vec(kept)?)?;
    out.sync_all()?;
    std::fs::rename(&partial, file)
}

fn kind_of(kind: CatalogKind) -> MediaKind {
    match kind {
        CatalogKind::Movie => MediaKind::Movie,
        CatalogKind::Show => MediaKind::Show,
        CatalogKind::Season => MediaKind::Season,
        CatalogKind::Episode => MediaKind::Episode,
        CatalogKind::Artist => MediaKind::Artist,
        CatalogKind::Album => MediaKind::Album,
        CatalogKind::Track => MediaKind::Track,
    }
}

fn catalog_kind(kind: MediaKind) -> CatalogKind {
    match kind {
        MediaKind::Movie => CatalogKind::Movie,
        MediaKind::Show => CatalogKind::Show,
        MediaKind::Season => CatalogKind::Season,
        MediaKind::Episode => CatalogKind::Episode,
        MediaKind::Artist => CatalogKind::Artist,
        MediaKind::Album => CatalogKind::Album,
        MediaKind::Track => CatalogKind::Track,
    }
}

/// The kinds a library holds, top level first: what it counts, and what
/// its kind select offers.
pub fn kinds(library: LibraryKind) -> &'static [MediaKind] {
    match library {
        LibraryKind::Movies => &[MediaKind::Movie],
        LibraryKind::Shows => &[MediaKind::Show, MediaKind::Season, MediaKind::Episode],
        LibraryKind::Music => &[MediaKind::Album, MediaKind::Track, MediaKind::Artist],
    }
}

/// The rows a library lists at its top: movies, shows, albums. Shows
/// open into seasons and episodes, albums into tracks.
pub fn top(library: LibraryKind) -> MediaKind {
    kinds(library)[0]
}

/// The kinds a title search looks at when no kind is picked. Seasons are
/// not searched: their titles are "Season 1".
fn searched(library: LibraryKind) -> Vec<MediaKind> {
    match library {
        LibraryKind::Movies => vec![MediaKind::Movie],
        LibraryKind::Shows => vec![MediaKind::Show, MediaKind::Episode],
        LibraryKind::Music => vec![MediaKind::Album, MediaKind::Track],
    }
}

/// Whether `kind` has files of its own.
fn is_leaf(kind: MediaKind) -> bool {
    matches!(
        kind,
        MediaKind::Movie | MediaKind::Episode | MediaKind::Track
    )
}

/// A catalog failure as the admin API says it.
pub fn catalog_error(e: CatalogError) -> ErrorObjectOwned {
    match e {
        CatalogError::NotFound(what) | CatalogError::ForeignId(what) => error(
            error_code::INVALID_INPUT,
            format!("no {what} in the catalog"),
        ),
        CatalogError::NotServed(what) => error(
            error_code::INVALID_INPUT,
            format!("{what} is in a library this server does not serve"),
        ),
        CatalogError::NotSetUp => error(error_code::NOT_CONFIGURED, e.to_string()),
        other => error(error_code::UNAVAILABLE, other.to_string()),
    }
}

pub fn file_state(f: &FileCheck) -> FileState {
    FileState {
        plex_path: f.reported.display().to_string(),
        mapped_path: f.local.display().to_string(),
        mapping: f.mapping.as_ref().map(|m| PathMappingEntry {
            library: m.library.clone(),
            from: m.from.display().to_string(),
            to: m.to.display().to_string(),
        }),
        found: f.found(),
        problem: f.problem.clone(),
    }
}

/// `row` as the API lists it; a show's or album's missing files counted
/// from `check`, when there is one.
pub fn catalog_row(row: Row, check: Option<&LibraryCheck>) -> CatalogRow {
    let leaf = is_leaf(row.item.id().kind());
    let missing = match check {
        Some(check) if !leaf => Some(check.missing_under(row.item.id())),
        _ => None,
    };
    CatalogRow {
        matched: row.matched(),
        files: row.files.iter().map(file_state).collect(),
        item: row.item,
        guid: row.guid,
        agents: row.agents,
        added_at: row.added_at,
        updated_at: row.updated_at,
        children: row.children,
        leaves: row.leaves,
        missing,
    }
}

/// Whether `item`'s title (or its show's, album's) has `text` in it, in
/// any case: how "Missing files" is searched, as the check's list is.
fn titled(item: &Item, text: &str) -> bool {
    let text = text.to_lowercase();
    let has = |s: &str| s.to_lowercase().contains(&text);
    match item {
        Item::Movie(m) => has(&m.title),
        Item::Episode(e) => has(&e.title) || e.show_title.as_deref().is_some_and(has),
        Item::Track(t) => has(&t.title),
        other => has(&title_of(other)),
    }
}

pub fn title_of(item: &Item) -> String {
    match item {
        Item::Movie(v) => v.title.clone(),
        Item::Show(v) => v.title.clone(),
        Item::Season(v) => v.title.clone(),
        Item::Episode(v) => v.title.clone(),
        Item::Artist(v) => v.name.clone(),
        Item::Album(v) => v.title.clone(),
        Item::Track(v) => v.title.clone(),
    }
}

/// The page `missing` lists under `query`: the check's own list, of the
/// leaf kind asked for and with the text, in its order.
pub fn missing_rows(check: &LibraryCheck, query: &CatalogQuery) -> (Vec<CatalogRow>, u32) {
    let kind = query.kind.map(kind_of).filter(|k| is_leaf(*k));
    let text = query
        .text
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let listed: Vec<_> = check
        .missing
        .iter()
        .filter(|m| kind.is_none_or(|k| m.row.item.id().kind() == k))
        .filter(|m| text.is_none_or(|t| titled(&m.row.item, t)))
        .collect();
    let total = u32::try_from(listed.len()).unwrap_or(u32::MAX);
    let rows = listed
        .into_iter()
        .skip(usize::try_from(query.offset).unwrap_or(usize::MAX))
        .take(usize::try_from(query.limit).unwrap_or(usize::MAX))
        .map(|m| catalog_row(m.row.clone(), None))
        .collect();
    (rows, total)
}

/// The artwork clients get for `item`.
fn artwork_of(item: &Item) -> &[spoolframe_types::Artwork] {
    match item {
        Item::Movie(v) => &v.artwork,
        Item::Show(v) => &v.artwork,
        Item::Season(v) => &v.artwork,
        Item::Episode(v) => &v.artwork,
        Item::Artist(v) => &v.artwork,
        Item::Album(v) => &v.artwork,
        Item::Track(_) => &[],
    }
}

impl AdminApi {
    fn composite(&self) -> RpcResult<Arc<Composite>> {
        self.plex.composite().ok_or_else(|| {
            error(
                error_code::NOT_CONFIGURED,
                "this server is not set up: no Plex server is configured",
            )
        })
    }

    /// The library `id` names, as clients know it.
    async fn library(composite: &Composite, id: &str) -> RpcResult<(LibraryId, Library)> {
        let library: LibraryId = id
            .parse()
            .map_err(|_| error(error_code::INVALID_INPUT, format!("no library {id}")))?;
        let found = composite
            .libraries()
            .await
            .map_err(catalog_error)?
            .into_iter()
            .find(|l| l.id == library)
            .ok_or_else(|| error(error_code::INVALID_INPUT, format!("no library {id}")))?;
        Ok((library, found))
    }

    pub(super) async fn catalog_library_of(&self, id: String) -> RpcResult<CatalogLibrary> {
        let composite = self.composite()?;
        let (library, found) = Self::library(&composite, &id).await?;
        let (server, local) = ids::library_local(&library);
        let source = composite
            .server(server)
            .ok_or_else(|| error(error_code::INVALID_INPUT, format!("no library {id}")))?;
        let mut counts = Vec::new();
        for &kind in kinds(found.kind) {
            let count = composite
                .count(&library, kind)
                .await
                .map_err(catalog_error)?;
            counts.push(KindCount {
                kind: catalog_kind(kind),
                count,
            });
        }
        let filtered = |filter| Browse {
            kinds: vec![top(found.kind)],
            text: None,
            filter: Some(filter),
            page: Page {
                offset: 0,
                limit: 0,
            },
        };
        let unmatched = composite
            .browse(&library, &filtered(RowFilter::Unmatched))
            .await
            .map_err(catalog_error)?
            .total;
        let no_artwork = composite
            .browse(&library, &filtered(RowFilter::NoArtwork))
            .await
            .map_err(catalog_error)?
            .total;
        let plex_title = source
            .catalog
            .sections()
            .await
            .ok()
            .and_then(|s| s.into_iter().find(|s| s.key == local.key()))
            .map(|s| s.title);
        let paths = self.checks.start(&composite, &library, false);
        Ok(CatalogLibrary {
            id: library.to_string(),
            name: found.title,
            kind: found.kind,
            server: source.id.clone(),
            server_name: source.name.clone(),
            plex_title,
            counts,
            unmatched,
            no_artwork,
            paths,
        })
    }

    pub(super) async fn catalog_rows_of(&self, query: CatalogQuery) -> RpcResult<CatalogPage> {
        let composite = self.composite()?;
        let (library, found) = Self::library(&composite, &query.library).await?;
        if query.limit > MAX_ROWS {
            return Err(error(
                error_code::INVALID_INPUT,
                format!("at most {MAX_ROWS} rows a page"),
            ));
        }
        if let Some(kind) = query.kind
            && !kinds(found.kind).contains(&kind_of(kind))
        {
            return Err(error(
                error_code::INVALID_INPUT,
                format!("{} has no {kind:?} items", found.title),
            ));
        }
        let key = library.to_string();
        if query.filter == Some(CatalogFilter::Missing) {
            let paths = self.checks.start(&composite, &library, false);
            let (_, check) = self.checks.state(&composite, &key);
            let (rows, total) = check
                .map(|check| missing_rows(&check, &query))
                .unwrap_or_default();
            return Ok(CatalogPage { rows, total, paths });
        }
        let text = query
            .text
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned);
        let kinds = match (query.kind, &text) {
            (Some(kind), _) => vec![kind_of(kind)],
            (None, Some(_)) => searched(found.kind),
            (None, None) => vec![top(found.kind)],
        };
        let filter = match query.filter {
            Some(CatalogFilter::Unmatched) => Some(RowFilter::Unmatched),
            Some(CatalogFilter::NoArtwork) => Some(RowFilter::NoArtwork),
            Some(CatalogFilter::Missing) | None => None,
        };
        let page = composite
            .browse(
                &library,
                &Browse {
                    kinds,
                    text,
                    filter,
                    page: Page {
                        offset: query.offset,
                        limit: query.limit,
                    },
                },
            )
            .await
            .map_err(catalog_error)?;
        let (paths, check) = self.checks.state(&composite, &key);
        Ok(CatalogPage {
            rows: page
                .rows
                .into_iter()
                .map(|r| catalog_row(r, check.as_deref()))
                .collect(),
            total: page.total,
            paths,
        })
    }

    /// The last check of the library `library` names, if done.
    fn check_of(
        &self,
        composite: &Arc<Composite>,
        library: Option<&LibraryId>,
    ) -> (PathsChecked, Option<Arc<LibraryCheck>>) {
        match library {
            Some(library) => self.checks.state(composite, &library.to_string()),
            None => (PathsChecked::NotChecked, None),
        }
    }

    pub(super) async fn catalog_children_of(
        &self,
        library: String,
        id: String,
    ) -> RpcResult<Vec<CatalogRow>> {
        let composite = self.composite()?;
        let (library, _) = Self::library(&composite, &library).await?;
        let id = media_id(&id)?;
        let rows = composite.child_rows(&id).await.map_err(catalog_error)?;
        let (_, check) = self.check_of(&composite, Some(&library));
        Ok(rows
            .into_iter()
            .map(|r| catalog_row(r, check.as_deref()))
            .collect())
    }

    pub(super) async fn catalog_item_of(
        &self,
        user: crate::store::users::LocalUserId,
        id: String,
    ) -> RpcResult<CatalogItem> {
        let composite = self.composite()?;
        let id = media_id(&id)?;
        let seen = composite.inspect(&id).await.map_err(catalog_error)?;
        let (server, _) = ids::media_local(&id);
        let source = composite.server(server);
        let mut found = seen.found.clone();
        // What a client of the caller's would get: their own playback
        // state, as the client protocol fills it in.
        if let Some(watch) = &self.watch {
            let mut items: Vec<&mut Item> = std::iter::once(&mut found.item)
                .chain(found.children.iter_mut())
                .collect();
            watch
                .fill(Principal::LocalUser(user), &mut items)
                .await
                .map_err(super::internal)?;
        }
        let reply = spoolframe_protocol::Reply::Item {
            item: found.item,
            children: found.children,
            markers: found.markers,
        };
        let reply = serde_json::to_value(&reply).map_err(super::internal)?;
        let libraries = composite.libraries().await.unwrap_or_default();
        let library = seen.library.as_ref().and_then(|l| {
            libraries.iter().find(|x| x.id == *l).map(|x| CatalogCrumb {
                id: x.id.to_string(),
                title: x.title.clone(),
            })
        });
        let (paths, check) = self.check_of(&composite, seen.library.as_ref());
        let open_in_plex = composite.backend_link(&id).await.unwrap_or_else(|e| {
            tracing::debug!(%id, error = %e, "no Plex web app link");
            None
        });
        Ok(CatalogItem {
            library,
            ancestors: seen
                .ancestors
                .iter()
                .map(|(id, title)| CatalogCrumb {
                    id: id.to_string(),
                    title: title.clone(),
                })
                .collect(),
            server: server.to_owned(),
            server_name: source.map(|s| s.name.clone()).unwrap_or_default(),
            reply,
            matched: seen.matched(),
            guid: seen.guid.clone(),
            external_ids: seen.found.external_ids.clone(),
            added_at: seen.added_at,
            updated_at: seen.updated_at,
            folders: seen
                .folders
                .iter()
                .map(|f| f.display().to_string())
                .collect(),
            versions: seen
                .versions
                .iter()
                .map(|v| MediaVersion {
                    id: v.id,
                    container: v.container.clone(),
                    video_codec: v.video_codec.clone(),
                    video_resolution: v.video_resolution.clone(),
                    video_profile: v.video_profile.clone(),
                    audio_codec: v.audio_codec.clone(),
                    audio_channels: v.audio_channels,
                    bitrate: v.bitrate,
                    duration_ms: v.duration_ms,
                    parts: v
                        .parts
                        .iter()
                        .map(|p| MediaPart {
                            id: p.id,
                            size: p.size,
                            duration_ms: p.duration_ms,
                            file: file_state(&p.file),
                        })
                        .collect(),
                })
                .collect(),
            children: seen
                .children
                .into_iter()
                .map(|r| catalog_row(r, check.as_deref()))
                .collect(),
            open_in_plex,
            paths,
        })
    }

    pub(super) async fn catalog_artwork_of(&self, id: String) -> RpcResult<Vec<ArtworkInfo>> {
        let composite = self.composite()?;
        let id = media_id(&id)?;
        let found = composite.item(&id).await.map_err(catalog_error)?;
        let mut infos = Vec::new();
        for art in artwork_of(&found.item) {
            let mut info = ArtworkInfo {
                id: art.id.to_string(),
                kind: art.kind,
                width: None,
                height: None,
                bytes: None,
                error: None,
            };
            match composite.artwork(&art.id).await {
                Ok(image) => {
                    info.bytes = u64::try_from(image.bytes.len()).ok();
                    let size = image::ImageReader::new(Cursor::new(&image.bytes))
                        .with_guessed_format()
                        .ok()
                        .and_then(|r| r.into_dimensions().ok());
                    if let Some((width, height)) = size {
                        info.width = Some(width);
                        info.height = Some(height);
                    }
                }
                Err(e) => info.error = Some(e.to_string()),
            }
            infos.push(info);
        }
        Ok(infos)
    }

    pub(super) async fn catalog_check_paths_of(&self, id: String) -> RpcResult<PathsChecked> {
        let composite = self.composite()?;
        let (library, _) = Self::library(&composite, &id).await?;
        Ok(self.checks.start(&composite, &library, true))
    }
}

/// An image a catalog page shows (P3-16): the artwork `id` at `size`.
pub struct PageImage {
    pub bytes: Vec<u8>,
    pub content_type: String,
    pub etag: Option<String>,
}

impl AdminApi {
    /// Artwork `id` at `size`, for an admin's page: resized through the
    /// artwork cache as clients get it, or the catalog's own image where
    /// the server has no cache.
    pub(crate) async fn catalog_image(
        &self,
        ext: &jsonrpsee::Extensions,
        id: &str,
        size: spoolframe_protocol::ArtworkSize,
    ) -> RpcResult<PageImage> {
        self.admin(ext).await?;
        let id: spoolframe_types::ArtworkId = id
            .parse()
            .map_err(|_| error(error_code::INVALID_INPUT, format!("no artwork {id}")))?;
        match &self.artwork {
            Some(cache) => {
                let art = cache.get(&id, size).await.map_err(|e| match e {
                    crate::artwork::ArtworkError::Catalog(e) => catalog_error(e),
                    other => error(error_code::INVALID_INPUT, other.to_string()),
                })?;
                Ok(PageImage {
                    bytes: art.bytes,
                    content_type: art.content_type,
                    etag: Some(art.etag),
                })
            }
            None => {
                let image = self
                    .composite()?
                    .artwork(&id)
                    .await
                    .map_err(catalog_error)?;
                Ok(PageImage {
                    bytes: image.bytes,
                    content_type: image.content_type,
                    etag: None,
                })
            }
        }
    }
}

pub(super) fn media_id(id: &str) -> RpcResult<MediaId> {
    id.parse()
        .map_err(|_| error(error_code::INVALID_INPUT, format!("no item {id}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_library_lists_its_top_kind_first() {
        assert_eq!(top(LibraryKind::Movies), MediaKind::Movie);
        assert_eq!(top(LibraryKind::Shows), MediaKind::Show);
        assert_eq!(top(LibraryKind::Music), MediaKind::Album);
    }
}
