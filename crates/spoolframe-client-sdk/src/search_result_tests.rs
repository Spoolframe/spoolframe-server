//! Both searches' results as one kind, narrowed to one server with the
//! counts made again.

use spoolframe_protocol::{SearchCounts, SearchGroup, TopResult};
use spoolframe_types::{MediaId, Namespace, Person, PersonId, PersonRole, SearchKind};

use super::*;
use crate::test_fixtures::{hit, movie};

fn movies<T>(items: Vec<T>) -> SearchResults<T> {
    SearchResults {
        groups: vec![SearchGroup {
            kind: SearchKind::Movies,
            items,
            people: Vec::new(),
            more: false,
            total: None,
            hidden: 0,
        }],
        counts: SearchCounts {
            all: Some(40),
            movies: Some(40),
            ..SearchCounts::default()
        },
        ..SearchResults::default()
    }
}

/// A plain search's items are this server's own, with no copies: the
/// user's server keeps them all and a paired one none.
#[test]
fn a_plain_search_s_items_are_this_server_s() {
    let results = search_results(movies(vec![movie("plex:movie:1", "Alien")]));
    let own = results.clone().only_from(None);
    assert_eq!(
        (
            own.groups[0].items[0].copies.is_none(),
            own.counts.movies,
            results.clone().only_from(Some("sa")).counts.movies,
        ),
        (true, Some(1), Some(0))
    );
}

/// A federated search's hits keep where they are from as their copies,
/// and one server's are counted as they are left.
#[test]
fn a_hit_s_copies_say_where_it_is_from() {
    let own: MediaId = "plex:movie:3".parse().unwrap();
    let fed = MediaId::federated("sa", &own).unwrap();
    let results = search_results(movies(vec![
        hit(movie("plex:movie:2", "Aliens"), None),
        hit(movie(&fed.to_string(), "Alien 3"), Some("sa")),
    ]));
    let sa = results.clone().only_from(Some("sa"));
    let kept = &sa.groups[0].items[0];
    assert_eq!(
        (
            kept.item.id().clone(),
            kept.copies.as_ref().map(|c| c.play_from.clone()),
            sa.counts.movies,
            sa.counts.all,
            matches!(results.top, None | Some(TopResult::Person(_))),
        ),
        (fed.clone(), Some(fed), Some(1), Some(1), true)
    );
}

fn weaver(key: &str) -> Person {
    Person {
        id: PersonId::new(Namespace::Plex, key).unwrap(),
        name: format!("Sigourney Weaver {key}"),
        role: PersonRole::Actor,
        count: Some(3),
    }
}

/// `results` with `top` the top person and a People group of `listed`
/// (none when empty), after the movies.
fn person_on_top<T>(results: SearchResults<T>, listed: Vec<Person>) -> SearchResults<T> {
    let mut results = SearchResults {
        top: Some(TopResult::Person(weaver("top"))),
        ..results
    };
    if !listed.is_empty() {
        results.groups.push(SearchGroup {
            kind: SearchKind::People,
            items: Vec::new(),
            people: listed,
            more: false,
            total: Some(2),
            hidden: 0,
        });
    }
    results
}

fn people<T>(results: &SearchResults<T>) -> Vec<Person> {
    results
        .groups
        .iter()
        .find(|g| g.kind == SearchKind::People)
        .map(|g| g.people.clone())
        .unwrap_or_default()
}

/// The top person is listed first in People, from a plain search and a
/// federated one alike.
#[test]
fn a_top_person_comes_first_in_people_from_either_search() {
    let plain = search_results(person_on_top(
        movies(vec![movie("plex:movie:1", "Alien")]),
        vec![weaver("other")],
    ));
    let federated = search_results(person_on_top(
        movies(vec![hit(movie("plex:movie:1", "Alien"), None)]),
        vec![weaver("other")],
    ));
    let expected = vec![weaver("top"), weaver("other")];
    assert_eq!(
        (people(&plain), people(&federated)),
        (expected.clone(), expected)
    );
}

/// With no other person found, a People group holds the top one, after
/// the movies and before the music, and says it is all of them.
#[test]
fn a_top_person_alone_gets_a_people_group_in_its_place() {
    let mut results = person_on_top(movies(vec![movie("plex:movie:1", "Alien")]), Vec::new());
    results.groups.push(SearchGroup {
        kind: SearchKind::Artists,
        items: Vec::new(),
        people: Vec::new(),
        more: false,
        total: None,
        hidden: 0,
    });
    let results = search_results(results);
    assert_eq!(
        results
            .groups
            .iter()
            .map(|g| (g.kind, g.people.clone(), g.total))
            .collect::<Vec<_>>(),
        [
            (SearchKind::Movies, vec![], None),
            (SearchKind::People, vec![weaver("top")], Some(1)),
            (SearchKind::Artists, vec![], None),
        ]
    );
}

/// The user's own server's results count the top person once, now that
/// People lists them too; a paired server's have no people.
#[test]
fn the_filter_counts_a_listed_top_person_once() {
    let results = search_results(person_on_top(
        movies(vec![hit(movie("plex:movie:1", "Alien"), None)]),
        vec![weaver("other")],
    ));
    let own = results.clone().only_from(None);
    let sa = results.clone().only_from(Some("sa"));
    assert_eq!(
        (
            own.counts.all,
            people(&own).len(),
            sa.counts.all,
            people(&sa).len()
        ),
        (Some(3), 2, Some(0), 0)
    );
}

/// The top person is known in People by id: the same person with
/// another item count is listed once, not twice.
#[test]
fn a_top_person_already_in_people_by_id_is_listed_once() {
    let listed = Person {
        count: Some(9),
        ..weaver("top")
    };
    let results = search_results(person_on_top(
        movies(vec![movie("plex:movie:1", "Alien")]),
        vec![listed.clone()],
    ));
    assert_eq!(people(&results), vec![listed]);
}
