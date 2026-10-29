//! What a client picks out of Home: the hero, Continue watching and Up
//! next as one row, and what a TV's Top Shelf or Watch Next shows.

use std::collections::HashSet;
use std::time::Duration;

use spoolframe_types::{Item, MediaId};

use crate::messages::HomeScreen;

/// Which of Home's rows a featured item came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeaturedRow {
    ContinueWatching,
    UpNext,
    RecentlyAdded,
}

/// One item a TV features ([`HomeScreen::featured`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Featured {
    pub item: Item,
    pub row: FeaturedRow,
}

/// How much of each row is featured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeaturedLimits {
    /// Continue watching viewed within this long; all of the newest when
    /// the server does not say when each was viewed.
    pub continue_within: Duration,
    pub continue_watching: usize,
    pub up_next: usize,
    /// Recently added fills up to this many in all.
    pub total: usize,
}

impl Default for FeaturedLimits {
    /// The Apple TV's Top Shelf: three continued within 48 hours, five up
    /// next, twelve in all.
    fn default() -> Self {
        Self {
            continue_within: Duration::from_secs(48 * 3600),
            continue_watching: 3,
            up_next: 5,
            total: 12,
        }
    }
}

/// A film, an episode, a show or a season: what a TV features.
fn video(item: &Item) -> bool {
    matches!(
        item,
        Item::Movie(_) | Item::Episode(_) | Item::Show(_) | Item::Season(_)
    )
}

/// What a row shows once: an episode or a season stands for its show,
/// anything else for itself.
fn shown_as(item: &Item) -> &MediaId {
    match item {
        Item::Episode(e) => &e.show_id,
        Item::Season(s) => &s.show_id,
        other => other.id(),
    }
}

impl HomeScreen {
    /// The item Home shows large: the first continued, else the newest
    /// added.
    pub fn hero(&self) -> Option<&Item> {
        self.continue_watching
            .first()
            .or_else(|| self.recently_added.first())
    }

    /// Continue watching, then Up next, as one row: each show once (its
    /// first episode listed, the one continued before one up next), each
    /// other item once.
    pub fn up_next_merged(&self) -> Vec<Item> {
        let mut seen = HashSet::new();
        self.continue_watching
            .iter()
            .chain(&self.next_up)
            .filter(|item| seen.insert(shown_as(item).clone()))
            .cloned()
            .collect()
    }

    /// What a TV's Top Shelf or Watch Next shows at `now` (Unix
    /// seconds): Continue watching viewed lately, then Up next of shows
    /// not already continued, then Recently added, films and shows only,
    /// each show once (an episode or a season stands for its show).
    pub fn featured(&self, now: i64, limits: FeaturedLimits) -> Vec<Featured> {
        let mut picked: Vec<Featured> = Vec::new();
        let mut seen = HashSet::new();
        let mut add = |item: &Item, row, picked: &mut Vec<Featured>| {
            if !video(item) || !seen.insert(shown_as(item).clone()) {
                return false;
            }
            picked.push(Featured {
                item: item.clone(),
                row,
            });
            true
        };
        let viewed = |item: &Item| item.playback().and_then(|p| p.last_viewed_at);
        // From a server that does not say when, all are taken as recent.
        let timed = self.continue_watching.iter().any(|i| viewed(i).is_some());
        let within = limits.continue_within.as_secs() as i64;
        let recent = self
            .continue_watching
            .iter()
            .filter(|i| !timed || viewed(i).is_some_and(|at| now - at < within))
            .take(limits.continue_watching);
        for item in recent {
            add(item, FeaturedRow::ContinueWatching, &mut picked);
        }
        let mut up_next = 0;
        for item in &self.next_up {
            if up_next == limits.up_next {
                break;
            }
            // A show already continued is skipped: `add` sees it.
            if add(item, FeaturedRow::UpNext, &mut picked) {
                up_next += 1;
            }
        }
        for item in &self.recently_added {
            if picked.len() >= limits.total {
                break;
            }
            add(item, FeaturedRow::RecentlyAdded, &mut picked);
        }
        picked
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_types::{Episode, MediaKind, Movie, Namespace, PlaybackState};

    fn id(kind: MediaKind, key: &str) -> MediaId {
        MediaId::new(Namespace::Plex, kind, key).unwrap()
    }

    fn movie(key: &str, viewed: Option<i64>) -> Item {
        Item::Movie(Movie {
            id: id(MediaKind::Movie, key),
            title: key.into(),
            year: None,
            release_date: None,
            summary: None,
            content_rating: None,
            duration: None,
            cast: Vec::new(),
            artwork: Vec::new(),
            playback: PlaybackState {
                last_viewed_at: viewed,
                ..PlaybackState::default()
            },
        })
    }

    fn episode(show: &str, n: u32) -> Item {
        Item::Episode(Episode {
            id: id(MediaKind::Episode, &format!("{show}{n}")),
            show_id: id(MediaKind::Show, show),
            show_title: Some(show.into()),
            season_id: id(MediaKind::Season, &format!("{show}1")),
            season: 1,
            episode: n,
            title: format!("{show} {n}"),
            summary: None,
            content_rating: None,
            year: None,
            air_date: None,
            duration: None,
            cast: Vec::new(),
            artwork: Vec::new(),
            playback: PlaybackState::default(),
        })
    }

    fn home(continuing: Vec<Item>, next_up: Vec<Item>, added: Vec<Item>) -> HomeScreen {
        HomeScreen {
            continue_watching: continuing,
            next_up,
            recently_added: added,
            unavailable: Vec::new(),
            servers: Vec::new(),
        }
    }

    #[test]
    fn the_hero_is_the_first_continued_else_the_newest_added() {
        let h = home(vec![movie("a", None)], Vec::new(), vec![movie("b", None)]);
        assert_eq!(h.hero().map(|i| i.title()), Some("a"));
        let h = home(Vec::new(), Vec::new(), vec![movie("b", None)]);
        assert_eq!(h.hero().map(|i| i.title()), Some("b"));
    }

    #[test]
    fn up_next_merged_has_each_show_once_continued_first() {
        let h = home(
            vec![movie("a", None), episode("s", 2)],
            vec![episode("s", 3), episode("t", 1)],
            Vec::new(),
        );
        let titles: Vec<_> = h
            .up_next_merged()
            .iter()
            .map(|i| i.title().to_owned())
            .collect();
        assert_eq!(titles, ["a", "s 2", "t 1"]);
    }

    #[test]
    fn featured_takes_what_was_continued_lately_then_up_next_then_added() {
        let now = 1_000_000;
        let h = home(
            vec![
                movie("lately", Some(now - 3600)),
                movie("long ago", Some(now - 72 * 3600)),
                episode("s", 4),
            ],
            vec![episode("s", 5), episode("t", 1)],
            (0..20).map(|n| movie(&format!("new {n}"), None)).collect(),
        );
        let featured = h.featured(now, FeaturedLimits::default());
        let rows: Vec<_> = featured
            .iter()
            .map(|f| (f.item.title().to_owned(), f.row))
            .collect();
        assert_eq!(rows[0], ("lately".into(), FeaturedRow::ContinueWatching));
        // Not viewed within 48 hours, and an episode with no time.
        assert!(!rows.iter().any(|(t, _)| t == "long ago" || t == "s 4"));
        assert_eq!(rows[1], ("s 5".into(), FeaturedRow::UpNext));
        assert_eq!(rows[2], ("t 1".into(), FeaturedRow::UpNext));
        assert_eq!(featured.len(), 12);
        assert_eq!(rows[11].1, FeaturedRow::RecentlyAdded);
    }

    #[test]
    fn up_next_of_a_show_already_continued_is_not_featured_again() {
        let h = home(vec![episode("s", 4)], vec![episode("s", 5)], Vec::new());
        let featured = h.featured(0, FeaturedLimits::default());
        assert_eq!(featured.len(), 1);
        assert_eq!(featured[0].row, FeaturedRow::ContinueWatching);
    }

    #[test]
    fn a_show_continued_is_not_featured_again_as_recently_added() {
        let h = home(
            vec![episode("s", 4)],
            Vec::new(),
            vec![episode("s", 9), movie("new", None)],
        );
        let titles: Vec<_> = h
            .featured(0, FeaturedLimits::default())
            .iter()
            .map(|f| f.item.title().to_owned())
            .collect();
        assert_eq!(titles, ["s 4", "new"]);
    }

    #[test]
    fn music_is_never_featured() {
        let track = Item::Artist(spoolframe_types::Artist {
            id: id(MediaKind::Artist, "1"),
            name: "A".into(),
            summary: None,
            artwork: Vec::new(),
        });
        let h = home(Vec::new(), Vec::new(), vec![track]);
        assert!(h.featured(0, FeaturedLimits::default()).is_empty());
    }
}
