//! Playlists (PX-40): ordered lists of movies, episodes and tracks a user
//! makes, shares, pins and plays.
//!
//! Every call takes an [`Actor`] and is judged by its rights alone, so a
//! client's session and, later, an MCP tool acting for a user (PX-42) get
//! the same answers. Who may do what is `spoolframe_policy::playlists`;
//! rows are `crate::store::playlists`; items are the catalog's.
//!
//! An entry keeps its item's key and stable ids. When the catalog no
//! longer has the key, the entry follows the item to its new key if the
//! catalog finds one by those ids (a Plex rebuild), else drops out: the
//! item left the library. An item in a library not served now is hidden,
//! never dropped, as watch state is kept (P3-11). A catalog that cannot
//! be asked drops nothing.
//!
//! Every user has two built-in music playlists (PX-43), made the first
//! time they are needed: Liked, the tracks they liked, kept as any
//! playlist's entries are; and All, every track they may see, whose
//! entries are never kept but worked out from the catalog's tracks, as
//! listed within the last minute, so a newly scanned album is in it
//! within that and no edit is made (`ALL_KEPT_FOR`). All has a row all
//! the same, for the user's place, settings and pin. Neither is renamed,
//! shared or deleted, and All is not edited: `spoolframe_policy`'s
//! `builtin_*`. They are listed only while a music library is served, and
//! only to callers that take them (a client listing `spoolframe:liked`).

use std::sync::Arc;

use futures_util::{StreamExt, TryStreamExt, stream};
use spoolframe_catalog::{Catalog, CatalogError, ItemWithChildren, Reach};
use spoolframe_policy::playlists::{self as policy, Access, Standing};
use std::collections::{HashMap, HashSet};

use spoolframe_protocol::playlists::{Builtin, EntryId, Liked, PlaySettings, PlaylistNext, Repeat};
use spoolframe_protocol::playlists::{
    Kinds, PinNote, PinNoteReason, PlaylistEdit, PlaylistEntry, PlaylistOwnerKind, PlaylistPage,
    PlaylistRef, PlaylistShare, PlaylistSummary, ShareCandidatePage, Visibility,
};
use spoolframe_types::{
    ArtworkId, ArtworkKind, Item, LibraryKind, Marker, MediaId, MediaKind, Namespace,
};

use crate::covers::{CoverError, Covers, Tile};
use crate::principal::{Actor, Principal};
use crate::store::playlists::PlaceRow;
use crate::store::playlists::{
    EntryEdit, EntryRow, HeldBy, NewEntry, NewPlaylist, PinNoteReason as StoredReason,
    PlaylistChange, PlaylistError, PlaylistRow, ShareRow,
};
use crate::store::{Store, StoreError};
use crate::watch::Watch;
use people::Names;

/// The longest name a playlist takes, in characters.
pub const MAX_NAME: usize = 100;
/// The most users one playlist is shared with.
pub const MAX_SHARES: usize = 100;
/// The most edits one `EditPlaylist` carries.
pub const MAX_EDITS: usize = 100;
/// The most entries one page of a playlist lists.
pub const MAX_PAGE: u32 = 200;
/// The most entries one `UpcomingInPlaylist` lists.
pub const MAX_UPCOMING: u32 = 100;
/// The most catalog lookups one call has in flight at once, as Home's.
pub(crate) const CATALOG_FETCHES: usize = 8;
/// The kinds Liked and All hold.
const TRACKS: Kinds = Kinds {
    movies: false,
    episodes: false,
    tracks: true,
};

#[derive(Debug, thiserror::Error)]
pub enum PlaylistsError {
    /// No such playlist, or one the actor does not see: the same answer.
    #[error("no such playlist")]
    NotFound,
    /// No such item, to add.
    #[error("{0} not found")]
    NoItem(MediaId),
    /// The actor sees it, and may not make this change: an entry edit on
    /// a read-only share, or a rename by an editor.
    #[error("you cannot change this playlist")]
    ReadOnly,
    /// Only an admin makes a playlist server-wide.
    #[error("only an admin can do that")]
    AdminRequired,
    /// The playlist's kinds do not take an item of this kind.
    #[error("this playlist does not take a {0:?}")]
    KindNotAllowed(MediaKind),
    /// Only a track or an album is liked (PX-43).
    #[error("only a track or an album can be liked, not a {0:?}")]
    NotLikeable(MediaKind),
    #[error("{0}")]
    InvalidInput(String),
    /// The edit was made on a revision someone else's change has
    /// overtaken in a way it cannot be redone over; `revision` is the
    /// playlist's now.
    #[error("someone changed this playlist; reload")]
    Conflict { revision: u64 },
    /// Not the actor's to do (PX-53): a cover set or taken away by
    /// someone who may not edit the playlist, or on Liked or All.
    #[error("you are not allowed to do that")]
    NotAllowed,
    /// A cover over the size limits, refused before it was decoded.
    #[error("the picture is too large")]
    ImageTooLarge,
    /// A cover that is not a picture taken, or did not decode.
    #[error("the picture cannot be read: {0}")]
    ImageUnreadable(String),
    /// Covers cannot be kept here now: none set up, or ffmpeg did not run.
    #[error("{0}")]
    Unavailable(String),
    /// A cover could not be written or read.
    #[error("covers: {0}")]
    Io(std::io::Error),
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<CoverError> for PlaylistsError {
    fn from(e: CoverError) -> Self {
        match e {
            CoverError::TooLarge(_) => PlaylistsError::ImageTooLarge,
            CoverError::Unreadable(e) => PlaylistsError::ImageUnreadable(e.to_string()),
            CoverError::Tool(e) => {
                tracing::error!(error = %e, "a playlist cover could not be decoded: ffmpeg did not run");
                PlaylistsError::Unavailable("covers cannot be read on this server now".into())
            }
            CoverError::Io(e) => PlaylistsError::Io(e),
        }
    }
}

impl From<PlaylistError> for PlaylistsError {
    fn from(e: PlaylistError) -> Self {
        match e {
            PlaylistError::NotFound => PlaylistsError::NotFound,
            // The caller that can say the revision says it.
            PlaylistError::Conflict => PlaylistsError::Conflict { revision: 0 },
            PlaylistError::Store(e) => PlaylistsError::Store(e),
        }
    }
}

impl From<crate::watch::WatchError> for PlaylistsError {
    fn from(e: crate::watch::WatchError) -> Self {
        match e {
            crate::watch::WatchError::Catalog(e) => PlaylistsError::Catalog(e),
            crate::watch::WatchError::Store(e) => PlaylistsError::Store(e),
            crate::watch::WatchError::NotTracked(id) => PlaylistsError::NoItem(id),
        }
    }
}

/// Whether a user may see an item. Today every item the catalog serves is
/// every user's to see; per-user rules (ratings, libraries) come later,
/// and a playlist then lists and counts only what its viewer may see.
pub trait ItemVisibility: Send + Sync + std::fmt::Debug {
    fn may_see(&self, actor: &Actor, item: &Item) -> bool;
}

/// Every item served is seen by everyone.
#[derive(Debug, Default)]
pub struct AllServed;

impl ItemVisibility for AllServed {
    fn may_see(&self, _: &Actor, _: &Item) -> bool {
        true
    }
}

/// Who sees a playlist, for telling them it changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    owner: String,
    server_wide: bool,
    shares: Vec<String>,
    /// Liked or All (PX-43): told only to who takes built-ins.
    pub builtin: bool,
}

impl Seen {
    fn of(row: &PlaylistRow) -> Self {
        Self {
            owner: row.owner.clone(),
            server_wide: row.server_wide,
            shares: row.shares.iter().map(|s| s.principal.clone()).collect(),
            builtin: row.builtin.is_some(),
        }
    }

    /// Seen by `owner`, everyone when `server_wide`, and `shares`.
    #[cfg(test)]
    pub(crate) fn for_tests(owner: Principal, server_wide: bool, shares: &[Principal]) -> Self {
        Self {
            owner: owner.to_string(),
            server_wide,
            shares: shares.iter().map(ToString::to_string).collect(),
            builtin: false,
        }
    }

    /// Whether `principal` sees it.
    pub fn sees(&self, principal: Principal) -> bool {
        let principal = principal.to_string();
        self.server_wide || self.owner == principal || self.shares.contains(&principal)
    }
}

/// What changed, for the connections it concerns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaylistNews {
    /// Playlist `id` changed (made, edited, renamed, shared) or went.
    /// Whoever sees it `after` is told it changed; whoever saw it
    /// `before` and does not now, that it is gone to them.
    Playlist {
        id: String,
        revision: u64,
        before: Option<Seen>,
        after: Option<Seen>,
    },
    /// `principal`'s pins or pin notes changed.
    Pins { principal: Principal },
    /// `principal` liked or unliked tracks (PX-51): for their devices
    /// that downloaded Liked.
    Liked { principal: Principal },
}

/// How many pieces of news a listener may fall behind by.
const NEWS: usize = 256;

/// What [`Playlists::update`] changes; `None` leaves it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaylistUpdate {
    pub name: Option<String>,
    pub kinds: Option<Kinds>,
    pub visibility: Option<Visibility>,
}

/// How long a playlist's mosaic, as worked out for one viewer at one
/// revision, is kept: the catalog's covers change too (an update gives a
/// poster a new id), and nothing else tells the playlist.
const MOSAIC_FOR: std::time::Duration = std::time::Duration::from_secs(600);

/// The most mosaics kept; then all are forgotten at once.
const MOST_MOSAICS: usize = 4096;

/// The most entries' items looked up for one mosaic: a playlist of one
/// album's tracks has a single cover however long it is.
const MOSAIC_LOOKUPS: usize = 16;

/// Each playlist's picture as last worked out, by playlist, revision and
/// viewer.
type Mosaics = parking_lot::Mutex<
    std::collections::HashMap<(String, u64, String), (std::time::Instant, ArtworkId)>,
>;

/// How long All's tracks (PX-43), as listed from the music libraries,
/// are used before they are listed again: a newly scanned album is in All
/// within this, as a new episode is on Home within the catalog's own
/// [`KEPT_FOR`](crate::catalog::replies::KEPT_FOR). Listing every track of
/// a large library takes the Plex servers seconds, too long to do at each
/// page read and play. A replaced catalog lists them again at once.
const ALL_KEPT_FOR: std::time::Duration = crate::catalog::replies::KEPT_FOR;

/// Every track of the music libraries served, with no duplicates, in
/// All's order, as last listed: one list for every user, each seeing of
/// it what [`ItemVisibility`] lets them.
struct AllTracks {
    /// Bumped as the catalog is replaced; without, only [`ALL_KEPT_FOR`]
    /// ends a list.
    changes: Option<tokio::sync::watch::Receiver<u64>>,
    /// Held while the libraries are listed, so reads at once list them
    /// once.
    kept: tokio::sync::Mutex<Option<KeptTracks>>,
}

/// All's tracks, when they were listed, and under which catalog.
struct KeptTracks {
    // Tokio's clock, so a test moves it on rather than sleeping.
    at: tokio::time::Instant,
    generation: u64,
    tracks: Arc<[Item]>,
}

impl AllTracks {
    fn new() -> Self {
        Self {
            changes: None,
            kept: tokio::sync::Mutex::new(None),
        }
    }

    /// The catalog's generation now: 0 without a signal.
    fn generation(&self) -> u64 {
        self.changes.as_ref().map_or(0, |c| *c.borrow())
    }
}

/// An entry's item, as the catalog has it now.
enum Resolved {
    /// There, readable or not.
    Here {
        item: Item,
        markers: Vec<Marker>,
        unavailable: bool,
    },
    /// There under a new key.
    Moved {
        item: Item,
        markers: Vec<Marker>,
        unavailable: bool,
        to: MediaId,
    },
    /// In a library not served now: not shown, kept.
    Hidden,
    /// Gone from the library: dropped.
    Gone,
}

pub struct Playlists {
    store: Store,
    catalog: Arc<dyn Catalog>,
    watch: Arc<Watch>,
    visibility: Arc<dyn ItemVisibility>,
    news: tokio::sync::broadcast::Sender<PlaylistNews>,
    /// Uploaded covers (PX-53); without, uploads are `Unavailable`.
    covers: Option<Arc<Covers>>,
    mosaics: Mosaics,
    /// All's tracks, kept a while.
    all: AllTracks,
    /// Where shuffling draws from: the system's randomness, or a test's
    /// named seeds.
    seeds: Seeds,
}

/// A source of shuffle seeds.
type Seeds = Arc<dyn Fn() -> u64 + Send + Sync>;

impl std::fmt::Debug for Playlists {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Playlists").finish_non_exhaustive()
    }
}

impl Playlists {
    pub fn new(store: Store, catalog: Arc<dyn Catalog>, watch: Arc<Watch>) -> Self {
        Self::with_visibility(store, catalog, watch, Arc::new(AllServed))
    }

    /// Judge which items a user may see by `visibility`.
    pub fn with_visibility(
        store: Store,
        catalog: Arc<dyn Catalog>,
        watch: Arc<Watch>,
        visibility: Arc<dyn ItemVisibility>,
    ) -> Self {
        Self {
            store,
            catalog,
            watch,
            visibility,
            news: tokio::sync::broadcast::Sender::new(NEWS),
            covers: None,
            mosaics: Mosaics::default(),
            all: AllTracks::new(),
            seeds: Arc::new(|| {
                use password_hash::rand_core::{OsRng, RngCore};
                OsRng.next_u64()
            }),
        }
    }

    /// Draw shuffle seeds from `seeds`, so a test knows its rounds.
    #[cfg(test)]
    pub(crate) fn with_seeds(mut self, seeds: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.seeds = Arc::new(seeds);
        self
    }

    /// A new shuffle seed.
    fn seed(&self) -> u64 {
        (self.seeds)()
    }

    /// List All's tracks again each time `changes` does: the catalog was
    /// replaced (`LiveCatalog::changes`).
    pub fn with_catalog_changes(mut self, changes: tokio::sync::watch::Receiver<u64>) -> Self {
        self.all.changes = Some(changes);
        self
    }

    /// Keep uploaded covers in `covers` (PX-53).
    pub fn with_covers(mut self, covers: Arc<Covers>) -> Self {
        self.covers = Some(covers);
        self
    }

    /// Whether `actor` may see `item`: a paired server's only from a
    /// client that named the experimental `federation` feature (P8-03),
    /// and anything only as [`ItemVisibility`] says.
    fn may_see(&self, actor: &Actor, item: &Item) -> bool {
        (actor.federated || item.id().namespace() != Namespace::Federated)
            && self.visibility.may_see(actor, item)
    }

    /// Every change, as it is made.
    pub fn news(&self) -> tokio::sync::broadcast::Receiver<PlaylistNews> {
        self.news.subscribe()
    }

    fn tell(&self, news: PlaylistNews) {
        // Nobody listening is fine.
        let _ = self.news.send(news);
    }

    fn told_playlist(&self, id: &str, revision: u64, before: Option<Seen>, after: Option<Seen>) {
        self.tell(PlaylistNews::Playlist {
            id: id.to_owned(),
            revision,
            before,
            after,
        });
    }

    /// Note that `principal` liked (or unliked) tracks `media_ids` now
    /// (PX-51), and tell their devices: every like and unlike comes here.
    async fn noted_liked(
        &self,
        principal: Principal,
        media_ids: &[String],
        liked: bool,
    ) -> Result<(), PlaylistsError> {
        self.store
            .note_liked(&principal.to_string(), media_ids, liked, self.store.now())
            .await?;
        self.tell(PlaylistNews::Liked { principal });
        Ok(())
    }

    /// What `actor` may do with `row`.
    fn access(actor: &Actor, row: &PlaylistRow) -> Access {
        let principal = actor.principal.to_string();
        let access = policy::access(Standing {
            owner: row.owner == principal,
            admin: actor.admin,
            server_wide: row.server_wide,
            share: row.share_of(&principal),
        });
        match row.builtin {
            Some(builtin) => policy::builtin_access(builtin, access),
            None => access,
        }
    }

    /// Playlist `id`, if `actor` sees it, with what they may do.
    async fn seen(&self, actor: &Actor, id: &str) -> Result<(PlaylistRow, Access), PlaylistsError> {
        let row = self
            .store
            .playlist(id)
            .await?
            .ok_or(PlaylistsError::NotFound)?;
        let access = Self::access(actor, &row);
        if !access.can_read() {
            return Err(PlaylistsError::NotFound);
        }
        Ok((row, access))
    }

    /// `row` as `actor` sees it, its owner and shares named by `names`.
    fn summary(
        actor: &Actor,
        row: &PlaylistRow,
        pinned: Option<u32>,
        names: &Names,
    ) -> PlaylistSummary {
        let access = Self::access(actor, row);
        let principal = actor.principal.to_string();
        let owner_kind = if row.server_wide {
            PlaylistOwnerKind::ServerWide
        } else if row.owner == principal {
            PlaylistOwnerKind::Mine
        } else {
            PlaylistOwnerKind::SharedWithMe
        };
        let visibility = if row.server_wide {
            Visibility::ServerWide
        } else if row.shares.is_empty() {
            Visibility::Personal
        } else {
            Visibility::Shared {
                // Who else it is shared with is the manager's to know.
                with: if access.can_manage() {
                    row.shares
                        .iter()
                        .map(|s| PlaylistShare {
                            display_name: names.of_principal(&s.principal),
                            kind: s.principal.parse().ok().map(people::kind_of),
                            ..PlaylistShare::new(s.principal.clone(), s.can_edit)
                        })
                        .collect()
                } else {
                    Vec::new()
                },
            }
        };
        PlaylistSummary {
            id: row.id.clone(),
            name: row.name.clone(),
            kinds: row.kinds,
            owner_kind,
            visibility,
            can_edit: access.can_edit(),
            can_manage: access.can_manage(),
            revision: row.revision,
            pinned,
            builtin: row.builtin,
            owner_name: names.of_principal(&row.owner),
            // Filled by `summarized`, which may ask the catalog.
            artwork: None,
            custom_art: row.art.is_some(),
        }
    }

    /// [`Self::summary`], with its picture.
    async fn summarized(
        &self,
        actor: &Actor,
        row: &PlaylistRow,
        pinned: Option<u32>,
        names: &Names,
    ) -> Result<PlaylistSummary, PlaylistsError> {
        let mut summary = Self::summary(actor, row, pinned, names);
        summary.artwork = Some(self.art_of(actor, row).await?);
        Ok(summary)
    }

    /// Where `id` is in `actor`'s Pinned.
    async fn pinned(&self, actor: &Actor, id: &str) -> Result<Option<u32>, StoreError> {
        let pins = self.store.pins(&actor.principal.to_string()).await?;
        Ok(pins.iter().position(|p| p == id).map(|n| n as u32))
    }

    /// Every playlist `actor` sees, and the notes of pinned ones that left
    /// their Pinned.
    /// With `builtins`, Liked and All first, made now if they are not
    /// yet, while a music library is served; without, they are left out,
    /// for a client that would not know them for what they are.
    pub async fn list(
        &self,
        actor: &Actor,
        builtins: bool,
    ) -> Result<(Vec<PlaylistSummary>, Vec<PinNote>), PlaylistsError> {
        let principal = actor.principal.to_string();
        let builtins = builtins && self.serves_music().await;
        if builtins {
            for builtin in [Builtin::Liked, Builtin::All] {
                self.store.builtin_playlist(&principal, builtin).await?;
            }
        }
        let mut rows = self.store.playlists_seen_by(&principal).await?;
        if !builtins {
            rows.retain(|row| row.builtin.is_none());
        }
        // Liked, then All, then the rest as they were: first among the
        // music playlists. Stable.
        rows.sort_by_key(|row| match row.builtin {
            Some(Builtin::Liked) => 0,
            Some(Builtin::All) => 1,
            Some(Builtin::Unknown) => 2,
            None => 3,
        });
        let pins = self.store.pins(&principal).await?;
        let names = Names::of(&self.store).await?;
        // Each may ask the catalog for its mosaic's covers: a few at once.
        let summarizing: Vec<_> = rows
            .iter()
            .map(|row| {
                let pinned = pins.iter().position(|p| *p == row.id).map(|n| n as u32);
                self.summarized(actor, row, pinned, &names)
            })
            .collect();
        let summaries = stream::iter(summarizing)
            .buffered(CATALOG_FETCHES)
            .try_collect()
            .await?;
        let notes = self
            .store
            .pin_notes(&principal)
            .await?
            .into_iter()
            .map(|n| PinNote {
                playlist_id: n.playlist_id,
                name: n.name,
                reason: match n.reason {
                    StoredReason::Unshared => PinNoteReason::Unshared,
                    StoredReason::Deleted => PinNoteReason::Deleted,
                },
            })
            .collect();
        Ok((summaries, notes))
    }

    /// One playlist and a page of its entries, from `offset`, at most
    /// `limit` ([`MAX_PAGE`]).
    pub async fn get(
        &self,
        actor: &Actor,
        id: &str,
        offset: u32,
        limit: u32,
    ) -> Result<PlaylistPage, PlaylistsError> {
        let (row, _) = self.seen(actor, id).await?;
        let principal = actor.principal.to_string();
        let (mut page, start, total, playable) = if row.builtin == Some(Builtin::All) {
            let all = self.all_tracks().await?;
            let tracks = self.seen_tracks(actor, &all);
            let (start, end) = cut(tracks.len(), offset, limit);
            // Its files are looked at as they are listed and played, not
            // the whole library's at every read; a few at once, as a
            // playlist's own entries are.
            let shown: Vec<Item> = tracks[start..end].iter().map(|&t| t.clone()).collect();
            let page: Vec<PlaylistEntry> = stream::iter(shown)
                .map(|item| async move {
                    Ok::<_, PlaylistsError>(PlaylistEntry {
                        entry: policy::all_entry(&item.id().to_string()),
                        unavailable: self.unavailable(item.id()).await?,
                        item,
                        liked: false,
                    })
                })
                .buffered(CATALOG_FETCHES)
                .try_collect()
                .await?;
            let total = tracks.len() as u32;
            (page, start, total, total)
        } else {
            let mut entries = self.entries(actor, &row).await?;
            let total = entries.len() as u32;
            let playable = entries.iter().filter(|e| !e.unavailable).count() as u32;
            let (start, end) = cut(entries.len(), offset, limit);
            (entries.drain(start..end).collect(), start, total, playable)
        };
        let liked = self.store.liked_media(&principal).await?;
        for entry in &mut page {
            entry.liked = matches!(entry.item, Item::Track(_))
                && liked.contains(&entry.item.id().to_string());
        }
        let mut items: Vec<&mut Item> = page.iter_mut().map(|e| &mut e.item).collect();
        self.watch.fill(actor.principal, &mut items).await?;
        // Its revision may have moved while entries were dropped or
        // re-linked.
        let row = self
            .store
            .playlist(id)
            .await?
            .ok_or(PlaylistsError::NotFound)?;
        let place = self
            .store
            .playlist_place(&principal, id)
            .await?
            .unwrap_or_default();
        Ok(PlaylistPage {
            playlist: self
                .summarized(
                    actor,
                    &row,
                    self.pinned(actor, id).await?,
                    &Names::of(&self.store).await?,
                )
                .await?,
            entries: page,
            offset: start as u32,
            total,
            playable,
            place: place.entry,
            settings: place.settings,
        })
    }

    /// Whether a music library is served: Liked and All are listed only
    /// then. A catalog that cannot be asked now hides nothing; one not
    /// set up has none.
    async fn serves_music(&self) -> bool {
        match self.catalog.libraries().await {
            Ok(libraries) => libraries.iter().any(|l| l.kind == LibraryKind::Music),
            Err(CatalogError::NotSetUp) => false,
            Err(e) => {
                tracing::debug!(error = %e, "cannot list the libraries; listing Liked and All");
                true
            }
        }
    }

    /// The tracks of `all` that `actor` may see, in its order: All's
    /// entries (PX-43).
    fn seen_tracks<'a>(&self, actor: &Actor, all: &'a [Item]) -> Vec<&'a Item> {
        all.iter().filter(|t| self.may_see(actor, t)).collect()
    }

    /// Every track in the music libraries served, each once, by artist,
    /// album, disc and track: as listed within [`ALL_KEPT_FOR`] under the
    /// catalog now, else listed now.
    async fn all_tracks(&self) -> Result<Arc<[Item]>, PlaylistsError> {
        let mut kept = self.all.kept.lock().await;
        // Read before listing: a catalog replaced while listing makes
        // the list stale at once.
        let generation = self.all.generation();
        if let Some(k) = kept.as_ref()
            && k.generation == generation
            && k.at.elapsed() < ALL_KEPT_FOR
        {
            return Ok(k.tracks.clone());
        }
        let at = tokio::time::Instant::now();
        let tracks: Arc<[Item]> = self.list_all_tracks().await?.into();
        *kept = Some(KeptTracks {
            at,
            generation,
            tracks: tracks.clone(),
        });
        Ok(tracks)
    }

    /// [`Self::all_tracks`], listed from the catalog now: the libraries a
    /// few at once.
    async fn list_all_tracks(&self) -> Result<Vec<Item>, PlaylistsError> {
        let music: Vec<_> = self
            .catalog
            .libraries()
            .await?
            .into_iter()
            .filter(|library| library.kind == LibraryKind::Music)
            .map(|library| library.id)
            .collect();
        let listed: Vec<Vec<Item>> = stream::iter(music)
            .map(|library| async move {
                match self.catalog.tracks(&library).await {
                    Ok(found) => Ok(found),
                    // Taken out of the config since it was listed.
                    Err(CatalogError::NotServed(_)) => Ok(Vec::new()),
                    Err(e) => Err(PlaylistsError::from(e)),
                }
            })
            // In the libraries' order, so the first of an item listed twice
            // is kept, as before.
            .buffered(CATALOG_FETCHES)
            .try_collect()
            .await?;
        let mut tracks: Vec<Item> = listed.into_iter().flatten().collect();
        let mut seen = HashSet::new();
        tracks.retain(|t| matches!(t, Item::Track(_)) && seen.insert(t.id().clone()));
        // Stable: who may see what is filtered from this order after.
        tracks.sort_by_cached_key(|t| match t {
            Item::Track(t) => (
                t.artist_name.as_deref().unwrap_or_default().to_lowercase(),
                t.album_title.as_deref().unwrap_or_default().to_lowercase(),
                t.album_id.to_string(),
                t.disc.unwrap_or(1),
                t.track.unwrap_or(u32::MAX),
                t.title.to_lowercase(),
            ),
            _ => Default::default(),
        });
        Ok(tracks)
    }

    /// The entries `row` plays, in order: its own, or All's worked out
    /// now, each named by [`policy::all_entry`].
    async fn order_of(
        &self,
        actor: &Actor,
        row: &PlaylistRow,
    ) -> Result<Vec<EntryRow>, PlaylistsError> {
        if row.builtin != Some(Builtin::All) {
            return Ok(self.store.playlist_entries(&row.id).await?);
        }
        let all = self.all_tracks().await?;
        Ok(self
            .seen_tracks(actor, &all)
            .into_iter()
            .map(|track| {
                let entry = new_entry(track, Vec::new());
                EntryRow {
                    entry_id: policy::all_entry(&entry.media_id),
                    media_id: entry.media_id,
                    kind: entry.kind,
                    show_id: entry.show_id,
                    album_id: entry.album_id,
                    artist_id: entry.artist_id,
                }
            })
            .collect())
    }

    /// Every entry of `row` that `actor` may see, in order, each with its
    /// item now. Entries whose item left the library are dropped, and
    /// those whose item has a new key follow it, as they are found.
    async fn entries(
        &self,
        actor: &Actor,
        row: &PlaylistRow,
    ) -> Result<Vec<PlaylistEntry>, PlaylistsError> {
        let stored = self.store.playlist_entries(&row.id).await?;
        let resolved: Vec<(EntryRow, Resolved)> = stream::iter(stored)
            .map(|entry| async move {
                let resolved = self.resolve(&entry).await?;
                Ok::<_, PlaylistsError>((entry, resolved))
            })
            .buffered(CATALOG_FETCHES)
            .try_collect()
            .await?;
        let mut shown = Vec::new();
        let mut gone = Vec::new();
        for (entry, resolved) in resolved {
            let (item, unavailable) = match resolved {
                Resolved::Here {
                    item, unavailable, ..
                } => (item, unavailable),
                Resolved::Moved {
                    item,
                    unavailable,
                    to,
                    ..
                } => {
                    let revision = self
                        .store
                        .relink_playlist_entry(&row.id, entry.entry_id, &to.to_string())
                        .await?;
                    self.told_playlist(&row.id, revision, Some(Seen::of(row)), Some(Seen::of(row)));
                    (item, unavailable)
                }
                Resolved::Hidden => continue,
                Resolved::Gone => {
                    gone.push(entry.entry_id);
                    continue;
                }
            };
            if self.may_see(actor, &item) {
                shown.push(PlaylistEntry {
                    entry: entry.entry_id,
                    item,
                    unavailable,
                    // Filled by the caller, for the page only.
                    liked: false,
                });
            }
        }
        if !gone.is_empty() {
            tracing::info!(playlist = %row.id, entries = gone.len(), "items left the library; dropped from the playlist");
            if let Some(revision) = self.store.drop_playlist_entries(&row.id, &gone).await? {
                self.told_playlist(&row.id, revision, Some(Seen::of(row)), Some(Seen::of(row)));
            }
        }
        Ok(shown)
    }

    /// What the catalog has for `entry` now.
    async fn resolve(&self, entry: &EntryRow) -> Result<Resolved, PlaylistsError> {
        let Ok(id) = entry.media_id.parse::<MediaId>() else {
            return Ok(Resolved::Gone);
        };
        match self.catalog.item_kept(&id).await {
            // A paired server's item whose pair or library went stays,
            // as no longer available (P8-03); one whose server is offline
            // does not play now.
            Ok((found, reach @ (Reach::Gone | Reach::Offline))) => Ok(Resolved::Here {
                unavailable: true,
                item: found.item,
                markers: if reach == Reach::Gone {
                    Vec::new()
                } else {
                    found.markers
                },
            }),
            Ok((found, Reach::Here)) => Ok(Resolved::Here {
                unavailable: self.unavailable(&id).await?,
                item: found.item,
                markers: found.markers,
            }),
            // All's entries are worked out from the catalog and keep no
            // stable ids to follow.
            Err(CatalogError::NotFound(_) | CatalogError::ForeignId(_))
                if entry.entry_id >= policy::ALL_ENTRIES =>
            {
                Ok(Resolved::Gone)
            }
            Err(CatalogError::NotFound(_) | CatalogError::ForeignId(_)) => {
                let ids = self.store.playlist_entry_ids(entry.entry_id).await?;
                let Some(to) = self.catalog.find_external(id.kind(), &ids).await? else {
                    return Ok(Resolved::Gone);
                };
                match self.catalog.item(&to).await {
                    Ok(found) => Ok(Resolved::Moved {
                        unavailable: self.unavailable(&to).await?,
                        item: found.item,
                        markers: found.markers,
                        to,
                    }),
                    Err(CatalogError::NotFound(_)) => Ok(Resolved::Gone),
                    Err(CatalogError::NotServed(_)) => Ok(Resolved::Hidden),
                    Err(e) => Err(e.into()),
                }
            }
            Err(CatalogError::NotServed(_)) => Ok(Resolved::Hidden),
            Err(e) => Err(e.into()),
        }
    }

    /// Whether `id`'s file cannot be read now; a paired server's item
    /// (P8-03), whether it does not play now: it plays from there.
    async fn unavailable(&self, id: &MediaId) -> Result<bool, PlaylistsError> {
        if id.namespace() == Namespace::Federated {
            return Ok(self.catalog.item_kept(id).await?.1 != Reach::Here);
        }
        match self.catalog.media_path(id).await {
            Ok(_) => Ok(false),
            Err(
                CatalogError::MediaFileUnavailable(_)
                | CatalogError::NotFound(_)
                | CatalogError::NotPlayable(_),
            ) => Ok(true),
            Err(e) => Err(e.into()),
        }
    }

    /// Make a playlist owned by `actor`.
    pub async fn create(
        &self,
        actor: &Actor,
        name: &str,
        kinds: Kinds,
        visibility: Visibility,
    ) -> Result<PlaylistSummary, PlaylistsError> {
        let name = checked_name(name)?;
        if kinds.is_empty() {
            return Err(PlaylistsError::InvalidInput(
                "a playlist takes at least one kind of item".into(),
            ));
        }
        let (server_wide, shares) = self.sharing(actor, visibility, false).await?;
        let row = self
            .store
            .create_playlist(&NewPlaylist {
                owner: actor.principal.to_string(),
                name,
                kinds,
                server_wide,
                shares,
            })
            .await?;
        self.told_playlist(&row.id, row.revision, None, Some(Seen::of(&row)));
        self.summarized(actor, &row, None, &Names::of(&self.store).await?)
            .await
    }

    /// `visibility` as rows: whether server-wide, and the shares. Only an
    /// admin makes a playlist server-wide; `was_server_wide` keeps one so
    /// for its owner who is no longer an admin.
    async fn sharing(
        &self,
        actor: &Actor,
        visibility: Visibility,
        was_server_wide: bool,
    ) -> Result<(bool, Vec<ShareRow>), PlaylistsError> {
        match visibility {
            Visibility::Personal => Ok((false, Vec::new())),
            Visibility::ServerWide if actor.admin || was_server_wide => Ok((true, Vec::new())),
            Visibility::ServerWide => Err(PlaylistsError::AdminRequired),
            Visibility::Shared { with } => {
                if with.len() > MAX_SHARES {
                    return Err(PlaylistsError::InvalidInput(format!(
                        "a playlist is shared with at most {MAX_SHARES} users"
                    )));
                }
                let mut shares: Vec<ShareRow> = Vec::with_capacity(with.len());
                for share in with {
                    let principal: Principal = share.user_id.parse().map_err(|_| {
                        PlaylistsError::InvalidInput(format!("{} is not a user", share.user_id))
                    })?;
                    if principal == actor.principal {
                        continue;
                    }
                    if !self.exists(principal).await? {
                        return Err(PlaylistsError::InvalidInput(format!(
                            "{} is not a user here",
                            share.user_id
                        )));
                    }
                    let principal = principal.to_string();
                    shares.retain(|s| s.principal != principal);
                    shares.push(ShareRow {
                        principal,
                        can_edit: share.can_edit,
                    });
                }
                Ok((false, shares))
            }
            Visibility::Unknown => Err(PlaylistsError::InvalidInput(
                "a sharing this server does not know".into(),
            )),
        }
    }

    /// Whether `principal` is a user here: an account, or a portable user
    /// granted access.
    async fn exists(&self, principal: Principal) -> Result<bool, PlaylistsError> {
        Ok(match principal {
            Principal::LocalUser(id) => self
                .store
                .user_by_id(id)
                .await
                .map_err(|e| match e {
                    crate::store::users::UserError::Store(e) => PlaylistsError::Store(e),
                    other => PlaylistsError::InvalidInput(other.to_string()),
                })?
                .is_some(),
            Principal::PortableUser(id) => self.store.grant_of(id).await?.is_some(),
            // A paired server's user has nothing here (P8-03): their
            // playlists are on their own server.
            Principal::RemoteUser { .. } => false,
        })
    }

    /// Rename playlist `id`, widen its kinds, or change who sees it: its
    /// owner's to do, or any admin's for a server-wide one.
    pub async fn update(
        &self,
        actor: &Actor,
        id: &str,
        update: PlaylistUpdate,
    ) -> Result<PlaylistSummary, PlaylistsError> {
        let (row, access) = self.seen(actor, id).await?;
        if !access.can_manage() {
            return Err(PlaylistsError::ReadOnly);
        }
        let name = update.name.as_deref().map(checked_name).transpose()?;
        if let Some(kinds) = update.kinds
            && (kinds.is_empty() || !row.kinds.widen_only(kinds))
        {
            return Err(PlaylistsError::InvalidInput(
                "a playlist's kinds can only be widened".into(),
            ));
        }
        let (server_wide, shares) = match update.visibility {
            Some(visibility) => {
                let (server_wide, shares) =
                    self.sharing(actor, visibility, row.server_wide).await?;
                (Some(server_wide), Some(shares))
            }
            None => (None, None),
        };
        let pinned_before = self.store.pinned_by(id).await?;
        let revision = self
            .store
            .update_playlist(
                id,
                &PlaylistChange {
                    name,
                    kinds: update.kinds,
                    server_wide,
                    shares,
                },
            )
            .await?;
        let after = self
            .store
            .playlist(id)
            .await?
            .ok_or(PlaylistsError::NotFound)?;
        self.told_playlist(id, revision, Some(Seen::of(&row)), Some(Seen::of(&after)));
        let pinned_after = self.store.pinned_by(id).await?;
        for lost in pinned_before.difference(&pinned_after) {
            if let Ok(principal) = lost.parse() {
                self.tell(PlaylistNews::Pins { principal });
            }
        }
        self.summarized(
            actor,
            &after,
            self.pinned(actor, id).await?,
            &Names::of(&self.store).await?,
        )
        .await
    }

    /// Apply `edits`, made on `revision`, to playlist `id`'s entries: its
    /// new revision. Its owner's, an editor's, or an admin's for a
    /// server-wide one.
    pub async fn edit(
        &self,
        actor: &Actor,
        id: &str,
        revision: u64,
        edits: Vec<PlaylistEdit>,
    ) -> Result<u64, PlaylistsError> {
        let (row, access) = self.seen(actor, id).await?;
        if !access.can_edit() {
            return Err(PlaylistsError::ReadOnly);
        }
        if edits.is_empty() || edits.len() > MAX_EDITS {
            return Err(PlaylistsError::InvalidInput(format!(
                "an edit carries 1 to {MAX_EDITS} changes"
            )));
        }
        if let Some(builtin) = row.builtin
            && edits.iter().any(|e| !policy::builtin_edit(builtin, e))
        {
            return Err(PlaylistsError::InvalidInput(
                "a track goes into Liked by liking it; here it is only moved or removed".into(),
            ));
        }
        let mut resolved = Vec::with_capacity(edits.len());
        for edit in edits {
            resolved.push(match edit {
                PlaylistEdit::Add { id: item, before } => EntryEdit::Add {
                    entries: self.adding(actor, row.kinds, &item).await?,
                    before,
                },
                PlaylistEdit::Remove { entry } => EntryEdit::Remove(entry),
                PlaylistEdit::Move { entry, before } => EntryEdit::Move { entry, before },
                PlaylistEdit::Unknown => {
                    return Err(PlaylistsError::InvalidInput(
                        "an edit this server does not know".into(),
                    ));
                }
            });
        }
        let new = match self.store.edit_playlist(id, revision, resolved).await {
            Ok(new) => new,
            Err(PlaylistError::Conflict) => {
                let now = self.store.playlist(id).await?.map_or(0, |row| row.revision);
                return Err(PlaylistsError::Conflict { revision: now });
            }
            Err(e) => return Err(e.into()),
        };
        let seen = Seen::of(&row);
        self.told_playlist(id, new, Some(seen.clone()), Some(seen));
        Ok(new)
    }

    /// The entries adding `item` makes, in their order: the item itself, or
    /// a season's episodes, a show's (specials last), an album's tracks,
    /// an artist's albums' tracks (oldest album first). Refused when the
    /// playlist's `kinds` do not take it; items `actor` may not see are
    /// left out.
    async fn adding(
        &self,
        actor: &Actor,
        kinds: Kinds,
        item: &MediaId,
    ) -> Result<Vec<NewEntry>, PlaylistsError> {
        if !kinds.allows(item.kind()) {
            return Err(PlaylistsError::KindNotAllowed(item.kind()));
        }
        let found = match self.catalog.item(item).await {
            Ok(found) => found,
            Err(
                CatalogError::NotFound(_) | CatalogError::NotServed(_) | CatalogError::ForeignId(_),
            ) => {
                return Err(PlaylistsError::NoItem(item.clone()));
            }
            Err(e) => return Err(e.into()),
        };
        if !self.may_see(actor, &found.item) {
            return Err(PlaylistsError::NoItem(item.clone()));
        }
        let mut leaves = match &found.item {
            Item::Movie(_) | Item::Episode(_) | Item::Track(_) => {
                return Ok(vec![new_entry(&found.item, found.external_ids)]);
            }
            Item::Season(_) => episodes_of(found.children),
            Item::Show(_) => {
                let mut episodes = episodes_of(self.catalog.episodes(item).await?);
                // Seasons in order, then the specials.
                episodes.sort_by_key(|e| match e {
                    Item::Episode(e) => (e.season == 0, e.season, e.episode),
                    _ => (true, u32::MAX, u32::MAX),
                });
                episodes
            }
            Item::Album(_) => tracks_of(found.children),
            Item::Artist(_) => self.artist_tracks(found).await?,
        };
        leaves.retain(|leaf| self.may_see(actor, leaf));
        // Each one's stable ids, for following it to a new key later.
        let entries = stream::iter(leaves)
            .map(|leaf| async move {
                let ids = self.catalog.external_ids(leaf.id()).await?;
                Ok::<_, CatalogError>(new_entry(&leaf, ids))
            })
            .buffered(CATALOG_FETCHES)
            .try_collect()
            .await?;
        Ok(entries)
    }

    /// Every track of `artist`'s albums, the oldest album first.
    async fn artist_tracks(&self, artist: ItemWithChildren) -> Result<Vec<Item>, PlaylistsError> {
        let mut albums: Vec<_> = artist
            .children
            .into_iter()
            .filter_map(|child| match child {
                Item::Album(album) => Some(album),
                _ => None,
            })
            .collect();
        albums.sort_by(|a, b| (a.year, &a.title).cmp(&(b.year, &b.title)));
        // A few at a time, in the albums' order.
        let albums: Vec<ItemWithChildren> = stream::iter(albums)
            .map(|album| async move { self.catalog.item(&album.id).await })
            .buffered(CATALOG_FETCHES)
            .try_collect()
            .await?;
        Ok(albums
            .into_iter()
            .flat_map(|album| tracks_of(album.children))
            .collect())
    }

    /// Delete playlist `id`: its owner's, or any admin's for a server-wide
    /// one.
    pub async fn delete(&self, actor: &Actor, id: &str) -> Result<(), PlaylistsError> {
        let (row, access) = self.seen(actor, id).await?;
        if !access.can_manage() {
            return Err(PlaylistsError::ReadOnly);
        }
        let pinned = self.store.pinned_by(id).await?;
        self.store
            .delete_playlist(id, &actor.principal.to_string())
            .await?;
        self.let_go_of(row.art.as_deref()).await;
        self.told_playlist(id, row.revision + 1, Some(Seen::of(&row)), None);
        for principal in pinned.iter().filter_map(|p| p.parse::<Principal>().ok()) {
            self.tell(PlaylistNews::Pins { principal });
        }
        Ok(())
    }

    /// Pin playlist `id` to `actor`'s Pinned at `position` (0 first), or
    /// move it there.
    pub async fn pin(&self, actor: &Actor, id: &str, position: u32) -> Result<(), PlaylistsError> {
        self.seen(actor, id).await?;
        self.store
            .pin_playlist(&actor.principal.to_string(), id, position as usize)
            .await?;
        self.tell(PlaylistNews::Pins {
            principal: actor.principal,
        });
        Ok(())
    }

    /// Take playlist `id` out of `actor`'s Pinned; nothing if it is not
    /// there.
    pub async fn unpin(&self, actor: &Actor, id: &str) -> Result<(), PlaylistsError> {
        if self
            .store
            .unpin_playlist(&actor.principal.to_string(), id)
            .await?
        {
            self.tell(PlaylistNews::Pins {
                principal: actor.principal,
            });
        }
        Ok(())
    }

    /// Forget `actor`'s pin notes: a client showed them.
    pub async fn dismiss_pin_notes(&self, actor: &Actor) -> Result<(), PlaylistsError> {
        if self
            .store
            .dismiss_pin_notes(&actor.principal.to_string())
            .await?
        {
            self.tell(PlaylistNews::Pins {
                principal: actor.principal,
            });
        }
        Ok(())
    }

    /// Start or go on playing playlist `id` with `settings`, kept for
    /// `actor` and it: the entry to play, or `None` when nothing is left.
    /// From `from`, else from `actor`'s place (that entry again unless
    /// they finished it), else from the top.
    pub async fn play(
        &self,
        actor: &Actor,
        id: &str,
        from: Option<EntryId>,
        settings: PlaySettings,
    ) -> Result<Option<PlaylistNext>, PlaylistsError> {
        let (row, _) = self.seen(actor, id).await?;
        let (place, entries, _) = self
            .place_with_round(actor, &row, Some((settings, from)))
            .await?;
        let start = match from {
            Some(from) => Some(Start::At(from)),
            None => place.entry.map(Start::Resume),
        };
        self.go_on(actor, &row, place, start, &entries).await
    }

    /// What plays in playlist `id` after entry `after` ended or was
    /// skipped, as `actor` plays it: `None` at the end.
    pub async fn next(
        &self,
        actor: &Actor,
        id: &str,
        after: EntryId,
    ) -> Result<Option<PlaylistNext>, PlaylistsError> {
        let (row, _) = self.seen(actor, id).await?;
        let (place, entries, _) = self.place_with_round(actor, &row, None).await?;
        self.go_on(actor, &row, place, Some(Start::After(after)), &entries)
            .await
    }

    /// Keep `settings` as how `actor` plays playlist `id`, playing nothing:
    /// the place and the round stay. Shuffle turned on makes a new round,
    /// starting at the place (the whole playlist ahead); turned off, the
    /// playlist's order, from the place.
    pub async fn set_settings(
        &self,
        actor: &Actor,
        id: &str,
        settings: PlaySettings,
    ) -> Result<(), PlaylistsError> {
        let (row, _) = self.seen(actor, id).await?;
        let principal = actor.principal.to_string();
        let mut place = self
            .store
            .playlist_place(&principal, id)
            .await?
            .unwrap_or_default();
        // The entries are read only for a new round: a repeat or
        // skip-watched toggle on All must not walk the library. A kept
        // round is brought up to date when it is next played or listed.
        place.round = match round_turn(&place, settings, None) {
            RoundTurn::Off => None,
            RoundTurn::Kept => place.round,
            RoundTurn::New { first } => {
                let entries = self.order_of(actor, &row).await?;
                let now: Vec<EntryId> = entries.iter().map(|e| e.entry_id).collect();
                Some(round_from(&now, first, self.seed()))
            }
        };
        place.settings = settings;
        self.store
            .set_playlist_place(&principal, &row.id, &place)
            .await?;
        Ok(())
    }

    /// The next `limit` ([`MAX_UPCOMING`]) entries `actor` plays in
    /// playlist `id` after their place, each the one `next` would give
    /// after the one before: the round's order when shuffling, else the
    /// playlist's; from the start again with repeat all; the place's
    /// entry alone with repeat one, which plays it again. With no place,
    /// from the top. Unavailable entries are passed, and with
    /// `skip_watched` watched ones, as playing passes them.
    pub async fn upcoming(
        &self,
        actor: &Actor,
        id: &str,
        limit: u32,
    ) -> Result<Vec<PlaylistEntry>, PlaylistsError> {
        let (row, _) = self.seen(actor, id).await?;
        let (place, entries, changed) = self.place_with_round(actor, &row, None).await?;
        // The round as `next` will play it, kept so: an entry added since
        // gets its place once.
        if changed {
            self.store
                .set_playlist_place(&actor.principal.to_string(), &row.id, &place)
                .await?;
        }
        let order = order_of_place(&place, &entries);
        let settings = place.settings;
        let mut lookup = Lookup::new(&row, &entries, settings);
        let mut upcoming = Vec::new();
        let mut after = place.entry;
        let limit = limit.min(MAX_UPCOMING) as usize;
        while upcoming.len() < limit {
            // As `go_on` tries them.
            let tries = policy::next_from(&order, after, settings.repeat);
            let Some(next) = self.play_on(actor, &mut lookup, &order, &tries).await? else {
                break;
            };
            after = Some(next.entry);
            upcoming.push(PlaylistEntry {
                entry: next.entry,
                item: next.item,
                unavailable: false,
                liked: next.liked,
            });
            if settings.repeat == Repeat::One {
                // `next` gives it again, and again.
                break;
            }
        }
        Ok(upcoming)
    }

    /// `actor`'s place in `row` and `row`'s entries now, the place's round
    /// as playing keeps it, and whether the place differs from the one
    /// stored (none stored reads as the default).
    /// With `chosen` settings and the entry picked, if any, those become
    /// the place's. The round is brought up to date with the entries:
    /// those taken out leave it, and those added since go among the ones
    /// not played yet, so what is listed as coming up is what plays.
    async fn place_with_round(
        &self,
        actor: &Actor,
        row: &PlaylistRow,
        chosen: Option<(PlaySettings, Option<EntryId>)>,
    ) -> Result<(PlaceRow, Vec<EntryRow>, bool), PlaylistsError> {
        let kept = self
            .store
            .playlist_place(&actor.principal.to_string(), &row.id)
            .await?;
        let entries = self.order_of(actor, row).await?;
        let now: Vec<EntryId> = entries.iter().map(|e| e.entry_id).collect();
        let kept = kept.unwrap_or_default();
        let mut place = kept.clone();
        if let Some((settings, from)) = chosen {
            place.round = match round_turn(&place, settings, from) {
                RoundTurn::Off => None,
                RoundTurn::Kept => place.round,
                RoundTurn::New { first } => Some(round_from(&now, first, self.seed())),
            };
            place.settings = settings;
        }
        if let Some(round) = &place.round {
            place.round = Some(policy::reconciled(round, &now, place.entry, self.seed()));
        }
        let changed = place != kept;
        Ok((place, entries, changed))
    }

    /// Find what plays from `start` among `entries` by `place`'s settings
    /// and round, keep it as the place, and give it.
    async fn go_on(
        &self,
        actor: &Actor,
        row: &PlaylistRow,
        mut place: PlaceRow,
        start: Option<Start>,
        entries: &[EntryRow],
    ) -> Result<Option<PlaylistNext>, PlaylistsError> {
        let order = order_of_place(&place, entries);
        let repeat = place.settings.repeat;
        let mut lookup = Lookup::new(row, entries, place.settings);
        // The entries to try in turn, and the one among them that plays
        // even when watched: chosen, resumed, or repeated.
        let tries = match start {
            Some(Start::At(from)) => policy::again_from(&order, from, repeat),
            Some(Start::Resume(at)) => {
                let finished = match lookup.entries.get(&at) {
                    Some(entry) => self.watched(actor, entry).await?,
                    None => true,
                };
                if finished {
                    policy::next_from(&order, Some(at), repeat)
                } else {
                    policy::again_from(&order, at, repeat)
                }
            }
            Some(Start::After(after)) => policy::next_from(&order, Some(after), repeat),
            None => policy::next_from(&order, None, repeat),
        };
        let found = self.play_on(actor, &mut lookup, &order, &tries).await?;
        // At the end the next play starts at the top, of the same round.
        place.entry = found.as_ref().map(|next| next.entry);
        self.store
            .set_playlist_place(&actor.principal.to_string(), &row.id, &place)
            .await?;
        Ok(found)
    }

    /// What plays by `tries` in `order`: the first of its entries that can
    /// be played, else, with repeat all, the first of the same round again
    /// from its start (shuffle was not turned off and on).
    async fn play_on(
        &self,
        actor: &Actor,
        lookup: &mut Lookup<'_>,
        order: &[EntryId],
        tries: &policy::Tries,
    ) -> Result<Option<PlaylistNext>, PlaylistsError> {
        let found = self
            .first_playable(actor, lookup, &tries.tried, tries.regardless)
            .await?;
        if found.is_some() || !tries.wraps {
            return Ok(found);
        }
        self.first_playable(actor, lookup, order, None).await
    }

    /// The first of `tried` that can be played ([`Self::playable`]) and,
    /// with `skip_watched`, is not watched (unless it is `regardless`).
    async fn first_playable(
        &self,
        actor: &Actor,
        lookup: &mut Lookup<'_>,
        tried: &[EntryId],
        regardless: Option<EntryId>,
    ) -> Result<Option<PlaylistNext>, PlaylistsError> {
        for entry_id in tried {
            let Some(next) = self.playable(actor, lookup, *entry_id).await? else {
                continue;
            };
            if lookup.settings.skip_watched && watched(&next.item) && regardless != Some(*entry_id)
            {
                continue;
            }
            return Ok(Some(next));
        }
        Ok(None)
    }

    /// Entry `entry_id` as it plays, if it can: still among `lookup`'s
    /// entries, its item there, readable and seen by `actor`. Each entry
    /// is looked at once per `lookup`, so one gone or moved is dropped or
    /// followed once.
    async fn playable(
        &self,
        actor: &Actor,
        lookup: &mut Lookup<'_>,
        entry_id: EntryId,
    ) -> Result<Option<PlaylistNext>, PlaylistsError> {
        if let Some(known) = lookup.plays.get(&entry_id) {
            return Ok(known.clone());
        }
        let found = self.look_at(actor, lookup, entry_id).await?;
        lookup.plays.insert(entry_id, found.clone());
        Ok(found)
    }

    /// [`Self::playable`], unremembered.
    async fn look_at(
        &self,
        actor: &Actor,
        lookup: &mut Lookup<'_>,
        entry_id: EntryId,
    ) -> Result<Option<PlaylistNext>, PlaylistsError> {
        let row = lookup.row;
        let Some(entry) = lookup.entries.get(&entry_id).copied() else {
            return Ok(None);
        };
        let (mut item, markers) = match self.resolve(entry).await? {
            Resolved::Here {
                item,
                markers,
                unavailable: false,
            } => (item, markers),
            Resolved::Moved {
                item,
                markers,
                unavailable: false,
                to,
            } => {
                let revision = self
                    .store
                    .relink_playlist_entry(&row.id, entry.entry_id, &to.to_string())
                    .await?;
                self.told_playlist(&row.id, revision, Some(Seen::of(row)), Some(Seen::of(row)));
                (item, markers)
            }
            // All's are not stored: nothing to drop.
            Resolved::Gone if row.builtin == Some(Builtin::All) => return Ok(None),
            Resolved::Gone => {
                if let Some(revision) = self
                    .store
                    .drop_playlist_entries(&row.id, &[entry.entry_id])
                    .await?
                {
                    self.told_playlist(&row.id, revision, Some(Seen::of(row)), Some(Seen::of(row)));
                }
                return Ok(None);
            }
            // Unavailable, or in a library not served now.
            _ => return Ok(None),
        };
        if !self.may_see(actor, &item) {
            return Ok(None);
        }
        self.watch.fill(actor.principal, &mut [&mut item]).await?;
        let liked = match item {
            Item::Track(_) => {
                let liked = match &mut lookup.liked {
                    Some(liked) => liked,
                    none => {
                        none.insert(self.store.liked_media(&actor.principal.to_string()).await?)
                    }
                };
                liked.contains(&item.id().to_string())
            }
            _ => false,
        };
        Ok(Some(PlaylistNext {
            entry: entry_id,
            item,
            markers,
            liked,
        }))
    }

    /// Whether `actor` has watched `entry`'s item. One no longer served
    /// counts as done with.
    async fn watched(&self, actor: &Actor, entry: &EntryRow) -> Result<bool, PlaylistsError> {
        let Ok(id) = entry.media_id.parse::<MediaId>() else {
            return Ok(true);
        };
        let mut item = match self.catalog.item(&id).await {
            Ok(found) => found.item,
            Err(e) if crate::client::unserved(&e) => return Ok(true),
            Err(e) => return Err(e.into()),
        };
        self.watch.fill(actor.principal, &mut [&mut item]).await?;
        Ok(watched(&item))
    }

    /// Make a personal playlist named `name` for `actor` of `items`, in
    /// their order: an import's (PX-40, from Plex). Its kinds are those of
    /// the items found. Each item the catalog does not serve, or that
    /// `actor` may not see, is left out, with why. With `dry_run` nothing
    /// is made. The playlist, if one was (or would be) made, and its
    /// entry count.
    pub async fn import(
        &self,
        actor: &Actor,
        name: &str,
        items: &[(MediaId, String)],
        dry_run: bool,
    ) -> Result<(Option<String>, u32, Vec<(String, String)>), PlaylistsError> {
        let name = checked_name(&name.chars().take(MAX_NAME).collect::<String>())?;
        let mut entries = Vec::new();
        let mut left_out = Vec::new();
        for (id, title) in items {
            if !matches!(
                id.kind(),
                MediaKind::Movie | MediaKind::Episode | MediaKind::Track
            ) {
                left_out.push((title.clone(), "not a movie, episode or track".into()));
                continue;
            }
            match self.catalog.item(id).await {
                Ok(found) if self.may_see(actor, &found.item) => {
                    entries.push(new_entry(&found.item, found.external_ids));
                }
                Ok(_) => left_out.push((title.clone(), "not in this server's libraries".into())),
                Err(e) if crate::client::unserved(&e) => {
                    left_out.push((title.clone(), "not in this server's libraries".into()));
                }
                Err(e) => return Err(e.into()),
            }
        }
        let count = entries.len() as u32;
        if entries.is_empty() || dry_run {
            return Ok((None, count, left_out));
        }
        let kinds = Kinds {
            movies: entries.iter().any(|e| e.kind == "movie"),
            episodes: entries.iter().any(|e| e.kind == "episode"),
            tracks: entries.iter().any(|e| e.kind == "track"),
        };
        let row = self
            .store
            .create_playlist(&NewPlaylist {
                owner: actor.principal.to_string(),
                name,
                kinds,
                server_wide: false,
                shares: Vec::new(),
            })
            .await?;
        let revision = self
            .store
            .edit_playlist(
                &row.id,
                row.revision,
                vec![EntryEdit::Add {
                    entries,
                    before: None,
                }],
            )
            .await?;
        self.told_playlist(&row.id, revision, None, Some(Seen::of(&row)));
        Ok((Some(row.id), count, left_out))
    }

    /// The playlists `actor` sees that hold `item` (with `children`, as
    /// the item's details have them): a movie, episode or track itself; a
    /// season's episodes; any of a show's episodes, an album's or an
    /// artist's tracks.
    ///
    /// Liked is among them with `builtins`; All never is, as it holds
    /// every track and saying so tells nothing.
    pub async fn holding(
        &self,
        actor: &Actor,
        item: &Item,
        children: &[Item],
        builtins: bool,
    ) -> Result<Vec<PlaylistRef>, PlaylistsError> {
        let id = item.id().to_string();
        let (by, values) = match item {
            Item::Movie(_) | Item::Episode(_) | Item::Track(_) => (HeldBy::Item, vec![id]),
            Item::Show(_) => (HeldBy::Show, vec![id]),
            Item::Album(_) => (HeldBy::Album, vec![id]),
            Item::Artist(_) => (HeldBy::Artist, vec![id]),
            Item::Season(_) => (
                HeldBy::Item,
                children.iter().map(|c| c.id().to_string()).collect(),
            ),
        };
        let rows = self
            .store
            .playlists_holding(&actor.principal.to_string(), by, &values)
            .await?;
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        // Read once for every reference: whose each is, as a summary says.
        let names = Names::of(&self.store).await?;
        let mut held = Vec::with_capacity(rows.len());
        for row in rows.iter().filter(|row| builtins || row.builtin.is_none()) {
            // Its picture as its own page has it (PX-53); a mosaic is kept
            // per revision and viewer, so a page opened again asks the
            // catalog nothing.
            let summary = self.summarized(actor, row, None, &names).await?;
            held.push(PlaylistRef {
                id: summary.id,
                name: summary.name,
                owner_kind: summary.owner_kind,
                builtin: summary.builtin,
                owner_name: summary.owner_name,
                artwork: summary.artwork,
            });
        }
        Ok(held)
    }
}

/// Sharing by name (`spoolframe:playlist-sharing`).
impl Playlists {
    /// Whom playlist `id` can be shared with, those whose name holds
    /// `query` (empty: everyone), a page from `offset` of at most `limit`
    /// ([`MAX_PAGE`]), and how many match in all. Only whoever manages
    /// it asks: anyone else who sees it is [`PlaylistsError::ReadOnly`],
    /// as for a rename. The caller is never among them.
    pub async fn share_candidates(
        &self,
        actor: &Actor,
        id: &str,
        query: &str,
        offset: u32,
        limit: u32,
    ) -> Result<ShareCandidatePage, PlaylistsError> {
        let (_, access) = self.seen(actor, id).await?;
        if !access.can_manage() {
            return Err(PlaylistsError::ReadOnly);
        }
        let found = people::matching(
            people::share_candidates(&self.store, actor.principal).await?,
            query,
        );
        let total = u32::try_from(found.len()).unwrap_or(u32::MAX);
        let candidates = found
            .iter()
            .skip(offset as usize)
            .take(limit.min(MAX_PAGE) as usize)
            .map(people::Person::candidate)
            .collect();
        Ok(ShareCandidatePage {
            candidates,
            offset,
            total,
        })
    }
}

/// What a device's music download follows (PX-51).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Followed {
    Artist(MediaId),
    /// A playlist, by id: not All.
    Playlist(String),
    Liked,
}

/// Music downloads (PX-51): what an artist, a playlist or Liked holds.
impl Playlists {
    /// The tracks `followed` holds now for `actor`, in its order, each
    /// once: an artist's albums oldest first, each in disc and track
    /// order; a playlist's or Liked's entries in theirs. Only tracks
    /// `actor` may see. `None` when it is gone, or not `actor`'s to see.
    /// An id that is not an artist is `InvalidInput`, and so is All, as
    /// it is every track.
    pub async fn followed_tracks(
        &self,
        actor: &Actor,
        followed: &Followed,
    ) -> Result<Option<Vec<MediaId>>, PlaylistsError> {
        let tracks = match followed {
            Followed::Artist(id) => {
                if id.kind() != MediaKind::Artist {
                    return Err(PlaylistsError::InvalidInput(format!(
                        "{id} is not an artist"
                    )));
                }
                let found = match self.catalog.item(id).await {
                    Ok(found) => found,
                    Err(e) if crate::client::unserved(&e) => return Ok(None),
                    Err(e) => return Err(e.into()),
                };
                if !self.may_see(actor, &found.item) {
                    return Ok(None);
                }
                let mut tracks = self.artist_tracks(found).await?;
                tracks.retain(|t| self.may_see(actor, t));
                tracks
            }
            Followed::Playlist(id) => {
                let row = match self.seen(actor, id).await {
                    Ok((row, _)) => row,
                    Err(PlaylistsError::NotFound) => return Ok(None),
                    Err(e) => return Err(e),
                };
                if row.builtin == Some(Builtin::All) {
                    return Err(PlaylistsError::InvalidInput(
                        "All is every track: follow its artists instead".into(),
                    ));
                }
                self.entry_items(actor, &row).await?
            }
            Followed::Liked => {
                let row = self
                    .store
                    .builtin_playlist(&actor.principal.to_string(), Builtin::Liked)
                    .await?;
                self.entry_items(actor, &row).await?
            }
        };
        let mut seen = HashSet::new();
        Ok(Some(
            tracks
                .into_iter()
                .filter(|t| matches!(t, Item::Track(_)))
                .map(|t| t.id().clone())
                .filter(|id| seen.insert(id.clone()))
                .collect(),
        ))
    }

    /// Take in a like or an unlike of `track` made by `actor` on a device
    /// with no server (PX-51): made only when newer than the track's last
    /// change here, newest winning whichever device made it. The device's
    /// clock is set right first, as for a watch state ([`Watch::offline`]):
    /// `at` is moved by how far `device_now` is from the server's clock,
    /// and kept no later than now. A track gone from the library is not
    /// liked.
    ///
    /// [`Watch::offline`]: crate::watch::Watch::offline
    pub async fn offline_like(
        &self,
        actor: &Actor,
        track: &MediaId,
        liked: bool,
        at: i64,
        device_now: i64,
    ) -> Result<crate::watch::Synced, PlaylistsError> {
        use crate::watch::Synced;
        if track.kind() != MediaKind::Track {
            return Ok(Synced::NotTracked);
        }
        let at = self.store.server_time_of(at, device_now);
        let principal = actor.principal.to_string();
        let media_id = track.to_string();
        if self
            .store
            .liked_changed_at(&principal, &media_id)
            .await?
            .is_some_and(|last| last >= at)
        {
            return Ok(Synced::Older);
        }
        let made = if liked {
            self.like(actor, std::slice::from_ref(track)).await
        } else {
            self.unlike(actor, std::slice::from_ref(track)).await
        };
        match made {
            Ok(_) => {}
            Err(PlaylistsError::NoItem(_)) => return Ok(Synced::NotTracked),
            Err(e) => return Err(e),
        }
        // When it was made on the device, not when it got here.
        self.store
            .note_liked(&principal, &[media_id], liked, at)
            .await?;
        Ok(Synced::Applied)
    }

    /// The items of `row`'s entries `actor` sees, in order.
    async fn entry_items(
        &self,
        actor: &Actor,
        row: &PlaylistRow,
    ) -> Result<Vec<Item>, PlaylistsError> {
        Ok(self
            .entries(actor, row)
            .await?
            .into_iter()
            .map(|e| e.item)
            .collect())
    }
}

/// How many times an unlike is made again on a Liked another device of
/// the user's changed meanwhile.
const UNLIKE_TRIES: usize = 3;

/// Liked (PX-43): hearts on tracks and albums.
impl Playlists {
    /// Which built-in playlist `id` is, if it is one: what a client that
    /// does not take them is refused by.
    pub async fn builtin_of(&self, id: &str) -> Result<Option<Builtin>, PlaylistsError> {
        Ok(self.store.playlist(id).await?.and_then(|row| row.builtin))
    }

    /// `items`, if they may be liked or unliked: 1 to [`MAX_EDITS`]
    /// tracks or albums.
    fn likeable(items: &[MediaId]) -> Result<(), PlaylistsError> {
        if items.is_empty() || items.len() > MAX_EDITS {
            return Err(PlaylistsError::InvalidInput(format!(
                "1 to {MAX_EDITS} items are liked or unliked at a time"
            )));
        }
        match items.iter().find(|id| !policy::likeable(id.kind())) {
            Some(id) => Err(PlaylistsError::NotLikeable(id.kind())),
            None => Ok(()),
        }
    }

    /// Playlist `id` as `actor` sees it now.
    async fn summary_now(
        &self,
        actor: &Actor,
        id: &str,
    ) -> Result<PlaylistSummary, PlaylistsError> {
        let row = self
            .store
            .playlist(id)
            .await?
            .ok_or(PlaylistsError::NotFound)?;
        self.summarized(
            actor,
            &row,
            self.pinned(actor, id).await?,
            &Names::of(&self.store).await?,
        )
        .await
    }

    /// Put `entries` at the top of Liked `row`, newest first: its new
    /// revision.
    async fn to_top(
        &self,
        row: &PlaylistRow,
        entries: Vec<NewEntry>,
    ) -> Result<u64, PlaylistsError> {
        let top = self
            .store
            .playlist_entries(&row.id)
            .await?
            .first()
            .map(|e| e.entry_id);
        let revision = self
            .store
            .edit_playlist(
                &row.id,
                row.revision,
                vec![EntryEdit::Add {
                    entries,
                    before: top,
                }],
            )
            .await?;
        self.told_playlist(&row.id, revision, Some(Seen::of(row)), Some(Seen::of(row)));
        Ok(revision)
    }

    /// Like `items`, tracks or albums: their tracks not liked yet go to
    /// the top of `actor`'s Liked, in the order given, an album's in disc
    /// and track order. Liked as it is after.
    pub async fn like(
        &self,
        actor: &Actor,
        items: &[MediaId],
    ) -> Result<PlaylistSummary, PlaylistsError> {
        Self::likeable(items)?;
        let row = self
            .store
            .builtin_playlist(&actor.principal.to_string(), Builtin::Liked)
            .await?;
        let mut held: HashSet<String> = self
            .store
            .playlist_entries(&row.id)
            .await?
            .into_iter()
            .map(|e| e.media_id)
            .collect();
        let mut new = Vec::new();
        for item in items {
            for entry in self.adding(actor, TRACKS, item).await? {
                if held.insert(entry.media_id.clone()) {
                    new.push(entry);
                }
            }
        }
        if !new.is_empty() {
            let liked: Vec<String> = new.iter().map(|e| e.media_id.clone()).collect();
            self.to_top(&row, new).await?;
            self.noted_liked(actor.principal, &liked, true).await?;
        }
        self.summary_now(actor, &row.id).await
    }

    /// Take `items` out of `actor`'s Liked: a track itself; an album each
    /// of its tracks, those it has now and those liked from it before.
    /// Liked as it is after.
    pub async fn unlike(
        &self,
        actor: &Actor,
        items: &[MediaId],
    ) -> Result<PlaylistSummary, PlaylistsError> {
        Self::likeable(items)?;
        let mut tracks: HashSet<String> = HashSet::new();
        let mut albums: HashSet<String> = HashSet::new();
        for item in items {
            if item.kind() == MediaKind::Track {
                tracks.insert(item.to_string());
                continue;
            }
            albums.insert(item.to_string());
            match self.catalog.item(item).await {
                Ok(found) => tracks.extend(
                    found
                        .children
                        .iter()
                        .filter(|c| matches!(c, Item::Track(_)))
                        .map(|c| c.id().to_string()),
                ),
                // Gone from the library: its tracks go by their album.
                Err(e) if crate::client::unserved(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        let principal = actor.principal.to_string();
        let liked = self
            .store
            .builtin_playlist(&principal, Builtin::Liked)
            .await?;
        for _ in 0..UNLIKE_TRIES {
            let row = self
                .store
                .playlist(&liked.id)
                .await?
                .ok_or(PlaylistsError::NotFound)?;
            let removed: Vec<EntryRow> = self
                .store
                .playlist_entries(&row.id)
                .await?
                .into_iter()
                .filter(|e| {
                    tracks.contains(&e.media_id)
                        || e.album_id.as_ref().is_some_and(|a| albums.contains(a))
                })
                .collect();
            if removed.is_empty() {
                return self.summary_now(actor, &row.id).await;
            }
            let removes = removed
                .iter()
                .map(|e| EntryEdit::Remove(e.entry_id))
                .collect();
            match self
                .store
                .edit_playlist(&row.id, row.revision, removes)
                .await
            {
                Ok(revision) => {
                    self.told_playlist(
                        &row.id,
                        revision,
                        Some(Seen::of(&row)),
                        Some(Seen::of(&row)),
                    );
                    let unliked: Vec<String> = removed.into_iter().map(|e| e.media_id).collect();
                    self.noted_liked(actor.principal, &unliked, false).await?;
                    return self.summary_now(actor, &row.id).await;
                }
                // Another device removed one meanwhile: again, on the new
                // revision.
                Err(PlaylistError::Conflict) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        let now = self
            .store
            .playlist(&liked.id)
            .await?
            .map_or(0, |r| r.revision);
        Err(PlaylistsError::Conflict { revision: now })
    }

    /// `item`'s heart for `actor`, and which of `children` (its details',
    /// as the catalog has them now) are liked: a track's `All` or `None`;
    /// an album's by the tracks it has now that `actor` may see, so a
    /// track added to it later makes a full heart half. `None` for
    /// anything else.
    pub async fn liked(
        &self,
        actor: &Actor,
        item: &Item,
        children: &[Item],
    ) -> Result<(Option<Liked>, Option<Vec<MediaId>>), PlaylistsError> {
        if !matches!(item, Item::Track(_) | Item::Album(_)) {
            return Ok((None, None));
        }
        let liked = self.store.liked_media(&actor.principal.to_string()).await?;
        if let Item::Track(track) = item {
            let heart = if liked.contains(&track.id.to_string()) {
                Liked::All
            } else {
                Liked::None
            };
            return Ok((Some(heart), None));
        }
        let tracks: Vec<&MediaId> = children
            .iter()
            .filter(|c| matches!(c, Item::Track(_)) && self.may_see(actor, c))
            .map(Item::id)
            .collect();
        let hearted: Vec<MediaId> = tracks
            .iter()
            .filter(|id| liked.contains(&id.to_string()))
            .map(|id| (*id).clone())
            .collect();
        Ok((
            Some(policy::album_liked(tracks.len(), hearted.len())),
            Some(hearted),
        ))
    }

    /// Like `items`, tracks rated in Plex, for `actor`: a Plex import's
    /// (PX-43), into Liked rather than a playlist of its own, the first
    /// of them at the top. Those liked already are left where they are;
    /// those the catalog does not serve, or that `actor` may not see, are
    /// left out, with why. With `dry_run` nothing is liked. Liked's id
    /// (`None` on a dry run or when nothing is new), how many were newly
    /// liked, and what was left out.
    pub async fn import_liked(
        &self,
        actor: &Actor,
        items: &[(MediaId, String)],
        dry_run: bool,
    ) -> Result<(Option<String>, u32, Vec<(String, String)>), PlaylistsError> {
        let principal = actor.principal.to_string();
        let mut held = self.store.liked_media(&principal).await?;
        let mut entries = Vec::new();
        let mut left_out = Vec::new();
        for (id, title) in items {
            if id.kind() != MediaKind::Track {
                left_out.push((title.clone(), "not a track".into()));
                continue;
            }
            match self.catalog.item(id).await {
                Ok(found) if self.may_see(actor, &found.item) => {
                    let entry = new_entry(&found.item, found.external_ids);
                    if held.insert(entry.media_id.clone()) {
                        entries.push(entry);
                    }
                }
                Ok(_) => left_out.push((title.clone(), "not in this server's libraries".into())),
                Err(e) if crate::client::unserved(&e) => {
                    left_out.push((title.clone(), "not in this server's libraries".into()));
                }
                Err(e) => return Err(e.into()),
            }
        }
        let count = entries.len() as u32;
        if entries.is_empty() || dry_run {
            return Ok((None, count, left_out));
        }
        let row = self
            .store
            .builtin_playlist(&principal, Builtin::Liked)
            .await?;
        self.to_top(&row, entries).await?;
        Ok((Some(row.id), count, left_out))
    }
}

/// Covers (PX-53): uploaded, a mosaic of the first four, or a tile.
impl Playlists {
    /// `row`'s picture as `actor` sees it: Liked's or All's tile, its
    /// uploaded cover, else its mosaic.
    async fn art_of(&self, actor: &Actor, row: &PlaylistRow) -> Result<ArtworkId, PlaylistsError> {
        match row.builtin {
            Some(Builtin::Liked) => return Ok(crate::covers::tile_id(Tile::Liked)),
            Some(Builtin::All) => return Ok(crate::covers::tile_id(Tile::All)),
            _ => {}
        }
        if let Some(art) = row.art.as_deref().and_then(|a| a.parse().ok()) {
            return Ok(art);
        }
        self.mosaic(actor, row).await
    }

    /// The covers of `row`'s first four different albums, shows or films
    /// that `actor` may see, in its order: their mosaic; the first alone
    /// when there are fewer; the placeholder when there are none. Kept a
    /// while per revision and viewer.
    async fn mosaic(&self, actor: &Actor, row: &PlaylistRow) -> Result<ArtworkId, PlaylistsError> {
        // A viewer's mosaic has a paired server's covers only when they
        // see its items (P8-03).
        let viewer = format!(
            "{}{}",
            actor.principal,
            if actor.federated { " federated" } else { "" }
        );
        let key = (row.id.clone(), row.revision, viewer);
        if let Some((at, id)) = self.mosaics.lock().get(&key)
            && at.elapsed() < MOSAIC_FOR
        {
            return Ok(id.clone());
        }
        let entries = self.store.playlist_entries(&row.id).await?;
        let mut groups = HashSet::new();
        let mut covers: Vec<ArtworkId> = Vec::new();
        let mut lookups = 0;
        // A catalog that could not be asked: worked out again next time.
        let mut complete = true;
        for entry in &entries {
            if covers.len() == 4 || lookups == MOSAIC_LOOKUPS {
                break;
            }
            // An album's tracks are one cover: the album is asked for
            // once. A show's episodes are not: each season has its own
            // poster.
            let group = entry
                .album_id
                .clone()
                .unwrap_or_else(|| entry.media_id.clone());
            if !groups.insert(group) {
                continue;
            }
            let Ok(id) = entry.media_id.parse::<MediaId>() else {
                continue;
            };
            lookups += 1;
            let item = match self.catalog.item(&id).await {
                Ok(found) => found.item,
                Err(e) if crate::client::unserved(&e) => continue,
                Err(e) => {
                    tracing::debug!(error = %e, playlist = %row.id, "a mosaic's cover could not be looked up");
                    complete = false;
                    continue;
                }
            };
            if !self.may_see(actor, &item) {
                continue;
            }
            if let Some(poster) = poster_of(&item)
                && !covers.contains(poster)
            {
                covers.push(poster.clone());
            }
        }
        let art = match covers.len() {
            0 => crate::covers::tile_id(Tile::Empty),
            4 => crate::covers::mosaic_id(&covers),
            _ => covers.swap_remove(0),
        };
        if complete {
            let mut kept = self.mosaics.lock();
            if kept.len() >= MOST_MOSAICS {
                kept.clear();
            }
            kept.insert(key, (std::time::Instant::now(), art.clone()));
        }
        Ok(art)
    }

    /// Playlist `id`, if `actor` may change its cover: who may edit its
    /// entries, on a playlist that is not Liked or All. What an upload is
    /// checked by before its bytes are read.
    pub async fn may_set_art(
        &self,
        actor: &Actor,
        id: &str,
    ) -> Result<PlaylistRow, PlaylistsError> {
        let (row, access) = self.seen(actor, id).await?;
        if row.builtin.is_some() || !access.can_edit() {
            return Err(PlaylistsError::NotAllowed);
        }
        Ok(row)
    }

    /// Give playlist `id` the cover in `bytes`, decoded and kept: the
    /// playlist after. Every user who sees it sees the cover.
    pub async fn set_art(
        &self,
        actor: &Actor,
        id: &str,
        bytes: &[u8],
    ) -> Result<PlaylistSummary, PlaylistsError> {
        self.may_set_art(actor, id).await?;
        let covers = self.covers.as_ref().ok_or_else(|| {
            PlaylistsError::Unavailable("this server keeps no playlist covers".into())
        })?;
        let art = covers.ingest(bytes).await?;
        self.put_art(actor, id, Some(art)).await
    }

    /// Take playlist `id`'s uploaded cover away: its mosaic again.
    pub async fn remove_art(
        &self,
        actor: &Actor,
        id: &str,
    ) -> Result<PlaylistSummary, PlaylistsError> {
        let row = self.may_set_art(actor, id).await?;
        if row.art.is_none() {
            return self.summary_now(actor, id).await;
        }
        self.put_art(actor, id, None).await
    }

    /// Set playlist `id`'s cover to `art`, tell who sees it, and let go of
    /// the one it had.
    async fn put_art(
        &self,
        actor: &Actor,
        id: &str,
        art: Option<ArtworkId>,
    ) -> Result<PlaylistSummary, PlaylistsError> {
        let art = art.map(|a| a.to_string());
        let (revision, had) = self.store.set_playlist_art(id, art.as_deref()).await?;
        if had != art {
            self.let_go_of(had.as_deref()).await;
        }
        let row = self
            .store
            .playlist(id)
            .await?
            .ok_or(PlaylistsError::NotFound)?;
        self.told_playlist(id, revision, Some(Seen::of(&row)), Some(Seen::of(&row)));
        self.summary_now(actor, id).await
    }

    /// Delete cover `art`'s file if no playlist has it now.
    async fn let_go_of(&self, art: Option<&str>) {
        let (Some(art), Some(covers)) = (art, &self.covers) else {
            return;
        };
        match self.store.playlist_art_used(art).await {
            Ok(false) => {
                if let Ok(id) = art.parse() {
                    covers.remove(&id).await;
                }
            }
            Ok(true) => {}
            Err(e) => tracing::warn!(error = %e, "cannot tell whether a cover is still used; kept"),
        }
    }

    /// Whether `actor` sees a playlist whose uploaded cover is `art`: who
    /// may fetch it.
    pub async fn sees_art(&self, actor: &Actor, art: &ArtworkId) -> Result<bool, PlaylistsError> {
        Ok(self
            .store
            .playlist_art_seen_by(&actor.principal.to_string(), &art.to_string())
            .await?)
    }

    /// Playlist `id`'s picture as `actor` sees it, if they see it: what
    /// the web UI shows.
    pub async fn art(&self, actor: &Actor, id: &str) -> Result<ArtworkId, PlaylistsError> {
        let (row, _) = self.seen(actor, id).await?;
        self.art_of(actor, &row).await
    }

    /// Delete the kept covers no playlist has: at start.
    pub async fn sweep_covers(&self) -> Result<usize, PlaylistsError> {
        let Some(covers) = &self.covers else {
            return Ok(0);
        };
        let used = self.store.playlist_art_in_use().await?;
        covers.sweep(&used).await.map_err(PlaylistsError::Io)
    }

    /// Give playlist `id`, just imported for `actor`, the cover in
    /// `bytes`: a Plex playlist's own (PX-53).
    pub async fn import_art(
        &self,
        actor: &Actor,
        id: &str,
        bytes: &[u8],
    ) -> Result<(), PlaylistsError> {
        self.set_art(actor, id, bytes).await.map(|_| ())
    }
}

/// An item's cover: an album's, a show's or season's poster (an episode's
/// and a track's carry theirs), a film's poster.
fn poster_of(item: &Item) -> Option<&ArtworkId> {
    let artwork = match item {
        Item::Movie(m) => &m.artwork,
        Item::Episode(e) => &e.artwork,
        Item::Track(t) => &t.artwork,
        Item::Album(a) => &a.artwork,
        Item::Show(s) => &s.artwork,
        Item::Season(s) => &s.artwork,
        Item::Artist(a) => &a.artwork,
    };
    artwork
        .iter()
        .find(|a| a.kind == ArtworkKind::Poster)
        .map(|a| &a.id)
}

/// Where page `offset`, at most `limit` ([`MAX_PAGE`]), of `len` entries
/// starts and ends.
fn cut(len: usize, offset: u32, limit: u32) -> (usize, usize) {
    let start = (offset as usize).min(len);
    let end = start.saturating_add(limit.min(MAX_PAGE) as usize).min(len);
    (start, end)
}

/// What one call has found of which of a playlist's entries play.
struct Lookup<'a> {
    row: &'a PlaylistRow,
    /// The playlist's entries now, by id.
    entries: HashMap<EntryId, &'a EntryRow>,
    /// How they are played.
    settings: PlaySettings,
    /// The user's Liked, read when a track is first found.
    liked: Option<HashSet<String>>,
    /// Each entry looked at, as it plays (watched or not) if it can.
    plays: HashMap<EntryId, Option<PlaylistNext>>,
}

impl<'a> Lookup<'a> {
    fn new(row: &'a PlaylistRow, entries: &'a [EntryRow], settings: PlaySettings) -> Self {
        Self {
            row,
            entries: entries.iter().map(|e| (e.entry_id, e)).collect(),
            settings,
            liked: None,
            plays: HashMap::new(),
        }
    }
}

/// The order `place` plays `entries` in: its round when shuffling, else
/// the playlist's.
fn order_of_place(place: &PlaceRow, entries: &[EntryRow]) -> Vec<EntryId> {
    match &place.round {
        Some(round) => round.clone(),
        None => entries.iter().map(|e| e.entry_id).collect(),
    }
}

/// Where playing starts.
#[derive(Debug, Clone, Copy)]
enum Start {
    /// The entry chosen.
    At(EntryId),
    /// The user's place: it again, unless they finished it.
    Resume(EntryId),
    /// The entry after this one, which ended or was skipped.
    After(EntryId),
}

/// What becomes of a place's round when its settings are chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoundTurn {
    /// Shuffle is off: no round.
    Off,
    /// The round kept as it is.
    Kept,
    /// A new round, `first` at its start.
    New { first: Option<EntryId> },
}

/// The round rule, for `kept` played with `settings`, `from` picked or
/// not: one round per turn-on, kept until shuffle is turned off. A new
/// one when shuffling starts (at the entry playing, so the rest of the
/// playlist still plays) or when there is none, and when an entry is
/// picked, that entry first. Otherwise the kept round.
fn round_turn(kept: &PlaceRow, settings: PlaySettings, from: Option<EntryId>) -> RoundTurn {
    match (settings.shuffle, from) {
        (false, _) => RoundTurn::Off,
        (true, Some(from)) => RoundTurn::New { first: Some(from) },
        (true, None) if !kept.settings.shuffle || kept.round.is_none() => {
            RoundTurn::New { first: kept.entry }
        }
        (true, None) => RoundTurn::Kept,
    }
}

/// A shuffled round of `order` drawn from `seed`, with `first` at its
/// start.
fn round_from(order: &[EntryId], first: Option<EntryId>, seed: u64) -> Vec<EntryId> {
    let first = first.filter(|f| order.contains(f));
    let rest: Vec<EntryId> = order
        .iter()
        .copied()
        .filter(|e| Some(*e) != first)
        .collect();
    let mut round: Vec<EntryId> = first.into_iter().collect();
    round.extend(policy::shuffled(&rest, seed));
    round
}

/// Whether the user's watch state on `item` says watched.
fn watched(item: &Item) -> bool {
    match item {
        Item::Movie(m) => m.playback.watched,
        Item::Episode(e) => e.playback.watched,
        Item::Track(t) => t.playback.watched,
        _ => false,
    }
}

/// `name` trimmed, if it is 1 to [`MAX_NAME`] characters.
fn checked_name(name: &str) -> Result<String, PlaylistsError> {
    let name = name.trim();
    let length = name.chars().count();
    if length == 0 || length > MAX_NAME {
        return Err(PlaylistsError::InvalidInput(format!(
            "a playlist's name is 1 to {MAX_NAME} characters"
        )));
    }
    Ok(name.to_owned())
}

/// A season's episodes, in their order.
fn episodes_of(children: Vec<Item>) -> Vec<Item> {
    let mut episodes: Vec<Item> = children
        .into_iter()
        .filter(|c| matches!(c, Item::Episode(_)))
        .collect();
    episodes.sort_by_key(|e| match e {
        Item::Episode(e) => (e.season, e.episode),
        _ => (u32::MAX, u32::MAX),
    });
    episodes
}

/// An album's tracks, in disc and track order.
fn tracks_of(children: Vec<Item>) -> Vec<Item> {
    let mut tracks: Vec<Item> = children
        .into_iter()
        .filter(|c| matches!(c, Item::Track(_)))
        .collect();
    tracks.sort_by_key(|t| match t {
        Item::Track(t) => (t.disc.unwrap_or(1), t.track.unwrap_or(u32::MAX)),
        _ => (u32::MAX, u32::MAX),
    });
    tracks
}

/// The entry `item` makes.
fn new_entry(item: &Item, external_ids: Vec<String>) -> NewEntry {
    let (kind, show_id, album_id, artist_id) = match item {
        Item::Episode(e) => ("episode", Some(e.show_id.to_string()), None, None),
        Item::Track(t) => (
            "track",
            None,
            Some(t.album_id.to_string()),
            Some(t.artist_id.to_string()),
        ),
        _ => ("movie", None, None, None),
    };
    NewEntry {
        media_id: item.id().to_string(),
        kind: kind.into(),
        show_id,
        album_id,
        artist_id,
        external_ids,
    }
}

pub mod people;
pub mod plex_import;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod builtin_tests;

#[cfg(test)]
mod art_tests;

#[cfg(test)]
mod all_tests;
