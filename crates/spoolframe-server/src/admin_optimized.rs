//! Server › Optimized (PX-37): the `optimized*` methods, over the copy
//! pool and the profiles in the store.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use jsonrpsee::core::RpcResult;
use spoolframe_admin_api::{
    OptimizedCopy, OptimizedCopyDeleted, OptimizedCopyState, OptimizedDeleted, OptimizedOverview,
    OptimizedProfile, OptimizedProfileWritten, OptimizedProgress, error_code,
};
use spoolframe_catalog::Catalog;
use spoolframe_types::MediaId;

use super::{AdminApi, error, internal};
use crate::optimized::{Deleted, Pool};
use crate::store::optimized::{CopyRow, CopyState, CopyTarget, Profile, parse_video_codec};
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
        // At most one copy is made at a time.
        let running = pool.running_progress();
        let titles = self.titles(&rows).await;
        Ok(OptimizedOverview {
            dir: pool.dir().display().to_string(),
            used_bytes: pool.used_bytes(&rows).await,
            max_size_bytes: pool.max_size(),
            profiles: profiles.into_iter().map(profile_view).collect(),
            copies: rows
                .into_iter()
                .map(|row| {
                    let (title, subtitle) = titles.get(&row.media_id).cloned().unzip();
                    let progress = running
                        .as_ref()
                        .filter(|(id, ..)| *id == row.id && row.state == CopyState::Making)
                        .map(|(_, fraction, eta)| OptimizedProgress {
                            fraction: *fraction,
                            eta_secs: eta.map(|eta| eta.as_secs()),
                        });
                    copy_view(row, pool, (title, subtitle.flatten()), progress)
                })
                .collect(),
        })
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
        tracing::info!(
            profile = profile.name,
            codec = %profile.video_codec,
            height = profile.height,
            max_bitrate = profile.max_bitrate,
            created = written.created,
            outdated = written.outdated,
            by = %admin.user,
            "an optimized profile written by an admin"
        );
        Ok(OptimizedProfileWritten {
            created: written.created,
            outdated: written.outdated,
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

fn copy_view(
    row: CopyRow,
    pool: &Pool,
    (title, subtitle): (Option<String>, Option<String>),
    progress: Option<OptimizedProgress>,
) -> OptimizedCopy {
    OptimizedCopy {
        reading: pool.is_read(&row.id),
        id: row.id,
        media_id: row.media_id,
        title,
        subtitle,
        path: row.media_path,
        target: match row.target {
            CopyTarget::Profile(name) => name,
            CopyTarget::Ahead => "ahead".to_owned(),
        },
        outdated: row.outdated,
        state: match row.state {
            CopyState::Waiting => OptimizedCopyState::Waiting,
            CopyState::Making => OptimizedCopyState::Making,
            CopyState::Ready => OptimizedCopyState::Ready,
            CopyState::Failed => OptimizedCopyState::Failed,
        },
        size_bytes: row.size_bytes,
        failure: row.failure,
        requested_at: row.requested_at,
        made_at: row.made_at,
        last_used_at: row.last_used_at,
        progress,
    }
}
