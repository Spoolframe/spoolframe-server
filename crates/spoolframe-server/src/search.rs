//! Scoped, grouped search (P3-09): a client's search as the catalog is
//! asked it, and what the catalog found as the client is shown it.
//!
//! The catalog applies the scope and the year; this picks the top match,
//! leaves quiet episodes out of `All`, cuts each group to the limit and
//! counts the scopes, once every Plex server's results are merged.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use spoolframe_catalog::{Catalog, CatalogError, Found, FoundGroup, SearchQuery};
use spoolframe_protocol::{SearchCounts, SearchGroup, SearchRequest, SearchResults, TopResult};
use spoolframe_types::{Item, MediaId, Person, SearchKind, SearchScope};

/// How many episodes `All` shows: episodes are quiet there, and come last.
pub const QUIET_EPISODES: usize = 3;

/// How many items "also matching without the year" offers.
pub const WITHOUT_YEAR: usize = 3;

/// Every kind of item a search finds: the kinds counted in a scope.
const MEDIA: [SearchKind; 6] = [
    SearchKind::Shows,
    SearchKind::Movies,
    SearchKind::Artists,
    SearchKind::Albums,
    SearchKind::Tracks,
    SearchKind::Episodes,
];

const MUSIC: [SearchKind; 3] = [SearchKind::Artists, SearchKind::Albums, SearchKind::Tracks];

/// A query as typed, split into the text to search for and a year.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub text: String,
    pub year: Option<u16>,
}

/// `query`, with a year at its end taken off as a filter: "dune 2021".
/// Only a year from 1900 to next year counts, and only after some text,
/// so "Blade Runner 2049" and "1917" are searched as they are.
pub fn parse(query: &str, this_year: u16) -> Parsed {
    let query = query.trim();
    if let Some((text, last)) = query.rsplit_once(char::is_whitespace) {
        let text = text.trim_end();
        let year = (last.len() == 4 && last.bytes().all(|b| b.is_ascii_digit()))
            .then(|| last.parse::<u16>().ok())
            .flatten()
            .filter(|y| (1900..=this_year.saturating_add(1)).contains(y));
        if let Some(year) = year.filter(|_| !text.is_empty()) {
            return Parsed {
                text: text.to_owned(),
                year: Some(year),
            };
        }
    }
    Parsed {
        text: query.to_owned(),
        year: None,
    }
}

/// The year now, near enough for [`parse`]: an average Gregorian year
/// from the epoch is off by at most a day at a year's turn, which next
/// year's being allowed covers.
pub fn this_year() -> u16 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    u16::try_from(1970 + secs / 31_556_952).unwrap_or(u16::MAX)
}

/// Answer `request` from `catalog`. Counting the other scopes and finding
/// items in other years are extras: when they fail, the results go
/// without them.
pub async fn search(
    catalog: &dyn Catalog,
    request: &SearchRequest,
    this_year: u16,
) -> Result<SearchResults, CatalogError> {
    let Parsed { text, year } = parse(&request.query, this_year);
    if text.is_empty() {
        return Ok(SearchResults::default());
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
    let found = catalog.search(&query).await?;

    let mut totals: HashMap<SearchKind, u32> =
        found.groups.iter().map(|g| (g.kind, g.total)).collect();
    if request.kind.is_none() && totals.values().all(|&t| t == 0) {
        // Nothing in this scope: the empty state offers the others.
        let others: Vec<_> = MEDIA
            .into_iter()
            .filter(|k| !query.kinds.contains(k))
            .collect();
        if !others.is_empty() {
            let counting = SearchQuery {
                kinds: others,
                limit: 0,
                ..query.clone()
            };
            match catalog.search(&counting).await {
                Ok(counted) => totals.extend(counted.groups.iter().map(|g| (g.kind, g.total))),
                Err(e) => tracing::warn!(error = %e, "search: the other scopes are not counted"),
            }
        }
    }

    let mut also = Vec::new();
    if let Some(year) = year {
        let kinds: Vec<_> = query
            .kinds
            .iter()
            .copied()
            .filter(|&k| k != SearchKind::People)
            .collect();
        if !kinds.is_empty() {
            let any_year = SearchQuery {
                year: None,
                kinds,
                // Some of them are this year's, already found.
                limit: 2 * WITHOUT_YEAR as u32,
                ..query.clone()
            };
            match catalog.search(&any_year).await {
                Ok(any_year) => also = without_year(&query.text, year, &found, any_year),
                Err(e) => tracing::warn!(error = %e, "search: nothing found without the year"),
            }
        }
    }

    Ok(group(request, &query.text, found, &totals, year, also))
}

/// What a client is shown of `found`.
fn group(
    request: &SearchRequest,
    text: &str,
    found: Found,
    totals: &HashMap<SearchKind, u32>,
    year: Option<u16>,
    without_year: Vec<Item>,
) -> SearchResults {
    let limit = usize::try_from(request.limit).unwrap_or(usize::MAX);
    let see_all = request.kind.is_some();
    let quiet = !see_all && request.scope == SearchScope::All;
    let text = normalized(text);
    let mut found = found.groups;
    found.sort_by_key(|g| g.kind);

    let top = match see_all {
        true => None,
        false => top(&found, &text),
    };
    let top_item = match &top {
        Some(TopResult::Item(item)) => Some(item.id()),
        _ => None,
    };
    let top_person = match &top {
        Some(TopResult::Person(person)) => Some(&person.id),
        _ => None,
    };
    let shows: HashSet<MediaId> = found
        .iter()
        .filter(|g| g.kind == SearchKind::Shows)
        .flat_map(|g| g.items.iter().map(|i| i.id().clone()))
        .collect();

    let mut groups = Vec::new();
    for FoundGroup {
        kind,
        mut items,
        mut people,
        total,
    } in found
    {
        let before = items.len() + people.len();
        items.retain(|i| Some(i.id()) != top_item);
        people.retain(|p| Some(&p.id) != top_person);
        let removed = before - items.len() - people.len();
        let mut hidden = 0;
        if quiet && kind == SearchKind::Episodes {
            let before = items.len();
            items.retain(|i| !matches!(i, Item::Episode(e) if shows.contains(&e.show_id)));
            hidden = before - items.len();
        }
        // Stable: within a tier, the catalog's order.
        items.sort_by_key(|i| tier(i, &text));
        let cap = match quiet && kind == SearchKind::Episodes {
            true => limit.min(QUIET_EPISODES),
            false => limit,
        };
        items.truncate(cap);
        people.truncate(cap);
        let shown = items.len() + people.len();
        let available = usize::try_from(total)
            .unwrap_or(usize::MAX)
            .saturating_sub(removed + hidden);
        if shown == 0 && hidden == 0 && !see_all {
            continue;
        }
        let more = available > shown;
        let total = match kind {
            SearchKind::People if more => None,
            _ => Some(total),
        };
        groups.push(SearchGroup {
            kind,
            items,
            people,
            more,
            total,
            hidden: u32::try_from(hidden).unwrap_or(u32::MAX),
        });
    }

    SearchResults {
        top,
        groups,
        counts: counts(totals),
        year,
        without_year,
    }
}

/// Each scope's count, from each kind's total. A scope is counted only
/// when every kind in it is.
fn counts(totals: &HashMap<SearchKind, u32>) -> SearchCounts {
    let of = |kind| totals.get(&kind).copied();
    let sum = |kinds: &[SearchKind]| kinds.iter().map(|&k| of(k)).sum::<Option<u32>>();
    SearchCounts {
        all: sum(&MEDIA).map(|media| media.saturating_add(of(SearchKind::People).unwrap_or(0))),
        movies: of(SearchKind::Movies),
        shows: of(SearchKind::Shows),
        episodes: of(SearchKind::Episodes),
        music: sum(&MUSIC),
    }
}

/// Up to [`WITHOUT_YEAR`] of `any_year` that are not of `year` and not
/// already found, best first.
fn without_year(text: &str, year: u16, found: &Found, any_year: Found) -> Vec<Item> {
    let matched: HashSet<&MediaId> = found
        .groups
        .iter()
        .flat_map(|g| &g.items)
        .map(Item::id)
        .collect();
    let mut groups = any_year.groups;
    groups.sort_by_key(|g| g.kind);
    let others = groups
        .iter()
        .flat_map(|g| &g.items)
        .filter(|i| year_of(i) != Some(year) && !matched.contains(i.id()));
    let mut ranked: Vec<_> = others.enumerate().collect();
    let text = normalized(text);
    ranked.sort_by_key(|&(at, item)| (tier(item, &text), at));
    ranked
        .into_iter()
        .take(WITHOUT_YEAR)
        .map(|(_, item)| item.clone())
        .collect()
}

/// The top match of `found`: the best item, unless a person's name
/// matches better than its title does (or no item is found). On a tie the
/// item wins: a person is shown in the top spot only when the query is
/// more surely them, as an exact "keanu reeves" over titles that only
/// start with it.
fn top(found: &[FoundGroup], text: &str) -> Option<TopResult> {
    let item = best(found.iter().flat_map(|g| &g.items), text);
    let person = found
        .iter()
        .flat_map(|g| &g.people)
        .enumerate()
        .min_by_key(|&(at, person)| (name_tier(person, text), at))
        .map(|(_, person)| person);
    match (item, person) {
        (Some(item), Some(person)) if name_tier(person, text) < tier(item, text) => {
            Some(TopResult::Person(person.clone()))
        }
        (Some(item), _) => Some(TopResult::Item(item.clone())),
        (None, Some(person)) => Some(TopResult::Person(person.clone())),
        (None, None) => None,
    }
}

/// The best match among `items`: by [`tier`], then the order they come
/// in. They come in the groups' order, so on a tie a show beats a movie,
/// and its own episodes, which are last.
fn best<'a>(items: impl Iterator<Item = &'a Item>, text: &str) -> Option<&'a Item> {
    items
        .enumerate()
        .min_by_key(|&(at, item)| (tier(item, text), at))
        .map(|(_, item)| item)
}

/// [`tier`] of a person's name.
fn name_tier(person: &Person, text: &str) -> u8 {
    tier_of(&person.name, text)
}

/// [`tier_of`] the item's title.
fn tier(item: &Item, text: &str) -> u8 {
    tier_of(title(item), text)
}

/// 0 for a title that is the query, 1 for one that starts with it, 2 for
/// the rest. `text` is [`normalized`].
fn tier_of(title: &str, text: &str) -> u8 {
    let title = normalized(title);
    if title == text {
        0
    } else if title.starts_with(text) {
        1
    } else {
        2
    }
}

/// `s` as titles are compared: lower case, single spaces, and without a
/// leading English article, as Plex sorts them, so "cure" is the exact
/// title of "The Cure".
fn normalized(s: &str) -> String {
    let s = s
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    ["the ", "a ", "an "]
        .iter()
        .find_map(|article| s.strip_prefix(article))
        .map(str::to_owned)
        .unwrap_or(s)
}

fn title(item: &Item) -> &str {
    match item {
        Item::Movie(v) => &v.title,
        Item::Show(v) => &v.title,
        Item::Season(v) => &v.title,
        Item::Episode(v) => &v.title,
        Item::Artist(v) => &v.name,
        Item::Album(v) => &v.title,
        Item::Track(v) => &v.title,
    }
}

fn year_of(item: &Item) -> Option<u16> {
    match item {
        Item::Movie(v) => v.year,
        Item::Show(v) => v.year,
        Item::Album(v) => v.year,
        _ => None,
    }
}

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
            id: id("movie", key),
            title: title.into(),
            year: Some(year),
            summary: None,
            content_rating: None,
            duration: None,
            artwork: Vec::new(),
            playback: PlaybackState::default(),
        })
    }

    fn show(key: &str, title: &str) -> Item {
        Item::Show(Show {
            id: id("show", key),
            title: title.into(),
            year: None,
            summary: None,
            content_rating: None,
            season_count: None,
            genres: Vec::new(),
            artwork: Vec::new(),
        })
    }

    fn episode(key: &str, title: &str, show: &str) -> Item {
        Item::Episode(Episode {
            id: id("episode", key),
            show_id: id("show", show),
            show_title: None,
            season_id: id("season", "1"),
            season: 1,
            episode: 1,
            title: title.into(),
            summary: None,
            content_rating: None,
            duration: None,
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
        let found = Found { groups };
        let totals = found.groups.iter().map(|g| (g.kind, g.total)).collect();
        group(request, text, found, &totals, None, Vec::new())
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

    #[test]
    fn in_all_a_show_s_own_episodes_are_left_out_and_counted() {
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
        let episodes = of(&results, SearchKind::Episodes).unwrap();
        assert_eq!(keys(&episodes.items), ["plex:episode:8"]);
        assert_eq!(episodes.hidden, 2);
        assert!(!episodes.more);
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

    #[test]
    fn without_the_year_offers_other_years_best_first() {
        let this_year = Found {
            groups: vec![found(
                SearchKind::Movies,
                vec![movie("1", "Dune: Part One", 2021)],
            )],
        };
        let any_year = Found {
            groups: vec![
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
            ],
        };
        let also = without_year("dune", 2021, &this_year, any_year);
        assert_eq!(keys(&also), ["plex:movie:3", "plex:show:6", "plex:movie:2"]);
    }
}
