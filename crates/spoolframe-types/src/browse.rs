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
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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
        SortKey::from_wire_name(name)
    }
}

/// How `Items` orders a library: by `key`, then by sort title A to Z,
/// then by id, so that items equal by `key` keep one order and a page
/// never repeats or skips one. The tiebreakers go A to Z both ways.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ItemSort {
    pub key: SortKey,
    /// Largest, newest or last first. Missing values stay last either way.
    #[serde(default)]
    #[cfg_attr(feature = "uniffi", uniffi(default = false))]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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
        Resolution::from_wire_name(name)
    }
}

/// What an item's picture shows of brightness and colour, by its first
/// version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ItemFilter {
    /// The session user's watch state of it; never another user's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "uniffi", uniffi(default = []))]
    pub watched: Vec<WatchedState>,
    /// Any of these genres, by the catalog's names, as
    /// [`ItemFilterValues::genres`] lists them; case aside.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "uniffi", uniffi(default = []))]
    pub genres: Vec<String>,
    /// Its year is this or later. A decade is `1990` to `1999`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub year_from: Option<u16>,
    /// Its year is this or earlier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub year_to: Option<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "uniffi", uniffi(default = []))]
    pub resolutions: Vec<Resolution>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "uniffi", uniffi(default = []))]
    pub dynamic_ranges: Vec<DynamicRange>,
    /// Any of these age ratings, as the catalog has them ("PG-13",
    /// "TV-MA"); case aside.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "uniffi", uniffi(default = []))]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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

impl ItemFilterValues {
    /// The keys a Sort menu lists: the library's, in its order, each
    /// once, none this build does not know.
    pub fn sort_keys(&self) -> Vec<SortKey> {
        let mut keys: Vec<SortKey> = Vec::new();
        for &key in &self.sorts {
            if key != SortKey::Unknown && !keys.contains(&key) {
                keys.push(key);
            }
        }
        keys
    }

    /// These values without any this build does not know: no `Unknown`
    /// sort key, filter part, resolution or range, and each sort key
    /// once. What the SDK hands an app, which offers only what it can
    /// name.
    pub fn known(mut self) -> Self {
        self.sorts = self.sort_keys();
        self.filters.retain(|p| *p != FilterPart::Unknown);
        self.resolutions.retain(|c| c.value != Resolution::Unknown);
        self.dynamic_ranges
            .retain(|c| c.value != DynamicRange::Unknown);
        self
    }

    /// The decades the library has, newest first, for the year filter:
    /// 1990 for 1990 to 1999.
    pub fn decades(&self) -> Vec<u16> {
        let mut decades: Vec<u16> = self.years.iter().map(|y| y.value / 10 * 10).collect();
        decades.sort_unstable_by(|a, b| b.cmp(a));
        decades.dedup();
        decades
    }

    /// A saved `sort` and `filter` as this library can use them now: a
    /// sort key it no longer lists goes back to [`ItemSort::DEFAULT`], a
    /// part it no longer filters by is cleared, and a resolution or range
    /// it does not list is dropped (the server would refuse it, where a
    /// genre it does not list only keeps nothing). Values this build does
    /// not know go too.
    pub fn sanitize(&self, sort: ItemSort, filter: ItemFilter) -> ItemQuery {
        let sort = if sort.key != SortKey::Unknown && self.sorts.contains(&sort.key) {
            sort
        } else {
            ItemSort::DEFAULT
        };
        let offers = |part| self.filters.contains(&part);
        let mut filter = filter;
        if !offers(FilterPart::Watched) {
            filter.watched.clear();
        }
        filter.watched.retain(|w| *w != WatchedState::Unknown);
        if !offers(FilterPart::Genres) {
            filter.genres.clear();
        }
        if !offers(FilterPart::Years) {
            filter.year_from = None;
            filter.year_to = None;
        }
        if !offers(FilterPart::ContentRatings) {
            filter.content_ratings.clear();
        }
        if offers(FilterPart::Resolutions) {
            filter
                .resolutions
                .retain(|r| self.resolutions.iter().any(|c| c.value == *r));
        } else {
            filter.resolutions.clear();
        }
        if offers(FilterPart::DynamicRanges) {
            filter
                .dynamic_ranges
                .retain(|r| self.dynamic_ranges.iter().any(|c| c.value == *r));
        } else {
            filter.dynamic_ranges.clear();
        }
        ItemQuery { sort, filter }
    }
}

/// One value a filter keeps items by, as a check row or a chip names it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FilterValue {
    Watched(WatchedState),
    Genre(String),
    ContentRating(String),
    Resolution(Resolution),
    DynamicRange(DynamicRange),
}

/// What the user chose for one library: its order and what it keeps.
/// What an app saves per library, and asks `Items` with
/// ([`ItemQuery::to_request`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct ItemQuery {
    pub sort: ItemSort,
    pub filter: ItemFilter,
}

impl Default for ItemQuery {
    fn default() -> Self {
        Self {
            sort: ItemSort::DEFAULT,
            filter: ItemFilter::default(),
        }
    }
}

impl ItemQuery {
    /// The library as `Items` gives it with neither: title A to Z, every
    /// item.
    pub fn is_default(&self) -> bool {
        *self == ItemQuery::default()
    }

    /// What to ask `Items` with: the default order and an empty filter
    /// are left out, as the server's own, so the plain listing stays the
    /// one a cache keeps.
    pub fn to_request(&self) -> (Option<ItemSort>, Option<ItemFilter>) {
        let sort = (self.sort != ItemSort::DEFAULT).then_some(self.sort);
        let filter = (!self.filter.is_empty()).then(|| self.filter.clone());
        (sort, filter)
    }

    /// Ordered by `key`: picked again, the other way round; else title
    /// A to Z first, any other key largest, newest or last first.
    pub fn sort_by(&mut self, key: SortKey) {
        self.sort = if self.sort.key == key {
            ItemSort {
                key,
                descending: !self.sort.descending,
            }
        } else {
            ItemSort {
                key,
                descending: key != SortKey::Title,
            }
        };
    }

    /// Whether it keeps items with `value`; names case aside.
    pub fn has(&self, value: &FilterValue) -> bool {
        let named =
            |names: &[String], name: &str| names.iter().any(|n| n.eq_ignore_ascii_case(name));
        let filter = &self.filter;
        match value {
            FilterValue::Watched(state) => filter.watched.contains(state),
            FilterValue::Genre(name) => named(&filter.genres, name),
            FilterValue::ContentRating(name) => named(&filter.content_ratings, name),
            FilterValue::Resolution(r) => filter.resolutions.contains(r),
            FilterValue::DynamicRange(r) => filter.dynamic_ranges.contains(r),
        }
    }

    /// Keeps items with `value` too, or no longer if it did.
    pub fn toggle(&mut self, value: &FilterValue) {
        fn flip<T: PartialEq + Clone>(list: &mut Vec<T>, value: &T, same: impl Fn(&T, &T) -> bool) {
            let before = list.len();
            list.retain(|v| !same(v, value));
            if list.len() == before {
                list.push(value.clone());
            }
        }
        let names = |a: &String, b: &String| a.eq_ignore_ascii_case(b);
        let filter = &mut self.filter;
        match value {
            FilterValue::Watched(s) => flip(&mut filter.watched, s, PartialEq::eq),
            FilterValue::Genre(name) => flip(&mut filter.genres, name, names),
            FilterValue::ContentRating(name) => flip(&mut filter.content_ratings, name, names),
            FilterValue::Resolution(r) => flip(&mut filter.resolutions, r, PartialEq::eq),
            FilterValue::DynamicRange(r) => flip(&mut filter.dynamic_ranges, r, PartialEq::eq),
        }
    }

    /// From `from` to `to`, either open. A To before the From is moved up
    /// to it, so the range is never back to front (which `Items`
    /// refuses).
    pub fn set_years(&mut self, from: Option<u16>, to: Option<u16>) {
        self.filter.year_from = from;
        self.filter.year_to = match (from, to) {
            (Some(from), Some(to)) if to < from => Some(from),
            _ => to,
        };
    }

    /// Without `part`: the other parts and the sort stay.
    pub fn reset(&mut self, part: FilterPart) {
        let filter = &mut self.filter;
        match part {
            FilterPart::Watched => filter.watched.clear(),
            FilterPart::Genres => filter.genres.clear(),
            FilterPart::Years => {
                filter.year_from = None;
                filter.year_to = None;
            }
            FilterPart::ContentRatings => filter.content_ratings.clear(),
            FilterPart::Resolutions => filter.resolutions.clear(),
            FilterPart::DynamicRanges => filter.dynamic_ranges.clear(),
            FilterPart::Unknown => {}
        }
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

    fn values() -> ItemFilterValues {
        ItemFilterValues {
            resolutions: vec![Counted {
                value: Resolution::Uhd,
                count: None,
            }],
            dynamic_ranges: vec![Counted {
                value: DynamicRange::Hdr,
                count: None,
            }],
            sorts: vec![
                SortKey::Title,
                SortKey::Added,
                SortKey::Added,
                SortKey::Unknown,
            ],
            filters: vec![
                FilterPart::Genres,
                FilterPart::Resolutions,
                FilterPart::DynamicRanges,
            ],
            ..ItemFilterValues::default()
        }
    }

    /// A newer server's values this build can't name never reach an app.
    #[test]
    fn only_the_values_this_build_knows_are_kept() {
        let mut from_newer = values();
        from_newer.filters.push(FilterPart::Unknown);
        from_newer.resolutions.push(Counted {
            value: Resolution::Unknown,
            count: Some(3),
        });
        from_newer.dynamic_ranges.push(Counted {
            value: DynamicRange::Unknown,
            count: None,
        });
        let known = from_newer.known();
        assert_eq!(
            (
                known.sorts,
                known.filters,
                known.resolutions.len(),
                known.dynamic_ranges.len()
            ),
            (
                vec![SortKey::Title, SortKey::Added],
                vec![
                    FilterPart::Genres,
                    FilterPart::Resolutions,
                    FilterPart::DynamicRanges
                ],
                1,
                1
            )
        );
    }

    #[test]
    fn nothing_chosen_asks_the_plain_listing() {
        assert!(ItemQuery::default().is_default());
        assert_eq!(ItemQuery::default().to_request(), (None, None));
    }

    #[test]
    fn a_sort_alone_asks_for_that_order_and_no_filter() {
        let mut query = ItemQuery::default();
        query.sort_by(SortKey::Added);
        let added = ItemSort {
            key: SortKey::Added,
            descending: true,
        };
        assert_eq!(query.to_request(), (Some(added), None));
        query.sort_by(SortKey::Added);
        assert!(!query.sort.descending, "picked again, the other way round");
    }

    #[test]
    fn a_value_toggles_in_and_out_case_aside() {
        let mut query = ItemQuery::default();
        query.toggle(&FilterValue::Genre("Drama".into()));
        assert!(query.has(&FilterValue::Genre("drama".into())));
        query.toggle(&FilterValue::Genre("DRAMA".into()));
        assert!(query.is_default());
    }

    #[test]
    fn a_year_range_is_never_back_to_front() {
        let mut query = ItemQuery::default();
        query.set_years(Some(2000), Some(1990));
        assert_eq!(
            (query.filter.year_from, query.filter.year_to),
            (Some(2000), Some(2000))
        );
        assert!(query.filter.check().is_ok());
        query.set_years(None, Some(1990));
        assert_eq!(
            (query.filter.year_from, query.filter.year_to),
            (None, Some(1990))
        );
    }

    #[test]
    fn a_saved_choice_keeps_only_what_the_library_still_offers() {
        let saved = ItemFilter {
            watched: vec![WatchedState::Watched],
            genres: vec!["Drama".into()],
            resolutions: vec![Resolution::Uhd, Resolution::Sd],
            dynamic_ranges: vec![DynamicRange::DolbyVision],
            year_from: Some(1990),
            ..ItemFilter::default()
        };
        let rating = ItemSort {
            key: SortKey::Rating,
            descending: true,
        };
        let query = values().sanitize(rating, saved);
        assert_eq!(query.sort, ItemSort::DEFAULT);
        assert_eq!(
            query.filter,
            ItemFilter {
                genres: vec!["Drama".into()],
                resolutions: vec![Resolution::Uhd],
                ..ItemFilter::default()
            }
        );
        assert_eq!(
            values().refuses(Some(&query.sort), Some(&query.filter)),
            None
        );
    }

    #[test]
    fn decades_are_the_librarys_years_newest_first() {
        let values = ItemFilterValues {
            years: [1994, 2023, 1999, 2020]
                .map(|value| Counted { value, count: None })
                .to_vec(),
            ..ItemFilterValues::default()
        };
        assert_eq!(values.decades(), [2020, 1990]);
    }

    #[test]
    fn the_sort_menu_lists_the_librarys_known_keys_once() {
        assert_eq!(values().sort_keys(), [SortKey::Title, SortKey::Added]);
    }

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
