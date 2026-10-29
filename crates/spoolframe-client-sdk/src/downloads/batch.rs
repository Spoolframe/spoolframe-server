//! A season or a show downloaded at once: each episode started as its own
//! download, one failing never stopping the rest.

use spoolframe_protocol::downloads::{DownloadSource, DownloadTracks};
use spoolframe_types::{Item, MediaId};

use super::{Downloads, LocalDownload, Shared};
use crate::{SdkError, Session};

/// Why [`Downloads::plan_children`] leaves an episode out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum SkipReason {
    /// Watched, with only the unwatched asked for.
    Watched,
    /// The user has it downloaded here already, or under way.
    AlreadyHere,
}

/// An episode [`Downloads::plan_children`] leaves out.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct SkippedChild {
    pub item: MediaId,
    pub reason: SkipReason,
}

/// An episode, or a season whose episodes could not be listed, that a
/// batch could not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedChild {
    pub item: MediaId,
    pub error: SdkError,
}

/// What downloading a season or a show would start
/// ([`Downloads::plan_children`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChildPlan {
    /// The episodes to start, by season and episode.
    pub to_start: Vec<Item>,
    pub skipped: Vec<SkippedChild>,
    /// A show's seasons whose episodes could not be listed.
    pub failed: Vec<FailedChild>,
}

/// What [`Downloads::start_children`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchStart {
    pub started: Vec<LocalDownload>,
    pub skipped: Vec<SkippedChild>,
    pub failed: Vec<FailedChild>,
}

impl Downloads {
    /// What downloading show or season `id` as `session`'s user would
    /// start: every episode under it, by season and episode, but those
    /// the user has here already and, with `only_unwatched`, those
    /// watched. Asks the server; starts nothing. A show's season whose
    /// episodes cannot be listed is in `failed`, and the others are
    /// planned.
    pub async fn plan_children(
        &self,
        session: &Session,
        id: &MediaId,
        only_unwatched: bool,
    ) -> Result<ChildPlan, SdkError> {
        let mut plan = ChildPlan::default();
        let detail = session.item(id).await?;
        let episodes = match &detail.item {
            Item::Season(_) => detail.children,
            Item::Show(_) => {
                let mut episodes = Vec::new();
                for season in detail.children {
                    match session.item(season.id()).await {
                        Ok(season) => episodes.extend(season.children),
                        Err(error) => plan.failed.push(FailedChild {
                            item: season.id().clone(),
                            error,
                        }),
                    }
                }
                episodes
            }
            _ => {
                return Err(SdkError::InvalidInput(format!(
                    "{id}: only a show or a season has episodes to download"
                )));
            }
        };
        let here = here(self.shared(), &session.server(), &session.user().user_id);
        for episode in episodes {
            let Item::Episode(e) = &episode else {
                continue;
            };
            let reason = if here.contains(&e.id) {
                Some(SkipReason::AlreadyHere)
            } else if only_unwatched && e.playback.watched {
                Some(SkipReason::Watched)
            } else {
                None
            };
            match reason {
                Some(reason) => plan.skipped.push(SkippedChild {
                    item: e.id.clone(),
                    reason,
                }),
                None => plan.to_start.push(episode),
            }
        }
        plan.to_start.sort_by_key(|item| match item {
            Item::Episode(e) => (e.season, e.episode),
            _ => (0, 0),
        });
        Ok(plan)
    }

    /// Download show or season `id` as `session`'s user from `source`:
    /// each episode [`Downloads::plan_children`] plans is started with
    /// every track, as its own download. One failing to start is in
    /// `failed` and the rest go on.
    pub async fn start_children(
        &self,
        session: &Session,
        id: &MediaId,
        source: DownloadSource,
        only_unwatched: bool,
    ) -> Result<BatchStart, SdkError> {
        let plan = self.plan_children(session, id, only_unwatched).await?;
        let mut batch = BatchStart {
            skipped: plan.skipped,
            failed: plan.failed,
            ..BatchStart::default()
        };
        for episode in plan.to_start {
            match self
                .start(session, episode.id(), source.clone(), DownloadTracks::All)
                .await
            {
                Ok(started) => batch.started.push(started),
                Err(error) => batch.failed.push(FailedChild {
                    item: episode.id().clone(),
                    error,
                }),
            }
        }
        Ok(batch)
    }
}

/// The items `user_id` has downloaded on `server` here, finished or not.
fn here(shared: &Shared, server: &str, user_id: &str) -> Vec<MediaId> {
    shared
        .store
        .as_ref()
        .map(|store| {
            store
                .scan()
                .into_iter()
                .filter(|m| m.server == server && m.held_by(user_id))
                .map(|m| m.item.id().clone())
                .collect()
        })
        .unwrap_or_default()
}
