//! Server › Optimized (PX-37): the `optimized*` methods, over the copy
//! pool, the profiles in the store, and the admins' on-demand requests.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use jsonrpsee::core::RpcResult;
use spoolframe_admin_api::{
    OptimizedCopy, OptimizedCopyDeleted, OptimizedCopyState, OptimizedDeleted, OptimizedOverview,
    OptimizedProfile, OptimizedProfileWritten, OptimizedProgress, OptimizedRequest,
    OptimizedRequestPut, OptimizedScope, OptimizedScopeKind, OptimizedTrack, error_code,
};
use spoolframe_catalog::{Catalog, CatalogError};
use spoolframe_types::{Item, MediaId};

use super::{AdminApi, error, internal};
use crate::optimized::requests::{Asked, RequestError, Scope};
use crate::optimized::{Deleted, Pool};
use crate::store::optimized::requests::{RequestProgress, ScopeKind};
use crate::store::optimized::{CopyRow, CopyState, Profile, Recipe, TrackRow, parse_video_codec};
use crate::store::sessions::ApiSessionUser;

/// How long the overview waits for the catalog to name the copies'
/// items: past it, the page shows their files' names.
const TITLES: Duration = Duration::from_secs(2);

/// Items as the catalog named them, by media id: a title and an
/// episode's own subtitle.
pub(crate) type Titles = HashMap<String, (String, Option<String>)>;

impl AdminApi {
    /// The copy pool, or why there is none.
    fn pool(&self) -> RpcResult<&Pool> {
        self.optimized.as_deref().ok_or_else(|| {
            error(
                error_code::NOT_CONFIGURED,
                "this server keeps no optimized copies",
            )
        })
    }

    /// The catalog that names the copies' items, if this API has one.
    fn titles_catalog(&self) -> Option<Arc<dyn Catalog>> {
        self.playback
            .as_ref()
            .map(|p| p.catalog.clone())
            .or_else(|| self.library.as_ref().map(|l| l.catalog.clone()))
    }

    pub(super) async fn optimized_view(&self) -> RpcResult<OptimizedOverview> {
        let pool = self.pool()?;
        let store = self.auth.store();
        let profiles = store.optimized_profiles().await.map_err(internal)?;
        let rows = store.optimized_copies().await.map_err(internal)?;
        let titles = self.titles(&rows).await;
        let mut tracks = Vec::with_capacity(rows.len());
        for row in &rows {
            tracks.push(
                store
                    .optimized_copy_tracks(&row.id)
                    .await
                    .map_err(internal)?,
            );
        }
        // How many copies each track is made into.
        let mut uses: HashMap<String, usize> = HashMap::new();
        for track in tracks.iter().flatten() {
            *uses.entry(track.id.clone()).or_default() += 1;
        }
        let mut copies = Vec::with_capacity(rows.len());
        for (row, tracks) in rows.into_iter().zip(tracks) {
            let (title, subtitle) = titles.get(&row.media_id).cloned().unzip();
            // At most one track is made at a time.
            let progress = match row.state {
                CopyState::Making => {
                    pool.progress(&row.id)
                        .await
                        .map_err(internal)?
                        .map(|p| OptimizedProgress {
                            fraction: p.fraction,
                            eta_secs: p.eta.map(|eta| eta.as_secs()),
                        })
                }
                _ => None,
            };
            let tracks = tracks
                .into_iter()
                .map(|t| {
                    let shared = uses.get(&t.id).is_some_and(|&n| n > 1);
                    track_view(t, shared)
                })
                .collect();
            copies.push(copy_view(
                row,
                pool,
                (title, subtitle.flatten()),
                progress,
                tracks,
            ));
        }
        Ok(OptimizedOverview {
            dir: pool.dir().display().to_string(),
            used_bytes: pool.used_bytes().await.map_err(internal)?,
            max_size_bytes: pool.max_size(),
            profiles: profiles.into_iter().map(profile_view).collect(),
            copies,
            requests: self.optimized_request_views().await?,
        })
    }

    /// Every on-demand request, as the page lists it.
    pub(super) async fn optimized_request_views(&self) -> RpcResult<Vec<OptimizedRequest>> {
        let requests = self.pool()?.requests().await.map_err(internal)?;
        let views = requests.into_iter().map(|p| self.request_view(p));
        Ok(futures_util::future::join_all(views).await)
    }

    /// Request `progress` as the page lists it: its scope named by the
    /// catalog within [`TITLES`], and its admin by username.
    async fn request_view(&self, progress: RequestProgress) -> OptimizedRequest {
        let RequestProgress {
            row,
            items,
            done,
            failed,
            size_bytes,
        } = progress;
        let title = match self.titles_catalog() {
            Some(catalog) => tokio::time::timeout(
                TITLES,
                scope_title(&*catalog, row.scope_kind, &row.scope_id),
            )
            .await
            .ok()
            .flatten(),
            None => None,
        };
        let created_by = match row.created_by.parse() {
            Ok(id) => self
                .auth
                .store()
                .user_by_id(id)
                .await
                .ok()
                .flatten()
                .map(|u| u.username),
            Err(_) => None,
        };
        OptimizedRequest {
            id: row.id,
            scope: OptimizedScope {
                kind: match row.scope_kind {
                    ScopeKind::Item => OptimizedScopeKind::Item,
                    ScopeKind::Season => OptimizedScopeKind::Season,
                    ScopeKind::Show => OptimizedScopeKind::Show,
                    ScopeKind::Library => OptimizedScopeKind::Library,
                },
                id: row.scope_id,
            },
            title,
            profile: row.profile,
            pinned: row.pinned,
            created_by,
            created_at: row.created_at,
            items,
            done,
            failed,
            size_bytes,
        }
    }

    pub(super) async fn put_optimized_request(
        &self,
        admin: &ApiSessionUser,
        request: OptimizedRequestPut,
    ) -> RpcResult<OptimizedRequest> {
        let pool = self.pool()?;
        let kind = match request.scope.kind {
            OptimizedScopeKind::Item => ScopeKind::Item,
            OptimizedScopeKind::Season => ScopeKind::Season,
            OptimizedScopeKind::Show => ScopeKind::Show,
            OptimizedScopeKind::Library => ScopeKind::Library,
        };
        let scope = Scope::parse(kind, &request.scope.id).map_err(request_error)?;
        let progress = pool
            .put_request(Asked {
                scope,
                profile: request.profile,
                pinned: request.pinned,
                created_by: admin.user.to_string(),
            })
            .await
            .map_err(request_error)?;
        tracing::info!(request = progress.row.id, by = %admin.user, "an on-demand optimize request by an admin");
        Ok(self.request_view(progress).await)
    }

    pub(super) async fn delete_optimized_request(
        &self,
        admin: &ApiSessionUser,
        id: &str,
    ) -> RpcResult<bool> {
        let gone = self.pool()?.delete_request(id).await.map_err(internal)?;
        tracing::info!(request = id, gone, by = %admin.user, "an on-demand optimize request deleted by an admin");
        Ok(gone)
    }

    pub(super) async fn pin_optimized_request(
        &self,
        admin: &ApiSessionUser,
        id: &str,
        pinned: bool,
    ) -> RpcResult<OptimizedRequest> {
        let progress = self
            .pool()?
            .pin_request(id, pinned)
            .await
            .map_err(request_error)?;
        tracing::info!(request = id, pinned, by = %admin.user, "an on-demand optimize request pinned by an admin");
        Ok(self.request_view(progress).await)
    }

    /// Each item of `rows` the catalog named within [`TITLES`]: its title
    /// and subtitle, as the Playback page shows them. Kept for the items
    /// still in the pool, so a poll asks the catalog only of new ones (and
    /// of those it did not answer for in time).
    async fn titles(&self, rows: &[CopyRow]) -> Titles {
        let Some(catalog) = self.titles_catalog() else {
            return HashMap::new();
        };
        let present: HashSet<&str> = rows.iter().map(|r| r.media_id.as_str()).collect();
        let missing: Vec<&str> = {
            let mut kept = self.optimized_titles.lock();
            kept.retain(|id, _| present.contains(id.as_str()));
            present
                .iter()
                .copied()
                .filter(|id| !kept.contains_key(*id))
                .collect()
        };
        let lookups = missing.into_iter().map(|id| {
            let catalog = catalog.clone();
            async move {
                let parsed: MediaId = id.parse().ok()?;
                let found = tokio::time::timeout(TITLES, catalog.item(&parsed))
                    .await
                    .ok()?
                    .ok()?;
                let shown = crate::playback::Shown::of(&found.item, None);
                Some((id.to_owned(), (shown.title, shown.subtitle)))
            }
        });
        let found = futures_util::future::join_all(lookups).await;
        let mut kept = self.optimized_titles.lock();
        kept.extend(found.into_iter().flatten());
        kept.clone()
    }

    pub(super) async fn put_optimized_profile(
        &self,
        admin: &ApiSessionUser,
        profile: OptimizedProfile,
    ) -> RpcResult<OptimizedProfileWritten> {
        let Some(codec) = parse_video_codec(&profile.video_codec) else {
            return Err(error(
                error_code::INVALID_INPUT,
                format!(
                    "{} is not a codec a copy is made in: use h264, hevc or av1",
                    profile.video_codec
                ),
            ));
        };
        let profile = Profile {
            name: profile.name,
            video_codec: codec,
            height: profile.height,
            max_bitrate: profile.max_bitrate,
        };
        if let Some(problem) = profile.problem() {
            return Err(error(error_code::INVALID_INPUT, problem));
        }
        let written = self
            .auth
            .store()
            .put_optimized_profile(&profile)
            .await
            .map_err(internal)?;
        // Its on-demand requests' copies are made again with its new
        // video (the user's decision, 2026-10-20).
        let requeued = match (&self.optimized, written.changed) {
            (Some(pool), true) => pool.remake(&profile.name).await.map_err(internal)?,
            _ => 0,
        };
        tracing::info!(
            profile = profile.name,
            codec = %profile.video_codec,
            height = profile.height,
            max_bitrate = profile.max_bitrate,
            created = written.created,
            outdated = written.outdated,
            requeued,
            by = %admin.user,
            "an optimized profile written by an admin"
        );
        Ok(OptimizedProfileWritten {
            created: written.created,
            outdated: written.outdated,
            requeued,
        })
    }

    pub(super) async fn delete_optimized_profile(
        &self,
        admin: &ApiSessionUser,
        name: &str,
    ) -> RpcResult<u64> {
        let outdated = self
            .auth
            .store()
            .delete_optimized_profile(name)
            .await
            .map_err(internal)?
            .ok_or_else(|| {
                error(
                    error_code::INVALID_INPUT,
                    format!("there is no profile {name}"),
                )
            })?;
        tracing::info!(
            profile = name,
            outdated,
            by = %admin.user,
            "an optimized profile removed by an admin"
        );
        Ok(outdated)
    }

    pub(super) async fn delete_outdated_copies(
        &self,
        admin: &ApiSessionUser,
        profile: Option<&str>,
    ) -> RpcResult<OptimizedDeleted> {
        let done = self
            .pool()?
            .delete_outdated(profile)
            .await
            .map_err(internal)?;
        tracing::info!(
            profile,
            now = done.now,
            deferred = done.deferred,
            by = %admin.user,
            "outdated optimized copies deleted by an admin"
        );
        Ok(OptimizedDeleted {
            now: done.now,
            deferred: done.deferred,
        })
    }

    pub(super) async fn delete_optimized_copy(
        &self,
        admin: &ApiSessionUser,
        id: &str,
    ) -> RpcResult<OptimizedCopyDeleted> {
        let deleted = self.pool()?.delete(id).await.map_err(internal)?;
        tracing::info!(copy = id, ?deleted, by = %admin.user, "an optimized copy deleted by an admin");
        Ok(match deleted {
            Deleted::Now => OptimizedCopyDeleted::Now,
            Deleted::Deferred => OptimizedCopyDeleted::Deferred,
            Deleted::NotFound => OptimizedCopyDeleted::NotFound,
        })
    }
}

fn profile_view(profile: Profile) -> OptimizedProfile {
    OptimizedProfile {
        name: profile.name,
        video_codec: profile.video_codec.as_str().to_owned(),
        height: profile.height,
        max_bitrate: profile.max_bitrate,
    }
}

fn state_view(state: CopyState) -> OptimizedCopyState {
    match state {
        CopyState::Waiting => OptimizedCopyState::Waiting,
        CopyState::Making => OptimizedCopyState::Making,
        CopyState::Ready => OptimizedCopyState::Ready,
        CopyState::Failed => OptimizedCopyState::Failed,
    }
}

/// Track `track` as the page lists it under its copy; `shared` when
/// another copy is made with it too.
fn track_view(track: TrackRow, shared: bool) -> OptimizedTrack {
    let (kind, label) = match track.recipe {
        Recipe::Video(v) => (
            "video",
            match v.max_bitrate {
                Some(rate) => format!("{} {}p {} kbit/s", v.codec, v.height, rate / 1000),
                None => format!("{} {}p", v.codec, v.height),
            },
        ),
        Recipe::Audio(a) => (
            "audio",
            format!("{} {} ch {} kbit/s", a.codec, a.channels, a.bitrate / 1000),
        ),
    };
    OptimizedTrack {
        id: track.id,
        kind: kind.to_owned(),
        label,
        state: state_view(track.state),
        size_bytes: track.size_bytes,
        shared,
    }
}

fn copy_view(
    row: CopyRow,
    pool: &Pool,
    (title, subtitle): (Option<String>, Option<String>),
    progress: Option<OptimizedProgress>,
    tracks: Vec<OptimizedTrack>,
) -> OptimizedCopy {
    OptimizedCopy {
        reading: pool.is_read(&row.id),
        id: row.id,
        media_id: row.media_id,
        title,
        subtitle,
        path: row.media_path,
        target: row.target.label(),
        outdated: row.outdated,
        state: state_view(row.state),
        size_bytes: row.size_bytes,
        failure: row.failure,
        requested_at: row.requested_at,
        made_at: row.made_at,
        last_used_at: row.last_used_at,
        progress,
        tracks,
        pinned: row.pinned,
    }
}

/// `why` a request was refused, as the API says it.
fn request_error(why: RequestError) -> jsonrpsee::types::ErrorObjectOwned {
    match why {
        RequestError::Store(e) => internal(e),
        RequestError::Catalog(CatalogError::NotFound(_) | CatalogError::ForeignId(_)) => {
            error(error_code::INVALID_INPUT, why.to_string())
        }
        RequestError::Catalog(_) => error(error_code::UNAVAILABLE, why.to_string()),
        RequestError::NoProfile(_)
        | RequestError::Scope(_)
        | RequestError::OverBudget { .. }
        | RequestError::NotFound => error(error_code::INVALID_INPUT, why.to_string()),
    }
}

/// Scope `id` of `kind` as the pages name it: a film or episode as the
/// Playback page does, a season with its show, a show, a library.
async fn scope_title(catalog: &dyn Catalog, kind: ScopeKind, id: &str) -> Option<String> {
    if kind == ScopeKind::Library {
        return catalog
            .libraries()
            .await
            .ok()?
            .into_iter()
            .find(|l| l.id.to_string() == id)
            .map(|l| l.title);
    }
    let id: MediaId = id.parse().ok()?;
    match catalog.item(&id).await.ok()?.item {
        Item::Season(season) => Some(match season.show_title {
            Some(show) => format!("{show} · {}", season.title),
            None => season.title,
        }),
        Item::Show(show) => Some(show.title),
        item => {
            let shown = crate::playback::Shown::of(&item, None);
            Some(match shown.subtitle {
                Some(subtitle) => format!("{} · {subtitle}", shown.title),
                None => shown.title,
            })
        }
    }
}
