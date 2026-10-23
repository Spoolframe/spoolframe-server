//! Downloads of a paired server's items (P8-07): sharing a library
//! includes downloading from it, with no switch of its own.
//!
//! The device asks the server its user is at home on (SB), as for any
//! item. SB vouches for the user to the paired server (SA) for a download
//! token bound to SB's own endpoint, redeems it on SA's `client/1` over the
//! pair's pooled connection ([`PlayClient`]), and asks SA's planner, for the
//! device (`DownloadDevice`): SA makes and keeps the file under its own id,
//! as it does its own users', its copy jobs on its own pool (PX-37). SB
//! keeps the download's record under an id of its own, so `DownloadStatus`,
//! `CancelDownload` and the device's Downloads screen work as for SB's own
//! items. Nothing of the file is written on SB.
//!
//! The bytes come straight from SA when the device reaches it: SB asks SA
//! for a download token bound to the device and to that one download
//! (`RemoteGrant` with a `download`). Otherwise SB reads the file from SA
//! through its own session and relays it, as it does a proxied play.
//!
//! SB keeps one session on SA per user and item while the file is made,
//! and ends it (`Logout`) once SA says the file is ready or failed, so a
//! session holds a place under SA's download token caps only while it is
//! needed: a playlist of hundreds of tracks downloads whole. A read relayed
//! through SB opens one again, ended once no read used it for
//! [`READ_IDLE`]. One SA ended (a restart, a cancel) is asked for again,
//! once, when SA says so.
//!
//! A library no longer shared, or the pair gone, leaves the record here,
//! marked no longer available: a download made stays `Ready`, marked so
//! (`no_longer_available`), and the device keeps the file; one not made
//! is `Failed`. Nothing more is fetched.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use futures_util::{StreamExt, stream};
use iroh::endpoint::RecvStream;
use spoolframe_federation::PlayClient;
use spoolframe_protocol::downloads::{
    DownloadDevice, DownloadFailure, DownloadInfo, DownloadOptionsInfo, DownloadSource,
    DownloadState, DownloadTracks,
};
use spoolframe_protocol::federation::RemoteGranted;
use spoolframe_protocol::resource::{ResourceMetadata, ResourceRequest, ResourceResponse};
use spoolframe_protocol::{Error, Event, Reply, Request, Response, SessionId};
use spoolframe_types::MediaId;

use super::proxy::{Held, answered_with, redeem};
use super::{AssertError, Federation, ProxyError};
use crate::principal::Who;
use crate::store::federation::PeerRecord;
use crate::store::federation_downloads::{NewRemoteDownload, RemoteDownloadRow};

/// How long a session opened for reads relayed through SB is kept with no
/// read: a device reads a file range after range, then is done.
pub const READ_IDLE: Duration = Duration::from_secs(10 * 60);

/// How long the record of a download that ended (no longer available, or
/// failed) is kept: long enough for a device away for weeks to hear it.
pub const ENDED_KEPT: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// How many of SB's records SA is asked about at once for a status.
const STATUSES_AT_ONCE: usize = 4;

/// A download session's key: the pair's server id, the user, and the item
/// by SA's own id.
type SessionKey = (String, String, String);

/// The key of `principal`'s session on `origin` for `remote`.
fn key(origin: &str, principal: &str, remote: &MediaId) -> SessionKey {
    (origin.to_owned(), principal.to_owned(), remote.to_string())
}

/// One session SB keeps on SA.
#[derive(Debug, Clone)]
struct Kept {
    session: SessionId,
    /// Opened for reads relayed through SB, of a file made already: ended
    /// after [`READ_IDLE`] with no read.
    reading: bool,
    used: Instant,
}

/// SB's sessions on its pairs' servers for its users' downloads, by
/// [`key`].
#[derive(Debug, Default)]
pub(super) struct DownloadSessions {
    open: parking_lot::Mutex<HashMap<SessionKey, Kept>>,
}

impl DownloadSessions {
    /// The session kept for `key`, used now.
    fn get(&self, key: &SessionKey) -> Option<SessionId> {
        let mut open = self.open.lock();
        let kept = open.get_mut(key)?;
        kept.used = Instant::now();
        Some(kept.session.clone())
    }

    fn insert(&self, key: SessionKey, session: SessionId, reading: bool) {
        self.open.lock().insert(
            key,
            Kept {
                session,
                reading,
                used: Instant::now(),
            },
        );
    }

    /// The session kept for `key`, no longer kept.
    fn take(&self, key: &SessionKey) -> Option<SessionId> {
        self.open.lock().remove(key).map(|kept| kept.session)
    }

    /// `session` for `key` is gone there: forgotten, unless another took
    /// its place meanwhile.
    fn forget(&self, key: &SessionKey, session: &SessionId) {
        let mut open = self.open.lock();
        if open.get(key).is_some_and(|kept| kept.session == *session) {
            open.remove(key);
        }
    }

    /// Every session kept on `origin`, no longer kept: the pair, or a
    /// library of its, is gone.
    fn take_of(&self, origin: &str) -> Vec<SessionId> {
        let mut taken = Vec::new();
        self.open.lock().retain(|(of, _, _), kept| {
            let gone = of == origin;
            if gone {
                taken.push(kept.session.clone());
            }
            !gone
        });
        taken
    }

    /// The reading sessions no read used for [`READ_IDLE`], by origin, no
    /// longer kept.
    fn take_idle(&self) -> Vec<(String, SessionId)> {
        let mut taken = Vec::new();
        self.open.lock().retain(|(origin, _, _), kept| {
            let idle = kept.reading && kept.used.elapsed() >= READ_IDLE;
            if idle {
                taken.push((origin.clone(), kept.session.clone()));
            }
            !idle
        });
        taken
    }
}

/// End `session` on SA over `client`, best effort: its place under SA's
/// download token caps is let go. SA ends it after a day otherwise.
async fn log_out(client: &PlayClient, session: SessionId) {
    if let Err(e) = client.call(Some(&session), Request::Logout, None).await {
        tracing::debug!(error = %e, "a download session on a paired server was not ended");
    }
}

/// A read of a paired server's download, as SA answered it: its bytes
/// follow on `stream`.
pub struct RemoteDownloadRead {
    /// The file's whole size.
    pub size: u64,
    /// What this read sends.
    pub length: u64,
    pub stream: RecvStream,
    /// Holds the pair's connection while the bytes come.
    pub hold: DownloadHold,
}

/// A read's hold on the pair's connection: dropped once it is sent.
#[derive(Debug)]
pub struct DownloadHold(#[allow(dead_code, reason = "held for its Drop")] Held);

/// What SB answers the device for its downloads among `ids` that are of
/// paired servers' items.
#[derive(Debug, Default)]
pub struct RemoteStatuses {
    /// By this server's id.
    pub found: HashMap<String, DownloadInfo>,
    /// Those SA no longer has: the device prepares them again.
    pub gone: Vec<String>,
}

impl Federation {
    /// What the device may download of `item`, a paired server's, as that
    /// server's planner answers for `device`.
    pub async fn download_options(
        &self,
        who: &Who,
        item: &MediaId,
        device: DownloadDevice,
    ) -> Result<DownloadOptionsInfo, ProxyError> {
        let reply = self
            .ask_origin(who, item, |id| Request::DownloadOptions {
                id: id.clone(),
                device: Some(device.clone()),
            })
            .await?;
        match reply {
            Reply::DownloadOptions(info) => Ok(info),
            other => Err(ProxyError::Origin(answered_with(
                "download options",
                &other,
            ))),
        }
    }

    /// Prepare `who`'s download of `item`, a paired server's, from
    /// `source` with `tracks`, for `device`: SA makes the file, and this
    /// server keeps the record, under its own id.
    pub async fn prepare_download(
        &self,
        who: &Who,
        item: &MediaId,
        source: DownloadSource,
        tracks: DownloadTracks,
        device: DownloadDevice,
    ) -> Result<DownloadInfo, ProxyError> {
        let (origin, _) = item
            .remote()
            .ok_or_else(|| AssertError::NotFederated(item.clone()))?;
        let library = self
            .shared_library_here(item)
            .await
            .ok_or_else(|| ProxyError::Origin(not_found("a paired server's item")))?;
        let reply = self
            .ask_origin(who, item, |id| Request::PrepareDownload {
                id: id.clone(),
                source: source.clone(),
                tracks: tracks.clone(),
                device: Some(device.clone()),
            })
            .await?;
        let Reply::Download(info) = reply else {
            return Err(ProxyError::Origin(answered_with("a download", &reply)));
        };
        let state = json(&info.state);
        let file = info.file.as_ref().map(json);
        let row = self
            .store()
            .put_remote_download(NewRemoteDownload {
                principal: &who.principal().to_string(),
                origin,
                library: &library,
                item: &item.to_string(),
                remote_id: &info.id,
                state: &state,
                file: file.as_deref(),
                expected_size: info.expected_size,
            })
            .await
            .map_err(AssertError::from)?;
        tracing::info!(
            %item,
            download = %row.id,
            remote = %info.id,
            "a paired server's item is downloaded"
        );
        if settled(&info.state) {
            self.end_download_session(who, item).await;
        }
        Ok(DownloadInfo { id: row.id, ..info })
    }

    /// `who`'s downloads among `ids` that are of paired servers' items:
    /// as SA says now for one still being made, else as SA last said. One
    /// no longer available says so; one SA no longer has is gone. Ids that
    /// are not such downloads of `who`'s are left out of both.
    pub async fn download_statuses(
        &self,
        who: &Who,
        ids: &[String],
    ) -> Result<RemoteStatuses, ProxyError> {
        let rows = self
            .store()
            .remote_downloads(&who.principal().to_string(), ids)
            .await
            .map_err(AssertError::from)?;
        // Asked of SA a few at once: each is one round trip there.
        let answers: Vec<(String, Option<DownloadInfo>)> = stream::iter(rows)
            .map(|row| async move {
                let info = self.download_status(who, &row).await;
                (row.id, info)
            })
            .buffer_unordered(STATUSES_AT_ONCE)
            .collect()
            .await;
        let mut statuses = RemoteStatuses::default();
        for (id, info) in answers {
            match info {
                Some(info) => {
                    statuses.found.insert(id, info);
                }
                None => statuses.gone.push(id),
            }
        }
        Ok(statuses)
    }

    /// Download `row` as the device is told it; `None` once SA no longer
    /// has it.
    async fn download_status(&self, who: &Who, row: &RemoteDownloadRow) -> Option<DownloadInfo> {
        let info = kept_info(row);
        if row.unavailable_at.is_some() || settled(&info.state) {
            return Some(info);
        }
        let Ok(item) = row.item.parse::<MediaId>() else {
            return Some(info);
        };
        let asked = self
            .ask_origin(who, &item, |_| Request::DownloadStatus {
                ids: vec![row.remote_id.clone()],
            })
            .await;
        match asked {
            Ok(Reply::Downloads { mut downloads, .. }) if downloads.len() == 1 => {
                let now = downloads.remove(0);
                self.keep_state(&row.id, &now).await;
                if settled(&now.state) {
                    self.end_download_session(who, &item).await;
                }
                Some(DownloadInfo {
                    id: row.id.clone(),
                    ..now
                })
            }
            Ok(Reply::Downloads { gone, .. }) if gone.contains(&row.remote_id) => None,
            // SA out of reach: as it last said.
            Ok(_) | Err(_) => Some(info),
        }
    }

    /// Cancel `who`'s download `id` of a paired server's item, as the
    /// device does once it has the file: SA is told, and lets its file go
    /// and its tokens for it end; the record here goes, told or not.
    /// Whether it was such a download of `who`'s.
    pub async fn cancel_download(&self, who: &Who, id: &str) -> Result<bool, ProxyError> {
        let principal = who.principal().to_string();
        let Some(row) = self
            .store()
            .remote_download(&principal, id)
            .await
            .map_err(AssertError::from)?
        else {
            return Ok(false);
        };
        if row.unavailable_at.is_none()
            && let Ok(item) = row.item.parse::<MediaId>()
        {
            let told = self
                .ask_origin(who, &item, |_| Request::CancelDownload {
                    id: row.remote_id.clone(),
                })
                .await;
            if let Err(e) = told {
                tracing::info!(download = id, error = %e, "the paired server was not told a download ended: it lets the file go a day after its last read");
            }
            // SA ended the sessions of that download.
            if let Some((origin, remote)) = item.remote() {
                self.download_sessions
                    .take(&key(origin, &principal, &remote));
            }
        }
        self.store()
            .delete_remote_download(&principal, id)
            .await
            .map_err(AssertError::from)?;
        Ok(true)
    }

    /// At most `length` bytes from `offset` of `who`'s download `id` of a
    /// paired server's item, read from SA through this server's session
    /// there: for a device that cannot reach SA. Relayed as it comes.
    /// `None` when `id` is no such download of `who`'s.
    pub async fn read_download(
        &self,
        who: &Who,
        id: &str,
        offset: u64,
        length: u64,
    ) -> Result<Option<RemoteDownloadRead>, ProxyError> {
        let Some(row) = self
            .store()
            .remote_download(&who.principal().to_string(), id)
            .await
            .map_err(AssertError::from)?
        else {
            return Ok(None);
        };
        if row.unavailable_at.is_some() {
            return Err(ProxyError::Origin(not_found("download")));
        }
        let item: MediaId = row
            .item
            .parse()
            .map_err(|_| ProxyError::Origin(not_found("download")))?;
        let (peer, remote) = self.remote_peer(&item).await?;
        for again in [true, false] {
            let (held, session) = self
                .download_session(who, &peer, &item, &remote, true)
                .await?;
            let (response, stream) = held
                .client
                .resource(&ResourceRequest::Download {
                    session: session.clone(),
                    id: row.remote_id.clone(),
                    offset,
                    length,
                })
                .await?;
            match response {
                ResourceResponse::Ready {
                    length: sent,
                    metadata: ResourceMetadata::Download { size },
                } => {
                    return Ok(Some(RemoteDownloadRead {
                        size,
                        // Never more than asked, whatever SA says: the
                        // device is told this length, and the relay sends
                        // no more of SA's stream.
                        length: sent.min(length),
                        stream,
                        hold: DownloadHold(held),
                    }));
                }
                ResourceResponse::Error {
                    error: Error::Unauthenticated,
                } if again => self.download_sessions.forget(
                    &key(&peer.server_id, &who.principal().to_string(), &remote),
                    &session,
                ),
                ResourceResponse::Error { error } => return Err(ProxyError::Origin(error)),
                ResourceResponse::Ready { .. } => {
                    return Err(ProxyError::Origin(Error::Unavailable {
                        message: "the paired server answered a download's read with something else"
                            .into(),
                    }));
                }
            }
        }
        Err(ProxyError::Origin(Error::Unauthenticated))
    }

    /// A download token for the device `device`, of `who`'s download `id`
    /// of `item` alone, which it reads on SA itself (`RemoteGrant` with a
    /// `download`). `OriginOffline` with SA offline.
    pub async fn grant_download(
        &self,
        who: &Who,
        item: &MediaId,
        id: &str,
        device: iroh::EndpointId,
    ) -> Result<RemoteGranted, ProxyError> {
        let row = self
            .store()
            .remote_download(&who.principal().to_string(), id)
            .await
            .map_err(AssertError::from)?
            .filter(|row| row.unavailable_at.is_none() && row.item == item.to_string())
            .ok_or_else(|| ProxyError::Origin(not_found("download")))?;
        let (peer, remote) = self.remote_peer(item).await?;
        if !self.online(&peer.server_id) {
            return Err(ProxyError::Origin(Error::OriginOffline));
        }
        let granted = self
            .assert_download_for(who, &peer, item, device, Some(row.remote_id.clone()))
            .await?;
        Ok(RemoteGranted {
            token: granted.token,
            item: remote,
            endpoint_id: peer.endpoint_id,
            expires_at: granted.expires_at,
            download: Some(row.remote_id),
        })
    }

    /// `request` of `item` (SA's own id given to `request`), asked of SA
    /// in `who`'s download session there: SA's answer. A session SA ended
    /// is asked for again, once.
    async fn ask_origin(
        &self,
        who: &Who,
        item: &MediaId,
        request: impl Fn(&MediaId) -> Request,
    ) -> Result<Reply, ProxyError> {
        let (peer, remote) = self.remote_peer(item).await?;
        for again in [true, false] {
            let (held, session) = self
                .download_session(who, &peer, item, &remote, false)
                .await?;
            let answered = held
                .client
                .call(Some(&session), request(&remote), None)
                .await?;
            match answered.response {
                Response::Ok(reply) => return Ok(reply),
                Response::Err(Error::Unauthenticated) if again => {
                    self.download_sessions.forget(
                        &key(&peer.server_id, &who.principal().to_string(), &remote),
                        &session,
                    );
                }
                Response::Err(e) => return Err(ProxyError::Origin(e)),
            }
        }
        Err(ProxyError::Origin(Error::Unauthenticated))
    }

    /// The pair's connection, held, and `who`'s download session for
    /// `item` (SA's `remote`) on it: the one kept, or one opened now with a
    /// download token bound to this server's endpoint, for reads relayed
    /// through this server when `reading`. Reading sessions left idle are
    /// ended first.
    async fn download_session(
        &self,
        who: &Who,
        peer: &PeerRecord,
        item: &MediaId,
        remote: &MediaId,
        reading: bool,
    ) -> Result<(Held, SessionId), ProxyError> {
        let endpoint = self
            .attached
            .get()
            .map(|a| a.endpoint.clone())
            .ok_or(AssertError::Offline)?;
        self.end_idle_sessions().await;
        let key = key(&peer.server_id, &who.principal().to_string(), remote);
        let kept = self.download_sessions.get(&key);
        let held = self.play_client(&endpoint, peer).await?;
        if let Some(session) = kept {
            return Ok((held, session));
        }
        let granted = self
            .assert_download_for(who, peer, item, endpoint.id(), None)
            .await?;
        let session = redeem(&held.client, granted.token).await?;
        self.download_sessions.insert(key, session.clone(), reading);
        Ok((held, session))
    }

    /// `who`'s session for `item`, a paired server's, ended there and no
    /// longer kept: SA said its file is made, or failed.
    async fn end_download_session(&self, who: &Who, item: &MediaId) {
        let Some((origin, remote)) = item.remote() else {
            return;
        };
        let Some(session) =
            self.download_sessions
                .take(&key(origin, &who.principal().to_string(), &remote))
        else {
            return;
        };
        if let Some(client) = self.client_of(origin).await {
            log_out(&client, session).await;
        }
    }

    /// The reading sessions no read used for [`READ_IDLE`], ended.
    pub(super) async fn end_idle_sessions(&self) {
        for (origin, session) in self.download_sessions.take_idle() {
            if let Some(client) = self.client_of(&origin).await {
                log_out(&client, session).await;
            }
        }
    }

    /// The connection open to pair `origin`'s client protocol, if one is:
    /// none is dialled to end a session, which SA ends in time itself.
    async fn client_of(&self, origin: &str) -> Option<Arc<PlayClient>> {
        let peer = self.live_peer_by_server_id(origin).await.ok()??;
        self.plays.held(&peer.id).map(|held| held.client.clone())
    }

    /// The library `item`, a paired server's, is in, by that server's id,
    /// while it is shared here.
    async fn shared_library_here(&self, item: &MediaId) -> Option<String> {
        use spoolframe_catalog::Catalog as _;
        let library = self.cache.library_of(item).await.ok()??;
        Some(library.remote()?.1.to_string())
    }

    /// Keep what SA said of download `id` here.
    async fn keep_state(&self, id: &str, info: &DownloadInfo) {
        let file = info.file.as_ref().map(json);
        if let Err(e) = self
            .store()
            .set_remote_download_state(id, &json(&info.state), file.as_deref(), info.expected_size)
            .await
        {
            tracing::warn!(download = id, error = %e, "cannot keep a paired server's download's state");
        }
    }

    /// The pair `origin` stopped sharing `library`, or every library with
    /// `None` (unpaired): its downloads here are no longer available. The
    /// sessions kept for them are refused there from now, and asked for
    /// again only for a download prepared again.
    pub(super) async fn downloads_unavailable(&self, origin: &str, library: Option<&str>) {
        // SA ended the sessions of what it no longer shares; any other of
        // the pair's is opened again when next needed.
        let sessions = self.download_sessions.take_of(origin);
        if library.is_some()
            && !sessions.is_empty()
            && let Some(client) = self.client_of(origin).await
        {
            for session in sessions {
                log_out(&client, session).await;
            }
        }
        match self
            .store()
            .remote_downloads_unavailable(origin, library)
            .await
        {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                origin,
                library,
                downloads = n,
                "downloads of a paired server's items are no longer available"
            ),
            Err(e) => tracing::warn!(error = %e, "cannot mark a paired server's downloads"),
        }
    }

    /// The downloads this server makes for pair `peer`'s users (as SA), of
    /// `library`'s items alone if given, are cancelled: the grant was
    /// revoked, or the pair ended. What was being made stops.
    pub(super) async fn cancel_served_downloads(&self, peer: &PeerRecord, library: Option<&str>) {
        let Some(downloads) = self.downloads.get() else {
            return;
        };
        let prefix = format!("remote:{}:", peer.server_id);
        let rows = match self.store().downloads_of_principals(&prefix).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, "cannot list a paired server's users' downloads");
                return;
            }
        };
        let mut of_library = Vec::with_capacity(rows.len());
        for row in rows {
            let shared = match library {
                None => true,
                Some(library) => match row.media_id.parse::<MediaId>() {
                    Ok(id) => self
                        .catalog
                        .library_of(&id)
                        .await
                        .ok()
                        .flatten()
                        .is_some_and(|l| l.to_string() == library),
                    Err(_) => false,
                },
            };
            if shared {
                of_library.push(row);
            }
        }
        let cancelled = downloads.cancel_rows(of_library).await;
        match cancelled {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                peer = %peer.endpoint_id,
                library,
                downloads = n,
                "a paired server's users' downloads are cancelled"
            ),
            Err(e) => tracing::warn!(error = %e, "cannot cancel a paired server's downloads"),
        }
    }
}

/// SA's events on the pair `origin`'s connection (`client`), until it
/// closes: each download of SA's that moved is told to its user here as
/// one of this server's own, and kept. One SA is done with ends its
/// session there.
pub(super) async fn forward_download_news(
    store: crate::store::Store,
    downloads: Option<Arc<crate::downloads::Downloads>>,
    sessions: Arc<DownloadSessions>,
    origin: String,
    client: Weak<PlayClient>,
    mut events: tokio::sync::broadcast::Receiver<Event>,
) {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        let event = match events.recv().await {
            Ok(event) => event,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => return,
        };
        let Event::DownloadChanged { id, state } = event else {
            continue;
        };
        let rows = match store.remote_downloads_by_remote_id(&origin, &id).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, "cannot find a paired server's download");
                continue;
            }
        };
        for row in rows.into_iter().filter(|r| r.unavailable_at.is_none()) {
            if let Err(e) = store
                .set_remote_download_state(&row.id, &json(&state), None, None)
                .await
            {
                tracing::warn!(error = %e, "cannot keep a paired server's download's state");
            }
            if settled(&state)
                && let Some((_, remote)) = row
                    .item
                    .parse::<MediaId>()
                    .ok()
                    .as_ref()
                    .and_then(MediaId::remote)
                && let Some(session) = sessions.take(&key(&origin, &row.principal, &remote))
                && let Some(client) = client.upgrade()
            {
                log_out(&client, session).await;
            }
            if let (Some(downloads), Some(state)) = (
                &downloads,
                crate::downloads::DownloadState::from_wire(&state),
            ) {
                downloads.tell(crate::downloads::DownloadChange {
                    id: row.id,
                    principal: row.principal,
                    state,
                });
            }
        }
    }
}

/// `row` as SA last said it, under this server's id. One no longer
/// available stays `Ready`, marked so, if its file was made, so the device
/// keeps it; else it failed.
fn kept_info(row: &RemoteDownloadRow) -> DownloadInfo {
    let mut info = DownloadInfo {
        id: row.id.clone(),
        state: serde_json::from_str(&row.state).unwrap_or(DownloadState::Unknown),
        file: row
            .file
            .as_deref()
            .and_then(|f| serde_json::from_str(f).ok()),
        expected_size: row.expected_size.and_then(|n| u64::try_from(n).ok()),
        no_longer_available: false,
    };
    if row.unavailable_at.is_some() {
        match info.state {
            DownloadState::Ready { .. } => info.no_longer_available = true,
            _ => {
                info.state = DownloadState::Failed {
                    reason: "the server it is from no longer shares it".into(),
                    cause: DownloadFailure::NoLongerAvailable,
                }
            }
        }
    }
    info
}

/// Whether SA is done with a download in `state`: its file made, or failed.
fn settled(state: &DownloadState) -> bool {
    matches!(
        state,
        DownloadState::Ready { .. } | DownloadState::Failed { .. }
    )
}

fn json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("a download's state is JSON")
}

fn not_found(what: &str) -> Error {
    Error::NotFound { what: what.into() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(state: &str, unavailable: bool) -> RemoteDownloadRow {
        RemoteDownloadRow {
            id: "d1".into(),
            principal: "local:00000000-0000-0000-0000-000000000007".into(),
            origin: "mzxw6ytbojtg633c".into(),
            library: "plex:1".into(),
            item: "fed:movie:mzxw6ytbojtg633c:plex:1".into(),
            remote_id: "r1".into(),
            state: state.into(),
            file: None,
            expected_size: None,
            unavailable_at: unavailable.then_some(1),
        }
    }

    /// A download made stays `Ready` once no longer available, marked so,
    /// for the device to keep its file; one not made fails.
    #[test]
    fn a_made_download_no_longer_available_stays_ready() {
        let ready = r#"{"type":"ready","size":1,"sha256":"00"}"#;
        let made = kept_info(&row(ready, true));
        assert!(matches!(made.state, DownloadState::Ready { .. }));
        assert!(made.no_longer_available);
        let making = kept_info(&row(r#"{"type":"waiting"}"#, true));
        assert!(matches!(
            making.state,
            DownloadState::Failed {
                cause: DownloadFailure::NoLongerAvailable,
                ..
            }
        ));
        assert!(!making.no_longer_available);
        assert!(!kept_info(&row(ready, false)).no_longer_available);
    }

    /// A pair's sessions go with it alone; one forgotten is forgotten only
    /// while it is still the one kept.
    #[test]
    fn download_sessions_are_kept_by_pair_user_and_item() {
        let sessions = DownloadSessions::default();
        let film: MediaId = "plex:movie:1".parse().unwrap();
        let sa = key("aaaaaaaaaaaaaaaa", "local:bob", &film);
        let sc = key("cccccccccccccccc", "local:bob", &film);
        sessions.insert(sa.clone(), SessionId("one".into()), false);
        sessions.insert(sc.clone(), SessionId("two".into()), true);
        sessions.forget(&sa, &SessionId("old".into()));
        assert_eq!(sessions.get(&sa), Some(SessionId("one".into())));
        assert_eq!(
            sessions.take_of("aaaaaaaaaaaaaaaa"),
            [SessionId("one".into())]
        );
        assert_eq!(sessions.get(&sa), None);
        assert!(sessions.take_idle().is_empty(), "read just now");
        assert_eq!(sessions.take(&sc), Some(SessionId("two".into())));
    }
}
