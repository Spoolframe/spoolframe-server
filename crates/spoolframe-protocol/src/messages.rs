//! The messages of `spoolframe/client/1`.
//!
//! Every enum is internally tagged: `{"type": "ping"}`,
//! `{"type": "item", "id": "plex:movie:1"}`. Peers ignore fields they do
//! not know, and an error or event type they do not know decodes as
//! `Unknown`, so a newer peer can add both without breaking an older one.
//! A request type the server does not know is answered with
//! [`Error::Unsupported`].

use std::fmt;

use serde::{Deserialize, Serialize};
use spoolframe_types::{
    ClientCapabilities, Item, Library, LibraryId, MediaId, MediaTracks, Role, SubtitleFormat,
    TrackPreferences,
};

use crate::media::PlaybackHandle;
use crate::playback::{BitmapSubtitles, ChosenTracks, PlaybackPlan, Selection};

/// The protocol revision inside the `spoolframe/client/1` ALPN. Raised
/// when features are added; a breaking change is a new ALPN instead.
pub const PROTOCOL_VERSION: u32 = 1;

/// An opaque session token from a login (P1-12). It is only valid from
/// the endpoint that logged in.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

impl fmt::Debug for SessionId {
    // A session is a credential (bound to one endpoint, but still): keep it
    // out of logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionId(<redacted>)")
    }
}

/// One request, and the session it is made in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    /// `None` for the pre-auth requests. Several sessions may be used on
    /// one connection.
    #[serde(default)]
    pub session: Option<SessionId>,
    pub request: Request,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    // Pre-auth: the only requests served without a session (§20).
    /// The first request on every connection.
    Hello(Hello),
    AuthMethods,
    LoginPassword(LoginPassword),
    Ping,

    // Need a session.
    Libraries,
    Items {
        library: LibraryId,
        offset: u32,
        limit: u32,
    },
    Item {
        id: MediaId,
    },
    Search {
        query: String,
        limit: u32,
    },
    Tracks {
        id: MediaId,
    },
    /// Who this request's session belongs to.
    WhoAmI,
    /// End this request's session (and only it).
    Logout,
    /// The session user's live sessions on every device.
    ListSessions,
    /// How the server would play `id` for this client with these tracks.
    /// Without a selection the server chooses them from the user's
    /// preferences (P1-20) and says which in the reply.
    PlaybackPlan {
        id: MediaId,
        #[serde(default)]
        selection: Option<Selection>,
        /// How a picked bitmap subtitle is shown (P2-07).
        #[serde(default)]
        bitmap: BitmapSubtitles,
    },
    /// Plan `id` and, if it plays directly, open a playback session to
    /// read it through. Without a selection, as for `PlaybackPlan`.
    StartPlayback {
        id: MediaId,
        #[serde(default)]
        selection: Option<Selection>,
        /// Where to start, for a converted playback (P2-03): the encode
        /// begins there. Direct play reads from wherever the client likes.
        #[serde(default)]
        start_ms: Option<u64>,
        /// How a picked bitmap subtitle is shown (P2-07).
        #[serde(default)]
        bitmap: BitmapSubtitles,
    },
    /// End a playback now, and any encode for it.
    StopPlayback {
        handle: PlaybackHandle,
    },
    /// Restart a converted playback's encode at `position_ms` (P2-04):
    /// reads then name the new epoch and start at offset 0.
    SeekPlayback {
        handle: PlaybackHandle,
        position_ms: u64,
    },
    /// Set or change the session user's PIN (P2-08): 4 to 8 digits. A PIN
    /// only locks and unlocks a session a real sign-in made, so the
    /// unlocked session is all this needs. `Done`.
    SetPin {
        pin: String,
    },
    /// Remove the session user's PIN, and its requirement on every device.
    /// `Done`.
    ClearPin,
    /// Unlock this session on this connection with the user's PIN (P2-08).
    /// Wrong PINs are rate-limited; after 10 in a row the session on this
    /// device is revoked (`Unauthenticated`): sign in with the password.
    /// `Done`.
    UnlockProfile {
        pin: String,
    },
    /// The devices the session user has signed in from. `Devices`.
    ListDevices,
    /// Ask for the session user's PIN on `device` (an endpoint id from
    /// `Devices`; this device if left out), or stop asking. A locked
    /// session cannot ask this (nor anything but `WhoAmI` and
    /// `UnlockProfile`). `Done`.
    SetPinRequired {
        #[serde(default)]
        device: Option<String>,
        required: bool,
    },
    /// Pick another subtitle (or none) for a playing playback (P2-07): it
    /// is planned again with the same audio. A plan that plays directly
    /// only changes what `ReadSubtitle` gives; a converted one restarts its
    /// encode at `position_ms` as a new epoch, from offset 0 (a burned-in
    /// subtitle is in the picture). `PlaybackChanged`; `CannotPlay` leaves
    /// the playback as it was.
    ChangeSubtitle {
        handle: PlaybackHandle,
        subtitle: Option<u32>,
        #[serde(default)]
        bitmap: BitmapSubtitles,
        position_ms: u64,
    },
    /// A converted playback's HLS playlist (P2-05).
    GetManifest {
        handle: PlaybackHandle,
        kind: ManifestKind,
    },
    /// The selected external subtitle of a playback, whole. One the server
    /// reads as text (P2-07) answers `SubtitleNotReady` until it is done:
    /// ask again.
    ReadSubtitle {
        handle: PlaybackHandle,
    },
    /// Segment `n` (from 1) of an HLS playback's subtitle rendition (P2-07):
    /// `Subtitle { format: WebVtt, .. }`, the cues over that segment's time,
    /// timed with `X-TIMESTAMP-MAP` to the video. `SubtitleNotReady` while
    /// the subtitle is made.
    ReadSubtitleSegment {
        handle: PlaybackHandle,
        n: u32,
    },
    /// Where playback of a movie or episode has got to. Send it now and
    /// then while playing and on pause or stop; the server decides what is
    /// written, and when.
    UpdatePlaybackProgress {
        id: MediaId,
        position_ms: u64,
    },
    MarkWatched {
        id: MediaId,
    },
    MarkUnwatched {
        id: MediaId,
    },
    /// Remove `id` from the session user's Continue watching (P1-22),
    /// leaving its progress and watched state: a movie itself, an episode
    /// its show (from Next up too). Playing or marking it again brings it
    /// back. `Done`.
    HideFromContinueWatching {
        id: MediaId,
    },
    /// Undo a hide (an "Undo" toast). `Done`.
    UnhideFromContinueWatching {
        id: MediaId,
    },
    /// The session user's home screen.
    Home,
    /// How the session user wants tracks chosen (P1-20).
    GetTrackPreferences,
    /// Replace them, for every device of the user.
    SetTrackPreferences {
        preferences: TrackPreferences,
    },
    /// Use the tracks of `selection` in `id` for the rest of its series
    /// (an episode) or for it (anything else), by language and role. No
    /// subtitle in `selection` means subtitles off.
    SetTrackOverride {
        id: MediaId,
        selection: Selection,
    },
    /// Back to the preferences for `id`'s series or for it.
    ClearTrackOverride {
        id: MediaId,
    },

    /// A request type this build does not know; answered with
    /// [`Error::Unsupported`].
    #[serde(other)]
    Unknown,
}

impl Request {
    /// Served without a session.
    pub fn is_pre_auth(&self) -> bool {
        matches!(
            self,
            Request::Hello(_) | Request::AuthMethods | Request::LoginPassword(_) | Request::Ping
        )
    }
}

/// The client introduces itself. Capabilities (P1-07) will be added as an
/// optional field, which older servers ignore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u32,
    /// e.g. `spoolframe-linux`.
    pub client_name: String,
    pub client_version: String,
    /// What the device calls itself (a hostname, "Living room TV"); shown
    /// in session lists. Optional.
    #[serde(default)]
    pub device_name: Option<String>,
    /// What this device can play (P1-07). Without it every playback plan
    /// is `Unsupported`.
    #[serde(default)]
    pub capabilities: Option<ClientCapabilities>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginPassword {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for LoginPassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginPassword")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// How a user can log in to this server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    Password,
    /// A method this build does not know.
    #[serde(other)]
    Unknown,
}

/// The answer to one request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[expect(
    clippy::large_enum_variant,
    reason = "built once per request and encoded at once; boxing would only add noise"
)]
pub enum Response {
    Ok(Reply),
    Err(Error),
}

impl From<Result<Reply, Error>> for Response {
    fn from(result: Result<Reply, Error>) -> Self {
        match result {
            Ok(reply) => Response::Ok(reply),
            Err(error) => Response::Err(error),
        }
    }
}

impl From<Response> for Result<Reply, Error> {
    fn from(response: Response) -> Self {
        match response {
            Response::Ok(reply) => Ok(reply),
            Response::Err(error) => Err(error),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    ServerInfo(ServerInfo),
    AuthMethods {
        methods: Vec<AuthMethod>,
    },
    LoggedIn {
        session: SessionId,
    },
    Pong,
    Libraries {
        libraries: Vec<Library>,
    },
    Items {
        items: Vec<Item>,
        total: u32,
    },
    Item {
        item: Item,
        children: Vec<Item>,
    },
    Search {
        items: Vec<Item>,
    },
    Tracks(MediaTracks),
    WhoAmI(UserSummary),
    LoggedOut,
    Sessions {
        sessions: Vec<SessionInfo>,
    },
    PlaybackPlan {
        plan: PlaybackPlan,
        /// The tracks the server chose, when the request had no selection.
        #[serde(default)]
        chosen: Option<ChosenTracks>,
    },
    PlaybackStarted {
        handle: PlaybackHandle,
        /// Always `Direct` in Phase 1.
        plan: PlaybackPlan,
        /// The size of the file the handle reads.
        size: u64,
        /// The tracks the server chose, when the request had no selection.
        #[serde(default)]
        chosen: Option<ChosenTracks>,
        /// The encode reads name; 0 for direct play.
        #[serde(default)]
        epoch: u32,
    },
    /// A seek restarted the encode: reads name this epoch from offset 0.
    PlaybackSeeked {
        epoch: u32,
    },
    /// The session user's devices (P2-08), most recently seen first.
    Devices {
        devices: Vec<DeviceInfo>,
    },
    /// The playback's new plan after `ChangeSubtitle`: read `epoch` (0 for
    /// direct play), whose size is `size` (0 while a conversion writes it).
    PlaybackChanged {
        plan: PlaybackPlan,
        size: u64,
        epoch: u32,
    },
    /// An HLS playlist, as text.
    Manifest {
        text: String,
    },
    Subtitle {
        format: SubtitleFormat,
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
    },
    /// The request was carried out; there is nothing to return.
    Done,
    Home(HomeScreen),
    TrackPreferences(TrackPreferences),
}

/// Which HLS playlist (P2-05).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestKind {
    /// The top playlist, naming the variant.
    Multivariant,
    /// The variant's segments.
    Media,
    /// The subtitle rendition's WebVTT segments (P2-07), read with
    /// `ReadSubtitleSegment`; there when the plan has a subtitle.
    Subtitles,
    #[serde(other)]
    Unknown,
}

/// Rows for a home screen. Every item carries the user's playback state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomeScreen {
    /// Started and not finished, most recent first.
    pub continue_watching: Vec<Item>,
    /// For each recently watched show, its next unwatched episode.
    pub next_up: Vec<Item>,
    pub recently_added: Vec<Item>,
}

/// A session's user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserSummary {
    /// Opaque; stable for the user on this server.
    pub user_id: String,
    pub username: String,
    /// What to show; the username when the user has no display name.
    pub display_name: String,
    /// What the user may do (P2-09).
    pub role: Role,
    /// The session needs the user's PIN on this connection before anything
    /// but `WhoAmI` and `UnlockProfile` (P2-08).
    #[serde(default)]
    pub locked: bool,
}

/// One of a user's devices (P2-08).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// Hex `EndpointId`: what `SetPinRequired { device }` takes.
    pub endpoint_id: String,
    /// What the device calls itself.
    #[serde(default)]
    pub name: Option<String>,
    /// Unix seconds.
    pub last_seen_at: i64,
    /// The user's PIN is asked for on it.
    pub pin_required: bool,
    /// The device this request came from.
    pub this_device: bool,
}

/// One live session, without its token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// The device's name; `None` for an admin API session or an unnamed
    /// device.
    pub device_name: Option<String>,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds.
    pub last_used_at: i64,
    /// The session this request was made in.
    pub current: bool,
}

/// Who the server is. `endpoint_id` is what the client pins (P1-13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    /// Hex, as `spoolframe-server id` prints it.
    pub endpoint_id: String,
    pub protocol_version: u32,
    pub server_version: String,
    pub auth_methods: Vec<AuthMethod>,
}

/// Why a request failed. Typed, so clients act on the kind, not on text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Error {
    /// The connection's first request was not `Hello`.
    #[error("send Hello first")]
    HandshakeRequired,
    /// The request needs a session, and none valid for this endpoint was
    /// given.
    #[error("not authenticated")]
    Unauthenticated,
    /// The server does not know this request type.
    #[error("unsupported request {request:?}")]
    Unsupported { request: String },
    #[error("malformed request: {message}")]
    Malformed { message: String },
    #[error("{what} not found")]
    NotFound { what: String },
    /// Wrong username or password; deliberately the same for both.
    #[error("invalid credentials")]
    InvalidCredentials,
    /// Too many failed logins for this username or from this endpoint.
    #[error("too many failed logins; retry in {retry_after_ms} ms")]
    RateLimited { retry_after_ms: u64 },
    /// The client cannot play this selection; one reason per stream, as
    /// in [`PlaybackPlan::Unsupported`].
    #[error("cannot play: {}", reasons.join("; "))]
    CannotPlay { reasons: Vec<String> },
    /// The playback handle is unknown or has been idle too long. Call
    /// `StartPlayback` again and go on from where playback was.
    #[error("the playback has expired; start it again")]
    PlaybackExpired,
    /// The server is converting as many files as it may at once.
    #[error("the server is busy converting; try again later")]
    Busy,
    /// The encode for this playback failed; `reason` is ffmpeg's.
    #[error("the conversion failed: {reason}")]
    PlaybackFailed { reason: String },
    /// The read names an encode that a seek replaced; read `epoch` from
    /// offset 0.
    #[error("the playback restarted at a seek (epoch {epoch})")]
    PlaybackRestarted { epoch: u32 },
    /// The subtitle is still being read as text (P2-07); `done_percent` of
    /// its cues are read. Ask again in a few seconds.
    #[error("the subtitle is still being read ({done_percent}% done)")]
    SubtitleNotReady { done_percent: u8 },
    /// The session's profile is locked on this connection: `UnlockProfile`
    /// with the user's PIN first (P2-08).
    #[error("this profile is locked; enter its PIN")]
    ProfileLocked,
    /// A value the request carries is not acceptable (a PIN that is not 4
    /// to 8 digits, say); the message says why.
    #[error("{message}")]
    InvalidInput { message: String },
    /// Something the server depends on (Plex, a media mount) is down.
    #[error("unavailable: {message}")]
    Unavailable { message: String },
    #[error("internal server error")]
    Internal,
    /// An error type this build does not know.
    #[serde(other)]
    #[error("unknown error")]
    Unknown,
}

/// Something the server tells the client without being asked, on the
/// events stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// The server is stopping; the connection will close.
    ShuttingDown,
    /// An event type this build does not know; ignore it.
    #[serde(other)]
    Unknown,
}
