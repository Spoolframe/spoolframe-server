//! Subtitles from subtitle sites on demand (PX-02): OpenSubtitles, SubDL,
//! or both, as the admin set; searched and downloaded only when someone
//! asks, with the server's one key for each, and kept for everyone on the
//! server.
//!
//! A search asks every site that is on at once and makes one list of what
//! they found; a site that fails or is out of quota leaves the others'
//! results, and the search says how each went. A result is named by its
//! site and the site's id of it: `subdl:3172974-3188291`.
//!
//! A download is checked ([`spoolframe_media::subtitle_file::keep`]: taken
//! out of its zip archive when SubDL sent one, made UTF-8, its format read
//! from its text, its cues read) and kept in
//! `state_dir/subtitles/downloaded`, named by an id of the server's own:
//! nothing a site said is part of a path. It becomes an external text
//! track of its media file at once, after the file's own tracks
//! ([`Prober::set_added_subtitles`]), served as any sidecar. One not found
//! by the file's hash is then synced (PX-03, [`SubtitleTimings::sync`]);
//! one found by it was made for this release. Only OpenSubtitles finds by
//! the hash: every SubDL download is synced.
//!
//! Searches are kept a day per site, item and language: opening the menu
//! twice asks each site once. A download names a result of a search kept
//! for the item, so what is downloaded is what was offered for it. Each
//! user may be held to a daily limit, over every site, and an admin may
//! take downloading from a user; searching is everyone's who can play the
//! item.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use spoolframe_catalog::{Catalog, CatalogError};
use spoolframe_media::subtitle_file::{self, FileError};
use spoolframe_media::{MediaDescriptor, Prober, SubtitleKind, SubtitleSource, SubtitleStream};
use spoolframe_types::{
    Item, MediaId, QuotaOf, SubtitleDownload, SubtitleDownloaded, SubtitleOrigin, SubtitleResult,
    SubtitleRole, SubtitleSearch, SubtitleSourceState, SubtitleSourceStatus,
};

use crate::principal::Actor;
use crate::store::subtitle_downloads::{DAY_SECS, DownloadRow};
use crate::store::{Store, StoreError};
use crate::subtitle_site::{SiteError, Source};
use crate::subtitle_sync::{SubtitleTimings, TrackRef};
use crate::{opensubtitles, subdl};

/// How long a search is kept per site, item and language.
pub const SEARCH_LIFE: Duration = Duration::from_secs(24 * 60 * 60);

/// The most searches kept; past it the oldest goes.
const MAX_SEARCHES: usize = 1024;

/// The most file hashes kept.
const MAX_HASHES: usize = 4096;

/// `subtitles { … }`, `opensubtitles { … }` and `subdl { … }` as the
/// server uses them. With no site, searching and downloading are off.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    /// The sites on, in the order they are asked; of results that rank
    /// the same, the first site's come first.
    pub sites: Vec<SiteSettings>,
    /// A user's downloads a day, from every site; 0 for no limit.
    pub daily_limit_per_user: u32,
}

/// One site, as the server uses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SiteSettings {
    OpenSubtitles {
        account: opensubtitles::Account,
        /// Searched when no language is asked for: `en`, `pt-br`.
        languages: Vec<String>,
    },
    SubDl {
        account: subdl::Account,
        languages: Vec<String>,
    },
}

impl SiteSettings {
    pub fn source(&self) -> Source {
        match self {
            SiteSettings::OpenSubtitles { .. } => Source::OpenSubtitles,
            SiteSettings::SubDl { .. } => Source::SubDl,
        }
    }

    pub fn languages(&self) -> &[String] {
        match self {
            SiteSettings::OpenSubtitles { languages, .. }
            | SiteSettings::SubDl { languages, .. } => languages,
        }
    }

    /// Whether `other` is the same site with the same key and account.
    fn same_account(&self, other: &SiteSettings) -> bool {
        match (self, other) {
            (
                SiteSettings::OpenSubtitles { account: a, .. },
                SiteSettings::OpenSubtitles { account: b, .. },
            ) => a == b,
            (SiteSettings::SubDl { account: a, .. }, SiteSettings::SubDl { account: b, .. }) => {
                a == b
            }
            _ => false,
        }
    }
}

/// A site's client, kept while its account is the same: a sign-in's
/// token and the pacing of calls go with it.
#[derive(Debug, Clone)]
enum Site {
    OpenSubtitles(Arc<opensubtitles::Site>),
    SubDl(Arc<subdl::Site>),
}

impl Site {
    fn of(settings: &SiteSettings) -> Site {
        match settings {
            SiteSettings::OpenSubtitles { account, .. } => {
                Site::OpenSubtitles(Arc::new(opensubtitles::Site::new(account.clone())))
            }
            SiteSettings::SubDl { account, .. } => {
                Site::SubDl(Arc::new(subdl::Site::new(account.clone())))
            }
        }
    }
}

/// The sites in use, each with the settings it came from.
#[derive(Debug, Clone, Default)]
struct Live {
    sites: Vec<(Site, SiteSettings)>,
    daily_limit_per_user: u32,
}

/// Who asks: their rights, and their name to show on what they download.
#[derive(Debug, Clone)]
pub struct Asker {
    pub actor: Actor,
    pub name: String,
}

/// Why a search, download or removal was refused.
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("this server has no subtitle source set up")]
    NoSource,
    #[error("{0} not found")]
    NotFound(&'static str),
    /// Not the downloader nor an admin, or a user an admin took
    /// downloading from.
    #[error("{0}")]
    NotAllowed(&'static str),
    /// The user's daily limit (no `site`), or the server's account at
    /// `site`.
    #[error("no subtitle downloads left today")]
    QuotaExhausted {
        whose: QuotaOf,
        resets_at: Option<i64>,
        site: Option<Source>,
    },
    #[error("{0}")]
    InvalidInput(String),
    #[error("{site} {error}")]
    Site { site: Source, error: SiteError },
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    /// The media file cannot be read or described.
    #[error("{0}")]
    Unavailable(String),
    /// The server's own file could not be written or removed.
    #[error("{0}")]
    Io(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl DownloadError {
    /// `error` from `source`: a used-up quota is the server's account's.
    fn of_site(site: Source, error: SiteError) -> Self {
        match error {
            SiteError::QuotaExhausted { resets_at } => DownloadError::QuotaExhausted {
                whose: QuotaOf::Account,
                resets_at,
                site: Some(site),
            },
            error => DownloadError::Site { site, error },
        }
    }
}

/// A media file's hash, and the size and modification time it was made
/// of.
type Hashed = ((u64, u128), Option<String>);

/// One subtitle a site found, as the server offers it.
#[derive(Debug, Clone)]
struct Offer {
    source: Source,
    /// The site's id of it: `5338574`, `3172974-3188291`.
    site_id: String,
    release: String,
    language: String,
    format: Option<String>,
    fps: Option<f64>,
    downloads: u64,
    /// 0 to 10; 0 while unrated.
    rating: f64,
    hearing_impaired: bool,
    origin: SubtitleOrigin,
    hash_match: bool,
    fetch: Fetch,
}

/// How an offer's file is downloaded.
#[derive(Debug, Clone)]
enum Fetch {
    /// By the site's file id, as SRT when the site's file is in a format
    /// the server does not read.
    OpenSubtitles { file_id: u64, as_srt: bool },
    /// By its path on SubDL's download host; of a season's pack, the
    /// episode's file.
    SubDl {
        path: String,
        episode: Option<(u32, u32)>,
    },
}

impl Offer {
    fn of_opensubtitles(found: &opensubtitles::Found) -> Offer {
        let format = found.file_name.as_deref().and_then(file_format);
        Offer {
            source: Source::OpenSubtitles,
            site_id: found.file_id.to_string(),
            release: found.release.clone(),
            language: found.language.clone(),
            format: format.map(str::to_owned),
            fps: found.fps,
            downloads: found.downloads,
            rating: found.rating,
            hearing_impaired: found.hearing_impaired,
            origin: if found.ai_translated {
                SubtitleOrigin::Ai
            } else if found.machine_translated {
                SubtitleOrigin::MachineTranslated
            } else {
                SubtitleOrigin::Human
            },
            hash_match: found.hash_match,
            fetch: Fetch::OpenSubtitles {
                file_id: found.file_id,
                // A format the server reads is taken as it is; any other
                // is asked for as SRT, which the site converts it to.
                as_srt: !format.is_some_and(|f| matches!(f, "srt" | "ass" | "ssa" | "vtt")),
            },
        }
    }

    fn of_subdl(found: &subdl::Found, episode: Option<(u32, u32)>) -> Offer {
        Offer {
            source: Source::SubDl,
            site_id: found.id.clone(),
            release: found.release.clone(),
            language: found.language.clone(),
            format: found.format.clone(),
            fps: found.fps,
            downloads: 0,
            rating: 0.0,
            // SubDL's own flag says yes to everything (`subdl::Query`).
            hearing_impaired: false,
            origin: SubtitleOrigin::Human,
            hash_match: false,
            fetch: Fetch::SubDl {
                path: found.path.clone(),
                episode,
            },
        }
    }

    /// What a result names it by.
    fn result_id(&self) -> String {
        format!("{}:{}", self.source.id(), self.site_id)
    }
}

/// A search a site answered, kept.
#[derive(Debug, Clone)]
struct Kept {
    at: Instant,
    found: Arc<Vec<Offer>>,
}

/// A search's key: the site, the item, the languages.
type SearchKey = (Source, String, String);

/// What a site's search found.
type Found = Arc<Vec<Offer>>;

/// What the sites are asked about an item, read from the catalog once
/// per search.
#[derive(Debug)]
struct Subject {
    item: Item,
    media: PathBuf,
    imdb: Option<u64>,
    tmdb: Option<u64>,
    /// An episode's show's.
    show_imdb: Option<u64>,
    show_tmdb: Option<u64>,
}

pub struct SubtitleDownloads {
    store: Store,
    catalog: Arc<dyn Catalog>,
    prober: Arc<Prober>,
    timings: Arc<SubtitleTimings>,
    /// `state_dir/subtitles/downloaded`.
    dir: PathBuf,
    live: Mutex<Live>,
    searches: Mutex<HashMap<SearchKey, Kept>>,
    /// Each media file's hash, by its path, size and modification time.
    hashes: Mutex<HashMap<PathBuf, Hashed>>,
    /// One download at a time per site: a result (one site's) asked for
    /// twice at once is downloaded once, and a site's quota is counted in
    /// order; a slow download from one site does not hold up the other's.
    downloading: HashMap<Source, tokio::sync::Mutex<()>>,
}

impl std::fmt::Debug for SubtitleDownloads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubtitleDownloads")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

impl SubtitleDownloads {
    /// Downloads kept in `dir`, set up as `settings` say.
    pub fn new(
        store: Store,
        catalog: Arc<dyn Catalog>,
        prober: Arc<Prober>,
        timings: Arc<SubtitleTimings>,
        dir: PathBuf,
        settings: Settings,
    ) -> std::io::Result<Self> {
        crate::files::private_dir(&dir)?;
        let this = Self {
            store,
            catalog,
            prober,
            timings,
            dir,
            live: Mutex::new(Live::default()),
            searches: Mutex::new(HashMap::new()),
            hashes: Mutex::new(HashMap::new()),
            downloading: Source::ALL
                .into_iter()
                .map(|s| (s, tokio::sync::Mutex::new(())))
                .collect(),
        };
        this.configure(settings);
        Ok(this)
    }

    /// Where the downloads are.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Use `settings` from now on (a live setting): no site turns
    /// searching and downloading off. What was downloaded stays. A site
    /// whose account is the same keeps its client; a changed one signs in
    /// again. The searches kept stay.
    pub fn configure(&self, settings: Settings) {
        let mut live = self.live.lock();
        let sites = settings
            .sites
            .into_iter()
            .map(|wanted| {
                let kept = live
                    .sites
                    .iter()
                    .find(|(_, had)| had.same_account(&wanted))
                    .map(|(site, _)| site.clone());
                (kept.unwrap_or_else(|| Site::of(&wanted)), wanted)
            })
            .collect();
        *live = Live {
            sites,
            daily_limit_per_user: settings.daily_limit_per_user,
        };
    }

    /// Whether any site is set up.
    pub fn is_on(&self) -> bool {
        !self.live.lock().sites.is_empty()
    }

    /// The sites set up, in the order they are asked.
    pub fn sources(&self) -> Vec<Source> {
        self.live
            .lock()
            .sites
            .iter()
            .map(|(_, s)| s.source())
            .collect()
    }

    fn live(&self) -> Result<Live, DownloadError> {
        let live = self.live.lock().clone();
        if live.sites.is_empty() {
            return Err(DownloadError::NoSource);
        }
        Ok(live)
    }

    /// Give each media file its downloads, as the server starts; files in
    /// the directory no download names (a download cut short by a stop)
    /// are removed.
    pub async fn restore(&self) -> Result<usize, DownloadError> {
        let rows = self.store.all_subtitle_downloads().await?;
        let mut by_file: HashMap<&str, Vec<&DownloadRow>> = HashMap::new();
        for row in &rows {
            by_file.entry(&row.media_path).or_default().push(row);
        }
        for (media, rows) in &by_file {
            let streams = rows.iter().map(|r| self.stream(r)).collect();
            self.prober.set_added_subtitles(Path::new(media), streams);
        }
        let named: HashSet<PathBuf> = rows.iter().map(|r| self.file_of(r)).collect();
        let mut entries = tokio::fs::read_dir(&self.dir)
            .await
            .map_err(|e| DownloadError::Io(e.to_string()))?;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if !named.contains(&path) {
                tracing::info!(file = %path.display(), "removing a subtitle download no record names");
                let _ = tokio::fs::remove_file(&path).await;
            }
        }
        Ok(rows.len())
    }

    /// Search every site that is on for item `id`'s subtitles in
    /// `language`, or in each site's languages, at once. When every site
    /// failed, the first one's error.
    pub async fn search(
        &self,
        id: &MediaId,
        language: Option<&str>,
    ) -> Result<SubtitleSearch, DownloadError> {
        let live = self.live()?;
        let asked = language.map(language_code).transpose()?;
        // Each site's search, and what of it is kept from the last day.
        let wanted: Vec<(&Site, Source, SearchKey, Option<Found>)> = live
            .sites
            .iter()
            .map(|(site, settings)| {
                let languages = match &asked {
                    Some(language) => language.clone(),
                    None => settings.languages().join(","),
                };
                let key = (settings.source(), id.to_string(), languages);
                let kept = self.kept(&key);
                (site, settings.source(), key, kept)
            })
            .collect();
        // The catalog and the file are read only when a site is asked.
        let asked_of = |source| {
            wanted
                .iter()
                .any(|(_, s, _, kept)| *s == source && kept.is_none())
        };
        let subject = if wanted.iter().any(|(_, _, _, kept)| kept.is_none()) {
            Some(
                self.subject(id, asked_of(Source::OpenSubtitles), asked_of(Source::SubDl))
                    .await?,
            )
        } else {
            None
        };
        let hash = match &subject {
            Some(subject)
                if wanted.iter().any(|(_, source, _, kept)| {
                    *source == Source::OpenSubtitles && kept.is_none()
                }) =>
            {
                self.hash(&subject.media).await
            }
            _ => None,
        };
        let searches = wanted.into_iter().map(|(site, source, key, kept)| {
            let subject = subject.as_ref();
            let hash = hash.clone();
            async move {
                let found = match (kept, subject) {
                    (Some(found), _) => Ok(found),
                    (None, Some(subject)) => self.site_search(site, subject, hash, key).await,
                    // Not so: the item is read whenever a site is asked.
                    (None, None) => Err(SiteError::Unreachable("the item was not read".into())),
                };
                (source, found)
            }
        });
        let answers = futures_util::future::join_all(searches).await;
        let mut sources = Vec::new();
        let mut offers = Vec::new();
        let mut first_failure = None;
        for (source, answer) in answers {
            let state = match answer {
                Ok(found) => {
                    let state = SubtitleSourceState::Found {
                        found: u32::try_from(found.len()).unwrap_or(u32::MAX),
                    };
                    offers.extend(found.iter().cloned());
                    state
                }
                Err(error) => {
                    // Its own words stay in the log: they may name the
                    // key's account, or be anything a site sends.
                    tracing::info!(%id, %source, %error, "a subtitle site's search failed");
                    let state = match &error {
                        SiteError::QuotaExhausted { resets_at } => {
                            SubtitleSourceState::QuotaExhausted {
                                resets_at: *resets_at,
                            }
                        }
                        SiteError::RateLimited { retry_after_ms } => {
                            SubtitleSourceState::RateLimited {
                                retry_after_ms: *retry_after_ms,
                            }
                        }
                        other => SubtitleSourceState::Failed {
                            message: failed_message(source, other),
                        },
                    };
                    first_failure.get_or_insert(DownloadError::of_site(source, error));
                    state
                }
            };
            sources.push(SubtitleSourceStatus {
                source: source.id().into(),
                state,
            });
        }
        if let Some(failure) = first_failure
            && sources
                .iter()
                .all(|s| !matches!(s.state, SubtitleSourceState::Found { .. }))
        {
            return Err(failure);
        }
        let rejected: HashSet<(String, String)> = self
            .store
            .subtitle_rejections(&id.to_string())
            .await?
            .into_iter()
            .collect();
        let downloaded: HashSet<(String, String)> = self
            .store
            .subtitle_downloads(&id.to_string())
            .await?
            .into_iter()
            .map(|r| (r.source, r.file_id))
            .collect();
        let mut results: Vec<SubtitleResult> = offers
            .iter()
            .map(|offer| {
                let key = (offer.source.id().to_owned(), offer.site_id.clone());
                result(offer, rejected.contains(&key), downloaded.contains(&key))
            })
            .collect();
        order(&mut results);
        let languages = match asked {
            Some(language) => vec![language],
            None => {
                let mut all: Vec<String> = Vec::new();
                for (_, settings) in &live.sites {
                    for language in settings.languages() {
                        if !all.contains(language) {
                            all.push(language.clone());
                        }
                    }
                }
                all
            }
        };
        Ok(SubtitleSearch {
            languages,
            results,
            sources,
        })
    }

    /// One site's results for the item, kept a day under `key`.
    async fn site_search(
        &self,
        site: &Site,
        subject: &Subject,
        hash: Option<String>,
        key: SearchKey,
    ) -> Result<Arc<Vec<Offer>>, SiteError> {
        let languages: Vec<String> = key.2.split(',').map(str::to_owned).collect();
        let found: Vec<Offer> = match site {
            Site::OpenSubtitles(site) => site
                .search(&opensubtitles_query(subject, hash, &languages))
                .await?
                .iter()
                .map(Offer::of_opensubtitles)
                .collect(),
            Site::SubDl(site) => {
                let episode = match &subject.item {
                    Item::Episode(e) => Some((e.season, e.episode)),
                    _ => None,
                };
                site.search(&subdl_query(subject, &languages))
                    .await?
                    .iter()
                    .map(|f| Offer::of_subdl(f, episode))
                    .collect()
            }
        };
        let found = Arc::new(found);
        self.keep(key, found.clone());
        Ok(found)
    }

    fn kept(&self, key: &SearchKey) -> Option<Arc<Vec<Offer>>> {
        let now = self.store.clock().now();
        self.searches
            .lock()
            .get(key)
            .filter(|k| now.duration_since(k.at) < SEARCH_LIFE)
            .map(|k| k.found.clone())
    }

    fn keep(&self, key: SearchKey, found: Arc<Vec<Offer>>) {
        let now = self.store.clock().now();
        let mut searches = self.searches.lock();
        searches.retain(|_, k| now.duration_since(k.at) < SEARCH_LIFE);
        if searches.len() >= MAX_SEARCHES
            && let Some(oldest) = searches
                .iter()
                .min_by_key(|(_, k)| k.at)
                .map(|(key, _)| key.clone())
        {
            searches.remove(&oldest);
        }
        searches.insert(key, Kept { at: now, found });
    }

    /// Result `site_id` of `source` of a search kept for item `id`.
    fn offered(&self, id: &MediaId, source: Source, site_id: &str) -> Option<Offer> {
        let now = self.store.clock().now();
        let id = id.to_string();
        self.searches
            .lock()
            .iter()
            .filter(|((s, item, _), k)| {
                *s == source && *item == id && now.duration_since(k.at) < SEARCH_LIFE
            })
            .find_map(|(_, k)| k.found.iter().find(|f| f.site_id == site_id).cloned())
    }

    /// Item `id` as the sites are asked about it: its ids, an episode's
    /// show's, and its media file.
    ///
    /// An episode's show's ids are asked for only when a site needs them:
    /// SubDL always (it lists a show's subtitles), OpenSubtitles when
    /// the episode has none of its own.
    async fn subject(
        &self,
        id: &MediaId,
        opensubtitles: bool,
        subdl: bool,
    ) -> Result<Subject, DownloadError> {
        let found = self.catalog.item(id).await?;
        if !matches!(found.item, Item::Movie(_) | Item::Episode(_)) {
            return Err(DownloadError::InvalidInput(
                "only a film or an episode has subtitles to find".into(),
            ));
        }
        let media = self.catalog.media_path(id).await?;
        let (imdb, tmdb) = site_ids(&found.external_ids);
        let wants_show = subdl || (opensubtitles && imdb.is_none() && tmdb.is_none());
        let (show_imdb, show_tmdb) = match &found.item {
            Item::Episode(episode) if wants_show => site_ids(
                &self
                    .catalog
                    .external_ids(&episode.show_id)
                    .await
                    .unwrap_or_default(),
            ),
            _ => (None, None),
        };
        Ok(Subject {
            item: found.item,
            media,
            imdb,
            tmdb,
            show_imdb,
            show_tmdb,
        })
    }

    /// The OpenSubtitles hash of `media`, made once while the file is as it
    /// is; `None` when it cannot be read (the search goes on by id).
    async fn hash(&self, media: &Path) -> Option<String> {
        let stamp = crate::files::stamp(media).await.ok()?;
        if let Some((kept, hash)) = self.hashes.lock().get(media)
            && *kept == stamp
        {
            return hash.clone();
        }
        let path = media.to_owned();
        let hash = match tokio::task::spawn_blocking(move || opensubtitles::movie_hash(&path)).await
        {
            Ok(Ok(hash)) => hash,
            Ok(Err(e)) => {
                tracing::warn!(media = %media.display(), error = %e, "cannot hash a media file for a subtitle search");
                return None;
            }
            Err(_) => return None,
        };
        let mut hashes = self.hashes.lock();
        if hashes.len() >= MAX_HASHES {
            hashes.clear();
        }
        hashes.insert(media.to_owned(), (stamp, hash.clone()));
        hash
    }

    /// Download result `result` (`opensubtitles:5338574`) of a search for
    /// item `id`, as `who`, from the site that found it.
    pub async fn download(
        &self,
        id: &MediaId,
        result: &str,
        who: &Asker,
    ) -> Result<SubtitleDownloaded, DownloadError> {
        let live = self.live()?;
        let not_offered = DownloadError::NotFound("a search result for this item; search again");
        let (source, site_id) = result
            .split_once(':')
            .and_then(|(source, site_id)| Some((Source::of(source)?, site_id)))
            .filter(|(_, site_id)| !site_id.is_empty())
            .ok_or_else(|| {
                DownloadError::InvalidInput(format!(
                    "{result:?} is not a search result's id, such as opensubtitles:5338574"
                ))
            })?;
        let Some((site, _)) = live
            .sites
            .iter()
            .find(|(_, settings)| settings.source() == source)
        else {
            return Err(not_offered);
        };
        let _one = self.downloading[&source].lock().await;
        let principal = who.actor.principal.to_string();
        let limit = live.daily_limit_per_user;
        if let Some(row) = self
            .store
            .subtitle_download_of(&id.to_string(), source.id(), site_id)
            .await?
        {
            let user_remaining = self.user_remaining(&principal, limit).await?;
            return self.answer(&row, who, None, user_remaining).await;
        }
        let offer = self.offered(id, source, site_id).ok_or(not_offered)?;
        if !self.store.may_download_subtitles(&principal).await? {
            return Err(DownloadError::NotAllowed(
                "an admin turned subtitle downloads off for you",
            ));
        }
        let today = self.store.subtitle_downloads_today(&principal).await?;
        if limit > 0 && today.len() >= limit as usize {
            return Err(DownloadError::QuotaExhausted {
                whose: QuotaOf::User,
                resets_at: today.first().map(|at| at + DAY_SECS),
                site: None,
            });
        }
        let media = self.catalog.media_path(id).await?;
        let (bytes, account_remaining) = match (site, &offer.fetch) {
            (Site::OpenSubtitles(site), Fetch::OpenSubtitles { file_id, as_srt }) => {
                let got = site
                    .download(*file_id, as_srt.then_some("srt"))
                    .await
                    .map_err(|e| DownloadError::of_site(source, e))?;
                (got.bytes, got.remaining)
            }
            (Site::SubDl(site), Fetch::SubDl { path, episode }) => {
                let got = site
                    .download(path)
                    .await
                    .map_err(|e| DownloadError::of_site(source, e))?;
                let file = if subtitle_file::is_archive(&got) {
                    subtitle_file::unpack(&got, *episode).map_err(not_kept)?
                } else {
                    got
                };
                (file, None)
            }
            _ => return Err(DownloadError::NotFound("a search result for this item")),
        };
        let kept = subtitle_file::keep(&bytes).map_err(not_kept)?;
        let row = DownloadRow {
            id: uuid::Uuid::new_v4().simple().to_string(),
            media_id: id.to_string(),
            media_path: media.display().to_string(),
            source: source.id().into(),
            file_id: offer.site_id.clone(),
            language: offer.language.clone(),
            release: offer.release.chars().take(300).collect(),
            format: kept.extension().into(),
            hearing_impaired: offer.hearing_impaired,
            origin: origin_text(offer.origin).into(),
            hash_match: offer.hash_match,
            downloaded_by: principal.clone(),
            downloaded_name: who.name.clone(),
            downloaded_at: self.store.now(),
        };
        let file = self.file_of(&row);
        write_into_place(&file, &kept.bytes).await?;
        if let Err(e) = self.store.put_subtitle_download(&row).await {
            let _ = tokio::fs::remove_file(&file).await;
            return Err(e.into());
        }
        tracing::info!(
            %id,
            %source,
            result = %offer.result_id(),
            download = %row.id,
            encoding = kept.encoding,
            cues = kept.cues,
            hash_match = row.hash_match,
            by = %who.name,
            "downloaded a subtitle"
        );
        self.attach(&media).await?;
        let user_remaining = self.user_remaining(&principal, limit).await?;
        let answer = self
            .answer(&row, who, account_remaining, user_remaining)
            .await?;
        if !row.hash_match {
            self.sync(id, &media, &file).await;
        }
        Ok(answer)
    }

    /// The downloads `principal` has left today, under a limit of `limit`.
    async fn user_remaining(&self, principal: &str, limit: u32) -> Result<Option<u32>, StoreError> {
        if limit == 0 {
            return Ok(None);
        }
        let today = self.store.subtitle_downloads_today(principal).await?.len();
        Ok(Some(limit.saturating_sub(today as u32)))
    }

    /// Sync the download at `file`, in the background (PX-03): it was not
    /// made for this release. It is served as it came meanwhile.
    async fn sync(&self, id: &MediaId, media: &Path, file: &Path) {
        let synced = async {
            let descriptor = self.describe(media).await?;
            let index = track_index(&descriptor, file)
                .ok_or(DownloadError::NotFound("the downloaded track"))?;
            self.timings
                .sync(TrackRef {
                    id: id.clone(),
                    media: media.to_owned(),
                    descriptor,
                    index,
                })
                .await
                .map_err(|e| DownloadError::Io(e.to_string()))
        }
        .await;
        if let Err(e) = synced {
            tracing::warn!(%id, error = %e, "cannot sync a downloaded subtitle");
        }
    }

    /// The reply to a download of `row`: its track as `who` sees it.
    async fn answer(
        &self,
        row: &DownloadRow,
        who: &Asker,
        account_remaining: Option<u32>,
        user_remaining: Option<u32>,
    ) -> Result<SubtitleDownloaded, DownloadError> {
        let media = PathBuf::from(&row.media_path);
        let descriptor = self.describe(&media).await?;
        let file = self.file_of(row);
        let index = track_index(&descriptor, &file)
            .ok_or(DownloadError::NotFound("the downloaded track"))?;
        let mut subtitle = descriptor.tracks().subtitles.swap_remove(index as usize);
        subtitle.download = Some(wire(row, &who.actor));
        Ok(SubtitleDownloaded {
            track: index,
            subtitle,
            account_remaining,
            user_remaining,
        })
    }

    /// Each subtitle track of `descriptor` (item `id`'s) that was
    /// downloaded, as `actor` sees it, by its place; `None` for the rest.
    pub async fn downloads(
        &self,
        id: &MediaId,
        descriptor: &MediaDescriptor,
        actor: &Actor,
    ) -> Result<Vec<Option<SubtitleDownload>>, StoreError> {
        let rows = self.store.subtitle_downloads(&id.to_string()).await?;
        let by_file: HashMap<PathBuf, &DownloadRow> =
            rows.iter().map(|r| (self.file_of(r), r)).collect();
        Ok(descriptor
            .subtitles
            .iter()
            .map(|s| match &s.source {
                SubtitleSource::External { path } => by_file.get(path).map(|row| wire(row, actor)),
                SubtitleSource::Embedded { .. } => None,
            })
            .collect())
    }

    /// Remove item `id`'s download `download`, as `actor`; one already
    /// gone is done.
    pub async fn remove(
        &self,
        id: &MediaId,
        download: &str,
        actor: &Actor,
    ) -> Result<(), DownloadError> {
        let rows = self.store.subtitle_downloads(&id.to_string()).await?;
        let Some(row) = rows.into_iter().find(|r| r.id == download) else {
            return Ok(());
        };
        let principal = actor.principal.to_string();
        if !actor.admin && row.downloaded_by != principal {
            return Err(DownloadError::NotAllowed(
                "only who downloaded it, or an admin, may remove it",
            ));
        }
        self.store
            .remove_subtitle_download(&row.id, &principal)
            .await?;
        let file = self.file_of(&row);
        let media = PathBuf::from(&row.media_path);
        self.attach(&media).await?;
        if let Err(e) = self.timings.forget(&file).await {
            tracing::warn!(%id, error = %e, "cannot forget a removed subtitle's timing");
        }
        match tokio::fs::remove_file(&file).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(DownloadError::Io(format!(
                    "cannot remove the subtitle: {e}"
                )));
            }
        }
        tracing::info!(%id, download = %row.id, by = %principal, "removed a downloaded subtitle");
        Ok(())
    }

    /// Give the media file at `media` its downloads as they are now.
    async fn attach(&self, media: &Path) -> Result<(), StoreError> {
        let rows = self
            .store
            .subtitle_downloads_of_file(&media.display().to_string())
            .await?;
        let streams = rows.iter().map(|r| self.stream(r)).collect();
        self.prober.set_added_subtitles(media, streams);
        Ok(())
    }

    async fn describe(&self, media: &Path) -> Result<Arc<MediaDescriptor>, DownloadError> {
        self.prober.describe(media).await.map_err(|e| {
            tracing::warn!(media = %media.display(), error = %e, "cannot describe a media file");
            DownloadError::Unavailable("the media file could not be read".into())
        })
    }

    fn file_of(&self, row: &DownloadRow) -> PathBuf {
        // The extension is one of three the server wrote; the id its own.
        self.dir.join(format!("{}.{}", row.id, row.format))
    }

    /// `row`'s track, as the media file lists it: titled by its site.
    fn stream(&self, row: &DownloadRow) -> SubtitleStream {
        let codec = match row.format.as_str() {
            "ass" => "ass",
            "vtt" => "webvtt",
            _ => "subrip",
        };
        let role = if row.hearing_impaired {
            SubtitleRole::Sdh
        } else {
            SubtitleRole::Full
        };
        SubtitleStream {
            source: SubtitleSource::External {
                path: self.file_of(row),
            },
            codec: codec.into(),
            kind: SubtitleKind::Text,
            language: Some(primary_language(&row.language)),
            title: Some(source_name(&row.source).into()),
            forced: false,
            default: false,
            role,
        }
    }
}

/// What a user is told of a site's part of a search that failed: by the
/// kind of failure only, as `client::download_error` tells a failed
/// download. What the site said (a refusal's text, a parse error) is
/// logged, not sent: it may name the account, or be anything at all.
fn failed_message(site: Source, error: &SiteError) -> String {
    match error {
        SiteError::Refused { .. } => format!("{site} refused the server's request"),
        SiteError::Unreadable(_) => format!("{site} answered with something unreadable"),
        SiteError::TooLarge(_) => format!("{site} sent more than the server takes"),
        SiteError::Unreachable(_) => format!("{site} cannot be reached"),
        SiteError::RateLimited { .. } => format!("{site} asks to wait"),
        SiteError::QuotaExhausted { .. } => format!("{site} has nothing left today"),
    }
}

/// A downloaded file not kept, as the client is told.
fn not_kept(e: FileError) -> DownloadError {
    DownloadError::InvalidInput(format!("the downloaded file was not kept: {e}"))
}

/// The name of the site a row's `source` names; the id itself for one
/// this build does not know.
pub fn source_name(source: &str) -> &str {
    Source::of(source).map_or(source, |s| s.name())
}

/// What OpenSubtitles is asked for an item: the file's hash, and the
/// item's IMDb or TMDB id (an episode's own, else its show's with the
/// season and episode), or its title and year when it has neither.
fn opensubtitles_query(
    subject: &Subject,
    hash: Option<String>,
    languages: &[String],
) -> opensubtitles::Query {
    let mut query = opensubtitles::Query {
        moviehash: hash,
        languages: languages.join(","),
        ..opensubtitles::Query::default()
    };
    let (imdb, tmdb) = (subject.imdb, subject.tmdb);
    match &subject.item {
        Item::Movie(movie) => {
            query.imdb_id = imdb;
            query.tmdb_id = tmdb.filter(|_| imdb.is_none());
            if imdb.is_none() && tmdb.is_none() {
                query.query = Some(movie.title.clone());
                query.year = movie.year;
            }
        }
        Item::Episode(episode) => {
            if imdb.is_some() || tmdb.is_some() {
                query.imdb_id = imdb;
                query.tmdb_id = tmdb.filter(|_| imdb.is_none());
            } else {
                let (show_imdb, show_tmdb) = (subject.show_imdb, subject.show_tmdb);
                query.season_number = Some(episode.season);
                query.episode_number = Some(episode.episode);
                query.parent_imdb_id = show_imdb;
                query.parent_tmdb_id = show_tmdb.filter(|_| show_imdb.is_none());
                if show_imdb.is_none() && show_tmdb.is_none() {
                    query.query = episode.show_title.clone();
                }
            }
        }
        _ => {}
    }
    query
}

/// What SubDL is asked for an item: a film's IMDb or TMDB id, else its
/// title and year; an episode's show's (SubDL lists a show's subtitles,
/// not an episode's), else its title, with the season and episode.
fn subdl_query(subject: &Subject, languages: &[String]) -> subdl::Query {
    let mut codes: Vec<String> = Vec::new();
    for code in languages.iter().map(|l| subdl::code_of(l)) {
        if !codes.contains(&code) {
            codes.push(code);
        }
    }
    let mut query = subdl::Query {
        languages: codes.join(","),
        ..subdl::Query::default()
    };
    let imdb = |n: u64| format!("tt{n:07}");
    match &subject.item {
        Item::Movie(movie) => {
            query.kind = "movie";
            query.imdb_id = subject.imdb.map(imdb);
            query.tmdb_id = subject.tmdb.filter(|_| subject.imdb.is_none());
            if subject.imdb.is_none() && subject.tmdb.is_none() {
                query.film_name = Some(movie.title.clone());
                query.year = movie.year;
            }
        }
        Item::Episode(episode) => {
            query.kind = "tv";
            query.season_number = Some(episode.season);
            query.episode_number = Some(episode.episode);
            query.imdb_id = subject.show_imdb.map(imdb);
            query.tmdb_id = subject.show_tmdb.filter(|_| subject.show_imdb.is_none());
            if subject.show_imdb.is_none() && subject.show_tmdb.is_none() {
                query.film_name = episode.show_title.clone();
            }
        }
        _ => {}
    }
    query
}

/// `row` on the wire, for `actor`.
fn wire(row: &DownloadRow, actor: &Actor) -> SubtitleDownload {
    SubtitleDownload {
        id: row.id.clone(),
        source: row.source.clone(),
        release: row.release.clone(),
        by: row.downloaded_name.clone(),
        at: row.downloaded_at,
        hash_match: row.hash_match,
        origin: match row.origin.as_str() {
            "machine_translated" => SubtitleOrigin::MachineTranslated,
            "ai" => SubtitleOrigin::Ai,
            _ => SubtitleOrigin::Human,
        },
        can_remove: actor.admin || row.downloaded_by == actor.principal.to_string(),
    }
}

fn origin_text(origin: SubtitleOrigin) -> &'static str {
    match origin {
        SubtitleOrigin::MachineTranslated => "machine_translated",
        SubtitleOrigin::Ai => "ai",
        SubtitleOrigin::Human => "human",
    }
}

/// An offer as a result.
fn result(offer: &Offer, rejected: bool, downloaded: bool) -> SubtitleResult {
    SubtitleResult {
        id: offer.result_id(),
        source: offer.source.id().into(),
        release: offer.release.clone(),
        language: offer.language.clone(),
        format: offer.format.clone(),
        fps_milli: offer
            .fps
            .map(|f| (f * 1000.0).round().clamp(0.0, 1e6) as u32),
        downloads: offer.downloads,
        rating: (offer.rating > 0.0).then(|| (offer.rating * 10.0).round() as u8),
        hearing_impaired: offer.hearing_impaired,
        origin: offer.origin,
        hash_match: offer.hash_match,
        rejected,
        downloaded,
    }
}

/// Best first: removed ones last; machine-translated and AI-made after
/// people's; found by the hash first; then by rating, then downloads. A
/// site that gives neither (SubDL) ranks its results after those that
/// were rated or downloaded, in its own order; the sort keeps the order
/// the sites were asked in for the rest.
fn order(results: &mut [SubtitleResult]) {
    results.sort_by_key(|r| {
        (
            r.rejected,
            r.origin != SubtitleOrigin::Human,
            !r.hash_match,
            std::cmp::Reverse(r.rating.unwrap_or(0)),
            std::cmp::Reverse(r.downloads),
        )
    });
}

/// The format a site's file name says: its extension, if it is one a
/// subtitle has.
fn file_format(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    [
        "srt", "ass", "ssa", "vtt", "sub", "smi", "txt", "mpl", "tmp",
    ]
    .into_iter()
    .find(|known| *known == ext)
}

/// The IMDb and TMDB ids among an item's `external_ids` (`imdb://tt…`,
/// `tmdb://…`), as numbers.
fn site_ids(external_ids: &[String]) -> (Option<u64>, Option<u64>) {
    let number = |prefix: &str| {
        external_ids.iter().find_map(|id| {
            let rest = id.strip_prefix(prefix)?;
            let digits = rest.strip_prefix("tt").unwrap_or(rest);
            digits.parse::<u64>().ok().filter(|n| *n > 0)
        })
    };
    (number("imdb://"), number("tmdb://"))
}

/// A language as the server names it: `en`, `pt-br`. Anything else is
/// refused.
pub fn language_code(language: &str) -> Result<String, DownloadError> {
    let code = language.trim().to_ascii_lowercase();
    let (primary, region) = match code.split_once('-') {
        Some((p, r)) => (p, Some(r)),
        None => (code.as_str(), None),
    };
    let letters = |s: &str, n: std::ops::RangeInclusive<usize>| {
        n.contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric())
    };
    if letters(primary, 2..=3)
        && primary.bytes().all(|b| b.is_ascii_alphabetic())
        && region.is_none_or(|r| letters(r, 2..=4))
    {
        Ok(code)
    } else {
        Err(DownloadError::InvalidInput(format!(
            "{language:?} is not a language code such as en or pt-BR"
        )))
    }
}

/// The language a track says: the primary subtag, `pt` of `pt-BR`, which
/// track preferences match.
fn primary_language(language: &str) -> String {
    language
        .split('-')
        .next()
        .unwrap_or(language)
        .to_ascii_lowercase()
}

/// Where `file` is among `descriptor`'s subtitles.
fn track_index(descriptor: &MediaDescriptor, file: &Path) -> Option<u32> {
    descriptor
        .subtitles
        .iter()
        .position(|s| matches!(&s.source, SubtitleSource::External { path } if path == file))
        .map(|i| i as u32)
}

/// `bytes` at `path`, written aside and renamed over it.
async fn write_into_place(path: &Path, bytes: &[u8]) -> Result<(), DownloadError> {
    let failed = |e: std::io::Error| DownloadError::Io(format!("cannot keep the subtitle: {e}"));
    let partial = path.with_extension("partial");
    crate::files::write_synced(&partial, bytes)
        .await
        .map_err(failed)?;
    tokio::fs::rename(&partial, path).await.map_err(failed)
}

#[cfg(test)]
mod tests;
