//! A paired server's library sorted and filtered in its cache
//! (`ItemFilterValues` and `Items`' sort and filter): one query over
//! `federation_cache_items` and its columns (migration 0060), the session
//! user's watch state joined in as JSON the caller made of their rows.
//!
//! Every sort key's column is indexed with the library's (origin,
//! library, kind), and a page reads only its bodies; an item without a
//! value for the key comes last both ways, then items equal by it go by
//! sort title and id.

use sqlx::{QueryBuilder, Sqlite};

use spoolframe_protocol::federation::RemoteItem;
use spoolframe_types::{
    Counted, DynamicRange, ItemFilter, ItemFilterValues, ItemSort, Resolution, SortKey,
    WatchedState,
};

use super::federation_cache::bodies;
use super::{Store, StoreError};

/// Which cached items a query is of: an origin's library, its items of
/// one kind (`movie`, `show`, `artist`), available.
#[derive(Debug, Clone, Copy)]
pub struct CachedLibraryOf<'a> {
    pub origin: &'a str,
    pub library: &'a str,
    pub kind: &'a str,
}

/// One seen item of the library, by its id on the origin: the session
/// user's state of it, and when it last changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenCached {
    pub item: String,
    pub state: WatchedState,
    pub last: Option<i64>,
}

/// The sort's column, of `i` (the item) or `s` (its seen row).
fn sort_column(key: SortKey) -> Option<&'static str> {
    Some(match key {
        // Written for every row (and by migration 0060 for those before).
        SortKey::Title => "i.sort_title COLLATE NOCASE",
        SortKey::Released => "i.released",
        SortKey::Added => "i.added_at",
        SortKey::LastPlayed => "s.last",
        SortKey::Rating => "i.rating",
        SortKey::AudienceRating => "i.audience_rating",
        SortKey::Duration => "i.duration_ms",
        SortKey::Size => "i.size",
        SortKey::Unknown => return None,
    })
}

fn state_name(state: WatchedState) -> Option<&'static str> {
    Some(match state {
        WatchedState::Unwatched => "unwatched",
        WatchedState::InProgress => "in_progress",
        WatchedState::Watched => "watched",
        WatchedState::Unknown => return None,
    })
}

/// The ranges' column values: HDR is any HDR, Dolby Vision included.
fn range_values(ranges: &[DynamicRange]) -> Vec<&'static str> {
    let mut values = Vec::new();
    for range in ranges {
        match range {
            DynamicRange::Sdr => values.push("sdr"),
            DynamicRange::Hdr => values.extend(["hdr", "dolby_vision"]),
            DynamicRange::DolbyVision => values.push("dolby_vision"),
            DynamicRange::Unknown => {}
        }
    }
    values.sort_unstable();
    values.dedup();
    values
}

/// ` LIMIT … OFFSET …`.
fn push_page(query: &mut QueryBuilder<Sqlite>, offset: i64, limit: i64) {
    query.push(" LIMIT ");
    query.push_bind(limit);
    query.push(" OFFSET ");
    query.push_bind(offset);
}

/// `[WITH seen …] SELECT <what> FROM … WHERE …`: `of`'s items that
/// `filter` keeps, as `i`, with `seen` (the user's, as JSON), if given,
/// joined as `s`; a filter by watch state needs it.
fn push_select(
    query: &mut QueryBuilder<Sqlite>,
    of: CachedLibraryOf<'_>,
    filter: &ItemFilter,
    seen: Option<String>,
    what: &'static str,
) {
    let joined = seen.is_some();
    if let Some(seen) = seen {
        query.push(
            "WITH seen (item, state, last) AS \
             (SELECT value ->> 0, value ->> 1, value ->> 2 FROM json_each(",
        );
        query.push_bind(seen);
        query.push(")) ");
    }
    query.push("SELECT ");
    query.push(what);
    query.push(" FROM federation_cache_items i");
    if joined {
        query.push(" LEFT JOIN seen s ON s.item = i.item");
    }
    query.push(" WHERE i.unavailable = 0 AND i.origin = ");
    query.push_bind(of.origin.to_owned());
    query.push(" AND i.library = ");
    query.push_bind(of.library.to_owned());
    query.push(" AND i.kind = ");
    query.push_bind(of.kind.to_owned());
    // Each list: an empty IN keeps nothing (only values this build does
    // not know were asked, which are refused before).
    let any_of = |query: &mut QueryBuilder<Sqlite>, head: &str, values: Vec<String>| {
        query.push(head.to_owned());
        let mut list = query.separated(", ");
        for value in values {
            list.push_bind(value);
        }
        query.push(")");
    };
    if !filter.watched.is_empty() {
        let states = filter
            .watched
            .iter()
            .filter_map(|s| state_name(*s))
            .map(str::to_owned)
            .collect();
        any_of(query, " AND COALESCE(s.state, 'unwatched') IN (", states);
    }
    if !filter.genres.is_empty() {
        // Not correlated, so read once from the genre index into a set
        // that each item is looked up in.
        query
            .push(" AND i.item IN (SELECT g.item FROM federation_cache_genres g WHERE g.origin = ");
        query.push_bind(of.origin.to_owned());
        query.push(" AND g.library = ");
        query.push_bind(of.library.to_owned());
        query.push(" AND g.kind = ");
        query.push_bind(of.kind.to_owned());
        any_of(
            query,
            " AND g.genre COLLATE NOCASE IN (",
            filter.genres.clone(),
        );
        query.push(")");
    }
    if let Some(from) = filter.year_from {
        query.push(" AND i.year >= ");
        query.push_bind(i64::from(from));
    }
    if let Some(to) = filter.year_to {
        query.push(" AND i.year <= ");
        query.push_bind(i64::from(to));
    }
    if !filter.content_ratings.is_empty() {
        any_of(
            query,
            " AND i.content_rating COLLATE NOCASE IN (",
            filter.content_ratings.clone(),
        );
    }
    if !filter.resolutions.is_empty() {
        let classes = filter
            .resolutions
            .iter()
            .map(|r| r.as_str().to_owned())
            .collect();
        any_of(query, " AND i.resolution IN (", classes);
    }
    if !filter.dynamic_ranges.is_empty() {
        let ranges = range_values(&filter.dynamic_ranges)
            .into_iter()
            .map(str::to_owned)
            .collect();
        any_of(query, " AND i.dynamic_range IN (", ranges);
    }
}

/// `seen` as the JSON [`push_select`] joins: `[[item, state, last], …]`.
fn seen_json(seen: &[SeenCached]) -> String {
    let rows: Vec<(&str, Option<&str>, Option<i64>)> = seen
        .iter()
        .map(|s| (s.item.as_str(), state_name(s.state), s.last))
        .collect();
    serde_json::to_string(&rows).expect("rows serialize")
}

impl Store {
    /// A page of `of`'s items that `filter` keeps, ordered by `sort`, by
    /// the user's `seen` items where either asks; and how many it keeps.
    pub async fn cached_sorted(
        &self,
        of: CachedLibraryOf<'_>,
        sort: ItemSort,
        filter: &ItemFilter,
        seen: &[SeenCached],
        offset: u32,
        limit: u32,
    ) -> Result<(Vec<RemoteItem>, u32), StoreError> {
        let Some(column) = sort_column(sort.key) else {
            return Ok((Vec::new(), 0));
        };
        // The user's rows only where they are asked for: a join on JSON
        // is not indexed, and costs a read of every row.
        let seen = (!filter.watched.is_empty() || sort.key == SortKey::LastPlayed)
            .then(|| seen_json(seen));
        let direction = if sort.descending { "DESC" } else { "ASC" };
        let mut tx = self.pool.begin().await?;
        let count = |nulls: Option<bool>| {
            let mut counted: QueryBuilder<Sqlite> = QueryBuilder::new("");
            push_select(&mut counted, of, filter, seen.clone(), "COUNT(*)");
            match nulls {
                Some(true) => counted.push(format!(" AND {column} IS NULL")),
                Some(false) => counted.push(format!(" AND {column} IS NOT NULL")),
                None => &mut counted,
            };
            counted
        };
        let total: i64 = count(None).build_query_scalar().fetch_one(&mut *tx).await?;
        let (offset, limit) = (i64::from(offset), i64::from(limit));
        let rows: Vec<String> = if sort.key == SortKey::Title {
            // Never NULL: one walk of its index.
            let mut page: QueryBuilder<Sqlite> = QueryBuilder::new("");
            push_select(&mut page, of, filter, seen, "i.body");
            page.push(format!(" ORDER BY {column} {direction}, i.item"));
            push_page(&mut page, offset, limit);
            page.build_query_scalar().fetch_all(&mut *tx).await?
        } else {
            // Those with a value in order, then those without by title:
            // two walks of the key's index and the title's, where one
            // ordered by `col IS NULL` first would sort every row.
            let valued: i64 = count(Some(false))
                .build_query_scalar()
                .fetch_one(&mut *tx)
                .await?;
            let mut rows = Vec::new();
            if offset < valued {
                let mut page: QueryBuilder<Sqlite> = QueryBuilder::new("");
                push_select(&mut page, of, filter, seen.clone(), "i.body");
                page.push(format!(
                    " AND {column} IS NOT NULL ORDER BY {column} {direction}, \
                     i.sort_title COLLATE NOCASE, i.item"
                ));
                push_page(&mut page, offset, limit);
                rows = page.build_query_scalar().fetch_all(&mut *tx).await?;
            }
            let left = limit - i64::try_from(rows.len()).unwrap_or(i64::MAX);
            if left > 0 && offset + limit > valued {
                let mut page: QueryBuilder<Sqlite> = QueryBuilder::new("");
                push_select(&mut page, of, filter, seen, "i.body");
                page.push(format!(
                    " AND {column} IS NULL ORDER BY i.sort_title COLLATE NOCASE, i.item"
                ));
                push_page(&mut page, (offset - valued).max(0), left);
                rows.extend(page.build_query_scalar().fetch_all(&mut *tx).await?);
            }
            rows
        };
        tx.commit().await?;
        Ok((bodies(rows), u32::try_from(total).unwrap_or(u32::MAX)))
    }

    /// What `of`'s items have to filter by, each value counted.
    pub async fn cached_values(
        &self,
        of: CachedLibraryOf<'_>,
    ) -> Result<ItemFilterValues, StoreError> {
        let mut tx = self.pool.begin().await?;
        let genres: Vec<(String, i64)> = sqlx::query_as(
            "SELECT MIN(genre), COUNT(*) FROM federation_cache_genres \
             WHERE origin = ?1 AND library = ?2 AND kind = ?3 \
             GROUP BY genre COLLATE NOCASE ORDER BY genre COLLATE NOCASE",
        )
        .bind(of.origin)
        .bind(of.library)
        .bind(of.kind)
        .fetch_all(&mut *tx)
        .await?;
        let grouped = |column: &str, order: &str| {
            format!(
                "SELECT {column}, COUNT(*) FROM federation_cache_items \
                 WHERE origin = ?1 AND library = ?2 AND kind = ?3 AND unavailable = 0 \
                 AND {column} IS NOT NULL GROUP BY {column} ORDER BY {column} {order}"
            )
        };
        let years: Vec<(i64, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(grouped("year", "DESC")))
            .bind(of.origin)
            .bind(of.library)
            .bind(of.kind)
            .fetch_all(&mut *tx)
            .await?;
        let ratings: Vec<(String, i64)> =
            sqlx::query_as(sqlx::AssertSqlSafe(grouped("content_rating", "ASC")))
                .bind(of.origin)
                .bind(of.library)
                .bind(of.kind)
                .fetch_all(&mut *tx)
                .await?;
        let resolutions: Vec<(String, i64)> =
            sqlx::query_as(sqlx::AssertSqlSafe(grouped("resolution", "ASC")))
                .bind(of.origin)
                .bind(of.library)
                .bind(of.kind)
                .fetch_all(&mut *tx)
                .await?;
        let ranges: Vec<(String, i64)> =
            sqlx::query_as(sqlx::AssertSqlSafe(grouped("dynamic_range", "ASC")))
                .bind(of.origin)
                .bind(of.library)
                .bind(of.kind)
                .fetch_all(&mut *tx)
                .await?;
        tx.commit().await?;
        let count = |n: i64| Some(u32::try_from(n).unwrap_or(u32::MAX));
        let mut resolutions: Vec<Counted<Resolution>> = resolutions
            .into_iter()
            .filter_map(|(value, n)| {
                Some(Counted {
                    value: Resolution::parse(&value)?,
                    count: count(n),
                })
            })
            .collect();
        resolutions.sort_by_key(|r| r.value);
        let of_range = |name: &str| {
            ranges
                .iter()
                .find(|(value, _)| value == name)
                .map_or(0, |(_, n)| *n)
        };
        let dynamic_ranges = [
            (DynamicRange::Sdr, of_range("sdr")),
            (
                DynamicRange::Hdr,
                of_range("hdr") + of_range("dolby_vision"),
            ),
            (DynamicRange::DolbyVision, of_range("dolby_vision")),
        ]
        .into_iter()
        .map(|(value, n)| Counted {
            value,
            count: count(n),
        })
        .collect();
        Ok(ItemFilterValues {
            genres: genres
                .into_iter()
                .map(|(value, n)| Counted {
                    value,
                    count: count(n),
                })
                .collect(),
            years: years
                .into_iter()
                .filter_map(|(year, n)| {
                    Some(Counted {
                        value: u16::try_from(year).ok()?,
                        count: count(n),
                    })
                })
                .collect(),
            content_ratings: ratings
                .into_iter()
                .map(|(value, n)| Counted {
                    value,
                    count: count(n),
                })
                .collect(),
            resolutions,
            dynamic_ranges,
            ..ItemFilterValues::default()
        })
    }

    /// How many episodes each of `origin`'s `shows` has cached and
    /// available: what a show's watched state is judged by.
    pub async fn cached_episode_counts(
        &self,
        origin: &str,
        shows: &[&str],
    ) -> Result<std::collections::HashMap<String, u32>, StoreError> {
        if shows.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT grandparent, COUNT(*) FROM federation_cache_items \
             WHERE origin = ?1 AND kind = 'episode' AND unavailable = 0 \
             AND grandparent IN (SELECT value FROM json_each(?2)) GROUP BY grandparent",
        )
        .bind(origin)
        .bind(serde_json::to_string(shows).expect("strings serialize"))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(show, n)| (show, u32::try_from(n).unwrap_or(u32::MAX)))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use spoolframe_protocol::federation::RemoteKind;
    use sqlx::Sqlite;

    use super::super::federation_cache::OriginLimits;
    use super::super::federation_cache::tests::{ORIGIN, film, movies, paired, synced};
    use super::super::tests::state_dir;
    use super::*;

    const MOVIES: CachedLibraryOf<'static> = CachedLibraryOf {
        origin: ORIGIN,
        library: "plex:1",
        kind: "movie",
    };

    fn keys(items: &[RemoteItem]) -> Vec<&str> {
        items.iter().map(|i| i.id.as_str()).collect()
    }

    /// Rows cached before migration 0060 get what their bodies already
    /// have: the sort title, length, content rating, a film's release and
    /// the genres; the rest stays NULL until the next sync.
    #[tokio::test]
    async fn the_migration_copies_out_what_the_bodies_have() {
        let dir = state_dir("fed-browse-migration").await;
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(dir.as_ref().join(super::super::DB_FILE))
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = sqlx::pool::PoolOptions::<Sqlite>::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        let mut before = sqlx::migrate!("./migrations");
        before.migrations = before
            .iter()
            .filter(|m| m.version < 60)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        before.run(&pool).await.unwrap();
        let matrix = RemoteItem {
            date: Some("1999-03-31".into()),
            duration_ms: Some(8_160_000),
            content_rating: Some("R".into()),
            genres: vec!["Action".into(), "Science Fiction".into()],
            ..film("1", "The Matrix")
        };
        let show = RemoteItem {
            kind: RemoteKind::Show,
            id: "plex:show:2".into(),
            date: Some("2008-01-20".into()),
            genres: vec!["Drama".into()],
            ..film("2", "A")
        };
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "INSERT INTO federation_peers \
               (id, server_id, endpoint_id, display_name, paired_by, paired_at) \
               VALUES ('p', '{ORIGIN}', 'k', 'Alice''s server', 'bob', 1); \
             INSERT INTO federation_cache_libraries (origin, library, peer, title, kind) \
               VALUES ('{ORIGIN}', 'plex:1', 'p', 'Movies', 'movies');"
        )))
        .execute(&pool)
        .await
        .unwrap();
        for (item, kind) in [(&matrix, "movie"), (&show, "show")] {
            sqlx::query(
                "INSERT INTO federation_cache_items \
                 (origin, item, library, peer, kind, title, body, hash) \
                 VALUES (?1, ?2, 'plex:1', 'p', ?3, ?4, ?5, ?6)",
            )
            .bind(ORIGIN)
            .bind(&item.id)
            .bind(kind)
            .bind(&item.title)
            .bind(serde_json::to_string(item).unwrap())
            .bind(&item.hash)
            .execute(&pool)
            .await
            .unwrap();
        }
        pool.close().await;

        let store = Store::open(dir.as_ref()).await.unwrap();
        type Row = (
            String,
            String,
            Option<String>,
            Option<i64>,
            Option<String>,
            Option<i64>,
        );
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT item, sort_title, released, duration_ms, content_rating, added_at \
             FROM federation_cache_items ORDER BY item",
        )
        .fetch_all(&store.pool)
        .await
        .unwrap();
        let genres: Vec<(String, String)> =
            sqlx::query_as("SELECT item, genre FROM federation_cache_genres ORDER BY item, genre")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        store.pool.close().await;
        assert_eq!(
            rows,
            [
                (
                    "plex:movie:1".to_owned(),
                    "Matrix".to_owned(),
                    Some("1999-03-31".to_owned()),
                    Some(8_160_000),
                    Some("R".to_owned()),
                    None,
                ),
                // "A" alone keeps its article; a show's date is not its
                // release.
                (
                    "plex:show:2".to_owned(),
                    "A".to_owned(),
                    None,
                    None,
                    None,
                    None
                ),
            ]
        );
        assert_eq!(
            genres,
            [
                ("plex:movie:1".to_owned(), "Action".to_owned()),
                ("plex:movie:1".to_owned(), "Science Fiction".to_owned()),
                ("plex:show:2".to_owned(), "Drama".to_owned()),
            ]
        );
    }

    /// A sync writes each item's columns and genres, and the next one
    /// replaces them: a genre the origin dropped no longer keeps it.
    #[tokio::test]
    async fn a_sync_writes_the_columns_and_the_next_replaces_them() {
        let (store, peer) = paired("fed-browse-resync").await;
        movies(&store, &peer).await;
        let first = RemoteItem {
            genres: vec!["Horror".into()],
            size: Some(10),
            ..film("1", "The Thing")
        }
        .hashed();
        synced(&store, &peer, "c1", &[first], &[], true).await;
        let horror = ItemFilter {
            genres: vec!["horror".into()],
            ..ItemFilter::default()
        };
        let (got, total) = store
            .cached_sorted(MOVIES, ItemSort::DEFAULT, &horror, &[], 0, 10)
            .await
            .unwrap();
        assert_eq!((keys(&got), total), (vec!["plex:movie:1"], 1));
        let second = RemoteItem {
            genres: vec!["Science Fiction".into()],
            size: Some(20),
            ..film("1", "The Thing")
        }
        .hashed();
        synced(&store, &peer, "c2", &[second], &[], true).await;
        let (got, total) = store
            .cached_sorted(MOVIES, ItemSort::DEFAULT, &horror, &[], 0, 10)
            .await
            .unwrap();
        assert_eq!((got.len(), total), (0, 0));
        let (size, sort_title): (Option<i64>, String) =
            sqlx::query_as("SELECT size, sort_title FROM federation_cache_items")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!((size, sort_title.as_str()), (Some(20), "Thing"));
    }

    /// 300,000 films: a page by each of four keys, with and without a
    /// filter, at the start and halfway; and the values. Timings are
    /// printed; run with `--ignored --nocapture` in release.
    #[tokio::test]
    #[ignore = "slow: a 300,000-item cache, for the timings"]
    async fn a_large_cache_answers_a_page_quickly() {
        const N: u32 = 300_000;
        let (store, peer) = paired("fed-browse-large").await;
        movies(&store, &peer).await;
        let genres = ["Action", "Comedy", "Drama", "Horror", "Science Fiction"];
        let resolutions = [
            spoolframe_types::Resolution::Uhd,
            spoolframe_types::Resolution::FullHd,
            spoolframe_types::Resolution::Hd,
        ];
        let started = Instant::now();
        store.begin_cache_sync(ORIGIN, "plex:1").await.unwrap();
        let mut page = Vec::with_capacity(1000);
        for n in 0..N {
            let i = usize::try_from(n).unwrap();
            page.push(
                RemoteItem {
                    year: Some(1950 + u16::try_from(n % 75).unwrap()),
                    added_at: Some(i64::from(n.wrapping_mul(2_654_435_761) % 1_000_000)),
                    rating_tenths: (n % 7 != 0).then(|| u16::try_from(n % 100).unwrap()),
                    size: Some(u64::from(n) * 7919 % 100_000_000_000),
                    resolution: Some(resolutions[i % 3]),
                    genres: vec![genres[i % 5].into(), genres[(i / 5) % 5].into()],
                    content_rating: Some(["G", "PG", "PG-13", "R"][i % 4].into()),
                    ..film(
                        &n.to_string(),
                        &format!("Film {}", n.wrapping_mul(40_503) % N),
                    )
                }
                .hashed(),
            );
            if page.len() == 1000 {
                store
                    .stage_cache_page(ORIGIN, "plex:1", &page, &[])
                    .await
                    .unwrap();
                page.clear();
            }
        }
        let key = store
            .federation_peer(&peer)
            .await
            .unwrap()
            .unwrap()
            .endpoint_id;
        store
            .finish_cache_sync(
                ORIGIN,
                (&peer, &key),
                "plex:1",
                "c1",
                true,
                OriginLimits::NONE,
            )
            .await
            .unwrap();
        println!("cached {N} in {:?}", started.elapsed());
        let filtered = ItemFilter {
            genres: vec!["Horror".into()],
            year_from: Some(1980),
            year_to: Some(1999),
            ..ItemFilter::default()
        };
        for key in [
            SortKey::Title,
            SortKey::Added,
            SortKey::Rating,
            SortKey::Size,
        ] {
            for (name, filter) in [
                ("all", ItemFilter::default()),
                ("filtered", filtered.clone()),
            ] {
                for offset in [0, 10_000] {
                    let started = Instant::now();
                    let sort = ItemSort {
                        key,
                        descending: true,
                    };
                    let (got, total) = store
                        .cached_sorted(MOVIES, sort, &filter, &[], offset, 50)
                        .await
                        .unwrap();
                    println!(
                        "{key:?} {name} offset {offset}: {} of {total} in {:?}",
                        got.len(),
                        started.elapsed()
                    );
                }
            }
        }
        let started = Instant::now();
        let values = store.cached_values(MOVIES).await.unwrap();
        println!(
            "values: {} genres, {} years in {:?}",
            values.genres.len(),
            values.years.len(),
            started.elapsed()
        );
    }
}
