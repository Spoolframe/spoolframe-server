//! `spoolframe/client/1` on one connection (ARCHITECTURE §20, §33).
//!
//! Each bidirectional stream the client opens is one RPC. The first
//! request must be `Hello`; after it the server opens the events stream.
//! Only the pre-auth requests are served without a session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use iroh::endpoint::{Connection, RecvStream, SendStream, VarInt};
use iroh::protocol::{AcceptError, ProtocolHandler};
use spoolframe_catalog::{CatalogError, Page};
use spoolframe_media::{MediaDescriptor, SubtitleSource, SubtitleStream, codecs};
use spoolframe_protocol::codec::{self, read_frame_bytes, write_frame};
use spoolframe_protocol::device_sign_in::normalize as device_sign_in_code;
use spoolframe_protocol::streams::{bi, reset, uni};
use spoolframe_protocol::{
    ArtworkReply, ArtworkRequest, ArtworkResponse, ChosenTracks, ConvertedSubtitle, MAX_READ,
    MediaRead, MediaReadReply, MediaReadResponse, PlaybackHandle, PlaybackPlan, SUBTITLE_CHUNK,
    Selection, SubtitleAction,
};
use spoolframe_protocol::{
    AuthMethod, DeviceInfo, DeviceSignInRequest, Error, Event, Hello, LoginPassword, Reply,
    Request, RequestEnvelope, Response, ServerInfo, SessionId, SessionInfo, UserSummary,
};
use spoolframe_types::{Item, MediaId, MediaKind, SubtitleFormat};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex, watch};

use crate::artwork::{Artwork, ArtworkError};
use crate::auth::{self, LoginError, SessionToken, UnlockError};
use crate::device_sign_in::{self, ClaimError, Outcome, Waiting};
use crate::encode::job::{EncodeJob, JobError, Progress};
use crate::encode::{Conversion, ConversionSource, hls};
use crate::network::{ConnectionAuth, Connections, Services};
use crate::planner;
use crate::playback::{Changed, Opened, Playback, Playbacks, Refusal, Sidecar};
use crate::principal::Principal;
use crate::store::pins::PinError;
use crate::store::sessions::{SessionBinding, SessionUser};
use crate::subtitles::{TextSource, TextState};
use crate::tracks::{self, TrackOverride};
use crate::watch::WatchError;
use spoolframe_protocol::ConvertedVideo;
use spoolframe_types::Delivery;

/// How long shutdown waits to deliver `ShuttingDown` to one client.
const SHUTDOWN_NOTICE: Duration = Duration::from_millis(500);

/// The client protocol handler shared by every connection.
#[derive(Debug, Clone)]
pub(crate) struct ClientProtocol {
    connections: Connections,
    services: Services,
    info: Arc<ServerInfo>,
    /// The events stream of each connection that said Hello.
    events: Arc<Mutex<HashMap<usize, SendStream>>>,
}

/// What one connection has said so far.
#[derive(Debug, Default)]
struct ConnectionState {
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
    /// The session the last request came from. Using another one locks it
    /// again: a switch to another profile and back asks for the PIN, and
    /// the server keeps to that whatever the client does.
    active: Option<String>,
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

    /// `session` makes a request: refused while it is locked; otherwise it
    /// is the active one from now, and the one before is locked again.
    fn use_session(&mut self, session: &SessionId) -> Result<(), Error> {
        if self.locked(session) {
            return Err(Error::ProfileLocked);
        }
        self.activate(session);
        Ok(())
    }

    fn activate(&mut self, session: &SessionId) {
        if self.active.as_deref() != Some(session.0.as_str()) {
            if let Some(previous) = self.active.take() {
                self.unlocked.remove(&previous);
            }
            self.active = Some(session.0.clone());
        }
    }

    fn unlock(&mut self, session: &SessionId) {
        self.activate(session);
        self.unlocked.insert(session.0.clone());
    }
}

impl ClientProtocol {
    pub(crate) fn new(connections: Connections, info: ServerInfo, services: Services) -> Self {
        Self {
            connections,
            services,
            info: Arc::new(info),
            events: Arc::default(),
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
        let Ok(kind) = recv.read_u8().await else {
            return;
        };
        if kind == bi::ARTWORK {
            self.artwork(&connection, &state, send, recv).await;
            return;
        }
        if kind == bi::MEDIA {
            self.media_read(&connection, &state, send, recv).await;
            return;
        }
        if kind != bi::RPC {
            // A kind from a newer client. The stream is refused; the
            // connection carries on.
            tracing::debug!(kind, "refusing a stream of unknown kind");
            let code = VarInt::from_u32(reset::UNKNOWN_STREAM_KIND);
            let _ = send.reset(code);
            let _ = recv.stop(code);
            return;
        }
        let request = read_frame_bytes(&mut recv).await;
        let response = match &request {
            Ok(bytes) => self.answer(&connection, &state, bytes).await,
            Err(e) => Response::Err(Error::Malformed {
                message: e.to_string(),
            }),
        };
        let sent = match write_frame(&mut send, &response).await {
            // Nothing of it was written. The client gets the reason, not a
            // stream that ends with no response.
            Err(codec::CodecError::TooLarge { len }) => {
                let request = request.ok().and_then(|bytes| codec::request_type(&bytes));
                tracing::warn!(request, len, "a reply is over the frame limit");
                let refusal = Response::Err(Error::ReplyTooLarge { bytes: len });
                write_frame(&mut send, &refusal).await
            }
            sent => sent,
        };
        if let Err(e) = sent {
            tracing::debug!(error = %e, "cannot send a response");
            return;
        }
        let _ = send.finish();
    }

    async fn answer(
        &self,
        connection: &Connection,
        state: &ConnectionState,
        bytes: &[u8],
    ) -> Response {
        let envelope: RequestEnvelope = match codec::decode(bytes) {
            Ok(envelope) => envelope,
            Err(e) => {
                return Response::Err(Error::Malformed {
                    message: e.to_string(),
                });
            }
        };
        let reply = match envelope.request {
            Request::Hello(hello) => {
                tracing::debug!(?hello, "hello");
                if state.hello.set(hello).is_ok() {
                    self.open_events(connection).await;
                }
                Ok(Reply::ServerInfo((*self.info).clone()))
            }
            _ if state.hello.get().is_none() => Err(Error::HandshakeRequired),
            Request::Unknown => Err(Error::Unsupported {
                request: codec::request_type(bytes).unwrap_or_default(),
            }),
            Request::AuthMethods => Ok(Reply::AuthMethods {
                methods: self.info.auth_methods.clone(),
            }),
            Request::Ping => Ok(Reply::Pong),
            Request::LoginPassword(login) => {
                let device_name = state.hello.get().and_then(|h| h.device_name.clone());
                self.login_password(connection, login, device_name).await
            }
            Request::BeginDeviceSignIn => {
                let hello = state.hello.get().expect("checked above");
                let started = self.services.auth.sign_ins().begin(Waiting {
                    endpoint: connection.remote_id().to_string(),
                    device_name: hello.device_name.clone(),
                    client_name: hello.client_name.clone(),
                    requested_at: crate::store::now(),
                });
                match started {
                    Some(started) => Ok(Reply::DeviceSignInStarted {
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
                        Ok(Reply::LoggedIn {
                            session: SessionId(token.0),
                        })
                    }
                    Outcome::Pending => Ok(Reply::DeviceSignInPending),
                    Outcome::Denied => Err(Error::DeviceSignInDenied),
                    Outcome::Expired => Err(Error::DeviceSignInExpired),
                }
            }
            // Everything else needs a session that is live on this
            // endpoint. Each request names its own, so one connection can
            // serve several users at once (§15).
            request => match self.authenticate(connection, state, envelope.session).await {
                Ok((session, user)) => {
                    // Locked, only these two (P2-08).
                    let exempt = matches!(request, Request::WhoAmI | Request::UnlockProfile { .. });
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
                        Ok(()) => self.serve(connection, state, session, user, request).await,
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

    /// One image on its own stream: a request frame in; a response frame
    /// and, if it is `ok`, the image's bytes out.
    async fn artwork(
        &self,
        connection: &Connection,
        state: &ConnectionState,
        mut send: SendStream,
        mut recv: RecvStream,
    ) {
        let answer = match codec::read_frame::<_, ArtworkRequest>(&mut recv).await {
            Ok(request) => self.find_artwork(connection, state, request).await,
            Err(e) => Err(Error::Malformed {
                message: e.to_string(),
            }),
        };
        let sent = async {
            match answer {
                Ok(image) => {
                    let reply = ArtworkReply {
                        content_type: image.content_type,
                        length: image.bytes.len() as u64,
                        etag: image.etag,
                    };
                    write_frame(&mut send, &ArtworkResponse::Ok(reply)).await?;
                    send.write_all(&image.bytes)
                        .await
                        .map_err(std::io::Error::other)?;
                }
                Err(error) => write_frame(&mut send, &ArtworkResponse::Err(error)).await?,
            }
            Ok::<_, codec::CodecError>(())
        };
        if let Err(e) = sent.await {
            tracing::debug!(error = %e, "cannot send artwork");
            return;
        }
        let _ = send.finish();
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
            .map_err(|refusal| match refusal {
                Refusal::Expired => Error::PlaybackExpired,
                Refusal::NotYours => Error::Unauthenticated,
            })?;
        if using.recheck {
            let live = self
                .services
                .auth
                .endpoint_session(&session.0, &endpoint)
                .await
                .map_err(internal)?;
            if live.is_none() {
                playbacks.end(handle);
                return Err(Error::Unauthenticated);
            }
        }
        Ok(using.playback)
    }

    /// One range read on its own stream: a request frame in; a response
    /// frame and exactly `sent` bytes of the file out.
    async fn media_read(
        &self,
        connection: &Connection,
        state: &ConnectionState,
        mut send: SendStream,
        mut recv: RecvStream,
    ) {
        let opened = match codec::read_frame::<_, MediaRead>(&mut recv).await {
            Ok(read) => self.open_range(connection, state, &read).await,
            Err(e) => Err(Error::Malformed {
                message: e.to_string(),
            }),
        };
        let sent = async {
            match opened {
                Ok((file, reply)) => {
                    let sent = reply.sent;
                    write_frame(&mut send, &MediaReadResponse::Ok(reply)).await?;
                    // Streamed from disk: nothing is held whole in memory.
                    let copied = tokio::io::copy(&mut file.take(sent), &mut send).await?;
                    if copied != sent {
                        return Err(codec::CodecError::Io(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            format!("the file ended after {copied} of {sent} bytes"),
                        )));
                    }
                }
                Err(error) => {
                    write_frame(&mut send, &MediaReadResponse::Err(error)).await?;
                }
            }
            Ok::<_, codec::CodecError>(())
        };
        match sent.await {
            Ok(()) => {
                let _ = send.finish();
            }
            Err(e) => {
                tracing::debug!(error = %e, "media read failed");
                let _ = send.reset(VarInt::from_u32(0));
            }
        }
    }

    /// The file positioned for `read`, and the reply that will precede it.
    async fn open_range(
        &self,
        connection: &Connection,
        state: &ConnectionState,
        read: &MediaRead,
    ) -> Result<(tokio::fs::File, MediaReadReply), Error> {
        let playback = self
            .use_playback(connection, &read.handle, &read.session)
            .await?;
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
        let mut file = tokio::fs::File::open(&playback.path)
            .await
            .map_err(unreadable)?;
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
    ) -> Result<Reply, Error> {
        let catalog = &self.services.catalog;
        let principal = Principal::LocalUser(user.user);
        match request {
            Request::WhoAmI => Ok(Reply::WhoAmI(UserSummary {
                user_id: format!("local:{}", user.user.0),
                username: user.username,
                display_name: user.display_name,
                role: user.role,
                locked: state.locks.lock().expect("never poisoned").locked(&session),
            })),
            Request::UnlockProfile { pin } => {
                let endpoint = connection.remote_id().to_string();
                match self.services.auth.unlock(user.user, &endpoint, &pin).await {
                    Ok(()) => {
                        state.locks.lock().expect("never poisoned").unlock(&session);
                        Ok(Reply::Done)
                    }
                    Err(UnlockError::Wrong) => Err(Error::InvalidCredentials),
                    Err(UnlockError::RateLimited { retry_after }) => Err(Error::RateLimited {
                        retry_after_ms: retry_after.as_millis() as u64,
                    }),
                    Err(UnlockError::Revoked) => Err(Error::Unauthenticated),
                    Err(UnlockError::Internal(e)) => Err(internal(e)),
                }
            }
            // The PIN, and where it is asked for, need only the session:
            // a real sign-in made it, a locked one never gets here, and a
            // PIN is never a way to sign in.
            Request::SetPin { pin } => {
                if !auth::valid_pin(&pin) {
                    return Err(Error::InvalidInput {
                        message: "a PIN is 4 to 8 digits".into(),
                    });
                }
                let phc = auth::hash_pin(&pin).await.map_err(internal)?;
                self.store()
                    .set_pin(user.user, &phc)
                    .await
                    .map_err(internal)?;
                tracing::info!(user = %user.username, "PIN set");
                Ok(Reply::Done)
            }
            Request::ClearPin => {
                self.store().clear_pin(user.user).await.map_err(internal)?;
                tracing::info!(user = %user.username, "PIN cleared");
                Ok(Reply::Done)
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
                        endpoint_id: d.endpoint_id,
                        name: d.name,
                        last_seen_at: d.last_seen_at,
                        pin_required: d.pin_required,
                    })
                    .collect();
                Ok(Reply::Devices { devices })
            }
            Request::SetPinRequired { device, required } => {
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
                    locks.unlock(&session);
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
            Request::Search { query, limit } => {
                let mut items = catalog
                    .search(&query, limit.min(MAX_PAGE))
                    .await
                    .map_err(catalog_error)?;
                self.fill(principal, items.iter_mut()).await?;
                Ok(Reply::Search { items })
            }
            Request::UpdatePlaybackProgress { id, position_ms } => {
                self.services
                    .watch
                    .progress(principal, &id, Duration::from_millis(position_ms))
                    .await
                    .map_err(watch_error)?;
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
                let markers = catalog.item(&id).await.map_err(catalog_error)?.markers;
                let handle = Playbacks::new_handle();
                // This device's earlier encodes are abandoned: free their
                // slots before this one takes one.
                let endpoint = connection.remote_id().to_string();
                let ended = self.services.playbacks.end_conversions_on(&endpoint);
                if ended > 0 {
                    tracing::info!(%endpoint, ended, "a device's earlier conversions ended");
                }
                let (subtitle, size, conversion) = self
                    .prepare(
                        &handle,
                        &id,
                        (&path, &descriptor),
                        &plan,
                        start_ms.map(Duration::from_millis),
                        None,
                    )
                    .await?;
                let handle = self.services.playbacks.start(
                    handle,
                    Opened {
                        session,
                        endpoint,
                        item: id.clone(),
                        selection,
                        path,
                        size,
                        subtitle,
                        conversion,
                    },
                );
                tracing::info!(%id, user = %user.username, "playback started");
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
                let (subtitle, size, conversion) = self
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
                        selection,
                        size,
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
                self.services.playbacks.end(&handle);
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
                    .restart(Duration::from_millis(position_ms), 1)
                    .map_err(|e| match e {
                        JobError::Busy => Error::Busy,
                        JobError::Prepare(reason) => Error::PlaybackFailed { reason },
                    })?;
                tracing::debug!(epoch, position_ms, "conversion restarted at a seek");
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
            Request::ReadSubtitle { handle, offset } => {
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
                let (bytes, total) = read_subtitle(&path, offset).await?;
                Ok(Reply::Subtitle {
                    format,
                    bytes,
                    total,
                })
            }
            Request::ReadSubtitleSegment { handle, n } => {
                let playback = self.use_playback(connection, &handle, &session).await?;
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
                    total: None,
                })
            }
            Request::LookupDeviceSignIn { code } => {
                let code = self.sign_in_code(connection, &user, &code)?;
                match self.services.auth.sign_ins().lookup(&code) {
                    Some(waiting) => Ok(Reply::DeviceSignInRequest(DeviceSignInRequest {
                        device_name: waiting.device_name,
                        client_name: waiting.client_name,
                        requested_at: waiting.requested_at,
                    })),
                    None => Err(self.wrong_sign_in_code(connection, &user)),
                }
            }
            Request::ApproveDeviceSignIn { code } => {
                let code = self.sign_in_code(connection, &user, &code)?;
                let approver = connection.remote_id().to_string();
                let sign_ins = self.services.auth.sign_ins();
                let claim = match sign_ins.claim(&code, &approver) {
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
                    Ok(token) => {
                        sign_ins.fulfil(&claim.request, Some(token));
                        tracing::info!(
                            user = %user.username,
                            device = %claim.endpoint,
                            "device sign-in approved"
                        );
                        Ok(Reply::Done)
                    }
                    Err(e) => {
                        sign_ins.fulfil(&claim.request, None);
                        Err(internal(e))
                    }
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
            | Request::Unknown => {
                unreachable!("pre-auth and unknown requests are answered before the session check")
            }
        }
    }

    /// A password login from this connection's endpoint. The session is
    /// bound to that endpoint (P1-12 checks it on every request), and the
    /// endpoint is the source the failure limit counts.
    async fn login_password(
        &self,
        connection: &Connection,
        login: LoginPassword,
        device_name: Option<String>,
    ) -> Result<Reply, Error> {
        let endpoint_id = connection.remote_id().to_string();
        let binding = SessionBinding::Endpoint {
            endpoint_id: endpoint_id.clone(),
            device_name,
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
                Ok(Reply::LoggedIn {
                    session: SessionId(done.token.0),
                })
            }
            Err(LoginError::InvalidCredentials) => Err(Error::InvalidCredentials),
            Err(LoginError::RateLimited { retry_after }) => Err(Error::RateLimited {
                retry_after_ms: u64::try_from(retry_after.as_millis()).unwrap_or(u64::MAX),
            }),
            Err(LoginError::Internal(e)) => {
                tracing::error!(error = %e, "login failed");
                Err(Error::Internal)
            }
        }
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
    fn end_playbacks_unless_back(&self, endpoint: iroh::EndpointId) {
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
    /// writes it), and the conversion, started at `start`. `replaces` is
    /// the playback as it was, when this is a subtitle change.
    async fn prepare(
        &self,
        handle: &PlaybackHandle,
        id: &MediaId,
        (path, descriptor): (&Path, &Arc<MediaDescriptor>),
        plan: &PlaybackPlan,
        start: Option<Duration>,
        replaces: Option<&Playback>,
    ) -> Result<(Option<Sidecar>, u64, Option<Arc<Conversion>>), Error> {
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
                let size = tokio::fs::metadata(path)
                    .await
                    .map_err(|e| {
                        tracing::warn!(%id, error = %e, "cannot stat a media file");
                        Error::Unavailable {
                            message: "the media file could not be read".into(),
                        }
                    })?
                    .len();
                (subtitle, size, None)
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
                (text, 0, Some(conversion))
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
        tracing::info!(?auth, "client connected");
        self.connections.insert(id, auth.clone());
        let state = Arc::new(ConnectionState::default());

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
        tracing::info!(?auth, %reason, "client disconnected");
        self.end_playbacks_unless_back(connection.remote_id());
        Ok(())
    }

    /// Tell every client the server is going away, before the router
    /// closes the connections.
    async fn shutdown(&self) {
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

/// The most items one page or search returns, whatever the client asks.
const MAX_PAGE: u32 = 500;

fn internal(e: impl std::fmt::Display) -> Error {
    tracing::error!(error = %e, "request failed");
    Error::Internal
}

/// How long a read past what an encode has written waits for more.
const READ_WAIT: Duration = Duration::from_secs(10);

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

/// A subtitle file's bytes for one reply, which is one frame (P2-11).
///
/// With an `offset`: at most [`SUBTITLE_CHUNK`] bytes from it, and the
/// file's length. At or past the end that is no bytes, so a client's loop
/// ends. Only the chunk is read, however large the file.
///
/// Without one (an older client): the whole file and no length. A file
/// that cannot fit a frame is refused by its size alone, not read first.
async fn read_subtitle(path: &Path, offset: Option<u64>) -> Result<(Vec<u8>, Option<u64>), Error> {
    let unreadable = |e: std::io::Error| {
        tracing::warn!(path = %path.display(), error = %e, "cannot read a subtitle");
        Error::Unavailable {
            message: "the subtitle file could not be read".into(),
        }
    };
    let mut file = tokio::fs::File::open(path).await.map_err(unreadable)?;
    let total = file.metadata().await.map_err(unreadable)?.len();
    let mut bytes = Vec::new();
    let Some(offset) = offset else {
        let limit = u64::from(codec::MAX_FRAME);
        if total > limit {
            tracing::warn!(total, "a subtitle read whole is over the frame limit");
            return Err(Error::ReplyTooLarge { bytes: total });
        }
        // One byte over the limit at most: a file that grew since is still
        // refused when the frame is written, never cut.
        let mut whole = file.take(limit + 1);
        whole.read_to_end(&mut bytes).await.map_err(unreadable)?;
        return Ok((bytes, None));
    };
    if offset < total {
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(unreadable)?;
        let mut chunk = file.take(SUBTITLE_CHUNK);
        chunk.read_to_end(&mut bytes).await.map_err(unreadable)?;
    }
    Ok((bytes, Some(total)))
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
        CatalogError::NotFound(what) | CatalogError::ForeignId(what) => Error::NotFound { what },
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
        format!("devcode:{}", user.user.0),
        format!("devcode@{}", connection.remote_id()),
    )
}

pub(crate) fn auth_methods() -> Vec<AuthMethod> {
    vec![AuthMethod::Password, AuthMethod::DeviceApproval]
}
