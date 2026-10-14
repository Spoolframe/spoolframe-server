//! The HTTP playback port (PX-49): a playback's HLS, or its file, over
//! plain HTTP, for a player with no Iroh: an AirPlay receiver first, a TV
//! player later.
//!
//! Off unless `http_playback.listen` is set. Nothing is served without a
//! token: a signed-in client asks for a URL for one of its playbacks over
//! Iroh (`HttpPlaybackUrl`, the `http-playback` experimental feature), and
//! the token in it is bound to that playback. It ends when the playback
//! does (its client stops it, an admin does, P3-15, it idles out), and
//! after [`Settings::idle`] with no request. A wrong or ended token is
//! `404`; there is no listing and no other route.
//!
//! A playback with a live token outlives its device's connection: a phone
//! that AirPlays and locks leaves, and the Apple TV plays on. It is not
//! ended when its device is gone past the grace, as other playbacks are,
//! but as any playback ends: stopped, signed out, or idle.
//!
//! The playback is the one the client planned and plays: the same encode,
//! counted once against the encode budget, read here from the transcode
//! and segment store as the client's own reads are, with no QUIC hop. A
//! read here keeps it playing, as the client's own reads do.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use spoolframe_hls_bridge::{
    Bridge, Chunk, Error as BridgeError, Kind, ManifestKind, Segment, Source, Stream, Subtitle,
};
use spoolframe_protocol::experimental::HttpPlaybackRefusal;
use spoolframe_protocol::{Error, MediaRead, PlaybackHandle, PlaybackPlan, SessionId};
use spoolframe_types::{Container, Delivery};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::client::{
    RangeOf, converted_range, direct_range, subtitle_segment_of, use_playback_checked,
};
use crate::clock::Clock;
use crate::encode::Conversion;
use crate::network::Services;
use crate::playback::{Playback, Sidecar, Using};

/// How long a token lives with no request (PX-49): as long as an idle
/// playback does ([`crate::playback::IDLE`]), so a TV paused for a while
/// finds its URL, and its playback, still there.
pub const IDLE: Duration = crate::playback::IDLE;

/// The most a piece of a segment read from its file holds, handed to the
/// player as it is read.
const PIECE: u64 = 256 << 10;

/// `http_playback` (PX-49).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Where the port listens; `None`, the default: off, and nothing
    /// listens.
    pub listen: Option<SocketAddr>,
    /// The host a URL names for a client that reached the server through a
    /// relay or over the Internet (`http_playback.address`); `None`: such a
    /// client gets no URL. A client on the LAN gets the address it reached
    /// the server at.
    pub address: Option<String>,
    /// How long a token lives with no request.
    pub idle: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            listen: None,
            address: None,
            idle: IDLE,
        }
    }
}

/// What a token serves: its playback's shape when it was made. A token
/// for another shape (a re-plan from direct play to HLS) is made anew.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Direct,
    Hls,
}

/// The port, listening, and the tokens it serves.
pub struct HttpPlayback {
    bridge: Bridge<ServerSource>,
    addr: SocketAddr,
    settings: Settings,
    services: Services,
    clock: Clock,
    /// Each playback's token, by handle, and what it serves.
    tokens: parking_lot::Mutex<HashMap<String, (String, Shape)>>,
    /// When each token was last asked for anything, or made, by the
    /// server's clock: the bridge says when a request comes.
    asked: Arc<parking_lot::Mutex<HashMap<String, std::time::Instant>>>,
    tasks: parking_lot::Mutex<Vec<JoinHandle<()>>>,
}

impl std::fmt::Debug for HttpPlayback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpPlayback")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl HttpPlayback {
    /// Listen as `settings` say, serving `services`' playbacks; `None` when
    /// the port is off.
    pub async fn start(
        settings: &Settings,
        services: &Services,
    ) -> std::io::Result<Option<Arc<Self>>> {
        let Some(listen) = settings.listen else {
            return Ok(None);
        };
        let listener = TcpListener::bind(listen).await?;
        let addr = listener.local_addr()?;
        let clock = services.auth.store().clock().clone();
        let asked: Arc<parking_lot::Mutex<HashMap<String, std::time::Instant>>> = Arc::default();
        let hearing = (asked.clone(), clock.clone());
        let bridge = Bridge::new().on_request(move |token| {
            let (asked, clock) = &hearing;
            // Only a token still served; one swept meanwhile stays gone.
            if let Some(at) = asked.lock().get_mut(token) {
                *at = clock.now();
            }
        });
        let this = Arc::new(Self {
            bridge,
            addr,
            settings: settings.clone(),
            services: services.clone(),
            clock,
            tokens: parking_lot::Mutex::default(),
            asked,
            tasks: parking_lot::Mutex::default(),
        });
        let bridge = this.bridge.clone();
        let serving = tokio::spawn(async move { bridge.serve(listener).await });
        let every = (settings.idle / 4).clamp(Duration::from_secs(1), Duration::from_secs(30));
        let weak = Arc::downgrade(&this);
        let sweeping = tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let Some(this) = weak.upgrade() else {
                    return;
                };
                this.sweep();
            }
        });
        // A playback's token ends with it, however it ends.
        let weak = Arc::downgrade(&this);
        services.playbacks.on_end(Box::new(move |handle| {
            if let Some(this) = weak.upgrade() {
                this.forget(handle);
            }
        }));
        *this.tasks.lock() = vec![serving, sweeping];
        tracing::info!(%addr, "HTTP playback listening");
        Ok(Some(this))
    }

    /// Where it listens.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stop listening, and end every token.
    pub fn stop(&self) {
        for task in self.tasks.lock().drain(..) {
            task.abort();
        }
        let tokens: Vec<(String, Shape)> = self.tokens.lock().drain().map(|(_, t)| t).collect();
        for (token, _) in tokens {
            self.end(&token);
        }
    }

    /// The URL `handle`'s `playback` is fetched at, for its own session and
    /// device: by `lan`, the server's address on the client's LAN path, if
    /// it reached the server by one; else `http_playback.address`. Its
    /// live token again if it has one for the same shape.
    pub(crate) fn url(
        &self,
        handle: &PlaybackHandle,
        playback: &Playback,
        lan: Option<IpAddr>,
    ) -> Result<String, HttpPlaybackRefusal> {
        let host = match (lan, &self.settings.address) {
            (Some(IpAddr::V6(ip)), _) => format!("[{ip}]"),
            (Some(ip), _) => ip.to_string(),
            (None, Some(address)) => address.clone(),
            (None, None) => return Err(HttpPlaybackRefusal::RelayOnly),
        };
        let shape = match &playback.conversion {
            None => Shape::Direct,
            Some(conversion) if conversion.delivery == Delivery::Hls => Shape::Hls,
            Some(_) => return Err(HttpPlaybackRefusal::Unsupported),
        };
        let token = {
            let mut tokens = self.tokens.lock();
            let live = tokens.get(&handle.0).filter(|(token, made)| {
                *made == shape && self.bridge.last_request(token).is_some()
            });
            match live {
                Some((token, _)) => token.clone(),
                None => {
                    if let Some((old, _)) = tokens.remove(&handle.0) {
                        self.end(&old);
                    }
                    let token = self.register(handle, playback, shape);
                    tokens.insert(handle.0.clone(), (token.clone(), shape));
                    token
                }
            }
        };
        // Ended while its token was made: its end did not see the token.
        if self.services.playbacks.item(handle).is_none() {
            self.forget(&handle.0);
            return Err(HttpPlaybackRefusal::NoSuchPlayback);
        }
        let name = match shape {
            Shape::Direct => "file",
            Shape::Hls => "master.m3u8",
        };
        Ok(format!("http://{host}:{}/{token}/{name}", self.addr.port()))
    }

    /// Serve `playback` as `shape` under a new token, made as session
    /// tokens are.
    fn register(&self, handle: &PlaybackHandle, playback: &Playback, shape: Shape) -> String {
        let source = ServerSource(Arc::new(Inner {
            services: self.services.clone(),
            handle: handle.clone(),
            session: playback.session.clone(),
            endpoint: playback.endpoint.clone(),
            revision: watch::channel(playback.revision).0,
            marked: Default::default(),
        }));
        let kind = || match shape {
            Shape::Direct => Kind::direct(playback.size, mime(&playback.planned)),
            Shape::Hls => Kind::hls(),
        };
        loop {
            let token = crate::auth::SessionToken::generate().0;
            let stream = Stream::new(
                source.clone(),
                kind(),
                playback.selection.subtitle,
                Default::default(),
            );
            // Its time first: a request may come as soon as it is served.
            self.asked.lock().insert(token.clone(), self.clock.now());
            if self.bridge.register_as(&token, stream) {
                return token;
            }
            self.asked.lock().remove(&token);
        }
    }

    /// Whether `handle`'s playback has a token asked for within
    /// [`Settings::idle`]: something plays it over HTTP, so its device may
    /// leave without ending it.
    pub fn holds(&self, handle: &str) -> bool {
        let Some((token, _)) = self.tokens.lock().get(handle).cloned() else {
            return false;
        };
        let now = self.clock.now();
        self.asked
            .lock()
            .get(&token)
            .is_some_and(|at| now.saturating_duration_since(*at) < self.settings.idle)
    }

    /// End `handle`'s token: its playback ended.
    fn forget(&self, handle: &str) {
        let gone = self.tokens.lock().remove(handle);
        if let Some((token, _)) = gone {
            self.end(&token);
        }
    }

    /// Serve `token` no more: `404` from now, and its requests under way
    /// end.
    fn end(&self, token: &str) {
        self.asked.lock().remove(token);
        self.bridge.unregister(token);
    }

    /// End every token not asked for anything in [`Settings::idle`], by
    /// the server's clock (tests move it on).
    pub fn sweep(&self) {
        let now = self.clock.now();
        let mut tokens = self.tokens.lock();
        let idle: Vec<String> = {
            let asked = self.asked.lock();
            tokens
                .iter()
                .filter(|(_, (token, _))| {
                    asked
                        .get(token)
                        .is_none_or(|at| now.saturating_duration_since(*at) >= self.settings.idle)
                })
                .map(|(handle, _)| handle.clone())
                .collect()
        };
        for handle in idle {
            if let Some((token, _)) = tokens.remove(&handle) {
                self.end(&token);
                tracing::debug!("an idle HTTP playback token ended");
            }
        }
    }
}

/// The type of a direct playback's file, by its container.
fn mime(plan: &PlaybackPlan) -> &'static str {
    match plan {
        PlaybackPlan::Direct { container, .. } => match container {
            Container::Mp4 => "video/mp4",
            Container::Matroska => "video/x-matroska",
            Container::MpegTs => "video/mp2t",
            Container::Avi => "video/x-msvideo",
            Container::Flac => "audio/flac",
            Container::Mp3 => "audio/mpeg",
            Container::Ogg => "audio/ogg",
            _ => "application/octet-stream",
        },
        _ => "application/octet-stream",
    }
}

/// A playback as the HTTP port reads it: as its own client does, with its
/// session and device, and no QUIC hop.
#[derive(Clone)]
pub struct ServerSource(Arc<Inner>);

struct Inner {
    services: Services,
    handle: PlaybackHandle,
    session: SessionId,
    endpoint: String,
    /// The playback's configuration revision as last read: it moves on a
    /// seek or a re-plan, and a segment read begun before is not
    /// continued after.
    revision: watch::Sender<u64>,
    /// The playback's activity row says it is played over HTTP.
    marked: std::sync::atomic::AtomicBool,
}

impl ServerSource {
    /// What a read uses of the playback, as its client's own read would.
    async fn using<T>(&self, read: impl FnOnce(&Playback) -> (u64, T)) -> Result<T, BridgeError> {
        let inner = &self.0;
        let (revision, read) = use_playback_checked(
            &inner.services,
            &inner.handle,
            &inner.session,
            &inner.endpoint,
            Using::Plays,
            read,
        )
        .await
        .map_err(bridge_error)?;
        inner.revision.send_if_modified(|seen| {
            let moved = *seen != revision;
            *seen = revision;
            moved
        });
        if !inner
            .marked
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            inner.services.playbacks.served_over_http(&inner.handle);
        }
        Ok(read)
    }

    async fn range(&self) -> Result<RangeOf, BridgeError> {
        self.using(|p| (p.revision, RangeOf::of(p))).await
    }

    /// The playback's conversion; none any more is a token for the plan
    /// it had: ended, and the client asks for another.
    async fn conversion(&self) -> Result<Arc<Conversion>, BridgeError> {
        self.range().await?.conversion.ok_or(BridgeError::Expired)
    }

    fn media_read(
        &self,
        offset: u64,
        length: u64,
        segment: Option<u32>,
        variant: u32,
    ) -> MediaRead {
        MediaRead {
            session: self.0.session.clone(),
            handle: self.0.handle.clone(),
            offset,
            length,
            epoch: 0,
            segment,
            variant,
        }
    }

    /// Segment `n` of `variant` from `offset`, its file open where the
    /// bytes start.
    async fn open(
        &self,
        variant: u32,
        n: u32,
        offset: u64,
        length: u64,
    ) -> Result<ServerSegment, BridgeError> {
        let conversion = self.conversion().await?;
        let read = self.media_read(offset, length, Some(n), variant);
        let playbacks = &self.0.services.playbacks;
        let keep_alive = || playbacks.touch(&self.0.handle);
        let (file, reply) = converted_range(&conversion, &read, &keep_alive)
            .await
            .map_err(bridge_error)?;
        playbacks.read(&self.0.handle, offset, reply.sent, Some(n));
        Ok(ServerSegment {
            source: self.clone(),
            variant,
            n,
            file,
            left: reply.sent,
            sent: reply.sent,
            size: reply.size,
            complete: reply.complete,
            epoch: reply.epoch,
            end: offset + reply.sent,
        })
    }
}

impl Source for ServerSource {
    type Snapshot = u64;
    type Segment = ServerSegment;

    fn changes(&self) -> watch::Receiver<u64> {
        self.0.revision.subscribe()
    }

    /// A re-plan keeps the handle, and the token serves the handle.
    fn replanned(&self) -> bool {
        false
    }

    async fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>, BridgeError> {
        let range = self.range().await?;
        if range.conversion.is_some() {
            // Planned again into a conversion: this token's file is gone.
            return Err(BridgeError::Expired);
        }
        let read = self.media_read(offset, length, None, 0);
        let (file, reply) = direct_range(&range, &read).await.map_err(bridge_error)?;
        let bytes = read_all(file, reply.sent).await?;
        self.0
            .services
            .playbacks
            .read(&self.0.handle, offset, reply.sent, None);
        Ok(bytes)
    }

    async fn manifest(&self, kind: ManifestKind, variant: u32) -> Result<String, BridgeError> {
        let kind = match kind {
            ManifestKind::Multivariant => spoolframe_protocol::ManifestKind::Multivariant,
            ManifestKind::Media => spoolframe_protocol::ManifestKind::Media,
            ManifestKind::Subtitles => spoolframe_protocol::ManifestKind::Subtitles,
        };
        self.conversion()
            .await?
            .manifest(kind, variant)
            .ok_or(BridgeError::NotFound)
    }

    async fn read_segment(
        &self,
        variant: u32,
        n: u32,
        offset: u64,
        length: u64,
    ) -> Result<Chunk, BridgeError> {
        let segment = self.open(variant, n, offset, length).await?;
        let (size, complete, epoch) = (segment.size, segment.complete, segment.epoch);
        let bytes = read_all(segment.file, segment.left).await?;
        Ok(Chunk {
            bytes,
            size,
            complete,
            epoch,
        })
    }

    async fn segment_stream(
        &self,
        variant: u32,
        n: u32,
        offset: u64,
        length: u64,
    ) -> Result<ServerSegment, BridgeError> {
        self.open(variant, n, offset, length).await
    }

    async fn read_subtitle_segment(&self, n: u32) -> Result<Subtitle, BridgeError> {
        let (conversion, subtitle): (Option<Arc<Conversion>>, Option<Sidecar>) = self
            .using(|p| (p.revision, (p.conversion.clone(), p.subtitle.clone())))
            .await?;
        let bytes = subtitle_segment_of(conversion.as_deref(), subtitle, n)
            .await
            .map_err(bridge_error)?;
        Ok(Subtitle { bytes })
    }
}

/// One answer for a segment, read from its file piece by piece.
pub struct ServerSegment {
    source: ServerSource,
    variant: u32,
    n: u32,
    file: tokio::fs::File,
    /// Of `sent`, the bytes not yet handed on.
    left: u64,
    sent: u64,
    size: u64,
    complete: bool,
    epoch: u32,
    /// Where in the segment this answer ends.
    end: u64,
}

impl Segment for ServerSegment {
    fn sent(&self) -> u64 {
        self.sent
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn complete(&self) -> bool {
        self.complete
    }

    fn epoch(&self) -> u32 {
        self.epoch
    }

    async fn next(&mut self) -> Result<Option<Vec<u8>>, BridgeError> {
        if self.left == 0 {
            return Ok(None);
        }
        let want = self.left.min(PIECE);
        let mut piece = Vec::with_capacity(want as usize);
        let got = (&mut self.file)
            .take(want)
            .read_to_end(&mut piece)
            .await
            .map_err(|e| BridgeError::Other(format!("cannot read a segment: {e}")))?;
        if got == 0 {
            return Err(BridgeError::Other("a segment ended short".into()));
        }
        self.left -= got as u64;
        Ok(Some(piece))
    }

    async fn reopen(&self, length: u64) -> Result<Self, BridgeError> {
        let next = self
            .source
            .open(self.variant, self.n, self.end, length)
            .await?;
        if next.epoch != self.epoch {
            return Err(BridgeError::Restarted { epoch: next.epoch });
        }
        Ok(next)
    }
}

/// The `sent` bytes `file` is open at, whole: a file shorter than the
/// range read said is an error, not a short answer.
async fn read_all(file: tokio::fs::File, sent: u64) -> Result<Vec<u8>, BridgeError> {
    let mut bytes = Vec::with_capacity(sent as usize);
    file.take(sent)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| BridgeError::Other(format!("cannot read the playback: {e}")))?;
    if (bytes.len() as u64) < sent {
        return Err(BridgeError::Other("the playback's file ended short".into()));
    }
    Ok(bytes)
}

/// What the HTTP side makes of a read's error: a player is told a
/// playback is gone (`410`), not there (`404`), not ready (`503`), or the
/// rest (`502`).
fn bridge_error(error: Error) -> BridgeError {
    match error {
        Error::PlaybackRestarted { epoch } => BridgeError::Restarted { epoch },
        Error::SubtitleNotReady { done_percent } => BridgeError::NotReady { done_percent },
        Error::SubtitleChanged => BridgeError::NotReady { done_percent: 0 },
        Error::PlaybackStopped => BridgeError::Stopped,
        // Its session ended: as gone for this token as an idled-out one.
        Error::PlaybackExpired | Error::Unauthenticated => BridgeError::Expired,
        Error::NotFound { .. } => BridgeError::NotFound,
        other => BridgeError::Other(other.to_string()),
    }
}
