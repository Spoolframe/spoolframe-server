//! A server's own client of another's `spoolframe/client/1` (P8-04): what
//! the server a library is shared with (SB) plays a paired server's (SA's)
//! item with, in proxy mode, as one more device of its user.
//!
//! `federation/1` carries no media: in proxy mode SB redeems a play token
//! on SA's client protocol and plays like any device, with the same
//! requests, reads and controls, then plays on to its own device. SA's code
//! is the same in both modes. The server cannot depend on the client SDK
//! (it is permissive, and the server AGPL, §56), so this is a small client
//! of its own over the protocol crate's codec: one connection, its `Hello`,
//! one request per stream, and the events SA sends.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use iroh::endpoint::{Connection, RecvStream, VarInt};
use iroh::{Endpoint, EndpointAddr, EndpointId};

use crate::DialError;
use crate::dial::within;
use spoolframe_protocol::codec::{CodecError, read_frame, read_reply, write_frame};
use spoolframe_protocol::policy::retry_safe;
use spoolframe_protocol::resource::{ResourceRequest, ResourceResponse};
use spoolframe_protocol::rpc::{
    OPERATION_WINDOW_MS, Operation, RpcReply, RpcRequest, RpcResponse, ServerInstance,
};
use spoolframe_protocol::streams::{bi, uni};
use spoolframe_protocol::{
    CLIENT_ALPN, Event, Hello, Reply, Request, RequestEnvelope, Response, ServerInfo, SessionId,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How long a request may take to be answered: a start probes and plans,
/// as the protocol's own waits say, and a little more.
fn answer_wait(request: &Request) -> Duration {
    spoolframe_protocol::policy::server_wait(request) + Duration::from_secs(15)
}

/// How long a resource's answer may take before its body: a read past
/// what an encode wrote waits on the server.
const RESOURCE_WAIT: Duration = Duration::from_secs(30);

/// How many of the other server's events are kept for a listener that
/// falls behind.
const EVENTS_KEPT: usize = 64;

/// The streams the other server may open on this connection: its events'.
/// A server's endpoint takes none from its own clients, so the connection
/// it dials as a client says so itself.
const EVENT_STREAMS: u32 = 4;

/// What the other server may send ahead of this one reading it: the
/// media it serves comes this way, as to any device.
const RECEIVE_WINDOW: u32 = 16 << 20;

/// A connection to another server's `client/1`, its `Hello` answered.
#[derive(Debug)]
pub struct PlayClient {
    connection: Connection,
    /// The other server's process, as its last answer named it, and when
    /// that answer came by this server's clock: what an operation is made
    /// for, and when it expires.
    instance: parking_lot::Mutex<(ServerInstance, Instant)>,
    /// What this client's operation ids start with: when it was made, so
    /// a later client's never meet an earlier one's.
    prefix: String,
    next: AtomicU64,
    events: tokio::sync::broadcast::Sender<Event>,
}

impl PlayClient {
    /// Dial `addr` from `endpoint` (the server's own: its key is the
    /// device the other server sees) and say `hello`: the client, and what
    /// the other server said of itself.
    pub async fn connect(
        endpoint: &Endpoint,
        addr: EndpointAddr,
        hello: Hello,
    ) -> Result<(Arc<Self>, ServerInfo), DialError> {
        let connection = crate::dial(endpoint, addr, CLIENT_ALPN).await?;
        connection.set_max_concurrent_uni_streams(VarInt::from_u32(EVENT_STREAMS));
        connection.set_receive_window(VarInt::from_u32(RECEIVE_WINDOW));
        let (events, _) = tokio::sync::broadcast::channel(EVENTS_KEPT);
        let since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let client = Arc::new(Self {
            connection,
            instance: parking_lot::Mutex::new((
                ServerInstance {
                    id: String::new(),
                    time_ms: 0,
                },
                Instant::now(),
            )),
            prefix: format!("{since:x}"),
            next: AtomicU64::new(0),
            events,
        });
        tokio::spawn(read_events(
            client.connection.clone(),
            client.events.clone(),
        ));
        let answered = client.call(None, Request::Hello(hello), None).await;
        let info = match answered.map(|reply| reply.response) {
            Ok(Response::Ok(Reply::ServerInfo(info))) => info,
            Ok(other) => {
                client.close();
                return Err(DialError::Unreachable(format!(
                    "the server did not say hello: {other:?}"
                )));
            }
            Err(e) => {
                client.close();
                return Err(e);
            }
        };
        Ok((client, info))
    }

    /// `request` in `session`, at `expected_revision` for a playback's
    /// control: the other server's answer, and the playback's revision
    /// after it, when it says. A request that changes something is sent
    /// as an operation of the other server's process, once.
    pub async fn call(
        &self,
        session: Option<&SessionId>,
        request: Request,
        expected_revision: Option<u64>,
    ) -> Result<RpcReply, DialError> {
        let operation = (!retry_safe(&request)).then(|| {
            let (instance, answered) = self.instance.lock().clone();
            Operation {
                id: format!(
                    "{}-{}",
                    self.prefix,
                    self.next.fetch_add(1, Ordering::Relaxed)
                ),
                instance: instance.id,
                expires_at_ms: expires_at_ms(instance.time_ms, answered, Instant::now()),
            }
        });
        let wait = answer_wait(&request);
        let wire = RpcRequest {
            envelope: RequestEnvelope {
                session: session.cloned(),
                request,
            },
            operation,
            expected_revision,
        };
        let exchange = async {
            let (mut send, mut recv) = self.connection.open_bi().await.map_err(broken)?;
            send.write_u8(bi::RPC).await.map_err(broken)?;
            write_frame(&mut send, &wire).await.map_err(codec_error)?;
            send.finish().map_err(broken)?;
            read_reply::<_, RpcResponse>(&mut recv)
                .await
                .map_err(codec_error)
        };
        let response = within(wait, "no answer in time", exchange).await?;
        *self.instance.lock() = (response.server, Instant::now());
        Ok(response.reply)
    }

    /// A resource stream's `request`: the other server's answer, and the
    /// stream its body follows on, for the caller to read `length` bytes
    /// of, as they arrive.
    pub async fn resource(
        &self,
        request: &ResourceRequest,
    ) -> Result<(ResourceResponse, RecvStream), DialError> {
        let exchange = async {
            let (mut send, mut recv) = self.connection.open_bi().await.map_err(broken)?;
            send.write_u8(bi::RESOURCE).await.map_err(broken)?;
            write_frame(&mut send, request).await.map_err(codec_error)?;
            send.finish().map_err(broken)?;
            let response: ResourceResponse = read_frame(&mut recv).await.map_err(codec_error)?;
            Ok::<_, DialError>((response, recv))
        };
        within(RESOURCE_WAIT, "no answer in time", exchange).await
    }

    /// The other server's events from now: a playback's share of its
    /// uplink, among others.
    pub fn events(&self) -> tokio::sync::broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// The other server's key: the one this connection was dialled at.
    pub fn remote_id(&self) -> EndpointId {
        self.connection.remote_id()
    }

    /// Whether the connection is closed.
    pub fn is_closed(&self) -> bool {
        self.connection.close_reason().is_some()
    }

    /// Once the connection closes, by either side or by its idle timeout.
    pub async fn closed(&self) {
        self.connection.closed().await;
    }

    /// Close the connection: every session on it plays on until the other
    /// server ends it.
    pub fn close(&self) {
        self.connection.close(VarInt::from_u32(0), b"done");
    }
}

/// When an operation made at `now` expires, by the other server's clock:
/// its time at its last answer (`server_ms`, which came at `answered` by
/// this server's clock), moved on by the time passed here since, and the
/// window. The connection is kept between plays, so that answer may be
/// long past: the window from it alone would have expired.
fn expires_at_ms(server_ms: u64, answered: Instant, now: Instant) -> u64 {
    let since =
        u64::try_from(now.saturating_duration_since(answered).as_millis()).unwrap_or(u64::MAX);
    server_ms
        .saturating_add(since)
        .saturating_add(OPERATION_WINDOW_MS)
}

/// Read the events the other server sends on `connection`, into `events`,
/// until it closes.
async fn read_events(connection: Connection, events: tokio::sync::broadcast::Sender<Event>) {
    while let Ok(mut stream) = connection.accept_uni().await {
        let events = events.clone();
        tokio::spawn(async move {
            if stream.read_u8().await.ok() != Some(uni::EVENTS) {
                return;
            }
            while let Ok(event) = read_frame::<_, Event>(&mut stream).await {
                // No listener now is no loss: an event is for a playback
                // that may have none yet.
                let _ = events.send(event);
            }
        });
    }
}

fn broken(e: impl std::fmt::Display) -> DialError {
    DialError::Broken(e.to_string())
}

fn codec_error(e: CodecError) -> DialError {
    DialError::Broken(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An operation made long after the other server's last answer, on a
    /// connection kept between plays, expires a window after the other
    /// server's now, as this server counts it, not after that answer.
    #[test]
    fn an_operation_long_after_the_last_answer_expires_a_window_from_now() {
        let answered = Instant::now();
        let idle = Duration::from_secs(20 * 60);
        assert_eq!(
            expires_at_ms(1_000_000, answered, answered + idle),
            1_000_000 + 20 * 60 * 1000 + OPERATION_WINDOW_MS
        );
        assert_eq!(
            expires_at_ms(1_000_000, answered, answered),
            1_000_000 + OPERATION_WINDOW_MS
        );
    }
}
