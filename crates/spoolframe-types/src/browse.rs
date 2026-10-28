//! Sorting and filtering a library's items:
//! what a client asks `Items` to order and keep, and what a library has
//! to filter by.
//!
//! Every list here combines its values with OR, and the parts of an
//! [`ItemFilter`] combine with AND: `{"genres": ["Drama", "Comedy"],
//! "year_from": 1990}` keeps the dramas and the comedies from 1990 on.

use serde::{Deserialize, Serialize};

/// The most values one list of an [`ItemFilter`] holds. A longer list is
/// refused, not cut: a filter cut short would keep items it should not.
pub const MAX_FILTER_VALUES: usize = 64;

/// The longest genre or content rating a filter names, in characters.
pub const MAX_FILTER_TEXT: usize = 128;

/// What `Items` orders a library by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortKey {
    /// The sort title: the title with a leading "The", "A" or "An" set
    /// aside, as Plex's `titleSort` does, or the catalog's own sort title.
    Title,
    /// The day it came out. An item without one comes last both ways.
    Released,
    /// When the server's catalog got it: a show by its latest episode's.
    Added,
    /// When the session user last watched it, from their watch state: a
    /// show by its latest episode. Never watched comes last both ways.
    LastPlayed,
    /// The critic rating, 0 to 10 (Plex's `rating`). An item without one
    /// comes last both ways.
    Rating,
    /// The audience rating, 0 to 10 (Plex's `audienceRating`). An item
    /// without one comes last both ways.
    AudienceRating,
    /// Its running time: a movie's. An item without one comes last both
    /// ways.
    Duration,
    /// Its file's size: a movie's first version. Only a paired server's
    /// library has it: Plex does not sort by it. An item without one
    /// comes last both ways.
    Size,
    /// A key this build does not know: `InvalidInput`.
    #[serde(other)]
    Unknown,
}

impl SortKey {
    /// Every key this build knows, in the order a client lists them.
    pub const ALL: [SortKey; 8] = [
        SortKey::Title,
        SortKey::Released,
        SortKey::Added,
        SortKey::LastPlayed,
        SortKey::Rating,
        SortKey::AudienceRating,
        SortKey::Duration,
        SortKey::Size,
    ];

    /// Its name on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            SortKey::Title => "title",
            SortKey::Released => "released",
            SortKey::Added => "added",
            SortKey::LastPlayed => "last_played",
            SortKey::Rating => "rating",
            SortKey::AudienceRating => "audience_rating",
            SortKey::Duration => "duration",
            SortKey::Size => "size",
            SortKey::Unknown => "unknown",
        }
    }

    /// The key named `name` on the wire; `None` for one this build does
    /// not know.
    pub fn parse(name: &str) -> Option<SortKey> {
        SortKey::ALL.into_iter().find(|k| k.as_str() == name)
    }
}

/// How `Items` orders a library: by `key`, then by sort title A to Z,
/// then by id, so that items equal by `key` keep one order and a page
/// never repeats or skips one. The tiebreakers go A to Z both ways.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ItemSort {
    pub key: SortKey,
    /// Largest, newest or last first. Missing values stay last either way.
    #[serde(default)]
    pub descending: bool,
}

impl ItemSort {
    /// The order `Items` gives without one: by title, A to Z.
    pub const DEFAULT: ItemSort = ItemSort {
        key: SortKey::Title,
        descending: false,
    };
}

/// The session user's watch state of an item. A movie or an episode is
/// watched once marked or played to the end, in progress once played past
/// its first minutes and not yet watched, unwatched otherwise. A show is
/// watched when every episode the library has is, in progress when some
/// episode is watched or in progress, unwatched when none is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchedState {
    Unwatched,
    InProgress,
    Watched,
    /// A state this build does not know: `InvalidInput`.
    #[serde(other)]
    Unknown,
}

/// An item's picture size class, by its first version's video height.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Resolution {
    /// 2160 lines or more.
    #[serde(rename = "4k")]
    Uhd,
    /// 1080 lines.
    #[serde(rename = "1080p")]
    FullHd,
    /// 720 lines.
    #[serde(rename = "720p")]
    Hd,
    /// Fewer.
    #[serde(rename = "sd")]
    Sd,
    /// A class this build does not know: `InvalidInput`.
    #[serde(other, rename = "unknown")]
    Unknown,
}

impl Resolution {
    /// The class of a picture `height` lines tall. A 1080p film cropped to
    /// 2.39:1 is 1920×800, so the classes go by the width when it says
    /// more: what Plex's own classes do.
    pub fn of(width: Option<u32>, height: Option<u32>) -> Option<Resolution> {
        let by_height = |h: u32| match h {
            h if h >= 1600 => Resolution::Uhd,
            h if h >= 900 => Resolution::FullHd,
            h if h >= 650 => Resolution::Hd,
            _ => Resolution::Sd,
        };
        let by_width = |w: u32| match w {
            w if w >= 3200 => Resolution::Uhd,
            w if w >= 1800 => Resolution::FullHd,
            w if w >= 1200 => Resolution::Hd,
            _ => Resolution::Sd,
        };
        match (width, height) {
            (Some(w), Some(h)) => Some(by_height(h).min(by_width(w))),
            (None, Some(h)) => Some(by_height(h)),
            (Some(w), None) => Some(by_width(w)),
            (None, None) => None,
        }
    }

    /// The class Plex names `videoResolution`: `4k`, `1080`, `720`, `576`,
    /// `480`, `sd`.
    pub fn of_plex(name: &str) -> Option<Resolution> {
        Some(match name.trim().to_ascii_lowercase().as_str() {
            "4k" | "2160" | "2160p" => Resolution::Uhd,
            "1080" | "1080p" => Resolution::FullHd,
            "720" | "720p" => Resolution::Hd,
            "" => return None,
            other if other.trim_end_matches('p').parse::<u32>().is_ok() || other == "sd" => {
                Resolution::Sd
            }
            _ => return None,
        })
    }

    /// Its name on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Resolution::Uhd => "4k",
            Resolution::FullHd => "1080p",
            Resolution::Hd => "720p",
            Resolution::Sd => "sd",
            Resolution::Unknown => "unknown",
        }
    }

    /// The class named `name` on the wire; `None` for one this build
    /// does not know.
    pub fn parse(name: &str) -> Option<Resolution> {
        [
            Resolution::Uhd,
            Resolution::FullHd,
            Resolution::Hd,
            Resolution::Sd,
        ]
        .into_iter()
        .find(|r| r.as_str() == name)
    }
}

/// What an item's picture shows of brightness and colour, by its first
/// version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DynamicRange {
    /// Neither HDR nor Dolby Vision.
    Sdr,
    /// Any HDR: HDR10, HDR10+, HLG, or Dolby Vision. A filter for it keeps
    /// Dolby Vision items too.
    Hdr,
    /// Dolby Vision, with or without an HDR10 base.
    DolbyVision,
    /// A range this build does not know: `InvalidInput`.
    #[serde(other)]
    Unknown,
}

/// Which items `Items` keeps. Every part is optional; an empty list or
/// `None` keeps every item as far as that part goes. An item that has no
/// value for a part a filter names (no year, no rating) is not kept.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ItemFilter {
    /// The session user's watch state of it; never another user's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watched: Vec<WatchedState>,
    /// Any of these genres, by the catalog's names, as
    /// [`ItemFilterValues::genres`] lists them; case aside.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    /// Its year is this or later. A decade is `1990` to `1999`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year_from: Option<u16>,
    /// Its year is this or earlier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year_to: Option<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resolutions: Vec<Resolution>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dynamic_ranges: Vec<DynamicRange>,
    /// Any of these age ratings, as the catalog has them ("PG-13",
    /// "TV-MA"); case aside.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content_ratings: Vec<String>,
}

/// Why a sort or filter cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrowseError {
    #[error("unknown sort key")]
    UnknownSort,
    #[error("unknown {0} in the filter")]
    UnknownValue(&'static str),
    #[error("{0} lists more than {MAX_FILTER_VALUES} values")]
    TooMany(&'static str),
    #[error("a {0} in the filter is longer than {MAX_FILTER_TEXT} characters")]
    TooLong(&'static str),
    #[error("year_from {from} is after year_to {to}")]
    Years { from: u16, to: u16 },
}

impl ItemSort {
    /// Whether this build can use it.
    pub fn check(&self) -> Result<(), BrowseError> {
        match self.key {
            SortKey::Unknown => Err(BrowseError::UnknownSort),
            _ => Ok(()),
        }
    }
}

impl ItemFilter {
    /// Whether it keeps every item.
    pub fn is_empty(&self) -> bool {
        *self == ItemFilter::default()
    }

    /// Whether this build can use it: no value it does not know, no list
    /// past [`MAX_FILTER_VALUES`], no text past [`MAX_FILTER_TEXT`], and
    /// years in order.
    pub fn check(&self) -> Result<(), BrowseError> {
        fn list<T>(name: &'static str, values: &[T]) -> Result<(), BrowseError> {
            if values.len() > MAX_FILTER_VALUES {
                return Err(BrowseError::TooMany(name));
            }
            Ok(())
        }
        fn texts(name: &'static str, values: &[String]) -> Result<(), BrowseError> {
            list(name, values)?;
            if values.iter().any(|v| v.chars().count() > MAX_FILTER_TEXT) {
                return Err(BrowseError::TooLong(name));
            }
            Ok(())
        }
        list("watched", &self.watched)?;
        if self.watched.contains(&WatchedState::Unknown) {
            return Err(BrowseError::UnknownValue("watched state"));
        }
        texts("genres", &self.genres)?;
        texts("content_ratings", &self.content_ratings)?;
        list("resolutions", &self.resolutions)?;
        if self.resolutions.contains(&Resolution::Unknown) {
            return Err(BrowseError::UnknownValue("resolution"));
        }
        list("dynamic_ranges", &self.dynamic_ranges)?;
        if self.dynamic_ranges.contains(&DynamicRange::Unknown) {
            return Err(BrowseError::UnknownValue("dynamic range"));
        }
        if let (Some(from), Some(to)) = (self.year_from, self.year_to)
            && from > to
        {
            return Err(BrowseError::Years { from, to });
        }
        Ok(())
    }
}

/// A value a library has, and how many of its top-level items have it
/// when that is cheap to say: a paired server's library always says, this
/// server's own (read from Plex) only for its resolutions and ranges.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Counted<T> {
    pub value: T,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
}

/// A part of an [`ItemFilter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterPart {
    Watched,
    Genres,
    Years,
    ContentRatings,
    Resolutions,
    DynamicRanges,
    /// A part this build does not know.
    #[serde(other)]
    Unknown,
}

impl FilterPart {
    /// Its name on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            FilterPart::Watched => "watched",
            FilterPart::Genres => "genres",
            FilterPart::Years => "years",
            FilterPart::ContentRatings => "content_ratings",
            FilterPart::Resolutions => "resolutions",
            FilterPart::DynamicRanges => "dynamic_ranges",
            FilterPart::Unknown => "unknown",
        }
    }
}

impl ItemFilter {
    /// The parts it uses.
    pub fn parts(&self) -> Vec<FilterPart> {
        let mut parts = Vec::new();
        if !self.watched.is_empty() {
            parts.push(FilterPart::Watched);
        }
        if !self.genres.is_empty() {
            parts.push(FilterPart::Genres);
        }
        if self.year_from.is_some() || self.year_to.is_some() {
            parts.push(FilterPart::Years);
        }
        if !self.content_ratings.is_empty() {
            parts.push(FilterPart::ContentRatings);
        }
        if !self.resolutions.is_empty() {
            parts.push(FilterPart::Resolutions);
        }
        if !self.dynamic_ranges.is_empty() {
            parts.push(FilterPart::DynamicRanges);
        }
        parts
    }
}

/// What a library's items have to filter by (`ItemFilterValues`), each
/// list with how many top-level items have each value. A value no item
/// has is not listed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemFilterValues {
    /// By name, case aside.
    #[serde(default)]
    pub genres: Vec<Counted<String>>,
    /// Newest first.
    #[serde(default)]
    pub years: Vec<Counted<u16>>,
    /// By name.
    #[serde(default)]
    pub content_ratings: Vec<Counted<String>>,
    /// Largest first.
    #[serde(default)]
    pub resolutions: Vec<Counted<Resolution>>,
    /// The ranges this library can be filtered by, of SDR, HDR and Dolby
    /// Vision. An HDR count includes Dolby Vision. One left out is
    /// `InvalidInput` in a filter: this server's own libraries cannot be
    /// filtered by Dolby Vision, as Plex cannot.
    #[serde(default)]
    pub dynamic_ranges: Vec<Counted<DynamicRange>>,
    /// The sort keys this library orders by, in [`SortKey::ALL`]'s order.
    /// A key left out is `InvalidInput` for this library: this server's
    /// own libraries have no `size`, as Plex does not sort by it.
    #[serde(default)]
    pub sorts: Vec<SortKey>,
    /// The parts of a filter this library applies. A part left out is
    /// `InvalidInput` for this library: a music library has no watch
    /// state, a TV library no resolution.
    #[serde(default)]
    pub filters: Vec<FilterPart>,
}

impl ItemFilterValues {
    /// Why this library cannot be ordered by `sort` or kept by `filter`,
    /// if it cannot: a key, a part or a range it does not list.
    pub fn refuses(&self, sort: Option<&ItemSort>, filter: Option<&ItemFilter>) -> Option<String> {
        if let Some(sort) = sort
            && !self.sorts.contains(&sort.key)
        {
            return Some(format!(
                "this library cannot be sorted by {}",
                sort.key.as_str()
            ));
        }
        let filter = filter?;
        if let Some(part) = filter
            .parts()
            .into_iter()
            .find(|p| !self.filters.contains(p))
        {
            return Some(format!(
                "this library cannot be filtered by {}",
                part.as_str()
            ));
        }
        if let Some(range) = filter
            .dynamic_ranges
            .iter()
            .find(|r| !self.dynamic_ranges.iter().any(|c| c.value == **r))
        {
            return Some(format!("this library cannot be filtered by {range:?}"));
        }
        None
    }
}

/// `title` as a sort title: a leading "The", "A" or "An" and the space
/// after it set aside, as Plex's `titleSort` does. A title that is only
/// the article stays as it is.
pub fn sort_title(title: &str) -> &str {
    let trimmed = title.trim_start();
    for article in ["the ", "a ", "an "] {
        if trimmed.len() > article.len()
            && trimmed.is_char_boundary(article.len())
            && trimmed[..article.len()].eq_ignore_ascii_case(article)
        {
            let rest = trimmed[article.len()..].trim_start();
            if !rest.is_empty() {
                return rest;
            }
        }
    }
    trimmed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sort_and_a_filter_are_snake_case_on_the_wire() {
        let sort = ItemSort {
            key: SortKey::LastPlayed,
            descending: true,
        };
        assert_eq!(
            serde_json::to_value(sort).unwrap(),
            serde_json::json!({"key": "last_played", "descending": true})
        );
        let filter = ItemFilter {
            watched: vec![WatchedState::InProgress],
            resolutions: vec![Resolution::Uhd, Resolution::Sd],
            dynamic_ranges: vec![DynamicRange::DolbyVision],
            year_from: Some(1990),
            ..ItemFilter::default()
        };
        assert_eq!(
            serde_json::to_value(&filter).unwrap(),
            serde_json::json!({
                "watched": ["in_progress"],
                "resolutions": ["4k", "sd"],
                "dynamic_ranges": ["dolby_vision"],
                "year_from": 1990,
            })
        );
    }

    #[test]
    fn a_sort_without_a_direction_is_ascending() {
        let sort: ItemSort = serde_json::from_value(serde_json::json!({"key": "size"})).unwrap();
        assert_eq!(
            sort,
            ItemSort {
                key: SortKey::Size,
                descending: false
            }
        );
    }

    #[test]
    fn values_from_a_newer_peer_read_as_unknown_and_are_refused() {
        let sort: ItemSort =
            serde_json::from_value(serde_json::json!({"key": "loudness"})).unwrap();
        assert_eq!(sort.key, SortKey::Unknown);
        assert_eq!(sort.check(), Err(BrowseError::UnknownSort));
        let filter: ItemFilter = serde_json::from_value(serde_json::json!({
            "resolutions": ["8k"],
        }))
        .unwrap();
        assert_eq!(filter.check(), Err(BrowseError::UnknownValue("resolution")));
        let filter: ItemFilter = serde_json::from_value(serde_json::json!({
            "watched": ["abandoned"],
        }))
        .unwrap();
        assert!(filter.check().is_err());
        let filter: ItemFilter = serde_json::from_value(serde_json::json!({
            "dynamic_ranges": ["hdr12"],
        }))
        .unwrap();
        assert!(filter.check().is_err());
    }

    #[test]
    fn years_out_of_order_and_long_lists_are_refused() {
        let years = ItemFilter {
            year_from: Some(2000),
            year_to: Some(1990),
            ..ItemFilter::default()
        };
        assert_eq!(
            years.check(),
            Err(BrowseError::Years {
                from: 2000,
                to: 1990
            })
        );
        let many = ItemFilter {
            genres: vec!["Drama".into(); MAX_FILTER_VALUES + 1],
            ..ItemFilter::default()
        };
        assert_eq!(many.check(), Err(BrowseError::TooMany("genres")));
        let long = ItemFilter {
            content_ratings: vec!["x".repeat(MAX_FILTER_TEXT + 1)],
            ..ItemFilter::default()
        };
        assert_eq!(long.check(), Err(BrowseError::TooLong("content_ratings")));
        let decade = ItemFilter {
            year_from: Some(1990),
            year_to: Some(1999),
            ..ItemFilter::default()
        };
        assert_eq!(decade.check(), Ok(()));
    }

    #[test]
    fn an_empty_filter_keeps_everything_and_reads_from_nothing() {
        let filter: ItemFilter = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(filter.is_empty());
        assert_eq!(
            serde_json::to_value(&filter).unwrap(),
            serde_json::json!({})
        );
    }

    #[test]
    fn the_sort_title_sets_a_leading_article_aside() {
        assert_eq!(sort_title("The Matrix"), "Matrix");
        assert_eq!(sort_title("a Beautiful Mind"), "Beautiful Mind");
        assert_eq!(sort_title("An Education"), "Education");
        assert_eq!(sort_title("Theremin"), "Theremin");
        assert_eq!(sort_title("Anatomy"), "Anatomy");
        assert_eq!(sort_title("The"), "The");
        assert_eq!(sort_title("Amélie"), "Amélie");
        assert_eq!(sort_title("A"), "A");
    }

    #[test]
    fn resolution_classes_go_by_height_and_cropped_width() {
        assert_eq!(
            Resolution::of(Some(3840), Some(2160)),
            Some(Resolution::Uhd)
        );
        assert_eq!(
            Resolution::of(Some(3840), Some(1600)),
            Some(Resolution::Uhd)
        );
        assert_eq!(
            Resolution::of(Some(1920), Some(800)),
            Some(Resolution::FullHd)
        );
        assert_eq!(Resolution::of(Some(1280), Some(536)), Some(Resolution::Hd));
        assert_eq!(Resolution::of(Some(720), Some(576)), Some(Resolution::Sd));
        assert_eq!(Resolution::of(None, None), None);
        assert_eq!(Resolution::of_plex("4k"), Some(Resolution::Uhd));
        assert_eq!(Resolution::of_plex("1080"), Some(Resolution::FullHd));
        assert_eq!(Resolution::of_plex("720"), Some(Resolution::Hd));
        assert_eq!(Resolution::of_plex("480"), Some(Resolution::Sd));
        assert_eq!(Resolution::of_plex("sd"), Some(Resolution::Sd));
        assert_eq!(Resolution::of_plex(""), None);
        assert_eq!(Resolution::parse("1080p"), Some(Resolution::FullHd));
    }
}
