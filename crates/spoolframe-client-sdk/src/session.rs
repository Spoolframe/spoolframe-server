//! One user signed in on one server from this device (§15–16). Several
//! sessions share the device's connection to that server; every call says
//! which session it is made in.

use std::time::Duration;

use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use spoolframe_protocol::{
    ArtworkRequest, ArtworkResponse, ArtworkSize, ChosenTracks, HomeScreen, MediaRead,
    MediaReadResponse, PlaybackHandle, PlaybackPlan, Reply, Request, Response, Selection,
    SessionId, SessionInfo, UserSummary,
};
use spoolframe_types::{
    ArtworkId, Item, Library, LibraryId, MediaId, MediaTracks, SubtitleFormat, TrackPreferences,
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
    /// The size of the file the handle reads.
    pub size: u64,
    /// The tracks the server chose, when none were asked for.
    pub chosen: Option<ChosenTracks>,
}

/// An external subtitle, whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subtitle {
    pub format: SubtitleFormat,
    pub bytes: Vec<u8>,
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
            Reply::Item { item, children } => Ok(ItemDetail { item, children }),
            other => Err(unexpected(other)),
        }
    }

    pub async fn search(&self, query: &str, limit: u32) -> Result<Vec<Item>, SdkError> {
        let request = Request::Search {
            query: query.to_owned(),
            limit,
        };
        match self.call(request).await? {
            Reply::Search { items } => Ok(items),
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
        let request = Request::PlaybackPlan {
            id: id.clone(),
            selection,
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
        let request = Request::StartPlayback {
            id: id.clone(),
            selection,
        };
        match self.call(request).await? {
            Reply::PlaybackStarted {
                handle,
                plan,
                size,
                chosen,
            } => Ok(Playback {
                handle,
                plan,
                size,
                chosen,
            }),
            other => Err(unexpected(other)),
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
        };
        let conn = self.client.connection(self.server).await?;
        match conn.read(&request).await? {
            (MediaReadResponse::Ok(_), bytes) => Ok(bytes),
            (MediaReadResponse::Err(e), _) => Err(self.failed(e).await),
        }
    }

    /// The selected external subtitle of a playback.
    pub async fn read_subtitle(&self, handle: &PlaybackHandle) -> Result<Subtitle, SdkError> {
        let request = Request::ReadSubtitle {
            handle: handle.clone(),
        };
        match self.call(request).await? {
            Reply::Subtitle { format, bytes } => Ok(Subtitle { format, bytes }),
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
            position_ms: u64::try_from(position.as_millis()).unwrap_or(u64::MAX),
        };
        self.done(request).await
    }

    pub async fn mark_watched(&self, id: &MediaId) -> Result<(), SdkError> {
        self.done(Request::MarkWatched { id: id.clone() }).await
    }

    pub async fn mark_unwatched(&self, id: &MediaId) -> Result<(), SdkError> {
        self.done(Request::MarkUnwatched { id: id.clone() }).await
    }

    async fn done(&self, request: Request) -> Result<(), SdkError> {
        match self.call(request).await? {
            Reply::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }
}
