//! Subtitle search on a paired server's items (P8-07).
//!
//! SA plans the play and burns subtitles in, so the file must be on SA:
//! the server the user is at home on (SB) passes `SearchSubtitles`,
//! `DownloadSubtitle` and `RemoveSubtitleDownload` of a `fed:` item to SA
//! over `federation/1`, with the user and their SDH and forced choices. SA
//! searches with its own sites and ranking, and keeps what it downloads as
//! its own, for everyone.
//!
//! Every user of SB's together gets one daily allowance on SA
//! (`subtitles { daily_limit_per_peer }`), counted under the pair and never
//! under SB's users, whom SA never records. SB holds each of its users to
//! its own per-user limit first, so one of them cannot use up the pair's
//! allowance unless SB's admin lets them.

use std::sync::Arc;

use iroh::EndpointId;
use spoolframe_federation::Dialed;
use spoolframe_protocol::federation::{SubtitleDownloadAsk, SubtitleRemovalAsk, SubtitleSearchAsk};
use spoolframe_protocol::{Error, ServerId};
use spoolframe_types::{MediaId, SubtitleDownloaded, SubtitlePreferences, SubtitleSearch};

use super::answer::Ask;
use super::proxy::origin_error;
use super::{AssertError, Federation};
use crate::client::download_error;
use crate::principal::{Principal, Who};
use crate::store::users::LocalUserId;
use crate::subtitle_downloads::{PairAsker, SubtitleDownloads};

/// How long a pair that asks too fast is told to wait.
const TOO_FAST_MS: u64 = 10_000;

/// A pair's ask, checked: the item by this server's id, the pair as who
/// asks, and this server's subtitle downloads.
struct PairAsk<'a> {
    item: MediaId,
    asker: PairAsker,
    subtitles: &'a SubtitleDownloads,
}

/// This server's user's ask of a pair's item: who, by their own id; the
/// item by its origin's id; and the pair's connection.
struct Route {
    user: LocalUserId,
    remote: String,
    dialed: Arc<Dialed>,
}

impl Federation {
    /// This server's subtitle downloads (P8-07): what its pairs' users
    /// search and download with, and what its own users' downloads of the
    /// pairs' items count against. Once, at start.
    pub fn attach_subtitle_downloads(&self, subtitles: Arc<SubtitleDownloads>) {
        let _ = self.subtitle_downloads.set(subtitles);
    }

    fn subtitles(&self) -> Result<&SubtitleDownloads, Error> {
        self.subtitle_downloads
            .get()
            .map(Arc::as_ref)
            .ok_or(Error::NoSubtitleSource)
    }

    // --- SA: a pair's user asks ---

    /// SB's search for one of its users (P8-07), ordered by their choices.
    pub(super) async fn answer_search(
        &self,
        server_id: &ServerId,
        endpoint: EndpointId,
        ask: &SubtitleSearchAsk,
    ) -> Result<SubtitleSearch, Error> {
        let mut checked = self
            .pair_ask(server_id, endpoint, &ask.user, &ask.item)
            .await?;
        checked.asker.subtitles = ask.subtitles.clone();
        checked
            .subtitles
            .search_for_pair(&checked.item, ask.language.as_deref(), &checked.asker)
            .await
            .map_err(download_error)
    }

    /// SB's download for one of its users (P8-07), counted under the pair.
    pub(super) async fn answer_download(
        &self,
        server_id: &ServerId,
        endpoint: EndpointId,
        ask: &SubtitleDownloadAsk,
    ) -> Result<SubtitleDownloaded, Error> {
        let checked = self
            .pair_ask(server_id, endpoint, &ask.user, &ask.item)
            .await?;
        checked
            .subtitles
            .download_for_pair(&checked.item, &ask.result, &checked.asker)
            .await
            .map_err(download_error)
    }

    /// SB's removal for one of its users (P8-07): of what the pair
    /// downloaded, any of its users may remove.
    pub(super) async fn answer_removal(
        &self,
        server_id: &ServerId,
        endpoint: EndpointId,
        ask: &SubtitleRemovalAsk,
    ) -> Result<(), Error> {
        let checked = self
            .pair_ask(server_id, endpoint, &ask.user, &ask.item)
            .await?;
        checked
            .subtitles
            .remove_for_pair(&checked.item, &ask.download, &checked.asker)
            .await
            .map_err(download_error)
    }

    /// A pair's ask read and checked: paced by the pair's bucket, from a
    /// live pair at the key it is pinned to (X8-01), for a local user of
    /// its, of an item in a library shared with it.
    async fn pair_ask(
        &self,
        server_id: &ServerId,
        endpoint: EndpointId,
        user: &str,
        item: &str,
    ) -> Result<PairAsk<'_>, Error> {
        if !self
            .answering
            .allowed(server_id.as_str(), Ask::Subtitles, &self.limits())
        {
            tracing::info!(server_id = %server_id, "a paired server asks for subtitles too fast");
            return Err(Error::RateLimited {
                retry_after_ms: TOO_FAST_MS,
            });
        }
        let not_found = || Error::NotFound {
            what: "a shared item".into(),
        };
        // The pair before the subtitle source: a key that is not the
        // pair's is told nothing, not even that this server has no
        // subtitle sites (X8-01).
        let peer = self
            .live_pair_at(server_id, endpoint)
            .await
            .map_err(|e| unavailable(&e))?
            .ok_or_else(not_found)?;
        let subtitles = self.subtitles()?;
        // A stable user id, as an assertion's (P8-03): never recorded here.
        uuid::Uuid::try_parse(user).map_err(|_| Error::InvalidInput {
            message: "the user is not a local account's id".into(),
        })?;
        let item: MediaId = item.parse().map_err(|_| not_found())?;
        match self.shared_library_of(&peer.id, &item).await {
            Ok(Some(_)) => {}
            Ok(None) => return Err(not_found()),
            Err(e) => {
                tracing::debug!(%item, error = %e, "a subtitle ask for an item not placed");
                return Err(not_found());
            }
        }
        Ok(PairAsk {
            item,
            asker: PairAsker {
                server_id: peer.server_id,
                name: peer.display_name,
                subtitles: SubtitlePreferences::default(),
            },
            subtitles,
        })
    }

    // --- SB: this server's user asks of a pair's item ---

    /// Subtitles found for `item`, a paired server's, by its server
    /// (P8-07), ordered by `who`'s own choices.
    pub async fn search_subtitles_of(
        &self,
        who: &Who,
        item: &MediaId,
        language: Option<&str>,
    ) -> Result<SubtitleSearch, Error> {
        let route = self.subtitle_route(who, item).await?;
        let subtitles = self
            .store()
            .track_preferences(Principal::LocalUser(route.user))
            .await
            .map_err(|e| unavailable(&e))?
            .subtitles;
        route
            .dialed
            .search_subtitles(SubtitleSearchAsk {
                user: route.user.to_string(),
                subtitles,
                item: route.remote,
                language: language.map(str::to_owned),
            })
            .await
            .map_err(origin_error)
    }

    /// Result `result` of a search for `item`, a paired server's,
    /// downloaded there for `who` (P8-07): held to this server's limit per
    /// user first, then to the pair's there, and counted under both. The
    /// user's limit here is held while the pair is asked, so two of their
    /// downloads at once never both take their last.
    pub async fn download_subtitle_of(
        &self,
        who: &Who,
        item: &MediaId,
        result: &str,
    ) -> Result<SubtitleDownloaded, Error> {
        let route = self.subtitle_route(who, item).await?;
        let principal = Principal::LocalUser(route.user).to_string();
        let held = self
            .subtitles()?
            .elsewhere(&principal, who.display_name())
            .await
            .map_err(download_error)?;
        // Logged as the pair's, not a site's: this server's own downloads
        // from each site stay its own.
        let (origin, _) = item.remote().expect("routed as a paired server's item");
        let source = format!("pair:{origin}");
        let ask = SubtitleDownloadAsk {
            user: route.user.to_string(),
            item: route.remote,
            result: result.to_owned(),
        };
        let dialed = route.dialed;
        // The ask and its count on work of their own, as SA's keeping is: a
        // request cancelled once the pair is asked still counts what the
        // pair downloads for the user, their limit held until then. The
        // ask has a wait of its own.
        let (mut downloaded, here) = tokio::spawn(async move {
            let downloaded = dialed.download_subtitle(ask).await.map_err(origin_error)?;
            let here = held.downloaded(&source).await;
            Ok::<_, Error>((downloaded, here))
        })
        .await
        .map_err(|e| unavailable(&e))??;
        // What the user may still download: the fewer of theirs here and
        // the pair's there.
        downloaded.user_remaining = match (here, downloaded.user_remaining) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        Ok(downloaded)
    }

    /// Download `download` of `item`, a paired server's, removed there for
    /// `who` (P8-07).
    pub async fn remove_subtitle_download_of(
        &self,
        who: &Who,
        item: &MediaId,
        download: &str,
    ) -> Result<(), Error> {
        let route = self.subtitle_route(who, item).await?;
        route
            .dialed
            .remove_subtitle_download(SubtitleRemovalAsk {
                user: route.user.to_string(),
                item: route.remote,
                download: download.to_owned(),
            })
            .await
            .map_err(origin_error)
    }

    /// Where `who`'s ask of `item` goes: the live pair it is from, over its
    /// connection. A portable user, or one of a pair's, asks of no paired
    /// server's item here.
    async fn subtitle_route(&self, who: &Who, item: &MediaId) -> Result<Route, Error> {
        let local = who.local().ok_or(Error::NotAllowed)?;
        let (peer, remote) = self.remote_peer(item).await.map_err(|e| match e {
            AssertError::NotFederated(_) => Error::NotFound {
                what: "a paired server's item".into(),
            },
            AssertError::NotPaired(_) => Error::OriginOffline,
            other => unavailable(&other),
        })?;
        let dialed = self.links.pinned(&peer).ok_or(Error::OriginOffline)?;
        Ok(Route {
            user: local.user,
            remote: remote.to_string(),
            dialed,
        })
    }
}

fn unavailable(e: &dyn std::fmt::Display) -> Error {
    tracing::warn!(error = %e, "cannot answer a subtitle ask of a paired server's item");
    Error::Unavailable {
        message: "the server cannot answer now".into(),
    }
}
