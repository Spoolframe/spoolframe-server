//! Sorting and filtering a section, by the
//! PMS itself: its own `sort` and filter parameters on
//! `/library/sections/{key}/all`, so a listing is as live as Plex is and
//! nothing of the section is copied.
//!
//! What a section can be sorted and filtered by is what the PMS lists for
//! it (`/library/sections/{key}/sorts` and `/filters`): a PMS ignores a
//! filter it does not know and answers every item, so one not listed is
//! never sent. No PMS sorts by file size. The user's watch state is the
//! server's: it walks the order asked here ([`PlexCatalog::walk`]) and
//! applies it.
//!
//! What a PMS's parameters mean, as checked against one (1.42):
//!
//! - `year>>=1990` is "after 1990" and `year<<=1999` "before 1999": both
//!   strict, so a range from `from` to `to` is sent as `from - 1` and
//!   `to + 1`.
//! - Values of one filter joined by commas are any of them
//!   (`genre=138,139`); the same filter twice is both.
//! - `push=1`, `or=1` and `pop=1` group filters: `push=1&hdr=1&or=1&dovi=1&pop=1`
//!   is HDR or Dolby Vision.
//! - An ascending sort lists the items with no value first, a descending
//!   one last. Without a value comes last both ways here, so an ascending
//!   listing is read as those with one (`rating>>=-1`) and then the rest.
//! - `/library/metadata/1,2,3` answers those items, in any order.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use spoolframe_catalog::{
    Browsing, CatalogError, Facts, ItemPage, ItemQuery, Listed, Page, WatchParts,
};
use spoolframe_types::{
    Counted, DynamicRange, FilterPart, Item, ItemFilterValues, Library, LibraryId, LibraryKind,
    MediaId, Resolution, SortKey,
};
use std::sync::Mutex;

use crate::wire::{Directory, Metadata, SectionKey};
use crate::{Call, PlexCatalog, convert, plex_key};

/// How long what a section sorts and filters by is kept: it changes only
/// with the PMS's version.
const KNOWN_FOR: Duration = Duration::from_secs(600);

/// Items per request of a walk.
const WALK_PAGE: u32 = 1_000;

/// Items per request of [`PlexCatalog::by_id`].
pub(crate) const BY_ID: usize = 100;

/// What a walk leaves out of each item: it reads only the key, the kind
/// and a show's episode count, and these are most of each item's bytes.
const WALK_EXCLUDE_ELEMENTS: &str = "Media,Genre,Country,Director,Role,Writer,Producer,Guid,Rating,Collection,Label,Image,UltraBlurColors";
const WALK_EXCLUDE_FIELDS: &str = "summary,tagline,thumb,art,theme,studio";

/// The earliest release date sent as "has one": nothing in a library
/// came out before it.
const EARLIEST: &str = "1800-01-01";

/// One query parameter, as sent.
type Param = (String, String);

/// One query parameter this module always sends the same.
type Fixed = (&'static str, &'static str);

/// What each section sorts and filters by, as the PMS last said.
#[derive(Default)]
pub(crate) struct Sorting {
    sections: Mutex<HashMap<String, (Instant, Arc<Section>)>>,
}

impl Sorting {
    /// The sections known. A lock poisoned by a panic elsewhere still
    /// holds a whole map: each entry is inserted at once.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, (Instant, Arc<Section>)>> {
        self.sections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl std::fmt::Debug for Sorting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sorting")
            .field("sections", &self.lock().len())
            .finish()
    }
}

/// One section's sort fields and filter fields, by Plex's names.
#[derive(Debug, Default)]
struct Section {
    sorts: HashSet<String>,
    filters: HashSet<String>,
}

/// The Plex field a key sorts by in a section that lists `sorts`; `None`
/// when the section cannot be sorted by it. A show is added when its
/// latest episode is: Plex sorts shows by that, not by the show's own.
fn sort_field(key: SortKey, sorts: &HashSet<String>) -> Option<&'static str> {
    let wanted: &[&'static str] = match key {
        SortKey::Title => &["titleSort"],
        SortKey::Released => &["originallyAvailableAt"],
        SortKey::Added => &["addedAt", "episode.addedAt"],
        SortKey::Rating => &["rating"],
        SortKey::AudienceRating => &["audienceRating"],
        SortKey::Duration => &["duration"],
        SortKey::LastPlayed | SortKey::Size | SortKey::Unknown => &[],
    };
    wanted.iter().copied().find(|f| sorts.contains(*f))
}

/// The filter that keeps only the items that have a value for `field`,
/// for the ones Plex lists first when ascending.
fn has_value(field: &str) -> Option<Fixed> {
    match field {
        "originallyAvailableAt" => Some(("originallyAvailableAt>>", EARLIEST)),
        "rating" => Some(("rating>>", "-1")),
        "audienceRating" => Some(("audienceRating>>", "-1")),
        "duration" => Some(("duration>>", "-1")),
        _ => None,
    }
}

/// What the server applies itself, for a library of `kind`: the user's
/// watch state, which only movies and shows have.
fn watched(kind: LibraryKind) -> bool {
    matches!(kind, LibraryKind::Movies | LibraryKind::Shows)
}

impl Section {
    fn browsing(&self, kind: LibraryKind) -> Browsing {
        let sorts = SortKey::ALL
            .iter()
            .copied()
            .filter(|&key| match key {
                SortKey::LastPlayed => watched(kind),
                key => sort_field(key, &self.sorts).is_some(),
            })
            .collect();
        let has = |field: &str| self.filters.contains(field);
        let mut filters = Vec::new();
        if watched(kind) {
            filters.push(FilterPart::Watched);
        }
        for (part, field) in [
            (FilterPart::Genres, "genre"),
            (FilterPart::Years, "year"),
            (FilterPart::ContentRatings, "contentRating"),
            (FilterPart::Resolutions, "resolution"),
        ] {
            if has(field) {
                filters.push(part);
            }
        }
        let mut dynamic_ranges = Vec::new();
        if has("hdr") {
            dynamic_ranges.extend([DynamicRange::Sdr, DynamicRange::Hdr]);
        }
        if has("dovi") {
            dynamic_ranges.push(DynamicRange::DolbyVision);
        }
        if !dynamic_ranges.is_empty() {
            filters.push(FilterPart::DynamicRanges);
        }
        Browsing {
            sorts,
            filters,
            dynamic_ranges,
            watch: WatchParts::ByServer,
        }
    }
}

/// What `m` says of itself to sort and filter by: a listing's item, or one
/// asked for by key, which alone has its streams, so its dynamic range,
/// and every genre (a listing has the first few).
pub(crate) fn facts(m: &Metadata) -> Facts {
    let media = m.media.first();
    let part = media.and_then(|media| media.parts.first());
    let video = part.and_then(|p| p.streams.iter().find(|s| s.stream_type == Some(1)));
    Facts {
        sort_title: m.title_sort.clone().filter(|t| !t.is_empty()),
        released: m.originally_available_at.map(|d| d.to_string()),
        added_at: m.added_at,
        rating_tenths: m.rating.and_then(Facts::tenths),
        audience_rating_tenths: m.audience_rating.and_then(Facts::tenths),
        size: part.and_then(|p| p.size),
        // Plex's own class first: what this server's filter goes by.
        resolution: media.and_then(|media| {
            media
                .video_resolution
                .as_deref()
                .and_then(Resolution::of_plex)
                .or_else(|| Resolution::of(media.width, media.height))
        }),
        dynamic_range: video.map(|v| {
            if v.dovi_present == Some(true) {
                DynamicRange::DolbyVision
            } else if matches!(v.color_trc.as_deref(), Some("smpte2084" | "arib-std-b67")) {
                DynamicRange::Hdr
            } else {
                DynamicRange::Sdr
            }
        }),
        genres: m.genres.iter().map(|g| g.tag.clone()).collect(),
    }
}

/// A filter's parameters, or that it keeps nothing: a genre or a rating
/// the section does not have.
enum Params {
    Some(Vec<(String, String)>),
    Nothing,
}

/// The dynamic ranges in `asked` as Plex filters, given whether the
/// section filters by Dolby Vision. SDR is neither HDR nor Dolby Vision;
/// HDR is either.
fn ranges(asked: &[DynamicRange], dovi: bool) -> Vec<(String, String)> {
    let pair = |k: &str, v: &str| (k.to_owned(), v.to_owned());
    let sdr = asked.contains(&DynamicRange::Sdr);
    let hdr = asked.contains(&DynamicRange::Hdr);
    let dv = asked.contains(&DynamicRange::DolbyVision);
    let hdr_or_dv = || {
        if dovi {
            vec![
                pair("push", "1"),
                pair("hdr", "1"),
                pair("or", "1"),
                pair("dovi", "1"),
                pair("pop", "1"),
            ]
        } else {
            vec![pair("hdr", "1")]
        }
    };
    match (sdr, hdr, dv) {
        // Every item, as far as its range goes.
        (true, true, _) => Vec::new(),
        (false, false, false) => Vec::new(),
        (true, false, false) if dovi => vec![pair("hdr", "0"), pair("dovi", "0")],
        (true, false, false) => vec![pair("hdr", "0")],
        // SDR or Dolby Vision: not HDR10 without Dolby Vision.
        (true, false, true) => vec![
            pair("push", "1"),
            pair("hdr", "0"),
            pair("or", "1"),
            pair("dovi", "1"),
            pair("pop", "1"),
        ],
        (false, true, _) => hdr_or_dv(),
        (false, false, true) => vec![pair("dovi", "1")],
    }
}

/// Plex's `videoResolution` values of `class`, of those the section has
/// (`present`): SD is every one below 720.
fn resolution_values(class: Resolution, present: &[String]) -> Vec<String> {
    present
        .iter()
        .filter(|v| Resolution::of_plex(v) == Some(class))
        .cloned()
        .collect()
}

/// A directory entry's key as a filter value: Plex hands them out
/// percent-encoded (`16%2B`).
fn decoded(key: &str) -> String {
    url::form_urlencoded::parse(key.as_bytes())
        .next()
        .map(|(k, _)| k.into_owned())
        .unwrap_or_default()
}

impl PlexCatalog {
    /// The section `library` names, if served.
    fn browsed_section(&self, library: &LibraryId) -> Result<String, CatalogError> {
        let key = plex_key(library.namespace(), library.key(), library)?;
        if !self.serves(Some(&SectionKey(key.to_owned()))) {
            return Err(CatalogError::NotServed(library.to_string()));
        }
        Ok(key.to_owned())
    }

    /// One of a section's directories (`genre`, `year`, `sorts`): its
    /// entries.
    async fn directory(&self, key: &str, which: &str) -> Result<Vec<Directory>, CatalogError> {
        let call = Call::get(format!("/library/sections/{key}/{which}"))
            .about(format!("library section {key}"));
        Ok(self.fetch(&call).await?.directories)
    }

    /// What section `key` sorts and filters by, as the PMS lists it.
    async fn section_sorting(&self, key: &str) -> Result<Arc<Section>, CatalogError> {
        if let Some((at, known)) = self.sorting.lock().get(key)
            && at.elapsed() < KNOWN_FOR
        {
            return Ok(known.clone());
        }
        let (sorts, filters) = futures_util::future::try_join(
            self.directory(key, "sorts"),
            self.directory(key, "filters"),
        )
        .await?;
        let section = Arc::new(Section {
            sorts: sorts.into_iter().map(|d| d.key).collect(),
            filters: filters.into_iter().filter_map(|d| d.filter).collect(),
        });
        self.sorting
            .lock()
            .insert(key.to_owned(), (Instant::now(), section.clone()));
        Ok(section)
    }

    pub(crate) async fn browsing_of(&self, library: &Library) -> Result<Browsing, CatalogError> {
        let key = self.browsed_section(&library.id)?;
        Ok(self.section_sorting(&key).await?.browsing(library.kind))
    }

    /// `names`, matched to the section's `which` directory by title, case
    /// aside, as the values Plex filters `which` by.
    async fn matched(
        &self,
        key: &str,
        which: &str,
        names: &[String],
    ) -> Result<Vec<String>, CatalogError> {
        let entries = self.directory(key, which).await?;
        Ok(entries
            .into_iter()
            .filter(|d| names.iter().any(|n| n.eq_ignore_ascii_case(d.title.trim())))
            .map(|d| decoded(&d.key))
            .collect())
    }

    /// `query`'s filter as Plex's parameters, checked against what the
    /// section lists.
    async fn params(
        &self,
        key: &str,
        section: &Section,
        query: &ItemQuery,
    ) -> Result<Params, CatalogError> {
        let filter = &query.filter;
        let has = |field: &str| section.filters.contains(field);
        let refuse = |what: &str| {
            Err(CatalogError::CannotBrowse(format!(
                "this library cannot be filtered by {what}"
            )))
        };
        if !filter.watched.is_empty() {
            return refuse("watch state here");
        }
        let mut params = Vec::new();
        for (names, field, part) in [
            (&filter.genres, "genre", "genres"),
            (&filter.content_ratings, "contentRating", "content ratings"),
        ] {
            if names.is_empty() {
                continue;
            }
            if !has(field) {
                return refuse(part);
            }
            let values = self.matched(key, field, names).await?;
            if values.is_empty() {
                return Ok(Params::Nothing);
            }
            params.push((field.to_owned(), values.join(",")));
        }
        if filter.year_from.is_some() || filter.year_to.is_some() {
            if !has("year") {
                return refuse("years");
            }
            if let Some(from) = filter.year_from.filter(|&f| f > 0) {
                params.push(("year>>".into(), (u32::from(from) - 1).to_string()));
            }
            if let Some(to) = filter.year_to.filter(|&t| t < u16::MAX) {
                params.push(("year<<".into(), (u32::from(to) + 1).to_string()));
            }
        }
        if !filter.resolutions.is_empty() {
            if !has("resolution") {
                return refuse("resolution");
            }
            let present: Vec<String> = self
                .directory(key, "resolution")
                .await?
                .into_iter()
                .map(|d| decoded(&d.key))
                .collect();
            let values: Vec<String> = filter
                .resolutions
                .iter()
                .flat_map(|&class| resolution_values(class, &present))
                .collect();
            if values.is_empty() {
                return Ok(Params::Nothing);
            }
            params.push(("resolution".into(), values.join(",")));
        }
        if !filter.dynamic_ranges.is_empty() {
            let dv = filter.dynamic_ranges.contains(&DynamicRange::DolbyVision);
            let sdr_or_hdr = filter
                .dynamic_ranges
                .iter()
                .any(|r| matches!(r, DynamicRange::Sdr | DynamicRange::Hdr));
            if (dv && !has("dovi")) || (sdr_or_hdr && !has("hdr")) {
                return refuse("this range");
            }
            params.extend(ranges(&filter.dynamic_ranges, has("dovi")));
        }
        Ok(Params::Some(params))
    }

    /// The sort parameter for `query`, and the filter that keeps the
    /// items with a value for it when Plex would list those without one
    /// first.
    fn sort_param(
        section: &Section,
        query: &ItemQuery,
    ) -> Result<(String, Option<Fixed>), CatalogError> {
        let field = sort_field(query.sort.key, &section.sorts).ok_or_else(|| {
            CatalogError::CannotBrowse(format!(
                "this library cannot be sorted by {}",
                query.sort.key.as_str()
            ))
        })?;
        let direction = if query.sort.descending { ":desc" } else { "" };
        let sort = if field == "titleSort" {
            format!("{field}{direction}")
        } else {
            // Ties by sort title, A to Z both ways.
            format!("{field}{direction},titleSort")
        };
        let valued = if query.sort.descending {
            None
        } else {
            has_value(field)
        };
        Ok((sort, valued))
    }

    /// One request of section `key`'s listing with `params`, from `start`,
    /// at most `size` items: its items as Plex sent them, and how many the
    /// listing has.
    async fn sorted_listing(
        &self,
        library: &LibraryId,
        key: &str,
        params: &[(String, String)],
        start: u32,
        size: u32,
        slim: bool,
    ) -> Result<(Call<'static>, Vec<Metadata>, u32), CatalogError> {
        // What errors name: the path, never the parameters.
        let named = Call::get(format!("/library/sections/{key}/all")).about(library);
        let mut call = Call::get(named.path.clone()).about(library);
        call.query = params
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        if slim {
            call.query.push(("excludeElements", WALK_EXCLUDE_ELEMENTS));
            call.query.push(("excludeFields", WALK_EXCLUDE_FIELDS));
        }
        call.headers = vec![
            ("X-Plex-Container-Start", start.to_string()),
            ("X-Plex-Container-Size", size.to_string()),
        ];
        let container = self.fetch(&call).await?;
        let total = container
            .total_size
            .ok_or_else(|| self.backend(&named, None, "listing has no totalSize".into()))?;
        Ok((named, container.metadata, total))
    }

    /// How many items of section `key` pass `params`.
    async fn count(
        &self,
        library: &LibraryId,
        key: &str,
        params: &[(String, String)],
    ) -> Result<u32, CatalogError> {
        Ok(self
            .sorted_listing(library, key, params, 0, 0, false)
            .await?
            .2)
    }

    /// The parameters of `query` for section `key`, the sort's first, and
    /// the filter that keeps those with a value; `None` when it keeps
    /// nothing.
    async fn asked(
        &self,
        key: &str,
        query: &ItemQuery,
    ) -> Result<Option<(Vec<Param>, Option<Param>)>, CatalogError> {
        let section = self.section_sorting(key).await?;
        let (sort, valued) = Self::sort_param(&section, query)?;
        let Params::Some(filters) = self.params(key, &section, query).await? else {
            return Ok(None);
        };
        let mut params = vec![("sort".to_owned(), sort)];
        params.extend(filters);
        Ok(Some((
            params,
            valued.map(|(k, v)| (k.to_owned(), v.to_owned())),
        )))
    }

    pub(crate) async fn query(
        &self,
        library: &LibraryId,
        query: &ItemQuery,
        page: Page,
    ) -> Result<ItemPage, CatalogError> {
        let key = self.browsed_section(library)?;
        let Some((params, valued)) = self.asked(&key, query).await? else {
            return Ok(ItemPage {
                items: Vec::new(),
                total: 0,
            });
        };
        let Some(valued) = valued else {
            let (call, metadata, total) = self
                .sorted_listing(library, &key, &params, page.offset, page.limit, false)
                .await?;
            return Ok(ItemPage {
                items: self.items_of(&call, &metadata)?,
                total,
            });
        };
        // Those with a value, in order, then those without: the first
        // `total - with` of the plain listing.
        let mut with_value = params.clone();
        with_value.push(valued);
        let (total, with) = futures_util::future::try_join(
            self.count(library, &key, &params),
            self.count(library, &key, &with_value),
        )
        .await?;
        // The page is [offset, end) of: those with a value, [0, with), then
        // those without, [with, total), which the plain listing has at
        // [0, total - with).
        let with = with.min(total);
        let end = page.offset.saturating_add(page.limit).min(total);
        let mut items = Vec::new();
        if page.offset < end.min(with) {
            let size = end.min(with) - page.offset;
            let (call, metadata, _) = self
                .sorted_listing(library, &key, &with_value, page.offset, size, false)
                .await?;
            items.extend(self.items_of(&call, &metadata)?);
        }
        let from = page.offset.max(with);
        if from < end {
            let (call, metadata, _) = self
                .sorted_listing(library, &key, &params, from - with, end - from, false)
                .await?;
            items.extend(self.items_of(&call, &metadata)?);
        }
        Ok(ItemPage { items, total })
    }

    /// Every item `query` keeps, in its order, as ids: read
    /// [`WALK_PAGE`] at a time, each item without the parts the walk does
    /// not read.
    pub(crate) async fn walk(
        &self,
        library: &LibraryId,
        query: &ItemQuery,
    ) -> Result<Vec<Listed>, CatalogError> {
        let key = self.browsed_section(library)?;
        let Some((params, valued)) = self.asked(&key, query).await? else {
            return Ok(Vec::new());
        };
        let mut walked: Vec<Listed> = Vec::new();
        loop {
            let start = u32::try_from(walked.len()).unwrap_or(u32::MAX);
            let (call, metadata, total) = self
                .sorted_listing(library, &key, &params, start, WALK_PAGE, true)
                .await?;
            let got = metadata.len();
            for m in &metadata {
                let Some(kind) = convert::media_kind(&m.kind) else {
                    continue;
                };
                let id =
                    convert::media_id(kind, &m.rating_key).map_err(|e| self.converted(&call, e))?;
                walked.push(Listed {
                    id,
                    leaves: m.leaf_count,
                });
            }
            if got == 0 || start as usize + got >= total as usize {
                break;
            }
        }
        if let Some(valued) = valued {
            // Plex listed those without a value first: they go last.
            let mut with_value = params.clone();
            with_value.push(valued);
            let with = self.count(library, &key, &with_value).await? as usize;
            let without = walked.len().saturating_sub(with);
            walked.rotate_left(without);
        }
        Ok(walked)
    }

    /// The items `ids` name, in that order, [`BY_ID`] a request; one gone,
    /// or in a section not served, is left out.
    pub(crate) async fn by_id(&self, ids: &[MediaId]) -> Result<Vec<Item>, CatalogError> {
        let mut found: HashMap<MediaId, Item> = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(BY_ID) {
            let keys: Vec<&str> = chunk
                .iter()
                .filter_map(|id| plex_key(id.namespace(), id.key(), id).ok())
                .collect();
            if keys.is_empty() {
                continue;
            }
            let call = Call::get(format!("/library/metadata/{}", keys.join(",")));
            let container = match self.fetch(&call).await {
                Ok(container) => container,
                Err(CatalogError::NotFound(_)) => continue,
                Err(e) => return Err(e),
            };
            let section = container.library_section_id.clone();
            for m in &container.metadata {
                if !self.serves(m.library_section_id.as_ref().or(section.as_ref())) {
                    continue;
                }
                let item = convert::item(m).map_err(|e| self.converted(&call, e))?;
                found.insert(item.id().clone(), item);
            }
        }
        Ok(ids.iter().filter_map(|id| found.remove(id)).collect())
    }

    /// What `library`'s items have to filter by: genres, years and
    /// content ratings as the PMS lists them (it does not count them),
    /// and resolutions and ranges counted, a request each.
    pub(crate) async fn values(&self, library: &Library) -> Result<ItemFilterValues, CatalogError> {
        let key = self.browsed_section(&library.id)?;
        let browsing = self.section_sorting(&key).await?.browsing(library.kind);
        let mut values = browsing.values();
        let offers = |part| browsing.filters.contains(&part);
        let titles = |entries: Vec<Directory>| -> Vec<String> {
            let mut titles: Vec<String> = entries.into_iter().map(|d| d.title).collect();
            titles.sort_by_key(|t| t.to_lowercase());
            titles.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
            titles
        };
        let uncounted = |value| Counted { value, count: None };
        if offers(FilterPart::Genres) {
            values.genres = titles(self.directory(&key, "genre").await?)
                .into_iter()
                .map(uncounted)
                .collect();
        }
        if offers(FilterPart::ContentRatings) {
            values.content_ratings = titles(self.directory(&key, "contentRating").await?)
                .into_iter()
                .map(uncounted)
                .collect();
        }
        if offers(FilterPart::Years) {
            let mut years: Vec<u16> = self
                .directory(&key, "year")
                .await?
                .iter()
                .filter_map(|d| decoded(&d.key).parse().ok())
                .collect();
            years.sort_unstable_by(|a, b| b.cmp(a));
            years.dedup();
            values.years = years
                .into_iter()
                .map(|value| Counted { value, count: None })
                .collect();
        }
        if offers(FilterPart::Resolutions) {
            let present: Vec<String> = self
                .directory(&key, "resolution")
                .await?
                .into_iter()
                .map(|d| decoded(&d.key))
                .collect();
            for class in [
                Resolution::Uhd,
                Resolution::FullHd,
                Resolution::Hd,
                Resolution::Sd,
            ] {
                let plex = resolution_values(class, &present);
                if plex.is_empty() {
                    continue;
                }
                let count = self
                    .count(&library.id, &key, &[("resolution".into(), plex.join(","))])
                    .await?;
                if count > 0 {
                    values.resolutions.push(Counted {
                        value: class,
                        count: Some(count),
                    });
                }
            }
        }
        let dovi = browsing.dynamic_ranges.contains(&DynamicRange::DolbyVision);
        let mut ranges_counted = Vec::new();
        for range in &browsing.dynamic_ranges {
            let count = self
                .count(&library.id, &key, &ranges(&[*range], dovi))
                .await?;
            ranges_counted.push(Counted {
                value: *range,
                count: Some(count),
            });
        }
        values.dynamic_ranges = ranges_counted;
        Ok(values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    /// A film's record as the PMS sends it, with one video stream.
    fn film(video: serde_json::Value, media: serde_json::Value) -> Metadata {
        let mut media = media;
        media["Part"] = serde_json::json!([{"id": 1, "size": 1234, "Stream": [video]}]);
        serde_json::from_value(serde_json::json!({
            "ratingKey": "1",
            "type": "movie",
            "title": "Alien",
            "titleSort": "Alien (1979)",
            "originallyAvailableAt": "1979-05-25",
            "addedAt": 1_700_000_000,
            "rating": 9.8,
            "audienceRating": 9.4,
            "Genre": [{"tag": "Horror"}, {"tag": "Science Fiction"}],
            "Media": [media],
        }))
        .unwrap()
    }

    #[test]
    fn a_film_s_facts_are_its_record_s() {
        let facts = super::facts(&film(
            serde_json::json!({"streamType": 1, "colorTrc": "bt709"}),
            serde_json::json!({"videoResolution": "1080"}),
        ));
        assert_eq!(
            facts,
            Facts {
                sort_title: Some("Alien (1979)".into()),
                released: Some("1979-05-25".into()),
                added_at: Some(1_700_000_000),
                rating_tenths: Some(98),
                audience_rating_tenths: Some(94),
                size: Some(1234),
                resolution: Some(Resolution::FullHd),
                dynamic_range: Some(DynamicRange::Sdr),
                genres: vec!["Horror".into(), "Science Fiction".into()],
            }
        );
    }

    /// Dolby Vision by its flag, over the transfer that says HDR; HDR10
    /// and HLG by their transfers; the class by the size where Plex has
    /// none.
    #[test]
    fn the_range_is_the_video_stream_s() {
        let range = |video: serde_json::Value| {
            super::facts(&film(video, serde_json::json!({}))).dynamic_range
        };
        assert_eq!(
            range(
                serde_json::json!({"streamType": 1, "DOVIPresent": true, "colorTrc": "smpte2084"})
            ),
            Some(DynamicRange::DolbyVision)
        );
        assert_eq!(
            range(serde_json::json!({"streamType": 1, "colorTrc": "smpte2084"})),
            Some(DynamicRange::Hdr)
        );
        assert_eq!(
            range(serde_json::json!({"streamType": 1, "colorTrc": "arib-std-b67"})),
            Some(DynamicRange::Hdr)
        );
        assert_eq!(range(serde_json::json!({"streamType": 2})), None);
        let sized = super::facts(&film(
            serde_json::json!({"streamType": 1}),
            serde_json::json!({"width": 3840, "height": 1600}),
        ));
        assert_eq!(sized.resolution, Some(Resolution::Uhd));
    }

    #[test]
    fn a_show_is_added_by_its_latest_episode_where_plex_sorts_so() {
        assert_eq!(
            sort_field(SortKey::Added, &set(&["titleSort", "episode.addedAt"])),
            Some("episode.addedAt")
        );
        assert_eq!(
            sort_field(SortKey::Added, &set(&["addedAt", "episode.addedAt"])),
            Some("addedAt")
        );
        assert_eq!(sort_field(SortKey::Size, &set(&["titleSort"])), None);
    }

    #[test]
    fn a_section_offers_what_its_pms_lists_and_the_watch_state_for_video() {
        let movies = Section {
            sorts: set(&["titleSort", "addedAt", "rating", "duration"]),
            filters: set(&["genre", "year", "hdr"]),
        };
        let browsing = movies.browsing(LibraryKind::Movies);
        assert_eq!(
            browsing.sorts,
            vec![
                SortKey::Title,
                SortKey::Added,
                SortKey::LastPlayed,
                SortKey::Rating,
                SortKey::Duration
            ]
        );
        assert_eq!(
            browsing.filters,
            vec![
                FilterPart::Watched,
                FilterPart::Genres,
                FilterPart::Years,
                FilterPart::DynamicRanges
            ]
        );
        assert_eq!(
            browsing.dynamic_ranges,
            vec![DynamicRange::Sdr, DynamicRange::Hdr]
        );
        let music = Section {
            sorts: set(&["titleSort", "addedAt"]),
            filters: set(&["genre"]),
        };
        let browsing = music.browsing(LibraryKind::Music);
        assert_eq!(browsing.sorts, vec![SortKey::Title, SortKey::Added]);
        assert_eq!(browsing.filters, vec![FilterPart::Genres]);
    }

    #[test]
    fn ranges_are_plex_filters_hdr_taking_in_dolby_vision() {
        let pairs = |r: &[DynamicRange], dovi| {
            ranges(r, dovi)
                .into_iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&")
        };
        use DynamicRange::*;
        assert_eq!(pairs(&[Sdr], true), "hdr=0&dovi=0");
        assert_eq!(pairs(&[Sdr], false), "hdr=0");
        assert_eq!(pairs(&[Hdr], true), "push=1&hdr=1&or=1&dovi=1&pop=1");
        assert_eq!(pairs(&[Hdr], false), "hdr=1");
        assert_eq!(pairs(&[DolbyVision], true), "dovi=1");
        assert_eq!(
            pairs(&[Hdr, DolbyVision], true),
            "push=1&hdr=1&or=1&dovi=1&pop=1"
        );
        assert_eq!(
            pairs(&[Sdr, DolbyVision], true),
            "push=1&hdr=0&or=1&dovi=1&pop=1"
        );
        assert_eq!(pairs(&[Sdr, Hdr], true), "");
    }

    #[test]
    fn sd_is_every_resolution_below_720() {
        let present: Vec<String> = ["4k", "1080", "720", "576", "480", "sd"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert_eq!(
            resolution_values(Resolution::Sd, &present),
            vec!["576", "480", "sd"]
        );
        assert_eq!(resolution_values(Resolution::Uhd, &present), vec!["4k"]);
    }

    #[test]
    fn a_directory_key_is_decoded() {
        assert_eq!(decoded("16%2B"), "16+");
        assert_eq!(decoded("Not%20Rated"), "Not Rated");
        assert_eq!(decoded("PG-13"), "PG-13");
    }
}
