//! A search as a catalog answers it (P3-09): the kinds asked for, each with
//! what the catalog found and how many it has. Choosing the top match,
//! leaving quiet episodes out and counting per scope happen above the
//! catalog, once the results of every catalog are in.

use std::collections::HashMap;

use spoolframe_types::{Item, LibraryId, MediaId, Person, SearchKind};

/// What to search for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    /// The text titles are matched against, without a year taken from it.
    pub text: String,
    /// Only items from this year.
    pub year: Option<u16>,
    /// The groups to fill. The catalog asks its backend for these and no
    /// others, rather than filtering afterwards.
    pub kinds: Vec<SearchKind>,
    /// Only this library's items; people are not found inside a library.
    pub library: Option<LibraryId>,
    /// The most items (or people) per group. 0 asks only how many there
    /// are.
    pub limit: u32,
}

/// What a search found: one group per kind asked for, in that order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found {
    pub groups: Vec<FoundGroup>,
    /// The title each item found is a copy of, by its id, where the
    /// catalog knows it (P8-05): its external ids, and for an episode its
    /// show's with its season and number ([`episode_ids`]). What a title
    /// found on several servers is merged by. Server side only, never on
    /// `Item`; an item missing here is matched to nothing.
    pub titles: HashMap<MediaId, Vec<String>>,
}

impl Found {
    /// `groups`, with no title known.
    pub fn of(groups: Vec<FoundGroup>) -> Self {
        Self {
            groups,
            titles: HashMap::new(),
        }
    }
}

/// An episode's ids by its show's `show_ids`, its `season` and its number
/// (`tmdb://1396#s1e2`): two servers' copies of an episode share their
/// show's ids more often than their own (P8-03, P8-05).
pub fn episode_ids(show_ids: &[String], season: u32, episode: u32) -> Vec<String> {
    show_ids
        .iter()
        .map(|id| format!("{id}#s{season}e{episode}"))
        .collect()
}

/// One kind's results, best first as the backend orders them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundGroup {
    pub kind: SearchKind,
    /// Empty for [`SearchKind::People`].
    pub items: Vec<Item>,
    /// Only for [`SearchKind::People`].
    pub people: Vec<Person>,
    /// How many the backend has, at least as many as are here. For people
    /// it is only as many as were returned.
    pub total: u32,
}

impl FoundGroup {
    pub fn empty(kind: SearchKind) -> Self {
        Self {
            kind,
            items: Vec::new(),
            people: Vec::new(),
            total: 0,
        }
    }

    /// `kind`'s groups from several sources (sections, servers) as one:
    /// taken from each in turn, best first, and their totals summed.
    pub fn merge(kind: SearchKind, groups: Vec<FoundGroup>, limit: u32) -> Self {
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        let total = groups.iter().map(|g| g.total).sum();
        let (items, people): (Vec<_>, Vec<_>) =
            groups.into_iter().map(|g| (g.items, g.people)).unzip();
        Self {
            kind,
            items: interleave(items, limit),
            people: interleave(people, limit),
            total,
        }
    }
}

/// Round robin over each list, best first within each, so that every
/// list's best make the cut; at most `limit`.
pub fn interleave<T>(lists: Vec<Vec<T>>, limit: usize) -> Vec<T> {
    let mut lists: Vec<_> = lists.into_iter().map(Vec::into_iter).collect();
    let mut out = Vec::new();
    while out.len() < limit {
        let before = out.len();
        for list in &mut lists {
            if out.len() == limit {
                break;
            }
            out.extend(list.next());
        }
        if out.len() == before {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleaving_takes_from_each_list_in_turn() {
        assert_eq!(interleave(vec![vec![1, 2, 3], vec![10]], 3), [1, 10, 2]);
        assert_eq!(interleave(vec![vec![1], vec![10, 20]], 9), [1, 10, 20]);
    }

    /// An episode is matched by each of its show's ids with its season and
    /// number.
    #[test]
    fn an_episode_is_matched_by_its_show_s_ids_season_and_number() {
        assert_eq!(
            episode_ids(&["tmdb://1396".into(), "tvdb://81189".into()], 1, 2),
            ["tmdb://1396#s1e2", "tvdb://81189#s1e2"]
        );
    }

    #[test]
    fn merged_groups_sum_their_totals() {
        let group = |total| FoundGroup {
            total,
            ..FoundGroup::empty(SearchKind::Movies)
        };
        let merged = FoundGroup::merge(SearchKind::Movies, vec![group(233), group(4)], 5);
        assert_eq!(merged.total, 237);
    }
}
