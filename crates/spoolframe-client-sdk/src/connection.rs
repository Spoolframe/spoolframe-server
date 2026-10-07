//! One connection to one server, shared by every session on it (§15),
//! and how long its requests may take (P1-15).
//!
//! Every request has a time limit. A connection can die without closing:
//! iOS suspends an app, the server side drops it, and when the app comes
//! back QUIC still counts it as open. A request on it would wait forever,
//! so it waits only so long; then the connection is asked for a `Ping`
//! and, if that goes unanswered too, dropped (see `Client::call`).

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::StreamExt;
use iroh::endpoint::{
    ConnectError, ConnectingError, Connection, ConnectionError, PathEvent, PathEventStream,
    RecvStream, WeakConnectionHandle,
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
    /// the one dialled.
    pub async fn dial(
        endpoint: &Endpoint,
        addr: EndpointAddr,
        device: &Device,
        timeouts: &Timeouts,
    ) -> Result<Self, SdkError> {
        let expected: EndpointId = addr.id;
        let unreachable = |e: String| SdkError::Unreachable(format!("{expected}: {e}"));
        let conn = tokio::time::timeout(DIAL_TIMEOUT, endpoint.connect(addr, CLIENT_ALPN))
            .await
            .map_err(|_| unreachable(format!("no answer within {DIAL_TIMEOUT:?}")))?
            .map_err(|e| refused(&e).unwrap_or_else(|| unreachable(e.to_string())))?;
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
        });
        // A server that takes the connection and then says nothing is as
        // unreachable as one that never answered the dial.
        let answer = match rpc(&conn, None, hello, timeouts.request).await {
            Ok(answer) => answer,
            Err(e) => {
                conn.close(0u32.into(), b"no hello");
                return Err(match e {
                    SdkError::TimedOut(why) => unreachable(why),
                    other => other,
                });
            }
        };
        let instance = answer.server;
        let mut info = match answer.reply.response {
            Response::Ok(Reply::ServerInfo(info)) => info,
            Response::Ok(other) => {
                return Err(SdkError::Protocol(format!("Hello answered with {other:?}")));
            }
            Response::Err(e) => return Err(SdkError::from_server(e)),
        };
        info.web_url = crate::client::web_page(info.web_url.take());
        if info.protocol_version != PROTOCOL_VERSION {
            conn.close(0u32.into(), b"wrong protocol revision");
            return Err(SdkError::Protocol(
                "server and SDK protocol revisions differ".into(),
            ));
        }
        // Iroh has proven the key; the server must also say it is that key,
        // or its name and the pin would disagree.
        if info.endpoint_id != expected.to_string() {
            conn.close(0u32.into(), b"wrong server");
            return Err(SdkError::WrongServer {
                expected: expected.to_string(),
                actual: info.endpoint_id,
            });
        }
        // It names the relying party of the server's app passkeys (PX-14):
        // one that is not an id is never pinned or used.
        if let Err(e) = spoolframe_protocol::ServerId::parse(&info.server_id) {
            conn.close(0u32.into(), b"not a server id");
            return Err(SdkError::Protocol(e.to_string()));
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
        let (mut recv, response) = exchange(&self.conn, bi::RESOURCE, request, limit).await?;
        let bytes = match &response {
            ResourceResponse::Ready { length, .. } => {
                body(&self.conn, &mut recv, *length, maximum, stall).await?
            }
            ResourceResponse::Error { .. } => Vec::new(),
        };
        Ok((response, bytes))
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

    pub async fn read(
        &self,
        request: &MediaRead,
        revision: u64,
        limit: Duration,
        stall: Duration,
    ) -> Result<(MediaReadResponse, Vec<u8>), SdkError> {
        let maximum = request.length.min(MAX_READ);
        let request = ResourceRequest::Media {
            request: request.clone(),
            revision,
        };
        let (response, bytes) = self.resource(&request, maximum, limit, stall).await?;
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
        Ok((response, bytes))
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
async fn body(
    conn: &Connection,
    recv: &mut RecvStream,
    length: u64,
    maximum: u64,
    stall: Duration,
) -> Result<Vec<u8>, SdkError> {
    if length > maximum {
        return Err(SdkError::Protocol(format!(
            "a body of {length} bytes exceeds the {maximum}-byte limit"
        )));
    }
    let limit = usize::try_from(length)
        .map_err(|_| SdkError::Protocol(format!("a body of {length} bytes")))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(limit)
        .map_err(|e| SdkError::Protocol(format!("cannot buffer {length} bytes: {e}")))?;
    let mut buf = vec![0; 64 << 10];
    loop {
        let read = tokio::time::timeout(stall, recv.read(&mut buf))
            .await
            .map_err(|_| timed_out(conn, "the rest of a body", stall))?
            .map_err(|e| lost(conn, e))?;
        // `None`: the server finished the stream.
        let Some(n) = read else {
            break;
        };
        if n > limit - bytes.len() {
            return Err(SdkError::Protocol(format!(
                "more than {length} bytes followed the response"
            )));
        }
        bytes.extend_from_slice(&buf[..n]);
    }
    if bytes.len() as u64 != length {
        return Err(SdkError::Protocol(format!(
            "{} bytes followed where {length} were announced",
            bytes.len()
        )));
    }
    Ok(bytes)
}
