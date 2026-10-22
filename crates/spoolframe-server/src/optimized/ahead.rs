//! Binge-ahead (PX-37): while a viewer watches an episode whose playback
//! encodes its video, the pool makes a copy of the show's next episode
//! with that playback's own plan (codec, height, bitrate, tone map), so
//! the next episode looks the same and plays from the copy, with no live
//! encode.
//!
//! What is possible is decided as the playback starts: a film, a special,
//! the show's last episode, and a playback that neither encodes its video
//! nor plays a copy (a remux, a conversion of the audio alone) get no
//! watcher. The next episode is found then, once.
//!
//! A watcher asks for the copy once, when the playback's encode has caught
//! up ([`Conversion::caught_up`]: its read-ahead is full, or it wrote to
//! the end), so the copy takes only what the budget has spare once the
//! episode plays smoothly; the pool's yielding holds give its place back
//! to any live playback. It waits for the encode to catch up
//! ([`Conversion::caught_up_signal`]), and looks again every [`LOOK`]
//! besides: a quality or subtitle change gives the playback a new plan and
//! encode, and says so to nobody. Each look counts as the viewer playing
//! the show, which keeps the copy wanted ([`Pool::played`],
//! [`Pool::cancel_unplayed`]).
//!
//! The chain goes on through the copies: an episode played from its
//! `ahead` copy asks at its first look (nothing encodes, so there is
//! nothing to catch up) for the next one with that copy's own video, so
//! a whole binge plays from copies, one episode ahead. A playback from a
//! profile's copy asks for nothing.
//!
//! [`Conversion::caught_up`]: crate::encode::Conversion::caught_up
//! [`Conversion::caught_up_signal`]: crate::encode::Conversion::caught_up_signal

use std::sync::{Arc, Weak};
use std::time::Duration;

use spoolframe_catalog::{Catalog, CatalogError};
use spoolframe_protocol::{PlaybackHandle, PlaybackPlan};
use spoolframe_types::{Episode, MediaId};

use super::Pool;
use crate::encode::CaughtUp;
use crate::playback::Playbacks;
use crate::store::optimized::{CopyRow, CopyTarget, CopyVideo, NewCopy};

/// How often a watcher looks at its playback when its encode says
/// nothing: for a new plan, and to keep the viewer's copy wanted.
pub const LOOK: Duration = Duration::from_secs(30);

/// What binge-ahead sees of a playback at one look.
#[derive(Debug, Clone)]
pub struct Seen {
    /// The plan it plays by now.
    pub plan: PlaybackPlan,
    /// Its encode has caught up with its reader.
    pub caught_up: bool,
    /// The optimized copy it plays in the film's place, by id.
    pub copy: Option<String>,
    /// When its encode catches up, if it has one.
    pub signal: Option<CaughtUp>,
}

impl Seen {
    /// Whether a watcher may ask for anything for it: it plays a copy, or
    /// encodes its video.
    fn may_ask(&self) -> bool {
        self.copy.is_some() || CopyVideo::of_plan(&self.plan).is_some()
    }
}

/// The episode a playback plays, as binge-ahead needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Played {
    pub id: MediaId,
    pub show_id: MediaId,
    pub season: u32,
    pub number: u32,
}

impl From<&Episode> for Played {
    fn from(episode: &Episode) -> Self {
        Self {
            id: episode.id.clone(),
            show_id: episode.show_id.clone(),
            season: episode.season,
            number: episode.episode,
        }
    }
}

/// One episode's playback, watched for its viewer.
pub struct Watcher {
    catalog: Arc<dyn Catalog>,
    /// The viewer's user id.
    viewer: String,
    show: String,
    /// The episode after the one played.
    next: MediaId,
    /// It asked for the next episode's copy, or found it asks for none.
    done: bool,
}

impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watcher")
            .field("viewer", &self.viewer)
            .field("next", &self.next)
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

impl Watcher {
    /// A watcher of `played` for `viewer`: `None` for a special or the
    /// show's last episode, which have no next one.
    pub async fn new(catalog: Arc<dyn Catalog>, viewer: String, played: &Played) -> Option<Self> {
        let next = match next_episode(&*catalog, played).await {
            Ok(next) => next?,
            Err(e) => {
                tracing::warn!(episode = %played.id, error = %e, "cannot find the next episode to make ahead");
                return None;
            }
        };
        Some(Self {
            catalog,
            viewer,
            show: played.show_id.to_string(),
            next,
            done: false,
        })
    }

    /// One look at the playback, as `seen`: the copy of the next episode
    /// asked for now, if it was.
    pub async fn look(&mut self, pool: &Pool, seen: Seen) -> Option<CopyRow> {
        pool.played(&self.viewer, &self.show);
        if self.done {
            return None;
        }
        let video = match &seen.copy {
            Some(copy) => {
                let video = ahead_video(pool, copy).await;
                // A profile's copy plays: it asks for nothing.
                self.done = video.is_none();
                video?
            }
            None if seen.caught_up => CopyVideo::of_plan(&seen.plan)?,
            None => return None,
        };
        self.done = true;
        let next = &self.next;
        let path = match self.catalog.media_path(next).await {
            Ok(path) => path,
            Err(e) => {
                tracing::warn!(episode = %next, error = %e, "the next episode cannot be made ahead");
                return None;
            }
        };
        let source_stamp = match crate::files::stamp_text(&path).await {
            Ok(stamp) => stamp,
            Err(e) => {
                tracing::warn!(episode = %next, error = %e, "the next episode cannot be made ahead");
                return None;
            }
        };
        let copy = NewCopy {
            media_id: next.to_string(),
            media_path: path.display().to_string(),
            source_stamp,
            target: CopyTarget::Ahead,
            video,
        };
        match pool.request_ahead(&self.viewer, &self.show, copy).await {
            Ok(row) => {
                tracing::info!(copy = row.id, episode = %next, "the next episode is made ahead");
                Some(row)
            }
            Err(e) => {
                tracing::warn!(episode = %next, error = %e, "cannot ask for the next episode's copy");
                None
            }
        }
    }
}

/// Watch `handle`, which plays `played` for `viewer`, until it ends: a
/// task of its own, if anything may be asked for it. It holds neither the
/// pool nor the playbacks, nor the playback's conversion, so they stop as
/// the server and the playback do.
pub fn spawn(
    pool: &Arc<Pool>,
    playbacks: &Arc<Playbacks>,
    catalog: Arc<dyn Catalog>,
    handle: PlaybackHandle,
    viewer: String,
    played: Played,
) {
    // A special is not part of the run: it has no next episode.
    if played.season == 0 || !playbacks.binge_view(&handle).is_some_and(|s| s.may_ask()) {
        return;
    }
    let pool: Weak<Pool> = Arc::downgrade(pool);
    let playbacks: Weak<Playbacks> = Arc::downgrade(playbacks);
    tokio::spawn(async move {
        let Some(mut watcher) = Watcher::new(catalog, viewer, &played).await else {
            return;
        };
        loop {
            let Some(seen) = playbacks.upgrade().and_then(|p| p.binge_view(&handle)) else {
                return;
            };
            let Some(pool) = pool.upgrade() else {
                return;
            };
            // Its encode, while it has yet to catch up: else the next look.
            let signal = seen
                .signal
                .clone()
                .filter(|_| !watcher.done && !seen.caught_up);
            watcher.look(&pool, seen).await;
            drop(pool);
            // A run gone or failed says nothing more: the next look.
            let caught_up = async {
                let caught_up = match signal {
                    Some(signal) => signal.wait().await,
                    None => false,
                };
                if !caught_up {
                    std::future::pending::<()>().await;
                }
            };
            tokio::select! {
                () = caught_up => {}
                () = tokio::time::sleep(LOOK) => {}
            }
        }
    });
}

/// The video copy `id` was made with, if it is an `ahead` one.
async fn ahead_video(pool: &Pool, id: &str) -> Option<CopyVideo> {
    match pool.inner.store.optimized_copy(id).await {
        Ok(Some(row)) if row.target == CopyTarget::Ahead => Some(row.video),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(copy = id, error = %e, "cannot read the copy a playback plays");
            None
        }
    }
}

/// The episode after `played` in its show's run
/// (`crate::watch::show_run`): a special, not part of the run, has none.
pub async fn next_episode(
    catalog: &dyn Catalog,
    played: &Played,
) -> Result<Option<MediaId>, CatalogError> {
    if played.season == 0 {
        return Ok(None);
    }
    let run = crate::watch::show_run(catalog.episodes(&played.show_id).await?);
    let at = run.iter().position(|e| *e.id() == played.id);
    Ok(at.and_then(|at| run.get(at + 1)).map(|e| e.id().clone()))
}
