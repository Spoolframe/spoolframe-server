//! What a client shows of a search and of a title's copies, worked out
//! once: the "Play from" choices, one server's results, "See all 23" and
//! "3 from Severance are under the show".

use spoolframe_types::{Item, LibraryServer, MediaId, SearchKind};

use crate::search::{Copies, SearchCounts, SearchGroup, SearchResult, SearchResults, TopResult};

/// One copy of a title, as the Play from list shows it
/// ([`Copies::choices`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayFromChoice {
    /// The copy's paired server; `None` for the user's own.
    pub server: Option<LibraryServer>,
    /// The copy's id: what plays when it is picked.
    pub id: MediaId,
    /// The copy that plays now.
    pub playing: bool,
    /// It can play now: the user's own server's, or a paired one online.
    /// One offline is shown greyed, browsed and not played.
    pub playable: bool,
}

impl Copies {
    /// Every copy of `item`'s title (these are its copies): the user's
    /// own server's first, then in the protocol's order. None for a
    /// title on one server, which has nothing to choose.
    pub fn choices(&self, item: &MediaId) -> Vec<PlayFromChoice> {
        if self.also_on.is_empty() {
            return Vec::new();
        }
        let copies = std::iter::once((self.origin.as_ref(), item))
            .chain(self.also_on.iter().map(|c| (c.server.as_ref(), &c.id)));
        let mut choices: Vec<PlayFromChoice> = copies
            .map(|(server, id)| PlayFromChoice {
                server: server.cloned(),
                id: id.clone(),
                playing: *id == self.play_from,
                playable: server.is_none_or(|s| s.online),
            })
            .collect();
        // Stable: the paired servers stay in the protocol's order.
        choices.sort_by_key(|c| c.server.is_some());
        choices
    }
}

/// Episodes left out of a search because their show is in it
/// ([`SearchResults::hidden_under`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HiddenUnder {
    pub count: u32,
    /// The titles of the shows found, which hold them; empty when the
    /// shows were not returned.
    pub shows: Vec<String>,
}

impl<T> SearchResults<T> {
    fn group(&self, kind: SearchKind) -> Option<&SearchGroup<T>> {
        self.groups.iter().find(|g| g.kind == kind)
    }

    /// The number "See all" names for `kind`'s group: its own total, else
    /// its scope's count. `None` when the group has no more to show, or
    /// nothing says how many: then a plain "See all".
    pub fn see_all_count(&self, kind: SearchKind) -> Option<u32> {
        let group = self.group(kind).filter(|g| g.more)?;
        group
            .total
            .or_else(|| kind.scope().and_then(|scope| self.counts.of(scope)))
    }
}

impl SearchResults<SearchResult> {
    /// The episodes `kind`'s group left under their show, with the shows'
    /// titles (the top match's too); `None` when none were.
    pub fn hidden_under(&self, kind: SearchKind) -> Option<HiddenUnder> {
        let count = self.group(kind).map(|g| g.hidden).filter(|n| *n > 0)?;
        let mut shows: Vec<String> = self
            .group(SearchKind::Shows)
            .map(|g| g.items.iter().map(|r| r.item.title().to_owned()).collect())
            .unwrap_or_default();
        if let Some(TopResult::Item(top)) = &self.top
            && let Item::Show(show) = &top.item
            && !shows.contains(&show.title)
        {
            shows.push(show.title.clone());
        }
        Some(HiddenUnder { count, shows })
    }

    /// Only the hits from paired server `origin` (its server id), or the
    /// user's own with `None`, for a server's chip. People are the user's
    /// own server's, so only it keeps them. The counts are what is left:
    /// `all` counts the people kept too. A group's total is its items
    /// left when it had no more to show, else not known. Episodes hidden
    /// under their show stay hidden only while a show is left to hold
    /// them.
    pub fn only_from(self, origin: Option<&str>) -> Self {
        let keep = |hit: &SearchResult| hit.origin() == origin;
        let people = origin.is_none();
        let top = match self.top {
            Some(TopResult::Item(hit)) if keep(&hit) => Some(TopResult::Item(hit)),
            Some(TopResult::Person(person)) if people => Some(TopResult::Person(person)),
            _ => None,
        };
        let top_is_show =
            matches!(&top, Some(TopResult::Item(hit)) if matches!(hit.item, Item::Show(_)));
        let shows_left = top_is_show
            || self
                .groups
                .iter()
                .any(|g| g.kind == SearchKind::Shows && g.items.iter().any(keep));
        let groups: Vec<_> = self
            .groups
            .into_iter()
            .map(|g| {
                let items: Vec<SearchResult> = g.items.into_iter().filter(keep).collect();
                let people = if people { g.people } else { Vec::new() };
                let hidden = if shows_left { g.hidden } else { 0 };
                SearchGroup {
                    kind: g.kind,
                    total: None,
                    items,
                    people,
                    more: g.more,
                    hidden,
                }
            })
            .collect();
        // The top match counted once: a top person the SDK also lists in
        // People is counted there.
        let top_kind = match &top {
            Some(TopResult::Item(hit)) => kind_of(&hit.item),
            Some(TopResult::Person(_)) => Some(SearchKind::People),
            None => None,
        };
        let top_listed = matches!(&top, Some(TopResult::Person(person))
            if groups.iter().any(|g| g.kind == SearchKind::People
                && g.people.iter().any(|p| p.id == person.id)));
        let unlisted = top_kind.filter(|_| !top_listed);
        let top_unlisted = |kind: SearchKind| unlisted == Some(kind);
        let groups: Vec<_> = groups
            .into_iter()
            .map(|g| {
                let shown = (g.items.len() + g.people.len()) as u32;
                SearchGroup {
                    total: (!g.more).then_some(shown + u32::from(top_unlisted(g.kind)) + g.hidden),
                    ..g
                }
            })
            .collect();
        let count = |kinds: &[SearchKind]| -> u32 {
            let listed: usize = groups
                .iter()
                .filter(|g| kinds.contains(&g.kind))
                .map(|g| g.items.len() + g.people.len() + g.hidden as usize)
                .sum();
            listed as u32 + kinds.iter().filter(|k| top_unlisted(**k)).count() as u32
        };
        let movies = count(&[SearchKind::Movies]);
        let shows = count(&[SearchKind::Shows]);
        let episodes = count(&[SearchKind::Episodes]);
        let music = count(&[SearchKind::Artists, SearchKind::Albums, SearchKind::Tracks]);
        let persons = count(&[SearchKind::People]);
        SearchResults {
            counts: SearchCounts {
                all: Some(movies + shows + episodes + music + persons),
                movies: Some(movies),
                shows: Some(shows),
                episodes: Some(episodes),
                music: Some(music),
            },
            top,
            groups,
            year: self.year,
            without_year: self.without_year.into_iter().filter(keep).collect(),
        }
    }
}

/// The group an item is listed in.
fn kind_of(item: &Item) -> Option<SearchKind> {
    Some(match item {
        Item::Movie(_) => SearchKind::Movies,
        Item::Show(_) => SearchKind::Shows,
        Item::Episode(_) => SearchKind::Episodes,
        Item::Artist(_) => SearchKind::Artists,
        Item::Album(_) => SearchKind::Albums,
        Item::Track(_) => SearchKind::Tracks,
        Item::Season(_) => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::TitleCopy;
    use spoolframe_types::{MediaKind, Movie, Namespace, PlaybackState, Show};

    fn server(id: &str, online: bool) -> LibraryServer {
        LibraryServer {
            server_id: id.into(),
            name: id.to_uppercase(),
            online,
        }
    }

    fn own(key: &str) -> MediaId {
        MediaId::new(Namespace::Plex, MediaKind::Movie, key).unwrap()
    }

    fn fed(origin: &str, key: &str) -> MediaId {
        MediaId::federated(origin, &own(key)).unwrap()
    }

    fn copy(origin: &str, online: bool) -> TitleCopy {
        TitleCopy {
            id: fed(origin, "9"),
            server: Some(server(origin, online)),
        }
    }

    fn heat_copies(play_from: MediaId) -> Copies {
        Copies {
            origin: None,
            also_on: vec![copy("bob", true)],
            play_from,
        }
    }

    #[test]
    fn this_servers_copy_is_listed_first_and_the_one_that_plays_is_picked() {
        let listed: Vec<_> = heat_copies(fed("bob", "9"))
            .choices(&own("3"))
            .iter()
            .map(|c| (c.server.as_ref().map(|s| s.server_id.clone()), c.playing))
            .collect();
        assert_eq!(listed, [(None, false), (Some("bob".into()), true)]);
    }

    #[test]
    fn this_servers_copy_comes_first_from_a_paired_items_page_too() {
        let copies = Copies {
            origin: Some(server("bob", true)),
            also_on: vec![
                copy("sb", true),
                TitleCopy {
                    id: own("3"),
                    server: None,
                },
            ],
            play_from: fed("bob", "9"),
        };
        let ids: Vec<_> = copies
            .choices(&fed("bob", "9"))
            .into_iter()
            .map(|c| c.id)
            .collect();
        // Home first, then the protocol's order: the item's own, then sb's.
        assert_eq!(ids, [own("3"), fed("bob", "9"), fed("sb", "9")]);
    }

    #[test]
    fn a_title_on_one_server_has_no_play_from_choice() {
        let copies = Copies {
            origin: Some(server("bob", true)),
            also_on: Vec::new(),
            play_from: fed("bob", "11"),
        };
        assert!(copies.choices(&fed("bob", "11")).is_empty());
    }

    #[test]
    fn a_copy_on_an_offline_server_cannot_be_played() {
        let copies = Copies {
            origin: None,
            also_on: vec![copy("bob", false)],
            play_from: own("3"),
        };
        let playable: Vec<_> = copies
            .choices(&own("3"))
            .iter()
            .map(|c| c.playable)
            .collect();
        assert_eq!(playable, [true, false]);
    }

    fn movie(id: MediaId, title: &str) -> Item {
        Item::Movie(Movie {
            id,
            title: title.into(),
            year: None,
            release_date: None,
            summary: None,
            content_rating: None,
            duration: None,
            cast: Vec::new(),
            artwork: Vec::new(),
            playback: PlaybackState::default(),
        })
    }

    fn show(title: &str) -> Item {
        Item::Show(Show {
            id: MediaId::new(Namespace::Plex, MediaKind::Show, title).unwrap(),
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

    fn hit(item: Item, origin: Option<&str>) -> SearchResult {
        crate::SearchHit {
            play_from: item.id().clone(),
            origin: origin.map(|o| server(o, true)),
            also_on: Vec::new(),
            item,
        }
        .into()
    }

    fn group<T>(kind: SearchKind, items: Vec<T>) -> SearchGroup<T> {
        SearchGroup {
            kind,
            items,
            people: Vec::new(),
            more: false,
            total: None,
            hidden: 0,
        }
    }

    #[test]
    fn one_servers_hits_are_counted_as_shown() {
        let results = SearchResults {
            top: Some(TopResult::Item(hit(
                movie(fed("sa", "1"), "Alien"),
                Some("sa"),
            ))),
            groups: vec![group(
                SearchKind::Movies,
                vec![
                    hit(movie(own("2"), "Aliens"), None),
                    hit(movie(fed("sa", "3"), "Alien 3"), Some("sa")),
                ],
            )],
            counts: SearchCounts {
                all: Some(40),
                movies: Some(40),
                ..SearchCounts::default()
            },
            ..SearchResults::default()
        };
        let sa = results.clone().only_from(Some("sa"));
        assert!(sa.top.is_some());
        assert_eq!(sa.groups[0].items.len(), 1);
        assert_eq!((sa.counts.movies, sa.counts.all), (Some(2), Some(2)));
        // The group had no more: what is left is all of it, top included.
        assert_eq!(sa.groups[0].total, Some(2));
        let home = results.clone().only_from(None);
        assert!(home.top.is_none());
        assert_eq!(home.counts.movies, Some(1));
    }

    fn person() -> spoolframe_types::Person {
        spoolframe_types::Person {
            id: spoolframe_types::PersonId::new(Namespace::Plex, "actor-1").unwrap(),
            name: "Sigourney Weaver".into(),
            role: spoolframe_types::PersonRole::Actor,
            count: Some(3),
        }
    }

    #[test]
    fn people_count_for_the_users_own_server_only() {
        let mut people = group(SearchKind::People, Vec::<SearchResult>::new());
        people.people = vec![person()];
        let results = SearchResults {
            groups: vec![people],
            ..SearchResults::default()
        };
        assert_eq!(results.clone().only_from(None).counts.all, Some(1));
        let sa = results.clone().only_from(Some("sa"));
        assert_eq!((sa.counts.all, sa.groups[0].people.len()), (Some(0), 0));
    }

    /// A top person also listed in People (as the SDK lists them) is
    /// counted once.
    #[test]
    fn a_top_person_listed_in_people_is_counted_once() {
        let mut people = group(SearchKind::People, Vec::<SearchResult>::new());
        people.people = vec![person()];
        let results = SearchResults {
            top: Some(TopResult::Person(person())),
            groups: vec![people],
            ..SearchResults::default()
        };
        let own = results.clone().only_from(None);
        assert_eq!((own.counts.all, own.groups[0].total), (Some(1), Some(1)));
    }

    /// The same person by id, though their item count differs between
    /// the top and People, is one person: counted once.
    #[test]
    fn a_top_person_is_known_in_people_by_id() {
        let mut people = group(SearchKind::People, Vec::<SearchResult>::new());
        people.people = vec![spoolframe_types::Person {
            count: Some(4),
            ..person()
        }];
        let results = SearchResults {
            top: Some(TopResult::Person(person())),
            groups: vec![people],
            ..SearchResults::default()
        };
        assert_eq!(results.clone().only_from(None).counts.all, Some(1));
    }

    #[test]
    fn hidden_episodes_stay_hidden_only_under_a_show_left() {
        let mut episodes = group(SearchKind::Episodes, Vec::new());
        episodes.hidden = 3;
        let results = SearchResults {
            groups: vec![
                group(SearchKind::Shows, vec![hit(show("Severance"), Some("sa"))]),
                episodes,
            ],
            ..SearchResults::default()
        };
        assert_eq!(results.clone().only_from(Some("sa")).groups[1].hidden, 3);
        assert_eq!(
            results.clone().only_from(Some("sa")).counts.episodes,
            Some(3)
        );
        assert_eq!(results.clone().only_from(None).groups[1].hidden, 0);
    }

    #[test]
    fn see_all_names_the_groups_total_else_its_scopes_count() {
        let mut movies = group(SearchKind::Movies, vec![movie(own("1"), "Alien")]);
        movies.more = true;
        let mut results = SearchResults {
            groups: vec![movies, group(SearchKind::Shows, Vec::new())],
            counts: SearchCounts {
                movies: Some(23),
                ..SearchCounts::default()
            },
            ..SearchResults::default()
        };
        assert_eq!(results.see_all_count(SearchKind::Movies), Some(23));
        results.groups[0].total = Some(25);
        assert_eq!(results.see_all_count(SearchKind::Movies), Some(25));
        // Nothing more to see.
        assert_eq!(results.see_all_count(SearchKind::Shows), None);
    }

    #[test]
    fn hidden_episodes_name_the_shows_that_hold_them() {
        let mut episodes = group(SearchKind::Episodes, Vec::new());
        episodes.hidden = 3;
        let results = SearchResults {
            top: Some(TopResult::Item(show("Severance").into())),
            groups: vec![
                group(SearchKind::Shows, vec![show("Silo").into()]),
                episodes,
            ],
            ..SearchResults::default()
        };
        assert_eq!(
            results.hidden_under(SearchKind::Episodes),
            Some(HiddenUnder {
                count: 3,
                shows: vec!["Silo".into(), "Severance".into()],
            })
        );
        assert_eq!(results.hidden_under(SearchKind::Shows), None);
    }
}
