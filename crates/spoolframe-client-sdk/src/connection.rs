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

use iroh::endpoint::{Connection, RecvStream};
use iroh::{Endpoint, EndpointAddr, EndpointId, TransportAddr};
use serde::Serialize;
use serde::de::DeserializeOwned;
use spoolframe_protocol::codec::{CodecError, read_frame, write_frame};
use spoolframe_protocol::streams::bi;
use spoolframe_protocol::{
    ArtworkRequest, ArtworkResponse, CLIENT_ALPN, Hello, MediaRead, MediaReadResponse,
    PROTOCOL_VERSION, Reply, Request, RequestEnvelope, Response, ServerInfo, SessionId,
};
use tokio::io::AsyncWriteExt;

use crate::client::Device;
use crate::error::SdkError;
use crate::link::ConnectionPath;

/// How long a dial may take before the server counts as unreachable.
pub(crate) const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

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

/// The server's own waits, mirrored from it. Each is how long the server
/// may take before it answers, not counting the round trip.
mod wait {
    use std::time::Duration;

    /// `AwaitDeviceSignIn` (the server's `device_sign_in::AWAIT`).
    pub const SIGN_IN: Duration = Duration::from_secs(25);
    /// A read past what an encode has written, or of a subtitle still
    /// being read as text (the server's `READ_WAIT`).
    pub const READ: Duration = Duration::from_secs(10);
    /// A request that may run ffprobe on the file (`Prober::DEFAULT_TIMEOUT`).
    pub const PROBE: Duration = Duration::from_secs(30);
    /// Starting a playback, or changing its subtitle: a probe, and for HLS
    /// of a copied video every keyframe of the file, which the server
    /// bounds only at 10 minutes. Two minutes is longer than a player
    /// should spin; past it the user is better told.
    pub const PLAYBACK: Duration = Duration::from_secs(120);
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

/// How long the server may hold `request` before it answers.
fn server_wait(request: &Request) -> Duration {
    match request {
        Request::AwaitDeviceSignIn { .. } => wait::SIGN_IN,
        Request::ReadSubtitle { .. } | Request::ReadSubtitleSegment { .. } => wait::READ,
        Request::Tracks { .. }
        | Request::PlaybackPlan { .. }
        | Request::SetTrackOverride { .. } => wait::PROBE,
        Request::StartPlayback { .. } | Request::ChangeSubtitle { .. } => wait::PLAYBACK,
        _ => Duration::ZERO,
    }
}

/// Whether `request` may be sent a second time, on a new connection, when
/// the first one died before it was answered. Only requests that read:
/// one that changes something may have been done before its answer was
/// lost, and doing it twice is not the same as once (a sign-in approved
/// twice, a PIN tried twice). Every request is named, so a new one is
/// decided here when it is added.
pub(crate) fn retry_safe(request: &Request) -> bool {
    match request {
        Request::Hello(_)
        | Request::AuthMethods
        | Request::Ping
        | Request::Libraries
        | Request::Items { .. }
        | Request::Item { .. }
        | Request::Search(_)
        | Request::PersonItems { .. }
        | Request::Tracks { .. }
        | Request::WhoAmI
        | Request::ListSessions
        | Request::PlaybackPlan { .. }
        | Request::ListDevices
        | Request::ListPasskeys
        | Request::GetPlexLink
        | Request::LookupDeviceSignIn { .. }
        | Request::GetManifest { .. }
        | Request::ReadSubtitle { .. }
        | Request::ReadSubtitleSegment { .. }
        | Request::GetPreviews { .. }
        | Request::Home
        | Request::GetTrackPreferences => true,
        // Signing in: a failed password counts, a challenge is replaced,
        // an approved sign-in is collected once.
        Request::LoginPassword(_)
        | Request::BeginDeviceSignIn
        | Request::AwaitDeviceSignIn { .. }
        | Request::BeginPasskeySignIn { .. }
        | Request::FinishPasskeySignIn(_)
        | Request::Logout
        // Playback: each starts, stops or restarts an encode.
        | Request::StartPlayback { .. }
        | Request::StopPlayback { .. }
        | Request::SeekPlayback { .. }
        | Request::ChangeSubtitle { .. }
        // PINs: a wrong one counts towards the lock-out.
        | Request::SetPin { .. }
        | Request::ClearPin { .. }
        | Request::UnlockProfile { .. }
        | Request::SetPinRequired { .. }
        | Request::BeginPasskeyRegistration
        | Request::FinishPasskeyRegistration(_)
        | Request::RenamePasskey { .. }
        | Request::RemovePasskey { .. }
        | Request::ApproveDeviceSignIn { .. }
        | Request::DenyDeviceSignIn { .. }
        | Request::UpdatePlaybackProgress { .. }
        | Request::MarkWatched { .. }
        | Request::MarkUnwatched { .. }
        | Request::HideFromContinueWatching { .. }
        | Request::UnhideFromContinueWatching { .. }
        | Request::SetTrackPreferences { .. }
        | Request::SetTrackOverride { .. }
        | Request::ClearTrackOverride { .. }
        | Request::Unknown => false,
    }
}

pub(crate) struct ServerConnection {
    pub conn: Connection,
    pub info: ServerInfo,
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
            .map_err(|e| unreachable(e.to_string()))?;
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
        let info = match answer {
            Response::Ok(Reply::ServerInfo(info)) => info,
            Response::Ok(other) => {
                return Err(SdkError::Protocol(format!("Hello answered with {other:?}")));
            }
            Response::Err(e) => return Err(SdkError::from_server(e)),
        };
        // Iroh has proven the key; the server must also say it is that key,
        // or its name and the pin would disagree.
        if info.endpoint_id != expected.to_string() {
            conn.close(0u32.into(), b"wrong server");
            return Err(SdkError::WrongServer {
                expected: expected.to_string(),
                actual: info.endpoint_id,
            });
        }
        Ok(Self { conn, info })
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

impl ServerConnection {
    /// One request in `session`, answered within `limit`.
    pub async fn call(
        &self,
        session: Option<&SessionId>,
        request: Request,
        limit: Duration,
    ) -> Result<Response, SdkError> {
        rpc(&self.conn, session, request, limit).await
    }

    /// Whether the server answers a `Ping` on this connection within
    /// `limit`. Any answer counts: the connection carries requests.
    pub async fn answers(&self, limit: Duration) -> bool {
        self.conn.close_reason().is_none()
            && rpc(&self.conn, None, Request::Ping, limit).await.is_ok()
    }

    /// One image: the response within `limit`, and the image when it is
    /// `Ok`, with no pause in it longer than `stall`.
    pub async fn artwork(
        &self,
        request: &ArtworkRequest,
        limit: Duration,
        stall: Duration,
    ) -> Result<(ArtworkResponse, Vec<u8>), SdkError> {
        let (mut recv, response) = exchange(&self.conn, bi::ARTWORK, request, limit).await?;
        let bytes = match &response {
            ArtworkResponse::Ok(reply) => body(&self.conn, &mut recv, reply.length, stall).await?,
            ArtworkResponse::Err(_) => Vec::new(),
        };
        Ok((response, bytes))
    }

    /// One range read: the response within `limit`, and the bytes when it
    /// is `Ok`, with no pause in them longer than `stall`. A read's bytes
    /// take as long as the link needs for them (16 MiB at most), so only
    /// a stall is a failure, not a slow link.
    pub async fn read(
        &self,
        request: &MediaRead,
        limit: Duration,
        stall: Duration,
    ) -> Result<(MediaReadResponse, Vec<u8>), SdkError> {
        let (mut recv, response) = exchange(&self.conn, bi::MEDIA, request, limit).await?;
        let bytes = match &response {
            MediaReadResponse::Ok(reply) => body(&self.conn, &mut recv, reply.sent, stall).await?,
            MediaReadResponse::Err(_) => Vec::new(),
        };
        Ok((response, bytes))
    }
}

fn lost(conn: &Connection, e: impl std::fmt::Display) -> SdkError {
    SdkError::Unreachable(format!("{}: {e}", conn.remote_id()))
}

fn timed_out(conn: &Connection, what: impl std::fmt::Display, limit: Duration) -> SdkError {
    SdkError::TimedOut(format!(
        "{}: {what}, no answer within {limit:?}",
        conn.remote_id()
    ))
}

/// One RPC stream: the request frame out, the response frame back within
/// `limit`.
async fn rpc(
    conn: &Connection,
    session: Option<&SessionId>,
    request: Request,
    limit: Duration,
) -> Result<Response, SdkError> {
    let envelope = RequestEnvelope {
        session: session.cloned(),
        request,
    };
    let (_, response) = exchange(conn, bi::RPC, &envelope, limit).await?;
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
        let response = read_frame(&mut recv).await.map_err(codec)?;
        Ok((recv, response))
    };
    tokio::time::timeout(limit, exchange)
        .await
        .map_err(|_| timed_out(conn, stream_name(kind), limit))?
}

fn stream_name(kind: u8) -> &'static str {
    match kind {
        bi::ARTWORK => "an image",
        bi::MEDIA => "a media read",
        _ => "a request",
    }
}

/// The `length` bytes after a response frame, and nothing more, with no
/// pause longer than `stall`.
async fn body(
    conn: &Connection,
    recv: &mut RecvStream,
    length: u64,
    stall: Duration,
) -> Result<Vec<u8>, SdkError> {
    let limit = usize::try_from(length)
        .map_err(|_| SdkError::Protocol(format!("a body of {length} bytes")))?;
    let mut bytes = Vec::with_capacity(limit);
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
        bytes.extend_from_slice(&buf[..n]);
        if bytes.len() > limit {
            break;
        }
    }
    if bytes.len() as u64 != length {
        return Err(SdkError::Protocol(format!(
            "{} bytes followed where {length} were announced",
            bytes.len()
        )));
    }
    Ok(bytes)
}
