//! `PlexCatalog`: reads the library from a Plex Media Server and turns it
//! into Spoolframe's own media model. Plex supplies metadata only; it is
//! never in the media path.
//!
//! It talks to the PMS directly over the LAN and never to plex.tv, so the
//! catalog keeps working when the Internet is down (ARCHITECTURE §32). It
//! reads on demand and does not copy the Plex database. Only [`account`],
//! signing in with Plex at setup, reaches plex.tv; and an image the PMS
//! redirects to Plex's image proxy ([`redirect`]) is fetched there.

pub mod account;
mod bif;
pub mod check;
mod convert;
pub mod gdm;
pub mod inspect;
pub mod paths;
pub mod playlists;
mod redirect;
mod search;
pub mod stats;
mod watch;
mod wire;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::StatusCode;
use spoolframe_catalog::{
    Catalog, CatalogError, Found, Image, ItemPage, ItemWithChildren, Page, PreviewFrames,
    SearchQuery, UnavailableFile,
};
use spoolframe_types::{
    ArtworkId, CalendarDate, Item, Library, LibraryId, LibraryKind, MediaId, MediaKind, Namespace,
    PersonId,
};
use url::Url;

pub use crate::check::{Failures, LibraryPaths, Scope};
pub use crate::paths::{PathMap, PathMapping};
pub use crate::stats::{RequestCounts, RequestStats};
pub use crate::watch::PlexWatch;

use crate::convert::ConvertError;
use crate::wire::{Envelope, MediaContainer, Metadata, SectionKey};

/// The parsers of what a PMS sends, for the fuzz targets (`fuzz/`,
/// PX-10): they are private otherwise. Not an API; only the `fuzzing`
/// feature builds it.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub mod fuzzing {
    use crate::{bif, convert, wire};

    /// A PMS JSON response, decoded and converted as the catalog does:
    /// its sections and items, with their ids, markers and files.
    pub fn json(body: &[u8]) {
        let Ok(envelope) = serde_json::from_slice::<wire::Envelope>(body) else {
            return;
        };
        let container = envelope.container;
        for directory in &container.directories {
            let _ = convert::library(directory);
        }
        for metadata in &container.metadata {
            let _ = convert::is_modelled(&metadata.kind);
            let _ = convert::item(metadata);
            let _ = convert::external_ids(metadata);
            let _ = convert::markers(metadata);
            let _ = convert::media_files(metadata);
        }
    }

    /// A preview index (BIF), as the catalog reads it.
    pub fn bif(bytes: &[u8]) -> Result<usize, String> {
        bif::frames(bytes).map(|frames| frames.len())
    }
}

/// How long one PMS call may take. Browsing waits on it, so a stuck PMS
/// should fail fast rather than hang the client.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// A [`Catalog`] backed by a Plex Media Server.
pub struct PlexCatalog {
    /// The PMS base URL, without a trailing slash.
    base: String,
    /// Sent as `X-Plex-Token`. `None` for a PMS that allows LAN clients
    /// without auth.
    token: Option<String>,
    http: reqwest::Client,
    /// What an image redirected off the PMS is fetched with: its names
    /// resolve to public addresses only ([`redirect::PublicOnly`]).
    outside: reqwest::Client,
    /// Which hosts off the PMS an image's redirect may go to.
    hosts: redirect::Outside,
    /// Plex paths → local paths. Applied here and only here: nothing
    /// outside this crate sees a Plex-side path.
    paths: PathMap,
    /// The library sections served (P3-11), by key; `None` serves every
    /// one. An item in another is [`CatalogError::NotServed`], and left
    /// out of listings, search and recently added.
    sections: Option<Arc<HashSet<String>>>,
    /// Every request to this PMS, from this catalog and the views of it
    /// made from it (P3-14).
    stats: Arc<RequestStats>,
}

/// An item fetched by ID: as Plex sent it, converted, and the library it
/// is in.
struct Fetched {
    metadata: Metadata,
    item: Item,
    library: Option<String>,
    /// The key of the section it is in, when Plex says.
    section: Option<String>,
    call: Call<'static>,
}

/// An item with when it was added (Unix s), for merging recently added
/// from several Plex servers (P3-11).
pub type Dated = (Option<i64>, Item);

impl fmt::Debug for PlexCatalog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlexCatalog")
            .field("base", &self.base)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("paths", &self.paths)
            .field("sections", &self.sections)
            .finish_non_exhaustive()
    }
}

/// One PMS request: what is fetched, and which item it is about.
struct Call<'a> {
    path: String,
    query: Vec<(&'a str, &'a str)>,
    headers: Vec<(&'static str, String)>,
    /// The Spoolframe ID the call is about, for errors.
    subject: Option<String>,
    /// Whether a redirect is followed ([`redirect`]): for an image only.
    redirects: bool,
}

impl<'a> Call<'a> {
    fn get(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            query: Vec::new(),
            headers: Vec::new(),
            subject: None,
            redirects: false,
        }
    }

    fn about(mut self, subject: impl ToString) -> Self {
        self.subject = Some(subject.to_string());
        self
    }

    fn describe(&self) -> String {
        format!("GET {}", self.path)
    }
}

impl PlexCatalog {
    /// A catalog reading from the PMS at `base`, e.g.
    /// `http://192.168.1.20:32400`. A path prefix (a reverse proxy) is kept.
    pub fn new(base: Url, token: Option<String>) -> Result<Self, CatalogError> {
        let base = base.as_str().trim_end_matches('/').to_owned();
        let hosts = redirect::Outside::default();
        Ok(Self {
            http: http_client(&base, None)?,
            outside: http_client(&base, Some(redirect::PublicOnly(hosts)))?,
            hosts,
            base,
            token,
            paths: PathMap::default(),
            sections: None,
            stats: Arc::default(),
        })
    }

    /// An image's redirect off the PMS may go to any host, over HTTP too:
    /// for a test whose image proxy is on loopback. The token is still
    /// never sent there.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn reaching_inner_hosts(mut self) -> Self {
        self.hosts = redirect::Outside { inner: true };
        self.outside = http_client(&self.base, Some(redirect::PublicOnly(self.hosts)))
            .expect("the client was built once already");
        self
    }

    /// Map the media paths Plex reports with `paths`.
    pub fn with_paths(mut self, paths: PathMap) -> Self {
        self.paths = paths;
        self
    }

    /// The same PMS and token, with other path mappings: for checking
    /// mappings before they are saved (P3-10).
    pub fn trying_paths(&self, paths: PathMap) -> Self {
        Self {
            base: self.base.clone(),
            token: self.token.clone(),
            http: self.http.clone(),
            outside: self.outside.clone(),
            hosts: self.hosts,
            paths,
            sections: self.sections.clone(),
            stats: self.stats.clone(),
        }
    }

    /// Serve only the library sections with these keys (P3-11); `None`
    /// serves every one, as a catalog does by default.
    pub fn serving(mut self, sections: Option<HashSet<String>>) -> Self {
        self.sections = sections.map(Arc::new);
        self
    }

    /// The same PMS with every section: what an admin chooses libraries
    /// from, and what the watch sync reads (PX-04 syncs the account's whole
    /// state).
    pub fn unrestricted(&self) -> Self {
        Self {
            sections: None,
            ..self.trying_paths(self.paths.clone())
        }
    }

    /// Whether the section with key `section` is served. An item whose
    /// section Plex does not say is served only by an unrestricted catalog.
    fn serves(&self, section: Option<&SectionKey>) -> bool {
        match (&self.sections, section) {
            (None, _) => true,
            (Some(served), Some(key)) => served.contains(&key.0),
            (Some(_), None) => false,
        }
    }

    /// The same PMS, every section, as the account `token` signs in
    /// (P3-12): each user's watch state is their own account's.
    pub fn with_token(&self, token: Option<String>) -> Self {
        Self {
            token,
            ..self.unrestricted()
        }
    }

    /// The PMS's machine identifier, which plex.tv's resources list knows
    /// it by (P3-12). `/identity` answers without a token.
    pub async fn machine_id(&self) -> Result<Option<String>, CatalogError> {
        Ok(self
            .fetch(&Call::get("/identity"))
            .await?
            .machine_identifier)
    }

    /// The key of the library section `id` is in, served or not; `None`
    /// when Plex does not say.
    pub async fn section_of(&self, id: &MediaId) -> Result<Option<String>, CatalogError> {
        Ok(self.unrestricted().metadata(id, false).await?.section)
    }

    /// Where the PMS is reached, as configured: no token, no trailing
    /// slash.
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// What this PMS's requests did so far (P3-14).
    pub fn request_counts(&self) -> RequestCounts {
        self.stats.counts()
    }

    /// Tell `observer` how long each request to this PMS takes, from this
    /// catalog and every view made of it. Only the first one is kept.
    pub fn observe_requests(&self, observer: stats::Observer) {
        self.stats.observe(observer);
    }

    /// What the PMS calls itself; asking is also how to tell it is up.
    pub async fn server_name(&self) -> Result<Option<String>, CatalogError> {
        Ok(self.identity().await?.0)
    }

    /// What the PMS calls itself, and its version.
    pub async fn identity(&self) -> Result<(Option<String>, Option<String>), CatalogError> {
        let root = self.fetch(&Call::get("/")).await?;
        Ok((root.friendly_name, root.version))
    }

    async fn fetch(&self, call: &Call<'_>) -> Result<MediaContainer, CatalogError> {
        let body = self
            .get_bytes(call, "application/json", MAX_JSON_BYTES)
            .await?
            .0;
        serde_json::from_slice::<Envelope>(&body)
            .map(|e| e.container)
            .map_err(|e| self.backend(call, None, format!("unreadable response: {e}")))
    }

    /// One GET, with the token; the body (at most `limit` bytes) and its
    /// content type. Status codes become errors here. Each is counted and
    /// timed (P3-14), in a span of its own that names the path, never the
    /// token.
    async fn get_bytes(
        &self,
        call: &Call<'_>,
        accept: &str,
        limit: usize,
    ) -> Result<(Vec<u8>, Option<String>), CatalogError> {
        use tracing::Instrument as _;
        let span = tracing::info_span!("plex request", path = %call.path);
        let started = std::time::Instant::now();
        let result = self.send(call, accept, limit).instrument(span).await;
        let failed = matches!(&result, Err(e) if !matches!(e, CatalogError::NotFound(_)));
        self.stats.record(started.elapsed(), failed);
        result
    }

    async fn send(
        &self,
        call: &Call<'_>,
        accept: &str,
        limit: usize,
    ) -> Result<(Vec<u8>, Option<String>), CatalogError> {
        // One request's time for the whole chain of redirects, the body
        // too: each hop has what is left of it.
        let deadline = std::time::Instant::now() + REQUEST_TIMEOUT;
        let pms = self.url(call)?;
        let mut url = pms.clone();
        // Once off the PMS, nothing after is sent the token.
        let mut on_pms = true;
        let mut hops = 0;
        let resp = loop {
            let client = if on_pms { &self.http } else { &self.outside };
            let mut req = client
                .get(url.clone())
                .header(reqwest::header::ACCEPT, accept)
                .timeout(deadline.saturating_duration_since(std::time::Instant::now()));
            if on_pms {
                if let Some(token) = &self.token {
                    req = req.header("X-Plex-Token", token);
                }
                for (name, value) in &call.headers {
                    req = req.header(*name, value);
                }
            }
            let resp = req
                .send()
                .await
                .map_err(|source| CatalogError::Unreachable {
                    url: self.reached(on_pms, &url),
                    call: call.describe(),
                    source: Box::new(source),
                })?;
            if !(call.redirects && resp.status().is_redirection()) {
                break resp;
            }
            hops += 1;
            if hops > redirect::MAX_REDIRECTS {
                return Err(self.backend(
                    call,
                    None,
                    format!("more than {} redirects", redirect::MAX_REDIRECTS),
                ));
            }
            let next = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|location| url.join(location).ok())
                .ok_or_else(|| {
                    let status = resp.status();
                    self.backend(call, None, format!("HTTP {status} names nowhere to go"))
                })?;
            on_pms = on_pms && redirect::same_origin(&next, &pms);
            if !on_pms && let Some(why) = self.hosts.refused(&next, self.token.as_deref()) {
                let to = next.host_str().unwrap_or("nowhere");
                return Err(self.backend(
                    call,
                    None,
                    format!("a redirect to {to} is refused: {why}"),
                ));
            }
            url = next;
        };
        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Err(CatalogError::NotFound(
                call.subject.clone().unwrap_or_else(|| call.path.clone()),
            ));
        }
        if on_pms && (status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN) {
            let message = match self.token {
                Some(_) => format!("the PMS rejected the token ({status})"),
                None => format!("the PMS requires a token and none is configured ({status})"),
            };
            return Err(self.backend(call, None, message));
        }
        if !status.is_success() {
            let message = if on_pms {
                format!("unexpected HTTP {status}")
            } else {
                format!("unexpected HTTP {status} from {}", origin(&url))
            };
            return Err(self.backend(call, None, message));
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        // Read up to `limit`, not whole and then measured (PX-09 finding
        // 26): what a PMS sends within the request's time is otherwise
        // all held.
        let mut resp = resp;
        let mut body = Vec::new();
        loop {
            let chunk = resp
                .chunk()
                .await
                .map_err(|source| CatalogError::Unreachable {
                    url: self.reached(on_pms, &url),
                    call: call.describe(),
                    source: Box::new(source),
                })?;
            let Some(chunk) = chunk else {
                break;
            };
            if body.len() + chunk.len() > limit {
                return Err(self.backend(
                    call,
                    None,
                    format!("an answer of more than {limit} bytes is over the limit"),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok((body, content_type))
    }

    /// Where a request went, for errors: the PMS as configured, or the
    /// origin of `url` off it.
    fn reached(&self, on_pms: bool, url: &Url) -> String {
        if on_pms {
            self.base.clone()
        } else {
            origin(url)
        }
    }

    /// Where `call` is asked of the PMS: the base, its path and query.
    fn url(&self, call: &Call<'_>) -> Result<Url, CatalogError> {
        let mut url = Url::parse(&format!("{}{}", self.base, call.path))
            .map_err(|e| self.backend(call, None, format!("not a URL: {e}")))?;
        if !call.query.is_empty() {
            url.query_pairs_mut().extend_pairs(&call.query);
        }
        Ok(url)
    }

    fn backend(&self, call: &Call<'_>, item: Option<String>, message: String) -> CatalogError {
        CatalogError::Backend {
            url: self.base.clone(),
            call: call.describe(),
            item: item.or_else(|| call.subject.clone()),
            message,
        }
    }

    fn converted(&self, call: &Call<'_>, e: ConvertError) -> CatalogError {
        let item = e.item.map(|key| format!("ratingKey {key}"));
        self.backend(call, item, e.message)
    }

    /// The single item at `/library/metadata/{key}`, checked to be the
    /// kind the ID says, both as Plex sent it and converted. Plex leaves
    /// the item's markers out unless `with_markers` asks for them.
    async fn metadata(&self, id: &MediaId, with_markers: bool) -> Result<Fetched, CatalogError> {
        let key = plex_key(id.namespace(), id.key(), id)?;
        let mut call = Call::get(format!("/library/metadata/{key}")).about(id);
        if with_markers {
            call.query = vec![("includeMarkers", "1")];
        }
        let mut container = self.fetch(&call).await?;
        if container.metadata.is_empty() {
            return Err(CatalogError::NotFound(id.to_string()));
        }
        let metadata = container.metadata.swap_remove(0);
        let section = metadata
            .library_section_id
            .clone()
            .or(container.library_section_id);
        if !self.serves(section.as_ref()) {
            return Err(CatalogError::NotServed(id.to_string()));
        }
        let mut item = convert::item(&metadata).map_err(|e| self.converted(&call, e))?;
        // Only an item's own response has its cast, with the ids people
        // are listed by: here, not in `convert::item`, which listings use.
        convert::add_cast(&mut item, &metadata);
        if item.id() != id {
            return Err(self.backend(
                &call,
                None,
                format!("the PMS returned {}, not {id}", item.id()),
            ));
        }
        Ok(Fetched {
            metadata,
            item,
            library: container.library_section_title,
            section: section.map(|s| s.0),
            call,
        })
    }

    /// The day the last of the show's episodes in Plex's order first
    /// aired, from one episode asked of its `allLeaves`: the last of its
    /// `leafCount`. Plex says nowhere whether a show has ended. It is
    /// extra, so a failure leaves it out; anything but a show has none.
    async fn last_aired(&self, id: &MediaId, fetched: &Fetched) -> Option<CalendarDate> {
        if id.kind() != MediaKind::Show {
            return None;
        }
        let last = fetched.metadata.leaf_count?.checked_sub(1)?;
        let mut call = Call::get(format!("{}/allLeaves", fetched.call.path)).about(id);
        call.headers = vec![
            ("X-Plex-Container-Start", last.to_string()),
            ("X-Plex-Container-Size", "1".to_owned()),
        ];
        match self.fetch(&call).await {
            Ok(container) => container
                .metadata
                .first()
                .and_then(|episode| episode.originally_available_at),
            Err(e) => {
                tracing::warn!(%id, error = %e, "a show is served without its last airing");
                None
            }
        }
    }

    /// `tracks`, an album's listing, with each track's streams: Plex gives
    /// a track's bit depth and sample rate only on the track asked for by
    /// key, and one request asks for them all. Without them the tracks
    /// are still whole, so a failure only loses those two numbers.
    async fn with_streams(&self, album: &MediaId, tracks: &mut [Metadata]) {
        // Plex's keys are numbers; anything else is left out of the path.
        let keys: Vec<&str> = tracks
            .iter()
            .map(|m| m.rating_key.as_str())
            .filter(|k| !k.is_empty() && k.bytes().all(|b| b.is_ascii_digit()))
            .collect();
        if keys.is_empty() {
            return;
        }
        let call = Call::get(format!("/library/metadata/{}", keys.join(","))).about(album);
        let full = match self.fetch(&call).await {
            Ok(container) => container.metadata,
            Err(e) => {
                tracing::warn!(
                    %album,
                    error = %e,
                    "an album's tracks are listed without their bit depth and sample rate"
                );
                return;
            }
        };
        let mut full: HashMap<String, Metadata> = full
            .into_iter()
            .map(|m| (m.rating_key.clone(), m))
            .collect();
        for track in tracks {
            if let Some(found) = full.remove(&track.rating_key) {
                track.media = found.media;
            }
        }
    }

    /// Every item in a listing, or the first one that does not convert.
    fn items_of(&self, call: &Call<'_>, metadata: &[Metadata]) -> Result<Vec<Item>, CatalogError> {
        metadata
            .iter()
            .map(convert::item)
            .collect::<Result<_, _>>()
            .map_err(|e| self.converted(call, e))
    }
}

/// One library section of a PMS, whatever it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// Its key, as `/library/sections` lists it.
    pub key: String,
    pub title: String,
    /// What it holds, or `None` for what Spoolframe does not serve
    /// (photos, other videos).
    pub kind: Option<LibraryKind>,
    /// Plex's `type`, for naming what it holds when `kind` is `None`.
    pub plex_type: Option<String>,
    /// Its folders, as Plex sees them.
    pub roots: Vec<PathBuf>,
}

impl PlexCatalog {
    /// Every library section of the PMS, served or not, in Plex's order:
    /// what an admin chooses libraries from (P3-11).
    pub async fn sections(&self) -> Result<Vec<Section>, CatalogError> {
        let call = Call::get("/library/sections");
        let container = self.fetch(&call).await?;
        container
            .directories
            .iter()
            .map(|d| {
                let kind = convert::library(d)
                    .map_err(|e| self.converted(&call, e))?
                    .map(|l| l.kind);
                Ok(Section {
                    key: d.key.clone(),
                    title: d.title.clone(),
                    kind,
                    plex_type: d.kind.clone(),
                    roots: d.locations.iter().map(|l| PathBuf::from(&l.path)).collect(),
                })
            })
            .collect()
    }

    /// How many top-level items section `key` has, served or not.
    pub async fn section_size(&self, key: &str) -> Result<u32, CatalogError> {
        let key = plex_key(Namespace::Plex, key, &key)?;
        let mut call = Call::get(format!("/library/sections/{key}/all"));
        call.headers = vec![
            ("X-Plex-Container-Start", "0".to_owned()),
            ("X-Plex-Container-Size", "0".to_owned()),
        ];
        let container = self.fetch(&call).await?;
        container
            .total_size
            .ok_or_else(|| self.backend(&call, None, "listing has no totalSize".into()))
    }
}

/// How many more recently added items are asked for when some sections
/// are not served, so that filtering them out still leaves enough.
const RECENT_WINDOW: u32 = 4;

impl PlexCatalog {
    /// The most recently added items in the served sections, newest first,
    /// at most `limit`, each with when it was added.
    pub async fn recently_added_dated(&self, limit: u32) -> Result<Vec<Dated>, CatalogError> {
        // `/library/recentlyAdded` groups new episodes into their seasons,
        // which a section's own listing does not: so it is asked for more
        // and filtered here, rather than asked per section.
        let window = match self.sections {
            None => limit,
            Some(_) => limit.saturating_mul(RECENT_WINDOW),
        };
        let mut call = Call::get("/library/recentlyAdded");
        call.headers = vec![
            ("X-Plex-Container-Start", "0".to_owned()),
            ("X-Plex-Container-Size", window.to_string()),
        ];
        let container = self.fetch(&call).await?;
        // Plex may list kinds Spoolframe does not model (photos, clips);
        // those are skipped.
        let mut items = container
            .metadata
            .iter()
            .filter(|m| convert::is_modelled(&m.kind))
            .filter(|m| {
                self.serves(
                    m.library_section_id
                        .as_ref()
                        .or(container.library_section_id.as_ref()),
                )
            })
            .map(|m| convert::item(m).map(|item| (m.added_at, item)))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| self.converted(&call, e))?;
        items.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(items)
    }
}

/// The Plex key inside a Spoolframe ID. Plex keys are numeric; anything
/// else cannot exist in Plex, and must not be spliced into a request path.
fn plex_key<'k>(
    namespace: Namespace,
    key: &'k str,
    id: &impl ToString,
) -> Result<&'k str, CatalogError> {
    if namespace != Namespace::Plex {
        return Err(CatalogError::ForeignId(id.to_string()));
    }
    if key.is_empty() || !key.bytes().all(|b| b.is_ascii_digit()) {
        return Err(CatalogError::NotFound(id.to_string()));
    }
    Ok(key)
}

impl PlexCatalog {
    /// The local path for a Plex-reported `file` of `id`, checked to be a
    /// readable file.
    fn resolve(
        &self,
        id: &MediaId,
        library: Option<&str>,
        file: &Path,
    ) -> Result<PathBuf, CatalogError> {
        let mapped = self.paths.map(library, file);
        match mapped.problem() {
            None => Ok(mapped.local),
            Some(reason) => Err(CatalogError::MediaFileUnavailable(Box::new(
                UnavailableFile {
                    id: id.clone(),
                    reported: file.to_owned(),
                    mapping: mapped.mapping.map(ToString::to_string),
                    local: mapped.local,
                    reason,
                },
            ))),
        }
    }
}

/// A client for the PMS at `base` (for errors). It follows no redirect
/// (PX-09 B-2): reqwest keeps custom headers across hosts, so one would
/// hand X-Plex-Token to wherever a PMS, or whoever answers as one,
/// points. An image's are followed by hand ([`redirect`]), those off the
/// PMS with a client whose names `resolver` resolves.
fn http_client(
    base: &str,
    resolver: Option<redirect::PublicOnly>,
) -> Result<reqwest::Client, CatalogError> {
    let mut builder = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none());
    if let Some(resolver) = resolver {
        builder = builder.dns_resolver(resolver);
    }
    builder.build().map_err(|e| CatalogError::Backend {
        url: base.to_owned(),
        call: "build HTTP client".into(),
        item: None,
        message: e.to_string(),
    })
}

/// `url`'s origin, for errors: never its path nor its query.
fn origin(url: &Url) -> String {
    url.origin().ascii_serialization()
}

/// The largest image passed on; posters and backgrounds are far smaller.
pub(crate) const MAX_IMAGE_BYTES: usize = 32 << 20;

/// The most pixels a side of a person's photo is asked for: the largest
/// artwork size served (`Large`); the artwork proxy resizes from it.
const PERSON_PHOTO_SIDE: &str = "1280";

/// The most of a JSON answer read: a page of a library, or a show with its
/// seasons, is well under a megabyte.
pub(crate) const MAX_JSON_BYTES: usize = 64 << 20;

/// The most of a preview index read: a film's is about 3.6 MB an hour at
/// a frame every 2 s.
const MAX_INDEX_BYTES: usize = 256 << 20;

/// Whether `key` is an image path as Plex hands them out:
/// `/library/metadata/<digits>/(thumb|art)/<digits>`.
fn is_image_path(key: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    matches!(
        key.split('/').collect::<Vec<_>>().as_slice(),
        ["", "library", "metadata", item, "thumb" | "art", stamp] if digits(item) && digits(stamp)
    )
}

/// The kinds whose children `/library/metadata/{key}/children` lists.
fn has_children(kind: MediaKind) -> bool {
    matches!(
        kind,
        MediaKind::Show | MediaKind::Season | MediaKind::Artist | MediaKind::Album
    )
}

/// The kinds Plex finds intros and credits in.
fn has_markers(kind: MediaKind) -> bool {
    matches!(kind, MediaKind::Movie | MediaKind::Episode)
}

#[async_trait]
impl Catalog for PlexCatalog {
    async fn libraries(&self) -> Result<Vec<Library>, CatalogError> {
        let call = Call::get("/library/sections");
        let container = self.fetch(&call).await?;
        container
            .directories
            .iter()
            .filter(|d| self.serves(Some(&SectionKey(d.key.clone()))))
            .filter_map(|d| convert::library(d).transpose())
            .collect::<Result<_, _>>()
            .map_err(|e| self.converted(&call, e))
    }

    async fn items(&self, library: &LibraryId, page: Page) -> Result<ItemPage, CatalogError> {
        let key = plex_key(library.namespace(), library.key(), library)?;
        if !self.serves(Some(&SectionKey(key.to_owned()))) {
            return Err(CatalogError::NotServed(library.to_string()));
        }
        let mut call = Call::get(format!("/library/sections/{key}/all")).about(library);
        call.headers = vec![
            ("X-Plex-Container-Start", page.offset.to_string()),
            ("X-Plex-Container-Size", page.limit.to_string()),
        ];
        let container = self.fetch(&call).await?;
        let total = container
            .total_size
            .ok_or_else(|| self.backend(&call, None, "listing has no totalSize".into()))?;
        let items = self.items_of(&call, &container.metadata)?;
        Ok(ItemPage { items, total })
    }

    async fn item(&self, id: &MediaId) -> Result<ItemWithChildren, CatalogError> {
        let mut fetched = self.metadata(id, has_markers(id.kind())).await?;
        if !has_children(id.kind()) {
            return self.with_children(&fetched, None, &[]);
        }
        // A show's last airing is one more request, asked beside its
        // seasons rather than after them.
        let (children, last_aired) = futures_util::future::join(
            self.children_of(id, &fetched.call.path, false),
            self.last_aired(id, &fetched),
        )
        .await;
        let (call, mut children, _) = children?;
        if let Item::Show(show) = &mut fetched.item {
            show.last_aired = last_aired;
        }
        if id.kind() != MediaKind::Album {
            return self.with_children(&fetched, Some(&call), &children);
        }
        self.with_streams(id, &mut children).await;
        let mut got = self.with_children(&fetched, Some(&call), &children)?;
        let count = match &got.item {
            Item::Album(album) => album.track_count,
            _ => None,
        };
        for child in &mut got.children {
            if let Item::Track(track) = child {
                track.album_track_count = count;
            }
        }
        Ok(got)
    }

    /// From the show's `allLeaves`: one request, where its seasons are two
    /// each and the show three.
    async fn episodes(&self, show: &MediaId) -> Result<Vec<Item>, CatalogError> {
        let key = plex_key(show.namespace(), show.key(), show)?;
        let call = Call::get(format!("/library/metadata/{key}/allLeaves")).about(show);
        let container = self.fetch(&call).await?;
        if !self.serves(container.library_section_id.as_ref()) {
            return Err(CatalogError::NotServed(show.to_string()));
        }
        // A key Plex gave to an item of another kind since lists no
        // episodes of this show.
        let episodes = container
            .metadata
            .iter()
            .filter(|m| convert::media_kind(&m.kind) == Some(MediaKind::Episode))
            .map(convert::item)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| self.converted(&call, e))?;
        Ok(episodes
            .into_iter()
            .filter(|e| matches!(e, Item::Episode(e) if e.show_id == *show))
            .collect())
    }

    /// One listing of the section's tracks (`type=10`), page by page,
    /// where walking its albums is a request each (PX-43).
    async fn tracks(&self, library: &LibraryId) -> Result<Vec<Item>, CatalogError> {
        let key = plex_key(library.namespace(), library.key(), library)?;
        if !self.serves(Some(&SectionKey(key.to_owned()))) {
            return Err(CatalogError::NotServed(library.to_string()));
        }
        let listed = self.listed(key, "10", &[]).await?;
        let call = Call::get(format!("/library/sections/{key}/all")).about(library);
        Ok(self
            .items_of(&call, &listed)?
            .into_iter()
            .filter(|i| matches!(i, Item::Track(_)))
            .collect())
    }

    async fn external_ids(&self, id: &MediaId) -> Result<Vec<String>, CatalogError> {
        let key = plex_key(id.namespace(), id.key(), id)?;
        let call = Call::get(format!("/library/metadata/{key}")).about(id);
        let container = self.fetch(&call).await?;
        // Plex's keys are one sequence for all kinds: a key it gave to an
        // item of another kind since is not this item.
        match container.metadata.first() {
            Some(m)
                if !self.serves(
                    m.library_section_id
                        .as_ref()
                        .or(container.library_section_id.as_ref()),
                ) =>
            {
                Err(CatalogError::NotServed(id.to_string()))
            }
            Some(m) if convert::media_kind(&m.kind) == Some(id.kind()) => {
                Ok(convert::external_ids(m))
            }
            _ => Err(CatalogError::NotFound(id.to_string())),
        }
    }

    async fn find_external(
        &self,
        kind: MediaKind,
        external_ids: &[String],
    ) -> Result<Option<MediaId>, CatalogError> {
        self.find_guid(kind, external_ids).await
    }

    async fn recently_added(&self, limit: u32) -> Result<Vec<Item>, CatalogError> {
        Ok(self
            .recently_added_dated(limit)
            .await?
            .into_iter()
            .map(|(_, item)| item)
            .collect())
    }

    async fn search(&self, query: &SearchQuery) -> Result<Found, CatalogError> {
        self.find(query).await
    }

    async fn person_items(&self, id: &PersonId) -> Result<Vec<Item>, CatalogError> {
        self.person_items(id).await
    }

    /// The `sd` preview index of the part a playback plays (the first of
    /// the first version), if Plex has made one.
    async fn preview_frames(&self, id: &MediaId) -> Result<Option<PreviewFrames>, CatalogError> {
        if !matches!(id.kind(), MediaKind::Movie | MediaKind::Episode) {
            return Ok(None);
        }
        let fetched = self.metadata(id, false).await?;
        let Some(part) = fetched.metadata.media.first().and_then(|m| m.parts.first()) else {
            return Ok(None);
        };
        let indexed = part
            .indexes
            .as_deref()
            .is_some_and(|i| i.split(',').any(|kind| kind.trim() == "sd"));
        let (Some(part), true) = (part.id, indexed) else {
            return Ok(None);
        };
        let call = Call::get(format!("/library/parts/{part}/indexes/sd")).about(id);
        let bytes = match self.get_bytes(&call, "*/*", MAX_INDEX_BYTES).await {
            Ok((bytes, _)) => bytes,
            // Listed, but gone (being made again): as if there were none.
            Err(CatalogError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        let frames = bif::frames(&bytes)
            .map_err(|e| self.backend(&call, None, format!("unreadable preview index: {e}")))?;
        Ok((!frames.is_empty()).then_some(PreviewFrames { frames }))
    }

    async fn authorize_artwork(&self, id: &ArtworkId) -> Result<(), CatalogError> {
        if id.namespace() != Namespace::Plex {
            return Err(CatalogError::ForeignId(id.to_string()));
        }
        // A person's photo is in no library: anyone signed in may have
        // it, as anyone may find the person in a search.
        if convert::person_photo_url(id.key()).is_some() {
            return Ok(());
        }
        if !is_image_path(id.key()) {
            return Err(CatalogError::NotFound(id.to_string()));
        }
        let key = id.key().split('/').nth(3).expect("validated image path");
        let call = Call::get(format!("/library/metadata/{key}")).about(id);
        let container = self.fetch(&call).await?;
        let metadata = container
            .metadata
            .iter()
            .find(|m| m.rating_key == key)
            .ok_or_else(|| CatalogError::NotFound(id.to_string()))?;
        let section = metadata
            .library_section_id
            .as_ref()
            .or(container.library_section_id.as_ref());
        if !self.serves(section) {
            return Err(CatalogError::NotServed(id.to_string()));
        }
        Ok(())
    }

    async fn artwork(&self, id: &ArtworkId) -> Result<Image, CatalogError> {
        self.authorize_artwork(id).await?;
        // A person's photo is on Plex's people host. The PMS's photo
        // transcoder fetches it, as Plex's own apps have it do, so the
        // server reaches no host but the PMS and the token goes nowhere
        // else; it is asked for no larger than PERSON_PHOTO_SIDE.
        let photo = convert::person_photo_url(id.key());
        let call = match &photo {
            Some(url) => {
                let mut call = Call::get("/photo/:/transcode").about(id);
                call.query = vec![
                    ("url", url.as_str()),
                    ("width", PERSON_PHOTO_SIDE),
                    ("height", PERSON_PHOTO_SIDE),
                ];
                call
            }
            None => Call::get(id.key()).about(id),
        };
        // Agent art is a redirect to Plex's image proxy ([`redirect`]).
        let call = Call {
            redirects: true,
            ..call
        };
        let (bytes, content_type) = self.get_bytes(&call, "image/*", MAX_IMAGE_BYTES).await?;
        match content_type {
            Some(content_type) if content_type.starts_with("image/") => Ok(Image {
                bytes,
                content_type,
            }),
            other => Err(self.backend(&call, None, format!("expected an image, got {other:?}"))),
        }
    }

    async fn media_path(&self, id: &MediaId) -> Result<PathBuf, CatalogError> {
        if !matches!(
            id.kind(),
            MediaKind::Movie | MediaKind::Episode | MediaKind::Track
        ) {
            return Err(CatalogError::NotPlayable(id.clone()));
        }
        let fetched = self.metadata(id, false).await?;
        let files = convert::media_files(&fetched.metadata);
        if files.is_empty() {
            return Err(self.backend(&fetched.call, None, "the PMS lists no media file".into()));
        }
        // Every part must resolve, so a bad mapping shows up even for the
        // parts Phase 1 does not play yet.
        let mut local = Vec::with_capacity(files.len());
        for file in &files {
            local.push(self.resolve(id, fetched.library.as_deref(), Path::new(file))?);
        }
        if local.len() > 1 {
            tracing::info!(%id, parts = local.len(), "item has several parts; playing the first");
        }
        Ok(local.swap_remove(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_hides_the_token() {
        let catalog =
            PlexCatalog::new("http://nas:32400".parse().unwrap(), Some("s3cr3t".into())).unwrap();
        let debug = format!("{catalog:?}");
        assert!(!debug.contains("s3cr3t"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
    }

    #[test]
    fn only_plex_image_paths_are_artwork() {
        assert!(is_image_path("/library/metadata/134883/thumb/1786645049"));
        assert!(is_image_path("/library/metadata/1/art/2"));
        for key in [
            "/library/sections/1/all",
            "/:/prefs",
            "/library/metadata/1/thumb",
            "/library/metadata/1/theme/2",
            "/library/metadata/x/thumb/2",
            "/library/metadata/1/thumb/2?X-Plex-Token=x",
            "/library/metadata/1/thumb/2/../../../sections",
            "library/metadata/1/thumb/2",
        ] {
            assert!(!is_image_path(key), "{key}");
        }
    }

    #[test]
    fn a_non_numeric_key_is_not_found() {
        let id = MediaId::new(Namespace::Plex, MediaKind::Movie, "../../x").unwrap();
        assert!(matches!(
            plex_key(id.namespace(), id.key(), &id),
            Err(CatalogError::NotFound(_))
        ));
    }
}
