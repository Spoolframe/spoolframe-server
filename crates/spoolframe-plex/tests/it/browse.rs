//! Sorting and filtering a section by the PMS's own parameters
//! against a fake PMS that sorts and
//! filters as a real one does (checked against PMS 1.42): `>>` and `<<`
//! strict, commas any of, `push`/`or`/`pop` groups, no value first when
//! ascending, an unknown filter ignored.

use std::cmp::Ordering;

use serde_json::{Value, json};
use spoolframe_catalog::{Catalog, CatalogError, ItemQuery, Page};
use spoolframe_plex::PlexCatalog;
use spoolframe_types::{
    DynamicRange, FilterPart, Item, ItemFilter, ItemSort, Library, LibraryId, LibraryKind, MediaId,
    MediaKind, Namespace, Resolution, SortKey,
};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// One movie of the fake section.
#[derive(Clone)]
struct Film {
    key: u32,
    title: &'static str,
    title_sort: Option<&'static str>,
    year: Option<u16>,
    date: Option<&'static str>,
    added: i64,
    rating: Option<f64>,
    audience: Option<f64>,
    duration: Option<u64>,
    content: Option<&'static str>,
    genres: &'static [u32],
    resolution: &'static str,
    hdr: bool,
    dovi: bool,
}

const GENRES: &[(u32, &str)] = &[
    (138, "Action"),
    (139, "Comedy"),
    (140, "Drama"),
    (141, "Science Fiction"),
];

fn films() -> Vec<Film> {
    let film = |key, title| Film {
        key,
        title,
        title_sort: None,
        year: None,
        date: None,
        added: 1_000,
        rating: None,
        audience: None,
        duration: None,
        content: None,
        genres: &[],
        resolution: "1080",
        hdr: false,
        dovi: false,
    };
    vec![
        Film {
            title_sort: Some("Matrix"),
            year: Some(1999),
            date: Some("1999-03-31"),
            added: 1_100,
            rating: Some(8.7),
            audience: Some(8.5),
            duration: Some(8_160_000),
            content: Some("R"),
            genres: &[138, 141],
            ..film(1, "The Matrix")
        },
        Film {
            year: Some(1979),
            date: Some("1979-05-25"),
            added: 1_200,
            rating: Some(9.8),
            audience: Some(9.4),
            duration: Some(7_020_000),
            content: Some("R"),
            genres: &[141],
            resolution: "4k",
            hdr: true,
            ..film(2, "Alien")
        },
        Film {
            year: Some(1986),
            date: Some("1986-07-18"),
            added: 1_300,
            audience: Some(9.4),
            duration: Some(8_220_000),
            content: Some("R"),
            genres: &[138, 141],
            ..film(3, "Aliens")
        },
        Film {
            year: Some(1995),
            date: Some("1995-12-15"),
            added: 1_400,
            rating: Some(8.7),
            audience: Some(9.4),
            duration: Some(10_200_000),
            content: Some("R"),
            genres: &[138, 140],
            resolution: "4k",
            dovi: true,
            ..film(4, "Heat")
        },
        Film {
            year: Some(2016),
            date: Some("2016-11-11"),
            added: 1_500,
            rating: Some(9.4),
            duration: Some(6_960_000),
            content: Some("PG-13"),
            genres: &[140, 141],
            resolution: "4k",
            hdr: true,
            dovi: true,
            ..film(5, "Arrival")
        },
        Film {
            year: Some(2009),
            date: Some("2009-05-29"),
            added: 1_600,
            rating: Some(9.8),
            audience: Some(9.0),
            duration: Some(5_760_000),
            content: Some("PG"),
            genres: &[139],
            resolution: "720",
            ..film(6, "Up")
        },
        Film {
            year: Some(1985),
            added: 1_700,
            duration: Some(8_520_000),
            content: Some("16+"),
            genres: &[139, 140],
            resolution: "480",
            ..film(7, "Brazil")
        },
        Film {
            title_sort: Some("Hamlet"),
            year: Some(1990),
            date: Some("1990-12-19"),
            added: 1_700,
            rating: Some(7.0),
            duration: Some(8_100_000),
            content: Some("PG"),
            genres: &[140],
            resolution: "576",
            ..film(8, "Hamlet")
        },
        Film {
            title_sort: Some("Hamlet"),
            year: Some(1996),
            date: Some("1996-12-25"),
            added: 1_800,
            rating: Some(7.0),
            duration: Some(14_520_000),
            content: Some("PG-13"),
            genres: &[140],
            ..film(9, "Hamlet")
        },
        Film {
            title_sort: Some("Quiet Place"),
            year: Some(2018),
            date: Some("2018-04-06"),
            added: 1_900,
            rating: Some(9.6),
            duration: Some(5_400_000),
            content: Some("PG-13"),
            genres: &[141],
            resolution: "4k",
            hdr: true,
            ..film(10, "A Quiet Place")
        },
        Film {
            year: Some(1985),
            date: Some("1985-12-13"),
            added: 1_700,
            rating: Some(6.9),
            duration: Some(5_640_000),
            content: Some("PG"),
            genres: &[139],
            ..film(11, "Clue")
        },
        Film {
            year: Some(2021),
            date: Some("2021-10-22"),
            added: 2_000,
            rating: Some(8.3),
            audience: Some(9.0),
            content: Some("PG-13"),
            genres: &[138, 141],
            resolution: "4k",
            hdr: true,
            ..film(12, "Dune")
        },
    ]
}

impl Film {
    fn sort_title(&self) -> &str {
        self.title_sort.unwrap_or(self.title)
    }

    fn json(&self) -> Value {
        let mut m = json!({
            "ratingKey": self.key.to_string(),
            "key": format!("/library/metadata/{}", self.key),
            "type": "movie",
            "title": self.title,
            "addedAt": self.added,
            "librarySectionID": 2,
        });
        let o = m.as_object_mut().unwrap();
        if let Some(y) = self.year {
            o.insert("year".into(), json!(y));
        }
        if let Some(d) = self.duration {
            o.insert("duration".into(), json!(d));
        }
        if let Some(c) = self.content {
            o.insert("contentRating".into(), json!(c));
        }
        m
    }

    /// The value Plex sorts by `field`, as text that sorts as it does.
    fn field(&self, field: &str) -> Option<String> {
        match field {
            "titleSort" => Some(self.sort_title().to_lowercase()),
            "originallyAvailableAt" => self.date.map(str::to_owned),
            "addedAt" => Some(format!("{:012}", self.added)),
            "rating" => self.rating.map(|r| format!("{:08.3}", r)),
            "audienceRating" => self.audience.map(|r| format!("{:08.3}", r)),
            "duration" => self.duration.map(|d| format!("{d:015}")),
            other => panic!("the fake PMS does not sort by {other}"),
        }
    }

    fn passes(&self, key: &str, value: &str) -> bool {
        let any = |values: &str, f: &dyn Fn(&str) -> bool| values.split(',').any(f);
        match key {
            "genre" => any(value, &|v| self.genres.iter().any(|g| g.to_string() == v)),
            "contentRating" => any(value, &|v| self.content == Some(v)),
            "resolution" => any(value, &|v| self.resolution == v),
            "year" => any(value, &|v| {
                self.year.map(|y| y.to_string()).as_deref() == Some(v)
            }),
            "year>>" => self
                .year
                .is_some_and(|y| i64::from(y) > value.parse::<i64>().unwrap()),
            "year<<" => self
                .year
                .is_some_and(|y| i64::from(y) < value.parse::<i64>().unwrap()),
            "hdr" => self.hdr == (value == "1"),
            "dovi" => self.dovi == (value == "1"),
            "rating>>" => self.rating.is_some_and(|r| r > value.parse().unwrap()),
            "audienceRating>>" => self.audience.is_some_and(|r| r > value.parse().unwrap()),
            "duration>>" => self
                .duration
                .is_some_and(|d| d as i64 > value.parse::<i64>().unwrap()),
            "originallyAvailableAt>>" => self.date.is_some_and(|d| d > value),
            // A filter a PMS does not know is ignored.
            _ => true,
        }
    }
}

/// A filter expression: `push`/`or`/`pop` groups, as Plex reads them.
enum Expr {
    Is(String, String),
    /// Any of these, each all of its parts.
    Any(Vec<Vec<Expr>>),
}

impl Expr {
    fn passes(&self, film: &Film) -> bool {
        match self {
            Expr::Is(k, v) => film.passes(k, v),
            Expr::Any(alternatives) => alternatives
                .iter()
                .any(|all| all.iter().all(|e| e.passes(film))),
        }
    }
}

fn group(pairs: &mut std::vec::IntoIter<(String, String)>) -> Vec<Vec<Expr>> {
    let mut alternatives = vec![Vec::new()];
    while let Some((k, v)) = pairs.next() {
        match k.as_str() {
            "push" => {
                let inner = group(pairs);
                alternatives.last_mut().unwrap().push(Expr::Any(inner));
            }
            "pop" => break,
            "or" => alternatives.push(Vec::new()),
            "sort" | "excludeElements" | "excludeFields" => {}
            _ => alternatives.last_mut().unwrap().push(Expr::Is(k, v)),
        }
    }
    alternatives
}

/// The fake PMS: one movie section, `2`.
struct Pms {
    films: Vec<Film>,
    /// Whether the section has the `dovi` filter, as a PMS from 2023 on.
    dovi: bool,
}

fn container(entries: Vec<Value>, total: usize, key: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "MediaContainer": {
            "size": entries.len(),
            "totalSize": total,
            "librarySectionID": 2,
            key: entries,
        }
    }))
}

fn directory(entries: &[(&str, &str)]) -> ResponseTemplate {
    let entries = entries
        .iter()
        .map(|(key, title)| json!({"key": key, "title": title}))
        .collect::<Vec<_>>();
    let n = entries.len();
    container(entries, n, "Directory")
}

impl Respond for Pms {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let path = request.url.path();
        let pairs: Vec<(String, String)> = request
            .url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        match path {
            "/library/sections/2/sorts" => directory(&[
                ("titleSort", "Title"),
                ("originallyAvailableAt", "Release Date"),
                ("rating", "Critic Rating"),
                ("audienceRating", "Audience Rating"),
                ("duration", "Duration"),
                ("addedAt", "Date Added"),
                ("lastViewedAt", "Date Viewed"),
            ]),
            "/library/sections/2/filters" => {
                let mut filters = vec!["genre", "year", "contentRating", "resolution", "hdr"];
                if self.dovi {
                    filters.push("dovi");
                }
                let entries: Vec<Value> = filters
                    .into_iter()
                    .map(|f| json!({"filter": f, "key": format!("/library/sections/2/{f}"), "title": f}))
                    .collect();
                let n = entries.len();
                container(entries, n, "Directory")
            }
            "/library/sections/2/genre" => {
                let keys: Vec<(String, &str)> =
                    GENRES.iter().map(|(k, t)| (k.to_string(), *t)).collect();
                let refs: Vec<(&str, &str)> = keys.iter().map(|(k, t)| (k.as_str(), *t)).collect();
                directory(&refs)
            }
            "/library/sections/2/contentRating" => directory(&[
                ("16%2B", "16+"),
                ("PG", "PG"),
                ("PG-13", "PG-13"),
                ("R", "R"),
            ]),
            "/library/sections/2/resolution" => directory(&[
                ("4k", "4K"),
                ("1080", "1080p"),
                ("720", "720p"),
                ("576", "576p"),
                ("480", "480p"),
            ]),
            "/library/sections/2/year" => {
                let mut years: Vec<String> = self
                    .films
                    .iter()
                    .filter_map(|f| f.year.map(|y| y.to_string()))
                    .collect();
                years.sort();
                years.dedup();
                years.reverse();
                let refs: Vec<(&str, &str)> =
                    years.iter().map(|y| (y.as_str(), y.as_str())).collect();
                directory(&refs)
            }
            "/library/sections/2/all" => self.all(request, pairs),
            p if p.starts_with("/library/metadata/") => {
                let keys: Vec<&str> = p["/library/metadata/".len()..].split(',').collect();
                // In no particular order: the reverse of what was asked.
                let entries: Vec<Value> = keys
                    .iter()
                    .rev()
                    .filter_map(|k| self.films.iter().find(|f| f.key.to_string() == *k))
                    .map(Film::json)
                    .collect();
                let n = entries.len();
                container(entries, n, "Metadata")
            }
            other => ResponseTemplate::new(404).set_body_string(format!("no {other}")),
        }
    }
}

impl Pms {
    fn all(&self, request: &Request, pairs: Vec<(String, String)>) -> ResponseTemplate {
        let sort = pairs
            .iter()
            .find(|(k, _)| k == "sort")
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "titleSort".into());
        let filter = Expr::Any(group(&mut pairs.into_iter()));
        let mut kept: Vec<&Film> = self.films.iter().filter(|f| filter.passes(f)).collect();
        let specs: Vec<(String, bool)> = sort
            .split(',')
            .map(|s| match s.split_once(':') {
                Some((f, "desc")) => (f.to_owned(), true),
                _ => (s.to_owned(), false),
            })
            .collect();
        kept.sort_by(|a, b| {
            for (field, desc) in &specs {
                let (x, y) = (a.field(field), b.field(field));
                // No value is least: first ascending, last descending.
                let order = x.cmp(&y);
                let order = if *desc { order.reverse() } else { order };
                if order != Ordering::Equal {
                    return order;
                }
            }
            a.key.cmp(&b.key)
        });
        let header = |name: &str| {
            request
                .headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<usize>().ok())
        };
        let start = header("X-Plex-Container-Start").unwrap_or(0);
        let size = header("X-Plex-Container-Size").unwrap_or(usize::MAX);
        let total = kept.len();
        let page: Vec<Value> = kept
            .into_iter()
            .skip(start)
            .take(size)
            .map(Film::json)
            .collect();
        container(page, total, "Metadata")
    }
}

async fn pms(dovi: bool) -> (MockServer, PlexCatalog) {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::method("GET"))
        .respond_with(Pms {
            films: films(),
            dovi,
        })
        .mount(&server)
        .await;
    let catalog = PlexCatalog::new(server.uri().parse().unwrap(), None).unwrap();
    (server, catalog)
}

fn movies() -> Library {
    Library {
        id: LibraryId::new(Namespace::Plex, "2").unwrap(),
        title: "Movies".into(),
        kind: LibraryKind::Movies,
        server: None,
    }
}

fn query(key: SortKey, descending: bool, filter: ItemFilter) -> ItemQuery {
    ItemQuery {
        sort: ItemSort { key, descending },
        filter,
        viewer: None,
    }
}

fn keys(items: &[Item]) -> Vec<u32> {
    items
        .iter()
        .map(|i| i.id().key().parse().unwrap())
        .collect()
}

async fn listed(catalog: &PlexCatalog, query: &ItemQuery) -> (Vec<u32>, u32) {
    let page = catalog
        .query_items(
            &movies().id,
            query,
            Page {
                offset: 0,
                limit: 100,
            },
        )
        .await
        .unwrap();
    (keys(&page.items), page.total)
}

/// What `key` orders by, as the test reads a film: no value is `None`.
fn value(film: &Film, key: SortKey) -> Option<String> {
    let field = match key {
        SortKey::Title => "titleSort",
        SortKey::Released => "originallyAvailableAt",
        SortKey::Added => "addedAt",
        SortKey::Rating => "rating",
        SortKey::AudienceRating => "audienceRating",
        SortKey::Duration => "duration",
        other => panic!("{other:?} is not Plex's"),
    };
    film.field(field)
}

/// What the films are in, by `key`: those with a value in its order,
/// ties by sort title A to Z, then those without one.
fn expected(key: SortKey, descending: bool) -> Vec<u32> {
    let mut all = films();
    all.sort_by(|a, b| {
        let (x, y) = (value(a, key), value(b, key));
        let by_key = match (&x, &y) {
            (Some(x), Some(y)) if descending => y.cmp(x),
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        };
        let by_title = if key == SortKey::Title {
            Ordering::Equal
        } else {
            a.sort_title()
                .to_lowercase()
                .cmp(&b.sort_title().to_lowercase())
        };
        by_key.then(by_title).then(a.key.cmp(&b.key))
    });
    all.iter().map(|f| f.key).collect()
}

const PLEX_KEYS: [SortKey; 6] = [
    SortKey::Title,
    SortKey::Released,
    SortKey::Added,
    SortKey::Rating,
    SortKey::AudienceRating,
    SortKey::Duration,
];

#[tokio::test]
async fn every_key_sorts_both_ways_with_no_value_last() {
    let (_server, catalog) = pms(true).await;
    for key in PLEX_KEYS {
        for descending in [false, true] {
            let (got, total) =
                listed(&catalog, &query(key, descending, ItemFilter::default())).await;
            assert_eq!(total, 12, "{key:?} {descending}");
            assert_eq!(
                got,
                expected(key, descending),
                "{key:?} descending {descending}"
            );
        }
    }
}

#[tokio::test]
async fn title_sets_a_leading_article_aside_and_ties_keep_one_order() {
    let (_server, catalog) = pms(true).await;
    let (got, _) = listed(
        &catalog,
        &query(SortKey::Title, false, ItemFilter::default()),
    )
    .await;
    // "A Quiet Place" is under Q, "The Matrix" under M; the two Hamlets
    // keep their order.
    assert_eq!(got, vec![2, 3, 5, 7, 11, 12, 8, 9, 4, 1, 10, 6]);
}

#[tokio::test]
async fn pages_of_every_order_never_repeat_or_skip_an_item() {
    let (_server, catalog) = pms(true).await;
    for key in PLEX_KEYS {
        for descending in [false, true] {
            let asked = query(key, descending, ItemFilter::default());
            let (whole, total) = listed(&catalog, &asked).await;
            let mut paged = Vec::new();
            for offset in (0..total).step_by(5) {
                let page = catalog
                    .query_items(&movies().id, &asked, Page { offset, limit: 5 })
                    .await
                    .unwrap();
                assert_eq!(page.total, total);
                paged.extend(keys(&page.items));
            }
            assert_eq!(paged, whole, "{key:?} descending {descending}");
        }
    }
}

/// Found against a real PMS: a page past the end of a listing whose
/// items without a value go last.
#[tokio::test]
async fn a_page_past_the_end_is_empty_and_says_the_total() {
    let (_server, catalog) = pms(true).await;
    for key in PLEX_KEYS {
        for offset in [12, 13, 200] {
            let page = catalog
                .query_items(
                    &movies().id,
                    &query(key, false, ItemFilter::default()),
                    Page { offset, limit: 5 },
                )
                .await
                .unwrap();
            assert_eq!((page.items.len(), page.total), (0, 12), "{key:?} {offset}");
        }
    }
}

#[tokio::test]
async fn a_walk_is_the_listing_in_order() {
    let (_server, catalog) = pms(true).await;
    for key in PLEX_KEYS {
        for descending in [false, true] {
            let asked = query(key, descending, ItemFilter::default());
            let walked: Vec<u32> = catalog
                .walk_items(&movies().id, &asked)
                .await
                .unwrap()
                .into_iter()
                .map(|l| l.id.key().parse().unwrap())
                .collect();
            assert_eq!(walked, expected(key, descending), "{key:?} {descending}");
        }
    }
}

async fn filtered(catalog: &PlexCatalog, filter: ItemFilter) -> Vec<u32> {
    let (mut got, total) = listed(catalog, &query(SortKey::Title, false, filter)).await;
    assert_eq!(total as usize, got.len());
    got.sort_unstable();
    got
}

#[tokio::test]
async fn genres_are_any_of_by_name_case_aside() {
    let (_server, catalog) = pms(true).await;
    let genres = |names: &[&str]| ItemFilter {
        genres: names.iter().map(|n| (*n).to_owned()).collect(),
        ..ItemFilter::default()
    };
    assert_eq!(
        filtered(&catalog, genres(&["comedy"])).await,
        vec![6, 7, 11]
    );
    assert_eq!(
        filtered(&catalog, genres(&["Comedy", "action"])).await,
        vec![1, 3, 4, 6, 7, 11, 12]
    );
    assert_eq!(
        filtered(&catalog, genres(&["Western"])).await,
        Vec::<u32>::new()
    );
}

#[tokio::test]
async fn years_are_inclusive_and_a_decade_is_a_range() {
    let (_server, catalog) = pms(true).await;
    let years = |from, to| ItemFilter {
        year_from: from,
        year_to: to,
        ..ItemFilter::default()
    };
    assert_eq!(
        filtered(&catalog, years(Some(1990), Some(1999))).await,
        vec![1, 4, 8, 9]
    );
    assert_eq!(
        filtered(&catalog, years(Some(1985), Some(1985))).await,
        vec![7, 11]
    );
    assert_eq!(
        filtered(&catalog, years(Some(2016), None)).await,
        vec![5, 10, 12]
    );
    assert_eq!(filtered(&catalog, years(None, Some(1979))).await, vec![2]);
}

#[tokio::test]
async fn content_ratings_and_resolutions_are_any_of() {
    let (_server, catalog) = pms(true).await;
    let rated = ItemFilter {
        content_ratings: vec!["16+".into(), "pg".into()],
        ..ItemFilter::default()
    };
    assert_eq!(filtered(&catalog, rated).await, vec![6, 7, 8, 11]);
    let resolutions = |r: &[Resolution]| ItemFilter {
        resolutions: r.to_vec(),
        ..ItemFilter::default()
    };
    assert_eq!(
        filtered(&catalog, resolutions(&[Resolution::Uhd])).await,
        vec![2, 4, 5, 10, 12]
    );
    assert_eq!(
        filtered(&catalog, resolutions(&[Resolution::Sd])).await,
        vec![7, 8]
    );
    assert_eq!(
        filtered(&catalog, resolutions(&[Resolution::Hd, Resolution::FullHd])).await,
        vec![1, 3, 6, 9, 11]
    );
}

#[tokio::test]
async fn hdr_takes_in_dolby_vision_and_sdr_is_neither() {
    let (_server, catalog) = pms(true).await;
    let ranges = |r: &[DynamicRange]| ItemFilter {
        dynamic_ranges: r.to_vec(),
        ..ItemFilter::default()
    };
    use DynamicRange::*;
    assert_eq!(
        filtered(&catalog, ranges(&[Hdr])).await,
        vec![2, 4, 5, 10, 12]
    );
    assert_eq!(filtered(&catalog, ranges(&[DolbyVision])).await, vec![4, 5]);
    assert_eq!(
        filtered(&catalog, ranges(&[Sdr])).await,
        vec![1, 3, 6, 7, 8, 9, 11]
    );
    assert_eq!(
        filtered(&catalog, ranges(&[Sdr, DolbyVision])).await,
        vec![1, 3, 4, 5, 6, 7, 8, 9, 11]
    );
}

#[tokio::test]
async fn parts_combine_with_and() {
    let (_server, catalog) = pms(true).await;
    let filter = ItemFilter {
        genres: vec!["Science Fiction".into()],
        year_from: Some(1980),
        resolutions: vec![Resolution::Uhd],
        ..ItemFilter::default()
    };
    assert_eq!(filtered(&catalog, filter).await, vec![5, 10, 12]);
    // And sorted, newest first.
    let (got, total) = listed(
        &catalog,
        &query(
            SortKey::Released,
            true,
            ItemFilter {
                genres: vec!["Science Fiction".into()],
                resolutions: vec![Resolution::Uhd],
                ..ItemFilter::default()
            },
        ),
    )
    .await;
    assert_eq!((got, total), (vec![12, 10, 5, 2], 4));
}

#[tokio::test]
async fn a_filtered_ascending_listing_pages_through_the_items_without_a_value() {
    let (_server, catalog) = pms(true).await;
    // Rated R or PG-13: eight, of which Aliens alone has no critic
    // rating, so it is last.
    let asked = query(
        SortKey::Rating,
        false,
        ItemFilter {
            content_ratings: vec!["R".into(), "PG-13".into()],
            ..ItemFilter::default()
        },
    );
    let (whole, total) = listed(&catalog, &asked).await;
    assert_eq!(total, 8);
    assert_eq!(whole.last(), Some(&3));
    let mut paged = Vec::new();
    for offset in (0..total).step_by(3) {
        let page = catalog
            .query_items(&movies().id, &asked, Page { offset, limit: 3 })
            .await
            .unwrap();
        paged.extend(keys(&page.items));
    }
    assert_eq!(paged, whole);
}

#[tokio::test]
async fn a_pms_without_the_dovi_filter_offers_no_dolby_vision_and_is_never_sent_one() {
    let (server, catalog) = pms(false).await;
    let browsing = catalog.browsing(&movies()).await.unwrap();
    assert_eq!(
        browsing.dynamic_ranges,
        vec![DynamicRange::Sdr, DynamicRange::Hdr]
    );
    let dv = ItemFilter {
        dynamic_ranges: vec![DynamicRange::DolbyVision],
        ..ItemFilter::default()
    };
    let asked = catalog
        .query_items(
            &movies().id,
            &query(SortKey::Title, false, dv),
            Page {
                offset: 0,
                limit: 10,
            },
        )
        .await;
    assert!(
        matches!(asked, Err(CatalogError::CannotBrowse(_))),
        "{asked:?}"
    );
    let requests = server.received_requests().await.unwrap();
    assert!(
        requests.iter().all(|r| !r.url.as_str().contains("dovi")),
        "a filter the PMS does not list is never sent"
    );
    // Without it, HDR is what Plex calls HDR.
    let hdr = ItemFilter {
        dynamic_ranges: vec![DynamicRange::Hdr],
        ..ItemFilter::default()
    };
    assert_eq!(filtered(&catalog, hdr).await, vec![2, 5, 10, 12]);
}

#[tokio::test]
async fn no_size_sort_and_the_watch_state_is_the_server_s() {
    let (_server, catalog) = pms(true).await;
    let browsing = catalog.browsing(&movies()).await.unwrap();
    assert!(!browsing.sorts.contains(&SortKey::Size));
    assert!(browsing.sorts.contains(&SortKey::LastPlayed));
    assert!(browsing.filters.contains(&FilterPart::Watched));
    assert_eq!(browsing.watch, spoolframe_catalog::WatchParts::ByServer);
    let size = catalog
        .query_items(
            &movies().id,
            &query(SortKey::Size, false, ItemFilter::default()),
            Page {
                offset: 0,
                limit: 10,
            },
        )
        .await;
    assert!(
        matches!(size, Err(CatalogError::CannotBrowse(_))),
        "{size:?}"
    );
}

#[tokio::test]
async fn items_by_id_come_in_the_order_asked() {
    let (_server, catalog) = pms(true).await;
    let ids: Vec<MediaId> = [5, 1, 12, 99]
        .iter()
        .map(|k| MediaId::new(Namespace::Plex, MediaKind::Movie, k.to_string()).unwrap())
        .collect();
    let items = catalog.items_by_id(&ids).await.unwrap();
    assert_eq!(keys(&items), vec![5, 1, 12]);
}

#[tokio::test]
async fn filter_values_are_what_the_section_has() {
    let (_server, catalog) = pms(true).await;
    let values = catalog.filter_values(&movies()).await.unwrap();
    let genres: Vec<&str> = values.genres.iter().map(|g| g.value.as_str()).collect();
    assert_eq!(genres, vec!["Action", "Comedy", "Drama", "Science Fiction"]);
    let years: Vec<u16> = values.years.iter().map(|y| y.value).collect();
    assert_eq!(years[0], 2021);
    assert_eq!(years.last(), Some(&1979));
    let ratings: Vec<&str> = values
        .content_ratings
        .iter()
        .map(|g| g.value.as_str())
        .collect();
    assert_eq!(ratings, vec!["16+", "PG", "PG-13", "R"]);
    let resolutions: Vec<(Resolution, Option<u32>)> = values
        .resolutions
        .iter()
        .map(|r| (r.value, r.count))
        .collect();
    assert_eq!(
        resolutions,
        vec![
            (Resolution::Uhd, Some(5)),
            (Resolution::FullHd, Some(4)),
            (Resolution::Hd, Some(1)),
            (Resolution::Sd, Some(2)),
        ]
    );
    let ranges: Vec<(DynamicRange, Option<u32>)> = values
        .dynamic_ranges
        .iter()
        .map(|r| (r.value, r.count))
        .collect();
    assert_eq!(
        ranges,
        vec![
            (DynamicRange::Sdr, Some(7)),
            (DynamicRange::Hdr, Some(5)),
            (DynamicRange::DolbyVision, Some(2)),
        ]
    );
    assert!(!values.sorts.contains(&SortKey::Size));
}
