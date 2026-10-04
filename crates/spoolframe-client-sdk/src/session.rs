//! One user signed in on one server from this device (§15–16). Several
//! sessions share the device's connection to that server; every call says
//! which session it is made in.

use std::time::Duration;

use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use spoolframe_protocol::{
    ArtworkRequest, ArtworkResponse, ArtworkSize, BitmapSubtitles, ChosenTracks, DeviceInfo,
    DeviceSignInRequest, HomeScreen, ManifestKind, MediaRead, MediaReadResponse, PlaybackHandle,
    PlaybackPlan, Previews, Reply, Request, Response, SearchRequest, SearchResults, Selection,
    SessionId, SessionInfo, UserSummary,
};
use spoolframe_types::{
    ArtworkId, Item, Library, LibraryId, Marker, MediaId, MediaTracks, PersonId, SubtitleFormat,
    TrackPreferences,
};

use crate::client::Client;
use crate::error::SdkError;
use crate::storage::{Storage, StorageError, keys};

/// A session as it is stored: the token is the secret, the names are what
/// "Who's watching" shows before the server is asked.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct StoredSession {
    pub token: String,
    pub username: String,
    pub display_name: String,
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlaybackOptions {
    /// Where a converted playback's encode begins (P2-03). Direct play
    /// reads from wherever the app likes.
    pub start: Option<Duration>,
    /// How a picked bitmap subtitle (PGS) is shown (P2-07).
    pub bitmap: BitmapSubtitles,
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

    /// Who this session is, as of sign-in or the last [`Session::whoami`].
    pub fn user(&self) -> &UserSummary {
        &self.user
    }

    /// One request in this session. A session the server no longer knows
    /// is forgotten here, and the caller gets [`SdkError::SignInRequired`].
    async fn call(&self, request: Request) -> Result<Reply, SdkError> {
        let conn = self.client.connection(self.server).await?;
        match conn.call(Some(&self.token), request).await? {
            Response::Ok(reply) => Ok(reply),
            Response::Err(e) => Err(self.failed(e).await),
        }
    }

    async fn failed(&self, error: spoolframe_protocol::Error) -> SdkError {
        let error = SdkError::from_server(error);
        if error == SdkError::SignInRequired
            && let Err(e) = self.client.drop_session(self.server, &self.token).await
        {
            return e;
        }
        error
    }

    /// Ask the server who this session is.
    pub async fn whoami(&self) -> Result<UserSummary, SdkError> {
        match self.call(Request::WhoAmI).await? {
            Reply::WhoAmI(user) => Ok(user),
            other => Err(unexpected(other)),
        }
    }

    /// End this session on the server and forget it here.
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
            } => Ok(ItemDetail {
                item,
                children,
                markers,
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
        let request = Request::PlaybackPlan {
            id: id.clone(),
            selection,
            bitmap,
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
    /// server is converting all it may.
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

    /// Set or change this user's PIN: 4 to 8 digits. The PIN only locks and
    /// unlocks this session, which a real sign-in made, so the unlocked
    /// session is all it takes.
    pub async fn set_pin(&self, pin: &str) -> Result<(), SdkError> {
        self.done(Request::SetPin {
            pin: pin.to_owned(),
        })
        .await
    }

    /// Remove this user's PIN, and where it is asked for.
    pub async fn clear_pin(&self) -> Result<(), SdkError> {
        self.done(Request::ClearPin).await
    }

    /// The devices this user has signed in from, with where the PIN is
    /// asked for.
    pub async fn devices(&self) -> Result<Vec<DeviceInfo>, SdkError> {
        match self.call(Request::ListDevices).await? {
            Reply::Devices { devices } => Ok(devices),
            other => Err(unexpected(other)),
        }
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

    /// The device showing sign-in code `code` (P3-07; typed any way, or
    /// from a scanned [`crate::device_sign_in::parse_link`]), to ask "Sign
    /// in on 'Living room TV'?". A code that is not pending is
    /// [`SdkError::NotFound`]; too many of those [`SdkError::RateLimited`].
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
    /// device ([`SdkError::InvalidInput`]).
    pub async fn approve_device_sign_in(&self, code: &str) -> Result<(), SdkError> {
        self.done(Request::ApproveDeviceSignIn {
            code: code.to_owned(),
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
    /// reads: 3, and 0.
    pub async fn manifest(
        &self,
        handle: &PlaybackHandle,
        kind: ManifestKind,
    ) -> Result<String, SdkError> {
        let request = Request::GetManifest {
            handle: handle.clone(),
            kind,
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
        self.media(handle, epoch, None, offset, length).await
    }

    /// As [`Session::read_chunk`], within HLS segment `n` (0 is the init
    /// segment). A segment far from what is encoded restarts the encode
    /// there; the chunk's epoch says so.
    pub async fn read_segment(
        &self,
        handle: &PlaybackHandle,
        epoch: u32,
        n: u32,
        offset: u64,
        length: u64,
    ) -> Result<Chunk, SdkError> {
        self.media(handle, epoch, Some(n), offset, length).await
    }

    async fn media(
        &self,
        handle: &PlaybackHandle,
        epoch: u32,
        segment: Option<u32>,
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
        };
        let conn = self.client.connection(self.server).await?;
        match conn.read(&request).await? {
            (MediaReadResponse::Ok(reply), bytes) => Ok(Chunk {
                bytes,
                size: reply.size,
                complete: reply.complete,
                epoch: reply.epoch,
            }),
            (MediaReadResponse::Err(e), _) => Err(self.failed(e).await),
        }
    }

    /// How this user wants tracks chosen, on every device.
    pub async fn track_preferences(&self) -> Result<TrackPreferences, SdkError> {
        match self.call(Request::GetTrackPreferences).await? {
            Reply::TrackPreferences(preferences) => Ok(preferences),
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
        };
        let conn = self.client.connection(self.server).await?;
        match conn.read(&request).await? {
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
    pub async fn artwork(&self, id: &ArtworkId, size: ArtworkSize) -> Result<Vec<u8>, SdkError> {
        let request = ArtworkRequest {
            session: self.token.clone(),
            id: id.clone(),
            size,
        };
        let conn = self.client.connection(self.server).await?;
        match conn.artwork(&request).await? {
            (ArtworkResponse::Ok(_), bytes) => Ok(bytes),
            (ArtworkResponse::Err(e), _) => Err(self.failed(e).await),
        }
    }

    /// Where playback of `id` has got to. Send it now and then while
    /// playing, and on pause and stop; the server decides what to keep.
    pub async fn update_progress(&self, id: &MediaId, position: Duration) -> Result<(), SdkError> {
        let request = Request::UpdatePlaybackProgress {
            id: id.clone(),
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

    /// "Remove from Continue watching" (P1-22): `id` leaves this user's
    /// Continue watching, an episode's show leaves Next up too, and its
    /// progress and watched state stay. Playing or marking it brings it
    /// back.
    pub async fn hide_from_continue(&self, id: &MediaId) -> Result<(), SdkError> {
        self.done(Request::HideFromContinueWatching { id: id.clone() })
            .await
    }

    /// Undo [`Session::hide_from_continue`].
    pub async fn unhide_from_continue(&self, id: &MediaId) -> Result<(), SdkError> {
        self.done(Request::UnhideFromContinueWatching { id: id.clone() })
            .await
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
