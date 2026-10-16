//! One user signed in on one server from this device (§15–16). Several
//! sessions share the device's connection to that server; every call says
//! which session it is made in.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use spoolframe_protocol::{
    ArtworkRequest, ArtworkResponse, ArtworkSize, BitmapSubtitles, ChangePassword, ChosenTracks,
    ConfirmPassword, DeviceInfo, DeviceSignInRequest, FinishPasskeyRegistration,
    FinishPasskeySignIn, HomeScreen, ManifestKind, MediaRead, MediaReadResponse, PasskeyCreation,
    PasskeyInfo, PasskeyRequest, PinProof, PlaybackHandle, PlaybackPlan, PlexLink, PortableTie,
    Previews, Quality, Reply, Request, Response, SearchRequest, SearchResults, Selection,
    SessionId, SessionInfo, TrackPreferencesInfo, UserSummary,
};
use spoolframe_types::{
    ArtworkId, DisplayCapabilities, Item, Library, LibraryId, Marker, MediaId, MediaTracks,
    PersonId, SubtitleFormat, TrackPreferences,
};

use crate::client::Client;
use crate::error::SdkError;
use crate::link::ConnectionPath;
use crate::portable::PortableUserId;
use crate::storage::{Storage, StorageError, keys};

/// A session as it is stored: the token is the secret, the names are what
/// "Who's watching" shows before the server is asked. A session the
/// server ended keeps its names without a token, so its user is still
/// shown, to sign in again.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct StoredSession {
    pub token: Option<String>,
    pub username: String,
    pub display_name: String,
    /// How they last signed in on this device. Stores written before it
    /// was kept have no such field: `None`.
    #[serde(default)]
    pub method: Option<SignInMethod>,
    /// The portable identity it signed in with (P7-06), whether the
    /// session is that identity's own or the account it is tied to here:
    /// what signs it in again. `None` for any other sign-in.
    #[serde(default)]
    pub portable: Option<PortableUserId>,
}

/// Someone who signed in on a server from this device, as "Who's
/// watching" shows them: from what the device keeps, without asking the
/// server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedProfile {
    pub username: String,
    /// As of their last sign-in or [`crate::Client::profiles`].
    pub display_name: String,
    pub sign_in: ProfileSignIn,
    /// How their last successful sign-in on this device was made, so
    /// "Who's watching" can offer the same again once their session has
    /// ended. `None` for a profile kept before the SDK remembered it.
    pub method: Option<SignInMethod>,
    /// With [`SignInMethod::Portable`], the identity to sign in again
    /// with: [`crate::Client::sign_in_portable`], no password asked.
    pub portable: Option<PortableUserId>,
}

/// How a profile signed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignInMethod {
    /// [`crate::Client::sign_in`], with a password.
    Password,
    /// [`crate::Client::finish_passkey_sign_in`], with a passkey.
    Passkey,
    /// [`crate::Client::await_device_sign_in`]: approved on another device
    /// they were signed in on (P3-07).
    Device,
    /// [`crate::Client::sign_in_portable`] or
    /// [`crate::Client::redeem_invite_portable`]: this device's credential
    /// for a portable identity (P7-06). Signing in again asks nothing:
    /// call [`crate::Client::sign_in_portable`] with
    /// [`SavedProfile::portable`].
    Portable,
}

/// How choosing a saved profile signs them in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProfileSignIn {
    /// A session is kept: choosing them signs in without asking. The
    /// server may still have ended it since; [`crate::Client::profiles`]
    /// asks it, and a session it ended turns into `Password`.
    Automatic,
    /// Their session ended (it expired, was revoked, or the server no
    /// longer knows it): they sign in again, with their password or a
    /// passkey. [`SavedProfile::method`] says which they used last.
    Password,
}

impl From<&StoredSession> for SavedProfile {
    fn from(stored: &StoredSession) -> Self {
        Self {
            username: stored.username.clone(),
            display_name: stored.display_name.clone(),
            sign_in: match stored.token {
                Some(_) => ProfileSignIn::Automatic,
                None => ProfileSignIn::Password,
            },
            method: stored.method,
            portable: stored.portable,
        }
    }
}

pub(crate) async fn load(
    storage: &dyn Storage,
    server: &EndpointId,
) -> Result<Vec<StoredSession>, StorageError> {
    match storage.secret(&keys::sessions(&server.to_string())).await? {
        None => Ok(Vec::new()),
        Some(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| StorageError(format!("the sessions cannot be read: {e}"))),
    }
}

pub(crate) async fn save(
    storage: &dyn Storage,
    server: &EndpointId,
    sessions: &[StoredSession],
) -> Result<(), StorageError> {
    let key = keys::sessions(&server.to_string());
    if sessions.is_empty() {
        return storage.set_secret(&key, None).await;
    }
    let bytes = serde_json::to_vec(sessions).expect("sessions always serialize");
    storage.set_secret(&key, Some(bytes)).await
}

/// A page of a library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemsPage {
    pub items: Vec<Item>,
    /// How many items the library has in all.
    pub total: u32,
}

/// An item and what is under it: a show's seasons, a season's episodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemDetail {
    pub item: Item,
    pub children: Vec<Item>,
    /// The item's intro and credits (P2-10), in order of start. Empty when
    /// it has none, or the server is too old to send them.
    pub markers: Vec<Marker>,
    /// The playlists the user sees that hold it (PX-40). Empty when none,
    /// or the server has no playlists.
    pub playlists: Vec<spoolframe_protocol::playlists::PlaylistRef>,
    /// Its heart (PX-43): a track's `All` or `None`, an album's `All`,
    /// `Some` or `None`. `None` for anything else, or when the server has
    /// no Liked.
    pub liked: Option<spoolframe_protocol::playlists::Liked>,
    /// An album's tracks among `children` that are liked (PX-43). Empty
    /// otherwise.
    pub liked_children: Vec<MediaId>,
}

/// How the server would play an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub plan: PlaybackPlan,
    /// The tracks the server chose, when none were asked for.
    pub chosen: Option<ChosenTracks>,
}

/// A started playback, read through [`Session::read`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Playback {
    pub handle: PlaybackHandle,
    pub plan: PlaybackPlan,
    /// The size of the file the handle reads; 0 for a converted playback,
    /// whose size grows as it is encoded (see [`Chunk::size`]).
    pub size: u64,
    /// The tracks the server chose, when none were asked for.
    pub chosen: Option<ChosenTracks>,
    /// The encode the reads are of: 0 at the start, one more after each
    /// [`Session::seek_playback`].
    pub epoch: u32,
    /// The item's intro and credits, as in [`ItemDetail::markers`]. Their
    /// times are the item's own, wherever the playback started.
    pub markers: Vec<Marker>,
}

/// A playback after [`Session::change_subtitle`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackChange {
    pub plan: PlaybackPlan,
    /// As [`Playback::size`].
    pub size: u64,
    /// As [`Playback::epoch`]; 0 for direct play.
    pub epoch: u32,
}

/// How to start a playback, beyond its tracks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaybackOptions {
    /// Where a converted playback's encode begins (P2-03). Direct play
    /// reads from wherever the app likes.
    pub start: Option<Duration>,
    /// How a picked bitmap subtitle (PGS) is shown (P2-07).
    pub bitmap: BitmapSubtitles,
    /// The quality the viewer chose for it (P6-06).
    pub quality: PlaybackQuality,
    /// What the screen shows as it starts (PGPU-06): its HDR formats and
    /// bit depth, over the device's capabilities, for this playback and
    /// its re-plans. An app whose display mode changes while it runs (a
    /// TV's Match Dynamic Range) works it out at each start. `None`: the
    /// device's capabilities as they are.
    pub display: Option<DisplayCapabilities>,
}

/// The quality a viewer chooses for a playback (P6-06). The app's own
/// limit (`ClientCapabilities::max_bitrate`) stays a ceiling over each.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PlaybackQuality {
    /// For the link, as the SDK measures it ([`Session::link_rate`]): a
    /// slow link starts as a conversion sized to it, and a direct play
    /// that stalls on it is switched to one.
    #[default]
    Auto,
    /// At most this many bits per second, whatever the link.
    Bitrate(u64),
    /// The file as it is, whatever the link: converted only for what the
    /// device cannot play. It may buffer on a slow link.
    Original,
}

/// Part of a converted playback's output (P2-04, P2-05).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub bytes: Vec<u8>,
    /// Bytes written so far: the whole output (or segment) once
    /// `complete`.
    pub size: u64,
    /// The encode has finished it: no more bytes will come.
    pub complete: bool,
    pub epoch: u32,
}

/// A converted read's answer with its bytes still arriving (PX-46): what
/// a [`Chunk`] holds, handed on piece by piece, so a player gets the first
/// bytes of a segment while the rest is on the link. The link meter is fed
/// as they arrive, as for a [`Chunk`].
#[derive(Debug)]
pub struct ChunkStream {
    /// The bytes this answer carries: all of the body.
    pub sent: u64,
    /// Bytes written so far: the whole output (or segment) once
    /// `complete`.
    pub size: u64,
    /// The encode has finished it: no more bytes will come.
    pub complete: bool,
    pub epoch: u32,
    body: ChunkBody,
    received: u64,
}

#[derive(Debug)]
enum ChunkBody {
    Arriving(crate::connection::BodyStream),
    /// A backend that reads whole hands its chunk on at once.
    Whole(Option<Vec<u8>>),
}

impl ChunkStream {
    /// The next piece of the body, as much as arrived; `None` once all of
    /// it (`sent` bytes) did. An error part-way (a lost connection, a
    /// stall) ends it: read again from `offset + received()`.
    pub async fn next(&mut self) -> Result<Option<Vec<u8>>, SdkError> {
        let piece = match &mut self.body {
            ChunkBody::Arriving(body) => body.next().await?,
            ChunkBody::Whole(bytes) => bytes.take().filter(|b| !b.is_empty()),
        };
        if let Some(piece) = &piece {
            self.received += piece.len() as u64;
        }
        Ok(piece)
    }

    /// The bytes handed on so far.
    pub fn received(&self) -> u64 {
        self.received
    }

    /// All that is left of the body, as one [`Chunk`].
    pub async fn into_chunk(mut self) -> Result<Chunk, SdkError> {
        let mut bytes = Vec::new();
        while let Some(piece) = self.next().await? {
            bytes.extend_from_slice(&piece);
        }
        Ok(Chunk {
            bytes,
            size: self.size,
            complete: self.complete,
            epoch: self.epoch,
        })
    }
}

impl From<Chunk> for ChunkStream {
    /// A chunk read whole, handed on as one piece.
    fn from(chunk: Chunk) -> Self {
        Self {
            sent: chunk.bytes.len() as u64,
            size: chunk.size,
            complete: chunk.complete,
            epoch: chunk.epoch,
            body: ChunkBody::Whole(Some(chunk.bytes)),
            received: 0,
        }
    }
}

/// An external subtitle, whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subtitle {
    pub format: SubtitleFormat,
    pub bytes: Vec<u8>,
}

/// One chunk of a subtitle, as the server answered it (P2-11).
struct SubtitleChunk {
    format: SubtitleFormat,
    bytes: Vec<u8>,
    total: u64,
    version: u64,
}

/// A subtitle as its chunks arrive (P2-11): `bytes` of the `total` the
/// first chunk named, of the file with the `version` it named.
struct Chunks {
    format: SubtitleFormat,
    total: u64,
    version: u64,
    bytes: Vec<u8>,
}

impl Chunks {
    fn complete(&self) -> bool {
        self.bytes.len() as u64 == self.total
    }

    /// Join the next chunk, as the server answered it. The read fails
    /// when the chunk is not the next part of the same file: joined, it
    /// would be a wrong file that looks whole, or a read that never ends.
    fn add(&mut self, chunk: SubtitleChunk) -> Result<(), SdkError> {
        if chunk.format != self.format || chunk.total != self.total || chunk.version != self.version
        {
            return Err(SdkError::Protocol(format!(
                "the subtitle changed while it was read: {:?} of {} bytes (version {:x}), then {:?} of {} (version {:x})",
                self.format, self.total, self.version, chunk.format, chunk.total, chunk.version
            )));
        }
        let have = self.bytes.len() as u64;
        if chunk.bytes.is_empty() && have < self.total {
            return Err(SdkError::Protocol(format!(
                "no bytes of the subtitle at {have} of {}",
                self.total
            )));
        }
        let now = have + chunk.bytes.len() as u64;
        if now > self.total {
            return Err(SdkError::Protocol(format!(
                "{now} bytes of a subtitle of {}",
                self.total
            )));
        }
        self.bytes.extend(chunk.bytes);
        Ok(())
    }
}

/// Where a subtitle's chunks come from: a playback on the server, or a
/// stand-in for one in the tests.
trait SubtitleChunks {
    /// The chunk at `offset`. With `version`, only of the file an earlier
    /// chunk came from; [`SdkError::SubtitleChanged`] if it is another.
    async fn chunk(&self, offset: u64, version: Option<u64>) -> Result<SubtitleChunk, SdkError>;
}

/// A subtitle read whole from `source`.
///
/// The server answers `SubtitleChanged` when the file is no longer the
/// one the first chunk came from. The chunks read so far are then of a
/// file that is gone, and the read starts again from 0, once: a file that
/// changes during two reads is still being written, and the caller reads
/// it again later.
async fn read_whole(source: &impl SubtitleChunks) -> Result<Subtitle, SdkError> {
    match read_once(source).await {
        Err(SdkError::SubtitleChanged) => read_once(source).await,
        read => read,
    }
}

/// The chunks from 0 until the total the first one named, joined.
async fn read_once(source: &impl SubtitleChunks) -> Result<Subtitle, SdkError> {
    let first = source.chunk(0, None).await?;
    let mut chunks = Chunks {
        format: first.format,
        total: first.total,
        version: first.version,
        bytes: Vec::new(),
    };
    chunks.add(first)?;
    while !chunks.complete() {
        let offset = chunks.bytes.len() as u64;
        chunks.add(source.chunk(offset, Some(chunks.version)).await?)?;
    }
    Ok(Subtitle {
        format: chunks.format,
        bytes: chunks.bytes,
    })
}

/// The selected external subtitle of a playback on the server.
struct PlaybackSubtitle<'a> {
    session: &'a Session,
    handle: &'a PlaybackHandle,
}

impl SubtitleChunks for PlaybackSubtitle<'_> {
    async fn chunk(&self, offset: u64, version: Option<u64>) -> Result<SubtitleChunk, SdkError> {
        let request = Request::ReadSubtitle {
            handle: self.handle.clone(),
            offset,
            version,
        };
        match self.session.call(request).await? {
            Reply::SubtitleChunk {
                format,
                bytes,
                total,
                version,
            } => Ok(SubtitleChunk {
                format,
                bytes,
                total,
                version,
            }),
            other => Err(unexpected(other)),
        }
    }
}

/// One signed-in user on one server. Cheap to clone.
#[derive(Clone)]
pub struct Session {
    pub(crate) client: Client,
    pub(crate) server: EndpointId,
    pub(crate) token: SessionId,
    pub(crate) user: UserSummary,
    /// Whether `WhoAmI` said the user may leave the server, when this
    /// session was made (PX-47).
    pub(crate) can_leave: bool,
    /// Whether the server trusted this device for the user when the
    /// session was made (P4-01); `None` for a session found again rather
    /// than made ([`crate::Client::profiles`]).
    pub(crate) remembered: Option<bool>,
    /// Signed in with a one-time password an admin reset to: the session
    /// may only change the password or sign out until it does. Shared by
    /// every clone, so a change made through one is seen by all (C5).
    pub(crate) password_change_required: Arc<AtomicBool>,
    /// The regular features the server listed in this session's `WhoAmI`.
    pub(crate) features: crate::playlists::Features,
}

/// A profile explicitly selected by the application. All session operations
/// remain available; choosing another profile relocks this one's PIN.
#[derive(Debug, Clone)]
pub struct ActiveProfile(pub(crate) Session);

impl std::ops::Deref for ActiveProfile {
    type Target = Session;
    fn deref(&self) -> &Session {
        &self.0
    }
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("server", &self.server)
            .field("user", &self.user.username)
            .finish_non_exhaustive()
    }
}

/// The reply was not the one the request has.
fn unexpected(reply: Reply) -> SdkError {
    SdkError::Protocol(format!("unexpected reply {reply:?}"))
}

impl Session {
    /// The server's key.
    pub fn server(&self) -> String {
        self.server.to_string()
    }

    /// Where a browser reaches the server's web UI (its `admin.public_url`),
    /// as the Hello of the connection this session is on said: for links
    /// into it, such as `<web_url>/profile` to link a Plex account.
    /// `None` when the server has no public URL. Read it when the link is
    /// shown, not once: a reconnect may bring a new one.
    pub fn web_url(&self) -> Option<String> {
        self.client.server_info(self.server)?.web_url
    }

    /// The experimental features the server turned on for this device
    /// (PX-49), as the Hello of the connection this session is on said:
    /// those [`crate::ClientOptions::experimental`] asked for that the
    /// server knows and has on. None before a connection, or from a server
    /// that knows none. Read it when it matters: a reconnect may change it.
    pub fn experimental(&self) -> crate::Experimental {
        self.client
            .server_info(self.server)
            .map(|info| crate::Experimental::of(&info.experimental))
            .unwrap_or_default()
    }

    /// Experimental (`http-playback`, PX-49): where a player with no Iroh
    /// (an AirPlay receiver) fetches playback `handle` over plain HTTP from
    /// the server's own port, or why not. Without the feature turned on
    /// ([`Session::experimental`]) it is [`HttpPlaybackRefusal::Off`] and
    /// the server is not asked. Asked again for the same playback, the
    /// same URL while it lives.
    ///
    /// [`HttpPlaybackRefusal::Off`]: crate::HttpPlaybackRefusal::Off
    pub async fn http_playback_url(
        &self,
        handle: &PlaybackHandle,
    ) -> Result<crate::HttpPlayback, SdkError> {
        use crate::{HttpPlayback, HttpPlaybackRefusal};
        if !self.experimental().http_playback {
            return Ok(HttpPlayback::Refused(HttpPlaybackRefusal::Off));
        }
        let request = Request::HttpPlaybackUrl {
            handle: handle.clone(),
        };
        match self.call(request).await? {
            Reply::HttpPlaybackUrl { url } => Ok(HttpPlayback::Url(url)),
            Reply::HttpPlaybackRefused { reason } => Ok(HttpPlayback::Refused(reason)),
            other => Err(unexpected(other)),
        }
    }

    /// How this device reaches the server now, for a player's
    /// diagnostics. Asks nothing and never dials: without a connection it
    /// is [`ConnectionPath::Unknown`].
    pub fn connection_path(&self) -> ConnectionPath {
        self.client.path_of(self.server)
    }

    /// How fast the link to the server carried direct play lately, in bits
    /// per second (P6-06): over the last few seconds of fetching, or the
    /// last such reading if it is no more than ten minutes old. `None`
    /// until enough was read directly to tell.
    pub fn link_rate(&self) -> Option<u64> {
        self.client
            .link(self.server)
            .meter()
            .recent(std::time::Instant::now())
            .map(|bytes| (bytes * 8.0) as u64)
    }

    /// Who this session is, as of sign-in or the last [`Session::whoami`].
    pub fn user(&self) -> &UserSummary {
        &self.user
    }

    /// Whether this device was trusted for the user once the sign-in that
    /// made this session was done (P4-01): for "Approved and remembered" or
    /// "Approved, not remembered" after a device sign-in, where the
    /// approver decides. `None` for a session found again by
    /// [`crate::Client::profiles`]; ask [`crate::Client::trusted_profiles`]
    /// or [`Session::devices`] then.
    pub fn remembered(&self) -> Option<bool> {
        self.remembered
    }

    /// One request in this session. A session the server no longer knows
    /// is forgotten here, and the caller gets [`SdkError::SignInRequired`].
    pub(crate) async fn call(&self, request: Request) -> Result<Reply, SdkError> {
        if matches!(
            request,
            Request::ReadSubtitle { .. } | Request::ReadSubtitleSegment { .. }
        ) {
            return self.subtitle_resource(request).await;
        }
        match self
            .client
            .call(self.server, Some(&self.token), request)
            .await?
        {
            Response::Ok(reply) => Ok(reply),
            Response::Err(e) => Err(self.failed(e).await),
        }
    }

    async fn subtitle_resource(&self, request: Request) -> Result<Reply, SdkError> {
        use spoolframe_protocol::resource::{ResourceMetadata, ResourceRequest, ResourceResponse};
        let handle = match &request {
            Request::ReadSubtitle { handle, .. } | Request::ReadSubtitleSegment { handle, .. } => {
                handle
            }
            _ => unreachable!(),
        };
        let revision = self
            .client
            .playback_revision(self.server, &self.token, handle)
            .await?;
        let chunk = matches!(request, Request::ReadSubtitle { .. });
        let request = match request {
            Request::ReadSubtitle {
                handle,
                offset,
                version,
            } => ResourceRequest::Subtitle {
                session: self.token.clone(),
                handle,
                revision,
                offset,
                version,
            },
            Request::ReadSubtitleSegment { handle, n } => ResourceRequest::SubtitleSegment {
                session: self.token.clone(),
                handle,
                revision,
                segment: n,
            },
            _ => unreachable!(),
        };
        let timeouts = self.client.inner.timeouts;
        let maximum = if chunk {
            spoolframe_protocol::SUBTITLE_CHUNK
        } else {
            spoolframe_protocol::MAX_READ
        };
        let (reply, bytes) = self
            .client
            .on_connection(self.server, Some(&self.token), true, |conn| {
                let request = &request;
                async move {
                    conn.resource(request, maximum, timeouts.of_read(), timeouts.request)
                        .await
                }
            })
            .await?;
        match reply {
            ResourceResponse::Ready {
                metadata:
                    ResourceMetadata::Subtitle {
                        format,
                        total,
                        version,
                        revision: actual,
                    },
                ..
            } if revision == actual => Ok(if chunk {
                Reply::SubtitleChunk {
                    format,
                    bytes,
                    total,
                    version,
                }
            } else {
                Reply::Subtitle { format, bytes }
            }),
            ResourceResponse::Error { error } => Err(self.failed(error).await),
            _ => Err(SdkError::Protocol(
                "subtitle returned another resource kind or revision".into(),
            )),
        }
    }

    async fn failed(&self, error: spoolframe_protocol::Error) -> SdkError {
        let error = SdkError::from_server(error);
        if error == SdkError::SignInRequired
            && let Err(e) = self.client.end_session(self.server, &self.token).await
        {
            return e;
        }
        error
    }

    /// Ask the server who this session is.
    pub async fn whoami(&self) -> Result<UserSummary, SdkError> {
        match self.call(Request::WhoAmI).await? {
            Reply::WhoAmI(spoolframe_protocol::WhoAmIReply { user, .. }) => Ok(user),
            other => Err(unexpected(other)),
        }
    }

    /// End this session on the server and forget it here, profile and
    /// all: signing out is leaving "Who's watching".
    pub async fn sign_out(&self) -> Result<(), SdkError> {
        match self.call(Request::Logout).await {
            Ok(Reply::LoggedOut) | Err(SdkError::SignInRequired) => {}
            Ok(other) => return Err(unexpected(other)),
            Err(e) => return Err(e),
        }
        self.client.drop_session(self.server, &self.token).await
    }

    /// This user's live sessions on every device.
    pub async fn list_sessions(&self) -> Result<Vec<SessionInfo>, SdkError> {
        match self.call(Request::ListSessions).await? {
            Reply::Sessions { sessions } => Ok(sessions),
            other => Err(unexpected(other)),
        }
    }

    pub async fn home(&self) -> Result<HomeScreen, SdkError> {
        match self.call(Request::Home).await? {
            Reply::Home(home) => Ok(home),
            other => Err(unexpected(other)),
        }
    }

    pub async fn libraries(&self) -> Result<Vec<Library>, SdkError> {
        match self.call(Request::Libraries).await? {
            Reply::Libraries { libraries } => Ok(libraries),
            other => Err(unexpected(other)),
        }
    }

    /// `limit` top-level items of `library`, from `offset`.
    pub async fn items(
        &self,
        library: &LibraryId,
        offset: u32,
        limit: u32,
    ) -> Result<ItemsPage, SdkError> {
        let request = Request::Items {
            library: library.clone(),
            offset,
            limit,
        };
        match self.call(request).await? {
            Reply::Items { items, total } => Ok(ItemsPage { items, total }),
            other => Err(unexpected(other)),
        }
    }

    pub async fn item(&self, id: &MediaId) -> Result<ItemDetail, SdkError> {
        match self.call(Request::Item { id: id.clone() }).await? {
            Reply::Item {
                item,
                children,
                markers,
                playlists,
                liked,
                liked_children,
            } => Ok(ItemDetail {
                item,
                children,
                markers,
                playlists: playlists.unwrap_or_default(),
                liked,
                liked_children: liked_children.unwrap_or_default(),
            }),
            other => Err(unexpected(other)),
        }
    }

    /// A scoped, grouped search (P3-09): the top match and a group per
    /// kind, or, with `request.kind`, one kind's page for "See all".
    pub async fn search(&self, request: SearchRequest) -> Result<SearchResults, SdkError> {
        match self.call(Request::Search(request)).await? {
            Reply::Search(results) => Ok(results),
            other => Err(unexpected(other)),
        }
    }

    /// One page of the items a person found by [`Self::search`] is in.
    pub async fn person_items(
        &self,
        person: &PersonId,
        offset: u32,
        limit: u32,
    ) -> Result<ItemsPage, SdkError> {
        let request = Request::PersonItems {
            person: person.clone(),
            offset,
            limit,
        };
        match self.call(request).await? {
            Reply::Items { items, total } => Ok(ItemsPage { items, total }),
            other => Err(unexpected(other)),
        }
    }

    /// The audio and subtitle tracks of a movie or episode.
    pub async fn tracks(&self, id: &MediaId) -> Result<MediaTracks, SdkError> {
        match self.call(Request::Tracks { id: id.clone() }).await? {
            Reply::Tracks(tracks) => Ok(tracks),
            other => Err(unexpected(other)),
        }
    }

    /// How the server would play `id` on this device with `selection`;
    /// with `None`, the server chooses the tracks from the user's
    /// preferences and says which in [`Plan::chosen`].
    pub async fn playback_plan(
        &self,
        id: &MediaId,
        selection: Option<Selection>,
    ) -> Result<Plan, SdkError> {
        self.playback_plan_with(id, selection, BitmapSubtitles::Auto)
            .await
    }

    /// [`Session::playback_plan`], showing a picked bitmap subtitle as
    /// `bitmap` says.
    pub async fn playback_plan_with(
        &self,
        id: &MediaId,
        selection: Option<Selection>,
        bitmap: BitmapSubtitles,
    ) -> Result<Plan, SdkError> {
        self.playback_plan_on(id, selection, bitmap, None).await
    }

    /// [`Session::playback_plan_with`] for a screen showing `display`
    /// (PGPU-06), as [`PlaybackOptions::display`] would start it.
    pub async fn playback_plan_on(
        &self,
        id: &MediaId,
        selection: Option<Selection>,
        bitmap: BitmapSubtitles,
        display: Option<DisplayCapabilities>,
    ) -> Result<Plan, SdkError> {
        let request = Request::PlaybackPlan {
            id: id.clone(),
            selection,
            bitmap,
            display,
        };
        match self.call(request).await? {
            Reply::PlaybackPlan { plan, chosen } => Ok(Plan { plan, chosen }),
            other => Err(unexpected(other)),
        }
    }

    /// Start playing `id`, with `selection` or, for `None`, the tracks the
    /// server chooses. [`SdkError::CannotPlay`] says why it cannot.
    pub async fn start_playback(
        &self,
        id: &MediaId,
        selection: Option<Selection>,
    ) -> Result<Playback, SdkError> {
        self.start_playback_with(id, selection, PlaybackOptions::default())
            .await
    }

    /// [`Session::start_playback`], with `options`. A converted plan
    /// (P2-03) starts the server's encode: read it with
    /// [`Session::read_chunk`] (progressive) or [`Session::manifest`] and
    /// [`Session::read_segment`] (HLS). [`SdkError::Busy`] means the
    /// server is converting all it may. The link's measured rate
    /// ([`Session::link_rate`]) goes with it: a slow link starts as a
    /// conversion sized to it (P6-06).
    pub async fn start_playback_with(
        &self,
        id: &MediaId,
        selection: Option<Selection>,
        options: PlaybackOptions,
    ) -> Result<Playback, SdkError> {
        let request = Request::StartPlayback {
            id: id.clone(),
            selection,
            start_ms: options.start.map(millis),
            bitmap: options.bitmap,
            quality: self.wire_quality(options.quality),
            display: options.display,
        };
        match self.call(request).await? {
            Reply::PlaybackStarted {
                handle,
                plan,
                size,
                chosen,
                epoch,
                markers,
            } => Ok(Playback {
                handle,
                plan,
                size,
                chosen,
                epoch,
                markers,
            }),
            other => Err(unexpected(other)),
        }
    }

    /// Restart a converted playback's encode at `position` (P2-04). Reads
    /// go on with the returned epoch, from offset 0 (or from the segment
    /// at `position`).
    pub async fn seek_playback(
        &self,
        handle: &PlaybackHandle,
        position: Duration,
    ) -> Result<u32, SdkError> {
        let request = Request::SeekPlayback {
            handle: handle.clone(),
            position_ms: millis(position),
        };
        match self.call(request).await? {
            Reply::PlaybackSeeked { epoch } => Ok(epoch),
            other => Err(unexpected(other)),
        }
    }

    /// Unlock this profile on this device with its PIN (P2-08). A wrong
    /// PIN is [`SdkError::InvalidCredentials`], too many in a while
    /// [`SdkError::RateLimited`]; after 10 wrong in a row the session here
    /// is ended ([`SdkError::SignInRequired`]) and the user signs in with
    /// their password. Switching to another profile and back locks it
    /// again.
    pub async fn unlock(&self, pin: &str) -> Result<(), SdkError> {
        self.done(Request::UnlockProfile {
            pin: pin.to_owned(),
        })
        .await
    }

    /// Set or change this user's PIN: 4 to 8 digits. A user with none
    /// ([`UserSummary::pin`]) sets their first with no `proof`. A user who
    /// has one changes it only with a [`PinProof`]; without one it is
    /// [`SdkError::InvalidInput`].
    ///
    /// [`PinProof::Pin`] is checked as [`Session::unlock`] checks a PIN and
    /// counts towards the same limit, with the same errors (10 wrong in a
    /// row sign this device out), but unlocks nothing.
    /// [`PinProof::Password`] is for a forgotten PIN: checked as
    /// [`Client::sign_in`](crate::Client::sign_in) checks it, a wrong one
    /// is a failed sign-in as this user from this device
    /// ([`SdkError::InvalidCredentials`], then [`SdkError::RateLimited`]),
    /// and it never signs the session out. A user with no password
    /// (passkeys or SSO only) gets [`SdkError::NoPassword`]: offer the PIN
    /// proof, or [`Session::web_url`]. A locked profile gets
    /// [`SdkError::ProfileLocked`].
    pub async fn set_pin(&self, pin: &str, proof: Option<PinProof>) -> Result<(), SdkError> {
        self.done(Request::SetPin {
            pin: pin.to_owned(),
            proof,
        })
        .await
    }

    /// Remove this user's PIN, and where it is asked for, with a
    /// [`PinProof`], checked as for [`Session::set_pin`].
    pub async fn clear_pin(&self, proof: PinProof) -> Result<(), SdkError> {
        self.done(Request::ClearPin { proof }).await
    }

    /// The devices this user has signed in from, with where the PIN is
    /// asked for.
    pub async fn devices(&self) -> Result<Vec<DeviceInfo>, SdkError> {
        match self.call(Request::ListDevices).await? {
            Reply::Devices { devices } => Ok(devices),
            other => Err(unexpected(other)),
        }
    }

    /// This user's Plex account link (P3-12): `None` when not linked. Read
    /// only: to link or unlink, open `<web_url>/profile`
    /// ([`Session::web_url`]) in a browser.
    pub async fn plex_link(&self) -> Result<Option<PlexLink>, SdkError> {
        match self.call(Request::GetPlexLink).await? {
            Reply::PlexLink { link } => Ok(link),
            other => Err(unexpected(other)),
        }
    }

    /// Trust `device` (an endpoint id from [`Session::devices`]; this
    /// device for `None`) for this user, or stop (P4-01), and say whether
    /// it opens their profile at start (`auto_login`, which needs trust).
    ///
    /// - Trust and auto-login can be turned on for this device only
    ///   ([`SdkError::InvalidInput`] for another: that is the web UI's),
    ///   and only soon after signing in
    ///   ([`SdkError::RecentSignInRequired`]: confirm it is them with
    ///   [`Session::confirm_password`] or [`Session::begin_confirm_passkey`]).
    /// - Auto-login turns it off for this device's other profiles, and the
    ///   PIN off for this profile here; [`Session::set_pin_required`] on
    ///   turns auto-login off. Say so before either.
    /// - Turning trust off ends the user's sessions on that device. On
    ///   this device that is this session: it is forgotten here, and the
    ///   profile stays in [`crate::Client::saved_profiles`] to sign in
    ///   again. For another device it needs a recent sign-in.
    pub async fn set_device_trust(
        &self,
        device: Option<&str>,
        trusted: bool,
        auto_login: bool,
    ) -> Result<(), SdkError> {
        let own = device.is_none_or(|d| d == self.client.id());
        self.done(Request::SetDeviceTrust {
            device: device.map(str::to_owned),
            trusted,
            auto_login,
        })
        .await?;
        if own && !trusted {
            self.client.end_session(self.server, &self.token).await?;
        }
        Ok(())
    }

    /// Delete another of this user's devices (an endpoint id from
    /// [`Session::devices`]; P4-03): their sessions there end and are
    /// forgotten, with its trust and PIN requirement, and it leaves
    /// [`Session::devices`]. If it signs in again it is listed as new.
    /// Other profiles on that device are kept.
    ///
    /// - Always needs a recent sign-in ([`SdkError::RecentSignInRequired`]:
    ///   confirm it is them with [`Session::confirm_password`] or
    ///   [`Session::begin_confirm_passkey`], then call again).
    /// - This device is [`SdkError::InvalidInput`]: use
    ///   [`Session::sign_out`].
    /// - A device that is not this user's is [`SdkError::NotFound`].
    pub async fn forget_device(&self, device: &str) -> Result<(), SdkError> {
        self.done(Request::ForgetDevice {
            device: device.to_owned(),
        })
        .await
    }

    /// Ask for this user's PIN on `device` (an endpoint id from
    /// [`Session::devices`]; this device for `None`), or stop asking.
    pub async fn set_pin_required(
        &self,
        device: Option<&str>,
        required: bool,
    ) -> Result<(), SdkError> {
        self.done(Request::SetPinRequired {
            device: device.map(str::to_owned),
            required,
        })
        .await
    }

    /// Begin making a passkey for this user in the app (P3-19): what the
    /// platform's passkey API makes one with, for the relying party in
    /// [`PasskeyCreation::rp_id`] (this server's, from the server id
    /// pinned with its key, PX-14), from [`PasskeyCreation::origin`], with
    /// user verification. The account's name in it is
    /// `<username> @ <server name>`. Only soon after
    /// signing in: a session older than 10 minutes gets
    /// [`SdkError::RecentSignInRequired`] (confirm it is them with
    /// [`Session::confirm_password`] or [`Session::begin_confirm_passkey`],
    /// then ask). The challenge is made for this session's server, as
    /// for [`crate::Client::begin_passkey_sign_in`].
    pub async fn begin_passkey_registration(&self) -> Result<PasskeyCreation, SdkError> {
        match self.call(Request::BeginPasskeyRegistration).await? {
            Reply::PasskeyRegistrationBegun(begun) => {
                let server_id = self.client.pinned_server_id(self.server).await?;
                Ok(begun.for_server(self.server.as_bytes(), &server_id))
            }
            other => Err(unexpected(other)),
        }
    }

    /// Keep the passkey the platform made for it, named for this device
    /// (`made.name`, 1 to 64 characters, or [`SdkError::InvalidInput`]).
    /// The user's profile shows it as "Spoolframe app · <name>". One that
    /// does not verify, or a registration that is over, is
    /// [`SdkError::InvalidCredentials`]: begin again.
    pub async fn finish_passkey_registration(
        &self,
        made: FinishPasskeyRegistration,
    ) -> Result<(), SdkError> {
        self.done(Request::FinishPasskeyRegistration(made)).await
    }

    /// This user's passkeys, oldest first: the app's and the web UI's, as
    /// My profile in the web UI lists them, so the user sees every way
    /// they sign in. Only those with [`PasskeyInfo::usable_here`] (made in
    /// an app, [`PasskeyInfo::app`]) sign in in the app.
    pub async fn passkeys(&self) -> Result<Vec<PasskeyInfo>, SdkError> {
        match self.call(Request::ListPasskeys).await? {
            Reply::Passkeys { passkeys } => Ok(passkeys),
            other => Err(unexpected(other)),
        }
    }

    /// Rename one of this user's passkeys (`id` from
    /// [`Session::passkeys`]). A name that is not 1 to 64 characters once
    /// trimmed is [`SdkError::InvalidInput`]; an `id` that is not one of
    /// this user's passkeys [`SdkError::NotFound`].
    pub async fn rename_passkey(&self, id: &str, name: &str) -> Result<(), SdkError> {
        self.done(Request::RenamePasskey {
            id: id.to_owned(),
            name: name.to_owned(),
        })
        .await
    }

    /// Remove one of this user's passkeys (`id` from
    /// [`Session::passkeys`]); a web UI's too. Only soon after signing in,
    /// as for adding one ([`SdkError::RecentSignInRequired`]: sign in
    /// again, then ask). Their last way to sign in, with no password or
    /// linked sign-in provider left, is [`SdkError::LastSignInMethod`]; an
    /// `id` that is not one of their passkeys [`SdkError::NotFound`].
    pub async fn remove_passkey(&self, id: &str) -> Result<(), SdkError> {
        self.done(Request::RemovePasskey { id: id.to_owned() })
            .await
    }

    /// Tie this user's account on this server to portable identity
    /// `identity` (P7-04), proved by this device: with the identity's root
    /// if it holds it (the phone), else with its device credential for it
    /// ([`crate::Portable::tie_proof`]). The root is never sent. The
    /// account then lists the identity among its ties
    /// ([`UserSummary::portable`], [`Session::tie_info`]); no second
    /// account is made. An account ties any number of identities (P7-09):
    /// its other ties stay.
    ///
    /// Only soon after signing in ([`SdkError::RecentSignInRequired`]:
    /// confirm, then ask again). An identity tied to another account here
    /// is [`SdkError::PortableIdTaken`]; one tied to this account already
    /// is `Ok`, changing nothing. A proof the server does not take (a key
    /// it has seen revoked, say) is [`SdkError::InvalidCredentials`].
    pub async fn tie(&self, identity: PortableUserId) -> Result<(), SdkError> {
        let nonce = match self.call(Request::TieBegin).await? {
            Reply::TieChallenge { nonce, server, .. } => {
                // Signed for the server dialled, whatever it names; one
                // that names another is not asking for itself.
                if server != *self.server.as_bytes() {
                    return Err(SdkError::Protocol(
                        "the tie challenge names another server".into(),
                    ));
                }
                nonce
            }
            other => return Err(unexpected(other)),
        };
        let proof = self
            .client
            .portable()
            .tie_proof(
                identity,
                self.server.as_bytes(),
                &nonce,
                &self.user.username,
            )
            .await?;
        self.done(Request::TieFinish {
            user: identity,
            proof,
        })
        .await
    }

    /// Sever this account's tie to `identity` (P7-04, P7-09). Only that
    /// link goes: the account's other ties, its sessions and its watch
    /// state stay. Only soon after signing in, as for [`Session::tie`]; no
    /// tie to `identity` on this account is [`SdkError::NotFound`].
    pub async fn untie(&self, identity: PortableUserId) -> Result<(), SdkError> {
        self.done(Request::Untie { user: identity }).await
    }

    /// Leave this server (PX-47). Once the server says it is done, this
    /// device removes this profile, and forgets the server when no other
    /// profile is kept for it ([`crate::Client::forget`]: its pin too).
    /// On a shared device, another user's profile and session on the
    /// server stay, and so does the pin. This session ends, on the server
    /// and here.
    ///
    /// What the server does depends on who signed in. A portable user's
    /// grant goes, and its watch history and settings there are deleted;
    /// coming back takes a new invite. A local account is set aside: its
    /// sessions end and it cannot sign in, its data stays, and the
    /// server's owner can restore it.
    ///
    /// Only soon after signing in ([`SdkError::RecentSignInRequired`]): a
    /// local user confirms ([`Session::confirm_password`] or
    /// [`Session::begin_confirm_passkey`]); a portable user signs in again
    /// with its credential ([`crate::Client::sign_in_portable`]), and leaves
    /// from the new session. The last admin gets [`SdkError::LastAdmin`]. A
    /// refused leave changes nothing, here or on the server.
    pub async fn leave_server(&self) -> Result<(), SdkError> {
        self.done(Request::LeaveServer).await?;
        self.client
            .left(self.server, &self.token, &self.user.username)
            .await
    }

    /// Whether the user may leave this server ([`Session::leave_server`]),
    /// as the server said when this session was made: signed in, resumed,
    /// or listed by [`crate::Client::profiles`]. `false` for the last
    /// admin, whom leaving would refuse with [`SdkError::LastAdmin`]:
    /// grey out "Leave this server" then. An admin made or demoted since
    /// is not seen here; [`Session::can_leave_now`] asks again, and
    /// leaving is still refused if it must be. A server that does not say
    /// counts as `true`.
    pub fn can_leave(&self) -> bool {
        self.can_leave
    }

    /// Whether the user may leave this server, as the server says now: as
    /// [`Session::can_leave`], read again, for a screen that offers
    /// leaving.
    pub async fn can_leave_now(&self) -> Result<bool, SdkError> {
        match self.call(Request::WhoAmI).await? {
            Reply::WhoAmI(reply) => Ok(reply.can_leave),
            other => Err(unexpected(other)),
        }
    }

    /// The portable identities this account is tied to, as the server
    /// says now (P7-04, P7-09): each with when it was tied and last signed
    /// in, the oldest tie first. Which is this device's own is the app's
    /// to mark, from [`crate::Portable::list`].
    pub async fn tie_info(&self) -> Result<Vec<PortableTie>, SdkError> {
        Ok(self.whoami().await?.portable)
    }

    /// Confirm it is this user with their account password (P3-07), when
    /// a change answers [`SdkError::RecentSignInRequired`]: approving a
    /// device, adding or removing a passkey, no longer asking for the PIN.
    /// This session then counts as signed in now, for 10 minutes; ask
    /// again. Only this session: the user's others are as they were.
    ///
    /// Checked as [`Client::sign_in`](crate::Client::sign_in) checks it: a
    /// wrong one is a failed sign-in as this user from this device
    /// ([`SdkError::InvalidCredentials`], then [`SdkError::RateLimited`]).
    /// A user with no password gets [`SdkError::NoPassword`]: offer
    /// [`Session::begin_confirm_passkey`]. A locked profile gets
    /// [`SdkError::ProfileLocked`]. The PIN does not confirm it is them.
    /// This session was signed in with a one-time password an admin reset
    /// to (from [`crate::Client::sign_in`]): show "Choose a new password"
    /// and call [`Session::change_password`]. Every other request is
    /// [`SdkError::PasswordChangeRequired`] until then. False for a session
    /// found again ([`crate::Client::profiles`]); such a one answers its
    /// first request with that error instead.
    pub fn password_change_required(&self) -> bool {
        self.password_change_required.load(Ordering::Relaxed)
    }

    /// Change this user's password from `current` to `new`. A wrong
    /// `current` is [`SdkError::InvalidCredentials`], counted as a failed
    /// sign-in; a `new` the server does not take (too short, or the same
    /// as `current`) is [`SdkError::InvalidInput`]; a user with no password
    /// gets [`SdkError::NoPassword`] (they set one in the web UI). Every
    /// other session of theirs ends, on every device. After a one-time
    /// password, `current` is that password, and this session may do
    /// anything again.
    pub async fn change_password(&self, current: &str, new: &str) -> Result<(), SdkError> {
        let request = Request::ChangePassword(ChangePassword {
            current_password: current.to_owned(),
            new_password: new.to_owned(),
        });
        match self.call(request).await? {
            Reply::Done => {
                self.password_change_required
                    .store(false, Ordering::Relaxed);
                Ok(())
            }
            other => Err(unexpected(other)),
        }
    }

    pub async fn confirm_password(&self, password: &str) -> Result<(), SdkError> {
        let request = Request::ConfirmPassword(ConfirmPassword {
            password: password.to_owned(),
        });
        self.confirmed(request).await
    }

    /// Begin confirming it is this user with one of their app passkeys,
    /// as for [`Session::confirm_password`]: what the platform's passkey
    /// API signs, for [`PasskeyRequest::rp_id`] as for
    /// [`crate::Client::begin_passkey_sign_in`], listing only this user's
    /// app passkeys. The challenge is this session's on this device, for 5
    /// minutes; beginning again replaces it. A user with no app passkey
    /// gets [`SdkError::NotFound`]. The challenge is made for this
    /// session's server, as for [`crate::Client::begin_passkey_sign_in`].
    pub async fn begin_confirm_passkey(&self) -> Result<PasskeyRequest, SdkError> {
        match self.call(Request::BeginConfirmPasskey).await? {
            Reply::PasskeySignInBegun(begun) => {
                let server_id = self.client.pinned_server_id(self.server).await?;
                Ok(begun.for_server(self.server.as_bytes(), &server_id))
            }
            other => Err(unexpected(other)),
        }
    }

    /// Finish it with what the passkey signed. Another user's passkey, a
    /// challenge that is over or not this session's, or anything else that
    /// does not verify is [`SdkError::InvalidCredentials`], counted as a
    /// failed passkey sign-in (then [`SdkError::RateLimited`]). An empty
    /// `user_handle` is sent as `None`, as
    /// [`crate::Client::finish_passkey_sign_in`] sends it.
    pub async fn finish_confirm_passkey(
        &self,
        signed: FinishPasskeySignIn,
    ) -> Result<(), SdkError> {
        let signed = crate::client::without_empty_user_handle(signed);
        self.confirmed(Request::FinishConfirmPasskey(signed)).await
    }

    /// The device showing sign-in code `code` (P3-07; typed any way, or
    /// from a scanned [`crate::device_sign_in::parse_link`]), to ask "Sign
    /// in on 'Living room TV'?". A code that is not pending is
    /// [`SdkError::NotFound`]; too many of those [`SdkError::RateLimited`].
    ///
    /// The approval also shows where the device asked from
    /// ([`DeviceSignInRequest::requested_from`], PX-09 finding 27): "The
    /// TV asked from 192.168.86.20.", or, for an address on the
    /// [`crate::AddressNetwork::Internet`] (or one this build does not
    /// know), a warning that the device is not on this network, so approve
    /// only one you can see: a code someone sent you is theirs.
    pub async fn lookup_device_sign_in(&self, code: &str) -> Result<DeviceSignInRequest, SdkError> {
        let request = Request::LookupDeviceSignIn {
            code: code.to_owned(),
        };
        match self.call(request).await? {
            Reply::DeviceSignInRequest(waiting) => Ok(waiting),
            other => Err(unexpected(other)),
        }
    }

    /// Sign the device showing `code` in as this user. It cannot be this
    /// device ([`SdkError::InvalidInput`]). Only soon after signing in: a
    /// session older than 10 minutes gets
    /// [`SdkError::RecentSignInRequired`] (confirm it is them with
    /// [`Session::confirm_password`] or [`Session::begin_confirm_passkey`],
    /// then ask).
    ///
    /// `remember` is the approver's answer to the device's request to be
    /// remembered ([`DeviceSignInRequest::remember`], P4-01): shown as
    /// "This device will be remembered", ticked, and untickable. The device
    /// is trusted only if it asked and this is true.
    pub async fn approve_device_sign_in(&self, code: &str, remember: bool) -> Result<(), SdkError> {
        self.done(Request::ApproveDeviceSignIn {
            code: code.to_owned(),
            remember,
        })
        .await
    }

    /// Refuse it; the device stops waiting.
    pub async fn deny_device_sign_in(&self, code: &str) -> Result<(), SdkError> {
        self.done(Request::DenyDeviceSignIn {
            code: code.to_owned(),
        })
        .await
    }

    /// Pick another subtitle (`None`: none) for a playing playback, shown
    /// as `bitmap` says (P2-07). Direct play only changes what
    /// [`Session::read_subtitle`] gives; a converted playback restarts its
    /// encode at `position`: read the returned plan's epoch from offset 0.
    pub async fn change_subtitle(
        &self,
        handle: &PlaybackHandle,
        subtitle: Option<u32>,
        bitmap: BitmapSubtitles,
        position: Duration,
    ) -> Result<PlaybackChange, SdkError> {
        let request = Request::ChangeSubtitle {
            handle: handle.clone(),
            subtitle,
            bitmap,
            position_ms: millis(position),
            // The playback's own, as it started.
            display: None,
        };
        match self.call(request).await? {
            Reply::PlaybackChanged { plan, size, epoch } => {
                Ok(PlaybackChange { plan, size, epoch })
            }
            other => Err(unexpected(other)),
        }
    }

    /// `quality` as the wire says it: `Auto` with the link's rate now.
    fn wire_quality(&self, quality: PlaybackQuality) -> Quality {
        match quality {
            PlaybackQuality::Auto => Quality::Auto {
                link_bitrate: self.link_rate(),
            },
            PlaybackQuality::Bitrate(max_bitrate) => Quality::Bitrate { max_bitrate },
            PlaybackQuality::Original => Quality::Original,
        }
    }

    /// Plan a playing playback again for the quality the viewer chose
    /// now (P6-06). A plan that changes restarts at `position` as a new
    /// epoch: read it from offset 0, as after [`Session::change_subtitle`].
    /// One that is the same comes back as it is.
    pub async fn change_quality(
        &self,
        handle: &PlaybackHandle,
        quality: PlaybackQuality,
        bitmap: BitmapSubtitles,
        position: Duration,
    ) -> Result<PlaybackChange, SdkError> {
        self.quality_request(handle, self.wire_quality(quality), bitmap, position)
            .await
    }

    /// Tell the server the link carries only `link_bitrate` bits per
    /// second now (P6-06): an `Auto` playback is planned again for it, as
    /// [`Session::change_quality`] plans one.
    pub async fn limit_playback(
        &self,
        handle: &PlaybackHandle,
        link_bitrate: u64,
        bitmap: BitmapSubtitles,
        position: Duration,
    ) -> Result<PlaybackChange, SdkError> {
        let quality = Quality::Auto {
            link_bitrate: Some(link_bitrate),
        };
        self.quality_request(handle, quality, bitmap, position)
            .await
    }

    async fn quality_request(
        &self,
        handle: &PlaybackHandle,
        quality: Quality,
        bitmap: BitmapSubtitles,
        position: Duration,
    ) -> Result<PlaybackChange, SdkError> {
        let request = Request::ChangeQuality {
            handle: handle.clone(),
            quality,
            bitmap,
            position_ms: millis(position),
            // The playback's own, as it started.
            display: None,
        };
        match self.call(request).await? {
            Reply::PlaybackChanged { plan, size, epoch } => {
                Ok(PlaybackChange { plan, size, epoch })
            }
            other => Err(unexpected(other)),
        }
    }

    /// End a playback now, and its encode. Otherwise it ends when it has
    /// not been read for a while.
    pub async fn stop_playback(&self, handle: &PlaybackHandle) -> Result<(), SdkError> {
        self.done(Request::StopPlayback {
            handle: handle.clone(),
        })
        .await
    }

    /// An HLS playback's playlist (P2-05). The media playlist's segment
    /// URIs (`seg3.m4s`, `init.mp4`) name what [`Session::read_segment`]
    /// reads: 3, and 0. A ladder's multivariant playlist (PX-21) names
    /// each rung's media playlist `v{n}/media.m3u8`: `variant` is that `n`
    /// for a media playlist, 0 for one encode and the other kinds; under
    /// `v{n}/` the segments are rung `n`'s.
    pub async fn manifest(
        &self,
        handle: &PlaybackHandle,
        kind: ManifestKind,
        variant: u32,
    ) -> Result<String, SdkError> {
        let request = Request::GetManifest {
            handle: handle.clone(),
            kind,
            variant,
        };
        match self.call(request).await? {
            Reply::Manifest { text } => Ok(text),
            other => Err(unexpected(other)),
        }
    }

    /// Up to `length` bytes of a converted playback's output from
    /// `offset`, for its `epoch`. A read past what is encoded waits up to
    /// 10 s and may come back empty and not `complete`: ask again. Read
    /// until `complete` and `offset` reaches `size`.
    /// [`SdkError::PlaybackRestarted`] means a seek replaced `epoch`.
    pub async fn read_chunk(
        &self,
        handle: &PlaybackHandle,
        epoch: u32,
        offset: u64,
        length: u64,
    ) -> Result<Chunk, SdkError> {
        self.media(handle, epoch, None, 0, offset, length).await
    }

    /// As [`Session::read_chunk`], within HLS segment `n` (0 is the init
    /// segment) of rung `variant` (PX-21; 0 for one encode). A segment far
    /// from what is encoded, or of a rung not being encoded, restarts the
    /// encode there; the chunk's epoch says so.
    pub async fn read_segment(
        &self,
        handle: &PlaybackHandle,
        epoch: u32,
        variant: u32,
        n: u32,
        offset: u64,
        length: u64,
    ) -> Result<Chunk, SdkError> {
        self.media(handle, epoch, Some(n), variant, offset, length)
            .await
    }

    /// [`Session::read_chunk`], its bytes handed on as they arrive (PX-46).
    pub async fn read_chunk_stream(
        &self,
        handle: &PlaybackHandle,
        epoch: u32,
        offset: u64,
        length: u64,
    ) -> Result<ChunkStream, SdkError> {
        self.media_stream(handle, epoch, None, 0, offset, length)
            .await
    }

    /// [`Session::read_segment`], its bytes handed on as they arrive
    /// (PX-46): the answer (`sent`, `size`, `complete`) comes first, and a
    /// bridge can promise a player the length and start the body at once.
    pub async fn read_segment_stream(
        &self,
        handle: &PlaybackHandle,
        epoch: u32,
        variant: u32,
        n: u32,
        offset: u64,
        length: u64,
    ) -> Result<ChunkStream, SdkError> {
        self.media_stream(handle, epoch, Some(n), variant, offset, length)
            .await
    }

    async fn media_stream(
        &self,
        handle: &PlaybackHandle,
        epoch: u32,
        segment: Option<u32>,
        variant: u32,
        offset: u64,
        length: u64,
    ) -> Result<ChunkStream, SdkError> {
        let request = MediaRead {
            session: self.token.clone(),
            handle: handle.clone(),
            offset,
            length,
            epoch,
            segment,
            variant,
        };
        match self.read_media_open(&request).await? {
            (MediaReadResponse::Ok(reply), Some(body)) => Ok(ChunkStream {
                sent: reply.sent,
                size: reply.size,
                complete: reply.complete,
                epoch: reply.epoch,
                body: ChunkBody::Arriving(body),
                received: 0,
            }),
            (MediaReadResponse::Ok(_), None) => {
                Err(SdkError::Protocol("a media answer with no body".into()))
            }
            (MediaReadResponse::Err(e), _) => Err(self.failed(e).await),
        }
    }

    async fn media(
        &self,
        handle: &PlaybackHandle,
        epoch: u32,
        segment: Option<u32>,
        variant: u32,
        offset: u64,
        length: u64,
    ) -> Result<Chunk, SdkError> {
        let request = MediaRead {
            session: self.token.clone(),
            handle: handle.clone(),
            offset,
            length,
            epoch,
            segment,
            variant,
        };
        match self.read_media(&request, false).await? {
            (MediaReadResponse::Ok(reply), bytes) => Ok(Chunk {
                bytes,
                size: reply.size,
                complete: reply.complete,
                epoch: reply.epoch,
            }),
            (MediaReadResponse::Err(e), _) => Err(self.failed(e).await),
        }
    }

    /// One media read, asked again on a new connection if the first one
    /// died under it: a read changes nothing.
    async fn read_media(
        &self,
        request: &MediaRead,
        direct: bool,
    ) -> Result<(MediaReadResponse, Vec<u8>), SdkError> {
        let timeouts = self.client.inner.timeouts;
        let revision = self
            .client
            .playback_revision(self.server, &self.token, &request.handle)
            .await?;
        let metering = self.metering(direct);
        self.client
            .on_connection(self.server, Some(&self.token), true, |conn| {
                let metering = metering.clone();
                async move {
                    conn.read(
                        request,
                        revision,
                        timeouts.of_read(),
                        timeouts.request,
                        metering,
                    )
                    .await
                }
            })
            .await
    }

    /// [`Self::read_media`]'s answer, with its body to read as it arrives
    /// (PX-46). Asked again on a new connection if the first one died
    /// before the answer; once the body is under way, a lost connection is
    /// the body's error, and the caller reads on from where it stopped.
    async fn read_media_open(
        &self,
        request: &MediaRead,
    ) -> Result<(MediaReadResponse, Option<crate::connection::BodyStream>), SdkError> {
        let timeouts = self.client.inner.timeouts;
        let revision = self
            .client
            .playback_revision(self.server, &self.token, &request.handle)
            .await?;
        let metering = self.metering(false);
        self.client
            .on_connection(self.server, Some(&self.token), true, |conn| {
                let metering = metering.clone();
                async move {
                    conn.read_open(
                        request,
                        revision,
                        timeouts.of_read(),
                        timeouts.request,
                        metering,
                    )
                    .await
                }
            })
            .await
    }

    /// How a read is metered: a direct read whole; a converted one (an HLS
    /// segment, a chunk) from its first byte, the encode's time left out
    /// (PX-46).
    fn metering(&self, direct: bool) -> crate::connection::Metering {
        let meter = self.client.link(self.server).meter();
        if direct {
            crate::connection::Metering::Whole(meter)
        } else {
            crate::connection::Metering::FromFirstByte(meter)
        }
    }

    /// How this user wants tracks chosen, on every device, and whether
    /// this server reads picture subtitles as text. When it does not, the
    /// picture subtitle choice (`subtitles.bitmap`) has no effect: show it
    /// disabled.
    pub async fn track_preferences(&self) -> Result<TrackPreferencesInfo, SdkError> {
        match self.call(Request::GetTrackPreferences).await? {
            Reply::TrackPreferences(info) => Ok(info),
            other => Err(unexpected(other)),
        }
    }

    pub async fn set_track_preferences(
        &self,
        preferences: &TrackPreferences,
    ) -> Result<(), SdkError> {
        self.done(Request::SetTrackPreferences {
            preferences: preferences.clone(),
        })
        .await
    }

    /// Keep the tracks of `selection` for the rest of `id`'s series (an
    /// episode) or for `id`. No subtitle in `selection` keeps them off.
    pub async fn set_track_override(
        &self,
        id: &MediaId,
        selection: Selection,
    ) -> Result<(), SdkError> {
        self.done(Request::SetTrackOverride {
            id: id.clone(),
            selection,
        })
        .await
    }

    /// Back to the preferences for `id`'s series or for `id`.
    pub async fn clear_track_override(&self, id: &MediaId) -> Result<(), SdkError> {
        self.done(Request::ClearTrackOverride { id: id.clone() })
            .await
    }

    /// Up to `length` bytes of a playback's file from `offset`; at most
    /// [`spoolframe_protocol::MAX_READ`], and none at the end of the file.
    /// Ask again from where a short read stopped. On
    /// [`SdkError::PlaybackExpired`], start the playback again.
    pub async fn read(
        &self,
        handle: &PlaybackHandle,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, SdkError> {
        let request = MediaRead {
            session: self.token.clone(),
            handle: handle.clone(),
            offset,
            length,
            epoch: 0,
            segment: None,
            variant: 0,
        };
        // Direct reads are timed whole; converted ones from their first
        // byte, as they wait on the encode (P6-06, PX-46).
        match self.read_media(&request, true).await? {
            (MediaReadResponse::Ok(_), bytes) => Ok(bytes),
            (MediaReadResponse::Err(e), _) => Err(self.failed(e).await),
        }
    }

    /// The selected external subtitle of a playback, whole. One the server
    /// reads as text (P2-07) is [`SdkError::SubtitleNotReady`] until it is
    /// done: play on, and ask again in a few seconds.
    ///
    /// The file is read in chunks (P2-11), so one larger than a frame
    /// arrives too. The result is never a mix of two files: when the file
    /// is written again or replaced on the server while it is read, the
    /// read starts again, once, and gives the new file. If it changes
    /// again during that, the call fails with
    /// [`SdkError::SubtitleChanged`]: read it again.
    pub async fn read_subtitle(&self, handle: &PlaybackHandle) -> Result<Subtitle, SdkError> {
        let session = self;
        read_whole(&PlaybackSubtitle { session, handle }).await
    }

    /// Segment `n` (from 1) of an HLS playback's subtitle rendition (P2-07):
    /// WebVTT, its URI `subs<n>.vtt` in [`ManifestKind::Subtitles`]'s
    /// playlist. [`SdkError::SubtitleNotReady`] while the server makes it.
    pub async fn read_subtitle_segment(
        &self,
        handle: &PlaybackHandle,
        n: u32,
    ) -> Result<Subtitle, SdkError> {
        let request = Request::ReadSubtitleSegment {
            handle: handle.clone(),
            n,
        };
        match self.call(request).await? {
            Reply::Subtitle { format, bytes } => Ok(Subtitle { format, bytes }),
            other => Err(unexpected(other)),
        }
    }

    /// The frame previews of a movie or episode, for scrubbing (P3-13).
    /// [`SdkError::PreviewsNotReady`] while the server makes them: show
    /// the time alone and ask again in a few seconds. [`SdkError::NoPreviews`]
    /// for an item that will have none. Fetch each of `sheets` once with
    /// [`Session::artwork`] at [`ArtworkSize::Original`], keep them for the
    /// item, and draw [`Previews::tile`]'s rectangle for a time.
    pub async fn previews(&self, id: &MediaId) -> Result<Previews, SdkError> {
        match self.call(Request::GetPreviews { id: id.clone() }).await? {
            Reply::Previews(previews) => Ok(previews),
            other => Err(unexpected(other)),
        }
    }

    /// An image, at `size`.
    /// Images larger than 64 MiB are refused before allocating their body.
    pub async fn artwork(&self, id: &ArtworkId, size: ArtworkSize) -> Result<Vec<u8>, SdkError> {
        let request = ArtworkRequest {
            session: self.token.clone(),
            id: id.clone(),
            size,
        };
        let request = &request;
        let timeouts = self.client.inner.timeouts;
        let artwork =
            self.client
                .on_connection(self.server, Some(&self.token), true, |conn| async move {
                    conn.artwork(request, timeouts.request, timeouts.request)
                        .await
                });
        match artwork.await? {
            (ArtworkResponse::Ok(_), bytes) => Ok(bytes),
            (ArtworkResponse::Err(e), _) => Err(self.failed(e).await),
        }
    }

    /// Where playback of `handle` has got to. Send it now and then while
    /// playing, and on pause and stop; the server decides what to keep.
    pub async fn update_progress(
        &self,
        handle: &PlaybackHandle,
        position: Duration,
    ) -> Result<(), SdkError> {
        // Keep the sequence cache entry alive until this report finishes,
        // so eviction cannot reuse a sequence while its request is in flight.
        let (sequence, _lease) = self
            .client
            .progress_sequence(self.server, &self.token, handle)
            .await?;
        let request = Request::UpdatePlaybackProgress {
            handle: handle.clone(),
            sequence,
            position_ms: millis(position),
        };
        self.done(request).await
    }

    pub async fn mark_watched(&self, id: &MediaId) -> Result<(), SdkError> {
        self.done(Request::MarkWatched { id: id.clone() }).await
    }

    pub async fn mark_unwatched(&self, id: &MediaId) -> Result<(), SdkError> {
        self.done(Request::MarkUnwatched { id: id.clone() }).await
    }

    /// "Remove from Continue watching" or "Next up" (P1-22): the card
    /// `id`, a movie or an episode, leaves this user's Home, and its
    /// progress and watched state stay. Only that card: the show's other
    /// episodes stay. Pass the card's own id, not its show's. Playing or
    /// marking it brings it back.
    pub async fn hide_from_continue(&self, id: &MediaId) -> Result<(), SdkError> {
        self.done(Request::HideFromContinueWatching { id: id.clone() })
            .await
    }

    /// Undo [`Session::hide_from_continue`].
    pub async fn unhide_from_continue(&self, id: &MediaId) -> Result<(), SdkError> {
        self.done(Request::UnhideFromContinueWatching { id: id.clone() })
            .await
    }

    async fn confirmed(&self, request: Request) -> Result<(), SdkError> {
        match self.call(request).await? {
            Reply::Confirmed => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    async fn done(&self, request: Request) -> Result<(), SdkError> {
        match self.call(request).await? {
            Reply::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[tokio::test]
    async fn sessions_kept_before_profiles_were_are_automatic() {
        let storage = crate::storage::MemoryStorage::default();
        let server = iroh::SecretKey::from_bytes(&[7; 32]).public();
        // As SDKs before saved profiles wrote it: every entry a token.
        let old = br#"[{"token": "abc", "username": "alice", "display_name": "Alice"}]"#;
        storage
            .set_secret(&keys::sessions(&server.to_string()), Some(old.to_vec()))
            .await
            .unwrap();
        let stored = load(&storage, &server).await.unwrap();
        let profiles: Vec<SavedProfile> = stored.iter().map(SavedProfile::from).collect();
        assert_eq!(
            profiles,
            [SavedProfile {
                username: "alice".into(),
                display_name: "Alice".into(),
                sign_in: ProfileSignIn::Automatic,
                method: None,
                portable: None,
            }]
        );
    }

    #[tokio::test]
    async fn a_store_from_before_sign_in_methods_keeps_its_profiles_without_one() {
        let storage = crate::storage::MemoryStorage::default();
        let server = iroh::SecretKey::from_bytes(&[7; 32]).public();
        // As SDKs before sign-in methods wrote it: a kept session and an
        // ended one, neither with a method.
        let old = br#"[
            {"token": "abc", "username": "alice", "display_name": "Alice"},
            {"token": null, "username": "bob", "display_name": "Bob"}
        ]"#;
        storage
            .set_secret(&keys::sessions(&server.to_string()), Some(old.to_vec()))
            .await
            .unwrap();
        let stored = load(&storage, &server).await.unwrap();
        let read: Vec<_> = stored
            .iter()
            .map(SavedProfile::from)
            .map(|p| (p.username, p.sign_in, p.method))
            .collect();
        assert_eq!(
            read,
            [
                ("alice".to_owned(), ProfileSignIn::Automatic, None),
                ("bob".to_owned(), ProfileSignIn::Password, None),
            ]
        );
    }

    #[tokio::test]
    async fn a_profile_s_sign_in_method_is_kept_in_the_store() {
        let storage = crate::storage::MemoryStorage::default();
        let server = iroh::SecretKey::from_bytes(&[7; 32]).public();
        let profiles = [
            (SignInMethod::Password, "alice"),
            (SignInMethod::Passkey, "bob"),
            (SignInMethod::Device, "carol"),
        ];
        let stored: Vec<_> = profiles
            .iter()
            .map(|&(method, name)| StoredSession {
                token: None,
                username: name.into(),
                display_name: name.into(),
                method: Some(method),
                portable: None,
            })
            .collect();
        save(&storage, &server, &stored).await.unwrap();
        let read: Vec<_> = load(&storage, &server)
            .await
            .unwrap()
            .iter()
            .map(SavedProfile::from)
            .map(|p| (p.method.unwrap(), p.username))
            .collect();
        let expected: Vec<_> = profiles.iter().map(|&(m, n)| (m, n.to_owned())).collect();
        assert_eq!(read, expected);
    }

    /// Six bytes of SubRip, version 1, of which `have` have arrived.
    fn chunks(have: &[u8]) -> Chunks {
        Chunks {
            format: SubtitleFormat::Srt,
            total: 6,
            version: 1,
            bytes: have.to_vec(),
        }
    }

    /// A chunk of that file.
    fn chunk(bytes: &[u8]) -> SubtitleChunk {
        SubtitleChunk {
            format: SubtitleFormat::Srt,
            bytes: bytes.to_vec(),
            total: 6,
            version: 1,
        }
    }

    fn protocol_error(result: Result<(), SdkError>) -> String {
        match result {
            Err(SdkError::Protocol(message)) => message,
            other => panic!("not a protocol error: {other:?}"),
        }
    }

    #[test]
    fn chunks_join_in_order_until_the_total() {
        let mut chunks = chunks(b"");
        chunks.add(chunk(b"abcd")).unwrap();
        assert!(!chunks.complete());
        chunks.add(chunk(b"ef")).unwrap();
        assert!(chunks.complete());
        assert_eq!(chunks.bytes, b"abcdef");
    }

    #[test]
    fn an_empty_subtitle_is_complete_with_its_empty_chunk() {
        let mut chunks = Chunks {
            total: 0,
            ..chunks(b"")
        };
        chunks
            .add(SubtitleChunk {
                total: 0,
                ..chunk(b"")
            })
            .unwrap();
        assert!(chunks.complete());
    }

    /// The server answered a chunk of a longer or a shorter file, and did
    /// not say that the file changed.
    #[test]
    fn a_total_that_changes_between_chunks_fails_the_read() {
        for total in [5, 7] {
            let error = protocol_error(chunks(b"abcd").add(SubtitleChunk {
                total,
                ..chunk(b"e")
            }));
            assert!(error.contains("changed"), "{error}");
        }
    }

    /// A read that would never end: the server says there is more, and
    /// sends none of it.
    #[test]
    fn a_chunk_with_no_bytes_before_the_total_fails_the_read() {
        let error = protocol_error(chunks(b"abcd").add(chunk(b"")));
        assert!(error.contains("no bytes"), "{error}");
    }

    #[test]
    fn a_chunk_that_passes_the_total_fails_the_read() {
        let error = protocol_error(chunks(b"abcd").add(chunk(b"efg")));
        assert!(error.contains("7 bytes"), "{error}");
    }

    /// `ChangeSubtitle` between two chunks: the rest is another subtitle.
    #[test]
    fn a_format_that_changes_between_chunks_fails_the_read() {
        let error = protocol_error(chunks(b"abcd").add(SubtitleChunk {
            format: SubtitleFormat::WebVtt,
            ..chunk(b"ef")
        }));
        assert!(error.contains("changed"), "{error}");
    }

    /// The server answered a chunk of another file of the same length,
    /// and did not say that the file changed.
    #[test]
    fn a_version_that_changes_between_chunks_fails_the_read() {
        let error = protocol_error(chunks(b"abcd").add(SubtitleChunk {
            version: 2,
            ..chunk(b"ef")
        }));
        assert!(error.contains("changed"), "{error}");
    }

    /// A stand-in server that answers four bytes a chunk. Its subtitle is
    /// `files[0]`, and the next of `files` from each request (counted
    /// from 0) that `replaced_at` names: the file was replaced just
    /// before the server read that request. A file's version is its place
    /// in `files`.
    struct Server {
        files: Vec<&'static [u8]>,
        replaced_at: Vec<usize>,
        /// Each request: its offset and version.
        requests: RefCell<Vec<(u64, Option<u64>)>>,
    }

    impl Server {
        fn new(files: &[&'static [u8]], replaced_at: &[usize]) -> Self {
            Server {
                files: files.to_vec(),
                replaced_at: replaced_at.to_vec(),
                requests: RefCell::default(),
            }
        }

        async fn read(&self) -> Result<Subtitle, SdkError> {
            read_whole(self).await
        }
    }

    impl SubtitleChunks for Server {
        async fn chunk(
            &self,
            offset: u64,
            version: Option<u64>,
        ) -> Result<SubtitleChunk, SdkError> {
            let request = self.requests.borrow().len();
            self.requests.borrow_mut().push((offset, version));
            let now = self.replaced_at.iter().filter(|at| **at <= request).count();
            if version.is_some_and(|read| read != now as u64) {
                return Err(SdkError::from_server(
                    spoolframe_protocol::Error::SubtitleChanged,
                ));
            }
            let file = self.files[now];
            let from = (offset as usize).min(file.len());
            let to = (from + 4).min(file.len());
            Ok(SubtitleChunk {
                format: SubtitleFormat::Srt,
                bytes: file[from..to].to_vec(),
                total: file.len() as u64,
                version: now as u64,
            })
        }
    }

    #[tokio::test]
    async fn each_chunk_after_the_first_is_asked_for_with_the_first_one_s_version() {
        let server = Server::new(&[b"abcdefghij"], &[]);
        let subtitle = server.read().await.unwrap();
        assert_eq!(subtitle.bytes, b"abcdefghij");
        assert_eq!(
            *server.requests.borrow(),
            [(0, None), (4, Some(0)), (8, Some(0))]
        );
    }

    /// The file is replaced by another of the same length after its first
    /// chunk is read: the result is the new file whole, and nothing of
    /// the old one.
    #[tokio::test]
    async fn a_subtitle_replaced_between_chunks_is_read_again_from_the_start() {
        let server = Server::new(&[b"aaaaaa", b"bbbbbb"], &[1]);
        let subtitle = server.read().await.unwrap();
        assert_eq!(subtitle.bytes, b"bbbbbb");
        assert_eq!(
            *server.requests.borrow(),
            [(0, None), (4, Some(0)), (0, None), (4, Some(1))]
        );
    }

    /// Replaced during the first read and again during the second: the
    /// read fails, and does not start a third time.
    #[tokio::test]
    async fn a_subtitle_replaced_during_both_reads_fails_the_read() {
        let server = Server::new(&[b"aaaaaa", b"bbbbbb", b"cccccc"], &[1, 3]);
        assert_eq!(server.read().await, Err(SdkError::SubtitleChanged));
        assert_eq!(server.requests.borrow().len(), 4);
    }
}
