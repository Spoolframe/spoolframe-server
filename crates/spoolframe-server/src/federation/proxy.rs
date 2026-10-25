//! Proxy mode (P8-04): the server a library is shared with (SB) plays a
//! paired server's (SA's) item in its own name, and on to its own device,
//! when the device cannot reach SA itself.
//!
//! SB asks SA for a play token bound to its own endpoint, redeems it on
//! SA's `spoolframe/client/1` as one more device ([`PlayClient`]), and
//! starts the playback there with the device's capabilities in place of
//! its own (the `capabilities` override). SA's code is the same in both
//! modes. SB keeps a playback of its own for the device, with no file and
//! no conversion: its controls and its reads pass through to SA's
//! ([`RemotePlay`]), and nothing is written to SB's disk.
//!
//! SB keeps one connection to each pair's client protocol, and each play
//! is a session of its own on it: a play ends by ending its session
//! (`Logout`), which ends SA's playback with it. When the connection
//! closes, SA restarting, SB unable to reach it any more, every play on it
//! ends; none moves to a direct play, nor back.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use iroh::EndpointId;
use spoolframe_federation::{DialError, PlayClient};
use spoolframe_protocol::federation::RedeemPlayToken;
use spoolframe_protocol::media::MediaRead;
use spoolframe_protocol::resource::{ResourceMetadata, ResourceRequest, ResourceResponse};
use spoolframe_protocol::{
    BitmapSubtitles, ChosenTracks, Error, MAX_READ, ManifestKind, PlaybackHandle, PlaybackPlan,
    Quality, Reply, Request, Response, Selection, SessionId,
};
use spoolframe_types::{ClientCapabilities, DisplayCapabilities, Marker, MediaId};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::Notify;

use super::{AssertError, Federation, peer_addr};
use crate::keyed_locks::KeyedLocks;
use crate::principal::Who;
use crate::store::federation::PeerRecord;

/// What the device asked to start, as SB plays it from SA.
#[derive(Debug, Clone)]
pub struct ProxyStart {
    /// The item, by its `fed:` id here.
    pub item: MediaId,
    pub selection: Option<Selection>,
    pub start_ms: Option<u64>,
    pub bitmap: BitmapSubtitles,
    pub quality: Quality,
    pub display: Option<DisplayCapabilities>,
    /// What the device plays: what SA plans for.
    pub capabilities: Option<ClientCapabilities>,
    /// SB's share of its own uplink for the play on to the device
    /// (PX-46): SA plans within it as well as within its own.
    pub onward_share: Option<u64>,
    /// The device's endpoint: a start it was granted a token for already
    /// is that play, its dial failed, not another.
    pub device: EndpointId,
}

/// SA's answer to a start SB played from there.
#[derive(Debug)]
pub struct ProxyStarted {
    pub remote: Arc<RemotePlay>,
    pub plan: PlaybackPlan,
    pub size: u64,
    pub chosen: Option<ChosenTracks>,
    pub epoch: u32,
    pub markers: Vec<Marker>,
}

/// Why SB could not play a paired server's item from there.
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    /// No token for the play.
    #[error(transparent)]
    Assert(#[from] AssertError),
    /// SA refused, as it would the device; or could not be reached, or
    /// stopped answering.
    #[error("the paired server: {0}")]
    Origin(Error),
}

impl From<DialError> for ProxyError {
    fn from(e: DialError) -> Self {
        Self::Origin(origin_error(e))
    }
}

/// How long SB keeps a pair's connection with no play on it.
pub const PLAY_IDLE: Duration = Duration::from_secs(60);

/// SB's connections to its pairs' client protocols, one per pair, each
/// carrying every play from there. One closed is dropped when next asked
/// for; one with no play on it for [`PLAY_IDLE`] is closed; an unpaired
/// pair's is closed at once.
#[derive(Debug)]
pub(super) struct PlayClients {
    open: parking_lot::Mutex<HashMap<String, Pooled>>,
    /// One dial at a time to each pair.
    dialing: KeyedLocks,
    idle: parking_lot::Mutex<Duration>,
}

impl Default for PlayClients {
    fn default() -> Self {
        Self {
            open: parking_lot::Mutex::default(),
            dialing: KeyedLocks::default(),
            idle: parking_lot::Mutex::new(PLAY_IDLE),
        }
    }
}

#[derive(Debug)]
struct Pooled {
    client: Arc<PlayClient>,
    /// The plays on it, and the starts on their way.
    plays: usize,
}

impl PlayClients {
    /// `peer`'s connection, open, held for a play: only one to the key
    /// the pair is pinned to as the caller read it (C8-01).
    pub(super) fn held(self: &Arc<Self>, peer: &PeerRecord) -> Option<Held> {
        let mut open = self.open.lock();
        let pooled = open.get_mut(&peer.id)?;
        if pooled.client.is_closed() {
            open.remove(&peer.id);
            return None;
        }
        if !peer.pinned_to(&pooled.client.remote_id()) {
            return None;
        }
        pooled.plays += 1;
        Some(Held {
            pool: Arc::downgrade(self),
            peer: peer.id.clone(),
            client: pooled.client.clone(),
        })
    }

    /// `client`, just dialled to `peer`, kept and held for a play.
    fn keep(self: &Arc<Self>, peer: &str, client: Arc<PlayClient>) -> Held {
        let replaced = self.open.lock().insert(
            peer.to_owned(),
            Pooled {
                client: client.clone(),
                plays: 1,
            },
        );
        if let Some(old) = replaced {
            old.client.close();
        }
        Held {
            pool: Arc::downgrade(self),
            peer: peer.to_owned(),
            client,
        }
    }

    /// A play on `peer`'s `client` ended: with none left, the connection
    /// is closed once it has stayed so for the idle time.
    fn release(self: Arc<Self>, peer: &str, client: &Arc<PlayClient>) {
        let idle = {
            let mut open = self.open.lock();
            let Some(pooled) = open
                .get_mut(peer)
                .filter(|p| Arc::ptr_eq(&p.client, client))
            else {
                return;
            };
            pooled.plays = pooled.plays.saturating_sub(1);
            if pooled.plays > 0 {
                return;
            }
            *self.idle.lock()
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (peer, client) = (peer.to_owned(), client.clone());
        runtime.spawn(async move {
            tokio::time::sleep(idle).await;
            let mut open = self.open.lock();
            if open
                .get(&peer)
                .is_some_and(|p| Arc::ptr_eq(&p.client, &client) && p.plays == 0)
            {
                open.remove(&peer);
                client.close();
                tracing::debug!(peer, "a pair's connection with no play on it is closed");
            }
        });
    }

    /// Close `peer`'s connection: the pair is gone. Its plays end with it.
    fn close(&self, peer: &str) {
        if let Some(pooled) = self.open.lock().remove(peer) {
            pooled.client.close();
        }
    }

    /// Close `client`, and forget it as `peer`'s if it still is.
    fn forget(&self, peer: &str, client: &Arc<PlayClient>) {
        super::forget_kept(&mut self.open.lock(), peer, client, |p| &p.client);
        client.close();
    }

    /// How many connections are kept.
    fn len(&self) -> usize {
        self.open.lock().len()
    }
}

/// A play's hold on its pair's connection: dropped, the play is off it.
/// A download's call or read (P8-07) holds it the same way.
#[derive(Debug)]
pub(super) struct Held {
    pool: std::sync::Weak<PlayClients>,
    peer: String,
    pub(super) client: Arc<PlayClient>,
}

impl Drop for Held {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.upgrade() {
            pool.release(&self.peer, &self.client);
        }
    }
}

/// A playback SB plays from SA (proxy mode): SA's session and playback,
/// on SB's connection to SA. Dropped, SA's playback is stopped and its
/// session ended.
#[derive(Debug)]
pub struct RemotePlay {
    client: Arc<PlayClient>,
    /// SA's session, from the redeemed token.
    session: SessionId,
    /// SA's handle.
    handle: PlaybackHandle,
    /// SA's playback's revision, as its last answer said.
    revision: AtomicU64,
    /// The pair's row here.
    peer: String,
    /// Told once it is dropped: what follows it stops.
    ended: Arc<Notify>,
    /// Its hold on the connection, let go of after its session's end.
    held: Option<Held>,
}

/// How long SA's bytes may stop coming before a relayed read fails: a
/// paired server that stalls must not hold the device's read for ever.
pub const ORIGIN_STALL: Duration = Duration::from_secs(30);

/// A read SA answered: its bytes follow on `stream`.
pub struct RelayedRead {
    pub length: u64,
    pub size: u64,
    pub complete: bool,
    pub epoch: u32,
    pub stream: iroh::endpoint::RecvStream,
}

/// `inner`, failing with `TimedOut` once a read of it waits `stall` with
/// nothing coming.
pub struct Stalling<R> {
    inner: R,
    stall: Duration,
    deadline: Pin<Box<tokio::time::Sleep>>,
}

impl<R> Stalling<R> {
    pub fn new(inner: R, stall: Duration) -> Self {
        Self {
            inner,
            stall,
            deadline: Box::pin(tokio::time::sleep(stall)),
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for Stalling<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(read) => {
                let next = tokio::time::Instant::now() + this.stall;
                this.deadline.as_mut().reset(next);
                Poll::Ready(read)
            }
            Poll::Pending => match this.deadline.as_mut().poll(cx) {
                Poll::Ready(()) => Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("the paired server sent nothing for {:?}", this.stall),
                ))),
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

impl RemotePlay {
    /// The pair's row here.
    pub fn peer(&self) -> &str {
        &self.peer
    }

    /// SA's handle: what its events name.
    pub fn handle(&self) -> &PlaybackHandle {
        &self.handle
    }

    /// SA's playback's revision, as its last answer said.
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    /// The connection to SA: its events, and when it closes.
    pub fn client(&self) -> &Arc<PlayClient> {
        &self.client
    }

    /// Told once the play ends here.
    pub fn ended(&self) -> Arc<Notify> {
        self.ended.clone()
    }

    /// `request`, a control of SB's playback, on SA's: its handle and
    /// revision SA's, and SA's answer.
    pub async fn control(&self, mut request: Request) -> Result<Reply, Error> {
        let revised = match &mut request {
            Request::ChangeSubtitle { handle, .. }
            | Request::ChangeQuality { handle, .. }
            | Request::SeekPlayback { handle, .. }
            | Request::StopPlayback { handle } => {
                *handle = self.handle.clone();
                true
            }
            Request::UpdatePlaybackProgress { handle, .. }
            | Request::GetManifest { handle, .. }
            | Request::ReadSubtitle { handle, .. }
            | Request::ReadSubtitleSegment { handle, .. } => {
                *handle = self.handle.clone();
                false
            }
            _ => {
                return Err(Error::InvalidInput {
                    message: "not a playback's control".into(),
                });
            }
        };
        // SB checked the device's revision against its own playback's; a
        // revision SA moved on from by itself is asked again at SA's, once.
        for _ in 0..2 {
            let expected = revised.then(|| self.revision.load(Ordering::Acquire));
            let reply = self
                .client
                .call(Some(&self.session), request.clone(), expected)
                .await
                .map_err(origin_error)?;
            if let Some(playback) = reply.playback {
                self.revision.store(playback.revision, Ordering::Release);
            }
            match reply.response {
                Response::Ok(reply) => return Ok(reply),
                Response::Err(Error::RevisionConflict { revision }) => {
                    self.revision.store(revision, Ordering::Release);
                }
                Response::Err(e) => return Err(e),
            }
        }
        Err(Error::Busy)
    }

    /// SA's manifest of `kind` for `variant`.
    pub async fn manifest(&self, kind: ManifestKind, variant: u32) -> Result<String, Error> {
        let request = Request::GetManifest {
            handle: self.handle.clone(),
            kind,
            variant,
        };
        match self.control(request).await? {
            Reply::Manifest { text } => Ok(text),
            other => Err(answered_with("a manifest", &other)),
        }
    }

    /// SA's subtitle segment `n`.
    pub async fn subtitle_segment(&self, n: u32) -> Result<Vec<u8>, Error> {
        let request = Request::ReadSubtitleSegment {
            handle: self.handle.clone(),
            n,
        };
        match self.control(request).await? {
            Reply::Subtitle { bytes, .. } => Ok(bytes),
            other => Err(answered_with("a subtitle segment", &other)),
        }
    }

    /// `read` of SB's playback, from SA's: its bytes to follow on.
    pub async fn read(&self, read: &MediaRead) -> Result<RelayedRead, Error> {
        let request = MediaRead {
            session: self.session.clone(),
            handle: self.handle.clone(),
            ..read.clone()
        };
        // A revision SA moved on from is asked again at its own, once.
        for _ in 0..2 {
            let (response, stream) = self
                .client
                .resource(&ResourceRequest::Media {
                    request: request.clone(),
                    revision: self.revision.load(Ordering::Acquire),
                })
                .await
                .map_err(origin_error)?;
            match response {
                ResourceResponse::Ready {
                    length,
                    metadata:
                        ResourceMetadata::Media {
                            size,
                            complete,
                            epoch,
                            ..
                        },
                } => {
                    let length = within_read(&request, length, size)?;
                    return Ok(RelayedRead {
                        length,
                        size,
                        complete,
                        epoch,
                        stream,
                    });
                }
                ResourceResponse::Ready { .. } => {
                    return Err(Error::Unavailable {
                        message: "the paired server answered a read with something else".into(),
                    });
                }
                ResourceResponse::Error {
                    error: Error::RevisionConflict { revision },
                } => self.revision.store(revision, Ordering::Release),
                ResourceResponse::Error { error } => return Err(error),
            }
        }
        Err(Error::Busy)
    }
}

/// SA's `length` for `read`, if it is one this server's own reads could
/// answer: no more than asked, nor than [`MAX_READ`], nor past the `size`
/// SA gives from the read's offset (F8-01). The device is told it, the
/// relay sends that much of SA's stream on, and the HTTP side holds it
/// whole: a paired server that says more is not believed, nor read.
fn within_read(read: &MediaRead, length: u64, size: u64) -> Result<u64, Error> {
    let most = read
        .length
        .min(MAX_READ)
        .min(size.saturating_sub(read.offset));
    if length > most {
        tracing::warn!(
            offset = read.offset,
            asked = read.length,
            length,
            size,
            "a paired server answered a read with more than it could send"
        );
        return Err(Error::Unavailable {
            message: "the paired server answered a read with more than it asked for".into(),
        });
    }
    Ok(length)
}

impl Drop for RemotePlay {
    fn drop(&mut self) {
        self.ended.notify_one();
        let client = self.client.clone();
        let session = self.session.clone();
        let handle = self.handle.clone();
        let revision = self.revision.load(Ordering::Acquire);
        let held = self.held.take();
        // Its session's end ends SA's playback; stopped first, it ends at
        // once as a stop, not as a sign-out. The connection carries the
        // pair's other plays, and stays until it has none for a while.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = client
                    .call(
                        Some(&session),
                        Request::StopPlayback { handle },
                        Some(revision),
                    )
                    .await;
                let _ = client.call(Some(&session), Request::Logout, None).await;
                drop(held);
            });
        }
    }
}

impl Federation {
    /// Play `start` from the paired server whose item it is, for `who`:
    /// SA's playback, and its answer to the start.
    pub async fn start_proxy(
        &self,
        who: &Who,
        start: ProxyStart,
    ) -> Result<ProxyStarted, ProxyError> {
        let (peer, remote) = self.remote_peer(&start.item).await?;
        let endpoint = self
            .attached
            .get()
            .map(|a| a.endpoint.clone())
            .ok_or(AssertError::Offline)?;
        // The token is for this server's own endpoint: SA counts the play
        // as proxied, and only this server may redeem it. A device that was
        // granted its own token for this play, and could not reach SA with
        // it, plays the same play: counted once, as SA counts it once.
        let counted = !self.took_device_grant(who, &start.item, start.device);
        let granted = self
            .assert_play_for(who, &peer, &start.item, endpoint.id(), counted)
            .await?;
        let held = self.play_client(&endpoint, &peer).await?;
        let client = held.client.clone();
        let started = async {
            let session = redeem(&client, granted.token).await?;
            // Ended here, its session is ended there: a start that fails
            // leaves none behind.
            let mut play = RemotePlay {
                client: client.clone(),
                session,
                handle: PlaybackHandle(String::new()),
                revision: AtomicU64::new(0),
                peer: peer.id.clone(),
                ended: Arc::default(),
                held: Some(held),
            };
            let answered = client
                .call(
                    Some(&play.session),
                    Request::StartPlayback {
                        id: remote,
                        selection: start.selection,
                        start_ms: start.start_ms,
                        bitmap: start.bitmap,
                        quality: start.quality,
                        display: start.display,
                        capabilities: start.capabilities,
                        onward_share: start.onward_share,
                    },
                    None,
                )
                .await?;
            let revision = answered.playback.map_or(0, |p| p.revision);
            match answered.response {
                Response::Ok(Reply::PlaybackStarted {
                    handle,
                    plan,
                    size,
                    chosen,
                    epoch,
                    markers,
                }) => {
                    play.handle = handle;
                    play.revision = AtomicU64::new(revision);
                    Ok(ProxyStarted {
                        remote: Arc::new(play),
                        plan,
                        size,
                        chosen,
                        epoch,
                        markers,
                    })
                }
                Response::Ok(other) => Err(ProxyError::Origin(answered_with("a start", &other))),
                Response::Err(e) => Err(ProxyError::Origin(e)),
            }
        }
        .await;
        match &started {
            Ok(_) => {
                // Its own uplink carries it: the Federation page says how
                // many of its users' plays of the pair's items went so.
                if let Err(e) = self.store().count_federation_proxied(&peer.id).await {
                    tracing::warn!(error = %e, "cannot count a play through this server");
                }
                tracing::info!(
                    peer = %peer.endpoint_id,
                    item = %start.item,
                    "a paired server's item plays through this server"
                );
            }
            Err(e) => tracing::info!(
                peer = %peer.endpoint_id,
                item = %start.item,
                error = %e,
                "a paired server's item cannot play through this server"
            ),
        }
        started
    }

    /// This server's connection to `peer`'s client protocol, held for a
    /// play: the one open to its key, or a new one. One whose key answers
    /// under another server id is not the pair (C8-06): refused, before
    /// any token is sent on it.
    pub(super) async fn play_client(
        &self,
        endpoint: &iroh::Endpoint,
        peer: &PeerRecord,
    ) -> Result<Held, ProxyError> {
        if let Some(held) = self.plays.held(peer) {
            return Ok(held);
        }
        let _dialing = self.plays.dialing.lock(&peer.id).await;
        // Read again under the lock every dial to it takes: a key
        // confirmed since the caller read it is dialled, not the old one.
        let not_paired = || AssertError::NotPaired(peer.server_id.clone());
        let peer = &self
            .pair_now(peer)
            .await
            .map_err(AssertError::from)?
            .ok_or_else(not_paired)?;
        if let Some(held) = self.plays.held(peer) {
            return Ok(held);
        }
        let addr = peer_addr(self.relay_hosts, peer).ok_or_else(not_paired)?;
        // SA sees this server as the device; what each play plans for is
        // its start's, and each download its own device's (P8-07), whose
        // news this connection hears.
        let hello = self.client_hello(vec![
            spoolframe_protocol::features::DOWNLOADS.into(),
            spoolframe_protocol::features::MUSIC_DOWNLOADS.into(),
        ]);
        let (client, info) = PlayClient::connect(endpoint, addr, hello).await?;
        if info.server_id != peer.server_id {
            client.close();
            self.identity_refused(peer, &info.server_id).await;
            return Err(not_paired().into());
        }
        #[cfg(test)]
        self.holds.dialing.here().await;
        // Kept, then checked against the pin, as the replica's connection
        // is (`replica.rs`): a key confirmed meanwhile closes it either way.
        let held = self.plays.keep(&peer.id, client.clone());
        if !self.still_pinned(peer).await.map_err(AssertError::from)? {
            self.plays.forget(&peer.id, &client);
            return Err(not_paired().into());
        }
        tokio::spawn(super::downloads::forward_download_news(
            self.store().clone(),
            self.downloads.get().cloned(),
            self.download_sessions.clone(),
            peer.server_id.clone(),
            Arc::downgrade(&client),
            client.events(),
        ));
        Ok(held)
    }

    /// Close this server's connection to pair `peer`'s client protocol
    /// (its row here): the pair is gone, and the plays on it end.
    pub(crate) fn close_plays(&self, peer: &str) {
        self.plays.close(peer);
    }

    /// How many of the pairs' client protocols this server holds a
    /// connection to, for its plays through it.
    pub fn play_connections(&self) -> usize {
        self.plays.len()
    }

    /// How long a pair's connection with no play on it is kept
    /// (a minute unless set): for tests.
    pub fn set_play_idle(&self, idle: Duration) {
        *self.plays.idle.lock() = idle;
    }
}

/// What the device is told when SA did not answer SB as asked: SA's own
/// refusal as it is (P8-07's subtitle asks), offline when SA cannot be
/// reached, else that it cannot be reached now. What went wrong on the
/// way (an address, a broken stream) is logged here, never told the
/// device.
pub(super) fn origin_error(e: DialError) -> Error {
    match e {
        DialError::Failed(error) => error,
        DialError::Unreachable(_) => Error::OriginOffline,
        other => {
            tracing::info!(error = %other, "a paired server did not answer as asked");
            Error::Unavailable {
                message: "the paired server cannot be reached now".into(),
            }
        }
    }
}

/// `token` redeemed on SA's client protocol over `client`: the session it
/// opens there (P8-04, P8-07).
pub(super) async fn redeem(client: &PlayClient, token: String) -> Result<SessionId, ProxyError> {
    let redeemed = client
        .call(
            None,
            Request::RedeemPlayToken(RedeemPlayToken { token }),
            None,
        )
        .await?;
    match redeemed.response {
        Response::Ok(Reply::PlayTokenRedeemed(redeemed)) => Ok(redeemed.session),
        Response::Ok(other) => Err(ProxyError::Origin(answered_with("a redemption", &other))),
        Response::Err(e) => Err(ProxyError::Origin(e)),
    }
}

/// SA answered `what` with `other`.
pub(super) fn answered_with(what: &str, other: &Reply) -> Error {
    Error::Unavailable {
        message: format!("the paired server answered {what} with {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// SA's own refusal reaches the device as it is; what failed on the
    /// way does not: the device is told SA cannot be reached now.
    #[test]
    fn the_device_hears_sa_s_refusal_but_no_transport_error() {
        assert_eq!(
            origin_error(DialError::Failed(Error::NoSubtitleSource)),
            Error::NoSubtitleSource
        );
        assert_eq!(
            origin_error(DialError::Unreachable("10.0.0.7:4433".into())),
            Error::OriginOffline
        );
        let told = origin_error(DialError::Broken("stream reset by 10.0.0.7:4433".into()));
        assert_eq!(
            told,
            Error::Unavailable {
                message: "the paired server cannot be reached now".into()
            }
        );
    }

    fn read_of(offset: u64, length: u64) -> MediaRead {
        MediaRead {
            session: SessionId("s".into()),
            handle: PlaybackHandle("h".into()),
            offset,
            length,
            epoch: 0,
            segment: None,
            variant: 0,
        }
    }

    /// F8-01: SA's answer to a read is held to what this server's own
    /// would be. Asked for 16 MiB of a 1 GiB file, an answer of that, of
    /// less, or of the file's last bytes passes; more than asked, than
    /// `MAX_READ`, or than is left past the offset does not, whatever the
    /// numbers, and nothing overflows.
    #[test]
    fn a_paired_server_s_read_is_no_longer_than_asked_or_than_the_file() {
        const GIB: u64 = 1 << 30;
        let asked = read_of(0, MAX_READ);
        assert_eq!(within_read(&asked, MAX_READ, GIB), Ok(MAX_READ));
        assert_eq!(within_read(&asked, 100, GIB), Ok(100));
        assert_eq!(within_read(&read_of(GIB - 10, MAX_READ), 10, GIB), Ok(10));
        for (read, length, size) in [
            (read_of(0, MAX_READ), MAX_READ + 1, GIB),
            (read_of(0, MAX_READ), u64::MAX, u64::MAX),
            (read_of(0, 100), 101, GIB),
            // A device asking past MAX_READ is still answered no more.
            (read_of(0, u64::MAX), MAX_READ + 1, u64::MAX),
            (read_of(GIB - 10, MAX_READ), 11, GIB),
            (read_of(u64::MAX, MAX_READ), 1, u64::MAX),
            (read_of(u64::MAX - 1, MAX_READ), MAX_READ, u64::MAX),
            (read_of(2 * GIB, MAX_READ), 1, GIB),
        ] {
            assert!(
                matches!(
                    within_read(&read, length, size),
                    Err(Error::Unavailable { .. })
                ),
                "{} + {length} of {size}",
                read.offset
            );
        }
    }

    /// A paired server that stops sending fails the read once it has sent
    /// nothing for the stall, not at once, and not never.
    #[tokio::test(start_paused = true)]
    async fn a_read_of_a_stalled_paired_server_fails_after_the_stall() {
        let (mut origin, relayed) = tokio::io::duplex(64);
        let mut reader = Stalling::new(relayed, Duration::from_secs(30));
        origin.write_all(b"some").await.unwrap();
        let mut buf = [0u8; 4];
        reader.read_exact(&mut buf).await.unwrap();
        let started = tokio::time::Instant::now();
        let stalled = reader.read(&mut buf).await.unwrap_err();
        assert_eq!(stalled.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(started.elapsed(), Duration::from_secs(30));
        drop(origin);
    }
}
