//! Scoped, grouped search: `Request::Search` and its reply.

use serde::{Deserialize, Serialize};
use spoolframe_types::{Item, LibraryId, LibraryServer, MediaId, Person, SearchKind, SearchScope};

/// A search. On the wire, `{"type": "search", "query": …, "scope": …}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchRequest {
    /// What was typed. A year at its end ("dune 2021") narrows the search
    /// to that year and is not itself searched for; the reply says which
    /// year it took.
    pub query: String,
    /// The most items (or people) a group holds.
    pub limit: u32,
    pub scope: SearchScope,
    /// One group only, for "See all": up to `limit` of its items and no
    /// top match. It overrides `scope`, since people, artists, albums and
    /// tracks have no scope of their own.
    pub kind: Option<SearchKind>,
    /// Only this library's items, and the people found in it, each with
    /// their count there. A person's items (`PersonItems`) are still all
    /// of theirs.
    pub library: Option<LibraryId>,
}

/// What a search found: `Item`s, or for a client that names the
/// experimental `federation` feature [`SearchHit`]s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResults<T = Item> {
    /// The single best match: an exact title, else a title that starts
    /// with the query (case and a leading "the", "a" or "an" aside), else
    /// the first result; on a tie a show before a movie, then artist,
    /// album, track and episode, so a show beats its own episodes. A
    /// person is the top match only when their name matches better than
    /// every title (an exact "keanu reeves" over titles that start with
    /// it), or when only people are found: on a tie the item wins. Not
    /// repeated in its group. `None` for a `kind` search.
    pub top: Option<TopResult<T>>,
    /// The groups that have something to show, in the order they are
    /// shown: shows, movies, people, artists, albums, tracks, episodes. A
    /// `kind` search has that one group, even when it is empty.
    pub groups: Vec<SearchGroup<T>>,
    pub counts: SearchCounts,
    /// The year taken from the end of the query as a filter, to show as a
    /// removable token.
    pub year: Option<u16>,
    /// With a `year`: up to 3 items the query matches in other years, for
    /// "also matching without the year".
    pub without_year: Vec<T>,
}

impl<T> Default for SearchResults<T> {
    fn default() -> Self {
        Self {
            top: None,
            groups: Vec::new(),
            counts: SearchCounts::default(),
            year: None,
            without_year: Vec::new(),
        }
    }
}

impl<T> SearchResults<T> {
    /// The same results with each item made `f` of it, in place.
    pub fn map<U>(self, mut f: impl FnMut(T) -> U) -> SearchResults<U> {
        SearchResults {
            top: self.top.map(|top| match top {
                TopResult::Item(item) => TopResult::Item(f(item)),
                TopResult::Person(person) => TopResult::Person(person),
            }),
            groups: self
                .groups
                .into_iter()
                .map(|g| SearchGroup {
                    kind: g.kind,
                    items: g.items.into_iter().map(&mut f).collect(),
                    people: g.people,
                    more: g.more,
                    total: g.total,
                    hidden: g.hidden,
                })
                .collect(),
            counts: self.counts,
            year: self.year,
            without_year: self.without_year.into_iter().map(f).collect(),
        }
    }
}

/// Experimental (`federation`): a search's item for a client that
/// names the feature. A title on several servers (the same TMDB, IMDb,
/// TVDB, MusicBrainz or Plex id; an episode by its show's, season and
/// number) is one hit: this server's copy when it has one, else the copy
/// the user plays from or last played, else the first online.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct SearchHit {
    pub item: Item,
    /// The paired server `item` is from; `None` for this server's own.
    #[serde(default)]
    pub origin: Option<LibraryServer>,
    /// The title's copies on other servers ("also on Bob's Media"), in
    /// the order a Play from list shows them; empty for a title on one
    /// server.
    #[serde(default)]
    pub also_on: Vec<TitleCopy>,
    /// The copy that plays: the one on the server the user chose for the
    /// title (`SetPlayFrom`) while it has one, else `item`'s own id.
    pub play_from: MediaId,
}

impl SearchHit {
    pub fn new(item: Item, copies: Copies) -> Self {
        Self {
            item,
            origin: copies.origin,
            also_on: copies.also_on,
            play_from: copies.play_from,
        }
    }
}

/// Experimental (`federation`): where an item is from, and its
/// title's copies on other servers, as a [`SearchHit`] and an `Item`
/// reply have them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct Copies {
    /// The paired server the item is from; `None` for this server's own.
    #[serde(default)]
    pub origin: Option<LibraryServer>,
    /// The title's copies on other servers: this server's first, then the
    /// paired servers online by name, then those offline (greyed: browsed,
    /// not played).
    #[serde(default)]
    pub also_on: Vec<TitleCopy>,
    /// The copy that plays: on the server the user chose for the title
    /// (`SetPlayFrom`) while it has one, else the item's own id.
    pub play_from: MediaId,
}

/// One copy of a title, on one server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TitleCopy {
    /// The copy's id here: a `fed:` id for a paired server's.
    pub id: MediaId,
    /// Its paired server; `None` for this server's own.
    #[serde(default)]
    pub server: Option<LibraryServer>,
}

/// The top match: an item, or a person. On the wire, `{"item": {...}}`
/// or `{"person": {...}}`. A person's items are listed with
/// `Request::PersonItems`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopResult<T = Item> {
    Item(T),
    Person(Person),
}

/// One kind's results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchGroup<T = Item> {
    pub kind: SearchKind,
    /// The items, best first; empty for [`SearchKind::People`].
    pub items: Vec<T>,
    /// The people, for [`SearchKind::People`] only.
    pub people: Vec<Person>,
    /// There are more than are here: a `kind` search for this group
    /// finds them.
    pub more: bool,
    /// How many a `kind` search for this group finds, for "See all 23":
    /// the top match and the episodes under their show (`hidden`)
    /// included. For items it is the catalog's exact count. People are
    /// counted only when every one of them is here (`more` is false):
    /// Plex lists them only up to the limit.
    pub total: Option<u32>,
    /// Episodes left out because their show is in the shows group (only
    /// in `All`, where episodes are quiet), for "3 from Severance are
    /// under the show". A group may be here with only this.
    pub hidden: u32,
}

/// How many results each scope has, for the scope chips and the empty
/// state. A count is what the catalog says, not a promise: the scope's
/// own search may find a few more or fewer. `None` where it is not known
/// without asking again; a search that finds nothing in its scope counts
/// the others.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct SearchCounts {
    /// Every group, people included.
    pub all: Option<u32>,
    pub movies: Option<u32>,
    pub shows: Option<u32>,
    /// Every matching episode, quiet ones included: what the Episodes
    /// scope shows.
    pub episodes: Option<u32>,
    /// Artists, albums and tracks together.
    pub music: Option<u32>,
}

impl SearchCounts {
    /// The count for `scope`.
    pub fn of(&self, scope: SearchScope) -> Option<u32> {
        match scope {
            SearchScope::All => self.all,
            SearchScope::Movies => self.movies,
            SearchScope::Shows => self.shows,
            SearchScope::Episodes => self.episodes,
            SearchScope::Music => self.music,
        }
    }
}
