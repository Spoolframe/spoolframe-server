//! Both searches' results as one kind ([`SearchResult`]), with the top
//! person listed in People too. An app keeps one screen for both, and
//! narrows either to one server with [`SearchResults::only_from`], which
//! counts again what is left.

use spoolframe_protocol::{SearchGroup, SearchResult, SearchResults, TopResult};
use spoolframe_types::SearchKind;

/// `results`, either kind, as [`SearchResult`]s. A person who is the
/// top match is listed first in People too, where the wire leaves them
/// out as it does a top item: a People row shows everyone it matched,
/// and the counts stay as they were.
pub fn search_results<T: Into<SearchResult>>(
    results: SearchResults<T>,
) -> SearchResults<SearchResult> {
    let mut results = results.map(Into::into);
    list_top_person(&mut results);
    results
}

fn list_top_person<T>(results: &mut SearchResults<T>) {
    let Some(TopResult::Person(person)) = &results.top else {
        return;
    };
    let at = match results
        .groups
        .iter()
        .position(|g| g.kind == SearchKind::People)
    {
        Some(at) => at,
        None => {
            let at = results
                .groups
                .iter()
                // SearchKind is ordered as the groups are shown.
                .position(|g| g.kind > SearchKind::People)
                .unwrap_or(results.groups.len());
            // Only the top person: all of them are here.
            results.groups.insert(
                at,
                SearchGroup {
                    kind: SearchKind::People,
                    items: Vec::new(),
                    people: Vec::new(),
                    more: false,
                    total: Some(1),
                    hidden: 0,
                },
            );
            at
        }
    };
    let people = &mut results.groups[at].people;
    // By id, as the server tells people apart: the counts may differ.
    if !people.iter().any(|p| p.id == person.id) {
        people.insert(0, person.clone());
    }
}

#[cfg(test)]
#[path = "search_result_tests.rs"]
mod tests;
