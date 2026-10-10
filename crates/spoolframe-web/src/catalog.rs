//! Catalog browsing for admins (P3-16), what the pages decide apart from
//! drawing: host-compiled, so `cargo test` covers it. The pages are
//! `app::catalog` (a library, WebCatalog) and `app::catalog_item` (an
//! item, WebCatalogItem).

use spoolframe_admin_api::{
    CatalogFilter, CatalogKind, CatalogLibrary, CatalogRow, FileState, Item, KindCount,
    LibraryKind, Marker, MarkerKind, MediaVersion, PathsChecked,
};

use crate::logic::{ago, count, thousands};

/// The rows a page can hold, the first the default.
pub const PER_PAGE: [u32; 3] = [50, 100, 200];

/// Where a library page is: what is in its address.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct View {
    /// 1 is the first.
    pub page: u32,
    pub per: u32,
    pub filter: Option<CatalogFilter>,
    pub kind: Option<CatalogKind>,
    pub text: String,
}

pub fn filter_name(filter: CatalogFilter) -> &'static str {
    match filter {
        CatalogFilter::Missing => "missing",
        CatalogFilter::Unmatched => "unmatched",
        CatalogFilter::NoArtwork => "noart",
    }
}

pub fn filter_label(filter: CatalogFilter) -> &'static str {
    match filter {
        CatalogFilter::Missing => "Missing files",
        CatalogFilter::Unmatched => "Unmatched",
        CatalogFilter::NoArtwork => "No artwork",
    }
}

pub const FILTERS: [CatalogFilter; 3] = [
    CatalogFilter::Missing,
    CatalogFilter::Unmatched,
    CatalogFilter::NoArtwork,
];

pub fn kind_name(kind: CatalogKind) -> &'static str {
    match kind {
        CatalogKind::Movie => "movie",
        CatalogKind::Show => "show",
        CatalogKind::Season => "season",
        CatalogKind::Episode => "episode",
        CatalogKind::Artist => "artist",
        CatalogKind::Album => "album",
        CatalogKind::Track => "track",
    }
}

const KINDS: [CatalogKind; 7] = [
    CatalogKind::Movie,
    CatalogKind::Show,
    CatalogKind::Season,
    CatalogKind::Episode,
    CatalogKind::Artist,
    CatalogKind::Album,
    CatalogKind::Track,
];

pub fn kind_from_name(name: &str) -> Option<CatalogKind> {
    KINDS.into_iter().find(|k| kind_name(*k) == name)
}

/// "Episode", as the Kind column says it.
pub fn kind_label(kind: CatalogKind) -> &'static str {
    match kind {
        CatalogKind::Movie => "Movie",
        CatalogKind::Show => "Show",
        CatalogKind::Season => "Season",
        CatalogKind::Episode => "Episode",
        CatalogKind::Artist => "Artist",
        CatalogKind::Album => "Album",
        CatalogKind::Track => "Track",
    }
}

/// "Episodes", as the kind select offers it.
pub fn kind_plural(kind: CatalogKind) -> &'static str {
    match kind {
        CatalogKind::Movie => "Movies",
        CatalogKind::Show => "Shows",
        CatalogKind::Season => "Seasons",
        CatalogKind::Episode => "Episodes",
        CatalogKind::Artist => "Artists",
        CatalogKind::Album => "Albums",
        CatalogKind::Track => "Tracks",
    }
}

/// The kinds a library's kind select offers: none for a movie library,
/// which holds one kind.
pub fn selectable(kind: LibraryKind) -> &'static [CatalogKind] {
    match kind {
        LibraryKind::Movies => &[],
        LibraryKind::Shows => &[CatalogKind::Show, CatalogKind::Season, CatalogKind::Episode],
        LibraryKind::Music => &[CatalogKind::Album, CatalogKind::Track, CatalogKind::Artist],
    }
}

pub fn item_kind(item: &Item) -> CatalogKind {
    match item {
        Item::Movie(_) => CatalogKind::Movie,
        Item::Show(_) => CatalogKind::Show,
        Item::Season(_) => CatalogKind::Season,
        Item::Episode(_) => CatalogKind::Episode,
        Item::Artist(_) => CatalogKind::Artist,
        Item::Album(_) => CatalogKind::Album,
        Item::Track(_) => CatalogKind::Track,
    }
}

impl View {
    /// The view an address names, anything it does not say at its
    /// default.
    pub fn parse(page: &str, per: &str, filter: &str, kind: &str, text: &str) -> Self {
        let per = per
            .parse()
            .ok()
            .filter(|p| PER_PAGE.contains(p))
            .unwrap_or(PER_PAGE[0]);
        Self {
            page: page.parse().ok().filter(|p| *p >= 1).unwrap_or(1),
            per,
            filter: FILTERS.into_iter().find(|f| filter_name(*f) == filter),
            kind: kind_from_name(kind),
            text: text.to_owned(),
        }
    }

    /// The first row of the page, counted from 0.
    pub fn offset(&self) -> u32 {
        self.page.saturating_sub(1).saturating_mul(self.per)
    }

    /// How many pages `total` rows make: at least one.
    pub fn pages(&self, total: u32) -> u32 {
        total.div_ceil(self.per.max(1)).max(1)
    }
}

/// The page numbers the pager shows, `None` for a gap: the first, the
/// last, and those next to the current one.
pub fn pager(current: u32, pages: u32) -> Vec<Option<u32>> {
    let mut shown: Vec<u32> = [1, current.saturating_sub(1), current, current + 1, pages]
        .into_iter()
        .filter(|p| (1..=pages).contains(p))
        .collect();
    // The board's "1 2 3 … 9": the first pages are shown together.
    if current <= 2 {
        shown.extend((1..=3).filter(|p| *p <= pages));
    }
    shown.sort_unstable();
    shown.dedup();
    let mut out = Vec::new();
    for (i, p) in shown.iter().enumerate() {
        if i > 0 && *p > shown[i - 1] + 1 {
            out.push(None);
        }
        out.push(Some(*p));
    }
    out
}

/// "1–50 of 412 shows", or under a filter or search, "3 of 412".
pub fn pager_text(view: &View, shown: usize, total: u32, all: u32, unit: &str) -> String {
    if total == 0 {
        return String::new();
    }
    let first = view.offset() + 1;
    let last = view.offset() + u32::try_from(shown).unwrap_or(u32::MAX);
    let range = if first == last {
        thousands(u64::from(first))
    } else {
        format!(
            "{}–{}",
            thousands(u64::from(first)),
            thousands(u64::from(last))
        )
    };
    if view.filter.is_some() || !view.text.trim().is_empty() || total != all {
        format!("{range} of {}", thousands(u64::from(total)))
    } else {
        format!("{range} of {} {unit}", thousands(u64::from(total)))
    }
}

/// What the header says the library holds: "412 shows · 18,930
/// episodes", seasons left out as the board leaves them.
pub fn counts_line(counts: &[KindCount]) -> String {
    counts
        .iter()
        .filter(|c| !matches!(c.kind, CatalogKind::Season | CatalogKind::Artist))
        .map(|c| {
            format!(
                "{} {}",
                thousands(u64::from(c.count)),
                kind_name(c.kind).to_owned() + if c.count == 1 { "" } else { "s" }
            )
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// How the header says the path check is: "paths checked 2 minutes ago".
pub fn paths_line(paths: &PathsChecked, now: i64) -> String {
    match paths {
        PathsChecked::NotChecked => "paths not checked yet".into(),
        PathsChecked::Checking { .. } => "checking paths…".into(),
        PathsChecked::Checked { at, .. } => format!("paths checked {}", ago(now, *at)),
        PathsChecked::Failed { .. } => "the path check failed".into(),
    }
}

/// The header's line: "Plex · Tower · TV Shows · 412 shows · 18,930
/// episodes · paths checked 2 minutes ago".
pub fn header_line(library: &CatalogLibrary, now: i64) -> String {
    let mut parts = vec!["Plex".to_owned(), library.server_name.clone()];
    parts.extend(library.plex_title.clone());
    let counts = counts_line(&library.counts);
    if !counts.is_empty() {
        parts.push(counts);
    }
    parts.push(paths_line(&library.paths, now));
    parts.join(" · ")
}

/// The top kind's plural, for "of 412 shows".
pub fn unit(kind: LibraryKind) -> &'static str {
    match kind {
        LibraryKind::Movies => "movies",
        LibraryKind::Shows => "shows",
        LibraryKind::Music => "albums",
    }
}

/// How many of the library's top kind there are.
pub fn top_count(library: &CatalogLibrary) -> u32 {
    library.counts.first().map_or(0, |c| c.count)
}

/// The count a quick filter's chip shows; `None` while it is not known
/// (Missing, before a check).
pub fn filter_count(library: &CatalogLibrary, filter: CatalogFilter) -> Option<u64> {
    match filter {
        CatalogFilter::Missing => match library.paths {
            PathsChecked::Checked { missing, .. } => Some(missing),
            _ => None,
        },
        CatalogFilter::Unmatched => Some(u64::from(library.unmatched)),
        CatalogFilter::NoArtwork => Some(u64::from(library.no_artwork)),
    }
}

/// The line above the table that says what a filter shows.
pub fn filter_banner(filter: CatalogFilter, paths: &PathsChecked, now: i64) -> String {
    match filter {
        CatalogFilter::Missing => match paths {
            PathsChecked::Checked { missing, at, .. } => format!(
                "{} — the same list as the last path check (Server › Media paths, {}).",
                match missing {
                    0 => "No file missing".to_owned(),
                    1 => "1 file missing".to_owned(),
                    n => format!("{} files missing", thousands(*n)),
                },
                ago(now, *at)
            ),
            PathsChecked::Failed { error, .. } => {
                format!("The path check could not run: {error}")
            }
            _ => "Checking every file of this library, as Server › Media paths does…".into(),
        },
        CatalogFilter::Unmatched => {
            "Items Plex has not matched to an agent. Match them in Plex; this page shows the change when it next loads.".into()
        }
        CatalogFilter::NoArtwork => {
            "Items with no poster or background. Clients show a placeholder.".into()
        }
    }
}

/// "S02E04".
pub fn episode_code(season: u32, episode: u32) -> String {
    format!("S{season:02}E{episode:02}")
}

/// What a row says under its title: a show's seasons and episodes, an
/// episode's show and place.
pub fn subtitle(row: &CatalogRow) -> String {
    match &row.item {
        Item::Show(_) => match (row.children, row.leaves) {
            (Some(s), Some(e)) => format!(
                "{} · {}",
                count(u64::from(s), "season"),
                count(u64::from(e), "episode")
            ),
            (None, Some(e)) => count(u64::from(e), "episode"),
            _ => String::new(),
        },
        Item::Season(s) => row
            .leaves
            .map(|e| count(u64::from(e), "episode"))
            .or_else(|| s.show_title.clone())
            .unwrap_or_default(),
        Item::Episode(e) => {
            let code = episode_code(e.season, e.episode);
            match &e.show_title {
                Some(show) => format!("{show} · {code}"),
                None => code,
            }
        }
        Item::Album(a) => {
            let artist = a.artist_name.clone().unwrap_or_default();
            match row.leaves.or(row.children) {
                Some(n) if !artist.is_empty() => {
                    format!("{artist} · {}", count(u64::from(n), "track"))
                }
                Some(n) => count(u64::from(n), "track"),
                None => artist,
            }
        }
        Item::Track(t) => t.track.map(|n| format!("Track {n}")).unwrap_or_default(),
        Item::Movie(_) | Item::Artist(_) => String::new(),
    }
}

/// A row's title: an episode's has its number in front.
pub fn row_title(item: &Item) -> String {
    match item {
        Item::Movie(v) => v.title.clone(),
        Item::Show(v) => v.title.clone(),
        Item::Season(v) => v.title.clone(),
        Item::Episode(v) => format!("{}. {}", v.episode, v.title),
        Item::Artist(v) => v.name.clone(),
        Item::Album(v) => v.title.clone(),
        Item::Track(v) => v.title.clone(),
    }
}

pub fn year(item: &Item) -> Option<u16> {
    match item {
        Item::Movie(v) => v.year,
        Item::Show(v) => v.year,
        Item::Episode(v) => v.year,
        Item::Album(v) => v.year,
        _ => None,
    }
}

/// The agents a row was matched with, as the Match column names them:
/// `tvdb tmdb imdb`, Plex's own GUID left out.
pub fn agent_names(agents: &[String]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for agent in agents {
        if let Some((scheme, _)) = agent.split_once("://")
            && !names.iter().any(|n| n == scheme)
        {
            names.push(scheme.to_owned());
        }
    }
    names
}

/// What a row's File column says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileCell {
    /// Every file found.
    Found,
    /// A file of its own is missing.
    Missing,
    /// A show's, season's or album's: all its files found by the last
    /// check, or how many were not.
    AllFound,
    SomeMissing(u64),
    /// Not known yet: no check, or no files.
    Unknown,
}

pub fn file_cell(row: &CatalogRow) -> FileCell {
    if !row.files.is_empty() {
        return if row.files.iter().all(|f| f.found) {
            FileCell::Found
        } else {
            FileCell::Missing
        };
    }
    match row.missing {
        Some(0) => FileCell::AllFound,
        Some(n) => FileCell::SomeMissing(n),
        None => FileCell::Unknown,
    }
}

pub fn file_text(cell: &FileCell) -> String {
    match cell {
        FileCell::Found => "✓ Found".into(),
        FileCell::Missing => "✕ Missing".into(),
        FileCell::AllFound => "✓ All found".into(),
        FileCell::SomeMissing(n) => format!("✕ {} missing", thousands(*n)),
        FileCell::Unknown => "—".into(),
    }
}

/// The paths a row shows: its first file's, or nothing for a show.
pub fn first_file(row: &CatalogRow) -> Option<&FileState> {
    row.files
        .iter()
        .find(|f| !f.found)
        .or_else(|| row.files.first())
}

/// Whether a row opens in place: a show into seasons, a season into
/// episodes, an album into tracks.
pub fn opens(item: &Item) -> bool {
    matches!(item, Item::Show(_) | Item::Season(_) | Item::Album(_))
}

/// The columns an admin added (Columns menu), as kept in the browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Columns {
    pub media_id: bool,
    pub plex_path: bool,
    pub added: bool,
    pub updated: bool,
}

impl Columns {
    /// Missing files is about paths: its rows show the ids and both paths
    /// whatever was chosen.
    pub fn for_filter(self, filter: Option<CatalogFilter>) -> Self {
        match filter {
            Some(CatalogFilter::Missing) => Self {
                media_id: true,
                plex_path: true,
                ..self
            },
            _ => self,
        }
    }

    /// As kept: `mediaid,plex`.
    pub fn store(self) -> String {
        [
            (self.media_id, "mediaid"),
            (self.plex_path, "plex"),
            (self.added, "added"),
            (self.updated, "updated"),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect::<Vec<_>>()
        .join(",")
    }

    pub fn load(kept: &str) -> Self {
        let has = |name: &str| kept.split(',').any(|n| n == name);
        Self {
            media_id: has("mediaid"),
            plex_path: has("plex"),
            added: has("added"),
            updated: has("updated"),
        }
    }
}

/// What a search and filter that found nothing says.
pub fn nothing_text(text: &str, filter: Option<CatalogFilter>) -> String {
    let text = text.trim();
    match (text.is_empty(), filter) {
        (false, Some(f)) => format!("Nothing matches “{text}” with {} on", filter_label(f)),
        (false, None) => format!("Nothing matches “{text}”"),
        (true, Some(f)) => format!("Nothing here with {} on", filter_label(f)),
        (true, None) => "This library is empty".into(),
    }
}

/// What a search looks at, under "nothing matches".
pub fn searched_text(kind: LibraryKind) -> &'static str {
    match kind {
        LibraryKind::Movies => "Search looks at movie titles.",
        LibraryKind::Shows => "Search looks at the titles of shows and episodes.",
        LibraryKind::Music => "Search looks at the titles of albums and tracks.",
    }
}

/// An external ID as the item page shows it: who gave it, the ID, and
/// where it is, when that address is well known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalId {
    pub label: String,
    pub value: String,
    pub url: Option<String>,
    /// Its site's name, as the link says it.
    pub site: Option<&'static str>,
}

/// `id` (`imdb://tt…`) of an item of `kind`, with a link out where the
/// site's address for it is well known: IMDb for any title; TMDB for a
/// movie or show; TVDB for a movie, series or episode (its dereferrer);
/// MusicBrainz for an artist. Plex's own GUIDs have no public page.
pub fn external_id(id: &str, kind: CatalogKind) -> ExternalId {
    let (scheme, value) = id.split_once("://").unwrap_or(("", id));
    let numeric = !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit());
    let (label, url, site) = match scheme {
        "imdb" if value.starts_with("tt") && value[2..].bytes().all(|b| b.is_ascii_digit()) => (
            "IMDb",
            Some(format!("https://www.imdb.com/title/{value}/")),
            Some("imdb.com"),
        ),
        "imdb" => ("IMDb", None, None),
        "tmdb" => {
            let path = match kind {
                CatalogKind::Movie => Some("movie"),
                CatalogKind::Show => Some("tv"),
                _ => None,
            };
            match path {
                Some(path) if numeric => (
                    "TMDB",
                    Some(format!("https://www.themoviedb.org/{path}/{value}")),
                    Some("themoviedb.org"),
                ),
                _ => ("TMDB", None, None),
            }
        }
        "tvdb" => {
            let path = match kind {
                CatalogKind::Movie => Some("movie"),
                CatalogKind::Show => Some("series"),
                CatalogKind::Episode => Some("episode"),
                _ => None,
            };
            match path {
                Some(path) if numeric => (
                    "TVDB",
                    Some(format!("https://thetvdb.com/dereferrer/{path}/{value}")),
                    Some("thetvdb.com"),
                ),
                _ => ("TVDB", None, None),
            }
        }
        "mbid" => match kind {
            CatalogKind::Artist if value.len() == 36 => (
                "MusicBrainz",
                Some(format!("https://musicbrainz.org/artist/{value}")),
                Some("musicbrainz.org"),
            ),
            _ => ("MusicBrainz", None, None),
        },
        "plex" => ("Plex GUID", None, None),
        _ => ("ID", None, None),
    };
    ExternalId {
        label: label.to_owned(),
        value: if scheme == "plex" || scheme.is_empty() {
            id.to_owned()
        } else {
            value.to_owned()
        },
        url,
        site,
    }
}

fn codec_name(codec: &str) -> String {
    match codec {
        "hevc" | "h265" => "HEVC".into(),
        "h264" | "avc" => "H.264".into(),
        "av1" => "AV1".into(),
        "vp9" => "VP9".into(),
        "mpeg2video" => "MPEG-2".into(),
        "mpeg4" => "MPEG-4".into(),
        "vc1" => "VC-1".into(),
        "eac3" => "E-AC-3".into(),
        "ac3" => "AC-3".into(),
        "aac" => "AAC".into(),
        "dca" | "dts" => "DTS".into(),
        "dca-ma" => "DTS-HD MA".into(),
        "truehd" => "TrueHD".into(),
        "flac" => "FLAC".into(),
        "opus" => "Opus".into(),
        "mp3" => "MP3".into(),
        "vorbis" => "Vorbis".into(),
        "pcm" => "PCM".into(),
        other => other.to_uppercase(),
    }
}

fn resolution_name(resolution: &str) -> String {
    match resolution {
        "4k" => "4K".into(),
        "sd" => "SD".into(),
        r if r.bytes().all(|b| b.is_ascii_digit()) => format!("{r}p"),
        other => other.to_owned(),
    }
}

fn channels_name(channels: u32) -> String {
    match channels {
        1 => "mono".into(),
        2 => "stereo".into(),
        6 => "5.1".into(),
        8 => "7.1".into(),
        n => format!("{n} ch"),
    }
}

/// A version as its card's title says it: "MKV · HEVC 4K · E-AC-3 5.1",
/// from what Plex says of it.
pub fn version_line(v: &MediaVersion) -> String {
    let mut parts = Vec::new();
    if let Some(c) = &v.container {
        parts.push(c.to_uppercase());
    }
    let video: Vec<String> = [
        v.video_codec.as_deref().map(codec_name),
        v.video_resolution.as_deref().map(resolution_name),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !video.is_empty() {
        parts.push(video.join(" "));
    }
    let audio: Vec<String> = [
        v.audio_codec.as_deref().map(codec_name),
        v.audio_channels.map(channels_name),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !audio.is_empty() {
        parts.push(audio.join(" "));
    }
    parts.join(" · ")
}

/// Bytes as the boards write sizes: "4.1 GB", "788 MB".
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 || value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A running time: "50 min", "1 h 52 min".
pub fn running_time(ms: u64) -> String {
    let minutes = (ms + 30_000) / 60_000;
    match minutes {
        0 => format!("{} s", ms / 1000),
        m if m < 60 => format!("{m} min"),
        m if m % 60 == 0 => format!("{} h", m / 60),
        m => format!("{} h {} min", m / 60, m % 60),
    }
}

/// A time in an item: "0:42", "47:10", "1:02:03".
pub fn clock(ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// How long a stretch is: "50 s", "2 min 54 s".
pub fn length(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        s if s < 60 => format!("{s} s"),
        s if s % 60 == 0 => format!("{} min", s / 60),
        s => format!("{} min {} s", s / 60, s % 60),
    }
}

pub fn marker_name(kind: MarkerKind) -> &'static str {
    match kind {
        MarkerKind::Intro => "Intro",
        MarkerKind::Credits => "Credits",
        MarkerKind::Unknown => "Other",
    }
}

/// A marker's line: "0:42 – 1:32 · 50 s", or for credits that run out,
/// "47:10 – 50:04 · to the end".
pub fn marker_line(m: &Marker) -> String {
    let range = format!("{} – {}", clock(m.start_ms), clock(m.end_ms));
    if m.to_end {
        format!("{range} · to the end")
    } else {
        format!("{range} · {}", length(m.end_ms.saturating_sub(m.start_ms)))
    }
}

/// Where a marker sits on the timeline, as percentages of `duration`:
/// its start and its width.
pub fn marker_span(m: &Marker, duration_ms: u64) -> (f64, f64) {
    if duration_ms == 0 {
        return (0.0, 0.0);
    }
    let at = |ms: u64| (ms.min(duration_ms) as f64) * 100.0 / duration_ms as f64;
    let start = at(m.start_ms);
    let end = if m.to_end { 100.0 } else { at(m.end_ms) };
    (start, (end - start).max(0.5))
}

/// The duration of an item clients get, when it has one.
pub fn duration_ms(item: &Item) -> Option<u64> {
    let d = match item {
        Item::Movie(v) => v.duration,
        Item::Episode(v) => v.duration,
        Item::Track(v) => v.duration,
        _ => None,
    }?;
    u64::try_from(d.as_millis()).ok()
}

/// What the item is, under its title: "Episode · Severance · Season 2,
/// Episode 4 · 50 min".
pub fn item_context(item: &Item) -> String {
    let mut parts = vec![kind_label(item_kind(item)).to_owned()];
    match item {
        Item::Movie(v) => parts.extend(v.year.map(|y| y.to_string())),
        Item::Show(v) => parts.extend(v.year.map(|y| y.to_string())),
        Item::Season(v) => parts.extend(v.show_title.clone()),
        Item::Episode(v) => {
            parts.extend(v.show_title.clone());
            parts.push(format!("Season {}, Episode {}", v.season, v.episode));
        }
        Item::Album(v) => {
            parts.extend(v.artist_name.clone());
            parts.extend(v.year.map(|y| y.to_string()));
        }
        Item::Track(v) => parts.extend(v.track.map(|n| format!("Track {n}"))),
        Item::Artist(_) => {}
    }
    parts.extend(duration_ms(item).map(running_time));
    parts.join(" · ")
}

/// The files chip: whether every file was found, and what it says.
pub fn files_chip(versions: &[MediaVersion]) -> Option<(bool, String)> {
    let files: Vec<&FileState> = versions
        .iter()
        .flat_map(|v| v.parts.iter().map(|p| &p.file))
        .collect();
    let missing = files.iter().filter(|f| !f.found).count();
    match (files.len(), missing) {
        (0, _) => None,
        (1, 0) => Some((true, "1 file, found".into())),
        (1, _) => Some((false, "1 file, missing".into())),
        (n, 0) => Some((true, format!("{n} files, all found"))),
        (n, m) => Some((false, format!("{m} of {n} files missing"))),
    }
}

/// What a file with no mapping says of where it is: the folder no
/// mapping covers.
pub fn uncovered(plex_path: &str) -> String {
    match plex_path.rsplit_once('/') {
        Some((folder, _)) if !folder.is_empty() => folder.to_owned(),
        _ => plex_path.to_owned(),
    }
}

/// The tabs an item has, by its kind: files, artwork and markers where
/// it has them, children for a show, season, artist or album, and
/// playback debug for what plays.
pub fn tabs(kind: CatalogKind) -> Vec<(&'static str, &'static str)> {
    let mut tabs = vec![("overview", "Overview"), ("files", "Files & paths")];
    if kind != CatalogKind::Track {
        tabs.push(("artwork", "Artwork"));
    }
    if matches!(kind, CatalogKind::Movie | CatalogKind::Episode) {
        tabs.push(("markers", "Markers"));
    }
    if matches!(
        kind,
        CatalogKind::Show | CatalogKind::Season | CatalogKind::Artist | CatalogKind::Album
    ) {
        tabs.push(("children", "Children"));
    }
    if matches!(
        kind,
        CatalogKind::Movie | CatalogKind::Episode | CatalogKind::Track
    ) {
        tabs.push(("debug", "Debug"));
    }
    tabs
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_admin_api::{Episode, PlaybackState, Show};

    fn row(item: Item) -> CatalogRow {
        CatalogRow {
            item,
            matched: true,
            guid: None,
            agents: Vec::new(),
            added_at: None,
            updated_at: None,
            children: None,
            leaves: None,
            files: Vec::new(),
            missing: None,
        }
    }

    fn show() -> Item {
        Item::Show(Show {
            id: "plex:show:52011".parse().unwrap(),
            title: "Severance".into(),
            year: Some(2022),
            summary: None,
            content_rating: None,
            season_count: None,
            genres: Vec::new(),
            artwork: Vec::new(),
        })
    }

    fn file(found: bool) -> FileState {
        FileState {
            plex_path: "/data/a.mkv".into(),
            mapped_path: "/media/a.mkv".into(),
            mapping: None,
            found,
            problem: (!found).then(|| "No such file or directory (os error 2)".into()),
        }
    }

    #[test]
    fn an_address_without_a_view_is_the_first_page_of_50() {
        let view = View::parse("", "", "", "", "");
        assert_eq!(
            (view.page, view.per, view.filter, view.kind),
            (1, 50, None, None)
        );
        let view = View::parse("3", "100", "missing", "episode", "hollow");
        assert_eq!(view.offset(), 200);
        assert_eq!(view.filter, Some(CatalogFilter::Missing));
        assert_eq!(view.kind, Some(CatalogKind::Episode));
        // Only the sizes the select offers.
        assert_eq!(View::parse("1", "7", "", "", "").per, 50);
    }

    #[test]
    fn the_pager_shows_the_first_pages_the_last_and_a_gap() {
        assert_eq!(pager(1, 9), [Some(1), Some(2), Some(3), None, Some(9)]);
        assert_eq!(
            pager(5, 9),
            [Some(1), None, Some(4), Some(5), Some(6), None, Some(9)]
        );
        assert_eq!(pager(1, 1), [Some(1)]);
        assert_eq!(View::parse("", "", "", "", "").pages(412), 9);
        assert_eq!(View::parse("", "", "", "", "").pages(0), 1);
    }

    #[test]
    fn the_pager_says_which_rows_of_how_many() {
        let view = View::parse("", "", "", "", "");
        assert_eq!(
            pager_text(&view, 50, 412, 412, "shows"),
            "1–50 of 412 shows"
        );
        let filtered = View::parse("", "", "missing", "", "");
        assert_eq!(pager_text(&filtered, 1, 1, 412, "shows"), "1 of 1");
    }

    #[test]
    fn a_show_says_its_seasons_and_episodes_and_an_episode_its_place() {
        let mut show = row(show());
        show.children = Some(4);
        show.leaves = Some(46);
        assert_eq!(subtitle(&show), "4 seasons · 46 episodes");
        let episode = row(Item::Episode(Episode {
            id: "plex:episode:61374".parse().unwrap(),
            show_id: "plex:show:52011".parse().unwrap(),
            show_title: Some("Severance".into()),
            season_id: "plex:season:61370".parse().unwrap(),
            season: 2,
            episode: 4,
            title: "Woe's Hollow".into(),
            summary: None,
            content_rating: None,
            year: None,
            air_date: None,
            duration: None,
            artwork: Vec::new(),
            playback: PlaybackState::default(),
        }));
        assert_eq!(subtitle(&episode), "Severance · S02E04");
        assert_eq!(row_title(&episode.item), "4. Woe's Hollow");
    }

    #[test]
    fn a_show_rolls_up_its_missing_files_and_a_movie_says_its_own() {
        let mut show = row(show());
        assert_eq!(file_cell(&show), FileCell::Unknown);
        show.missing = Some(1);
        assert_eq!(file_text(&file_cell(&show)), "✕ 1 missing");
        show.missing = Some(0);
        assert_eq!(file_text(&file_cell(&show)), "✓ All found");
        let mut movie = row(show.item.clone());
        movie.files = vec![file(true), file(false)];
        assert_eq!(file_cell(&movie), FileCell::Missing);
        assert!(!first_file(&movie).unwrap().found);
    }

    #[test]
    fn the_match_column_names_each_agent_once() {
        let agents = [
            "imdb://tt1".to_owned(),
            "tmdb://2".to_owned(),
            "imdb://tt3".to_owned(),
        ];
        assert_eq!(agent_names(&agents), ["imdb", "tmdb"]);
    }

    #[test]
    fn columns_are_kept_as_a_list_and_missing_files_shows_ids_and_paths() {
        let columns = Columns {
            added: true,
            ..Columns::default()
        };
        assert_eq!(columns.store(), "added");
        assert!(Columns::load("mediaid,updated").updated);
        let missing = columns.for_filter(Some(CatalogFilter::Missing));
        assert!(missing.media_id && missing.plex_path && missing.added);
    }

    #[test]
    fn the_header_names_the_source_counts_and_the_check() {
        let library = CatalogLibrary {
            id: "plex:1".into(),
            name: "TV Shows".into(),
            kind: LibraryKind::Shows,
            server: "plex".into(),
            server_name: "Tower".into(),
            plex_title: Some("TV Shows".into()),
            counts: vec![
                KindCount {
                    kind: CatalogKind::Show,
                    count: 412,
                },
                KindCount {
                    kind: CatalogKind::Season,
                    count: 1300,
                },
                KindCount {
                    kind: CatalogKind::Episode,
                    count: 18_930,
                },
            ],
            unmatched: 1,
            no_artwork: 1,
            paths: PathsChecked::Checked {
                at: 1000,
                ok: 18_929,
                missing: 1,
            },
        };
        assert_eq!(
            header_line(&library, 1120),
            "Plex · Tower · TV Shows · 412 shows · 18,930 episodes · paths checked 2 minutes ago"
        );
        assert_eq!(filter_count(&library, CatalogFilter::Missing), Some(1));
        assert_eq!(
            filter_banner(CatalogFilter::Missing, &library.paths, 1120),
            "1 file missing — the same list as the last path check (Server › Media paths, 2 minutes ago)."
        );
    }

    #[test]
    fn nothing_found_says_what_was_asked() {
        assert_eq!(
            nothing_text("sevrance", Some(CatalogFilter::Missing)),
            "Nothing matches “sevrance” with Missing files on"
        );
    }
    #[test]
    fn external_ids_link_out_where_the_address_is_well_known() {
        let imdb = external_id("imdb://tt27560178", CatalogKind::Episode);
        assert_eq!(
            imdb.url.as_deref(),
            Some("https://www.imdb.com/title/tt27560178/")
        );
        assert_eq!(
            (imdb.label.as_str(), imdb.value.as_str()),
            ("IMDb", "tt27560178")
        );
        assert_eq!(
            external_id("tmdb://60948", CatalogKind::Show)
                .url
                .as_deref(),
            Some("https://www.themoviedb.org/tv/60948")
        );
        // An episode's TMDB id needs its show's to be addressed.
        assert_eq!(
            external_id("tmdb://5604311", CatalogKind::Episode).url,
            None
        );
        assert_eq!(
            external_id("tvdb://10292371", CatalogKind::Episode)
                .url
                .as_deref(),
            Some("https://thetvdb.com/dereferrer/episode/10292371")
        );
        let guid = external_id(
            "plex://episode/5d9c10307b5c2e001e6ac51a",
            CatalogKind::Episode,
        );
        assert_eq!((guid.label.as_str(), guid.url), ("Plex GUID", None));
        assert_eq!(guid.value, "plex://episode/5d9c10307b5c2e001e6ac51a");
    }

    #[test]
    fn a_version_is_its_container_video_and_audio() {
        let v = MediaVersion {
            id: Some(88_213),
            container: Some("mkv".into()),
            video_codec: Some("hevc".into()),
            video_resolution: Some("4k".into()),
            video_profile: None,
            audio_codec: Some("eac3".into()),
            audio_channels: Some(6),
            bitrate: None,
            duration_ms: None,
            parts: Vec::new(),
        };
        assert_eq!(version_line(&v), "MKV · HEVC 4K · E-AC-3 5.1");
        assert_eq!(bytes(4_100_000_000), "4.1 GB");
        assert_eq!(bytes(787_842_211), "788 MB");
    }

    #[test]
    fn markers_read_as_the_board_writes_them() {
        let intro = Marker {
            kind: MarkerKind::Intro,
            start_ms: 42_000,
            end_ms: 92_000,
            to_end: false,
        };
        assert_eq!(marker_line(&intro), "0:42 – 1:32 · 50 s");
        let credits = Marker {
            kind: MarkerKind::Credits,
            start_ms: 2_830_000,
            end_ms: 3_004_513,
            to_end: true,
        };
        assert_eq!(marker_line(&credits), "47:10 – 50:04 · to the end");
        let (start, width) = marker_span(&credits, 3_004_512);
        assert!((start + width - 100.0).abs() < 1e-9);
        assert_eq!(running_time(3_004_512), "50 min");
        assert_eq!(running_time(6_720_000), "1 h 52 min");
    }

    #[test]
    fn only_what_plays_has_debug_and_only_containers_have_children() {
        let names = |k| tabs(k).into_iter().map(|(id, _)| id).collect::<Vec<_>>();
        assert_eq!(
            names(CatalogKind::Episode),
            ["overview", "files", "artwork", "markers", "debug"]
        );
        assert_eq!(
            names(CatalogKind::Show),
            ["overview", "files", "artwork", "children"]
        );
    }

    #[test]
    fn a_file_no_mapping_covers_names_its_folder() {
        assert_eq!(
            uncovered("/mnt/kids/Bluey/Season 1/Bluey - S01E01.mkv"),
            "/mnt/kids/Bluey/Season 1"
        );
    }
}
