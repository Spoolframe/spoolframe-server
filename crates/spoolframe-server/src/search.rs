//! Scoped, grouped search (P3-09): a client's search as the catalog is
//! asked it, and what the catalog found as the client is shown it.
//!
//! The catalog applies the scope and the year; this picks the top match,
//! leaves quiet episodes out of `All`, cuts each group to the limit and
//! counts the scopes, once every Plex server's results are merged. For a
//! client that names the experimental `federation` feature the paired
//! servers' cache is searched too, and a title found on several servers
//! is merged into one result before the top match and the groups (P8-05).

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use spoolframe_catalog::{Catalog, CatalogError, Found, FoundGroup, SearchQuery};
use spoolframe_protocol::search_rank::{Matched, Ranker};
use spoolframe_protocol::{SearchRequest, SearchResults, TopResult};
use spoolframe_types::{Item, MediaId, SearchKind};

// The ranking rules, shared with the SDK's offline search.
#[cfg(test)]
use spoolframe_protocol::search_rank::QUIET_EPISODES;
use spoolframe_protocol::search_rank::WITHOUT_YEAR;
#[cfg(test)]
use spoolframe_protocol::search_rank::counts;
use spoolframe_protocol::search_rank::{MEDIA, Parsed, parse};
pub use spoolframe_protocol::search_rank::{this_year, year_of};
#[cfg(test)]
use spoolframe_protocol::{SearchCounts, SearchGroup};
#[cfg(test)]
use spoolframe_types::SearchScope;

/// The schemes of the external ids a title is matched across servers by
/// (P8-05): TMDB, IMDb, TVDB, MusicBrainz and Plex's own guid. Any other
/// (`local://`, an old agent's, a test's) names nothing outside its
/// server, and a title with none of these is never merged.
const MATCHED: [&str; 5] = ["tmdb://", "imdb://", "tvdb://", "mbid://", "plex://"];

/// The ids of `title` ([`Found::titles`], the watch state's title key)
/// that match its copies on other servers.
pub fn matched(title: &[String]) -> Vec<String> {
    title
        .iter()
        .filter(|id| MATCHED.iter().any(|scheme| id.starts_with(scheme)))
        .cloned()
        .collect()
}

/// What decides which copy of a title found only on paired servers a
/// search shows (P8-05): this server's own copy always goes first.
#[async_trait]
pub trait Preferred: Send + Sync {
    /// Of `copies` of the title `title`, none of them this server's, the
    /// one the user plays the title from or played last; `None` when it
    /// is neither of them.
    async fn preferred(&self, title: &[String], copies: &[MediaId]) -> Option<MediaId>;

    /// Whether `id`'s server is online: its copy plays now.
    fn online(&self, id: &MediaId) -> bool;
}

/// The paired servers' cache searched beside the server's own catalog,
/// and which copy of a title found on several of them is shown.
pub struct Peers<'a> {
    pub cache: &'a dyn Catalog,
    pub preferred: &'a dyn Preferred,
}

/// What a search found, and what of it is a title on several servers.
#[derive(Debug, Default)]
pub struct Searched {
    pub results: SearchResults,
    /// The other copies of each item shown that is a title found on
    /// several servers, by its id, best first.
    pub copies: HashMap<MediaId, Vec<MediaId>>,
    /// The title of each item found ([`Found::titles`]), by its id.
    pub titles: HashMap<MediaId, Vec<String>>,
}

/// Answer `request` from `catalog`. Counting the other scopes and finding
/// items in other years are extras: when they fail, the results go
/// without them.
pub async fn search(
    catalog: &dyn Catalog,
    request: &SearchRequest,
    this_year: u16,
) -> Result<SearchResults, CatalogError> {
    Ok(search_with(catalog, None, request, this_year)
        .await?
        .results)
}

/// [`search`], over `peers` too: the paired servers' cache (experimental
/// `federation`, P8-02), for a client that named the feature. A search
/// over every library finds the cache's items after the server's own in
/// each group; one scoped to a library asks only `catalog`, which sends a
/// paired server's library to the cache itself.
///
/// A title found on several servers is one result (P8-05), merged after
/// the backends answer and before the top match and the groups: this
/// server's copy, else the one [`Preferred`] says, else the first online,
/// else the first. Totals count each copy the backends counted, less the
/// copies merged among those found: approximate, as counts are.
pub async fn search_with(
    catalog: &dyn Catalog,
    peers: Option<&Peers<'_>>,
    request: &SearchRequest,
    this_year: u16,
) -> Result<Searched, CatalogError> {
    let Parsed { text, year } = parse(&request.query, this_year);
    if text.is_empty() {
        return Ok(Searched::default());
    }
    let kinds = match request.kind {
        Some(kind) => vec![kind],
        None => request.scope.kinds().to_vec(),
    };
    let query = SearchQuery {
        text,
        year,
        kinds,
        library: request.library.clone(),
        // One more than shown: the top match is taken out of its group,
        // and a group of people knows there are more only by getting one.
        limit: request.limit.saturating_add(1),
    };
    // Items in other years, asked alongside the search itself: it does not
    // depend on what the search finds (review 2026-10-11 L27).
    let any_year = year.and_then(|_| {
        let kinds: Vec<_> = query
            .kinds
            .iter()
            .copied()
            .filter(|&k| k != SearchKind::People)
            .collect();
        (!kinds.is_empty()).then(|| SearchQuery {
            year: None,
            kinds,
            // Some of them are this year's, already found.
            limit: 2 * WITHOUT_YEAR as u32,
            ..query.clone()
        })
    });
    let (found, any_year) = tokio::join!(found_in(catalog, peers, &query), async {
        match &any_year {
            Some(any_year) => Some(found_in(catalog, peers, any_year).await),
            None => None,
        }
    });
    let (found, mut copies) = found?;

    let mut totals: HashMap<SearchKind, u32> =
        found.groups.iter().map(|g| (g.kind, g.total)).collect();
    if request.kind.is_none() && totals.values().all(|&t| t == 0) {
        // Nothing in this scope: the empty state offers the others.
        let others: Vec<_> = MEDIA
            .into_iter()
            .filter(|k| !query.kinds.contains(k))
            .collect();
        if !others.is_empty() {
            // Counted without items (P8-05): a title on this server and a
            // paired one is counted once per server, as nothing found says
            // they are one. Plex counts its own, so the store cannot count
            // titles across both; the counts say approximate.
            let counting = SearchQuery {
                kinds: others,
                limit: 0,
                ..query.clone()
            };
            match found_in(catalog, peers, &counting).await {
                Ok((counted, _)) => {
                    totals.extend(counted.groups.iter().map(|g| (g.kind, g.total)));
                }
                Err(e) => tracing::warn!(error = %e, "search: the other scopes are not counted"),
            }
        }
    }

    let mut titles = found.titles.clone();
    let mut also = Vec::new();
    if let (Some(year), Some(any_year)) = (year, any_year) {
        match any_year {
            Ok((any_year, more)) => {
                titles.extend(any_year.titles.clone());
                also = without_year(&query.text, year, &found, any_year);
                for item in &also {
                    if let Some(other) = more.get(item.id()) {
                        copies.insert(item.id().clone(), other.clone());
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "search: nothing found without the year"),
        }
    }

    let results = group(request, &query.text, found, &totals, year, also, &copies);
    copies.retain(|id, _| shown(&results).any(|i| i == id));
    Ok(Searched {
        results,
        copies,
        titles,
    })
}

/// The ids of every item `results` shows.
fn shown(results: &SearchResults) -> impl Iterator<Item = &MediaId> {
    let top = match &results.top {
        Some(TopResult::Item(item)) => Some(item),
        _ => None,
    };
    top.into_iter()
        .chain(results.groups.iter().flat_map(|g| &g.items))
        .chain(&results.without_year)
        .map(Item::id)
}

/// Every item `results` shows, to fill in: the top one, the groups' and
/// those of other years.
pub fn items_mut<T>(results: &mut SearchResults<T>) -> impl Iterator<Item = &mut T> {
    let top = match &mut results.top {
        Some(TopResult::Item(item)) => Some(item),
        _ => None,
    };
    top.into_iter()
        .chain(results.groups.iter_mut().flat_map(|g| g.items.iter_mut()))
        .chain(results.without_year.iter_mut())
}

/// What `catalog` finds for `query`, and after it in each group what
/// `peers` finds, when there are peers and `query` covers every library:
/// a title found on several servers once ([`merged`]), with its other
/// copies by the shown one's id. The peers failing costs only their
/// items.
async fn found_in(
    catalog: &dyn Catalog,
    peers: Option<&Peers<'_>>,
    query: &SearchQuery,
) -> Result<(Found, HashMap<MediaId, Vec<MediaId>>), CatalogError> {
    let Some(peers) = peers.filter(|_| query.library.is_none()) else {
        return Ok((catalog.search(query).await?, HashMap::new()));
    };
    let (own, theirs) = tokio::join!(catalog.search(query), peers.cache.search(query));
    let mut own = own?;
    let theirs = match theirs {
        Ok(theirs) => theirs,
        Err(e) => {
            tracing::warn!(error = %e, "search: the paired servers' cache is left out");
            return Ok((own, HashMap::new()));
        }
    };
    own.titles.extend(theirs.titles);
    let limit = usize::try_from(query.limit).unwrap_or(usize::MAX);
    let mut copies = HashMap::new();
    for group in &mut own.groups {
        let Some(more) = theirs.groups.iter().find(|g| g.kind == group.kind) else {
            continue;
        };
        let items = std::mem::take(&mut group.items)
            .into_iter()
            .chain(more.items.iter().cloned())
            .collect();
        let (mut items, merged) = merged(items, &own.titles, peers.preferred).await;
        let gone: usize = merged.values().map(Vec::len).sum();
        items.truncate(limit);
        group.items = items;
        group.total = group
            .total
            .saturating_add(more.total)
            .saturating_sub(u32::try_from(gone).unwrap_or(u32::MAX));
        copies.extend(merged);
    }
    Ok((own, copies))
}

/// One kind's `items`, this server's then the paired servers', each title
/// found on several servers once (P8-05): two items are one title when
/// they share an id [`matched`] by (an episode's show's, season and number
/// among them, in `titles`), are of the same year when both have one, and
/// are on different servers. The year is checked because a paired server's
/// ids are its word only: one that says `tmdb://348` of another year's film
/// is not merged with it. Two copies on
/// one server stay two, as their watch state does. The copy shown is this
/// server's; without one, the one `preferred` says, else the first online,
/// else the first. In the order each title was first found; with the other
/// copies, best first, by the shown one's id.
async fn merged(
    items: Vec<Item>,
    titles: &HashMap<MediaId, Vec<String>>,
    preferred: &dyn Preferred,
) -> (Vec<Item>, HashMap<MediaId, Vec<MediaId>>) {
    struct Title {
        ids: HashSet<String>,
        year: Option<u16>,
        copies: Vec<Item>,
    }
    let mut found: Vec<Title> = Vec::new();
    for item in items {
        let ids: HashSet<String> = titles
            .get(item.id())
            .map(|title| matched(title).into_iter().collect())
            .unwrap_or_default();
        let server = item.id().origin().map(str::to_owned);
        let year = year_of(&item);
        let same = found.iter_mut().find(|t| {
            !ids.is_disjoint(&t.ids)
                && same_year(t.year, year)
                && t.copies
                    .iter()
                    .all(|c| c.id().origin() != server.as_deref())
        });
        match same {
            Some(title) => {
                title.ids.extend(ids);
                title.year = title.year.or(year);
                title.copies.push(item);
            }
            None => found.push(Title {
                ids,
                year,
                copies: vec![item],
            }),
        }
    }
    let mut shown = Vec::with_capacity(found.len());
    let mut copies = HashMap::new();
    for Title {
        ids,
        copies: mut all,
        ..
    } in found
    {
        if all.len() > 1 {
            let at = match all.iter().position(|c| c.id().origin().is_none()) {
                Some(at) => at,
                None => {
                    let title: Vec<String> = ids.into_iter().collect();
                    let ids: Vec<MediaId> = all.iter().map(|c| c.id().clone()).collect();
                    let chosen = preferred.preferred(&title, &ids).await;
                    chosen
                        .and_then(|chosen| ids.iter().position(|id| *id == chosen))
                        .or_else(|| ids.iter().position(|id| preferred.online(id)))
                        .unwrap_or(0)
                }
            };
            let item = all.remove(at);
            copies.insert(
                item.id().clone(),
                all.iter().map(|c| c.id().clone()).collect(),
            );
            shown.push(item);
        } else {
            shown.extend(all);
        }
    }
    (shown, copies)
}

/// What a client is shown of `found` ([`Ranker::group`]). A show's own
/// episodes are quiet under any of its copies (`copies`).
fn group(
    request: &SearchRequest,
    text: &str,
    found: Found,
    totals: &HashMap<SearchKind, u32>,
    year: Option<u16>,
    without_year: Vec<Item>,
    copies: &HashMap<MediaId, Vec<MediaId>>,
) -> SearchResults {
    let query = Parsed {
        text: text.to_owned(),
        year,
    };
    Ranker::default().group(
        request,
        &query,
        matched_of(found.groups),
        totals,
        without_year,
        copies,
    )
}

/// Up to [`WITHOUT_YEAR`] of `any_year` that are not of `year` and not
/// already found, best first ([`Ranker::without_year`]).
fn without_year(text: &str, year: u16, found: &Found, any_year: Found) -> Vec<Item> {
    Ranker::default().without_year(
        text,
        year,
        &matched_of(found.groups.clone()),
        matched_of(any_year.groups),
    )
}

/// The catalog's groups as the ranking takes them.
fn matched_of(groups: Vec<FoundGroup>) -> Vec<Matched> {
    groups
        .into_iter()
        .map(|g| Matched {
            kind: g.kind,
            items: g.items,
            people: g.people,
            total: g.total,
        })
        .collect()
}

/// Whether two copies' years let them be one title: the same, or one
/// unknown (P8-05).
pub fn same_year(a: Option<u16>, b: Option<u16>) -> bool {
    a.zip(b).is_none_or(|(a, b)| a == b)
}

#[cfg(test)]
mod recorded;

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_types::{
        Artist, Episode, Movie, Namespace, Person, PersonId, PersonRole, PlaybackState, Show,
    };

    fn id(kind: &str, key: &str) -> MediaId {
        format!("plex:{kind}:{key}").parse().unwrap()
    }

    fn movie(key: &str, title: &str, year: u16) -> Item {
        Item::Movie(Movie {
            merge: Default::default(),
            id: id("movie", key),
            title: title.into(),
            year: Some(year),
            release_date: None,
            summary: None,
            content_rating: None,
            duration: None,
            cast: Vec::new(),
            artwork: Vec::new(),
            playback: PlaybackState::default(),
        })
    }

    fn show(key: &str, title: &str) -> Item {
        Item::Show(Show {
            merge: Default::default(),
            id: id("show", key),
            title: title.into(),
            year: None,
            summary: None,
            content_rating: None,
            season_count: None,
            genres: Vec::new(),
            last_aired: None,
            cast: Vec::new(),
            artwork: Vec::new(),
        })
    }

    fn episode(key: &str, title: &str, show: &str) -> Item {
        Item::Episode(Episode {
            merge: Default::default(),
            id: id("episode", key),
            show_id: id("show", show),
            show_title: None,
            season_id: id("season", "1"),
            season: 1,
            episode: 1,
            title: title.into(),
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

    fn found(kind: SearchKind, items: Vec<Item>) -> FoundGroup {
        FoundGroup {
            total: items.len() as u32,
            items,
            ..FoundGroup::empty(kind)
        }
    }

    fn request(scope: SearchScope, kind: Option<SearchKind>, limit: u32) -> SearchRequest {
        SearchRequest {
            query: String::new(),
            limit,
            scope,
            kind,
            library: None,
        }
    }

    fn grouped(request: &SearchRequest, text: &str, groups: Vec<FoundGroup>) -> SearchResults {
        let found = Found::of(groups);
        let totals = found.groups.iter().map(|g| (g.kind, g.total)).collect();
        group(
            request,
            text,
            found,
            &totals,
            None,
            Vec::new(),
            &HashMap::new(),
        )
    }

    fn keys(items: &[Item]) -> Vec<String> {
        items.iter().map(|i| i.id().to_string()).collect()
    }

    fn of(results: &SearchResults, kind: SearchKind) -> Option<&SearchGroup> {
        results.groups.iter().find(|g| g.kind == kind)
    }

    #[test]
    fn a_trailing_year_is_taken_off_the_query() {
        assert_eq!(
            parse("  dune   2021 ", 2026),
            Parsed {
                text: "dune".into(),
                year: Some(2021)
            }
        );
    }

    #[test]
    fn a_number_that_cannot_be_a_release_year_stays_in_the_query() {
        for query in ["blade runner 2049", "1917", "dune 1899", "dune 20210"] {
            assert_eq!(parse(query, 2026).year, None, "{query}");
            assert_eq!(parse(query, 2026).text, query, "{query}");
        }
        assert_eq!(parse("dune 2027", 2026).year, Some(2027));
    }

    #[test]
    fn an_exact_title_beats_one_that_starts_with_the_query() {
        let results = grouped(
            &request(SearchScope::All, None, 10),
            "alien",
            vec![found(
                SearchKind::Movies,
                vec![
                    movie("1", "Alien: Romulus", 2024),
                    movie("2", "Aliens", 1986),
                    movie("3", "Alien", 1979),
                ],
            )],
        );
        assert_eq!(top_item(&results), "plex:movie:3");
    }

    #[test]
    fn a_leading_article_does_not_count_against_an_exact_title() {
        let results = grouped(
            &request(SearchScope::All, None, 10),
            "cure",
            vec![
                found(
                    SearchKind::Artists,
                    vec![Item::Artist(Artist {
                        merge: Default::default(),
                        id: id("artist", "1"),
                        name: "The Cure".into(),
                        summary: None,
                        artwork: Vec::new(),
                    })],
                ),
                found(
                    SearchKind::Movies,
                    vec![movie("2", "Cure for the Itch", 2000)],
                ),
            ],
        );
        assert_eq!(top_item(&results), "plex:artist:1");
    }

    #[test]
    fn a_title_that_starts_with_the_query_beats_one_that_contains_it() {
        let results = grouped(
            &request(SearchScope::All, None, 10),
            "star",
            vec![found(
                SearchKind::Movies,
                vec![
                    movie("1", "Battlestar Galactica: Razor", 2007),
                    movie("2", "Stargate: Continuum", 2008),
                ],
            )],
        );
        assert_eq!(top_item(&results), "plex:movie:2");
    }

    #[test]
    fn without_a_title_match_the_top_is_the_first_result() {
        let results = grouped(
            &request(SearchScope::All, None, 10),
            "gate",
            vec![found(
                SearchKind::Movies,
                vec![movie("1", "Stargate", 1994), movie("2", "Watergate", 2018)],
            )],
        );
        assert_eq!(top_item(&results), "plex:movie:1");
    }

    #[test]
    fn a_show_beats_its_own_episode_of_the_same_title() {
        let results = grouped(
            &request(SearchScope::All, None, 10),
            "the sopranos",
            vec![
                found(
                    SearchKind::Episodes,
                    vec![episode("9", "The Sopranos", "5")],
                ),
                found(SearchKind::Shows, vec![show("5", "The Sopranos")]),
            ],
        );
        assert_eq!(top_item(&results), "plex:show:5");
    }

    #[test]
    fn the_top_match_is_not_repeated_in_its_group() {
        let results = grouped(
            &request(SearchScope::Movies, None, 10),
            "alien",
            vec![found(
                SearchKind::Movies,
                vec![movie("1", "Alien", 1979), movie("2", "Aliens", 1986)],
            )],
        );
        let movies = of(&results, SearchKind::Movies).unwrap();
        assert_eq!(keys(&movies.items), ["plex:movie:2"]);
        assert!(!movies.more);
    }

    /// "2 from Letterkenny Live are under the show": a show listed in the
    /// shows group holds its own episodes in `All`.
    #[test]
    fn in_all_a_listed_show_s_own_episodes_are_left_out_and_counted() {
        let results = grouped(
            &request(SearchScope::All, None, 10),
            "letterkenny",
            vec![
                found(
                    SearchKind::Shows,
                    vec![show("5", "Letterkenny"), show("4", "Letterkenny Live")],
                ),
                found(
                    SearchKind::Episodes,
                    vec![
                        episode("6", "Letterkenny Talent Show", "4"),
                        episode("7", "The Letterkenny Leave", "4"),
                        episode("8", "Letterkenny Problems", "99"),
                    ],
                ),
            ],
        );
        assert_eq!(top_item(&results), "plex:show:5");
        let shows = of(&results, SearchKind::Shows).unwrap();
        assert_eq!(keys(&shows.items), ["plex:show:4"]);
        let episodes = of(&results, SearchKind::Episodes).unwrap();
        assert_eq!(keys(&episodes.items), ["plex:episode:8"]);
        assert_eq!(episodes.hidden, 2);
        assert!(!episodes.more);
    }

    /// The top match is no shows group: its episodes are listed, as there
    /// is no row they would be under.
    #[test]
    fn in_all_the_top_match_show_s_own_episodes_are_listed() {
        let results = grouped(
            &request(SearchScope::All, None, 10),
            "black",
            vec![
                found(SearchKind::Shows, vec![show("5", "Black Sails")]),
                found(SearchKind::Episodes, vec![episode("6", "II.", "5")]),
            ],
        );
        assert_eq!(top_item(&results), "plex:show:5");
        let episodes = of(&results, SearchKind::Episodes).unwrap();
        assert_eq!(keys(&episodes.items), ["plex:episode:6"]);
        assert_eq!(episodes.hidden, 0);
    }

    /// A group is there only with an item: one whose only item is the top
    /// match, or whose items are all under their show, is left out.
    #[test]
    fn in_all_a_group_with_no_items_is_left_out() {
        let results = grouped(
            &request(SearchScope::All, None, 10),
            "letterkenny",
            vec![
                found(
                    SearchKind::Shows,
                    vec![show("5", "Letterkenny"), show("4", "Letterkenny Live")],
                ),
                found(
                    SearchKind::Episodes,
                    vec![episode("6", "Letterkenny Talent Show", "4")],
                ),
                found(SearchKind::Movies, vec![movie("1", "Letterkenny", 2016)]),
            ],
        );
        let kinds: Vec<_> = results.groups.iter().map(|g| g.kind).collect();
        assert_eq!(kinds, [SearchKind::Shows, SearchKind::Movies]);
        assert!(results.groups.iter().all(|g| !g.items.is_empty()));
    }

    #[test]
    fn in_all_episodes_come_last_and_few() {
        let many: Vec<_> = (1..=6)
            .map(|n| episode(&n.to_string(), &format!("Pilot {n}"), "99"))
            .collect();
        let results = grouped(
            &request(SearchScope::All, None, 10),
            "zzz",
            vec![
                found(SearchKind::Episodes, many),
                found(
                    SearchKind::Movies,
                    vec![movie("1", "Pilot", 2000), movie("2", "Pilots", 2001)],
                ),
            ],
        );
        let kinds: Vec<_> = results.groups.iter().map(|g| g.kind).collect();
        assert_eq!(kinds, [SearchKind::Movies, SearchKind::Episodes]);
        let episodes = of(&results, SearchKind::Episodes).unwrap();
        assert_eq!(episodes.items.len(), QUIET_EPISODES);
        assert!(episodes.more);
    }

    #[test]
    fn under_the_episodes_scope_episodes_are_shown_in_full() {
        let six: Vec<_> = (1..=6)
            .map(|n| episode(&n.to_string(), &format!("Letterkenny {n}"), "5"))
            .collect();
        let results = grouped(
            &request(SearchScope::Episodes, None, 10),
            "zzz",
            vec![found(SearchKind::Episodes, six)],
        );
        // One is the top match; the others are all here.
        assert!(matches!(
            results.top,
            Some(TopResult::Item(Item::Episode(_)))
        ));
        let episodes = of(&results, SearchKind::Episodes).unwrap();
        assert_eq!(episodes.items.len(), 5);
        assert_eq!(episodes.hidden, 0);
    }

    #[test]
    fn more_says_the_catalog_has_more_than_are_shown() {
        let mut movies = found(
            SearchKind::Movies,
            vec![movie("1", "A", 1), movie("2", "B", 2), movie("3", "C", 3)],
        );
        movies.total = 233;
        let results = grouped(&request(SearchScope::Movies, None, 2), "zzz", vec![movies]);
        let movies = of(&results, SearchKind::Movies).unwrap();
        assert_eq!(movies.items.len(), 2);
        assert!(movies.more);
    }

    #[test]
    fn a_group_says_how_many_see_all_finds() {
        let mut movies = found(
            SearchKind::Movies,
            vec![
                movie("1", "Alien", 1979),
                movie("2", "Aliens", 1986),
                movie("3", "Alien 3", 1992),
            ],
        );
        movies.total = 23;
        let results = grouped(&request(SearchScope::All, None, 2), "alien", vec![movies]);
        // The top match is out of its group but still one of the 23 a
        // search for movies alone finds.
        assert_eq!(top_item(&results), "plex:movie:1");
        let movies = of(&results, SearchKind::Movies).unwrap();
        assert_eq!(keys(&movies.items), ["plex:movie:2", "plex:movie:3"]);
        assert_eq!(movies.total, Some(23));
    }

    #[test]
    fn episodes_under_their_show_count_in_their_group_s_total() {
        let results = grouped(
            &request(SearchScope::All, None, 10),
            "letterkenny",
            vec![
                found(SearchKind::Shows, vec![show("5", "Letterkenny")]),
                found(
                    SearchKind::Episodes,
                    vec![
                        episode("6", "Letterkenny Talent Show", "5"),
                        episode("7", "The Letterkenny Leave", "5"),
                        episode("8", "Letterkenny Problems", "99"),
                    ],
                ),
            ],
        );
        assert_eq!(of(&results, SearchKind::Episodes).unwrap().total, Some(3));
    }

    fn person(key: &str, name: &str) -> Person {
        Person {
            id: PersonId::new(Namespace::Plex, key).unwrap(),
            name: name.into(),
            role: PersonRole::Actor,
            count: Some(1),
        }
    }

    fn people(names: &[&str]) -> FoundGroup {
        let people: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(n, name)| person(&format!("actor-{n}"), name))
            .collect();
        FoundGroup {
            total: people.len() as u32,
            people,
            ..FoundGroup::empty(SearchKind::People)
        }
    }

    fn top_person(results: &SearchResults) -> &str {
        match &results.top {
            Some(TopResult::Person(p)) => &p.name,
            other => panic!("not a person: {other:?}"),
        }
    }

    fn top_item(results: &SearchResults) -> String {
        match &results.top {
            Some(TopResult::Item(i)) => i.id().to_string(),
            other => panic!("not an item: {other:?}"),
        }
    }

    #[test]
    fn an_exact_name_beats_titles_that_only_start_with_it() {
        let results = grouped(
            &request(SearchScope::All, None, 10),
            "keanu reeves",
            vec![
                found(
                    SearchKind::Movies,
                    vec![movie("1", "Keanu Reeves: Unscripted", 2020)],
                ),
                people(&["Keanu Reeves"]),
            ],
        );
        assert_eq!(top_person(&results), "Keanu Reeves");
        // Not repeated in its group, which is left out with nobody else.
        assert_eq!(of(&results, SearchKind::People), None);
        assert_eq!(
            keys(&of(&results, SearchKind::Movies).unwrap().items),
            ["plex:movie:1"]
        );
    }

    #[test]
    fn a_title_as_good_as_a_name_wins() {
        let results = grouped(
            &request(SearchScope::All, None, 10),
            "madonna",
            vec![
                people(&["Madonna"]),
                found(
                    SearchKind::Artists,
                    vec![Item::Artist(Artist {
                        merge: Default::default(),
                        id: id("artist", "1"),
                        name: "Madonna".into(),
                        summary: None,
                        artwork: Vec::new(),
                    })],
                ),
            ],
        );
        assert_eq!(top_item(&results), "plex:artist:1");
        let tom = grouped(
            &request(SearchScope::All, None, 10),
            "tom",
            vec![
                found(SearchKind::Shows, vec![show("5", "Tom & Jerry")]),
                people(&["Tom Hanks"]),
            ],
        );
        assert_eq!(top_item(&tom), "plex:show:5");
    }

    #[test]
    fn with_only_people_found_the_first_is_the_top_and_still_counted() {
        let results = grouped(
            &request(SearchScope::All, None, 10),
            "keanu",
            vec![people(&["Keanu Reeves", "Keanu Karim", "Keanu Lam"])],
        );
        assert_eq!(top_person(&results), "Keanu Reeves");
        let group = of(&results, SearchKind::People).unwrap();
        let names: Vec<_> = group.people.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Keanu Karim", "Keanu Lam"]);
        assert!(!group.more);
        assert_eq!(group.total, Some(3));
    }

    #[test]
    fn a_kind_search_for_people_has_no_top() {
        let results = grouped(
            &request(SearchScope::All, Some(SearchKind::People), 10),
            "keanu",
            vec![people(&["Keanu Reeves"])],
        );
        assert_eq!(results.top, None);
        assert_eq!(of(&results, SearchKind::People).unwrap().people.len(), 1);
    }

    #[test]
    fn people_are_counted_only_when_all_of_them_are_here() {
        // An exact title, so that neither person is the top match.
        let zzz = || found(SearchKind::Movies, vec![movie("1", "Zzz", 2000)]);
        let few = grouped(
            &request(SearchScope::All, None, 3),
            "zzz",
            vec![zzz(), people(&["Keanu Reeves", "Keanu Akana"])],
        );
        assert_eq!(of(&few, SearchKind::People).unwrap().total, Some(2));
        // Plex lists people only up to the limit, so more than fit is
        // not a count.
        let many = grouped(
            &request(SearchScope::All, None, 1),
            "zzz",
            vec![zzz(), people(&["Keanu Reeves", "Keanu Akana"])],
        );
        let group = of(&many, SearchKind::People).unwrap();
        assert!(group.more);
        assert_eq!(group.total, None);
    }

    #[test]
    fn a_kind_search_is_that_one_group_without_a_top_even_empty() {
        let results = grouped(
            &request(SearchScope::All, Some(SearchKind::Movies), 10),
            "alien",
            vec![found(SearchKind::Movies, Vec::new())],
        );
        assert_eq!(results.top, None);
        assert_eq!(results.groups.len(), 1);
        assert_eq!(results.groups[0].kind, SearchKind::Movies);
    }

    #[test]
    fn empty_groups_are_left_out() {
        let results = grouped(
            &request(SearchScope::All, None, 10),
            "alien",
            vec![
                found(SearchKind::Shows, Vec::new()),
                found(SearchKind::Movies, vec![movie("1", "Alien", 1979)]),
            ],
        );
        // The one movie is the top match: nothing is left to group.
        assert!(results.groups.is_empty());
    }

    #[test]
    fn counts_sum_music_and_all_and_leave_out_what_is_not_known() {
        let totals = HashMap::from([
            (SearchKind::Movies, 233),
            (SearchKind::Shows, 1),
            (SearchKind::Episodes, 4),
            (SearchKind::Artists, 1),
            (SearchKind::Albums, 5),
            (SearchKind::Tracks, 2),
            (SearchKind::People, 3),
        ]);
        assert_eq!(
            counts(&totals),
            SearchCounts {
                all: Some(249),
                movies: Some(233),
                shows: Some(1),
                episodes: Some(4),
                music: Some(8),
            }
        );
        let movies_only = HashMap::from([(SearchKind::Movies, 2)]);
        assert_eq!(
            counts(&movies_only),
            SearchCounts {
                movies: Some(2),
                ..SearchCounts::default()
            }
        );
    }

    /// What the user prefers and which servers are offline, as a test
    /// says.
    #[derive(Default)]
    struct Prefers {
        copy: Option<MediaId>,
        offline: Vec<&'static str>,
    }

    #[async_trait]
    impl Preferred for Prefers {
        async fn preferred(&self, _: &[String], copies: &[MediaId]) -> Option<MediaId> {
            self.copy.clone().filter(|c| copies.contains(c))
        }

        fn online(&self, id: &MediaId) -> bool {
            id.origin()
                .is_none_or(|origin| !self.offline.contains(&origin))
        }
    }

    const SA: &str = "aaaaaaaaaaaaaaaa";
    const SC: &str = "cccccccccccccccc";

    /// `item` as paired server `origin` shares it.
    fn on(origin: &str, item: Item) -> Item {
        let id = MediaId::federated(origin, item.id()).unwrap();
        match item {
            Item::Movie(m) => Item::Movie(Movie { id, ..m }),
            Item::Show(s) => Item::Show(Show { id, ..s }),
            Item::Episode(e) => Item::Episode(Episode { id, ..e }),
            other => other,
        }
    }

    fn titled(items: &[(&Item, &[&str])]) -> HashMap<MediaId, Vec<String>> {
        items
            .iter()
            .map(|(item, ids)| {
                let ids = ids.iter().map(|s| (*s).to_owned()).collect();
                (item.id().clone(), ids)
            })
            .collect()
    }

    async fn merge(
        items: &[&Item],
        titles: &HashMap<MediaId, Vec<String>>,
        prefers: &Prefers,
    ) -> (Vec<String>, HashMap<MediaId, Vec<MediaId>>) {
        let items = items.iter().map(|i| (*i).clone()).collect();
        let (shown, copies) = merged(items, titles, prefers).await;
        (keys(&shown), copies)
    }

    /// The same film here and on SA (one TMDB id) is one result, this
    /// server's, with SA's copy beside it (P8-05).
    #[tokio::test]
    async fn a_film_on_this_server_and_a_paired_one_is_one_result_from_here() {
        let here = movie("1", "Alien", 1979);
        let there = on(SA, movie("7", "Alien", 1979));
        let titles = titled(&[
            (&here, &["plex://movie/5d776", "tmdb://348"]),
            (&there, &["tmdb://348"]),
        ]);
        let (shown, copies) = merge(&[&there, &here], &titles, &Prefers::default()).await;
        assert_eq!(shown, ["plex:movie:1"]);
        assert_eq!(copies[here.id()], [there.id().clone()]);
    }

    /// Two films of one title and year with different ids are two films.
    #[tokio::test]
    async fn two_films_of_one_title_and_year_with_different_ids_stay_two() {
        let here = movie("1", "The Thing", 1982);
        let there = on(SA, movie("7", "The Thing", 1982));
        let titles = titled(&[(&here, &["tmdb://1091"]), (&there, &["tmdb://9999"])]);
        let (shown, copies) = merge(&[&here, &there], &titles, &Prefers::default()).await;
        assert_eq!(shown.len(), 2);
        assert!(copies.is_empty());
    }

    /// A paired server's id is not taken on its word: SA's film of another
    /// year that says `tmdb://348` is not merged with Alien here.
    #[tokio::test]
    async fn a_paired_copy_of_another_year_is_not_merged() {
        let here = movie("1", "Alien", 1979);
        let there = on(SA, movie("7", "Alien", 2004));
        let titles = titled(&[(&here, &["tmdb://348"]), (&there, &["tmdb://348"])]);
        let (shown, copies) = merge(&[&here, &there], &titles, &Prefers::default()).await;
        assert_eq!(shown.len(), 2);
        assert!(copies.is_empty());
    }

    /// Titles without ids, or with only ids that name nothing outside
    /// their server, are never merged, whatever their title.
    #[tokio::test]
    async fn titles_without_ids_are_never_merged() {
        let here = movie("1", "Home Video", 2004);
        let there = on(SA, movie("7", "Home Video", 2004));
        let titles = titled(&[(&here, &["local://1"]), (&there, &["local://1"])]);
        let (shown, _) = merge(&[&here, &there], &titles, &Prefers::default()).await;
        assert_eq!(shown.len(), 2);
        let (shown, _) = merge(&[&here, &there], &HashMap::new(), &Prefers::default()).await;
        assert_eq!(shown.len(), 2);
    }

    /// An episode is matched by its show's id, season and number, though
    /// its own ids differ.
    #[tokio::test]
    async fn an_episode_matches_by_its_show_s_id_season_and_number() {
        let here = episode("9", "Pilot", "5");
        let there = on(SA, episode("19", "Pilot", "15"));
        let other = on(SC, episode("29", "Pilot", "25"));
        let titles = titled(&[
            (&here, &["tvdb://111", "tmdb://1396#s1e1"]),
            (&there, &["tvdb://222", "tmdb://1396#s1e1"]),
            // The same show's next episode.
            (&other, &["tvdb://333", "tmdb://1396#s1e2"]),
        ]);
        let (shown, copies) = merge(&[&here, &there, &other], &titles, &Prefers::default()).await;
        assert_eq!(shown.len(), 2);
        assert_eq!(copies[here.id()], [there.id().clone()]);
    }

    /// Two copies on one server keep their own results: only servers'
    /// copies are one title.
    #[tokio::test]
    async fn two_copies_on_one_server_stay_two() {
        let first = movie("1", "Alien", 1979);
        let second = movie("2", "Alien", 1979);
        let titles = titled(&[(&first, &["tmdb://348"]), (&second, &["tmdb://348"])]);
        let (shown, _) = merge(&[&first, &second], &titles, &Prefers::default()).await;
        assert_eq!(shown, ["plex:movie:1", "plex:movie:2"]);
    }

    /// Without a copy here, the copy the user prefers is shown; without
    /// one, the first online.
    #[tokio::test]
    async fn without_a_copy_here_the_preferred_then_the_first_online_is_shown() {
        let sa = on(SA, movie("7", "Alien", 1979));
        let sc = on(SC, movie("8", "Alien", 1979));
        let titles = titled(&[(&sa, &["tmdb://348"]), (&sc, &["tmdb://348"])]);
        let prefers = Prefers {
            copy: Some(sc.id().clone()),
            ..Prefers::default()
        };
        let (shown, copies) = merge(&[&sa, &sc], &titles, &prefers).await;
        assert_eq!(shown, [sc.id().to_string()]);
        assert_eq!(copies[sc.id()], [sa.id().clone()]);
        let offline = Prefers {
            offline: vec![SA],
            ..Prefers::default()
        };
        let (shown, _) = merge(&[&sa, &sc], &titles, &offline).await;
        assert_eq!(shown, [sc.id().to_string()]);
    }

    /// Only ids of the agents every server shares match: TMDB, IMDb,
    /// TVDB, MusicBrainz and Plex's own.
    #[test]
    fn only_shared_agents_ids_are_matched() {
        let title: Vec<String> = [
            "tmdb://1",
            "imdb://tt2",
            "tvdb://3",
            "mbid://4",
            "plex://movie/5",
            "local://6",
            "test://7",
            "com.plexapp.agents.none://8",
        ]
        .map(str::to_owned)
        .to_vec();
        assert_eq!(matched(&title), title[..5]);
    }

    #[test]
    fn without_the_year_offers_other_years_best_first() {
        let this_year = Found::of(vec![found(
            SearchKind::Movies,
            vec![movie("1", "Dune: Part One", 2021)],
        )]);
        let any_year = Found::of(vec![
            found(
                SearchKind::Movies,
                vec![
                    movie("1", "Dune: Part One", 2021),
                    movie("2", "Dune: Part Two", 2024),
                    movie("3", "Dune", 1984),
                    movie("4", "Jodorowsky's Dune", 2013),
                    movie("5", "Dune Drifter", 2020),
                ],
            ),
            found(SearchKind::Shows, vec![show("6", "Dune: Prophecy")]),
        ]);
        let also = without_year("dune", 2021, &this_year, any_year);
        assert_eq!(keys(&also), ["plex:movie:3", "plex:show:6", "plex:movie:2"]);
    }
}
