//! `spoolframe/client/1` on one connection (ARCHITECTURE §20, §33).
//!
//! Each bidirectional stream the client opens is one RPC. The first
//! request must be `Hello`; after it the server opens the events stream.
//! Only the pre-auth requests are served without a session.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use iroh::endpoint::{Connection, RecvStream, SendStream, VarInt};
use iroh::protocol::{AcceptError, ProtocolHandler};
use spoolframe_catalog::{CatalogError, Page};
use spoolframe_media::SubtitleSource;
use spoolframe_protocol::codec::{self, read_frame_bytes, write_frame};
use spoolframe_protocol::streams::{bi, reset, uni};
use spoolframe_protocol::{
    ArtworkReply, ArtworkRequest, ArtworkResponse, MAX_READ, MediaRead, MediaReadReply,
    MediaReadResponse, PlaybackHandle, PlaybackPlan,
};
use spoolframe_protocol::{
    AuthMethod, Error, Event, Hello, LoginPassword, Reply, Request, RequestEnvelope, Response,
    ServerInfo, SessionId, SessionInfo, UserSummary,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::artwork::{Artwork, ArtworkError};
use crate::auth::{LoginError, SessionToken};
use crate::network::{ConnectionAuth, Connections, Services};
use crate::planner;
use crate::playback::{Playback, Refusal, Sidecar};
use crate::principal::Principal;
use crate::store::sessions::{SessionBinding, SessionUser};
use crate::watch::WatchError;

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
            self.artwork(&connection, send, recv).await;
            return;
        }
        if kind == bi::MEDIA {
            self.media_read(&connection, send, recv).await;
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
        let response = match read_frame_bytes(&mut recv).await {
            Ok(bytes) => self.answer(&connection, &state, &bytes).await,
            Err(e) => Response::Err(Error::Malformed {
                message: e.to_string(),
            }),
        };
        if let Err(e) = write_frame(&mut send, &response).await {
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
            // Everything else needs a session that is live on this
            // endpoint. Each request names its own, so one connection can
            // serve several users at once (§15).
            request => match self.authenticate(connection, envelope.session).await {
                Ok((session, user)) => self.serve(connection, state, session, user, request).await,
                Err(e) => Err(e),
            },
        };
        reply.into()
    }

    /// The user of `session`, if it is live and bound to this connection's
    /// endpoint. Every failure is the same `Unauthenticated`.
    async fn authenticate(
        &self,
        connection: &Connection,
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
            Ok(Some(user)) => Ok((session, user)),
            Ok(None) => Err(Error::Unauthenticated),
            Err(e) => Err(internal(e)),
        }
    }

    /// One image on its own stream: a request frame in; a response frame
    /// and, if it is `ok`, the image's bytes out.
    async fn artwork(&self, connection: &Connection, mut send: SendStream, mut recv: RecvStream) {
        let answer = match codec::read_frame::<_, ArtworkRequest>(&mut recv).await {
            Ok(request) => self.find_artwork(connection, request).await,
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
        request: ArtworkRequest,
    ) -> Result<Artwork, Error> {
        self.authenticate(connection, Some(request.session)).await?;
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
        mut send: SendStream,
        mut recv: RecvStream,
    ) {
        let opened = match codec::read_frame::<_, MediaRead>(&mut recv).await {
            Ok(read) => self.open_range(connection, &read).await,
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
        read: &MediaRead,
    ) -> Result<(tokio::fs::File, MediaReadReply), Error> {
        let playback = self
            .use_playback(connection, &read.handle, &read.session)
            .await?;
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
        Ok((file, MediaReadReply { sent, size }))
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
                is_admin: user.is_admin,
            })),
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
            Request::Home => Ok(Reply::Home(
                self.services
                    .watch
                    .home(principal)
                    .await
                    .map_err(watch_error)?,
            )),
            Request::Tracks { id } => {
                let path = catalog.media_path(&id).await.map_err(catalog_error)?;
                let descriptor = self.services.prober.describe(&path).await.map_err(|e| {
                    tracing::warn!(%id, error = %e, "cannot describe a media file");
                    Error::Unavailable {
                        message: "the media file could not be read".into(),
                    }
                })?;
                Ok(Reply::Tracks(descriptor.tracks()))
            }
            Request::PlaybackPlan { id, selection } => {
                // Capabilities belong to the connection: they came with Hello.
                let caps = state.hello.get().and_then(|h| h.capabilities.as_ref());
                if caps.is_none() {
                    // No file needs reading to know the answer.
                    return Ok(Reply::PlaybackPlan {
                        plan: planner::plan_without_capabilities(),
                    });
                }
                let path = catalog.media_path(&id).await.map_err(catalog_error)?;
                let descriptor = self.services.prober.describe(&path).await.map_err(|e| {
                    tracing::warn!(%id, error = %e, "cannot describe a media file");
                    Error::Unavailable {
                        message: "the media file could not be read".into(),
                    }
                })?;
                Ok(Reply::PlaybackPlan {
                    plan: planner::plan(&descriptor, caps, selection),
                })
            }
            Request::StartPlayback { id, selection } => {
                let caps = state.hello.get().and_then(|h| h.capabilities.as_ref());
                let path = catalog.media_path(&id).await.map_err(catalog_error)?;
                let descriptor = self.services.prober.describe(&path).await.map_err(|e| {
                    tracing::warn!(%id, error = %e, "cannot describe a media file");
                    Error::Unavailable {
                        message: "the media file could not be read".into(),
                    }
                })?;
                let plan = planner::plan(&descriptor, caps, selection);
                let subtitle = match &plan {
                    PlaybackPlan::Direct { subtitle, .. } => subtitle.and_then(|planned| {
                        match &descriptor.subtitles.get(planned.index as usize)?.source {
                            SubtitleSource::External { path } => Some(Sidecar {
                                path: path.clone(),
                                format: planned.format,
                            }),
                            SubtitleSource::Embedded { .. } => None,
                        }
                    }),
                    PlaybackPlan::Unsupported { reasons } => {
                        return Err(Error::CannotPlay {
                            reasons: reasons.clone(),
                        });
                    }
                    // Phase 1 plans nothing else.
                    other => {
                        return Err(Error::CannotPlay {
                            reasons: vec![format!("{other:?} is not served yet")],
                        });
                    }
                };
                let size = tokio::fs::metadata(&path)
                    .await
                    .map_err(|e| {
                        tracing::warn!(%id, error = %e, "cannot stat a media file");
                        Error::Unavailable {
                            message: "the media file could not be read".into(),
                        }
                    })?
                    .len();
                let handle = self.services.playbacks.start(
                    session,
                    connection.remote_id().to_string(),
                    path,
                    size,
                    subtitle,
                );
                tracing::info!(%id, user = %user.username, "playback started");
                Ok(Reply::PlaybackStarted { handle, plan, size })
            }
            Request::ReadSubtitle { handle } => {
                let playback = self.use_playback(connection, &handle, &session).await?;
                let Some(sidecar) = playback.subtitle else {
                    return Err(Error::NotFound {
                        what: "an external subtitle in this playback".into(),
                    });
                };
                let bytes = tokio::fs::read(&sidecar.path).await.map_err(|e| {
                    tracing::warn!(path = %sidecar.path.display(), error = %e, "cannot read a subtitle");
                    Error::Unavailable {
                        message: "the subtitle file could not be read".into(),
                    }
                })?;
                Ok(Reply::Subtitle {
                    format: sidecar.format,
                    bytes,
                })
            }
            Request::Hello(_)
            | Request::AuthMethods
            | Request::LoginPassword(_)
            | Request::Ping
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
            .login_password(&login.username, &login.password, binding, &endpoint_id)
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
pub(crate) fn auth_methods() -> Vec<AuthMethod> {
    vec![AuthMethod::Password]
}
