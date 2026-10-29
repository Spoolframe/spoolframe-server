//! Playlists: a user's playlists, the ones shared with them and the
//! server-wide ones; editing, pinning and playing them. Changes made
//! elsewhere (another device, another user) come as [`PlaylistEvent`]s
//! from [`crate::Client::playlist_events`].
//!
//! Liked and All: hearts on tracks and albums, and the two built-in
//! playlists, listed first while the server has a music
//! library, each summary's `builtin` saying which.
//!
//! Sharing by name: whom a playlist can be shared with
//! ([`Session::share_candidates`]), and the names of a playlist's owner
//! (`PlaylistSummary::owner_name`, and an item's
//! `PlaylistRef::owner_name`) and of who it is shared with
//! (`PlaylistShare::display_name`, `kind`).
//!
//! Covers: each summary's `artwork`, fetched with
//! [`Session::artwork`] as any artwork is (the uploaded cover, else a
//! mosaic of its first four covers, else a placeholder; Liked's and All's
//! fixed tiles), and `custom_art`; [`Session::set_playlist_art`] uploads
//! a cover, and [`Session::remove_playlist_art`] takes it away.

pub mod view;

use spoolframe_protocol::playlists::{
    EntryId, Kinds, PinNote, PlaySettings, PlaylistEdit, PlaylistEntry, PlaylistNext, PlaylistPage,
    PlaylistSummary, ShareCandidatePage, Visibility,
};
use spoolframe_protocol::resource::{ResourceMetadata, ResourceRequest, ResourceResponse};
use spoolframe_protocol::{Reply, Request};
use spoolframe_types::ArtworkId;
use spoolframe_types::MediaId;

use crate::session::unexpected;
use crate::{SdkError, Session};

/// Every playlist the user sees ([`Session::playlists`]), and the notes of
/// pinned ones that left their Pinned: show each once, then
/// [`Session::dismiss_pin_notes`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PlaylistList {
    pub playlists: Vec<PlaylistSummary>,
    pub pin_notes: Vec<PinNote>,
}

/// What [`Session::edit_playlist`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PlaylistEdited {
    /// The playlist's revision now.
    pub revision: u64,
    /// The entries the edit's adds made, in its order: one an episode for
    /// a show added. `None` from a server older than the field: read the
    /// playlist again to find them.
    pub added: Option<Vec<EntryId>>,
}

/// What [`Session::update_playlist`] changes; `None` leaves it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PlaylistUpdate {
    pub name: Option<String>,
    /// Only ever wider than now.
    pub kinds: Option<Kinds>,
    pub visibility: Option<Visibility>,
}

/// A playlist change a server told this device of. Read again what is
/// shown, for any user signed in on that server.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PlaylistEvent {
    /// The server's key.
    pub server: String,
    pub change: PlaylistChange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum PlaylistChange {
    /// A playlist the user sees was made or changed.
    Changed { id: String, revision: u64 },
    /// A playlist the user saw is no longer theirs to see.
    Gone { id: String },
    /// Their Pinned, or its notes, changed: list them again.
    PinsChanged,
}

impl Session {
    /// Every playlist the user sees: theirs, shared with them, and
    /// server-wide, with whether and where each is pinned.
    pub async fn playlists(&self) -> Result<PlaylistList, SdkError> {
        use crate::catalog_cache::Value;
        self.cached(
            crate::CatalogKey::Playlists,
            |s| async move { s.playlists_fresh().await },
            |list| Value::Playlists(Box::new(list)),
            |value| match value {
                Value::Playlists(list) => Some(*list),
                _ => None,
            },
        )
        .await
    }

    async fn playlists_fresh(&self) -> Result<PlaylistList, SdkError> {
        match self.call(Request::ListPlaylists).await? {
            Reply::Playlists {
                playlists,
                pin_notes,
            } => Ok(PlaylistList {
                playlists,
                pin_notes,
            }),
            other => Err(unexpected(other)),
        }
    }

    /// Playlist `id` and a page of its entries, from `offset`, at most
    /// `limit` (200), with the user's place and settings.
    pub async fn playlist(
        &self,
        id: &str,
        offset: u32,
        limit: u32,
    ) -> Result<PlaylistPage, SdkError> {
        use crate::catalog_cache::Value;
        let asked = id.to_owned();
        self.cached(
            crate::CatalogKey::Playlist {
                id: id.into(),
                offset,
                limit,
            },
            move |s| async move { s.playlist_fresh(&asked, offset, limit).await },
            |page| Value::Playlist(Box::new(page)),
            |value| match value {
                Value::Playlist(page) => Some(*page),
                _ => None,
            },
        )
        .await
    }

    async fn playlist_fresh(
        &self,
        id: &str,
        offset: u32,
        limit: u32,
    ) -> Result<PlaylistPage, SdkError> {
        let request = Request::GetPlaylist {
            id: id.into(),
            offset,
            limit,
        };
        match self.call(request).await? {
            Reply::Playlist(page) => Ok(page),
            other => Err(unexpected(other)),
        }
    }

    /// Make a playlist the user owns. `ServerWide` is an admin's
    /// ([`SdkError::AdminRequired`]).
    pub async fn create_playlist(
        &self,
        name: &str,
        kinds: Kinds,
        visibility: Visibility,
    ) -> Result<PlaylistSummary, SdkError> {
        let request = Request::CreatePlaylist {
            name: name.into(),
            kinds,
            visibility,
        };
        match self.call(request).await? {
            Reply::PlaylistInfo(summary) => Ok(summary),
            other => Err(unexpected(other)),
        }
    }

    /// Rename a playlist, widen its kinds, or change who sees it. Its
    /// owner's, or an admin's for a server-wide one; anyone else gets
    /// [`SdkError::PlaylistReadOnly`].
    pub async fn update_playlist(
        &self,
        id: &str,
        update: PlaylistUpdate,
    ) -> Result<PlaylistSummary, SdkError> {
        let request = Request::UpdatePlaylist {
            id: id.into(),
            name: update.name,
            kinds: update.kinds,
            visibility: update.visibility,
        };
        match self.call(request).await? {
            Reply::PlaylistInfo(summary) => Ok(summary),
            other => Err(unexpected(other)),
        }
    }

    /// Change a playlist's entries, as seen at `revision`: its new
    /// revision, and the entries added. [`SdkError::PlaylistConflict`] when
    /// someone changed the same entries since: "Someone changed this
    /// playlist; reload".
    pub async fn edit_playlist(
        &self,
        id: &str,
        revision: u64,
        edits: Vec<PlaylistEdit>,
    ) -> Result<PlaylistEdited, SdkError> {
        let request = Request::EditPlaylist {
            id: id.into(),
            revision,
            edits,
        };
        match self.call(request).await? {
            Reply::PlaylistEdited { revision, added } => Ok(PlaylistEdited { revision, added }),
            other => Err(unexpected(other)),
        }
    }

    /// Give playlist `id` the picture in `bytes` as its cover: a
    /// JPEG, PNG, WebP or HEIC of at most
    /// [`spoolframe_protocol::playlists::MAX_ART_BYTES`] and
    /// [`spoolframe_protocol::playlists::MAX_ART_PIXELS`]. Its new
    /// `artwork`. Sent on a stream of its own after the request, not in a
    /// frame. Whoever may edit its entries may; anyone else, and anyone
    /// for Liked and All, gets [`SdkError::NotAllowed`]; a picture past
    /// the limits [`SdkError::ImageTooLarge`], one the server cannot
    /// decode [`SdkError::ImageUnreadable`]. Every user who sees the
    /// playlist is told it changed.
    pub async fn set_playlist_art(&self, id: &str, bytes: Vec<u8>) -> Result<ArtworkId, SdkError> {
        let request = ResourceRequest::SetPlaylistArt {
            session: self.token.clone(),
            id: id.into(),
            length: bytes.len() as u64,
        };
        let (request, bytes) = (&request, &bytes);
        // The picture's way there and its decode, a few seconds at most.
        let limit = self.client.inner.timeouts.request + std::time::Duration::from_secs(60);
        let uploaded = self
            .client
            .on_connection(self.server, Some(&self.token), false, |conn| async move {
                conn.upload(request, bytes, limit).await
            })
            .await?;
        match uploaded {
            ResourceResponse::Ready {
                metadata: ResourceMetadata::PlaylistArt { artwork },
                ..
            } => Ok(artwork),
            ResourceResponse::Error { error } => Err(self.failed(error).await),
            other => Err(SdkError::Protocol(format!(
                "unexpected answer to an upload: {other:?}"
            ))),
        }
    }

    /// Take playlist `id`'s uploaded cover away: the playlist
    /// after, its `artwork` the mosaic again. Who may is as for
    /// [`Session::set_playlist_art`].
    pub async fn remove_playlist_art(&self, id: &str) -> Result<PlaylistSummary, SdkError> {
        let request = Request::RemovePlaylistArt { id: id.into() };
        match self.call(request).await? {
            Reply::PlaylistInfo(summary) => Ok(summary),
            other => Err(unexpected(other)),
        }
    }

    pub async fn delete_playlist(&self, id: &str) -> Result<(), SdkError> {
        let request = Request::DeletePlaylist { id: id.into() };
        match self.call(request).await? {
            Reply::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    /// Pin a playlist at `position` in the user's Pinned (0 first), or
    /// move it there. Every device of theirs sees it.
    pub async fn pin_playlist(&self, id: &str, position: u32) -> Result<(), SdkError> {
        let request = Request::PinPlaylist {
            id: id.into(),
            position,
        };
        match self.call(request).await? {
            Reply::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    pub async fn unpin_playlist(&self, id: &str) -> Result<(), SdkError> {
        let request = Request::UnpinPlaylist { id: id.into() };
        match self.call(request).await? {
            Reply::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    /// Forget the pin notes [`Session::playlists`] gave: they were shown.
    pub async fn dismiss_pin_notes(&self) -> Result<(), SdkError> {
        match self.call(Request::DismissPinNotes).await? {
            Reply::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    /// Start or go on playing a playlist: the entry to play, or `None`
    /// when nothing is left. From `from`, else from the user's place.
    /// Play the item as any other, skipping its credits by its markers,
    /// and ask [`Session::next_in_playlist`] when it ends.
    pub async fn play_playlist(
        &self,
        id: &str,
        from: Option<EntryId>,
        settings: PlaySettings,
    ) -> Result<Option<PlaylistNext>, SdkError> {
        let request = Request::PlayPlaylist {
            id: id.into(),
            from,
            shuffle: settings.shuffle,
            repeat: settings.repeat,
            skip_watched: settings.skip_watched,
        };
        next_of(self.call(request).await?)
    }

    /// What plays after entry `after` ended or was skipped: `None` at the
    /// end, which starts the next play at the top.
    pub async fn next_in_playlist(
        &self,
        id: &str,
        after: EntryId,
    ) -> Result<Option<PlaylistNext>, SdkError> {
        let request = Request::NextInPlaylist {
            id: id.into(),
            after,
        };
        next_of(self.call(request).await?)
    }
}

impl Session {
    /// Send `request` if the server has what comes up in a playlist.
    async fn upcoming_call(&self, request: Request) -> Result<Reply, SdkError> {
        self.call(request).await
    }

    /// What plays next in a playlist, without playing it: at most `limit`
    /// (100) entries after the user's place, each the one
    /// [`Session::next_in_playlist`] would give after the one before. The
    /// shuffled round's order while shuffling, else the playlist's; from
    /// the start again with repeat all; with repeat one, the entry playing
    /// alone. With no place, from the top. Unavailable entries, and with
    /// `skip_watched` watched ones, are passed.
    pub async fn upcoming_in_playlist(
        &self,
        id: &str,
        limit: u32,
    ) -> Result<Vec<PlaylistEntry>, SdkError> {
        let request = Request::UpcomingInPlaylist {
            id: id.into(),
            limit,
        };
        match self.upcoming_call(request).await? {
            Reply::PlaylistUpcoming { entries } => Ok(entries),
            other => Err(unexpected(other)),
        }
    }

    /// Keep `settings` as how the user plays a playlist, playing nothing:
    /// the place and the shuffled round stay. Shuffle turned on shuffles
    /// anew from the place, so the whole playlist is ahead; turned off, the
    /// playlist's order from the place.
    pub async fn set_playlist_settings(
        &self,
        id: &str,
        settings: PlaySettings,
    ) -> Result<(), SdkError> {
        let request = Request::SetPlaylistSettings {
            id: id.into(),
            shuffle: settings.shuffle,
            repeat: settings.repeat,
            skip_watched: settings.skip_watched,
        };
        match self.upcoming_call(request).await? {
            Reply::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }
}

impl Session {
    /// Send `request` if the server has Liked.
    async fn liked_call(&self, request: Request) -> Result<Reply, SdkError> {
        self.call(request).await
    }

    /// Like tracks or albums: their tracks not liked yet go to the top of
    /// the user's Liked, an album's in order. Liked as it is after. Only
    /// tracks and albums ([`SdkError::KindNotAllowed`] otherwise); 1 to
    /// 100 at a time.
    pub async fn like(&self, items: &[MediaId]) -> Result<PlaylistSummary, SdkError> {
        let request = Request::Like {
            items: items.to_vec(),
        };
        match self.liked_call(request).await? {
            Reply::PlaylistInfo(liked) => Ok(liked),
            other => Err(unexpected(other)),
        }
    }

    /// Take tracks, or every track of albums, out of the user's Liked.
    /// Liked as it is after.
    pub async fn unlike(&self, items: &[MediaId]) -> Result<PlaylistSummary, SdkError> {
        let request = Request::Unlike {
            items: items.to_vec(),
        };
        match self.liked_call(request).await? {
            Reply::PlaylistInfo(liked) => Ok(liked),
            other => Err(unexpected(other)),
        }
    }
}

impl Session {
    /// Whom playlist `id` can be shared with, a page from `offset` of at
    /// most `limit` (200), by display name, and how many match in all:
    /// every enabled account and granted portable user but the session
    /// user. With a `query`, only those whose display name or username
    /// holds it, in any case; empty, everyone. Only whoever manages the
    /// playlist may ask ([`SdkError::PlaylistReadOnly`] otherwise).
    pub async fn share_candidates(
        &self,
        id: &str,
        query: &str,
        offset: u32,
        limit: u32,
    ) -> Result<ShareCandidatePage, SdkError> {
        let request = Request::ListShareCandidates {
            id: id.into(),
            query: query.into(),
            offset,
            limit,
        };
        match self.call(request).await? {
            Reply::ShareCandidates(page) => Ok(page),
            other => Err(unexpected(other)),
        }
    }
}

impl Session {
    /// Import the user's Plex playlists once (each video or music
    /// playlist of their linked account becomes a personal one here), or
    /// with `dry_run` say what would be made. Imported twice, made twice.
    pub async fn import_plex_playlists(
        &self,
        dry_run: bool,
    ) -> Result<spoolframe_protocol::playlists::PlexImport, SdkError> {
        match self.call(Request::ImportPlexPlaylists { dry_run }).await? {
            Reply::PlexImport(import) => Ok(import),
            other => Err(unexpected(other)),
        }
    }
}

fn next_of(reply: Reply) -> Result<Option<PlaylistNext>, SdkError> {
    match reply {
        Reply::PlaylistItem(next) => Ok(Some(next)),
        Reply::PlaylistFinished => Ok(None),
        other => Err(unexpected(other)),
    }
}
