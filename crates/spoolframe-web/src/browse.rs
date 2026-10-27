//! A library's top level sorted and filtered as clients ask it (`Items`'
//! `sort` and `filter`), as the library page puts it in its address and
//! says it: host-compiled, so `cargo test` covers it. The page is
//! `app::catalog`; the server answers through `catalogRows`' `sort` and
//! `itemFilter`, and `catalogFilterValues` says what a library offers.
//!
//! In the address: `sort=added-desc`, and the item filter in `filter`,
//! where a quick filter's name is otherwise, as parts by `;`, each a
//! name, `:`, and values by `,`:
//! `filter=watched:unwatched;genre:Drama,Science Fiction;year:1990-1999`.

use spoolframe_admin_api::{DynamicRange, ItemFilter, ItemSort, Resolution, SortKey, WatchedState};

/// `sort` in an address: `added-desc`, `title`.
pub fn sort_name(sort: ItemSort) -> String {
    if sort.descending {
        format!("{}-desc", sort.key.as_str())
    } else {
        sort.key.as_str().to_owned()
    }
}

/// [`sort_name`] read back; `None` for none, or one this build does not
/// know.
pub fn sort_from_name(name: &str) -> Option<ItemSort> {
    let (key, descending) = match name.strip_suffix("-desc") {
        Some(key) => (key, true),
        None => (name, false),
    };
    let key = SortKey::parse(key)?;
    (key != SortKey::Unknown).then_some(ItemSort { key, descending })
}

/// The sort select's words for `sort`.
pub fn sort_label(sort: ItemSort) -> &'static str {
    match (sort.key, sort.descending) {
        (SortKey::Title, false) => "Title, A to Z",
        (SortKey::Title, true) => "Title, Z to A",
        (SortKey::Released, true) => "Released, newest first",
        (SortKey::Released, false) => "Released, oldest first",
        (SortKey::Added, true) => "Added, newest first",
        (SortKey::Added, false) => "Added, oldest first",
        (SortKey::LastPlayed, true) => "Played by you, latest first",
        (SortKey::LastPlayed, false) => "Played by you, earliest first",
        (SortKey::Rating, true) => "Critic rating, highest first",
        (SortKey::Rating, false) => "Critic rating, lowest first",
        (SortKey::AudienceRating, true) => "Audience rating, highest first",
        (SortKey::AudienceRating, false) => "Audience rating, lowest first",
        (SortKey::Duration, true) => "Length, longest first",
        (SortKey::Duration, false) => "Length, shortest first",
        (SortKey::Size, true) => "File size, largest first",
        (SortKey::Size, false) => "File size, smallest first",
        (SortKey::Unknown, _) => "Unknown",
    }
}

/// Each of `keys` both ways, the way most people want first: titles A to
/// Z, the rest newest, highest or largest.
pub fn sort_choices(keys: &[SortKey]) -> Vec<ItemSort> {
    keys.iter()
        .filter(|k| **k != SortKey::Unknown)
        .flat_map(|&key| {
            let first = key != SortKey::Title;
            [
                ItemSort {
                    key,
                    descending: first,
                },
                ItemSort {
                    key,
                    descending: !first,
                },
            ]
        })
        .collect()
}

pub const WATCHED: [WatchedState; 3] = [
    WatchedState::Unwatched,
    WatchedState::InProgress,
    WatchedState::Watched,
];

pub fn watched_name(state: WatchedState) -> &'static str {
    match state {
        WatchedState::Unwatched => "unwatched",
        WatchedState::InProgress => "in_progress",
        WatchedState::Watched => "watched",
        WatchedState::Unknown => "unknown",
    }
}

pub fn watched_label(state: WatchedState) -> &'static str {
    match state {
        WatchedState::Unwatched => "Unwatched",
        WatchedState::InProgress => "In progress",
        WatchedState::Watched => "Watched",
        WatchedState::Unknown => "Unknown",
    }
}

const RANGES: [DynamicRange; 3] = [
    DynamicRange::Sdr,
    DynamicRange::Hdr,
    DynamicRange::DolbyVision,
];

pub fn range_name(range: DynamicRange) -> &'static str {
    match range {
        DynamicRange::Sdr => "sdr",
        DynamicRange::Hdr => "hdr",
        DynamicRange::DolbyVision => "dolby_vision",
        DynamicRange::Unknown => "unknown",
    }
}

pub fn range_label(range: DynamicRange) -> &'static str {
    match range {
        DynamicRange::Sdr => "SDR",
        DynamicRange::Hdr => "HDR",
        DynamicRange::DolbyVision => "Dolby Vision",
        DynamicRange::Unknown => "Unknown",
    }
}

pub fn resolution_label(resolution: Resolution) -> &'static str {
    match resolution {
        Resolution::Uhd => "4K",
        Resolution::FullHd => "1080p",
        Resolution::Hd => "720p",
        Resolution::Sd => "SD",
        Resolution::Unknown => "Unknown",
    }
}

/// The characters a value cannot hold as they are: the parts' and
/// values' separators, and what the router reads in a query (it decodes
/// `%` escapes before it splits one, so those would not survive).
fn reserved(c: char) -> bool {
    matches!(c, '~' | ',' | ';' | ':' | '&' | '=' | '%' | '#' | '+')
}

/// `value` with each reserved character as `~` and two hex digits.
fn escaped(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if reserved(c) {
            out.push_str(&format!("~{:02X}", u32::from(c)));
        } else {
            out.push(c);
        }
    }
    out
}

fn unescaped(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '~' {
            out.push(c);
            continue;
        }
        let hex: String = chars.by_ref().take(2).collect();
        match u8::from_str_radix(&hex, 16) {
            Ok(byte) => out.push(char::from(byte)),
            Err(_) => {
                out.push('~');
                out.push_str(&hex);
            }
        }
    }
    out
}

/// The item filter as the address's `filter` holds it.
pub fn filter_param(filter: &ItemFilter) -> String {
    let list =
        |values: &mut dyn Iterator<Item = &str>| values.map(escaped).collect::<Vec<_>>().join(",");
    let mut parts = Vec::new();
    if !filter.watched.is_empty() {
        let names = &mut filter.watched.iter().map(|w| watched_name(*w));
        parts.push(format!("watched:{}", list(names)));
    }
    if !filter.genres.is_empty() {
        let names = &mut filter.genres.iter().map(String::as_str);
        parts.push(format!("genre:{}", list(names)));
    }
    if filter.year_from.is_some() || filter.year_to.is_some() {
        let side = |y: Option<u16>| y.map(|y| y.to_string()).unwrap_or_default();
        parts.push(format!(
            "year:{}-{}",
            side(filter.year_from),
            side(filter.year_to)
        ));
    }
    if !filter.resolutions.is_empty() {
        let names = &mut filter.resolutions.iter().map(|r| r.as_str());
        parts.push(format!("res:{}", list(names)));
    }
    if !filter.dynamic_ranges.is_empty() {
        let names = &mut filter.dynamic_ranges.iter().map(|r| range_name(*r));
        parts.push(format!("range:{}", list(names)));
    }
    if !filter.content_ratings.is_empty() {
        let names = &mut filter.content_ratings.iter().map(String::as_str);
        parts.push(format!("rated:{}", list(names)));
    }
    parts.join(";")
}

/// [`filter_param`] read back; a part or a value it does not know is left
/// out, so a quick filter's name reads as no item filter.
pub fn filter_from(param: &str) -> ItemFilter {
    let mut filter = ItemFilter::default();
    for part in param.split(';') {
        let Some((name, values)) = part.split_once(':') else {
            continue;
        };
        let values: Vec<String> = values
            .split(',')
            .filter(|v| !v.is_empty())
            .map(unescaped)
            .collect();
        let named = |v: &str| values.iter().any(|value| value == v);
        match name {
            "watched" => {
                filter.watched = WATCHED
                    .into_iter()
                    .filter(|w| named(watched_name(*w)))
                    .collect();
            }
            "genre" => filter.genres = values.clone(),
            "year" => {
                if let Some((from, to)) = values.first().and_then(|y| y.split_once('-')) {
                    filter.year_from = from.parse().ok();
                    filter.year_to = to.parse().ok();
                }
            }
            "res" => {
                filter.resolutions = values
                    .iter()
                    .filter_map(|v| Resolution::parse(v))
                    .filter(|r| *r != Resolution::Unknown)
                    .collect();
            }
            "range" => {
                filter.dynamic_ranges = RANGES
                    .into_iter()
                    .filter(|r| named(range_name(*r)))
                    .collect();
            }
            "rated" => filter.content_ratings = values.clone(),
            _ => {}
        }
    }
    filter
}

/// What the item filter keeps, as the banner over the rows says it:
/// "Unwatched · Drama or Comedy · 1990 to 1999".
pub fn filter_text(filter: &ItemFilter) -> String {
    let any = |values: Vec<&str>| values.join(" or ");
    let mut parts = Vec::new();
    if !filter.watched.is_empty() {
        parts.push(any(filter
            .watched
            .iter()
            .map(|w| watched_label(*w))
            .collect()));
    }
    if !filter.genres.is_empty() {
        parts.push(any(filter.genres.iter().map(String::as_str).collect()));
    }
    match (filter.year_from, filter.year_to) {
        (Some(from), Some(to)) if from == to => parts.push(from.to_string()),
        (Some(from), Some(to)) => parts.push(format!("{from} to {to}")),
        (Some(from), None) => parts.push(format!("{from} or later")),
        (None, Some(to)) => parts.push(format!("{to} or earlier")),
        (None, None) => {}
    }
    if !filter.resolutions.is_empty() {
        parts.push(any(filter
            .resolutions
            .iter()
            .map(|r| resolution_label(*r))
            .collect()));
    }
    if !filter.dynamic_ranges.is_empty() {
        parts.push(any(filter
            .dynamic_ranges
            .iter()
            .map(|r| range_label(*r))
            .collect()));
    }
    if !filter.content_ratings.is_empty() {
        let rated = any(filter.content_ratings.iter().map(String::as_str).collect());
        parts.push(format!("rated {rated}"));
    }
    parts.join(" · ")
}

/// `values` with `value` added, or taken out if it is there.
pub fn toggled<T: PartialEq + Clone>(values: &[T], value: &T) -> Vec<T> {
    if values.contains(value) {
        values.iter().filter(|v| *v != value).cloned().collect()
    } else {
        let mut out = values.to_vec();
        out.push(value.clone());
        out
    }
}

/// A year typed into the filter: four digits, or nothing.
pub fn typed_year(text: &str) -> Option<u16> {
    let text = text.trim();
    (text.len() == 4)
        .then(|| text.parse().ok())
        .flatten()
        .filter(|y| (1800..=2200).contains(y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sort_reads_back_from_its_address() {
        for key in SortKey::ALL {
            for descending in [false, true] {
                let sort = ItemSort { key, descending };
                assert_eq!(sort_from_name(&sort_name(sort)), Some(sort));
            }
        }
        assert_eq!(
            sort_name(ItemSort {
                key: SortKey::Added,
                descending: true
            }),
            "added-desc"
        );
        assert_eq!(sort_from_name(""), None);
        assert_eq!(sort_from_name("loudness-desc"), None);
    }

    #[test]
    fn the_select_offers_each_key_both_ways_the_usual_way_first() {
        let choices = sort_choices(&[SortKey::Title, SortKey::Added]);
        let labels: Vec<&str> = choices.into_iter().map(sort_label).collect();
        assert_eq!(
            labels,
            [
                "Title, A to Z",
                "Title, Z to A",
                "Added, newest first",
                "Added, oldest first"
            ]
        );
    }

    #[test]
    fn a_filter_reads_back_from_its_address() {
        let filter = ItemFilter {
            watched: vec![WatchedState::Unwatched, WatchedState::InProgress],
            genres: vec!["Science Fiction".into(), "Action, Adventure".into()],
            year_from: Some(1990),
            year_to: Some(1999),
            resolutions: vec![Resolution::Uhd],
            dynamic_ranges: vec![DynamicRange::Hdr, DynamicRange::DolbyVision],
            content_ratings: vec!["PG-13".into(), "gb/15".into(), "100%;=".into()],
        };
        let param = filter_param(&filter);
        assert_eq!(
            param,
            "watched:unwatched,in_progress;genre:Science Fiction,Action~2C Adventure;\
             year:1990-1999;res:4k;range:hdr,dolby_vision;rated:PG-13,gb/15,100~25~3B~3D"
        );
        assert_eq!(filter_from(&param), filter);
        let open = ItemFilter {
            year_from: Some(2000),
            ..ItemFilter::default()
        };
        assert_eq!(filter_param(&open), "year:2000-");
        assert_eq!(filter_from("year:2000-"), open);
    }

    #[test]
    fn what_a_filter_address_does_not_say_is_left_out() {
        assert!(filter_from("").is_empty());
        assert!(filter_from("missing").is_empty());
        assert!(filter_from("loudness:11;res:8k").is_empty());
    }

    #[test]
    fn the_banner_says_what_is_kept() {
        let filter = ItemFilter {
            watched: vec![WatchedState::Unwatched],
            genres: vec!["Drama".into(), "Comedy".into()],
            year_from: Some(1990),
            year_to: Some(1999),
            content_ratings: vec!["R".into()],
            ..ItemFilter::default()
        };
        assert_eq!(
            filter_text(&filter),
            "Unwatched · Drama or Comedy · 1990 to 1999 · rated R"
        );
    }

    #[test]
    fn a_value_is_toggled_and_a_year_is_four_digits() {
        assert_eq!(toggled(&[1, 2], &2), [1]);
        assert_eq!(toggled(&[1], &2), [1, 2]);
        assert_eq!(typed_year(" 1999 "), Some(1999));
        assert_eq!(typed_year("99"), None);
        assert_eq!(typed_year("abcd"), None);
    }
}
