//! One connection to one server, shared by every session on it (§15).

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

/// How long a dial may take before the server counts as unreachable.
pub(crate) const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

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
            device_name: Some(device.name.clone()),
            capabilities: Some(device.capabilities.clone()),
        });
        let info = match rpc(&conn, None, hello).await? {
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
    /// One request in `session`.
    pub async fn call(
        &self,
        session: Option<&SessionId>,
        request: Request,
    ) -> Result<Response, SdkError> {
        rpc(&self.conn, session, request).await
    }

    /// One image: the response, and the image when it is `Ok`.
    pub async fn artwork(
        &self,
        request: &ArtworkRequest,
    ) -> Result<(ArtworkResponse, Vec<u8>), SdkError> {
        let (mut recv, response) = exchange(&self.conn, bi::ARTWORK, request).await?;
        let bytes = match &response {
            ArtworkResponse::Ok(reply) => body(&self.conn, &mut recv, reply.length).await?,
            ArtworkResponse::Err(_) => Vec::new(),
        };
        Ok((response, bytes))
    }

    /// One range read: the response, and the bytes when it is `Ok`.
    pub async fn read(
        &self,
        request: &MediaRead,
    ) -> Result<(MediaReadResponse, Vec<u8>), SdkError> {
        let (mut recv, response) = exchange(&self.conn, bi::MEDIA, request).await?;
        let bytes = match &response {
            MediaReadResponse::Ok(reply) => body(&self.conn, &mut recv, reply.sent).await?,
            MediaReadResponse::Err(_) => Vec::new(),
        };
        Ok((response, bytes))
    }
}

fn lost(conn: &Connection, e: impl std::fmt::Display) -> SdkError {
    SdkError::Unreachable(format!("{}: {e}", conn.remote_id()))
}

/// One RPC stream: the request frame out, the response frame back.
async fn rpc(
    conn: &Connection,
    session: Option<&SessionId>,
    request: Request,
) -> Result<Response, SdkError> {
    let envelope = RequestEnvelope {
        session: session.cloned(),
        request,
    };
    let (_, response) = exchange(conn, bi::RPC, &envelope).await?;
    Ok(response)
}

/// Open a stream of `kind`, send `request` as its one frame, and read the
/// response frame. The stream is returned for what follows the frame.
async fn exchange<Req, Resp>(
    conn: &Connection,
    kind: u8,
    request: &Req,
) -> Result<(RecvStream, Resp), SdkError>
where
    Req: Serialize,
    Resp: DeserializeOwned,
{
    let codec = |e: CodecError| match e {
        CodecError::Io(e) => lost(conn, e),
        other => SdkError::Protocol(other.to_string()),
    };
    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| lost(conn, e))?;
    send.write_u8(kind).await.map_err(|e| lost(conn, e))?;
    write_frame(&mut send, request).await.map_err(codec)?;
    send.finish().map_err(|e| lost(conn, e))?;
    let response = read_frame(&mut recv).await.map_err(codec)?;
    Ok((recv, response))
}

/// The `length` bytes after a response frame, and nothing more.
async fn body(conn: &Connection, recv: &mut RecvStream, length: u64) -> Result<Vec<u8>, SdkError> {
    let limit = usize::try_from(length)
        .map_err(|_| SdkError::Protocol(format!("a body of {length} bytes")))?;
    let bytes = recv.read_to_end(limit).await.map_err(|e| lost(conn, e))?;
    if bytes.len() as u64 != length {
        return Err(SdkError::Protocol(format!(
            "{} bytes followed where {length} were announced",
            bytes.len()
        )));
    }
    Ok(bytes)
}
