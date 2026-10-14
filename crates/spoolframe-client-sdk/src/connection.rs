//! One connection to one server, shared by every session on it (§15),
//! and how long its requests may take (P1-15).
//!
//! Every request has a time limit. A connection can die without closing:
//! iOS suspends an app, the server side drops it, and when the app comes
//! back QUIC still counts it as open. A request on it would wait forever,
//! so it waits only so long; then the connection is asked for a `Ping`
//! and, if that goes unanswered too, dropped (see `Client::call`).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::LinkMeter;

use futures_util::StreamExt;
use iroh::endpoint::{
    ConnectError, ConnectWithOptsError, ConnectingError, Connection, ConnectionError, PathEvent,
    PathEventStream, RecvStream, WeakConnectionHandle,
};
use iroh::{Endpoint, EndpointAddr, EndpointId, TransportAddr};
use serde::Serialize;
use serde::de::DeserializeOwned;
use spoolframe_protocol::codec::{CodecError, read_frame, write_frame};
use spoolframe_protocol::streams::bi;
use spoolframe_protocol::{
    ArtworkRequest, ArtworkResponse, CLIENT_ALPN, Hello, MAX_READ, MediaRead, MediaReadResponse,
    PROTOCOL_VERSION, Reply, Request, RequestEnvelope, Response, ServerInfo, SessionId, close,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;

use spoolframe_protocol::policy::{server_wait, wait};
use spoolframe_protocol::resource::{ResourceMetadata, ResourceRequest, ResourceResponse};
use spoolframe_protocol::rpc::{RpcReply, RpcRequest, RpcResponse, ServerInstance};

use crate::client::Device;
use crate::error::SdkError;
use crate::link::ConnectionPath;
use crate::reachability::DialStage;

/// How long a dial may take before the server counts as unreachable.
pub(crate) const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// Local SDK allocation policy, not a new wire restriction. Even an
/// original image must fit this budget before the SDK buffers it.
const MAX_ARTWORK_BYTES: u64 = 64 << 20;

/// How long the SDK waits for a server's answers.
///
/// A request the server is built to hold for a while gets that while on
/// top of [`Timeouts::request`]: `AwaitDeviceSignIn` (the server holds it
/// up to 25 s), reads of a converted playback's output and of subtitles
/// still being made (10 s), requests that may probe the file (30 s), and
/// starting a playback or changing its subtitle (up to 2 minutes: a probe,
/// and the keyframes of a large file on a slow mount).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// How long an answer may take, and the longest pause within a body
    /// of bytes (a media read, an image). Default 15 s.
    pub request: Duration,
    /// How long a `Ping` may take before the connection counts as dead:
    /// in [`crate::Client::resume`], and after a request timed out.
    /// Default 3 s.
    pub ping: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            request: Duration::from_secs(15),
            ping: Duration::from_secs(3),
        }
    }
}

impl Timeouts {
    /// How long `request` may take to be answered.
    pub(crate) fn of(&self, request: &Request) -> Duration {
        self.request + server_wait(request)
    }

    /// How long a media read may take to be answered.
    pub(crate) fn of_read(&self) -> Duration {
        self.request + wait::READ
    }
}

pub(crate) struct ServerConnection {
    pub conn: Connection,
    pub info: ServerInfo,
    pub instance: ServerInstance,
    pub greeted_at: std::time::Instant,
    /// Every address this connection has reached the server at, in the
    /// order first reached: each path iroh opened, while it lasted or
    /// not. Updated for as long as the connection is open.
    pub reached: watch::Receiver<Vec<SocketAddr>>,
}

impl ServerConnection {
    /// Dial `addr`, say `Hello` as `device`, and check that the server is
    /// the one dialled. A failure says at which stage
    /// ([`crate::reachability::DialStage`]).
    pub async fn dial(
        endpoint: &Endpoint,
        addr: EndpointAddr,
        device: &Device,
        experimental: crate::Experimental,
        timeouts: &Timeouts,
    ) -> Result<Self, DialFailed> {
        let expected: EndpointId = addr.id;
        let unreachable = |e: String| SdkError::Unreachable(format!("{expected}: {e}"));
        let conn =
            match tokio::time::timeout(DIAL_TIMEOUT, endpoint.connect(addr, CLIENT_ALPN)).await {
                Err(_) => {
                    return Err(DialFailed {
                        error: unreachable(format!("no answer within {DIAL_TIMEOUT:?}")),
                        stage: DialStage::Timeout,
                    });
                }
                Ok(Err(e)) => {
                    return Err(DialFailed {
                        stage: stage_of(&e),
                        error: refused(&e).unwrap_or_else(|| unreachable(e.to_string())),
                    });
                }
                Ok(Ok(conn)) => conn,
            };
        Self::greet(conn, expected, device, experimental, timeouts)
            .await
            .map_err(|(error, stage)| DialFailed { error, stage })
    }

    /// Say `Hello` on `conn`, just made to `expected`, and check the answer.
    async fn greet(
        conn: Connection,
        expected: EndpointId,
        device: &Device,
        experimental: crate::Experimental,
        timeouts: &Timeouts,
    ) -> Result<Self, (SdkError, DialStage)> {
        let unreachable = |e: String| SdkError::Unreachable(format!("{expected}: {e}"));
        let refusal = |e: SdkError| (e, DialStage::Refused);
        // Followed from here, before anything is sent. Once the server
        // has the connection, it announces each of its addresses; iroh
        // opens a path to every one it can reach, keeps the best, and
        // closes the rest within a millisecond. `Connection::paths` then
        // shows one path, so only the events tell where else the server
        // was reached. They are a small broadcast that drops what is not
        // read in time: the task reads them from now, and cannot miss the
        // first, which come a round trip after the handshake at least.
        let reached = watch::Sender::new(ip_addrs(&conn));
        let receiver = reached.subscribe();
        tokio::spawn(follow_paths(
            conn.weak_handle(),
            conn.path_events(),
            reached,
        ));
        let hello = Request::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: device.client_name.clone(),
            client_version: device.client_version.clone(),
            platform: device.platform,
            device_name: Some(device.name.clone()),
            capabilities: Some(device.capabilities.clone()),
            // What the app asked for (PX-49); the answer names what the
            // server turned on.
            experimental: experimental.names(),
        });
        // A server that takes the connection and then says nothing is as
        // unreachable as one that never answered the dial.
        let answer = match rpc(&conn, None, hello, timeouts.request).await {
            Ok(answer) => answer,
            Err(e) => {
                conn.close(0u32.into(), b"no hello");
                return Err(match e {
                    SdkError::TimedOut(why) => (unreachable(why), DialStage::Hello),
                    e @ SdkError::Unreachable(_) => (e, DialStage::Hello),
                    other => refusal(other),
                });
            }
        };
        let instance = answer.server;
        let mut info = match answer.reply.response {
            Response::Ok(Reply::ServerInfo(info)) => info,
            Response::Ok(other) => {
                return Err(refusal(SdkError::Protocol(format!(
                    "Hello answered with {other:?}"
                ))));
            }
            // The one `Hello` a server refuses is one of a revision it does
            // not speak; it does not say its own.
            Response::Err(spoolframe_protocol::Error::Unsupported { .. }) => {
                conn.close(0u32.into(), b"wrong protocol revision");
                return Err(refusal(SdkError::UpdateRequired {
                    server_protocol: None,
                }));
            }
            Response::Err(e) => return Err(refusal(SdkError::from_server(e))),
        };
        info.web_url = crate::client::web_page(info.web_url.take());
        if info.protocol_version != PROTOCOL_VERSION {
            conn.close(0u32.into(), b"wrong protocol revision");
            return Err(refusal(SdkError::UpdateRequired {
                server_protocol: Some(info.protocol_version),
            }));
        }
        // Iroh has proven the key; the server must also say it is that key,
        // or its name and the pin would disagree.
        if info.endpoint_id != expected.to_string() {
            conn.close(0u32.into(), b"wrong server");
            return Err(refusal(SdkError::WrongServer {
                expected: expected.to_string(),
                actual: info.endpoint_id,
            }));
        }
        // It names the relying party of the server's app passkeys (PX-14):
        // one that is not an id is never pinned or used.
        if let Err(e) = spoolframe_protocol::ServerId::parse(&info.server_id) {
            conn.close(0u32.into(), b"not a server id");
            return Err(refusal(SdkError::Protocol(e.to_string())));
        }
        Ok(Self {
            conn,
            info,
            instance,
            greeted_at: std::time::Instant::now(),
            reached: receiver,
        })
    }

    /// The path packets to the server take now: the selected one.
    pub fn path(&self) -> ConnectionPath {
        let paths = self.conn.paths();
        let Some(selected) = paths.iter().find(|p| p.is_selected()) else {
            return ConnectionPath::Unknown;
        };
        match selected.remote_addr() {
            TransportAddr::Ip(addr) => ConnectionPath::of_ip(*addr),
            TransportAddr::Relay(_) => ConnectionPath::Relay,
            _ => ConnectionPath::Unknown,
        }
    }

    /// Every address this connection has reached the server at
    /// ([`ServerConnection::reached`]) now.
    pub fn reached(&self) -> Vec<SocketAddr> {
        self.reached.borrow().clone()
    }

    /// The server's addresses on this connection's open paths, the one in
    /// use first.
    pub fn remote_addrs(&self) -> Vec<SocketAddr> {
        let paths = self.conn.paths();
        let mut paths: Vec<_> = paths.iter().collect();
        paths.sort_by_key(|p| !p.is_selected());
        paths
            .iter()
            .filter_map(|p| match p.remote_addr() {
                TransportAddr::Ip(addr) => Some(*addr),
                _ => None,
            })
            .collect()
    }
}

/// The server's addresses on `conn`'s open IP paths.
fn ip_addrs(conn: &Connection) -> Vec<SocketAddr> {
    conn.paths()
        .iter()
        .filter_map(|p| match p.remote_addr() {
            TransportAddr::Ip(addr) => Some(*addr),
            _ => None,
        })
        .collect()
}

/// Add each IP address a path of `conn` opens to, is selected at, or
/// closes from to `reached`, until the connection closes or no one reads
/// `reached`. Events dropped while lagging are made up from the paths open
/// then.
async fn follow_paths(
    conn: WeakConnectionHandle,
    mut events: PathEventStream,
    reached: watch::Sender<Vec<SocketAddr>>,
) {
    let closed = conn.closed();
    tokio::pin!(closed);
    loop {
        let found = tokio::select! {
            event = events.next() => match event {
                None => return,
                Some(
                    PathEvent::Opened { remote_addr, .. }
                    | PathEvent::Selected { remote_addr, .. }
                    | PathEvent::Closed { remote_addr, .. },
                ) => match remote_addr {
                    TransportAddr::Ip(addr) => vec![addr],
                    _ => continue,
                },
                Some(PathEvent::Lagged { missed, .. }) => {
                    tracing::debug!(missed, "path events dropped; reading the open paths");
                    match conn.upgrade() {
                        Some(conn) => ip_addrs(&conn),
                        None => return,
                    }
                }
                Some(_) => continue,
            },
            _ = &mut closed => return,
            () = reached.closed() => return,
        };
        reached.send_if_modified(|list| {
            let before = list.len();
            for addr in found {
                if !list.contains(&addr) {
                    list.push(addr);
                }
            }
            list.len() != before
        });
    }
}

impl ServerConnection {
    /// One request in `session`, answered within `limit`.
    pub async fn call(&self, request: &RpcRequest, limit: Duration) -> Result<RpcReply, SdkError> {
        let (_, response): (_, RpcResponse) = exchange(&self.conn, bi::RPC, request, limit).await?;
        Ok(response.reply)
    }

    /// Whether the server answers a `Ping` on this connection within
    /// `limit`. Any answer counts: the connection carries requests.
    pub async fn answers(&self, limit: Duration) -> bool {
        self.conn.close_reason().is_none()
            && rpc(&self.conn, None, Request::Ping, limit).await.is_ok()
    }

    pub async fn resource(
        &self,
        request: &ResourceRequest,
        maximum: u64,
        limit: Duration,
        stall: Duration,
    ) -> Result<(ResourceResponse, Vec<u8>), SdkError> {
        self.resource_metered(request, maximum, limit, stall, Metering::Off)
            .await
    }

    /// [`Self::resource`], its time and bytes counted as `metering`
    /// says (P6-06, PX-46).
    async fn resource_metered(
        &self,
        request: &ResourceRequest,
        maximum: u64,
        limit: Duration,
        stall: Duration,
        metering: Metering,
    ) -> Result<(ResourceResponse, Vec<u8>), SdkError> {
        let (response, body) = self
            .resource_open(request, maximum, limit, stall, metering)
            .await?;
        let bytes = match body {
            Some(body) => body.collect().await?,
            None => Vec::new(),
        };
        Ok((response, bytes))
    }

    /// The response to `request`, and its body to read as it arrives
    /// (`None` for an error, which has none): what [`Self::resource_metered`]
    /// gathers whole.
    async fn resource_open(
        &self,
        request: &ResourceRequest,
        maximum: u64,
        limit: Duration,
        stall: Duration,
        metering: Metering,
    ) -> Result<(ResourceResponse, Option<BodyStream>), SdkError> {
        // A direct read's time starts with the request.
        let whole = match &metering {
            Metering::Whole(meter) => Some(Metered::begin(meter.clone())),
            _ => None,
        };
        let (recv, response) = exchange(&self.conn, bi::RESOURCE, request, limit).await?;
        let body = match &response {
            ResourceResponse::Ready { length, .. } => {
                if *length > maximum {
                    return Err(SdkError::Protocol(format!(
                        "a body of {length} bytes exceeds the {maximum}-byte limit"
                    )));
                }
                let meter = match metering {
                    Metering::Off => BodyMeter::Off,
                    Metering::Whole(meter) => BodyMeter::Counting(meter),
                    // From the first byte: the server held the read while
                    // the encode caught up, which is not the link's time.
                    Metering::FromFirstByte(meter) => BodyMeter::FromFirstChunk(meter),
                };
                Some(BodyStream {
                    conn: self.conn.clone(),
                    recv,
                    length: *length,
                    received: 0,
                    stall,
                    meter,
                    timed: whole,
                })
            }
            ResourceResponse::Error { .. } => None,
        };
        Ok((response, body))
    }

    pub async fn artwork(
        &self,
        request: &ArtworkRequest,
        limit: Duration,
        stall: Duration,
    ) -> Result<(ArtworkResponse, Vec<u8>), SdkError> {
        let request = ResourceRequest::Artwork {
            request: request.clone(),
            max_bytes: MAX_ARTWORK_BYTES,
        };
        let (response, bytes) = self
            .resource(&request, MAX_ARTWORK_BYTES, limit, stall)
            .await?;
        let response = match response {
            ResourceResponse::Ready {
                length,
                metadata: ResourceMetadata::Artwork { content_type, etag },
            } => ArtworkResponse::Ok(spoolframe_protocol::ArtworkReply {
                length,
                content_type,
                etag,
            }),
            ResourceResponse::Error { error } => ArtworkResponse::Err(error),
            _ => {
                return Err(SdkError::Protocol(
                    "artwork returned another resource kind".into(),
                ));
            }
        };
        Ok((response, bytes))
    }

    /// One media read, counted as `metering` says: a direct read whole, a
    /// converted one from its first byte (P6-06, PX-46).
    pub async fn read(
        &self,
        request: &MediaRead,
        revision: u64,
        limit: Duration,
        stall: Duration,
        metering: Metering,
    ) -> Result<(MediaReadResponse, Vec<u8>), SdkError> {
        let (response, body) = self
            .read_open(request, revision, limit, stall, metering)
            .await?;
        let bytes = match body {
            Some(body) => body.collect().await?,
            None => Vec::new(),
        };
        Ok((response, bytes))
    }

    /// [`Self::read`]'s answer, its body to read as it arrives (PX-46): a
    /// bridge hands a player the first bytes of a segment while the rest
    /// is on its way. `None` for a refusal, which has no body.
    pub async fn read_open(
        &self,
        request: &MediaRead,
        revision: u64,
        limit: Duration,
        stall: Duration,
        metering: Metering,
    ) -> Result<(MediaReadResponse, Option<BodyStream>), SdkError> {
        let maximum = request.length.min(MAX_READ);
        let request = ResourceRequest::Media {
            request: request.clone(),
            revision,
        };
        let (response, body) = self
            .resource_open(&request, maximum, limit, stall, metering)
            .await?;
        let response = match response {
            ResourceResponse::Ready {
                length,
                metadata:
                    ResourceMetadata::Media {
                        size,
                        complete,
                        epoch,
                        revision: actual,
                    },
            } if actual == revision => MediaReadResponse::Ok(spoolframe_protocol::MediaReadReply {
                sent: length,
                size,
                complete,
                epoch,
            }),
            ResourceResponse::Error { error } => MediaReadResponse::Err(error),
            _ => {
                return Err(SdkError::Protocol(
                    "media returned another resource kind or revision".into(),
                ));
            }
        };
        Ok((response, body))
    }
}

/// What the server meant by closing a connection with `error`, when it
/// gave one of its reasons ([`spoolframe_protocol::close`]): the device was
/// signed out, or the server is full. `None` for any other end of a
/// connection: it was lost.
pub(crate) fn closed_by_server(error: &ConnectionError) -> Option<SdkError> {
    let ConnectionError::ApplicationClosed(close) = error else {
        return None;
    };
    match u32::try_from(close.error_code.into_inner()).ok()? {
        close::SIGNED_OUT => Some(SdkError::SignInRequired),
        close::BUSY => Some(SdkError::ServerFull),
        _ => None,
    }
}

/// [`closed_by_server`], for a dial the server closed before it was set up.
fn refused(error: &ConnectError) -> Option<SdkError> {
    match error {
        ConnectError::Connection { source, .. }
        | ConnectError::Connecting {
            source: ConnectingError::ConnectionError { source, .. },
            ..
        } => closed_by_server(source),
        _ => None,
    }
}

/// A dial that failed: why, as the app is told, and at which stage.
#[derive(Debug)]
pub(crate) struct DialFailed {
    pub error: SdkError,
    pub stage: DialStage,
}

/// The stage a dial iroh could not complete failed at.
pub(crate) fn stage_of(error: &ConnectError) -> DialStage {
    match error {
        ConnectError::Connect {
            source: ConnectWithOptsError::NoAddress { .. },
            ..
        } => DialStage::NoAddress,
        ConnectError::Connecting {
            source: ConnectingError::HandshakeFailure { .. },
            ..
        } => DialStage::Handshake,
        ConnectError::Connection { source, .. }
        | ConnectError::Connecting {
            source: ConnectingError::ConnectionError { source, .. },
            ..
        } => stage_of_connection(source),
        _ => DialStage::Connect,
    }
}

/// The stage a connection being set up was lost at, by how.
pub(crate) fn stage_of_connection(error: &ConnectionError) -> DialStage {
    match error {
        ConnectionError::TimedOut => DialStage::Timeout,
        ConnectionError::VersionMismatch | ConnectionError::TransportError(_) => {
            DialStage::Handshake
        }
        ConnectionError::ApplicationClosed(_)
        | ConnectionError::ConnectionClosed(_)
        | ConnectionError::Reset => DialStage::Closed,
        _ => DialStage::Connect,
    }
}

/// A request on `conn` failed with `e`. When the server closed the
/// connection for a reason it gave, that reason is the error: a call cut
/// off by a sign-out is [`SdkError::SignInRequired`], not a network
/// failure.
fn lost(conn: &Connection, e: impl std::fmt::Display) -> SdkError {
    conn.close_reason()
        .as_ref()
        .and_then(closed_by_server)
        .unwrap_or_else(|| SdkError::Unreachable(format!("{}: {e}", conn.remote_id())))
}

/// As [`lost`], for a request that got no answer in time.
fn timed_out(conn: &Connection, what: impl std::fmt::Display, limit: Duration) -> SdkError {
    conn.close_reason()
        .as_ref()
        .and_then(closed_by_server)
        .unwrap_or_else(|| {
            SdkError::TimedOut(format!(
                "{}: {what}, no answer within {limit:?}",
                conn.remote_id()
            ))
        })
}

/// One RPC stream: the request frame out, the response frame back within
/// `limit`.
async fn rpc(
    conn: &Connection,
    session: Option<&SessionId>,
    request: Request,
    limit: Duration,
) -> Result<RpcResponse, SdkError> {
    let envelope = RequestEnvelope {
        session: session.cloned(),
        request,
    };
    let request = RpcRequest {
        envelope,
        operation: None,
        expected_revision: None,
    };
    let (_, response) = exchange(conn, bi::RPC, &request, limit).await?;
    Ok(response)
}

/// Open a stream of `kind`, send `request` as its one frame, and read the
/// response frame, all within `limit`. The stream is returned for what
/// follows the frame. Opening is in the limit too: it waits when the
/// server allows no more streams.
async fn exchange<Req, Resp>(
    conn: &Connection,
    kind: u8,
    request: &Req,
    limit: Duration,
) -> Result<(RecvStream, Resp), SdkError>
where
    Req: Serialize,
    Resp: DeserializeOwned,
{
    let codec = |e: CodecError| match e {
        CodecError::Io(e) => lost(conn, e),
        other => SdkError::Protocol(other.to_string()),
    };
    let exchange = async {
        let (mut send, mut recv) = conn.open_bi().await.map_err(|e| lost(conn, e))?;
        send.write_u8(kind).await.map_err(|e| lost(conn, e))?;
        write_frame(&mut send, request).await.map_err(codec)?;
        send.finish().map_err(|e| lost(conn, e))?;
        let response = if kind == bi::RPC {
            spoolframe_protocol::codec::read_reply(&mut recv).await
        } else {
            read_frame(&mut recv).await
        }
        .map_err(codec)?;
        Ok((recv, response))
    };
    tokio::time::timeout(limit, exchange)
        .await
        .map_err(|_| timed_out(conn, stream_name(kind), limit))?
}

fn stream_name(kind: u8) -> &'static str {
    match kind {
        bi::RESOURCE => "a resource read",
        _ => "a request",
    }
}

/// The `length` bytes after a response frame, and nothing more, with no
/// pause longer than `stall`.
/// How a read is counted by the link meter.
#[derive(Debug, Clone)]
pub enum Metering {
    Off,
    /// From the request to the last byte: a direct read, whose server
    /// answers at once.
    Whole(Arc<LinkMeter>),
    /// From the first chunk of the body to the last, that chunk not
    /// counted: a converted read, which the server may hold while it
    /// encodes. Any length: a short body (a short final read) adds little
    /// and takes no time, and a run of 512 KiB reads on a slow link is
    /// each seconds long.
    FromFirstByte(Arc<LinkMeter>),
}

/// How a [`BodyStream`] tells the link meter what arrives.
#[derive(Debug)]
enum BodyMeter {
    Off,
    /// Every byte, timed by a [`Metered`] begun with the request.
    Counting(Arc<LinkMeter>),
    /// Timed from the first chunk, which is not counted: it came with the
    /// response, all at once, and would read as an instant transfer.
    FromFirstChunk(Arc<LinkMeter>),
}

/// A read [`LinkMeter`] counts, ended when dropped: a read that fails or
/// is given up on ends too.
#[derive(Debug)]
struct Metered(Arc<LinkMeter>);

impl Metered {
    fn begin(meter: Arc<LinkMeter>) -> Self {
        meter.begin(Instant::now());
        Self(meter)
    }
}

impl Drop for Metered {
    fn drop(&mut self) {
        self.0.end(Instant::now());
    }
}

/// The body of a resource read, as it arrives (PX-46): its announced
/// `length`, then pieces of it until it is all there. No pause between
/// pieces may be longer than the read's stall limit. The link meter is
/// fed as pieces arrive, the same as for a read gathered whole; dropping
/// the body part-way ends its time there.
#[derive(Debug)]
pub struct BodyStream {
    conn: Connection,
    recv: RecvStream,
    length: u64,
    received: u64,
    stall: Duration,
    meter: BodyMeter,
    /// The meter's time, once begun.
    timed: Option<Metered>,
}

impl BodyStream {
    /// The next piece of the body, as much as arrived (at most 64 KiB);
    /// `None` once all of it arrived and the server ended the stream.
    pub async fn next(&mut self) -> Result<Option<Vec<u8>>, SdkError> {
        let mut buf = vec![0; 64 << 10];
        let read = tokio::time::timeout(self.stall, self.recv.read(&mut buf))
            .await
            .map_err(|_| timed_out(&self.conn, "the rest of a body", self.stall))?
            .map_err(|e| lost(&self.conn, e))?;
        // `None`: the server finished the stream.
        let Some(n) = read else {
            self.timed = None;
            if self.received != self.length {
                return Err(SdkError::Protocol(format!(
                    "{} bytes followed where {} were announced",
                    self.received, self.length
                )));
            }
            return Ok(None);
        };
        if n as u64 > self.length - self.received {
            return Err(SdkError::Protocol(format!(
                "more than {} bytes followed the response",
                self.length
            )));
        }
        self.received += n as u64;
        buf.truncate(n);
        match &self.meter {
            BodyMeter::Off => {}
            BodyMeter::Counting(meter) => meter.received(Instant::now(), n as u64),
            BodyMeter::FromFirstChunk(meter) => match &self.timed {
                Some(_) => meter.received(Instant::now(), n as u64),
                None => self.timed = Some(Metered::begin(meter.clone())),
            },
        }
        Ok(Some(buf))
    }

    /// All of the body.
    pub async fn collect(mut self) -> Result<Vec<u8>, SdkError> {
        let limit = usize::try_from(self.length)
            .map_err(|_| SdkError::Protocol(format!("a body of {} bytes", self.length)))?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(limit)
            .map_err(|e| SdkError::Protocol(format!("cannot buffer {} bytes: {e}", self.length)))?;
        while let Some(piece) = self.next().await? {
            bytes.extend_from_slice(&piece);
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use iroh::endpoint::presets;
    use iroh::{RelayMode, SecretKey};

    use super::*;

    /// A key with no address, relay or lookup to find it by: iroh says so
    /// before it sends anything.
    #[tokio::test]
    async fn a_dial_with_nothing_to_dial_is_no_address() {
        let endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind()
            .await
            .unwrap();
        let nowhere = SecretKey::from_bytes(&[7; 32]).public();
        let error = endpoint
            .connect(EndpointAddr::new(nowhere), CLIENT_ALPN)
            .await
            .unwrap_err();
        assert_eq!(stage_of(&error), DialStage::NoAddress, "{error:#}");
        endpoint.close().await;
    }

    #[test]
    fn a_connection_lost_as_it_is_set_up_is_staged_by_how() {
        assert_eq!(
            stage_of_connection(&ConnectionError::TimedOut),
            DialStage::Timeout
        );
        assert_eq!(
            stage_of_connection(&ConnectionError::VersionMismatch),
            DialStage::Handshake
        );
        assert_eq!(
            stage_of_connection(&ConnectionError::Reset),
            DialStage::Closed
        );
        assert_eq!(
            stage_of_connection(&ConnectionError::LocallyClosed),
            DialStage::Connect
        );
    }
}
