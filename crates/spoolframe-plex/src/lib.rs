//! `PlexCatalog`: reads the library from a Plex Media Server and turns it
//! into Spoolframe's own media model. Plex supplies metadata only; it is
//! never in the media path.
//!
//! It talks to the PMS directly over the LAN and never to plex.tv, so the
//! catalog keeps working when the Internet is down (ARCHITECTURE §32). It
//! reads on demand and does not copy the Plex database.

pub mod check;
mod convert;
pub mod paths;
mod wire;

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::StatusCode;
use spoolframe_catalog::{
    Catalog, CatalogError, Image, ItemPage, ItemWithChildren, Page, UnavailableFile,
};
use spoolframe_types::{ArtworkId, Item, Library, LibraryId, MediaId, MediaKind, Namespace};
use url::Url;

pub use crate::check::{Failures, LibraryPaths, Scope};
pub use crate::paths::{PathMap, PathMapping};

use crate::convert::ConvertError;
use crate::wire::{Envelope, MediaContainer, Metadata};

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
    /// Plex paths → local paths. Applied here and only here: nothing
    /// outside this crate sees a Plex-side path.
    paths: PathMap,
}

/// An item fetched by ID: as Plex sent it, converted, and the library it
/// is in.
struct Fetched {
    metadata: Metadata,
    item: Item,
    library: Option<String>,
    call: Call<'static>,
}

impl fmt::Debug for PlexCatalog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlexCatalog")
            .field("base", &self.base)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("paths", &self.paths)
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
}

impl<'a> Call<'a> {
    fn get(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            query: Vec::new(),
            headers: Vec::new(),
            subject: None,
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
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| CatalogError::Backend {
                url: base.clone(),
                call: "build HTTP client".into(),
                item: None,
                message: e.to_string(),
            })?;
        Ok(Self {
            base,
            token,
            http,
            paths: PathMap::default(),
        })
    }

    /// Map the media paths Plex reports with `paths`.
    pub fn with_paths(mut self, paths: PathMap) -> Self {
        self.paths = paths;
        self
    }

    async fn fetch(&self, call: &Call<'_>) -> Result<MediaContainer, CatalogError> {
        let body = self.get_bytes(call, "application/json").await?.0;
        serde_json::from_slice::<Envelope>(&body)
            .map(|e| e.container)
            .map_err(|e| self.backend(call, None, format!("unreadable response: {e}")))
    }

    /// One GET, with the token; the body and its content type. Status codes
    /// become errors here.
    async fn get_bytes(
        &self,
        call: &Call<'_>,
        accept: &str,
    ) -> Result<(Vec<u8>, Option<String>), CatalogError> {
        let mut req = self
            .http
            .get(format!("{}{}", self.base, call.path))
            .header(reqwest::header::ACCEPT, accept)
            .query(&call.query);
        if let Some(token) = &self.token {
            req = req.header("X-Plex-Token", token);
        }
        for (name, value) in &call.headers {
            req = req.header(*name, value);
        }

        let resp = req
            .send()
            .await
            .map_err(|source| CatalogError::Unreachable {
                url: self.base.clone(),
                call: call.describe(),
                source: Box::new(source),
            })?;
        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Err(CatalogError::NotFound(
                call.subject.clone().unwrap_or_else(|| call.path.clone()),
            ));
        }
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            let message = match self.token {
                Some(_) => format!("the PMS rejected the token ({status})"),
                None => format!("the PMS requires a token and none is configured ({status})"),
            };
            return Err(self.backend(call, None, message));
        }
        if !status.is_success() {
            return Err(self.backend(call, None, format!("unexpected HTTP {status}")));
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = resp
            .bytes()
            .await
            .map_err(|source| CatalogError::Unreachable {
                url: self.base.clone(),
                call: call.describe(),
                source: Box::new(source),
            })?;
        Ok((body.to_vec(), content_type))
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
    /// kind the ID says, both as Plex sent it and converted.
    async fn metadata(&self, id: &MediaId) -> Result<Fetched, CatalogError> {
        let key = plex_key(id.namespace(), id.key(), id)?;
        let call = Call::get(format!("/library/metadata/{key}")).about(id);
        let mut container = self.fetch(&call).await?;
        if container.metadata.is_empty() {
            return Err(CatalogError::NotFound(id.to_string()));
        }
        let metadata = container.metadata.swap_remove(0);
        let item = convert::item(&metadata).map_err(|e| self.converted(&call, e))?;
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
            call,
        })
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
        match paths::unreadable(&mapped.local) {
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

/// The largest image passed on; posters and backgrounds are far smaller.
const MAX_IMAGE_BYTES: usize = 32 << 20;

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

#[async_trait]
impl Catalog for PlexCatalog {
    async fn libraries(&self) -> Result<Vec<Library>, CatalogError> {
        let call = Call::get("/library/sections");
        let container = self.fetch(&call).await?;
        container
            .directories
            .iter()
            .filter_map(|d| convert::library(d).transpose())
            .collect::<Result<_, _>>()
            .map_err(|e| self.converted(&call, e))
    }

    async fn items(&self, library: &LibraryId, page: Page) -> Result<ItemPage, CatalogError> {
        let key = plex_key(library.namespace(), library.key(), library)?;
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
        let Fetched {
            item,
            call,
            metadata,
            ..
        } = self.metadata(id).await?;
        let children = if has_children(id.kind()) {
            let children = Call::get(format!("{}/children", call.path)).about(id);
            // Only `Metadata` entries are children; a show's "All episodes"
            // shortcut comes as a `Directory` and is not one.
            let container = self.fetch(&children).await?;
            self.items_of(&children, &container.metadata)?
        } else {
            Vec::new()
        };
        Ok(ItemWithChildren {
            item,
            children,
            external_ids: convert::external_ids(&metadata),
        })
    }

    async fn recently_added(&self, limit: u32) -> Result<Vec<Item>, CatalogError> {
        let mut call = Call::get("/library/recentlyAdded");
        call.headers = vec![
            ("X-Plex-Container-Start", "0".to_owned()),
            ("X-Plex-Container-Size", limit.to_string()),
        ];
        let container = self.fetch(&call).await?;
        // Plex groups new episodes into their seasons and may list kinds
        // Spoolframe does not model (photos, clips); those are skipped.
        container
            .metadata
            .iter()
            .filter(|m| convert::is_modelled(&m.kind))
            .map(convert::item)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| self.converted(&call, e))
    }

    async fn search(&self, query: &str, limit: u32) -> Result<Vec<Item>, CatalogError> {
        let limit_str = limit.to_string();
        let mut call = Call::get("/hubs/search").about(format!("search {query:?}"));
        call.query = vec![("query", query), ("limit", &limit_str)];
        let container = self.fetch(&call).await?;
        let mut items =
            convert::search_results(&container.hubs).map_err(|e| self.converted(&call, e))?;
        // PMS applies `limit` per hub (per kind), not to the whole result.
        items.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(items)
    }

    async fn artwork(&self, id: &ArtworkId) -> Result<Image, CatalogError> {
        if id.namespace() != Namespace::Plex {
            return Err(CatalogError::ForeignId(id.to_string()));
        }
        // The key is a PMS path, so only the image paths Plex hands out
        // are fetched; anything else would read any PMS endpoint with the
        // server's token.
        if !is_image_path(id.key()) {
            return Err(CatalogError::NotFound(id.to_string()));
        }
        let call = Call::get(id.key()).about(id);
        let (bytes, content_type) = self.get_bytes(&call, "image/*").await?;
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(self.backend(
                &call,
                None,
                format!("image of {} bytes is over the limit", bytes.len()),
            ));
        }
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
        let fetched = self.metadata(id).await?;
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
