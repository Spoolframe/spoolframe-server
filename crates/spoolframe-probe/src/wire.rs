//! The probe's raw side of `spoolframe/client/1`: connections, streams and
//! frames, each one counted against the budget, and the stop condition.
//!
//! Nothing here goes through the SDK, which never holds a stream open
//! unwritten or leaves one silent after its kind byte.

use parking_lot::Mutex;
use std::fmt;
use std::ops::Deref;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use iroh::endpoint::{Connection, QuicTransportConfig, RecvStream, SendStream, VarInt, presets};
use iroh::{Endpoint, EndpointAddr, RelayMode};
use spoolframe_protocol::rpc::{Operation, RpcRequest, RpcResponse, ServerInstance};
use spoolframe_protocol::streams::bi;
use spoolframe_protocol::{
    CLIENT_ALPN, Error, Hello, Reply, Request, RequestEnvelope, Response, ServerInfo, SessionId,
    codec,
};
use spoolframe_types::Platform;
use tokio::io::AsyncWriteExt;
use tokio::time::timeout;

/// How many exchanges in a row may go unanswered, or be answered
/// `Internal`, before the run stops: a server in trouble is not probed
/// further.
pub const BURST: u32 = 3;

/// As the SDK keeps its connections alive: the first-frame check waits
/// longer than the server's idle timeout with nothing else to send.
const KEEP_ALIVE: Duration = Duration::from_secs(5);
const IDLE_TIMEOUT: Duration = Duration::from_secs(15);

/// Why a run ended before its last check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// The request budget is spent.
    Budget { limit: u32 },
    /// [`BURST`] exchanges in a row went unanswered or failed.
    Unresponsive { last: String },
}

impl fmt::Display for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Stop::Budget { limit } => write!(f, "the budget of {limit} requests is spent"),
            Stop::Unresponsive { last } => write!(
                f,
                "the server stopped answering ({BURST} failures in a row; last: {last})"
            ),
        }
    }
}

/// What went wrong with one exchange.
#[derive(Debug)]
pub enum Problem {
    /// The run stops here.
    Stop(Stop),
    /// No answer where one was due, or not the one any server gives: the
    /// check fails.
    Failed(String),
}

/// A connection the probe closes when it is done with it.
pub struct Conn(Connection);

impl Deref for Conn {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.0
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.0.close(VarInt::from_u32(0), b"probe done");
    }
}

/// One run's endpoint (a fresh key: an anonymous device no server has
/// seen), its target, its budget and its health.
pub struct Probe {
    endpoint: Endpoint,
    target: EndpointAddr,
    limit: u32,
    spent: AtomicU32,
    /// How long any one answer may take.
    pub reply_timeout: Duration,
    /// Exchanges in a row that failed.
    failures: AtomicU32,
    /// The server process the replies came from: operations name it.
    instance: Mutex<Option<ServerInstance>>,
    operations: AtomicU64,
}

impl Probe {
    pub async fn new(
        target: EndpointAddr,
        limit: u32,
        reply_timeout: Duration,
    ) -> anyhow::Result<Self> {
        let transport = QuicTransportConfig::builder()
            .keep_alive_interval(KEEP_ALIVE)
            .max_idle_timeout(Some(IDLE_TIMEOUT.try_into()?))
            .build();
        // No relays and no address lookup: only the addresses given are
        // dialled, and nothing is announced.
        let endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .transport_config(transport)
            .bind()
            .await?;
        Ok(Self {
            endpoint,
            target,
            limit,
            spent: AtomicU32::new(0),
            reply_timeout,
            failures: AtomicU32::new(0),
            instance: Mutex::default(),
            operations: AtomicU64::new(0),
        })
    }

    /// This run's own endpoint id: a fresh random key.
    pub fn id(&self) -> iroh::EndpointId {
        self.endpoint.id()
    }

    pub fn spent(&self) -> u32 {
        self.spent.load(Ordering::Acquire)
    }

    /// Count one connection or stream against the budget, or stop.
    pub fn spend(&self) -> Result<(), Problem> {
        let limit = self.limit;
        self.spent
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |spent| {
                (spent < limit).then_some(spent + 1)
            })
            .map(|_| ())
            .map_err(|_| Problem::Stop(Stop::Budget { limit }))
    }

    /// An exchange failed: the check's failure, or, the [`BURST`]th in a
    /// row, the end of the run.
    pub fn failed(&self, what: String) -> Problem {
        let failures = self.failures.fetch_add(1, Ordering::AcqRel) + 1;
        if failures >= BURST {
            Problem::Stop(Stop::Unresponsive { last: what })
        } else {
            Problem::Failed(what)
        }
    }

    /// The server answered: the failures in a row start again.
    pub fn answered(&self) {
        self.failures.store(0, Ordering::Release);
    }

    pub async fn connect(&self) -> Result<Conn, Problem> {
        self.spend()?;
        match timeout(
            self.reply_timeout,
            self.endpoint.connect(self.target.clone(), CLIENT_ALPN),
        )
        .await
        {
            Ok(Ok(connection)) => {
                self.answered();
                Ok(Conn(connection))
            }
            Ok(Err(e)) => Err(self.failed(format!("cannot connect: {e}"))),
            Err(_) => Err(self.failed(format!("no handshake within {:?}", self.reply_timeout))),
        }
    }

    /// A connection that said a good `Hello`, and what the server said.
    pub async fn connect_hello(&self) -> Result<(Conn, ServerInfo), Problem> {
        let conn = self.connect().await?;
        match self.request(&conn, None, hello(None)).await? {
            Response::Ok(Reply::ServerInfo(info)) => Ok((conn, info)),
            other => Err(Problem::Failed(format!(
                "a good Hello was answered {}",
                describe(&other)
            ))),
        }
    }

    /// A bidirectional stream, nothing written on it yet.
    pub async fn open(&self, conn: &Connection) -> Result<(SendStream, RecvStream), Problem> {
        self.spend()?;
        match timeout(self.reply_timeout, conn.open_bi()).await {
            Ok(Ok(stream)) => Ok(stream),
            Ok(Err(e)) => Err(self.failed(format!("cannot open a stream: {e}"))),
            Err(_) => Err(self.failed(format!("no stream credit within {:?}", self.reply_timeout))),
        }
    }

    /// `request` as a well-behaved client frames it, and its reply.
    ///
    /// The only way the probe writes a frame: the bytes are always
    /// `codec::encode` of a real `RpcRequest` under their true length.
    /// Malformed frames are the fuzz targets' job (`protocol_frame`,
    /// `protocol_cbor`), not a live server's.
    pub async fn request(
        &self,
        conn: &Connection,
        session: Option<&str>,
        request: Request,
    ) -> Result<Response, Problem> {
        let bytes = self.encode(session, request)?;
        let (mut send, mut recv) = self.open(conn).await?;
        // A server that refuses a frame by its length stops the stream:
        // what is left to write is never read. Only the reply says how it
        // went.
        let _ = send.write_u8(bi::RPC).await;
        let _ = send.write_all(&(bytes.len() as u32).to_be_bytes()).await;
        let _ = send.write_all(&bytes).await;
        let _ = send.finish();
        self.reply(&mut recv).await
    }

    /// `request` as the frame body [`Probe::request`] sends.
    pub fn encode(&self, session: Option<&str>, request: Request) -> Result<Vec<u8>, Problem> {
        let wire = RpcRequest {
            envelope: RequestEnvelope {
                session: session.map(|s| SessionId(s.to_owned())),
                request,
            },
            operation: Some(self.operation()),
            expected_revision: None,
        };
        let bytes = codec::encode(&wire).map_err(|e| Problem::Failed(e.to_string()))?;
        // Within the protocol's own frame limit, or the length prefix
        // would not be its true length.
        if bytes.len() > codec::MAX_FRAME as usize {
            return Err(Problem::Failed(format!(
                "a {}-byte request is past the protocol's frame limit",
                bytes.len()
            )));
        }
        Ok(bytes)
    }

    /// The reply on `recv`.
    pub async fn reply(&self, recv: &mut RecvStream) -> Result<Response, Problem> {
        match timeout(
            self.reply_timeout,
            codec::read_reply::<_, RpcResponse>(recv),
        )
        .await
        {
            Ok(Ok(reply)) => {
                *self.instance.lock() = Some(reply.server);
                let response = reply.reply.response;
                if response == Response::Err(Error::Internal) {
                    if let Problem::Stop(stop) = self.failed("Internal".into()) {
                        return Err(Problem::Stop(stop));
                    }
                } else {
                    self.answered();
                }
                Ok(response)
            }
            Ok(Err(e)) => Err(self.failed(format!("no reply: {e}"))),
            Err(_) => Err(self.failed(format!("no reply within {:?}", self.reply_timeout))),
        }
    }

    /// A fresh operation for the server instance the last reply came from,
    /// for its clock: mutations need one, reads ignore it.
    fn operation(&self) -> Operation {
        let instance = self.instance.lock().clone();
        let n = self.operations.fetch_add(1, Ordering::AcqRel);
        Operation {
            // Operation ids are kept per endpoint, and the endpoint is this
            // run's own.
            id: format!("probe-{n}"),
            instance: instance.as_ref().map(|i| i.id.clone()).unwrap_or_default(),
            expires_at_ms: instance.map_or(0, |i| i.time_ms) + 60_000,
        }
    }

    pub async fn close(self) {
        self.endpoint.close().await;
    }
}

/// The probe's `Hello`, with the current protocol revision.
pub fn hello(device_name: Option<String>) -> Request {
    hello_for(spoolframe_protocol::PROTOCOL_VERSION, device_name)
}

pub fn hello_for(protocol_version: u32, device_name: Option<String>) -> Request {
    Request::Hello(Hello {
        protocol_version,
        client_name: "spoolframe-probe".into(),
        client_version: env!("CARGO_PKG_VERSION").into(),
        platform: Platform::Cli,
        device_name,
        capabilities: None,
    })
}

/// A response, short enough for a line of evidence.
pub fn describe(response: &Response) -> String {
    let text = match response {
        Response::Ok(reply) => format!("Ok({reply:?})"),
        Response::Err(error) => format!("Err({error:?})"),
    };
    const MAX: usize = 160;
    match text.char_indices().nth(MAX) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text,
    }
}
