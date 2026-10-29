//! The downloads as an offline library shows them, from disk alone:
//! grouped into shows and films, what to continue watching, the copy of
//! an item to play, and what each profile keeps here.

use std::collections::HashMap;

use spoolframe_types::{Item, MediaId};

use super::store::EntryKind;
use super::{DownloadPhase, Downloads, LocalDownload, Shared};

/// A show with episodes downloaded ([`DownloadLibrary::shows`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DownloadedShow {
    /// As the server had it when its first episode here was downloaded.
    pub show: Item,
    /// By season and episode.
    pub episodes: Vec<LocalDownload>,
}

/// The active profile's downloads, grouped as an offline library shows
/// them ([`Downloads::library`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DownloadLibrary {
    /// Shows with episodes here, by title.
    pub shows: Vec<DownloadedShow>,
    /// Films here, by title.
    pub films: Vec<LocalDownload>,
    /// Downloads not finished (preparing, moving, paused or failed),
    /// oldest first.
    pub in_progress: Vec<LocalDownload>,
}

/// One profile's downloads on this device ([`Downloads::profiles_here`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ProfileHere {
    pub server: String,
    pub user_id: String,
    /// Empty when every file they hold here was fetched by another
    /// profile.
    pub username: String,
    /// Films, episodes and tracks they downloaded here.
    pub downloads: u32,
    /// Bytes of every file they hold, shared ones too.
    pub bytes: u64,
    /// Bytes of the files only they hold: what removing their downloads
    /// would free.
    pub own_bytes: u64,
}

/// What the downloads take on this device, by profile
/// ([`Downloads::profiles_here`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DownloadsHere {
    pub profiles: Vec<ProfileHere>,
    /// Bytes of every file here, each counted once however many profiles
    /// hold it. Tracks kept from listening too.
    pub total_bytes: u64,
}

fn title(item: &Item) -> String {
    match item {
        Item::Movie(m) => m.title.to_lowercase(),
        Item::Show(s) => s.title.to_lowercase(),
        Item::Episode(e) => e.title.to_lowercase(),
        other => other.id().to_string(),
    }
}

fn episode_order(item: &Item) -> (u32, u32) {
    match item {
        Item::Episode(e) => (e.season, e.episode),
        _ => (0, 0),
    }
}

fn finished(d: &LocalDownload) -> bool {
    d.phase == DownloadPhase::Done
}

/// Begun and not watched: where to go on from.
fn in_the_middle(d: &LocalDownload) -> bool {
    finished(d) && !d.watched_here() && d.watch.position_ms.is_some_and(|p| p > 0)
}

/// `downloads` grouped as [`Downloads::library`] has them.
pub(super) fn library(downloads: Vec<LocalDownload>) -> DownloadLibrary {
    let mut shows: HashMap<MediaId, DownloadedShow> = HashMap::new();
    let mut films = Vec::new();
    let mut in_progress = Vec::new();
    for d in downloads {
        if !finished(&d) {
            in_progress.push(d);
            continue;
        }
        match (&d.item, &d.show) {
            (Item::Episode(e), show) => {
                let show = show.clone().unwrap_or_else(|| d.item.clone());
                shows
                    .entry(e.show_id.clone())
                    .or_insert_with(|| DownloadedShow {
                        show,
                        episodes: Vec::new(),
                    })
                    .episodes
                    .push(d);
            }
            _ => films.push(d),
        }
    }
    let mut shows: Vec<DownloadedShow> = shows.into_values().collect();
    for show in &mut shows {
        show.episodes.sort_by_key(|d| episode_order(&d.item));
    }
    shows.sort_by_key(|s| title(&s.show));
    films.sort_by_key(|d| title(&d.item));
    in_progress.sort_by_key(|d| d.created_at);
    DownloadLibrary {
        shows,
        films,
        in_progress,
    }
}

/// The finished downloads begun and not watched, the latest watched
/// first: one per show, its latest.
pub(super) fn continue_watching(downloads: Vec<LocalDownload>) -> Vec<LocalDownload> {
    let mut going: Vec<LocalDownload> = downloads.into_iter().filter(in_the_middle).collect();
    going.sort_by_key(|d| std::cmp::Reverse(d.watch.at));
    let mut shows_seen = Vec::new();
    going.retain(|d| match &d.item {
        Item::Episode(e) if shows_seen.contains(&e.show_id) => false,
        Item::Episode(e) => {
            shows_seen.push(e.show_id.clone());
            true
        }
        _ => true,
    });
    going
}

/// The episode of `show` to play next here: the one begun, the latest
/// watched; else the first not watched after the last watched, by
/// season and episode; else the first not watched. `None` with every one
/// here watched.
pub(super) fn show_resume(downloads: Vec<LocalDownload>, show: &MediaId) -> Option<LocalDownload> {
    let mut episodes: Vec<LocalDownload> = downloads
        .into_iter()
        .filter(|d| finished(d) && matches!(&d.item, Item::Episode(e) if &e.show_id == show))
        .collect();
    episodes.sort_by_key(|d| episode_order(&d.item));
    if let Some(begun) = episodes
        .iter()
        .filter(|d| in_the_middle(d))
        .max_by_key(|d| d.watch.at)
    {
        return Some(begun.clone());
    }
    let after = episodes
        .iter()
        .rposition(LocalDownload::watched_here)
        .map_or(0, |last| last + 1);
    episodes
        .iter()
        .skip(after)
        .chain(episodes.iter().take(after))
        .find(|d| !d.watched_here())
        .cloned()
}

/// The download of `id` on `server` to play: the newest finished one,
/// else the newest of any.
pub(super) fn of_item(
    downloads: Vec<LocalDownload>,
    server: &str,
    id: &MediaId,
) -> Option<LocalDownload> {
    downloads
        .into_iter()
        .filter(|d| d.server == server && d.item.id() == id)
        .max_by_key(|d| (finished(d), d.created_at))
}

impl Downloads {
    /// The active profile's downloads grouped into shows, films and
    /// those still in progress. Needs no server.
    pub fn library(&self) -> DownloadLibrary {
        library(self.list())
    }

    /// Continue watching, offline: the finished downloads begun and not
    /// watched, the latest watched first, one per show.
    pub fn continue_watching(&self) -> Vec<LocalDownload> {
        continue_watching(self.list())
    }

    /// The episode of `show` to play next from the downloads here: the
    /// one begun, else the next not watched. `None` with none to play.
    pub fn show_resume(&self, show: &MediaId) -> Option<LocalDownload> {
        show_resume(self.list_of(show), show)
    }

    /// The active profile's download of `id` on `server` to play in
    /// place of the server's: the finished copy wins.
    pub fn of_item(&self, server: &str, id: &MediaId) -> Option<LocalDownload> {
        of_item(self.list_of(id), server, id)
    }

    /// Every profile with downloads on this device and what they take,
    /// each file counted once in the total however many hold it. Every
    /// profile's, whichever is active: for the device's storage page.
    pub fn profiles_here(&self) -> DownloadsHere {
        profiles_here(self.shared())
    }
}

/// [`Downloads::profiles_here`].
pub(super) fn profiles_here(shared: &Shared) -> DownloadsHere {
    let Some(store) = &shared.store else {
        return DownloadsHere::default();
    };
    let mut profiles: HashMap<(String, String), ProfileHere> = HashMap::new();
    let mut total_bytes = 0;
    for m in store.scan() {
        if m.kind == EntryKind::Unknown {
            continue;
        }
        let bytes = shared.received_of(&m);
        total_bytes += bytes;
        let mut users: Vec<&str> = m.holds.iter().filter_map(|h| h.user.as_deref()).collect();
        users.sort_unstable();
        users.dedup();
        // Freed only when nothing else holds it: not another profile,
        // nor the device's tracks kept from listening.
        let alone = users.len() == 1 && m.holds.iter().all(|h| h.user.is_some());
        for user in users {
            let profile = profiles
                .entry((m.server.clone(), user.to_owned()))
                .or_insert_with(|| ProfileHere {
                    server: m.server.clone(),
                    user_id: user.to_owned(),
                    username: String::new(),
                    downloads: 0,
                    bytes: 0,
                    own_bytes: 0,
                });
            // A manifest names only the user whose session fetched it.
            if user == m.user_id {
                profile.username = m.username.clone();
            }
            profile.downloads += 1;
            profile.bytes += bytes;
            if alone {
                profile.own_bytes += bytes;
            }
        }
    }
    let mut profiles: Vec<ProfileHere> = profiles.into_values().collect();
    profiles.sort_by(|a, b| (&a.server, &a.user_id).cmp(&(&b.server, &b.user_id)));
    DownloadsHere {
        profiles,
        total_bytes,
    }
}

#[cfg(test)]
#[path = "library_tests.rs"]
mod tests;
