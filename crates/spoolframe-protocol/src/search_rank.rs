//! How a search's results are ranked and grouped once what matched is
//! known (P3-09): the year taken off the query, the top match, quiet
//! episodes left out of `All`, each group cut to the limit, and the scopes
//! counted.
//!
//! One implementation for the two places that answer a search: the
//! server, over what its catalogs found, and the SDK with no server, over
//! what is on the device (`Client::search_offline`). Each finds the
//! matches its own way; the same rules then make the same
//! [`SearchResults`] of them, so a search screen works unchanged on
//! either.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use spoolframe_types::{Item, MediaId, Person, SearchKind, SearchScope};

use crate::search::{SearchCounts, SearchGroup, SearchRequest, SearchResults, TopResult};

/// How many episodes `All` shows: episodes are quiet there, and come last.
pub const QUIET_EPISODES: usize = 3;

/// How many items "also matching without the year" offers.
pub const WITHOUT_YEAR: usize = 3;

/// Every kind of item a search finds: the kinds counted in a scope.
pub const MEDIA: [SearchKind; 6] = [
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

/// What matched of one kind, best first as the finder has them: at most
/// one more than the request's limit, and how many there are in all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matched {
    pub kind: SearchKind,
    /// Empty for [`SearchKind::People`].
    pub items: Vec<Item>,
    /// Only for [`SearchKind::People`].
    pub people: Vec<Person>,
    /// How many there are, at least as many as are here. For people it
    /// is only as many as were returned.
    pub total: u32,
}

/// The rules, with how a title is compared to the query: the server
/// compares as Plex sorts ([`normalized`]); the SDK with no server folds
/// accents and punctuation as well, as it matches.
#[derive(Debug, Clone, Copy)]
pub struct Ranker {
    normalize: fn(&str) -> String,
}

impl Default for Ranker {
    /// Titles compared [`normalized`]: the server's.
    fn default() -> Self {
        Self::new(normalized)
    }
}

impl Ranker {
    /// Titles and the query compared after `normalize`, which drops a
    /// leading article as [`normalized`] does.
    pub fn new(normalize: fn(&str) -> String) -> Self {
        Self { normalize }
    }

    /// What a client is shown of `found`, the matches for `query` (its
    /// text and the year taken off it). `totals` has each kind's count,
    /// the other scopes' too when they were counted. In `All`, an episode
    /// whose show is listed in the shows group (or any of that show's
    /// other copies, `copies`) is quiet: counted in `hidden`, not listed.
    /// One whose show is the top match is listed. A group with no item is
    /// left out, but for a "See all".
    pub fn group(
        &self,
        request: &SearchRequest,
        query: &Parsed,
        mut found: Vec<Matched>,
        totals: &HashMap<SearchKind, u32>,
        without_year: Vec<Item>,
        copies: &HashMap<MediaId, Vec<MediaId>>,
    ) -> SearchResults {
        let year = query.year;
        let limit = usize::try_from(request.limit).unwrap_or(usize::MAX);
        let see_all = request.kind.is_some();
        let quiet = !see_all && request.scope == SearchScope::All;
        let text = (self.normalize)(&query.text);
        found.sort_by_key(|g| g.kind);

        let top = match see_all {
            true => None,
            false => self.top(&found, &text),
        };
        let top_item = match &top {
            Some(TopResult::Item(item)) => Some(item.id()),
            _ => None,
        };
        let top_person = match &top {
            Some(TopResult::Person(person)) => Some(&person.id),
            _ => None,
        };
        // The shows the shows group lists, with their other copies: what a
        // show's own episodes are under in `All`. The top match is in no
        // group, so its episodes are listed.
        let shows: HashSet<MediaId> = found
            .iter()
            .filter(|g| g.kind == SearchKind::Shows)
            .flat_map(|g| self.listed(&g.items, top_item, &text, limit))
            .flat_map(|id| {
                let more = copies.get(id).cloned().unwrap_or_default();
                std::iter::once(id.clone()).chain(more)
            })
            .collect();

        let mut groups = Vec::new();
        for Matched {
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
            // Stable: within a tier, the finder's order.
            items.sort_by_cached_key(|i| self.tier(i, &text));
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
            // A group is shown only with something in it; "See all" is
            // its group, empty or not.
            if shown == 0 && !see_all {
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

    /// The ids a group of `items` lists, as [`Self::group`] makes it with
    /// nothing hidden: the top match left out, best first (stable within a
    /// tier), at most `cap`.
    fn listed<'a>(
        &self,
        items: &'a [Item],
        top: Option<&MediaId>,
        text: &str,
        cap: usize,
    ) -> Vec<&'a MediaId> {
        let mut kept: Vec<&Item> = items.iter().filter(|i| Some(i.id()) != top).collect();
        kept.sort_by_cached_key(|i| self.tier(i, text));
        kept.into_iter().take(cap).map(Item::id).collect()
    }

    /// Up to [`WITHOUT_YEAR`] of `any_year` that are not of `year` and not
    /// already `found`, best first.
    pub fn without_year(
        &self,
        text: &str,
        year: u16,
        found: &[Matched],
        mut any_year: Vec<Matched>,
    ) -> Vec<Item> {
        let matched: HashSet<&MediaId> =
            found.iter().flat_map(|g| &g.items).map(Item::id).collect();
        any_year.sort_by_key(|g| g.kind);
        let others = any_year
            .iter()
            .flat_map(|g| &g.items)
            .filter(|i| year_of(i) != Some(year) && !matched.contains(i.id()));
        let mut ranked: Vec<_> = others.enumerate().collect();
        let text = (self.normalize)(text);
        ranked.sort_by_cached_key(|&(at, item)| (self.tier(item, &text), at));
        ranked
            .into_iter()
            .take(WITHOUT_YEAR)
            .map(|(_, item)| item.clone())
            .collect()
    }

    /// The top match of `found`: the best item, unless a person's name
    /// matches better than its title does (or no item is found). On a tie
    /// the item wins: a person is shown in the top spot only when the
    /// query is more surely them, as an exact "keanu reeves" over titles
    /// that only start with it.
    fn top(&self, found: &[Matched], text: &str) -> Option<TopResult> {
        let item = self.best(found.iter().flat_map(|g| &g.items), text);
        let person = found
            .iter()
            .flat_map(|g| &g.people)
            .enumerate()
            .min_by_key(|&(at, person)| (self.tier_of(&person.name, text), at))
            .map(|(_, person)| person);
        match (item, person) {
            (Some(item), Some(person))
                if self.tier_of(&person.name, text) < self.tier(item, text) =>
            {
                Some(TopResult::Person(person.clone()))
            }
            (Some(item), _) => Some(TopResult::Item(item.clone())),
            (None, Some(person)) => Some(TopResult::Person(person.clone())),
            (None, None) => None,
        }
    }

    /// The best match among `items`: by [`Self::tier`], then the order
    /// they come in. They come in the groups' order, so on a tie a show
    /// beats a movie, and its own episodes, which are last.
    fn best<'a>(&self, items: impl Iterator<Item = &'a Item>, text: &str) -> Option<&'a Item> {
        items
            .enumerate()
            .min_by_key(|&(at, item)| (self.tier(item, text), at))
            .map(|(_, item)| item)
    }

    /// [`Self::tier_of`] the item's title.
    fn tier(&self, item: &Item, text: &str) -> u8 {
        self.tier_of(item.title(), text)
    }

    /// 0 for a title that is the query, 1 for one that starts with it, 2
    /// for the rest. `text` is normalized already.
    pub fn tier_of(&self, title: &str, text: &str) -> u8 {
        let title = (self.normalize)(title);
        if title == text {
            0
        } else if title.starts_with(text) {
            1
        } else {
            2
        }
    }
}

/// Each scope's count, from each kind's total. A scope is counted only
/// when every kind in it is.
pub fn counts(totals: &HashMap<SearchKind, u32>) -> SearchCounts {
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

/// `s` as the server compares titles: lower case, single spaces, and
/// without a leading English article, as Plex sorts them, so "cure" is
/// the exact title of "The Cure".
pub fn normalized(s: &str) -> String {
    let s = s
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    strip_article(&s).to_owned()
}

/// `s`, lower case with single spaces, without a leading "the ", "a " or
/// "an ".
pub fn strip_article(s: &str) -> &str {
    ["the ", "a ", "an "]
        .iter()
        .find_map(|article| s.strip_prefix(article))
        .unwrap_or(s)
}

/// The year a film, show or album came out; `None` for the others.
pub fn year_of(item: &Item) -> Option<u16> {
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

    /// A ranker folds as its finder matches: with accents folded, "amelie"
    /// is the exact title of "Amélie"; the server's leaves the accent.
    #[test]
    fn a_ranker_compares_titles_as_its_normalize_says() {
        fn ascii(s: &str) -> String {
            normalized(&s.replace('é', "e"))
        }
        let amelie = "Amélie";
        assert_eq!(Ranker::default().tier_of(amelie, "amelie"), 2);
        assert_eq!(Ranker::new(ascii).tier_of(amelie, "amelie"), 0);
    }

    #[test]
    fn only_a_leading_article_is_dropped() {
        assert_eq!(normalized("The  Cure"), "cure");
        assert_eq!(normalized("Theory"), "theory");
        assert_eq!(normalized("Cure for the Itch"), "cure for the itch");
    }
}
