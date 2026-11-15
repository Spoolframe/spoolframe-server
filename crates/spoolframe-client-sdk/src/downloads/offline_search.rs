//! Search with no server (`Client::search_offline`): what a profile holds
//! on this device, found in its offline database's FTS5 index
//! ([`super::local_db`]) and ranked by the server's rules
//! ([`spoolframe_protocol::search_rank`]), so a search screen shows the
//! same [`SearchResults`] with or without a server. The index only finds
//! the candidates; the top match, the groups, the year and the counts are
//! the shared rules'.
//!
//! Searched: films and episodes downloaded whole, the shows those
//! episodes are of, and the artists, albums and tracks kept here
//! (downloaded or kept from listening, as `Downloads::offline_music` has
//! them). Not searched: people (an item's cast is kept with its download,
//! but a person's items are listed only by the server). Playlists, which a
//! [`SearchResults`] has no group for, are searched on their own
//! (`Client::search_offline_playlists`).

use std::collections::HashMap;

use spoolframe_protocol::search_rank::{self, MEDIA, Matched, Parsed, Ranker, WITHOUT_YEAR};
use spoolframe_protocol::{SearchRequest, SearchResults};
use spoolframe_types::SearchKind;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use super::local_db::{self, Local};
use super::{OfflinePlaylist, Shared};
use crate::SdkError;
use crate::client::Client;

impl Client {
    /// Search what `user_id` of `server` holds on this device, with no
    /// server and no [`crate::Session`]: the same [`SearchResults`] as
    /// [`crate::Session::search`], with its scope, its kind ("See all"),
    /// its limit and a year at the query's end honoured as the server
    /// honours them, and the top match and groups ranked by the same
    /// rules.
    ///
    /// Found: films and episodes downloaded whole, the shows of those
    /// episodes, and the artists, albums and tracks kept here. Each word
    /// typed must start a word of the item's title, or of an episode's
    /// show, a track's album or artist, or an album's artist; case,
    /// accents ("amelie" finds "Amélie", "strasse" "Straße"), punctuation
    /// and a leading "the", "a" or "an" aside. People and
    /// `request.library` are not searched offline; playlists are
    /// [`Client::search_offline_playlists`].
    ///
    /// A profile finds only what it holds here. A locked profile is
    /// [`SdkError::ProfileLocked`]; with no downloads directory nothing is
    /// found.
    pub fn search_offline(
        &self,
        server: &str,
        user_id: &str,
        request: SearchRequest,
    ) -> Result<SearchResults, SdkError> {
        search(&self.inner.downloads, server, user_id, request)
    }
}

impl Client {
    /// The playlists of `user_id` on `server` whose name `query` finds,
    /// with no server: every playlist read while online, those with
    /// nothing here too (their [`OfflinePlaylist::on_device`] is 0: show
    /// them greyed), and those downloaded. Matched as
    /// [`Client::search_offline`] matches titles; best first (the exact
    /// name, then one that starts with the query), then in the server's
    /// order. The playlists' group of an offline search screen: a
    /// [`SearchResults`] has none. A locked profile is
    /// [`SdkError::ProfileLocked`].
    pub fn search_offline_playlists(
        &self,
        server: &str,
        user_id: &str,
        query: &str,
    ) -> Result<Vec<OfflinePlaylist>, SdkError> {
        search_playlists(&self.inner.downloads, server, user_id, query)
    }
}

/// [`Client::search_offline_playlists`] over `shared`'s store.
pub(super) fn search_playlists(
    shared: &Shared,
    server: &str,
    user_id: &str,
    query: &str,
) -> Result<Vec<OfflinePlaylist>, SdkError> {
    if shared.locks.is_locked(server, user_id) {
        return Err(SdkError::ProfileLocked);
    }
    let words = words(query);
    if words.is_empty() {
        return Ok(Vec::new());
    }
    let text = folded(query);
    let ranker = Ranker::new(folded);
    let matched = fts_query(&words);
    let kept: Vec<String> = shared
        .dbs
        .as_ref()
        .and_then(|dbs| {
            let (s, u) = (server.to_owned(), user_id.to_owned());
            dbs.call(move |local: &mut Local| {
                let conn = local.db(&s, &u)?;
                local_db::find_playlists(conn, &matched)
                    .map_err(|e| tracing::warn!(error = %e, "offline database: no playlists found"))
                    .ok()
            })
            .flatten()
        })
        .unwrap_or_default();
    let mut found: Vec<(u8, OfflinePlaylist)> = shared
        .offline_playlists_of(server, user_id, kept.clone())
        .into_iter()
        .filter(|p| match &p.summary {
            Some(summary) => kept.contains(&summary.id),
            // Downloaded, not kept: found by its name here.
            None => finds(&words, std::slice::from_ref(&p.name)),
        })
        .map(|p| (ranker.tier_of(&p.name, &text), p))
        .collect();
    // Stable: within a tier, the server's order.
    found.sort_by_key(|(tier, _)| *tier);
    Ok(found.into_iter().map(|(_, p)| p).collect())
}

/// [`Client::search_offline`] over `shared`'s store.
pub(super) fn search(
    shared: &Shared,
    server: &str,
    user_id: &str,
    request: SearchRequest,
) -> Result<SearchResults, SdkError> {
    if shared.locks.is_locked(server, user_id) {
        return Err(SdkError::ProfileLocked);
    }
    let Parsed { text, year } = search_rank::parse(&request.query, search_rank::this_year());
    let words = words(&text);
    if words.is_empty() {
        return Ok(SearchResults::default());
    }
    let kinds: Vec<SearchKind> = match request.kind {
        Some(kind) => vec![kind],
        None => request.scope.kinds().to_vec(),
    };
    // One more than shown, as the server asks its catalog: the top match
    // is taken out of its group.
    let limit = usize::try_from(request.limit.saturating_add(1)).unwrap_or(usize::MAX);
    let Some(dbs) = &shared.dbs else {
        return Ok(SearchResults::default());
    };
    let (s, u, matched, asked, ranked) = (
        server.to_owned(),
        user_id.to_owned(),
        fts_query(&words),
        kinds.clone(),
        folded(&text),
    );
    let only_kind = request.kind.is_some();
    let answer = dbs
        .call(move |local: &mut Local| {
            let conn = local.db(&s, &u)?;
            // Each kind's count in one query, then a page of each kind
            // asked.
            let find = |kinds: &mut dyn Iterator<Item = SearchKind>, year, limit| {
                let counted = local_db::counts(conn, &matched, year)?;
                kinds
                    .map(|kind| {
                        Ok(Matched {
                            kind,
                            items: local_db::find(conn, kind, &matched, &ranked, year, limit)?,
                            people: Vec::new(),
                            total: counted.get(&kind).copied().unwrap_or(0),
                        })
                    })
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map(|found| (found, counted))
            };
            let run = || -> rusqlite::Result<Found> {
                let (found, counted) = find(&mut asked.iter().copied(), year, limit)?;
                let mut totals: HashMap<SearchKind, u32> =
                    found.iter().map(|g| (g.kind, g.total)).collect();
                if !only_kind && totals.values().all(|&t| t == 0) {
                    // Nothing in this scope: the empty state offers the others.
                    for kind in MEDIA.into_iter().filter(|k| !asked.contains(k)) {
                        totals.insert(kind, counted.get(&kind).copied().unwrap_or(0));
                    }
                }
                let any_year = match year {
                    Some(_) => {
                        let mut kinds = asked.iter().copied().filter(|&k| k != SearchKind::People);
                        find(&mut kinds, None, 2 * WITHOUT_YEAR)?.0
                    }
                    None => Vec::new(),
                };
                Ok((found, totals, any_year))
            };
            run()
                .map_err(|e| tracing::warn!(error = %e, "offline database: nothing found"))
                .ok()
        })
        .flatten();
    let Some((found, totals, any_year)) = answer else {
        return Ok(SearchResults::default());
    };
    let ranker = Ranker::new(folded);
    let also = match year {
        Some(year) => ranker.without_year(&text, year, &found, any_year),
        None => Vec::new(),
    };
    let query = Parsed { text, year };
    Ok(ranker.group(&request, &query, found, &totals, also, &HashMap::new()))
}

/// What the index found: each kind asked, every kind's count, and with a
/// year the same kinds in any year.
type Found = (Vec<Matched>, HashMap<SearchKind, u32>, Vec<Matched>);

/// The FTS5 query for `words`: each a prefix of a word, all of them.
fn fts_query(words: &[String]) -> String {
    words
        .iter()
        .map(|w| format!("\"{w}\"*"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Every word of `words` starts a word of one of `text`.
fn finds(words: &[String], text: &[String]) -> bool {
    let candidate: Vec<String> = text
        .iter()
        .flat_map(|t| fold(t).split(' ').map(str::to_owned).collect::<Vec<_>>())
        .collect();
    words
        .iter()
        .all(|w| candidate.iter().any(|c| c.starts_with(w.as_str())))
}

/// The words of a query, folded, without a leading article.
fn words(query: &str) -> Vec<String> {
    folded(query)
        .split(' ')
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `s` as offline search compares it: lower case, accents taken off
/// (Unicode NFKD, combining marks dropped), the letters NFKD leaves whole
/// spelled as typed on a plain keyboard ("ß" as "ss", "æ" as "ae", "ø" as
/// "o"), punctuation as space, single spaces.
pub(super) fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.nfkd().filter(|&c| !is_combining_mark(c)) {
        let c = c.to_lowercase().next().unwrap_or(c);
        match c {
            'ß' => out.push_str("ss"),
            'æ' => out.push_str("ae"),
            'œ' => out.push_str("oe"),
            'ø' => out.push('o'),
            'đ' => out.push('d'),
            'ł' => out.push('l'),
            'þ' => out.push_str("th"),
            c if c.is_alphanumeric() => out.extend(c.to_lowercase()),
            _ => out.push(' '),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// [`fold`], without a leading "the", "a" or "an": how titles are ranked
/// offline, as the server ranks them [`search_rank::normalized`].
pub(super) fn folded(s: &str) -> String {
    search_rank::strip_article(&fold(s)).to_owned()
}

#[cfg(test)]
#[path = "offline_search_tests.rs"]
mod tests;
