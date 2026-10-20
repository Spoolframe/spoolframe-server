//! On-demand requests (PX-37, the user's decision 2026-10-20): an admin
//! asks for a film, an episode, a season, a show or a library to be
//! optimized in a profile. Its films and episodes are asked of the pool as
//! any copy is, in [`Lane::Background`]: behind live playback, downloads
//! and binge-ahead, and yielding to live playback as every track does.
//!
//! A request is a kept rule: its items are kept in the store, and the
//! asker looks at each rule's scope again every [`FOLLOW`] and each time
//! the catalog is replaced, so an episode a season gains is asked for
//! too. An item is asked for once: its copy, once evicted, is not made
//! again, unless its profile's video changes ([`Pool::remake`]), which
//! asks for every item of the profile's requests anew.
//!
//! A pinned request's copies are never evicted (the sweep skips their
//! tracks), and count against `optimized.max_size_mb`: a pin is refused
//! when the pinned requests' expected size would pass it. Deleting a
//! request unpins its copies; they stay, evicted as any other.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use spoolframe_catalog::{Catalog, CatalogError, Page};
use spoolframe_types::{Item, LibraryId, LibraryKind, MediaId, MediaKind};
use tokio::sync::watch;

use super::{Inner, Lane, Pool, stopped};

use crate::store::StoreError;
use crate::store::optimized::requests::{NewRequest, RequestProgress, ScopeKind, UnaskedItem};
use crate::store::optimized::{CopyTarget, CopyVideo, NewCopy, Profile};

/// How often the asker looks at every kept rule's scope again for the
/// items it gained: the catalog says nothing when a library gains an
/// episode.
pub const FOLLOW: Duration = Duration::from_secs(60 * 60);

/// The catalog pages a library is read in.
const PAGE: u32 = 200;

/// What a request covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// A film or an episode.
    Item(MediaId),
    Season(MediaId),
    Show(MediaId),
    Library(LibraryId),
}

impl Scope {
    /// The scope `id` names as `kind`: refused, saying why, for an id that
    /// is not one, or of another kind.
    pub fn parse(kind: ScopeKind, id: &str) -> Result<Self, RequestError> {
        let bad = |why: &str| RequestError::Scope(format!("{id} {why}"));
        if kind == ScopeKind::Library {
            return id
                .parse()
                .map(Self::Library)
                .map_err(|_| bad("is not a library id"));
        }
        let media: MediaId = id.parse().map_err(|_| bad("is not a media id"))?;
        match (kind, media.kind()) {
            (ScopeKind::Item, MediaKind::Movie | MediaKind::Episode) => Ok(Self::Item(media)),
            (ScopeKind::Season, MediaKind::Season) => Ok(Self::Season(media)),
            (ScopeKind::Show, MediaKind::Show) => Ok(Self::Show(media)),
            (ScopeKind::Item, _) => Err(bad("is not a film or an episode")),
            (ScopeKind::Season, _) => Err(bad("is not a season")),
            (ScopeKind::Show, _) => Err(bad("is not a show")),
            (ScopeKind::Library, _) => unreachable!("read as a library id above"),
        }
    }

    pub fn kind(&self) -> ScopeKind {
        match self {
            Self::Item(_) => ScopeKind::Item,
            Self::Season(_) => ScopeKind::Season,
            Self::Show(_) => ScopeKind::Show,
            Self::Library(_) => ScopeKind::Library,
        }
    }

    /// Its id, as the store keeps it.
    pub fn id(&self) -> String {
        match self {
            Self::Item(id) | Self::Season(id) | Self::Show(id) => id.to_string(),
            Self::Library(id) => id.to_string(),
        }
    }
}

/// An admin's on-demand request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    pub scope: Scope,
    /// A profile's name.
    pub profile: String,
    pub pinned: bool,
    /// The admin's user id.
    pub created_by: String,
}

/// Why a request was refused.
#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    #[error("there is no profile {0}")]
    NoProfile(String),
    /// What its scope is wrong for, said for the admin.
    #[error("{0}")]
    Scope(String),
    #[error(
        "the pinned copies would not fit the budget: this request is expected to take about \
         {}, the other pinned requests about {}, and optimized.max_size_mb is {}. Leave it \
         unpinned, unpin or delete another pinned request, or raise optimized.max_size_mb",
        size(*.wanted),
        size(*.pinned),
        size(*.max)
    )]
    OverBudget { wanted: u64, pinned: u64, max: u64 },
    #[error("there is no such request")]
    NotFound,
    #[error("the catalog cannot say what it holds: {0}")]
    Catalog(#[from] CatalogError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// `bytes` for the admin, in GiB to one decimal.
fn size(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / f64::from(1u32 << 30))
}

/// A film or an episode a scope holds, and its length if the catalog
/// knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Covered {
    id: MediaId,
    length: Option<Duration>,
}

impl Covered {
    /// `item`, if it is a film or an episode.
    fn of(item: &Item) -> Option<Self> {
        match item {
            Item::Movie(m) => Some(Self {
                id: m.id.clone(),
                length: m.duration,
            }),
            Item::Episode(e) => Some(Self {
                id: e.id.clone(),
                length: e.duration,
            }),
            _ => None,
        }
    }
}

/// The films and episodes `scope` holds now.
async fn expand(catalog: &dyn Catalog, scope: &Scope) -> Result<Vec<Covered>, RequestError> {
    match scope {
        Scope::Item(id) => {
            let found = catalog.item(id).await?;
            Covered::of(&found.item)
                .map(|c| vec![c])
                .ok_or_else(|| RequestError::Scope(format!("{id} is not a film or an episode")))
        }
        Scope::Season(id) => Ok(catalog
            .item(id)
            .await?
            .children
            .iter()
            .filter_map(Covered::of)
            .collect()),
        Scope::Show(id) => Ok(catalog
            .episodes(id)
            .await?
            .iter()
            .filter_map(Covered::of)
            .collect()),
        Scope::Library(id) => {
            let library = catalog
                .libraries()
                .await?
                .into_iter()
                .find(|l| l.id == *id)
                .ok_or_else(|| RequestError::Scope(format!("there is no library {id}")))?;
            if library.kind == LibraryKind::Music {
                return Err(RequestError::Scope(format!(
                    "{} is a music library: it has no films or episodes to optimize",
                    library.title
                )));
            }
            let mut tops = Vec::new();
            loop {
                let page = catalog
                    .items(
                        id,
                        Page {
                            offset: tops.len() as u32,
                            limit: PAGE,
                        },
                    )
                    .await?;
                let got = page.items.len();
                tops.extend(page.items);
                if got == 0 || tops.len() >= page.total as usize {
                    break;
                }
            }
            let mut covered = Vec::new();
            for top in &tops {
                match top {
                    Item::Show(show) => covered.extend(
                        catalog
                            .episodes(&show.id)
                            .await?
                            .iter()
                            .filter_map(Covered::of),
                    ),
                    other => covered.extend(Covered::of(other)),
                }
            }
            Ok(covered)
        }
    }
}

/// What copies of `covered` in `profile` are expected to take: its
/// bitrate over their lengths, a tenth more for the container and the
/// audio, as a track being made reserves. An item of no known length
/// counts nothing.
fn estimate(profile: &Profile, covered: &[Covered]) -> u64 {
    let millis: u128 = covered
        .iter()
        .filter_map(|c| c.length)
        .map(|l| l.as_millis())
        .sum();
    let bits = u128::from(profile.max_bitrate) * 11 / 10 * millis / 1000;
    u64::try_from(bits / 8).unwrap_or(u64::MAX)
}

impl Pool {
    /// Optimize `asked.scope` in `asked.profile`: the request for that
    /// scope and profile, made now or found and its pin set as asked,
    /// with the films and episodes the scope holds now among its items.
    /// The pool is asked for them in the background. A pin is refused
    /// when the pinned requests, this one with them, are expected to pass
    /// the budget.
    pub async fn put_request(&self, asked: Asked) -> Result<RequestProgress, RequestError> {
        let inner = &self.inner;
        let profile = inner
            .store
            .optimized_profile(&asked.profile)
            .await?
            .ok_or_else(|| RequestError::NoProfile(asked.profile.clone()))?;
        let covered = expand(&*inner.catalog, &asked.scope).await?;
        let estimated_bytes = estimate(&profile, &covered);
        let scope_id = asked.scope.id();
        let kind = asked.scope.kind();
        if asked.pinned {
            let before = inner
                .store
                .optimized_request_for(kind, &scope_id, &profile.name)
                .await?;
            inner
                .fits_pinned(before.as_ref().map(|r| r.id.as_str()), estimated_bytes)
                .await?;
        }
        let ids: Vec<String> = covered.iter().map(|c| c.id.to_string()).collect();
        let (row, created) = inner
            .store
            .put_optimized_request(
                &NewRequest {
                    scope_kind: kind,
                    scope_id: &scope_id,
                    profile: &profile.name,
                    pinned: asked.pinned,
                    estimated_bytes,
                    created_by: &asked.created_by,
                },
                &ids,
            )
            .await?;
        tracing::info!(
            request = row.id,
            scope = scope_id,
            kind = kind.as_str(),
            profile = profile.name,
            pinned = row.pinned,
            items = ids.len(),
            created,
            "an on-demand optimize request"
        );
        inner.ask.notify_one();
        // An unpinned request's copies may be evicted now.
        inner.changed();
        self.request_progress(&row.id)
            .await?
            .ok_or(RequestError::NotFound)
    }

    /// Pin or unpin request `id`: a pin refused as [`Pool::put_request`]
    /// refuses one.
    pub async fn pin_request(
        &self,
        id: &str,
        pinned: bool,
    ) -> Result<RequestProgress, RequestError> {
        let inner = &self.inner;
        let row = inner
            .store
            .optimized_request(id)
            .await?
            .ok_or(RequestError::NotFound)?;
        if pinned && !row.pinned {
            inner.fits_pinned(Some(id), row.estimated_bytes).await?;
        }
        inner.store.set_optimized_request_pinned(id, pinned).await?;
        tracing::info!(
            request = id,
            pinned,
            "an on-demand optimize request pinned or unpinned"
        );
        // Unpinned copies may be over the budget now.
        inner.changed();
        self.request_progress(id)
            .await?
            .ok_or(RequestError::NotFound)
    }

    /// Delete request `id`: no item is asked for it from now on, and its
    /// copies stay, no longer pinned by it, evicted as any other copy.
    /// Whether it was there.
    pub async fn delete_request(&self, id: &str) -> Result<bool, StoreError> {
        let gone = self.inner.store.delete_optimized_request(id).await?;
        if gone {
            tracing::info!(request = id, "an on-demand optimize request deleted");
            self.inner.changed();
        }
        Ok(gone)
    }

    /// Every request with how far it is, newest first.
    pub async fn requests(&self) -> Result<Vec<RequestProgress>, StoreError> {
        self.inner.store.optimized_request_progress().await
    }

    /// Request `id` with how far it is.
    pub async fn request_progress(&self, id: &str) -> Result<Option<RequestProgress>, StoreError> {
        Ok(self.requests().await?.into_iter().find(|p| p.row.id == id))
    }

    /// Profile `profile`'s video changed: every item of its requests is
    /// asked for again, made with its new video. How many.
    pub async fn remake(&self, profile: &str) -> Result<u64, StoreError> {
        let inner = &self.inner;
        let mut requeued = 0;
        for request in inner.store.optimized_requests_in(profile).await? {
            requeued += inner
                .store
                .requeue_optimized_request_items(&request.id)
                .await?;
        }
        if requeued > 0 {
            tracing::info!(
                profile,
                items = requeued,
                "a profile's video changed: its on-demand items are made again"
            );
            inner.ask.notify_one();
        }
        Ok(requeued)
    }

    /// Look at every kept rule's scope now, as the asker does every
    /// [`FOLLOW`], and ask for what is not asked for yet: how many items
    /// the rules gained.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn follow_requests(&self) -> Result<u64, RequestError> {
        let gained = self.inner.follow().await?;
        self.inner.ask.notify_one();
        Ok(gained)
    }
}

impl Inner {
    /// Refuse `wanted` more pinned bytes when the pinned requests but
    /// `except` already expect so much that the budget would not hold it.
    async fn fits_pinned(&self, except: Option<&str>, wanted: u64) -> Result<(), RequestError> {
        let pinned = self.store.pinned_optimized_bytes(except).await?;
        if pinned.saturating_add(wanted) > self.max_size {
            return Err(RequestError::OverBudget {
                wanted,
                pinned,
                max: self.max_size,
            });
        }
        Ok(())
    }

    /// The asker: ask the pool for each item not asked for, and look at
    /// the kept rules' scopes at start, every [`FOLLOW`] and whenever the
    /// catalog is replaced, until the pool stops.
    pub(super) async fn ask_for_requests(
        self: Arc<Self>,
        mut changes: Option<watch::Receiver<u64>>,
    ) {
        let mut stop = self.stop.subscribe();
        let mut follow = true;
        while !*stop.borrow() {
            if std::mem::take(&mut follow) {
                match self.follow().await {
                    Ok(0) => {}
                    Ok(gained) => {
                        tracing::info!(items = gained, "on-demand requests gained items");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "cannot look at what the on-demand requests cover");
                    }
                }
            }
            let wait = match self.ask_unasked(&mut stop).await {
                Ok(()) => FOLLOW,
                Err(e) => {
                    tracing::warn!(error = %e, "cannot ask for the on-demand requests' copies");
                    self.retry
                }
            };
            let changed = async {
                match changes.as_mut() {
                    Some(changes) => {
                        if changes.changed().await.is_err() {
                            std::future::pending::<()>().await;
                        }
                    }
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                () = self.ask.notified() => {}
                () = changed => follow = true,
                () = tokio::time::sleep(wait) => follow = wait == FOLLOW,
                () = stopped(&mut stop) => {}
            }
        }
    }

    /// Add to each kept rule the films and episodes its scope holds now:
    /// how many it gained. A rule whose scope cannot be read now is left
    /// as it is.
    async fn follow(&self) -> Result<u64, StoreError> {
        let mut gained = 0;
        for request in self.store.optimized_requests().await? {
            if request.scope_kind == ScopeKind::Item {
                continue;
            }
            let covered = match Scope::parse(request.scope_kind, &request.scope_id) {
                Ok(scope) => expand(&*self.catalog, &scope).await,
                Err(e) => Err(e),
            };
            match covered {
                Ok(covered) => {
                    let ids: Vec<String> = covered.iter().map(|c| c.id.to_string()).collect();
                    gained += self
                        .store
                        .add_optimized_request_items(&request.id, &ids)
                        .await?;
                }
                Err(e) => {
                    tracing::warn!(request = request.id, scope = request.scope_id, error = %e, "cannot look at what an on-demand request covers");
                }
            }
        }
        Ok(gained)
    }

    /// Ask the pool for each item not asked for yet, until the pool
    /// stops. One the catalog cannot answer for now is left for the next
    /// time; one with no file or profile is noted as failed.
    async fn ask_unasked(&self, stop: &mut watch::Receiver<bool>) -> Result<(), StoreError> {
        let mut profiles: HashMap<String, Option<Profile>> = HashMap::new();
        for item in self.store.unasked_optimized_request_items().await? {
            if *stop.borrow() {
                break;
            }
            let profile = match profiles.get(&item.profile) {
                Some(profile) => profile.clone(),
                None => {
                    let profile = self.store.optimized_profile(&item.profile).await?;
                    profiles.insert(item.profile.clone(), profile.clone());
                    profile
                }
            };
            let asked = match profile {
                Some(profile) => self.ask_item(&item, &profile).await,
                None => Err(Failed::Kept(format!(
                    "there is no profile {}",
                    item.profile
                ))),
            };
            let (copy, failure) = match asked {
                Ok(copy) => (Some(copy), None),
                Err(Failed::Kept(why)) => {
                    tracing::warn!(
                        request = item.request_id,
                        media = item.media_id,
                        reason = why,
                        "an on-demand item cannot be optimized"
                    );
                    (None, Some(why))
                }
                Err(Failed::Later(e)) => {
                    tracing::warn!(request = item.request_id, media = item.media_id, error = %e, "an on-demand item waits for the catalog");
                    continue;
                }
                Err(Failed::Store(e)) => return Err(e),
            };
            self.store
                .optimized_request_item_asked(&item, copy.as_deref(), failure.as_deref())
                .await?;
        }
        Ok(())
    }

    /// Ask the pool for `item`'s copy in `profile`, behind every other
    /// kind of copy: its id.
    async fn ask_item(&self, item: &UnaskedItem, profile: &Profile) -> Result<String, Failed> {
        let id: MediaId = item
            .media_id
            .parse()
            .map_err(|_| Failed::Kept(format!("{} is not a media id", item.media_id)))?;
        let path = match self.catalog.media_path(&id).await {
            Ok(path) => path,
            Err(
                e @ (CatalogError::NotFound(_)
                | CatalogError::NotPlayable(_)
                | CatalogError::MediaFileUnavailable(_)
                | CatalogError::ForeignId(_)),
            ) => return Err(Failed::Kept(e.to_string())),
            Err(e) => return Err(Failed::Later(e)),
        };
        let unreadable =
            |e: &dyn std::fmt::Display| Failed::Kept(format!("the media file cannot be read: {e}"));
        let source_stamp = crate::files::stamp_text(&path)
            .await
            .map_err(|e| unreadable(&e))?;
        let descriptor = self
            .prober
            .describe(Path::new(&path))
            .await
            .map_err(|e| unreadable(&e))?;
        let video = descriptor
            .video
            .as_ref()
            .ok_or_else(|| Failed::Kept("the media file has no video".to_owned()))?;
        let copy = NewCopy {
            media_id: item.media_id.clone(),
            media_path: path.display().to_string(),
            source_stamp,
            target: CopyTarget::Profile(profile.name.clone()),
            video: CopyVideo::of_profile(profile, video.height, video.hdr.is_some()),
        };
        let row = self
            .request(copy, Lane::Background)
            .await
            .map_err(Failed::Store)?;
        Ok(row.id)
    }
}

/// Why an item was not asked for.
enum Failed {
    /// For good, until its request is asked again: said for the admin.
    Kept(String),
    /// The catalog cannot say now: tried again later.
    Later(CatalogError),
    Store(StoreError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scope_of_the_wrong_kind_is_refused() {
        let err = Scope::parse(ScopeKind::Season, "plex:show:10").unwrap_err();
        assert_eq!(err.to_string(), "plex:show:10 is not a season");
    }

    #[test]
    fn an_item_scope_is_a_film_or_an_episode() {
        assert!(Scope::parse(ScopeKind::Item, "plex:episode:12").is_ok());
        assert!(Scope::parse(ScopeKind::Item, "plex:season:11").is_err());
    }

    #[test]
    fn an_estimate_is_the_profile_s_bitrate_over_the_lengths_and_a_tenth() {
        let profile = Profile {
            name: "Mobile".into(),
            video_codec: spoolframe_types::VideoCodec::H264,
            height: 1080,
            max_bitrate: 8_000_000,
        };
        let covered = [Covered {
            id: "plex:movie:1".parse().unwrap(),
            length: Some(Duration::from_secs(100)),
        }];
        // 8 Mbit/s for 100 s is 100 MB; a tenth more.
        assert_eq!(estimate(&profile, &covered), 110_000_000);
    }
}
