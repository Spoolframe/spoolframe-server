//! Sorting and filtering a library's items:
//! what a catalog is asked, and what it says it can do.
//!
//! A catalog sorts and filters by what it has of the items themselves. The
//! session user's watch state is the server's, not the catalog's: a
//! catalog that keeps its items beside it (the paired servers' cache, in
//! the server's own database) applies it too, and says so
//! ([`WatchParts::InCatalog`]); one that does not (Plex) leaves the
//! `watched` filter and the `last_played` sort to the server, which walks
//! the catalog's order ([`crate::Catalog::walk_items`]) and applies them.

use spoolframe_types::{
    Counted, DynamicRange, FilterPart, ItemFilter, ItemFilterValues, ItemSort, LibraryKind,
    MediaId, SortKey,
};

/// What [`crate::Catalog::query_items`] is asked.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ItemQuery {
    pub sort: ItemSort,
    pub filter: ItemFilter,
    /// Whose watch state the `watched` filter and the `last_played` sort
    /// go by, for a catalog that applies them itself; `None` when the
    /// query has neither.
    pub viewer: Option<Viewer>,
}

impl ItemQuery {
    /// Every item, by title: the order `Items` gives without a sort.
    pub fn plain() -> Self {
        Self {
            sort: ItemSort::DEFAULT,
            filter: ItemFilter::default(),
            viewer: None,
        }
    }

    /// Whether it asks for the user's watch state.
    pub fn needs_viewer(&self) -> bool {
        self.sort.key == SortKey::LastPlayed || !self.filter.watched.is_empty()
    }

    /// This query without the parts that need the user's watch state:
    /// ordered by title instead of last played, and without the
    /// `watched` filter. What the server asks a catalog that leaves those
    /// to it, and orders the answer itself.
    pub fn without_watch_state(&self) -> Self {
        let sort = match self.sort.key {
            SortKey::LastPlayed => ItemSort::DEFAULT,
            _ => self.sort,
        };
        Self {
            sort,
            filter: ItemFilter {
                watched: Vec::new(),
                ..self.filter.clone()
            },
            viewer: None,
        }
    }
}

/// The session user, as their watch state is kept.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Viewer {
    /// As the watch state is kept: `local:<uuid>`.
    pub principal: String,
    /// The least position, in milliseconds, that counts as started: an
    /// item played less than this and not watched is unwatched.
    pub started_after_ms: u64,
}

/// Who applies the `watched` filter and the `last_played` sort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchParts {
    /// The catalog, given a [`Viewer`].
    InCatalog,
    /// The server, over [`crate::Catalog::walk_items`].
    ByServer,
}

/// What a library can be sorted and filtered by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Browsing {
    /// In [`SortKey::ALL`]'s order.
    pub sorts: Vec<SortKey>,
    pub filters: Vec<FilterPart>,
    pub dynamic_ranges: Vec<DynamicRange>,
    pub watch: WatchParts,
}

impl Browsing {
    /// Only by title, and no filter: a catalog that cannot do more.
    pub fn plain() -> Self {
        Self {
            sorts: vec![SortKey::Title],
            filters: Vec::new(),
            dynamic_ranges: Vec::new(),
            watch: WatchParts::ByServer,
        }
    }

    /// What a paired server's library of `kind`, in the cache, is sorted
    /// and filtered by: whatever its items carry (a movie every key, part
    /// and range; a show no duration, size or quality; music its title,
    /// when it was added and its genres), and the user's watch state,
    /// joined in the cache's own database.
    pub fn cached(kind: LibraryKind) -> Self {
        use FilterPart::*;
        use SortKey::*;
        let (sorts, filters, dynamic_ranges) = match kind {
            LibraryKind::Movies => (
                SortKey::ALL.to_vec(),
                vec![
                    Watched,
                    Genres,
                    Years,
                    ContentRatings,
                    Resolutions,
                    DynamicRanges,
                ],
                vec![
                    DynamicRange::Sdr,
                    DynamicRange::Hdr,
                    DynamicRange::DolbyVision,
                ],
            ),
            LibraryKind::Shows => (
                vec![Title, Released, Added, LastPlayed, Rating, AudienceRating],
                vec![Watched, Genres, Years, ContentRatings],
                Vec::new(),
            ),
            LibraryKind::Music => (vec![Title, Added], vec![Genres], Vec::new()),
        };
        Self {
            sorts,
            filters,
            dynamic_ranges,
            watch: WatchParts::InCatalog,
        }
    }

    /// Why `query` cannot be asked of this library, if it cannot.
    pub fn refuses(&self, sort: Option<&ItemSort>, filter: Option<&ItemFilter>) -> Option<String> {
        self.values().refuses(sort, filter)
    }

    /// What a client is told of it, with no values listed yet.
    pub fn values(&self) -> ItemFilterValues {
        ItemFilterValues {
            dynamic_ranges: self
                .dynamic_ranges
                .iter()
                .map(|&value| Counted { value, count: None })
                .collect(),
            sorts: self.sorts.clone(),
            filters: self.filters.clone(),
            ..ItemFilterValues::default()
        }
    }
}

/// What a library is sorted and filtered by, of one item, beyond what
/// [`spoolframe_types::Item`] carries: what a catalog's listing has of it
/// ([`crate::Catalog::items_with_facts`]), or its own record
/// ([`crate::Catalog::details`]). What a paired server is sent of an item
/// for its cache to sort and filter by. Each is `None` or empty where the
/// catalog does not say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    /// The catalog's own sort title, when it has one.
    pub sort_title: Option<String>,
    /// The day it first came out, `YYYY-MM-DD`: a show's first airing.
    pub released: Option<String>,
    /// Unix seconds.
    pub added_at: Option<i64>,
    /// 0 to 100: tenths of the 0 to 10 rating.
    pub rating_tenths: Option<u16>,
    pub audience_rating_tenths: Option<u16>,
    /// A film's first version's file size, in bytes.
    pub size: Option<u64>,
    pub resolution: Option<spoolframe_types::Resolution>,
    /// `Sdr`, `Hdr` (HDR without Dolby Vision) or `DolbyVision`.
    pub dynamic_range: Option<DynamicRange>,
    /// Every genre, in the catalog's order; a listing may have only the
    /// first few, its own record all of them.
    pub genres: Vec<String>,
}

impl Facts {
    /// A rating of 0 to 10 in tenths; `None` for one out of range.
    pub fn tenths(rating: f64) -> Option<u16> {
        let tenths = (rating * 10.0).round();
        (0.0..=100.0).contains(&tenths).then_some(tenths as u16)
    }
}

/// One item of a walk: its id, and for a show how many episodes it has,
/// what its watched state is judged by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub id: MediaId,
    pub leaves: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_types::{ItemFilter, WatchedState};

    #[test]
    fn without_the_watch_state_a_query_orders_by_title_and_keeps_its_other_parts() {
        let query = ItemQuery {
            sort: ItemSort {
                key: SortKey::LastPlayed,
                descending: true,
            },
            filter: ItemFilter {
                watched: vec![WatchedState::Watched],
                genres: vec!["Drama".into()],
                ..ItemFilter::default()
            },
            viewer: None,
        };
        assert!(query.needs_viewer());
        let bare = query.without_watch_state();
        assert!(!bare.needs_viewer());
        assert_eq!(bare.sort, ItemSort::DEFAULT);
        assert_eq!(bare.filter.genres, vec!["Drama".to_owned()]);
    }

    #[test]
    fn plain_is_title_only_and_the_cache_has_size_and_dolby_vision() {
        let size = ItemSort {
            key: SortKey::Size,
            descending: false,
        };
        let dv = ItemFilter {
            dynamic_ranges: vec![DynamicRange::DolbyVision],
            ..ItemFilter::default()
        };
        let plain = Browsing::plain();
        assert!(plain.refuses(Some(&size), None).is_some());
        assert!(plain.refuses(None, Some(&dv)).is_some());
        assert_eq!(plain.refuses(Some(&ItemSort::DEFAULT), None), None);
        let cached = Browsing::cached(LibraryKind::Movies);
        assert_eq!(cached.refuses(Some(&size), Some(&dv)), None);
    }

    #[test]
    fn a_tv_library_has_no_resolution_and_music_no_watch_state() {
        let quality = ItemFilter {
            resolutions: vec![spoolframe_types::Resolution::Uhd],
            ..ItemFilter::default()
        };
        let shows = Browsing::cached(LibraryKind::Shows);
        assert!(shows.refuses(None, Some(&quality)).is_some());
        let watched = ItemFilter {
            watched: vec![WatchedState::Unwatched],
            ..ItemFilter::default()
        };
        assert!(
            Browsing::cached(LibraryKind::Music)
                .refuses(None, Some(&watched))
                .is_some()
        );
        assert_eq!(shows.refuses(None, Some(&watched)), None);
    }
}
