//! `spoolframe/client/1` on one connection (ARCHITECTURE §20, §33).
//!
//! Each bidirectional stream the client opens is one RPC. The first
//! request must be `Hello`; after it the server opens the events stream.
//! Only the pre-auth requests are served without a session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use iroh::EndpointId;
use iroh::endpoint::{Connection, RecvStream, SendStream, VarInt};
use iroh::protocol::{AcceptError, ProtocolHandler};
use spoolframe_catalog::{CatalogError, Page};
use spoolframe_media::{MediaDescriptor, SubtitleSource, SubtitleStream, codecs};
use spoolframe_protocol::close;
use spoolframe_protocol::codec::{self, write_frame};
use spoolframe_protocol::device_sign_in::{
    normalize as device_sign_in_code, web_link as device_sign_in_web_link,
};
use spoolframe_protocol::resource::{ResourceMetadata, ResourceRequest, ResourceResponse};
use spoolframe_protocol::rpc::{
    PlaybackRevision, RpcReply, RpcRequest, RpcResponse, ServerInstance,
};
use spoolframe_protocol::streams::{bi, reset, uni};
use spoolframe_protocol::{
    APP_RP_ID, AuthMethod, DeviceInfo, DeviceSignInRequest, Error, Event, FinishPasskeySignIn,
    Hello, LoginPassword, PasskeyInfo, PinProof, PinState, PlexLink, PlexSyncSummary, Reply,
    Request, Response, ServerInfo, SessionId, SessionInfo, UserSummary,
};
use spoolframe_protocol::{
    ArtworkRequest, ArtworkSize, ChosenTracks, ConvertedSubtitle, MAX_READ, MediaRead,
    MediaReadReply, PlaybackHandle, PlaybackPlan, SUBTITLE_CHUNK, Selection, SubtitleAction,
    TopResult,
};
use spoolframe_types::{ArtworkId, Item, MediaId, MediaKind, Namespace, SubtitleFormat};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex, watch};

use crate::admission::{Peer, SIGN_OUT_DRAIN};
use crate::artwork::{Artwork, ArtworkError};
use crate::auth::{self, Approval, LoginError, RECENT_SIGN_IN, SessionToken, UnlockError};
use crate::device_sign_in::{self, ClaimError, Outcome, Waiting};
use crate::encode::job::{EncodeJob, JobError, Progress};
use crate::encode::{Conversion, ConversionSource, hls};
use crate::network::{ConnectionAuth, Connections, ServerName, Services};
use crate::passkeys::{CeremonyError, SignInError};
use crate::planner;
use crate::playback::{Changed, FileId, Opened, Playback, Playbacks, Refusal, Sidecar};
use crate::plex_accounts::Link;
use crate::previews::{self, PreviewState};
use crate::principal::Principal;
use crate::store::passkeys::{PasskeyError, PasskeyRecord};
use crate::store::pins::PinError;
use crate::store::sessions::{SessionBinding, SessionUser};
use crate::subtitles::{TextSource, TextState};
use crate::tracks::{self, TrackOverride};
use crate::watch::WatchError;
use spoolframe_admin_api::PlexSyncRun;
use spoolframe_protocol::ConvertedVideo;
use spoolframe_types::Delivery;

/// How long shutdown waits to deliver `ShuttingDown` to one client.
const SHUTDOWN_NOTICE: Duration = Duration::from_millis(500);

/// The client protocol handler shared by every connection.
#[derive(Debug, Clone)]
pub(crate) struct ClientProtocol {
    /// This server's endpoint id, which app passkey challenges are made
    /// with (PX-10).
    id: EndpointId,
    connections: Connections,
    services: Services,
    info: Arc<ServerInfo>,
    /// What `info` calls the server, as it is now (P3-10).
    name: ServerName,
    /// The events stream of each connection that said Hello.
    events: Arc<Mutex<HashMap<usize, SendStream>>>,
    instance: String,
    operations: Arc<crate::operations::Operations>,
    anonymous_operations: Arc<crate::operations::Operations>,
}

/// What one connection has said so far.
#[derive(Debug)]
struct ConnectionState {
    /// Its limits, and its streams being answered (PX-10).
    peer: Arc<Peer>,
    hello: OnceLock<Hello>,
    /// Profile locks (P2-08).
    locks: std::sync::Mutex<Locks>,
}

/// Which sessions on one connection need their user's PIN, and which are
/// unlocked (P2-08). Kept per connection: a new connection starts locked,
/// whatever an earlier one unlocked.
#[derive(Debug, Default)]
struct Locks {
    /// Each session looked up on this connection: whether its device asks
    /// for its user's PIN.
    needs_pin: HashMap<String, bool>,
    unlocked: std::collections::HashSet<String>,
    /// The explicitly selected foreground profile. Background requests
    /// never change it or invalidate its PIN unlock.
    active: Option<String>,
    generation: u64,
}

enum ResourceBody {
    Bytes(Vec<u8>),
    File(tokio::fs::File),
}

impl Locks {
    fn knows(&self, session: &SessionId) -> bool {
        self.needs_pin.contains_key(&session.0)
    }

    fn learn(&mut self, session: &SessionId, needs_pin: bool) {
        self.needs_pin.insert(session.0.clone(), needs_pin);
    }

    fn locked(&self, session: &SessionId) -> bool {
        self.needs_pin.get(&session.0) == Some(&true) && !self.unlocked.contains(&session.0)
    }

    /// Background and foreground requests obey the same PIN check.
    fn use_session(&mut self, session: &SessionId) -> Result<(), Error> {
        if self.locked(session) {
            return Err(Error::ProfileLocked);
        }
        Ok(())
    }

    fn activate(&mut self, session: &SessionId) {
        if self.active.as_deref() != Some(session.0.as_str()) {
            self.generation += 1;
            self.unlocked.retain(|unlocked| unlocked == &session.0);
            self.active = Some(session.0.clone());
        }
    }

    fn unlock(&mut self, session: &SessionId) {
        self.unlocked.insert(session.0.clone());
    }
}

impl ClientProtocol {
    pub(crate) fn new(
        id: EndpointId,
        connections: Connections,
        info: ServerInfo,
        name: ServerName,
        services: Services,
    ) -> Self {
        Self {
            id,
            connections,
            services,
            info: Arc::new(info),
            name,
            events: Arc::default(),
            instance: uuid::Uuid::now_v7().to_string(),
            operations: Arc::default(),
            anonymous_operations: Arc::new(crate::operations::Operations::anonymous()),
        }
    }

    /// One stream: its kind byte, then (for RPC) one request and its
    /// response.
    async fn stream(
        &self,
        connection: Connection,
        state: Arc<ConnectionState>,
        mut send: SendStream,
        mut recv: RecvStream,
    ) {
        let _answering = state.peer.answering();
        // A stream holds one of the peer's few stream credits, and its
        // frame's memory, until its first frame is in (PX-10).
        let first = tokio::time::timeout(self.connections.admission().first_frame, async {
            let kind = recv.read_u8().await?;
            let frame = if kind == bi::RPC || kind == bi::RESOURCE {
                Some(codec::read_frame_bytes_within(&mut recv, state.peer.max_frame()).await)
            } else {
                None
            };
            Ok::<_, std::io::Error>((kind, frame))
        })
        .await;
        let (kind, frame) = match first {
            Ok(Ok(first)) => first,
            // The stream ended before its kind byte.
            Ok(Err(_)) => return,
            Err(_) => {
                tracing::debug!(
                    endpoint = %connection.remote_id().fmt_short(),
                    "a stream's first frame did not arrive in time"
                );
                let code = VarInt::from_u32(reset::FIRST_FRAME_TIMEOUT);
                let _ = send.reset(code);
                let _ = recv.stop(code);
                return;
            }
        };
        let Some(request) = frame else {
            // A kind from a newer client. The stream is refused; the
            // connection carries on.
            tracing::debug!(kind, "refusing a stream of unknown kind");
            let code = VarInt::from_u32(reset::UNKNOWN_STREAM_KIND);
            let _ = send.reset(code);
            let _ = recv.stop(code);
            return;
        };
        if kind == bi::RESOURCE {
            self.resource(&connection, &state, send, request).await;
            return;
        }
        let response = match &request {
            Ok(bytes) => {
                // One span per request, named by its type: never its
                // session or its arguments (P3-14).
                use tracing::Instrument as _;
                let span = tracing::info_span!(
                    "client request",
                    request = codec::rpc_request_type(bytes).unwrap_or_default()
                );
                self.answer(&connection, &state, bytes)
                    .instrument(span)
                    .await
            }
            Err(e) => Response::Err(Error::Malformed {
                message: e.to_string(),
            })
            .into(),
        };
        if let Response::Err(error) = &response.response {
            self.count_error(error);
        }
        let response = RpcResponse {
            server: self.instance(),
            reply: response,
        };
        let sent = match codec::write_reply(&mut send, &response).await {
            // Nothing of it was written. The client gets the reason, not a
            // stream that ends with no response.
            Err(codec::CodecError::TooLarge { len }) => {
                let request = request
                    .ok()
                    .and_then(|bytes| codec::rpc_request_type(&bytes));
                tracing::warn!(request, len, "a reply is over the frame limit");
                let refusal = RpcResponse {
                    server: self.instance(),
                    reply: Response::Err(Error::ReplyTooLarge { bytes: len }).into(),
                };
                codec::write_reply(&mut send, &refusal).await
            }
            sent => sent,
        };
        if let Err(e) = sent {
            tracing::debug!(error = %e, "cannot send a response");
            return;
        }
        let _ = send.finish();
        delivered(&send).await;
    }

    fn instance(&self) -> ServerInstance {
        ServerInstance {
            id: self.instance.clone(),
            time_ms: crate::operations::now_ms(),
        }
    }

    async fn answer(
        &self,
        connection: &Connection,
        state: &Arc<ConnectionState>,
        bytes: &[u8],
    ) -> RpcReply {
        let wire: RpcRequest = match codec::decode(bytes) {
            Ok(wire) => wire,
            Err(e) => {
                return Response::Err(Error::Malformed {
                    message: e.to_string(),
                })
                .into();
            }
        };
        let execute = {
            let this = self.clone();
            let connection = connection.clone();
            let state = state.clone();
            let bytes = bytes.to_vec();
            let request = wire.envelope.request.clone();
            let expected = wire.expected_revision;
            async move {
                let response = this.answer_once(&connection, &state, &bytes).await;
                let playback = match (&response, request) {
                    (Response::Ok(Reply::PlaybackStarted { handle, .. }), _) => {
                        Some(PlaybackRevision {
                            handle: handle.clone(),
                            revision: 0,
                        })
                    }
                    (
                        Response::Ok(Reply::PlaybackChanged { .. } | Reply::PlaybackSeeked { .. }),
                        Request::ChangeSubtitle { handle, .. }
                        | Request::SeekPlayback { handle, .. },
                    ) => Some(PlaybackRevision {
                        handle,
                        revision: expected.expect("checked by control") + 1,
                    }),
                    (
                        Response::Ok(Reply::PlaybackRevision { revision }),
                        Request::PlaybackRevision { handle },
                    ) => Some(PlaybackRevision {
                        handle,
                        revision: *revision,
                    }),
                    _ => None,
                };
                RpcReply { response, playback }
            }
        };
        if spoolframe_protocol::policy::retry_safe(&wire.envelope.request)
            || matches!(wire.envelope.request, Request::Unknown)
        {
            return execute.await;
        }
        // Preserve the handshake error even for an old or anonymous request.
        if state.hello.get().is_none() {
            return Response::Err(Error::HandshakeRequired).into();
        }
        let Some(operation) = wire.operation else {
            return Response::Err(Error::Malformed {
                message: "mutation requires an operation ID".into(),
            })
            .into();
        };
        let endpoint = connection.remote_id().to_string();
        let anonymous = matches!(
            wire.envelope.request,
            Request::LoginPassword(_)
                | Request::BeginDeviceSignIn
                | Request::AwaitDeviceSignIn { .. }
                | Request::BeginPasskeySignIn { .. }
                | Request::FinishPasskeySignIn(_)
        );
        let operations = if anonymous {
            &self.anonymous_operations
        } else {
            &self.operations
        };
        let fingerprint = *blake3::hash(bytes).as_bytes();
        let now = crate::operations::now_ms();
        let cached =
            match operations.lookup(&endpoint, &self.instance, &operation, fingerprint, now) {
                Ok(cached) => cached,
                Err(error) => return Response::Err(error).into(),
            };
        let invocation = if let Some(cached) = cached {
            Ok(cached)
        } else {
            let mut user = None;
            if !anonymous {
                let (session, signed_in) = match self
                    .authenticate(connection, state, wire.envelope.session.clone())
                    .await
                {
                    Ok(user) => user,
                    Err(error) => return Response::Err(error).into(),
                };
                if !matches!(wire.envelope.request, Request::UnlockProfile { .. })
                    && let Err(error) = state
                        .locks
                        .lock()
                        .expect("never poisoned")
                        .use_session(&session)
                {
                    return Response::Err(error).into();
                }
                user = Some(signed_in.user.to_string());
            }
            operations.begin(
                crate::operations::Owner {
                    endpoint: &endpoint,
                    user: user.as_deref(),
                },
                &self.instance,
                &operation,
                fingerprint,
                crate::operations::reservation(&wire.envelope.request),
                crate::operations::now_ms(),
            )
        };
        let invocation = match invocation {
            Ok(invocation) => invocation,
            Err(error) => return Response::Err(error).into(),
        };
        let replay = invocation.execute.is_none();
        if let Some(send) = invocation.execute {
            let operations = operations.clone();
            tokio::spawn(async move {
                let reply = execute.await;
                operations.finish(
                    &endpoint,
                    &operation.id,
                    send,
                    reply,
                    crate::operations::now_ms(),
                );
            });
        }
        let mut outcome = invocation.outcome;
        loop {
            let reply = outcome.borrow().clone();
            if let Some(reply) = reply {
                // A replay can recover an outcome, but cannot restore a PIN
                // unlock from another connection or from before a profile switch.
                if replay
                    && matches!(reply.response, Response::Ok(_))
                    && matches!(
                        wire.envelope.request,
                        Request::UnlockProfile { .. } | Request::SetPinRequired { .. }
                    )
                {
                    let (session, _) = match self
                        .authenticate(connection, state, wire.envelope.session.clone())
                        .await
                    {
                        Ok(user) => user,
                        Err(error) => return Response::Err(error).into(),
                    };
                    if state.locks.lock().expect("never poisoned").locked(&session) {
                        return Response::Err(Error::ProfileLocked).into();
                    }
                }
                return reply;
            }
            if outcome.changed().await.is_err() {
                return Response::Err(Error::Internal).into();
            }
        }
    }

    async fn answer_once(
        &self,
        connection: &Connection,
        state: &ConnectionState,
        bytes: &[u8],
    ) -> Response {
        let wire: RpcRequest = match codec::decode(bytes) {
            Ok(envelope) => envelope,
            Err(e) => {
                return Response::Err(Error::Malformed {
                    message: e.to_string(),
                });
            }
        };
        let envelope = wire.envelope;
        let reply = match envelope.request {
            Request::Hello(hello) => {
                // Kept, and shown to approvers: each string is bounded
                // (PX-10, finding 17).
                let hello = hello.within_bounds();
                if hello.protocol_version != spoolframe_protocol::PROTOCOL_VERSION {
                    return Response::Err(Error::Unsupported {
                        request: "protocol revision; update the SDK".into(),
                    });
                }
                tracing::debug!(?hello, "hello");
                let platform = hello.platform;
                if state.hello.set(hello).is_ok() {
                    // A device signed in before platforms were kept gets
                    // one now; a failure costs only the label.
                    let endpoint = connection.remote_id().to_string();
                    if let Some(hello) = state.hello.get() {
                        self.services.connected.hello(
                            connection.stable_id(),
                            endpoint.clone(),
                            hello,
                        );
                    }
                    if let Err(e) = self.store().note_device_platform(&endpoint, platform).await {
                        tracing::warn!(error = %e, %endpoint, "cannot keep the device's platform");
                    }
                    self.open_events(connection).await;
                }
                Ok(Reply::ServerInfo(ServerInfo {
                    name: self.name.get(),
                    ..(*self.info).clone()
                }))
            }
            _ if state.hello.get().is_none() => Err(Error::HandshakeRequired),
            Request::ReadSubtitle { .. } | Request::ReadSubtitleSegment { .. } => {
                Err(Error::Unsupported {
                    request: "subtitle bytes require a resource stream".into(),
                })
            }
            Request::Unknown => Err(Error::Unsupported {
                request: codec::rpc_request_type(bytes).unwrap_or_default(),
            }),
            Request::AuthMethods => Ok(Reply::AuthMethods {
                methods: self.info.auth_methods.clone(),
            }),
            Request::Ping => Ok(Reply::Pong),
            Request::LoginPassword(login) => {
                let hello = state.hello.get().expect("checked above");
                match self.stranger_attempt(state) {
                    Ok(()) => self.login_password(connection, login, hello).await,
                    Err(e) => Err(e),
                }
            }
            Request::BeginDeviceSignIn => {
                let hello = state.hello.get().expect("checked above");
                let started = self.services.auth.sign_ins().begin(Waiting {
                    endpoint: connection.remote_id().to_string(),
                    device_name: hello.device_name.clone(),
                    client_name: hello.client_name.clone(),
                    platform: hello.platform,
                    requested_at: crate::store::now(),
                });
                match started {
                    Some(started) => Ok(Reply::DeviceSignInStarted {
                        web_url: self
                            .services
                            .public_url
                            .as_deref()
                            .map(|origin| device_sign_in_web_link(origin, &started.code)),
                        request: started.request,
                        code: started.code,
                        expires_in_ms: started.expires_in.as_millis() as u64,
                    }),
                    None => Err(Error::RateLimited {
                        retry_after_ms: device_sign_in::LIFETIME.as_millis() as u64,
                    }),
                }
            }
            Request::AwaitDeviceSignIn { request } => {
                let endpoint = connection.remote_id().to_string();
                let outcome = self
                    .services
                    .auth
                    .sign_ins()
                    .wait(&request, &endpoint, device_sign_in::AWAIT)
                    .await;
                match outcome {
                    Outcome::Approved(token) => {
                        tracing::info!(%endpoint, "device signed in by approval");
                        self.connections.signed_in(&connection.remote_id());
                        Ok(Reply::LoggedIn {
                            session: SessionId(token.0),
                        })
                    }
                    Outcome::Pending => Ok(Reply::DeviceSignInPending),
                    Outcome::Denied => Err(Error::DeviceSignInDenied),
                    Outcome::Expired => Err(Error::DeviceSignInExpired),
                }
            }
            Request::BeginPasskeySignIn { username } => {
                let endpoint = connection.remote_id().to_string();
                // Held per endpoint, and endpoints are free: strangers'
                // beginnings share their sign-in budget (PX-10).
                if let Err(e) = self.stranger_attempt(state) {
                    return Response::Err(e);
                }
                self.services
                    .app_passkeys
                    .begin_sign_in(
                        self.store(),
                        self.id.as_bytes(),
                        &endpoint,
                        username.as_deref(),
                    )
                    .await
                    .map(Reply::PasskeySignInBegun)
                    .map_err(sign_in_error)
            }
            Request::FinishPasskeySignIn(signed) => {
                let hello = state.hello.get().expect("checked above");
                match self.stranger_attempt(state) {
                    Ok(()) => {
                        self.finish_passkey_sign_in(connection, &signed, hello)
                            .await
                    }
                    Err(e) => Err(e),
                }
            }
            // Everything else needs a session that is live on this
            // endpoint. Each request names its own, so one connection can
            // serve several users at once (§15).
            request => match self.authenticate(connection, state, envelope.session).await {
                Ok((session, user)) => {
                    // Locked, only these two (P2-08).
                    let exempt = matches!(
                        request,
                        Request::ActivateProfile | Request::WhoAmI | Request::UnlockProfile { .. }
                    );
                    let allowed = if exempt {
                        Ok(())
                    } else {
                        state
                            .locks
                            .lock()
                            .expect("never poisoned")
                            .use_session(&session)
                    };
                    match allowed {
                        Ok(()) => {
                            self.serve(
                                connection,
                                state,
                                session,
                                user,
                                request,
                                wire.expected_revision,
                            )
                            .await
                        }
                        Err(e) => Err(e),
                    }
                }
                Err(e) => Err(e),
            },
        };
        reply.into()
    }

    /// The user of `session`, if it is live and bound to this connection's
    /// endpoint. Every failure is the same `Unauthenticated`. Whether its
    /// device asks for the user's PIN is noted for the connection's locks.
    async fn authenticate(
        &self,
        connection: &Connection,
        state: &ConnectionState,
        session: Option<SessionId>,
    ) -> Result<(SessionId, SessionUser), Error> {
        let session = session.ok_or(Error::Unauthenticated)?;
        let endpoint = connection.remote_id().to_string();
        match self
            .services
            .auth
            .endpoint_session(&session.0, &endpoint)
            .await
        {
            Ok(Some(user)) => {
                state
                    .locks
                    .lock()
                    .expect("never poisoned")
                    .learn(&session, user.pin_required);
                Ok((session, user))
            }
            Ok(None) => Err(Error::Unauthenticated),
            Err(e) => Err(internal(e)),
        }
    }

    async fn resource(
        &self,
        connection: &Connection,
        state: &ConnectionState,
        mut send: SendStream,
        frame: Result<Vec<u8>, codec::CodecError>,
    ) {
        let request = frame.and_then(|bytes| codec::decode::<ResourceRequest>(&bytes));
        let result = match request {
            Ok(request) => self.open_resource(connection, state, request).await,
            Err(e) => Err(Error::Malformed {
                message: e.to_string(),
            }),
        };
        let sent = async {
            match result {
                Ok((metadata, length, body)) => {
                    let media = matches!(metadata, ResourceMetadata::Media { .. });
                    write_frame(&mut send, &ResourceResponse::Ready { length, metadata }).await?;
                    match body {
                        ResourceBody::Bytes(bytes) => send
                            .write_all(&bytes)
                            .await
                            .map_err(std::io::Error::other)?,
                        ResourceBody::File(file) => {
                            let copied = tokio::io::copy(&mut file.take(length), &mut send).await?;
                            if media {
                                self.services.metrics.media_reads.inc();
                                self.services.metrics.media_read_bytes.add(copied);
                            }
                            if copied != length {
                                return Err(codec::CodecError::Io(std::io::Error::new(
                                    std::io::ErrorKind::UnexpectedEof,
                                    "resource ended before its advertised length",
                                )));
                            }
                        }
                    }
                }
                Err(error) => {
                    self.count_error(&error);
                    write_frame(&mut send, &ResourceResponse::Error { error }).await?
                }
            }
            Ok::<_, codec::CodecError>(())
        }
        .await;
        match sent {
            Ok(()) => {
                let _ = send.finish();
                delivered(&send).await;
            }
            Err(e) => {
                tracing::debug!(error = %e, "resource read failed");
                let _ = send.reset(0u32.into());
            }
        }
    }

    async fn open_resource(
        &self,
        connection: &Connection,
        state: &ConnectionState,
        request: ResourceRequest,
    ) -> Result<(ResourceMetadata, u64, ResourceBody), Error> {
        if state.hello.get().is_none() {
            return Err(Error::HandshakeRequired);
        }
        match request {
            ResourceRequest::Artwork { request, max_bytes } => {
                let image = self.find_artwork(connection, state, request).await?;
                let length = image.bytes.len() as u64;
                if length > max_bytes {
                    return Err(Error::ReplyTooLarge { bytes: length });
                }
                Ok((
                    ResourceMetadata::Artwork {
                        content_type: image.content_type,
                        etag: image.etag,
                    },
                    length,
                    ResourceBody::Bytes(image.bytes),
                ))
            }
            ResourceRequest::Media { request, revision } => {
                let result = self.open_range(connection, state, &request, revision).await;
                if let (Some(n), Err(error)) = (request.segment, &result) {
                    tracing::warn!(id = %self.played(&request.handle), endpoint = %connection.remote_id().fmt_short(),
                        segment = n, %error, "an HLS segment read failed");
                }
                let (file, reply) = result?;
                // Seeking can replace the encode while a read waits. Check
                // again after preparation, outside that wait, before labelling
                // the pinned file with this revision.
                let _control = self
                    .services
                    .playbacks
                    .control(
                        &request.handle,
                        &request.session,
                        &connection.remote_id().to_string(),
                    )
                    .await
                    .map_err(playback_refusal)?;
                let current = self
                    .use_playback(connection, &request.handle, &request.session)
                    .await?;
                if current.revision != revision {
                    return Err(Error::RevisionConflict {
                        revision: current.revision,
                    });
                }
                // On its chart in playback debug (P3-15).
                self.services.playbacks.read(
                    &request.handle,
                    request.offset,
                    reply.sent,
                    request.segment,
                );
                Ok((
                    ResourceMetadata::Media {
                        size: reply.size,
                        complete: reply.complete,
                        epoch: reply.epoch,
                        revision,
                    },
                    reply.sent,
                    ResourceBody::File(file),
                ))
            }
            request => {
                let (session, handle, revision, read) = match request {
                    ResourceRequest::Subtitle {
                        session,
                        handle,
                        revision,
                        offset,
                        version,
                    } => (
                        session,
                        handle.clone(),
                        revision,
                        Request::ReadSubtitle {
                            handle,
                            offset,
                            version,
                        },
                    ),
                    ResourceRequest::SubtitleSegment {
                        session,
                        handle,
                        revision,
                        segment,
                    } => (
                        session,
                        handle.clone(),
                        revision,
                        Request::ReadSubtitleSegment { handle, n: segment },
                    ),
                    _ => unreachable!(),
                };
                let (session, user) = self.authenticate(connection, state, Some(session)).await?;
                state
                    .locks
                    .lock()
                    .expect("never poisoned")
                    .use_session(&session)?;
                // Preparing the selected subtitle and its metadata is atomic
                // with control changes. Sending its bytes holds no lock.
                let _control = self
                    .services
                    .playbacks
                    .control(&handle, &session, &connection.remote_id().to_string())
                    .await
                    .map_err(playback_refusal)?;
                let playback = self.use_playback(connection, &handle, &session).await?;
                if playback.revision != revision {
                    return Err(Error::RevisionConflict {
                        revision: playback.revision,
                    });
                }
                let reply = self
                    .serve(connection, state, session, user, read, None)
                    .await?;
                let (format, bytes, total, version) = match reply {
                    Reply::SubtitleChunk {
                        format,
                        bytes,
                        total,
                        version,
                    } => (format, bytes, total, version),
                    Reply::Subtitle { format, bytes } => {
                        let total = bytes.len() as u64;
                        (format, bytes, total, 0)
                    }
                    _ => unreachable!(),
                };
                let length = bytes.len() as u64;
                if length > MAX_READ {
                    return Err(Error::ReplyTooLarge { bytes: length });
                }
                Ok((
                    ResourceMetadata::Subtitle {
                        format,
                        total,
                        version,
                        revision,
                    },
                    length,
                    ResourceBody::Bytes(bytes),
                ))
            }
        }
    }

    async fn find_artwork(
        &self,
        connection: &Connection,
        state: &ConnectionState,
        request: ArtworkRequest,
    ) -> Result<Artwork, Error> {
        let (session, _) = self
            .authenticate(connection, state, Some(request.session))
            .await?;
        state
            .locks
            .lock()
            .expect("never poisoned")
            .use_session(&session)?;
        if request.id.namespace() == Namespace::Preview {
            return self.preview_sheet(&request.id, request.size).await;
        }
        self.services
            .artwork
            .get(&request.id, request.size)
            .await
            .map_err(|e| match e {
                ArtworkError::Catalog(e) => catalog_error(e),
                ArtworkError::BadSize(size) => Error::Malformed {
                    message: format!("artwork size {} is not served", size.as_str()),
                },
                other => internal(other),
            })
    }

    /// A sheet of frame previews (P3-13), from the previews cache rather
    /// than the catalog's. A size below the sheet's scales it whole; the
    /// client scales the tile rectangles to match.
    async fn preview_sheet(&self, id: &ArtworkId, size: ArtworkSize) -> Result<Artwork, Error> {
        let not_found = || Error::NotFound {
            what: id.to_string(),
        };
        let (key, n) = previews::sheet_of(id).ok_or_else(not_found)?;
        let bytes = self
            .services
            .previews
            .sheet(key, n)
            .await
            .ok_or_else(not_found)?;
        let sheet = Artwork {
            bytes,
            content_type: "image/jpeg".into(),
            etag: format!("{key}-{n}-{}", size.as_str()),
        };
        let width = match size {
            ArtworkSize::Unknown => {
                return Err(Error::Malformed {
                    message: format!("artwork size {} is not served", size.as_str()),
                });
            }
            size => size.width(),
        };
        let Some(width) = width else {
            return Ok(sheet);
        };
        let etag = sheet.etag.clone();
        let image = tokio::task::spawn_blocking(move || crate::artwork::resize(sheet, width))
            .await
            .map_err(internal)?
            .map_err(internal)?;
        Ok(Artwork {
            bytes: image.bytes,
            content_type: image.content_type,
            etag,
        })
    }

    /// The playback behind `handle`, if `session` on this connection's
    /// endpoint may use it. The session itself is checked again at most
    /// every `playback::RECHECK`, so reads stay off the database.
    async fn use_playback(
        &self,
        connection: &Connection,
        handle: &PlaybackHandle,
        session: &SessionId,
    ) -> Result<Playback, Error> {
        let endpoint = connection.remote_id().to_string();
        let playbacks = &self.services.playbacks;
        let using = playbacks
            .use_handle(handle, session, &endpoint)
            .map_err(playback_refusal)?;
        if using.recheck {
            let live = self
                .services
                .auth
                .endpoint_session(&session.0, &endpoint)
                .await
                .map_err(internal)?;
            if live.is_none() {
                playbacks.end(handle, crate::playback::EndReason::SignedOut);
                return Err(Error::Unauthenticated);
            }
        }
        Ok(using.playback)
    }

    /// A playback's end as clients see it, counted (P3-14).
    fn count_error(&self, error: &Error) {
        let metrics = &self.services.metrics;
        match error {
            Error::PlaybackExpired => metrics.playback_expired.inc(),
            Error::PlaybackFailed { .. } => metrics.playback_failed.inc(),
            _ => {}
        }
    }

    /// What `handle` plays, for the log; the handle itself is never logged.
    fn played(&self, handle: &PlaybackHandle) -> String {
        self.services
            .playbacks
            .item(handle)
            .map_or_else(|| "(no open playback)".to_owned(), |id| id.to_string())
    }

    /// WebVTT segment `n` of `handle`'s subtitle rendition (P2-07).
    async fn subtitle_segment(
        &self,
        connection: &Connection,
        session: &SessionId,
        handle: &PlaybackHandle,
        n: u32,
    ) -> Result<Reply, Error> {
        let playback = self.use_playback(connection, handle, session).await?;
        let span = playback
            .conversion
            .as_ref()
            .filter(|c| c.subtitle_rendition().is_some())
            .and_then(|c| c.segment_span(n));
        let (Some((start_ms, end_ms)), Some(Sidecar::Made { state, .. })) =
            (span, playback.subtitle)
        else {
            return Err(Error::NotFound {
                what: format!("subtitle segment {n} in this playback"),
            });
        };
        let path = read_text(state).await?;
        let vtt = tokio::fs::read_to_string(&path).await.map_err(|e| {
            tracing::warn!(path = %path.display(), error = %e, "cannot read a subtitle");
            Error::Unavailable {
                message: "the subtitle file could not be read".into(),
            }
        })?;
        Ok(Reply::Subtitle {
            format: SubtitleFormat::WebVtt,
            bytes: hls::subtitle_segment(&vtt, start_ms, end_ms).into_bytes(),
        })
    }

    /// The file positioned for `read`, and the reply that will precede it.
    async fn open_range(
        &self,
        connection: &Connection,
        state: &ConnectionState,
        read: &MediaRead,
        revision: u64,
    ) -> Result<(tokio::fs::File, MediaReadReply), Error> {
        let playback = self
            .use_playback(connection, &read.handle, &read.session)
            .await?;
        if playback.revision != revision {
            return Err(Error::RevisionConflict {
                revision: playback.revision,
            });
        }
        // The lock, in memory, on every read. A session this connection
        // has not looked up yet (a playback started before a reconnect) is
        // looked up first, so a new connection does not skip its lock.
        let known = state
            .locks
            .lock()
            .expect("never poisoned")
            .knows(&read.session);
        if !known {
            self.authenticate(connection, state, Some(read.session.clone()))
                .await?;
        }
        state
            .locks
            .lock()
            .expect("never poisoned")
            .use_session(&read.session)?;
        if let Some(conversion) = &playback.conversion {
            let keep_alive = || self.services.playbacks.touch(&read.handle);
            return converted_range(conversion, read, &keep_alive).await;
        }
        let size = playback.size;
        let sent = read
            .length
            .min(MAX_READ)
            .min(size.saturating_sub(read.offset));
        let unreadable = |e: std::io::Error| {
            tracing::warn!(path = %playback.path.display(), error = %e, "cannot read a media file");
            Error::Unavailable {
                message: "the media file could not be read".into(),
            }
        };
        let mut file = crate::files::open_nonblocking(&playback.path)
            .await
            .map_err(unreadable)?;
        // The file the playback started with, or none (PX-09 B-1): the
        // path may name another by now.
        let now = FileId::of(&file.metadata().await.map_err(unreadable)?);
        if playback.file != Some(now) {
            tracing::warn!(path = %playback.path.display(), "a media file was replaced during its playback");
            return Err(Error::Unavailable {
                message: "the media file changed during playback".into(),
            });
        }
        if sent > 0 {
            file.seek(std::io::SeekFrom::Start(read.offset))
                .await
                .map_err(unreadable)?;
        }
        Ok((
            file,
            MediaReadReply {
                sent,
                size,
                complete: true,
                epoch: 0,
            },
        ))
    }

    /// Fill in `principal`'s watch state on `items`, in one lookup.
    async fn fill(
        &self,
        principal: Principal,
        items: impl Iterator<Item = &mut spoolframe_types::Item>,
    ) -> Result<(), Error> {
        let mut items: Vec<_> = items.collect();
        self.services
            .watch
            .fill(principal, &mut items)
            .await
            .map_err(watch_error)
    }

    /// A request in a checked session.
    async fn serve(
        &self,
        connection: &Connection,
        state: &ConnectionState,
        session: SessionId,
        user: SessionUser,
        request: Request,
        expected_revision: Option<u64>,
    ) -> Result<Reply, Error> {
        let catalog = &self.services.catalog;
        let principal = Principal::LocalUser(user.user);
        // Hold across preparation and publication, and re-read the handle
        // inside the handler. QUIC requests may arrive concurrently.
        let _control = match &request {
            Request::UpdatePlaybackProgress { handle, .. }
            | Request::ChangeSubtitle { handle, .. }
            | Request::SeekPlayback { handle, .. }
            | Request::StopPlayback { handle } => Some(
                self.services
                    .playbacks
                    .control(handle, &session, &connection.remote_id().to_string())
                    .await
                    .map_err(playback_refusal)?,
            ),
            _ => None,
        };
        if let Request::ChangeSubtitle { handle, .. }
        | Request::SeekPlayback { handle, .. }
        | Request::StopPlayback { handle } = &request
        {
            let playback = self.use_playback(connection, handle, &session).await?;
            if expected_revision != Some(playback.revision) {
                return Err(Error::RevisionConflict {
                    revision: playback.revision,
                });
            }
        }
        match request {
            Request::ActivateProfile => {
                state
                    .locks
                    .lock()
                    .expect("never poisoned")
                    .activate(&session);
                Ok(Reply::Done)
            }
            Request::PlaybackProgressSequence { handle } => Ok(Reply::PlaybackProgressSequence {
                sequence: self
                    .use_playback(connection, &handle, &session)
                    .await?
                    .progress_sequence,
            }),
            Request::PlaybackRevision { handle } => Ok(Reply::PlaybackRevision {
                revision: self
                    .use_playback(connection, &handle, &session)
                    .await?
                    .revision,
            }),
            Request::WhoAmI => {
                // Asked while locked too: a PIN pad sizes itself by it.
                let pin = self.store().pin_info(user.user).await.map_err(internal)?;
                Ok(Reply::WhoAmI(UserSummary {
                    user_id: format!("local:{}", user.user),
                    username: user.username,
                    display_name: user.display_name,
                    role: user.role,
                    locked: state.locks.lock().expect("never poisoned").locked(&session),
                    pin: pin.map(|p| PinState { digits: p.digits }),
                }))
            }
            Request::BeginPasskeyRegistration => {
                // P3-03's rule, as for a passkey added in the web UI: a
                // session left signed in on a device does not add one.
                recently_signed_in(&user)?;
                let account = self.account(&user).await?;
                self.services
                    .app_passkeys
                    .begin_registration(
                        self.store(),
                        self.id.as_bytes(),
                        &SessionToken(session.0.clone()).hash(),
                        &connection.remote_id().to_string(),
                        &account,
                        &self.name.get(),
                    )
                    .await
                    .map(Reply::PasskeyRegistrationBegun)
                    .map_err(ceremony_error)
            }
            Request::FinishPasskeyRegistration(made) => {
                let account = self.account(&user).await?;
                self.services
                    .app_passkeys
                    .finish_registration(
                        self.store(),
                        &SessionToken(session.0.clone()).hash(),
                        &connection.remote_id().to_string(),
                        &account,
                        &made,
                    )
                    .await
                    .map_err(ceremony_error)?;
                Ok(Reply::Done)
            }
            Request::UnlockProfile { pin } => {
                let generation = state.locks.lock().expect("never poisoned").generation;
                let endpoint = connection.remote_id().to_string();
                self.services
                    .auth
                    .check_pin(user.user, &endpoint, &pin)
                    .await
                    .map_err(pin_error)?;
                let mut locks = state.locks.lock().expect("never poisoned");
                if locks.generation != generation
                    || locks
                        .active
                        .as_ref()
                        .is_some_and(|active| active != &session.0)
                {
                    return Err(Error::ProfileLocked);
                }
                locks.unlock(&session);
                Ok(Reply::Done)
            }
            // The PIN, and where it is asked for, need only the session
            // (a real sign-in made it, a locked one never gets here, and a
            // PIN is never a way to sign in), and changing or removing the
            // PIN a proof too: an unlocked session left on a TV must not
            // let whoever holds the remote take the PIN over.
            Request::SetPin { pin, proof } => {
                let Some(digits) = auth::pin_digits(&pin) else {
                    return Err(Error::InvalidInput {
                        message: "a PIN is 4 to 8 digits".into(),
                    });
                };
                match proof {
                    Some(proof) => self.check_pin_proof(connection, &user, proof).await?,
                    None => {
                        if self
                            .store()
                            .pin_info(user.user)
                            .await
                            .map_err(internal)?
                            .is_some()
                        {
                            return Err(Error::InvalidInput {
                                message: "enter the current PIN or the password to change it"
                                    .into(),
                            });
                        }
                    }
                }
                let phc = auth::hash_pin(&pin).await.map_err(internal)?;
                self.store()
                    .set_pin(user.user, &phc, digits)
                    .await
                    .map_err(internal)?;
                tracing::info!(user = %user.username, "PIN set");
                Ok(Reply::Done)
            }
            Request::ClearPin { proof } => {
                self.check_pin_proof(connection, &user, proof).await?;
                self.store().clear_pin(user.user).await.map_err(internal)?;
                tracing::info!(user = %user.username, "PIN cleared");
                Ok(Reply::Done)
            }
            // The web UI's me.passkeys, me.passkeyRename and
            // me.passkeyRemove (P3-05), with the same store calls and so
            // the same rules: only the user's own, the name's length, and
            // the last way to sign in kept.
            Request::ListPasskeys => {
                let passkeys = self
                    .store()
                    .list_passkeys(user.user)
                    .await
                    .map_err(internal)?
                    .into_iter()
                    .map(passkey_info)
                    .collect();
                Ok(Reply::Passkeys { passkeys })
            }
            Request::RenamePasskey { id, name } => {
                self.store()
                    .rename_passkey(user.user, &id, &name)
                    .await
                    .map_err(|e| passkey_error(e, &id))?;
                Ok(Reply::Done)
            }
            Request::RemovePasskey { id } => {
                // As the web UI asks: a device left signed in does not
                // take a way to sign in away.
                recently_signed_in(&user)?;
                self.store()
                    .remove_passkey(user.user, &id)
                    .await
                    .map_err(|e| passkey_error(e, &id))?;
                tracing::info!(user = %user.user, "passkey removed by its user, in an app");
                Ok(Reply::Done)
            }
            // P3-07: the web UI's me.confirmPassword and me.confirmPasskey,
            // for an app, counted under the same keys. The PIN is no proof
            // here: it guards switching profiles on a shared device, and
            // is weaker than a sign-in. A locked session never gets here.
            Request::ConfirmPassword(confirm) => {
                self.confirm_password(connection, &user, &confirm.password)
                    .await?;
                self.confirmed(&session, &user, "password").await
            }
            Request::BeginConfirmPasskey => {
                let endpoint = connection.remote_id().to_string();
                // Refused before a challenge is made that could only fail.
                if let Some(retry_after) = self
                    .services
                    .auth
                    .attempt_limited(&crate::passkeys::limit_key(user.user), Some(&endpoint))
                {
                    return Err(sign_in_error(SignInError::RateLimited { retry_after }));
                }
                let begun = self
                    .services
                    .app_passkeys
                    .begin_confirm(
                        self.store(),
                        self.id.as_bytes(),
                        &SessionToken(session.0.clone()).hash(),
                        &endpoint,
                        user.user,
                    )
                    .await
                    .map_err(sign_in_error)?;
                begun.map(Reply::PasskeySignInBegun).ok_or(Error::NotFound {
                    what: "app passkey".into(),
                })
            }
            Request::FinishConfirmPasskey(signed) => {
                let signer = self
                    .services
                    .app_passkeys
                    .finish_confirm(
                        &self.services.auth,
                        &SessionToken(session.0.clone()).hash(),
                        &connection.remote_id().to_string(),
                        &signed,
                    )
                    .await
                    .map_err(sign_in_error)?;
                // The challenge listed only the user's own; this is the
                // web UI's check too, in case that ever changes.
                if signer.id != user.user {
                    tracing::warn!(
                        user = %user.user,
                        passkey_of = %signer.id,
                        "a passkey of another user's was offered as a confirmation"
                    );
                    return Err(Error::InvalidCredentials);
                }
                self.confirmed(&session, &user, "app passkey").await
            }
            Request::ListDevices => {
                let this = connection.remote_id().to_string();
                let devices = self
                    .store()
                    .user_devices(user.user)
                    .await
                    .map_err(internal)?
                    .into_iter()
                    .map(|d| DeviceInfo {
                        this_device: d.endpoint_id == this,
                        platform: d.platform(),
                        endpoint_id: d.endpoint_id,
                        name: d.name,
                        last_seen_at: d.last_seen_at,
                        pin_required: d.pin_required,
                    })
                    .collect();
                Ok(Reply::Devices { devices })
            }
            Request::GetPlexLink => {
                let Some(accounts) = &self.services.plex_accounts else {
                    return Ok(Reply::PlexLink { link: None });
                };
                let link = accounts.link(user.user).map_err(internal)?;
                let sync = match (&self.services.plex_sync, &link) {
                    (Some(sync), Some(_)) => sync.status(user.user),
                    _ => (None, false),
                };
                Ok(Reply::PlexLink {
                    link: plex_link(link, sync),
                })
            }
            Request::SetPinRequired { device, required } => {
                if !required {
                    recently_signed_in(&user)?;
                }
                let generation = state.locks.lock().expect("never poisoned").generation;
                let this = connection.remote_id().to_string();
                let device = device.unwrap_or_else(|| this.clone());
                self.store()
                    .set_pin_required(user.user, &device, required)
                    .await
                    .map_err(|e| match e {
                        PinError::NoPin => Error::InvalidInput {
                            message: "set a PIN first".into(),
                        },
                        PinError::NoSuchDevice => Error::NotFound {
                            what: format!("device {device}"),
                        },
                        PinError::Store(e) => internal(e),
                    })?;
                if required && device == this {
                    // Who turns it on here is not locked out mid-use: this
                    // connection stays unlocked; the next one asks.
                    let mut locks = state.locks.lock().expect("never poisoned");
                    locks.learn(&session, true);
                    if locks.generation == generation
                        && locks
                            .active
                            .as_ref()
                            .is_none_or(|active| active == &session.0)
                    {
                        locks.unlock(&session);
                    }
                }
                tracing::info!(user = %user.username, %device, required, "PIN requirement changed");
                Ok(Reply::Done)
            }
            Request::Logout => {
                self.services
                    .auth
                    .logout(&session.0)
                    .await
                    .map_err(internal)?;
                Ok(Reply::LoggedOut)
            }
            Request::ListSessions => {
                let current = SessionToken(session.0).hash();
                let sessions = self
                    .services
                    .auth
                    .store()
                    .list_sessions(user.user)
                    .await
                    .map_err(internal)?
                    .into_iter()
                    .map(|s| SessionInfo {
                        current: s.token_hash == current,
                        platform: s.platform(),
                        device_name: s.device_name,
                        created_at: s.created_at,
                        last_used_at: s.last_used_at,
                    })
                    .collect();
                Ok(Reply::Sessions { sessions })
            }
            Request::Libraries => Ok(Reply::Libraries {
                libraries: catalog.libraries().await.map_err(catalog_error)?,
            }),
            Request::Items {
                library,
                offset,
                limit,
            } => {
                let page = Page {
                    offset,
                    limit: limit.min(MAX_PAGE),
                };
                let mut page = catalog.items(&library, page).await.map_err(catalog_error)?;
                self.fill(principal, page.items.iter_mut()).await?;
                Ok(Reply::Items {
                    items: page.items,
                    total: page.total,
                })
            }
            Request::Item { id } => {
                let mut found = catalog.item(&id).await.map_err(catalog_error)?;
                // The one place a client looks at a single item: bring over
                // any state it had under an earlier key.
                self.services
                    .watch
                    .relink(principal, &id, &found.external_ids)
                    .await
                    .map_err(watch_error)?;
                self.fill(
                    principal,
                    std::iter::once(&mut found.item).chain(found.children.iter_mut()),
                )
                .await?;
                Ok(Reply::Item {
                    item: found.item,
                    children: found.children,
                    markers: found.markers,
                })
            }
            Request::Search(mut search) => {
                search.limit = search.limit.min(MAX_PAGE);
                let mut results =
                    crate::search::search(catalog.as_ref(), &search, crate::search::this_year())
                        .await
                        .map_err(catalog_error)?;
                let top = match &mut results.top {
                    Some(TopResult::Item(item)) => Some(item),
                    _ => None,
                };
                let items = top
                    .into_iter()
                    .chain(results.groups.iter_mut().flat_map(|g| g.items.iter_mut()))
                    .chain(results.without_year.iter_mut());
                self.fill(principal, items).await?;
                Ok(Reply::Search(results))
            }
            Request::PersonItems {
                person,
                offset,
                limit,
            } => {
                // A person is in few items: the catalog lists them all,
                // and the page is cut here.
                let all = catalog.person_items(&person).await.map_err(catalog_error)?;
                let total = u32::try_from(all.len()).unwrap_or(u32::MAX);
                let mut items: Vec<_> = all
                    .into_iter()
                    .skip(usize::try_from(offset).unwrap_or(usize::MAX))
                    .take(usize::try_from(limit.min(MAX_PAGE)).unwrap_or(usize::MAX))
                    .collect();
                self.fill(principal, items.iter_mut()).await?;
                Ok(Reply::Items { items, total })
            }
            Request::UpdatePlaybackProgress {
                handle,
                sequence,
                position_ms,
            } => {
                let playback = self.use_playback(connection, &handle, &session).await?;
                let _progress = self
                    .services
                    .playbacks
                    .progress_control(&session, &playback.item)
                    .await;
                self.services.playbacks.check_progress(&handle, sequence)?;
                self.services
                    .watch
                    .progress(
                        principal,
                        &playback.item,
                        Duration::from_millis(position_ms),
                    )
                    .await
                    .map_err(watch_error)?;
                self.services
                    .playbacks
                    .record_progress(&handle, sequence, position_ms);
                Ok(Reply::Done)
            }
            Request::MarkWatched { id } => {
                self.services
                    .watch
                    .mark(principal, &id, true)
                    .await
                    .map_err(watch_error)?;
                Ok(Reply::Done)
            }
            Request::MarkUnwatched { id } => {
                self.services
                    .watch
                    .mark(principal, &id, false)
                    .await
                    .map_err(watch_error)?;
                Ok(Reply::Done)
            }
            Request::HideFromContinueWatching { id } => {
                self.services
                    .watch
                    .hide(principal, &id)
                    .await
                    .map_err(watch_error)?;
                Ok(Reply::Done)
            }
            Request::UnhideFromContinueWatching { id } => {
                self.services
                    .watch
                    .unhide(principal, &id)
                    .await
                    .map_err(watch_error)?;
                Ok(Reply::Done)
            }
            Request::Home => Ok(Reply::Home(
                self.services
                    .watch
                    .home(principal)
                    .await
                    .map_err(watch_error)?,
            )),
            Request::Tracks { id } => {
                let (_, descriptor) = self.describe(&id).await?;
                Ok(Reply::Tracks(descriptor.tracks()))
            }
            Request::GetPreviews { id } => {
                match self
                    .services
                    .previews
                    .state(&id)
                    .await
                    .map_err(catalog_error)?
                {
                    PreviewState::Ready { key, meta } => {
                        Ok(Reply::Previews(previews::reply(&key, &meta)))
                    }
                    PreviewState::Making { done_percent } => {
                        Err(Error::PreviewsNotReady { done_percent })
                    }
                    PreviewState::None => Err(Error::NoPreviews),
                }
            }
            Request::GetTrackPreferences => Ok(Reply::TrackPreferences(
                self.store()
                    .track_preferences(principal)
                    .await
                    .map_err(internal)?,
            )),
            Request::SetTrackPreferences { preferences } => {
                self.store()
                    .set_track_preferences(principal, &preferences)
                    .await
                    .map_err(internal)?;
                Ok(Reply::Done)
            }
            Request::SetTrackOverride { id, selection } => {
                let (_, descriptor) = self.describe(&id).await?;
                let override_ = TrackOverride::from_selection(&descriptor.tracks(), selection);
                let scope = self.override_scope(&id).await?;
                self.store()
                    .set_track_override(principal, &scope, &override_)
                    .await
                    .map_err(internal)?;
                Ok(Reply::Done)
            }
            Request::ClearTrackOverride { id } => {
                let scope = self.override_scope(&id).await?;
                self.store()
                    .clear_track_override(principal, &scope)
                    .await
                    .map_err(internal)?;
                Ok(Reply::Done)
            }
            Request::PlaybackPlan {
                id,
                selection,
                bitmap,
            } => {
                // Capabilities belong to the connection: they came with Hello.
                let caps = state.hello.get().and_then(|h| h.capabilities.as_ref());
                if caps.is_none() {
                    // No file needs reading to know the answer.
                    return Ok(Reply::PlaybackPlan {
                        plan: planner::plan_without_capabilities(),
                        chosen: None,
                    });
                }
                let (_, descriptor) = self.describe(&id).await?;
                let (selection, chosen) = self
                    .selection(principal, &id, &descriptor, selection)
                    .await?;
                Ok(Reply::PlaybackPlan {
                    plan: planner::plan(
                        &descriptor,
                        caps,
                        selection,
                        bitmap,
                        &self.services.transcoding.encoders(),
                        self.services.subtitles.ocr(),
                    ),
                    chosen,
                })
            }
            Request::StartPlayback {
                id,
                selection,
                start_ms,
                bitmap,
            } => {
                let caps = state.hello.get().and_then(|h| h.capabilities.as_ref());
                let (path, descriptor) = self.describe(&id).await?;
                let (selection, chosen) = self
                    .selection(principal, &id, &descriptor, selection)
                    .await?;
                let plan = planner::plan(
                    &descriptor,
                    caps,
                    selection,
                    bitmap,
                    &self.services.transcoding.encoders(),
                    self.services.subtitles.ocr(),
                );
                // The item's markers go with the playback (P2-10), so a
                // player needs no second request for them.
                let found = catalog.item(&id).await.map_err(catalog_error)?;
                let markers = found.markers;
                let mut shown = crate::playback::Shown::of(&found.item);
                if shown.duration_ms.is_none() {
                    shown.duration_ms = descriptor.duration.map(|d| d.as_millis() as u64);
                }
                let hello = state.hello.get().expect("served after Hello");
                let viewer = crate::playback::Viewer {
                    user_id: format!("local:{}", user.user),
                    username: user.username.clone(),
                    display_name: user.display_name.clone(),
                    device_name: hello.device_name.clone(),
                    client_name: hello.client_name.clone(),
                    platform: hello.platform,
                };
                let handle = Playbacks::new_handle();
                // This device's earlier encodes are abandoned: free their
                // slots before this one takes one.
                let endpoint = connection.remote_id().to_string();
                let ended = self.services.playbacks.end_conversions_on(&endpoint);
                if ended > 0 {
                    tracing::info!(%endpoint, ended, "a device's earlier conversions ended");
                }
                let (subtitle, (size, file), conversion) = self
                    .prepare(
                        &handle,
                        &id,
                        (&path, &descriptor),
                        &plan,
                        start_ms.map(Duration::from_millis),
                        None,
                    )
                    .await?;
                // Its previews next, if it has none yet: the player asks
                // for them as soon as it shows a seek bar.
                self.services.previews.played(&id);
                let _progress = self
                    .services
                    .playbacks
                    .progress_control(&session, &id)
                    .await;
                let handle = self.services.playbacks.start(
                    handle,
                    Opened {
                        viewer,
                        shown,
                        planned: plan.clone(),
                        session,
                        endpoint,
                        item: id.clone(),
                        selection,
                        path,
                        size,
                        file,
                        subtitle,
                        conversion,
                    },
                );
                // What was decided, so a playback that then goes quiet
                // can be read from the log; the endpoint as the
                // connection lines print it.
                let decided = planner::summary(&plan);
                tracing::info!(
                    %id,
                    user = %user.username,
                    endpoint = %connection.remote_id().fmt_short(),
                    plan = %decided.kind,
                    delivery = %decided.delivery,
                    video = %decided.video,
                    audio = %decided.audio,
                    subtitle = %decided.subtitle,
                    start_ms = start_ms.unwrap_or(0),
                    "playback started"
                );
                Ok(Reply::PlaybackStarted {
                    handle,
                    plan,
                    size,
                    chosen,
                    epoch: 0,
                    markers,
                })
            }
            Request::ChangeSubtitle {
                handle,
                subtitle,
                bitmap,
                position_ms,
            } => {
                let playback = self.use_playback(connection, &handle, &session).await?;
                let caps = state.hello.get().and_then(|h| h.capabilities.as_ref());
                let (path, descriptor) = self.describe(&playback.item).await?;
                let selection = Selection {
                    audio: playback.selection.audio,
                    subtitle,
                };
                let plan = planner::plan(
                    &descriptor,
                    caps,
                    selection,
                    bitmap,
                    &self.services.transcoding.encoders(),
                    self.services.subtitles.ocr(),
                );
                let (subtitle, (size, file), conversion) = self
                    .prepare(
                        &handle,
                        &playback.item,
                        (&path, &descriptor),
                        &plan,
                        Some(Duration::from_millis(position_ms)),
                        Some(&playback),
                    )
                    .await?;
                let epoch = conversion.as_ref().map_or(0, |c| c.job().epoch);
                let replaced = self.services.playbacks.change(
                    &handle,
                    Changed {
                        planned: plan.clone(),
                        selection,
                        size,
                        file,
                        subtitle,
                        conversion,
                    },
                );
                drop(replaced);
                tracing::info!(id = %playback.item, epoch, "playback's subtitle changed");
                Ok(Reply::PlaybackChanged { plan, size, epoch })
            }
            Request::StopPlayback { handle } => {
                // Only the session and endpoint that started it may end it.
                self.use_playback(connection, &handle, &session).await?;
                self.services
                    .playbacks
                    .end(&handle, crate::playback::EndReason::Finished);
                Ok(Reply::Done)
            }
            // Converted playback only; served from P2-04 and P2-05.
            Request::SeekPlayback {
                handle,
                position_ms,
            } => {
                let playback = self.use_playback(connection, &handle, &session).await?;
                let Some(conversion) = playback.conversion else {
                    // A direct-play client seeks by reading elsewhere.
                    return Err(Error::NotFound {
                        what: "a conversion in this playback".into(),
                    });
                };
                let epoch = conversion
                    .seek(Duration::from_millis(position_ms))
                    .map_err(|e| match e {
                        JobError::Busy => Error::Busy,
                        JobError::Prepare(reason) => Error::PlaybackFailed { reason },
                    })?;
                tracing::debug!(epoch, position_ms, "conversion restarted at a seek");
                self.services.playbacks.revised(&handle);
                Ok(Reply::PlaybackSeeked { epoch })
            }
            Request::GetManifest { handle, kind } => {
                let playback = self.use_playback(connection, &handle, &session).await?;
                playback
                    .conversion
                    .and_then(|c| c.manifest(kind))
                    .map(|text| Reply::Manifest { text })
                    .ok_or_else(|| Error::NotFound {
                        what: "an HLS conversion in this playback".into(),
                    })
            }
            Request::ReadSubtitle {
                handle,
                offset,
                version,
            } => {
                let playback = self.use_playback(connection, &handle, &session).await?;
                let (path, format) = match playback.subtitle {
                    None => {
                        return Err(Error::NotFound {
                            what: "an external subtitle in this playback".into(),
                        });
                    }
                    Some(Sidecar::File { path, format }) => (path, format),
                    Some(Sidecar::Made { state, format }) => (read_text(state).await?, format),
                };
                let (bytes, total, version) = read_subtitle(&path, offset, version).await?;
                self.services.metrics.subtitle_reads.inc();
                Ok(Reply::SubtitleChunk {
                    format,
                    bytes,
                    total,
                    version,
                })
            }
            Request::ReadSubtitleSegment { handle, n } => {
                let read = self
                    .subtitle_segment(connection, &session, &handle, n)
                    .await;
                // As for a segment read (`media_read`). Not ready yet is
                // not a failure: the client asks again.
                if let Err(error) = &read
                    && !matches!(error, Error::SubtitleNotReady { .. })
                {
                    tracing::warn!(
                        id = %self.played(&handle),
                        endpoint = %connection.remote_id().fmt_short(),
                        subtitle_segment = n,
                        %error,
                        "an HLS subtitle segment read failed"
                    );
                }
                if read.is_ok() {
                    self.services.metrics.subtitle_reads.inc();
                }
                read
            }
            Request::LookupDeviceSignIn { code } => {
                let code = self.sign_in_code(connection, &user, &code)?;
                match self.services.auth.sign_ins().lookup(&code) {
                    Some(waiting) => Ok(Reply::DeviceSignInRequest(DeviceSignInRequest {
                        device_name: waiting.device_name,
                        client_name: waiting.client_name,
                        platform: waiting.platform,
                        requested_at: waiting.requested_at,
                    })),
                    None => Err(self.wrong_sign_in_code(connection, &user)),
                }
            }
            Request::ApproveDeviceSignIn { code } => {
                recently_signed_in(&user)?;
                let code = self.sign_in_code(connection, &user, &code)?;
                let approver = connection.remote_id().to_string();
                let claim = match self.services.auth.sign_ins().claim(&code, Some(&approver)) {
                    Ok(claim) => claim,
                    Err(ClaimError::Unknown) => {
                        return Err(self.wrong_sign_in_code(connection, &user));
                    }
                    Err(ClaimError::SameDevice) => {
                        return Err(Error::InvalidInput {
                            message: "approve this device's sign-in on another device".into(),
                        });
                    }
                };
                match self.services.auth.approve_device(user.user, &claim).await {
                    Ok(Approval::SignedIn) => {
                        tracing::info!(
                            user = %user.username,
                            device = %claim.endpoint,
                            "device sign-in approved"
                        );
                        Ok(Reply::Done)
                    }
                    // The right code, run out while it was approved: not
                    // a guess, so not counted as one.
                    Ok(Approval::Expired) => Err(Error::NotFound {
                        what: "sign-in code".into(),
                    }),
                    Err(e) => Err(internal(e)),
                }
            }
            Request::DenyDeviceSignIn { code } => {
                let code = self.sign_in_code(connection, &user, &code)?;
                if self.services.auth.sign_ins().deny(&code) {
                    tracing::info!(user = %user.username, "device sign-in denied");
                    Ok(Reply::Done)
                } else {
                    Err(self.wrong_sign_in_code(connection, &user))
                }
            }
            Request::Hello(_)
            | Request::AuthMethods
            | Request::LoginPassword(_)
            | Request::Ping
            | Request::BeginDeviceSignIn
            | Request::AwaitDeviceSignIn { .. }
            | Request::BeginPasskeySignIn { .. }
            | Request::FinishPasskeySignIn(_)
            | Request::Unknown => {
                unreachable!("pre-auth and unknown requests are answered before the session check")
            }
        }
    }

    /// A sign-in attempt on `state`'s connection, if it may make one now:
    /// a device that is not signed in draws on the bucket all such devices
    /// share (PX-10); a signed-in one (adding a user) does not.
    fn stranger_attempt(&self, state: &ConnectionState) -> Result<(), Error> {
        if state.peer.known() {
            return Ok(());
        }
        self.services
            .auth
            .stranger_attempt()
            .map_err(|wait| Error::RateLimited {
                retry_after_ms: wait.as_millis().max(1) as u64,
            })
    }

    /// A password login from this connection's endpoint. The session is
    /// bound to that endpoint (P1-12 checks it on every request), and the
    /// endpoint is the source the failure limit counts.
    async fn login_password(
        &self,
        connection: &Connection,
        login: LoginPassword,
        hello: &Hello,
    ) -> Result<Reply, Error> {
        let endpoint_id = connection.remote_id().to_string();
        let binding = SessionBinding::Endpoint {
            endpoint_id: endpoint_id.clone(),
            device_name: hello.device_name.clone(),
            platform: hello.platform,
        };
        match self
            .services
            .auth
            .login_password(
                &login.username,
                &login.password,
                binding,
                Some(&endpoint_id),
            )
            .await
        {
            Ok(done) => {
                tracing::info!(endpoint = %endpoint_id, user = ?done.user, "logged in");
                self.connections.signed_in(&connection.remote_id());
                Ok(Reply::LoggedIn {
                    session: SessionId(done.token.0),
                })
            }
            Err(e) => Err(login_error(e)),
        }
    }

    /// A sign-in with an app passkey (P3-19) from this connection's
    /// endpoint: a session bound to it, as [`Self::login_password`] opens.
    async fn finish_passkey_sign_in(
        &self,
        connection: &Connection,
        signed: &FinishPasskeySignIn,
        hello: &Hello,
    ) -> Result<Reply, Error> {
        let endpoint_id = connection.remote_id().to_string();
        let user = self
            .services
            .app_passkeys
            .finish_sign_in(&self.services.auth, &endpoint_id, signed)
            .await
            .map_err(sign_in_error)?;
        let binding = SessionBinding::Endpoint {
            endpoint_id: endpoint_id.clone(),
            device_name: hello.device_name.clone(),
            platform: hello.platform,
        };
        let token = self
            .services
            .auth
            .open_session(user.id, &binding)
            .await
            .map_err(internal)?;
        tracing::info!(endpoint = %endpoint_id, user = %user.id, "logged in with an app passkey");
        self.connections.signed_in(&connection.remote_id());
        Ok(Reply::LoggedIn {
            session: SessionId(token.0),
        })
    }

    /// `session`'s user proved who they are again, `how`: it counts as
    /// signed in now ([`RECENT_SIGN_IN`]). Only this session.
    async fn confirmed(
        &self,
        session: &SessionId,
        user: &SessionUser,
        how: &str,
    ) -> Result<Reply, Error> {
        self.store()
            .confirm_session(&SessionToken(session.0.clone()).hash())
            .await
            .map_err(internal)?;
        tracing::info!(user = %user.username, how, "confirmed it is them, in an app");
        Ok(Reply::Confirmed)
    }

    /// The account of `user`, a session's.
    async fn account(&self, user: &SessionUser) -> Result<crate::store::users::UserRecord, Error> {
        self.store()
            .user_by_id(user.user)
            .await
            .map_err(internal)?
            .ok_or(Error::Unauthenticated)
    }

    /// `proof` that `user` may change or remove their PIN. The PIN is
    /// checked as at the lock screen and counted with it, but unlocks
    /// nothing (the lock stays as it is); the password as at sign-in.
    async fn check_pin_proof(
        &self,
        connection: &Connection,
        user: &SessionUser,
        proof: PinProof,
    ) -> Result<(), Error> {
        match proof {
            PinProof::Pin { pin } => {
                let endpoint = connection.remote_id().to_string();
                self.services
                    .auth
                    .check_pin(user.user, &endpoint, &pin)
                    .await
                    .map_err(pin_error)
            }
            PinProof::Password { password } => {
                self.confirm_password(connection, user, &password).await
            }
        }
    }

    /// `password` is `user`'s account password, checked and limited as a
    /// sign-in from this connection's endpoint is: a wrong one is a failed
    /// login for their username from here, and nothing else (no session
    /// is revoked, no PIN counted). A user with no password (passkeys or
    /// SSO only) gets `NoPassword` before anything is counted: their
    /// session already says who they are, so that tells no one anything.
    async fn confirm_password(
        &self,
        connection: &Connection,
        user: &SessionUser,
        password: &str,
    ) -> Result<(), Error> {
        let account = self
            .store()
            .user_by_id(user.user)
            .await
            .map_err(internal)?
            .ok_or(Error::Unauthenticated)?;
        if !account.has_password {
            return Err(Error::NoPassword);
        }
        let endpoint = connection.remote_id().to_string();
        let checked = self
            .services
            .auth
            .check_password(&account.username, password, Some(&endpoint))
            .await
            .map_err(login_error)?;
        // The username is the session user's own; a rename between the two
        // lookups is not a way to pass with another user's password.
        if checked.id != user.user {
            return Err(Error::InvalidCredentials);
        }
        Ok(())
    }

    /// `typed` as a device sign-in code, if `user` on this endpoint may
    /// try one now. A malformed code is a wrong one.
    fn sign_in_code(
        &self,
        connection: &Connection,
        user: &SessionUser,
        typed: &str,
    ) -> Result<String, Error> {
        let (what, from) = sign_in_code_keys(connection, user);
        if let Some(retry_after) = self.services.auth.attempt_limited(&what, Some(&from)) {
            return Err(Error::RateLimited {
                retry_after_ms: retry_after.as_millis() as u64,
            });
        }
        device_sign_in_code(typed).ok_or_else(|| self.wrong_sign_in_code(connection, user))
    }

    /// Count a code that is not pending against `user` and this endpoint:
    /// codes are short, so guessing them is limited as passwords are.
    fn wrong_sign_in_code(&self, connection: &Connection, user: &SessionUser) -> Error {
        let (what, from) = sign_in_code_keys(connection, user);
        self.services.auth.attempt_failed(&what, Some(&from));
        Error::NotFound {
            what: "sign-in code".into(),
        }
    }

    /// A device's last connection closed: end its playbacks, and their
    /// encodes, unless it is back within the grace. A killed app's encode
    /// would otherwise hold a slot until it idles out.
    fn end_playbacks_unless_back(&self, endpoint: EndpointId) {
        let connections = self.connections.clone();
        let playbacks = self.services.playbacks.clone();
        tokio::spawn(async move {
            tokio::time::sleep(playbacks.disconnect_grace()).await;
            if connections.has_endpoint(&endpoint) {
                return;
            }
            let ended = playbacks.end_device(&endpoint.to_string());
            if ended > 0 {
                tracing::info!(%endpoint, ended, "a gone device's playbacks ended");
            }
        });
    }

    fn store(&self) -> &crate::store::Store {
        self.services.auth.store()
    }

    /// `stream` of the file at `path` as `to` text: reading it starts now
    /// (or is joined, or is cached) and the client fetches it when done.
    async fn read_as_text(
        &self,
        path: &Path,
        stream: &SubtitleStream,
        to: SubtitleFormat,
    ) -> Sidecar {
        let state = self
            .services
            .subtitles
            .text(TextSource { path, stream, to })
            .await;
        Sidecar::Made { state, format: to }
    }

    /// What `plan` needs to play as `handle`: the subtitle the client
    /// fetches beside it, the size of what it reads (0 while a conversion
    /// writes it) with the file read for direct play, and the conversion,
    /// started at `start`. `replaces` is the playback as it was, when this
    /// is a subtitle change: direct play goes on reading the file it began
    /// with, or none.
    async fn prepare(
        &self,
        handle: &PlaybackHandle,
        id: &MediaId,
        (path, descriptor): (&Path, &Arc<MediaDescriptor>),
        plan: &PlaybackPlan,
        start: Option<Duration>,
        replaces: Option<&Playback>,
    ) -> Result<
        (
            Option<Sidecar>,
            (u64, Option<FileId>),
            Option<Arc<Conversion>>,
        ),
        Error,
    > {
        Ok(match plan {
            PlaybackPlan::Direct { subtitle, .. } => {
                let subtitle = match subtitle {
                    Some(planned) => {
                        let stream = &descriptor.subtitles[planned.index as usize];
                        // Planned as another format than its own: read
                        // as text (P2-07).
                        if planned.format != codecs::subtitle_format(&stream.codec) {
                            Some(self.read_as_text(path, stream, planned.format).await)
                        } else if let SubtitleSource::External { path } = &stream.source {
                            Some(Sidecar::File {
                                path: path.clone(),
                                format: planned.format,
                            })
                        } else {
                            None
                        }
                    }
                    None => None,
                };
                let metadata = tokio::fs::metadata(path).await.map_err(|e| {
                    tracing::warn!(%id, error = %e, "cannot stat a media file");
                    Error::Unavailable {
                        message: "the media file could not be read".into(),
                    }
                })?;
                let file = FileId::of(&metadata);
                if replaces
                    .and_then(|p| p.file)
                    .is_some_and(|began| began != file)
                {
                    tracing::warn!(%id, "a media file was replaced during its playback");
                    return Err(Error::Unavailable {
                        message: "the media file changed during playback".into(),
                    });
                }
                (subtitle, (metadata.len(), Some(file)), None)
            }
            PlaybackPlan::Converted {
                delivery,
                video,
                subtitle,
                ..
            } => {
                // Text the client fetches beside the stream: read from
                // pictures; or, for HLS, any text, as the WebVTT of
                // its subtitle rendition (P2-07).
                let text = match (delivery, subtitle) {
                    (
                        Delivery::Hls,
                        Some(ConvertedSubtitle {
                            index,
                            action:
                                SubtitleAction::Copy { .. }
                                | SubtitleAction::Convert { .. }
                                | SubtitleAction::Ocr { .. },
                        }),
                    ) => Some(
                        self.read_as_text(
                            path,
                            &descriptor.subtitles[*index as usize],
                            SubtitleFormat::WebVtt,
                        )
                        .await,
                    ),
                    (
                        _,
                        Some(ConvertedSubtitle {
                            index,
                            action: SubtitleAction::Ocr { to, .. },
                        }),
                    ) => Some(
                        self.read_as_text(path, &descriptor.subtitles[*index as usize], *to)
                            .await,
                    ),
                    _ => None,
                };
                // HLS of a copied video is cut at its keyframes.
                let keyframes = match (delivery, video) {
                    (Delivery::Hls, Some(ConvertedVideo::Copy { .. })) => {
                        Some(self.services.prober.keyframes(path).await.map_err(|e| {
                            tracing::warn!(%id, error = %e, "cannot list keyframes");
                            Error::PlaybackFailed {
                                reason: "the file's keyframes could not be read".into(),
                            }
                        })?)
                    }
                    _ => None,
                };
                // A new conversion's first epoch is past the old one's, so
                // reads of the old one are told to start over. The old one
                // runs until the new one has started, so the new one takes
                // no slot of its own: it shares the old one's.
                let replaced = replaces
                    .and_then(|p| p.conversion.as_ref())
                    .map(|c| c.job());
                let first_epoch = match replaces {
                    Some(_) => replaced.as_ref().map_or(0, |job| job.epoch) + 1,
                    None => 0,
                };
                let transcoding = &self.services.transcoding;
                let conversion = Conversion::start(
                    self.services.jobs.clone(),
                    ConversionSource {
                        handle: &handle.0,
                        path,
                        descriptor: descriptor.clone(),
                        plan,
                        encoders: transcoding.encoders(),
                        vaapi_device: &transcoding.settings().vaapi_device,
                        keyframes,
                        first_epoch,
                        slot: replaced.and_then(|job| job.slot()),
                    },
                    start,
                )
                .map_err(|e| match e {
                    JobError::Busy => Error::Busy,
                    JobError::Prepare(reason) => {
                        tracing::warn!(%id, reason, "cannot start a conversion");
                        Error::PlaybackFailed { reason }
                    }
                })?;
                (text, (0, None), Some(conversion))
            }
            PlaybackPlan::Unsupported { reasons } => {
                return Err(Error::CannotPlay {
                    reasons: reasons.clone(),
                });
            }
            other => {
                return Err(Error::CannotPlay {
                    reasons: vec![format!("{other:?} is not served yet")],
                });
            }
        })
    }

    /// `id`'s media file and Spoolframe's description of it.
    async fn describe(&self, id: &MediaId) -> Result<(PathBuf, Arc<MediaDescriptor>), Error> {
        let path = self
            .services
            .catalog
            .media_path(id)
            .await
            .map_err(catalog_error)?;
        let descriptor = self.services.prober.describe(&path).await.map_err(|e| {
            tracing::warn!(%id, error = %e, "cannot describe a media file");
            Error::Unavailable {
                message: "the media file could not be read".into(),
            }
        })?;
        Ok((path, descriptor))
    }

    /// The tracks to plan with: the client's, or, when it sent none, the
    /// ones the principal's preferences and override choose (P1-20).
    async fn selection(
        &self,
        principal: Principal,
        id: &MediaId,
        descriptor: &MediaDescriptor,
        selection: Option<Selection>,
    ) -> Result<(Selection, Option<ChosenTracks>), Error> {
        if let Some(selection) = selection {
            return Ok((selection, None));
        }
        let store = self.store();
        let preferences = store.track_preferences(principal).await.map_err(internal)?;
        let scope = self.override_scope(id).await?;
        let override_ = store
            .track_override(principal, &scope)
            .await
            .map_err(internal)?;
        let chosen = tracks::choose(&descriptor.tracks(), &preferences, override_.as_ref());
        Ok((chosen.selection, Some(chosen)))
    }

    /// What a track override for `id` applies to: its show for an episode,
    /// so a pick carries to the rest of the series; else `id` itself.
    async fn override_scope(&self, id: &MediaId) -> Result<String, Error> {
        if id.kind() != MediaKind::Episode {
            return Ok(id.to_string());
        }
        let found = self
            .services
            .catalog
            .item(id)
            .await
            .map_err(catalog_error)?;
        Ok(match found.item {
            Item::Episode(e) => e.show_id.to_string(),
            _ => id.to_string(),
        })
    }

    /// Open the server→client events stream for `connection`.
    async fn open_events(&self, connection: &Connection) {
        let opened = async {
            let mut stream = connection.open_uni().await.map_err(std::io::Error::other)?;
            stream.write_u8(uni::EVENTS).await?;
            Ok::<_, std::io::Error>(stream)
        };
        match opened.await {
            Ok(stream) => {
                self.events
                    .lock()
                    .await
                    .insert(connection.stable_id(), stream);
            }
            Err(e) => tracing::debug!(error = %e, "cannot open the events stream"),
        }
    }
}

impl ProtocolHandler for ClientProtocol {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let id = connection.stable_id();
        let auth = ConnectionAuth::Anonymous {
            endpoint: connection.remote_id(),
        };
        // A device with a live session gets the normal limits; any other
        // is a stranger, held only while there is room for one (PX-10).
        let known = match self
            .store()
            .endpoint_signed_in(&connection.remote_id().to_string())
            .await
        {
            Ok(known) => known,
            Err(e) => {
                tracing::warn!(error = %e, "cannot look the device's sessions up");
                false
            }
        };
        let Some(peer) = self.connections.admit(&connection, known) else {
            tracing::info!(
                ?auth,
                "refusing a connection: too many devices that are not signed in"
            );
            connection.close(
                VarInt::from_u32(close::BUSY),
                b"too many devices that are not signed in; try again later",
            );
            // Stay until the close is delivered, or the peer never sees why.
            let _ = tokio::time::timeout(SHUTDOWN_NOTICE, connection.closed()).await;
            return Ok(());
        };
        tracing::info!(?auth, known, "client connected");
        let state = Arc::new(ConnectionState {
            peer,
            hello: OnceLock::new(),
            locks: std::sync::Mutex::default(),
        });

        let reason = loop {
            match connection.accept_bi().await {
                Ok((send, recv)) => {
                    let this = self.clone();
                    let connection = connection.clone();
                    let state = state.clone();
                    tokio::spawn(async move { this.stream(connection, state, send, recv).await });
                }
                Err(reason) => break reason,
            }
        };

        self.events.lock().await.remove(&id);
        self.connections.remove(id);
        self.services.connected.closed(id);
        tracing::info!(?auth, %reason, "client disconnected");
        self.end_playbacks_unless_back(connection.remote_id());
        Ok(())
    }

    /// Tell every client the server is going away, before the router
    /// closes the connections.
    async fn shutdown(&self) {
        self.services.previews.stop();
        let streams: Vec<_> = self.events.lock().await.drain().map(|(_, s)| s).collect();
        for mut stream in streams {
            let notice = async {
                write_frame(&mut stream, &Event::ShuttingDown).await?;
                stream.finish().map_err(std::io::Error::other)?;
                // Wait for the client to acknowledge it; closing the
                // connection first could drop the frame.
                let _ = stream.stopped().await;
                Ok::<_, codec::CodecError>(())
            };
            match tokio::time::timeout(SHUTDOWN_NOTICE, notice).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::debug!(error = %e, "cannot send ShuttingDown"),
                Err(_) => tracing::debug!("a client did not take ShuttingDown in time"),
            }
        }
    }
}

/// Wait, for at most [`SIGN_OUT_DRAIN`], for the peer to acknowledge all
/// of a finished stream: until then the stream counts as being answered,
/// so a signed-out device's connection is not closed under its reply.
async fn delivered(send: &SendStream) {
    let _ = tokio::time::timeout(SIGN_OUT_DRAIN, send.stopped()).await;
}

/// The most items one page or search returns, whatever the client asks.
const MAX_PAGE: u32 = 500;

/// A user's Plex link and their last sync run, as a client sees them: what
/// My profile shows (`Link::shown`), and never the token.
fn plex_link(
    link: Option<Link>,
    (last_run, syncing): (Option<PlexSyncRun>, bool),
) -> Option<PlexLink> {
    let shown = link?.shown();
    Some(PlexLink {
        username: shown.username,
        email: shown.email,
        by_config: shown.by_config,
        linked_at: shown.linked_at,
        last_sync: last_run.map(|run| PlexSyncSummary {
            at: run.at,
            libraries: run.libraries,
            pulled: run.pulled,
            pushed: run.pushed,
            skipped: run.skipped,
            failed: run.failed,
        }),
        syncing,
    })
}

/// A password that did not pass, for the client: the same answers at
/// sign-in and wherever else it is asked for.
fn login_error(e: LoginError) -> Error {
    match e {
        LoginError::InvalidCredentials => Error::InvalidCredentials,
        LoginError::RateLimited { retry_after } => Error::RateLimited {
            retry_after_ms: u64::try_from(retry_after.as_millis()).unwrap_or(u64::MAX),
        },
        LoginError::Internal(e) => {
            tracing::error!(error = %e, "login failed");
            Error::Internal
        }
    }
}

/// P3-03's rule for what adds or takes away a way to sign in: `user`'s
/// session proved who they are within [`RECENT_SIGN_IN`].
fn recently_signed_in(user: &SessionUser) -> Result<(), Error> {
    let age = crate::store::now() - user.authenticated_at;
    if age > RECENT_SIGN_IN.as_secs() as i64 {
        return Err(Error::RecentSignInRequired);
    }
    Ok(())
}

/// A stored passkey as the apps see it. Only an app passkey signs in in
/// an app; a web UI's is listed so its user sees every way they sign in.
fn passkey_info(p: PasskeyRecord) -> PasskeyInfo {
    let app = p.rp_id == APP_RP_ID;
    PasskeyInfo {
        id: p.credential_id,
        name: p.name,
        created_at: p.created_at,
        last_used_at: p.last_used_at,
        rp_id: p.rp_id,
        app,
        usable_here: app,
    }
}

/// A passkey not renamed or removed, for the client.
fn passkey_error(e: PasskeyError, id: &str) -> Error {
    match e {
        PasskeyError::NoSuchPasskey => Error::NotFound {
            what: format!("passkey {id}"),
        },
        PasskeyError::InvalidName | PasskeyError::Exists => Error::InvalidInput {
            message: e.to_string(),
        },
        PasskeyError::LastSignInMethod => Error::LastSignInMethod,
        PasskeyError::Store(e) => internal(e),
    }
}

/// An app passkey that signed no one in, for the client: as a wrong
/// password is answered. The log has the reason.
fn sign_in_error(e: SignInError) -> Error {
    match e {
        SignInError::Refused => Error::InvalidCredentials,
        SignInError::RateLimited { retry_after } => Error::RateLimited {
            retry_after_ms: u64::try_from(retry_after.as_millis()).unwrap_or(u64::MAX),
        },
        SignInError::Unavailable(e) | SignInError::Internal(e) => internal(e),
    }
}

/// An app passkey that was not kept, for the client.
fn ceremony_error(e: CeremonyError) -> Error {
    match e {
        CeremonyError::NoCeremony | CeremonyError::Refused(_) => {
            tracing::warn!(reason = %e, "app passkey refused");
            Error::InvalidCredentials
        }
        CeremonyError::Passkey(e @ (PasskeyError::InvalidName | PasskeyError::Exists)) => {
            Error::InvalidInput {
                message: e.to_string(),
            }
        }
        CeremonyError::Passkey(e) => internal(e),
        CeremonyError::Unavailable(e) | CeremonyError::Internal(e) => internal(e),
    }
}

/// A PIN that did not pass, for the client: the same answers wherever it
/// was entered.
fn pin_error(e: UnlockError) -> Error {
    match e {
        UnlockError::Wrong => Error::InvalidCredentials,
        UnlockError::RateLimited { retry_after } => Error::RateLimited {
            retry_after_ms: retry_after.as_millis() as u64,
        },
        UnlockError::Revoked => Error::Unauthenticated,
        UnlockError::Internal(e) => internal(e),
    }
}

fn internal(e: impl std::fmt::Display) -> Error {
    tracing::error!(error = %e, "request failed");
    Error::Internal
}

fn playback_refusal(refusal: Refusal) -> Error {
    match refusal {
        Refusal::Expired => Error::PlaybackExpired,
        Refusal::Stopped => Error::PlaybackStopped,
        Refusal::NotYours => Error::Unauthenticated,
    }
}

/// How long a read past what an encode has written waits for more.
const READ_WAIT: Duration = spoolframe_protocol::policy::wait::READ;

/// A range of a converted playback's progressive output (P2-04): the bytes
/// written so far, waiting a while for more when the read is past them.
async fn converted_range(
    conversion: &Conversion,
    read: &MediaRead,
    keep_alive: &(dyn Fn() + Sync),
) -> Result<(tokio::fs::File, MediaReadReply), Error> {
    if let Some(n) = read.segment {
        return segment_range(conversion, n, read, keep_alive).await;
    }
    let job = conversion.job();
    if read.epoch != job.epoch {
        return Err(Error::PlaybackRestarted { epoch: job.epoch });
    }
    // Asking for bytes ahead is reading: it may let a paused encode go on.
    job.read_to(read.offset);
    let offset = read.offset;
    // Still nothing after the wait: say so, and the client asks again.
    let now = wait_for(&job, keep_alive, |p| p.bytes > offset || p.done.is_some()).await;
    if let Some(Err(reason)) = &now.done {
        return Err(Error::PlaybackFailed {
            reason: reason.clone(),
        });
    }
    let size = now.bytes;
    let sent = read
        .length
        .min(MAX_READ)
        .min(size.saturating_sub(read.offset));
    job.read_to(read.offset + sent);
    let file = job.dir().join("out.mkv");
    let unreadable = |e: std::io::Error| {
        tracing::warn!(path = %file.display(), error = %e, "cannot read an encode's output");
        Error::PlaybackFailed {
            reason: "the conversion's output could not be read".into(),
        }
    };
    let handle = open_at(&file, read.offset, sent)
        .await
        .map_err(unreadable)?;
    Ok((
        handle,
        MediaReadReply {
            sent,
            size,
            complete: matches!(now.done, Some(Ok(()))),
            epoch: job.epoch,
        },
    ))
}

/// Wait up to [`READ_WAIT`] for `job` to get where `ready` says, keeping
/// the playback alive meanwhile; then how far it has got.
async fn wait_for(
    job: &EncodeJob,
    keep_alive: &(dyn Fn() + Sync),
    ready: impl Fn(&Progress) -> bool,
) -> Progress {
    let mut progress = job.progress();
    let deadline = tokio::time::Instant::now() + READ_WAIT;
    loop {
        keep_alive();
        let slice = tokio::time::timeout(Duration::from_secs(1), progress.wait_for(&ready)).await;
        match slice {
            Ok(Ok(p)) => return p.clone(),
            Ok(Err(_)) => break,
            Err(_) if tokio::time::Instant::now() >= deadline => break,
            Err(_) => {}
        }
    }
    job.progress().borrow().clone()
}

/// `file` at `offset`, to send `sent` bytes from. With nothing to send the
/// file may not exist yet; an empty stand-in does.
async fn open_at(file: &Path, offset: u64, sent: u64) -> std::io::Result<tokio::fs::File> {
    if sent == 0 {
        return tokio::fs::File::open("/dev/null").await;
    }
    let mut handle = tokio::fs::File::open(file).await?;
    handle.seek(std::io::SeekFrom::Start(offset)).await?;
    Ok(handle)
}

/// A range of HLS segment `n` (P2-05), 0 being the init segment. Segments
/// are addressed by number, not by epoch: a read far from what is written
/// restarts the encode there and waits for it.
async fn segment_range(
    conversion: &Conversion,
    n: u32,
    read: &MediaRead,
    keep_alive: &(dyn Fn() + Sync),
) -> Result<(tokio::fs::File, MediaReadReply), Error> {
    let segments = conversion.segments().ok_or_else(|| Error::NotFound {
        what: "an HLS conversion in this playback".into(),
    })?;
    if n > segments.count() {
        return Err(Error::NotFound {
            what: format!("segment {n}"),
        });
    }
    let job = if n == 0 {
        conversion.job()
    } else {
        conversion.job_for_segment(n).map_err(|e| match e {
            JobError::Busy => Error::Busy,
            JobError::Prepare(reason) => Error::PlaybackFailed { reason },
        })?
    };
    // A segment is final once ffmpeg has finished it or the encode has
    // ended; the init segment once the first media segment is finished.
    let wanted = if n == 0 { job.first_segment } else { n };
    // The reader is at the end of the segment it waits for, as far as the
    // throttle is concerned, or the encode pauses just short of finishing
    // it. A read of the init segment does not move a reader back.
    let end = segments.start(wanted + 1).unwrap_or(segments.total_ms);
    if n == 0 {
        job.reader_at_least(wanted, end);
    } else {
        job.reader_at(wanted, end);
    }
    let now = wait_for(&job, keep_alive, |p| {
        p.last_segment.is_some_and(|l| l >= wanted) || p.done.is_some()
    })
    .await;
    if let Some(Err(reason)) = &now.done {
        return Err(Error::PlaybackFailed {
            reason: reason.clone(),
        });
    }
    // Not a segment removed meanwhile to make room: asked for again, the
    // encode restarts there.
    let finished = (now.last_segment.is_some_and(|l| l >= wanted) || now.done.is_some())
        && (n == 0 || n >= job.first_kept());
    let name = if n == 0 {
        "init.mp4".to_owned()
    } else {
        format!("seg{n}.m4s")
    };
    let file = job.dir().join(name);
    let unreadable = |e: std::io::Error| {
        tracing::warn!(path = %file.display(), error = %e, "cannot read a segment");
        Error::PlaybackFailed {
            reason: "a segment of the conversion could not be read".into(),
        }
    };
    // Opened first and measured through the handle: a segment can be
    // removed after the check above (for room, or with all the job's
    // files when the job fails), and an open file stays readable.
    let opened = if finished {
        match tokio::fs::File::open(&file).await {
            Ok(handle) => {
                let size = handle.metadata().await.map_err(unreadable)?.len();
                Some((handle, size))
            }
            // Gone since the check: not ready, as a removed segment is,
            // unless the job has failed by now.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(Err(reason)) = &job.progress().borrow().done {
                    return Err(Error::PlaybackFailed {
                        reason: reason.clone(),
                    });
                }
                None
            }
            Err(e) => return Err(unreadable(e)),
        }
    } else {
        None
    };
    let complete = opened.is_some();
    let size = opened.as_ref().map_or(0, |(_, size)| *size);
    let sent = read
        .length
        .min(MAX_READ)
        .min(size.saturating_sub(read.offset));
    let handle = match opened {
        Some((mut handle, _)) if sent > 0 => {
            handle
                .seek(std::io::SeekFrom::Start(read.offset))
                .await
                .map_err(unreadable)?;
            handle
        }
        // Nothing to send: an empty stand-in.
        _ => open_at(&file, read.offset, 0).await.map_err(unreadable)?,
    };
    Ok((
        handle,
        MediaReadReply {
            sent,
            size,
            complete,
            epoch: job.epoch,
        },
    ))
}

/// One chunk of a subtitle file (P2-11): at most [`SUBTITLE_CHUNK`] bytes
/// from `offset`, the file's length and its version. At or past the end
/// that is no bytes, so a client's loop ends. Only the chunk is read,
/// however large the file.
///
/// With the `version` of an earlier chunk, a file that is not that one
/// any more is `SubtitleChanged`, and none of it is read.
async fn read_subtitle(
    path: &Path,
    offset: u64,
    version: Option<u64>,
) -> Result<(Vec<u8>, u64, u64), Error> {
    let unreadable = |e: std::io::Error| {
        tracing::warn!(path = %path.display(), error = %e, "cannot read a subtitle");
        Error::Unavailable {
            message: "the subtitle file could not be read".into(),
        }
    };
    let mut file = crate::files::open_regular(path).await.map_err(unreadable)?;
    // Of the open file, not of the path: the version is of the file the
    // bytes below come from, whatever the path names by then. It is taken
    // before the bytes, so a write during the read gives the next chunk's
    // request a version that no longer holds.
    let metadata = file.metadata().await.map_err(unreadable)?;
    let now = subtitle_version(&metadata);
    if version.is_some_and(|read| read != now) {
        return Err(Error::SubtitleChanged);
    }
    let total = metadata.len();
    let mut bytes = Vec::new();
    if offset < total {
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(unreadable)?;
        let mut chunk = file.take(SUBTITLE_CHUNK);
        chunk.read_to_end(&mut bytes).await.map_err(unreadable)?;
    }
    Ok((bytes, total, now))
}

/// A value that names a subtitle file as it is now: another one once the
/// file is written again (its length or its modification time changes) or
/// replaced (another inode). A hash, so that a client learns nothing of
/// the server's disk from it.
fn subtitle_version(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let mut hash = blake3::Hasher::new();
    for part in [
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime() as u64,
        metadata.mtime_nsec() as u64,
    ] {
        hash.update(&part.to_le_bytes());
    }
    let mut first = [0; 8];
    first.copy_from_slice(&hash.finalize().as_bytes()[..8]);
    u64::from_le_bytes(first)
}

/// The text a reading made: waits for it a while, then tells the client to
/// ask again.
async fn read_text(mut state: watch::Receiver<TextState>) -> Result<PathBuf, Error> {
    let _ = tokio::time::timeout(READ_WAIT, state.wait_for(TextState::finished)).await;
    let now = state.borrow().clone();
    match now {
        TextState::Ready(path) => Ok(path),
        // The reason is in the log.
        TextState::Failed(_) => Err(Error::Unavailable {
            message: "the subtitle could not be read as text".into(),
        }),
        TextState::Waiting => Err(Error::SubtitleNotReady { done_percent: 0 }),
        TextState::Reading { done_percent } => Err(Error::SubtitleNotReady { done_percent }),
    }
}

/// A catalog failure as the client sees it. Backend details (the PMS URL,
/// file paths) go to the log, not to the client.
fn catalog_error(e: CatalogError) -> Error {
    match e {
        // A library removed or never added is, to a client, not there.
        CatalogError::NotFound(what)
        | CatalogError::ForeignId(what)
        | CatalogError::NotServed(what) => Error::NotFound { what },
        CatalogError::NotPlayable(id) => Error::NotFound {
            what: format!("a media file for {id}"),
        },
        CatalogError::MediaFileUnavailable(_) => {
            tracing::warn!(error = %e, "media file unavailable");
            Error::Unavailable {
                message: "the media file is not readable on the server".into(),
            }
        }
        CatalogError::Unreachable { .. } | CatalogError::Backend { .. } => {
            tracing::warn!(error = %e, "catalog unavailable");
            Error::Unavailable {
                message: "the library is unavailable".into(),
            }
        }
        CatalogError::NotSetUp => Error::NotSetUp,
    }
}

fn watch_error(e: WatchError) -> Error {
    match e {
        WatchError::NotTracked(id) => Error::NotFound {
            what: format!("watch state for {id}"),
        },
        WatchError::Catalog(e) => catalog_error(e),
        WatchError::Store(e) => internal(e),
    }
}

/// The auth methods this server offers.
/// The failure limit's keys for device sign-in codes tried by `user` on
/// this connection's endpoint: apart from passwords and PINs.
fn sign_in_code_keys(connection: &Connection, user: &SessionUser) -> (String, String) {
    (
        format!("devcode:{}", user.user),
        format!("devcode@{}", connection.remote_id()),
    )
}

pub(crate) fn auth_methods() -> Vec<AuthMethod> {
    vec![AuthMethod::Password, AuthMethod::DeviceApproval]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plex_accounts::AccountLink;

    #[test]
    fn the_shared_probe_allowance_matches_the_media_probers_default() {
        assert_eq!(
            spoolframe_protocol::policy::wait::PROBE,
            spoolframe_media::Prober::DEFAULT_TIMEOUT
        );
    }

    fn run() -> PlexSyncRun {
        PlexSyncRun {
            at: 1_790_000_100,
            libraries: 2,
            pulled: 3,
            pushed: 4,
            skipped: 5,
            failed: 1,
            notes: vec!["Cabin movies skipped".into()],
        }
    }

    #[test]
    fn a_linked_account_is_shown_with_its_last_sync_and_never_its_token() {
        let link = Link::Account(AccountLink {
            token: "plex-token".into(),
            username: Some("alice_plex".into()),
            email: Some("alice@example.com".into()),
            linked_at: 1_790_000_000,
        });
        let shown = plex_link(Some(link), (Some(run()), true)).unwrap();
        assert_eq!(
            shown,
            PlexLink {
                username: Some("alice_plex".into()),
                email: Some("alice@example.com".into()),
                by_config: false,
                linked_at: Some(1_790_000_000),
                last_sync: Some(PlexSyncSummary {
                    at: 1_790_000_100,
                    libraries: 2,
                    pulled: 3,
                    pushed: 4,
                    skipped: 5,
                    failed: 1,
                }),
                syncing: true,
            }
        );
        let wire =
            spoolframe_protocol::codec::encode(&Reply::PlexLink { link: Some(shown) }).unwrap();
        assert!(!wire.windows(10).any(|w| w == b"plex-token"));
    }

    #[test]
    fn the_config_s_owner_link_has_no_account_details() {
        let shown = plex_link(Some(Link::Owner), (None, false)).unwrap();
        assert!(shown.by_config);
        assert_eq!(
            (shown.username, shown.linked_at, shown.last_sync),
            (None, None, None)
        );
    }

    #[test]
    fn no_link_is_none_whatever_the_sync_says() {
        assert_eq!(plex_link(None, (Some(run()), false)), None);
    }
}
