//! Subtitles from OpenSubtitles on demand (PX-02): searched and
//! downloaded only when someone asks, with the server's one account, and
//! kept for everyone on the server.
//!
//! A download is checked ([`spoolframe_media::subtitle_file::keep`]: made
//! UTF-8, its format read from its text, its cues read) and kept in
//! `state_dir/subtitles/downloaded`, named by an id of the server's own:
//! nothing the site said is part of a path. It becomes an external text
//! track of its media file at once, after the file's own tracks
//! ([`Prober::set_added_subtitles`]), served as any sidecar. One not
//! found by the file's hash is then synced (PX-03,
//! [`SubtitleTimings::sync`]); one found by it was made for this release.
//!
//! Searches are kept a day per item and language: opening the menu twice
//! asks the site once. A download names a result of a search kept for the
//! item, so what is downloaded is what was offered for it. Each user may
//! be held to a daily limit, and an admin may take downloading from a
//! user; searching is everyone's who can play the item.

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
    SubtitleRole, SubtitleSearch,
};

use crate::opensubtitles::{self, Account, Found, Query, Site, SiteError};
use crate::principal::Actor;
use crate::store::subtitle_downloads::{DAY_SECS, DownloadRow};
use crate::store::{Store, StoreError};
use crate::subtitle_sync::{SubtitleTimings, TrackRef};

/// The source a download's row names.
pub const SOURCE: &str = "opensubtitles";

/// What a downloaded track's title and source say.
pub const SOURCE_NAME: &str = "OpenSubtitles";

/// How long a search is kept per item and language.
pub const SEARCH_LIFE: Duration = Duration::from_secs(24 * 60 * 60);

/// The most searches kept; past it the oldest goes.
const MAX_SEARCHES: usize = 1024;

/// The most file hashes kept.
const MAX_HASHES: usize = 4096;

/// `opensubtitles { … }` as the server uses it: `None` is off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteSettings {
    pub account: Account,
    /// Searched when no language is asked for: `en`, `pt-br`.
    pub languages: Vec<String>,
    /// A user's downloads a day; 0 for no limit.
    pub daily_limit_per_user: u32,
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
    #[error("no subtitle downloads left today")]
    QuotaExhausted {
        whose: QuotaOf,
        resets_at: Option<i64>,
    },
    #[error("{0}")]
    InvalidInput(String),
    #[error(transparent)]
    Site(#[from] SiteError),
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

/// A media file's hash, and the size and modification time it was made
/// of.
type Hashed = ((u64, u128), Option<String>);

/// A search the site answered, kept.
#[derive(Debug, Clone)]
struct Kept {
    at: Instant,
    found: Arc<Vec<Found>>,
}

pub struct SubtitleDownloads {
    store: Store,
    catalog: Arc<dyn Catalog>,
    prober: Arc<Prober>,
    timings: Arc<SubtitleTimings>,
    /// `state_dir/subtitles/downloaded`.
    dir: PathBuf,
    site: Mutex<Option<(Arc<Site>, SiteSettings)>>,
    /// By item and languages.
    searches: Mutex<HashMap<(String, String), Kept>>,
    /// Each media file's hash, by its path, size and modification time.
    hashes: Mutex<HashMap<PathBuf, Hashed>>,
    /// One download at a time: a result asked for twice at once is
    /// downloaded once, and the site's quota counted in order.
    downloading: tokio::sync::Mutex<()>,
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
        settings: Option<SiteSettings>,
    ) -> std::io::Result<Self> {
        crate::files::private_dir(&dir)?;
        let this = Self {
            store,
            catalog,
            prober,
            timings,
            dir,
            site: Mutex::new(None),
            searches: Mutex::new(HashMap::new()),
            hashes: Mutex::new(HashMap::new()),
            downloading: tokio::sync::Mutex::new(()),
        };
        this.configure(settings);
        Ok(this)
    }

    /// Where the downloads are.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Use `settings` from now on (a live setting): `None` turns searching
    /// and downloading off. What was downloaded stays. A changed account
    /// signs in again; the searches kept stay.
    pub fn configure(&self, settings: Option<SiteSettings>) {
        let mut site = self.site.lock();
        let same_account = matches!((&*site, &settings), (Some((_, old)), Some(new)) if old.account == new.account);
        *site = match settings {
            None => None,
            Some(settings) if same_account => {
                let kept = site.take().expect("matched Some").0;
                Some((kept, settings))
            }
            Some(settings) => Some((Arc::new(Site::new(settings.account.clone())), settings)),
        };
    }

    /// Whether a site is set up.
    pub fn is_on(&self) -> bool {
        self.site.lock().is_some()
    }

    fn site(&self) -> Result<(Arc<Site>, SiteSettings), DownloadError> {
        self.site.lock().clone().ok_or(DownloadError::NoSource)
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

    /// Search for item `id`'s subtitles in `language`, or in the languages
    /// set, as `who`.
    pub async fn search(
        &self,
        id: &MediaId,
        language: Option<&str>,
    ) -> Result<SubtitleSearch, DownloadError> {
        let (site, settings) = self.site()?;
        let languages = match language {
            Some(language) => vec![language_code(language)?],
            None => settings.languages.clone(),
        };
        let key = (id.to_string(), languages.join(","));
        let found = match self.kept(&key) {
            Some(found) => found,
            None => {
                let query = self.query(id, &languages).await?;
                let found = Arc::new(site.search(&query).await?);
                self.keep(key, found.clone());
                found
            }
        };
        let rejected: HashSet<u64> = self
            .store
            .subtitle_rejections(&id.to_string(), SOURCE)
            .await?
            .into_iter()
            .collect();
        let downloaded: HashSet<u64> = self
            .store
            .subtitle_downloads(&id.to_string())
            .await?
            .into_iter()
            .filter(|r| r.source == SOURCE)
            .map(|r| r.file_id)
            .collect();
        let mut results: Vec<SubtitleResult> = found
            .iter()
            .map(|f| {
                result(
                    f,
                    rejected.contains(&f.file_id),
                    downloaded.contains(&f.file_id),
                )
            })
            .collect();
        order(&mut results);
        Ok(SubtitleSearch { languages, results })
    }

    fn kept(&self, key: &(String, String)) -> Option<Arc<Vec<Found>>> {
        let now = self.store.clock().now();
        self.searches
            .lock()
            .get(key)
            .filter(|k| now.duration_since(k.at) < SEARCH_LIFE)
            .map(|k| k.found.clone())
    }

    fn keep(&self, key: (String, String), found: Arc<Vec<Found>>) {
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

    /// Result `file_id` of a search kept for item `id`.
    fn offered(&self, id: &MediaId, file_id: u64) -> Option<Found> {
        let now = self.store.clock().now();
        let id = id.to_string();
        self.searches
            .lock()
            .iter()
            .filter(|((item, _), k)| *item == id && now.duration_since(k.at) < SEARCH_LIFE)
            .find_map(|(_, k)| k.found.iter().find(|f| f.file_id == file_id).cloned())
    }

    /// What to ask the site for item `id`: the file's hash, and the item's
    /// IMDb or TMDB id (an episode's own, else its show's with the season
    /// and episode), or its title and year when it has neither.
    async fn query(&self, id: &MediaId, languages: &[String]) -> Result<Query, DownloadError> {
        let found = self.catalog.item(id).await?;
        let media = self.catalog.media_path(id).await?;
        let mut query = Query {
            moviehash: self.hash(&media).await,
            languages: languages.join(","),
            ..Query::default()
        };
        let (imdb, tmdb) = site_ids(&found.external_ids);
        match &found.item {
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
                    let show = self
                        .catalog
                        .external_ids(&episode.show_id)
                        .await
                        .unwrap_or_default();
                    let (show_imdb, show_tmdb) = site_ids(&show);
                    query.season_number = Some(episode.season);
                    query.episode_number = Some(episode.episode);
                    query.parent_imdb_id = show_imdb;
                    query.parent_tmdb_id = show_tmdb.filter(|_| show_imdb.is_none());
                    if show_imdb.is_none() && show_tmdb.is_none() {
                        query.query = episode.show_title.clone();
                    }
                }
            }
            _ => {
                return Err(DownloadError::InvalidInput(
                    "only a film or an episode has subtitles to find".into(),
                ));
            }
        }
        Ok(query)
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

    /// Download result `file_id` of a search for item `id`, as `who`.
    pub async fn download(
        &self,
        id: &MediaId,
        file_id: u64,
        who: &Asker,
    ) -> Result<SubtitleDownloaded, DownloadError> {
        let (site, settings) = self.site()?;
        let _one = self.downloading.lock().await;
        let principal = who.actor.principal.to_string();
        let limit = settings.daily_limit_per_user;
        if let Some(row) = self
            .store
            .subtitle_download_of(&id.to_string(), SOURCE, file_id)
            .await?
        {
            let user_remaining = self.user_remaining(&principal, limit).await?;
            return self.answer(&row, who, None, user_remaining).await;
        }
        let found = self.offered(id, file_id).ok_or(DownloadError::NotFound(
            "a search result for this item; search again",
        ))?;
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
            });
        }
        let media = self.catalog.media_path(id).await?;
        // A format the server reads is taken as it is; any other is asked
        // for as SRT, which the site converts it to.
        let as_it_is = found
            .file_name
            .as_deref()
            .and_then(file_format)
            .is_some_and(|f| matches!(f, "srt" | "ass" | "ssa" | "vtt"));
        let got = site
            .download(file_id, (!as_it_is).then_some("srt"))
            .await
            .map_err(|e| match e {
                SiteError::QuotaExhausted { resets_at } => DownloadError::QuotaExhausted {
                    whose: QuotaOf::Account,
                    resets_at,
                },
                other => DownloadError::Site(other),
            })?;
        let kept = subtitle_file::keep(&got.bytes).map_err(|e| match e {
            FileError::TooLarge | FileError::NotText | FileError::NotSubtitle(_) => {
                DownloadError::InvalidInput(format!("the downloaded file was not kept: {e}"))
            }
        })?;
        let row = DownloadRow {
            id: uuid::Uuid::new_v4().simple().to_string(),
            media_id: id.to_string(),
            media_path: media.display().to_string(),
            source: SOURCE.into(),
            file_id,
            language: found.language.clone(),
            release: found.release.chars().take(300).collect(),
            format: kept.extension().into(),
            hearing_impaired: found.hearing_impaired,
            origin: origin_text(origin(&found)).into(),
            hash_match: found.hash_match,
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
            file_id,
            download = %row.id,
            encoding = kept.encoding,
            cues = kept.cues,
            hash_match = row.hash_match,
            by = %who.name,
            "downloaded a subtitle from OpenSubtitles"
        );
        self.attach(&media).await?;
        let user_remaining = self.user_remaining(&principal, limit).await?;
        let answer = self
            .answer(&row, who, got.remaining, user_remaining)
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

    /// `row`'s track, as the media file lists it.
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
            title: Some(SOURCE_NAME.into()),
            forced: false,
            default: false,
            role,
        }
    }
}

/// `row` on the wire, for `actor`.
fn wire(row: &DownloadRow, actor: &Actor) -> SubtitleDownload {
    SubtitleDownload {
        id: row.id.clone(),
        source: SOURCE_NAME.into(),
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

fn origin(found: &Found) -> SubtitleOrigin {
    if found.ai_translated {
        SubtitleOrigin::Ai
    } else if found.machine_translated {
        SubtitleOrigin::MachineTranslated
    } else {
        SubtitleOrigin::Human
    }
}

fn origin_text(origin: SubtitleOrigin) -> &'static str {
    match origin {
        SubtitleOrigin::MachineTranslated => "machine_translated",
        SubtitleOrigin::Ai => "ai",
        SubtitleOrigin::Human => "human",
    }
}

/// A found file as a result.
fn result(found: &Found, rejected: bool, downloaded: bool) -> SubtitleResult {
    SubtitleResult {
        file_id: found.file_id,
        release: found.release.clone(),
        language: found.language.clone(),
        format: found
            .file_name
            .as_deref()
            .and_then(file_format)
            .map(str::to_owned),
        fps_milli: found
            .fps
            .map(|f| (f * 1000.0).round().clamp(0.0, 1e6) as u32),
        downloads: found.downloads,
        rating: (found.rating > 0.0).then(|| (found.rating * 10.0).round() as u8),
        hearing_impaired: found.hearing_impaired,
        origin: origin(found),
        hash_match: found.hash_match,
        rejected,
        downloaded,
    }
}

/// Best first: removed ones last; machine-translated and AI-made after
/// people's; found by the hash first; then by rating, then downloads.
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

/// A language as the site takes it: `en`, `pt-br`. Anything else is
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
