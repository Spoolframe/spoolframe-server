//! What an app does with the downloads while it is away: a background
//! refresh that sends what was done here and brings followed music up to
//! date, and a background run's progress, as a task scheduler shows it.
//! Moved here from the Apple app, which had its own.

use std::collections::BTreeSet;
use std::time::Duration;

use super::addons::{self, AddonKind, AddonState, DownloadAddon};
use super::music;
use super::{DownloadPhase, DownloadState, Downloads, LocalDownload, Shared};
use crate::SdkError;

/// How long after one background refresh the next is worth asking for
/// while music is followed ([`Downloads::refresh_hint`]): the Apple app's
/// six hours.
pub const REFRESH_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// What a [`Downloads::refresh_all`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct RefreshReport {
    /// Profiles whose server was reached and told.
    pub reached: u32,
    /// Items whose watch, play or like the servers heard.
    pub synced: u32,
    /// Profiles not reached or not told: each is tried again with the
    /// next refresh, or the next session made.
    pub missed: Vec<RefreshMissed>,
}

/// A profile a [`Downloads::refresh_all`] could not bring up to date.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct RefreshMissed {
    pub server: String,
    pub user_id: String,
    pub reason: String,
}

/// The servers and users with a watch, play or like here not sent yet.
fn to_send(shared: &Shared) -> Vec<(String, String)> {
    let Some(store) = &shared.store else {
        return Vec::new();
    };
    let mut users = store.carried_users();
    users.extend(music::plays_to_send(store));
    users.sort();
    users.dedup();
    users
}

/// The servers and users who follow an artist, a playlist or Liked here.
fn following(shared: &Shared) -> Vec<(String, String)> {
    shared
        .store
        .as_ref()
        .map(music::following)
        .unwrap_or_default()
}

impl Downloads {
    /// A background refresh has something to do: a watch, play or like
    /// made here the server has not heard, or music followed to bring up
    /// to date. Read from disk; nothing is asked.
    pub fn needs_refresh(&self) -> bool {
        let shared = self.shared();
        !to_send(shared).is_empty() || !following(shared).is_empty()
    }

    /// For a background refresh: each profile with something to send or
    /// music followed, on its server, sends what was done here
    /// ([`Downloads::sync_watch`], [`Downloads::sync_music`]) and has its
    /// followed music brought up to date. One profile not reached never
    /// stops the others. A profile is reached with its session here, else
    /// with one its server's saved sign-ins make; one that needs a
    /// password typed is missed.
    pub async fn refresh_all(&self) -> Result<RefreshReport, SdkError> {
        let shared = self.shared();
        shared.store()?;
        let mut users = to_send(shared);
        users.extend(following(shared));
        users.sort();
        users.dedup();
        let mut report = RefreshReport::default();
        for (server, user_id) in users {
            let missed = |reason: String| RefreshMissed {
                server: server.clone(),
                user_id: user_id.clone(),
                reason,
            };
            let session =
                match super::manager::session_for(&self.client, shared, &server, &user_id).await {
                    Ok(session) => session,
                    Err(e) => {
                        report.missed.push(missed(e.to_string()));
                        continue;
                    }
                };
            // Each its own requests, and the store keeps its own lock.
            let (watched, music) = tokio::join!(
                super::manager::sync(shared, &session),
                music::sync(shared, &session)
            );
            match (watched, music) {
                (Ok(watched), Ok(music)) => {
                    report.reached += 1;
                    let items = watched.len() + music.plays.len() + music.liked.len();
                    report.synced += u32::try_from(items).unwrap_or(u32::MAX);
                }
                (Err(e), _) | (_, Err(e)) => report.missed.push(missed(e.to_string())),
            }
        }
        Ok(report)
    }

    /// How soon a background refresh is worth asking for: now with
    /// something to send, else when a download waiting to try again or a
    /// followed download due to be asked about is due, else
    /// [`REFRESH_EVERY`] while music is followed. `None`: no refresh is
    /// needed.
    pub fn refresh_hint(&self) -> Option<Duration> {
        let shared = self.shared();
        if !to_send(shared).is_empty() {
            return Some(Duration::ZERO);
        }
        let now = tokio::time::Instant::now();
        let until = |at: tokio::time::Instant| at.saturating_duration_since(now);
        let retries = shared
            .retry_at
            .lock()
            .values()
            .map(|(at, _)| until(*at))
            .min();
        let checks = shared.check_at.lock().values().map(|at| until(*at)).min();
        let follow = shared.follow_due.lock().soonest().map(until);
        let every = (!following(shared).is_empty()).then_some(REFRESH_EVERY);
        [retries, checks, follow, every].into_iter().flatten().min()
    }

    /// A background run ([`Downloads::run_until_cancelled`]) is worth
    /// asking the system for: a download, or an audio track being added
    /// to one, is coming to this device or being converted with a figure
    /// to show, and the network allows downloads. Every profile's: a run
    /// moves them all.
    pub fn worth_background(&self) -> bool {
        Self::worth(self.shared())
    }

    fn worth(shared: &Shared) -> bool {
        let Some(store) = &shared.store else {
            return false;
        };
        !shared.network_held()
            && store.scan().iter().any(|m| {
                (m.phase.is_active() && moving(&m.phase))
                    || addons::managed(m).is_some_and(|a| addon_moving(&a.addon.state))
            })
    }
}

/// Coming to this device, or being converted with a figure to show.
fn moving(phase: &DownloadPhase) -> bool {
    matches!(
        phase,
        DownloadPhase::Transferring
            | DownloadPhase::Preparing {
                server: DownloadState::Converting {
                    done_percent: Some(_),
                    ..
                },
            }
    )
}

/// An audio add-on's [`moving`]: coming here, or being converted with a
/// figure to show.
fn addon_moving(state: &AddonState) -> bool {
    matches!(
        state,
        AddonState::Transferring { .. }
            | AddonState::Preparing {
                server: DownloadState::Converting {
                    done_percent: Some(_),
                    ..
                },
            }
    )
}

/// The audio add-on of `d` the manager moves now, as the app sees it
/// ([`addons::managed`]).
fn adding(d: &LocalDownload) -> Option<&DownloadAddon> {
    d.addons.iter().find(|a| a.is_managed())
}

/// Units of a background run's progress for each download: the first
/// half while the server prepares it, the second while it comes here.
/// Fine enough that a slow conversion still moves it: a system may end a
/// task whose progress stands.
pub const UNITS_PER_DOWNLOAD: u64 = 1000;

/// How far a background run got ([`RunTally::progress`]), as a task
/// scheduler shows it.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct RunProgress {
    /// Of `total`.
    pub completed: u64,
    /// [`UNITS_PER_DOWNLOAD`] for each download the run has had under
    /// way.
    pub total: u64,
    /// Downloads the run has had under way, finished ones too; an audio
    /// track added to one counts as one more.
    pub downloads: u32,
    /// The one to name: the first coming here or being converted, else
    /// the first under way. `None` when none is.
    pub moving: Option<LocalDownload>,
}

/// The downloads a background run has had under way, for its progress:
/// one that finishes or goes stays counted, so the progress never goes
/// back. A new one per run.
#[derive(Debug, Clone, Default)]
pub struct RunTally {
    /// A download by its key, and an audio add-on by its download's key
    /// and its place: apart from its download, so the count never goes
    /// back when a film finishes and an add-on of it starts.
    keys: BTreeSet<(String, Option<u32>)>,
}

impl RunTally {
    pub fn new() -> Self {
        Self::default()
    }

    /// The run's progress, with `latest` the downloads as now
    /// ([`Downloads::list`]): each one under way is counted from now on.
    pub fn progress(&mut self, latest: &[LocalDownload]) -> RunProgress {
        let half = UNITS_PER_DOWNLOAD / 2;
        for d in latest {
            if d.phase.is_active() {
                self.keys.insert((d.key.clone(), None));
            }
            if let Some(a) = adding(d) {
                self.keys.insert((d.key.clone(), Some(a.place)));
            }
        }
        let fraction = |received: u64, size: Option<u64>| match size {
            Some(size) if size > 0 => half + (half * received.min(size)) / size,
            _ => half,
        };
        let percent = |p: &Option<u8>| p.map_or(0, |p| u64::from(p.min(100)) * half / 100);
        let mut completed = 0;
        for (key, place) in &self.keys {
            if let Some(place) = *place {
                let addon = latest
                    .iter()
                    .find(|d| &d.key == key)
                    .and_then(|d| d.addons.iter().find(|a| a.is(AddonKind::Audio, place)));
                completed += match addon.map(|a| (&a.state, a)) {
                    // Gone: done with, as far as the run goes.
                    None => UNITS_PER_DOWNLOAD,
                    Some((
                        AddonState::Preparing {
                            server:
                                DownloadState::Converting { done_percent, .. }
                                | DownloadState::Writing { done_percent },
                        },
                        _,
                    )) => percent(done_percent),
                    Some((AddonState::Preparing { .. }, _)) => 0,
                    Some((AddonState::Transferring { .. } | AddonState::Paused, a)) => {
                        fraction(a.received, a.size)
                    }
                    Some((AddonState::Ready | AddonState::Failed { .. }, _)) => UNITS_PER_DOWNLOAD,
                };
                continue;
            }
            let Some(d) = latest.iter().find(|d| &d.key == key) else {
                // Gone: done with, as far as the run goes.
                completed += UNITS_PER_DOWNLOAD;
                continue;
            };
            completed += match &d.phase {
                DownloadPhase::Preparing {
                    server: DownloadState::Converting { done_percent, .. },
                }
                | DownloadPhase::Preparing {
                    server: DownloadState::Writing { done_percent },
                } => percent(done_percent),
                DownloadPhase::Preparing { .. } => 0,
                DownloadPhase::Transferring | DownloadPhase::Paused => fraction(d.received, d.size),
                DownloadPhase::Done | DownloadPhase::Failed { .. } => UNITS_PER_DOWNLOAD,
            };
        }
        let total = self.keys.len() as u64 * UNITS_PER_DOWNLOAD;
        let under_way = |d: &&LocalDownload| d.phase.is_active() || adding(d).is_some();
        let moving = latest
            .iter()
            .find(|d| {
                (d.phase.is_active() && moving(&d.phase))
                    || adding(d).is_some_and(|a| addon_moving(&a.state))
            })
            .or_else(|| latest.iter().find(under_way))
            .cloned();
        RunProgress {
            completed: completed.min(total),
            total,
            downloads: u32::try_from(self.keys.len()).unwrap_or(u32::MAX),
            moving,
        }
    }
}

#[cfg(test)]
#[path = "background_tests.rs"]
mod tests;
