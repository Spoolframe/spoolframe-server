//! The probe's raw side of `spoolframe/client/1`: connections, streams and
//! frames, each one counted against the budget, and the stop condition.
//!
//! Nothing here goes through the SDK, which never holds a stream open
//! unwritten or leaves one silent after its kind byte.

use parking_lot::Mutex;
use std::fmt;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use iroh::endpoint::{Connection, QuicTransportConfig, RecvStream, SendStream, VarInt, presets};
use iroh::{Endpoint, EndpointAddr, RelayMode};
use spoolframe_protocol::resource::{ResourceRequest, ResourceResponse};
use spoolframe_protocol::rpc::{Operation, RpcRequest, RpcResponse, ServerInstance};
use spoolframe_protocol::streams::bi;
use spoolframe_protocol::{
    CLIENT_ALPN, Error, Hello, Reply, Request, RequestEnvelope, Response, ServerInfo, SessionId,
    codec,
};
use spoolframe_types::{ClientCapabilities, Platform};
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
    /// Signing out what the run signed in: past the budget too.
    cleaning: AtomicBool,
}

/// A fresh endpoint key, dialling only the addresses it is given.
pub async fn bind() -> anyhow::Result<Endpoint> {
    let transport = QuicTransportConfig::builder()
        .keep_alive_interval(KEEP_ALIVE)
        .max_idle_timeout(Some(IDLE_TIMEOUT.try_into()?))
        .build();
    // No relays and no address lookup: only the addresses given are
    // dialled, and nothing is announced.
    Ok(Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .transport_config(transport)
        .bind()
        .await?)
}

impl Probe {
    pub async fn new(
        target: EndpointAddr,
        limit: u32,
        reply_timeout: Duration,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            endpoint: bind().await?,
            target,
            limit,
            spent: AtomicU32::new(0),
            reply_timeout,
            failures: AtomicU32::new(0),
            instance: Mutex::default(),
            operations: AtomicU64::new(0),
            cleaning: AtomicBool::new(false),
        })
    }

    /// This run's own endpoint id: a fresh random key.
    pub fn id(&self) -> iroh::EndpointId {
        self.endpoint.id()
    }

    pub fn spent(&self) -> u32 {
        self.spent.load(Ordering::Acquire)
    }

    /// From now on the run only signs out what it signed in: that is not
    /// refused for want of budget, though it is still counted.
    pub fn cleaning_up(&self) {
        self.cleaning.store(true, Ordering::Release);
        self.answered();
    }

    /// Count one connection, stream or web request against the budget, or
    /// stop.
    pub fn spend(&self) -> Result<(), Problem> {
        if self.cleaning.load(Ordering::Acquire) {
            self.spent.fetch_add(1, Ordering::AcqRel);
            return Ok(());
        }
        let limit = self.limit;
        self.spent
            .try_update(Ordering::AcqRel, Ordering::Acquire, |spent| {
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
        self.connect_from(&self.endpoint).await
    }

    /// A connection from `endpoint`, another key than the run's own.
    pub async fn connect_from(&self, endpoint: &Endpoint) -> Result<Conn, Problem> {
        self.spend()?;
        match timeout(
            self.reply_timeout,
            endpoint.connect(self.target.clone(), CLIENT_ALPN),
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
        self.connect_hello_with(&self.endpoint, None).await
    }

    /// As [`Probe::connect_hello`], from `endpoint`.
    pub async fn connect_hello_from(
        &self,
        endpoint: &Endpoint,
    ) -> Result<(Conn, ServerInfo), Problem> {
        self.connect_hello_with(endpoint, None).await
    }

    /// As [`Probe::connect_hello_from`], naming the device: what an
    /// account's devices list shows for it.
    pub async fn connect_hello_with(
        &self,
        endpoint: &Endpoint,
        device_name: Option<String>,
    ) -> Result<(Conn, ServerInfo), Problem> {
        self.connect_hello_as(endpoint, device_name, None).await
    }

    /// As [`Probe::connect_hello_with`], saying what the device can play:
    /// a playback is planned by the capabilities its connection's `Hello`
    /// gave.
    pub async fn connect_hello_as(
        &self,
        endpoint: &Endpoint,
        device_name: Option<String>,
        capabilities: Option<ClientCapabilities>,
    ) -> Result<(Conn, ServerInfo), Problem> {
        let conn = self.connect_from(endpoint).await?;
        let request = hello_full(
            spoolframe_protocol::PROTOCOL_VERSION,
            device_name,
            capabilities,
        );
        match self.request(&conn, None, request).await? {
            Response::Ok(Reply::ServerInfo(info)) => Ok((conn, info)),
            other => Err(Problem::Failed(format!(
                "a good Hello was answered {}",
                describe(&other)
            ))),
        }
    }

    /// A connection whose `Hello` names the experimental features
    /// `experimental`, as an app that asks for them does (the federation
    /// run, P8).
    pub async fn connect_hello_naming(&self, experimental: Vec<String>) -> Result<Conn, Problem> {
        let conn = self.connect().await?;
        let mut request = hello(None);
        if let Request::Hello(hello) = &mut request {
            hello.experimental = experimental;
        }
        match self.request(&conn, None, request).await? {
            Response::Ok(Reply::ServerInfo(_)) => Ok(conn),
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
        self.request_at(conn, session, request, None).await
    }

    /// As [`Probe::request`], claiming the playback's revision
    /// `expected_revision`, as a playback control does.
    pub async fn request_at(
        &self,
        conn: &Connection,
        session: Option<&str>,
        request: Request,
        expected_revision: Option<u64>,
    ) -> Result<Response, Problem> {
        let bytes = self.encode_at(session, request, expected_revision)?;
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
        self.encode_at(session, request, None)
    }

    fn encode_at(
        &self,
        session: Option<&str>,
        request: Request,
        expected_revision: Option<u64>,
    ) -> Result<Vec<u8>, Problem> {
        let wire = RpcRequest {
            envelope: RequestEnvelope {
                session: session.map(|s| SessionId(s.to_owned())),
                request,
            },
            operation: Some(self.operation()),
            expected_revision,
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

    /// `request` on a resource stream (bi kind 2), as a client sends it:
    /// its response frame, and the bytes after it, at most `max_bytes`
    /// of them.
    pub async fn resource(
        &self,
        conn: &Connection,
        request: &ResourceRequest,
        max_bytes: usize,
    ) -> Result<(ResourceResponse, Vec<u8>), Problem> {
        let (mut send, mut recv) = self.open(conn).await?;
        let _ = send.write_u8(bi::RESOURCE).await;
        if let Err(e) = codec::write_frame(&mut send, request).await {
            return Err(Problem::Failed(format!(
                "cannot write the resource request: {e}"
            )));
        }
        let _ = send.finish();
        let response = match timeout(
            self.reply_timeout,
            codec::read_frame::<_, ResourceResponse>(&mut recv),
        )
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(e)) => return Err(self.failed(format!("no resource reply: {e}"))),
            Err(_) => {
                return Err(
                    self.failed(format!("no resource reply within {:?}", self.reply_timeout))
                );
            }
        };
        if matches!(
            response,
            ResourceResponse::Error {
                error: Error::Internal
            }
        ) {
            if let Problem::Stop(stop) = self.failed("Internal".into()) {
                return Err(Problem::Stop(stop));
            }
        } else {
            self.answered();
        }
        let bytes = match timeout(self.reply_timeout, recv.read_to_end(max_bytes)).await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(e)) => return Err(Problem::Failed(format!("the resource's bytes: {e}"))),
            Err(_) => {
                return Err(Problem::Failed(format!(
                    "the resource's bytes did not end within {:?}",
                    self.reply_timeout
                )));
            }
        };
        Ok((response, bytes))
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
    hello_full(protocol_version, device_name, None)
}

fn hello_full(
    protocol_version: u32,
    device_name: Option<String>,
    capabilities: Option<ClientCapabilities>,
) -> Request {
    Request::Hello(Hello {
        protocol_version,
        client_name: "spoolframe-probe".into(),
        client_version: env!("CARGO_PKG_VERSION").into(),
        platform: Platform::Cli,
        device_name,
        capabilities,
        experimental: Vec::new(),
        features: spoolframe_protocol::Features::empty(),
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
