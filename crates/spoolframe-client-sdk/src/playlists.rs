//! Playlists (PX-40, feature `spoolframe:playlists`): a user's playlists,
//! the ones shared with them and the server-wide ones; editing, pinning
//! and playing them.
//!
//! Every call needs the feature on the session's server
//! ([`Session::features`]); without it the call is
//! [`SdkError::Unsupported`] and nothing is sent. Changes made elsewhere
//! (another device, another user) come as [`PlaylistEvent`]s from
//! [`crate::Client::playlist_events`].
//!
//! Liked and All (PX-43, feature `spoolframe:liked`): hearts on tracks and
//! albums, and the two built-in playlists, listed first, each summary's
//! `builtin` saying which. A server without the feature lists neither and
//! [`Session::like`] is [`SdkError::Unsupported`].
//!
//! Sharing by name (feature `spoolframe:playlist-sharing`): whom a
//! playlist can be shared with ([`Session::share_candidates`]), and the
//! names of a playlist's owner (`PlaylistSummary::owner_name`) and of who
//! it is shared with (`PlaylistShare::display_name`, `kind`). A server
//! without the feature sends no names, and the call is
//! [`SdkError::Unsupported`].

use spoolframe_protocol::features::{LAST_VIEWED, LIKED, PLAYLIST_SHARING, PLAYLISTS};
use spoolframe_protocol::playlists::{
    EntryId, Kinds, PinNote, PlaySettings, PlaylistEdit, PlaylistNext, PlaylistPage,
    PlaylistSummary, ShareCandidatePage, Visibility,
};
use spoolframe_protocol::{Reply, Request};
use spoolframe_types::MediaId;

use crate::{SdkError, Session};

/// The regular features a server has (PX-16) that this SDK uses, as the
/// session's `WhoAmI` listed them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Features {
    /// Playlists (PX-40).
    pub playlists: bool,
    /// Liked and All, and hearts (PX-43).
    pub liked: bool,
    /// When each item was last viewed: `PlaybackState::last_viewed_at` on
    /// Home's rows and an item's details. Data only, nothing to call.
    pub last_viewed: bool,
    /// Sharing by name: [`Session::share_candidates`], and the names of a
    /// playlist's owner and of who it is shared with.
    pub playlist_sharing: bool,
}

impl Features {
    /// What `names`, a server's list, has. A name this SDK does not know
    /// is ignored.
    pub(crate) fn of(names: &[String]) -> Self {
        Self {
            playlists: spoolframe_protocol::features::has(names, PLAYLISTS),
            liked: spoolframe_protocol::features::has(names, PLAYLISTS)
                && spoolframe_protocol::features::has(names, LIKED),
            last_viewed: spoolframe_protocol::features::has(names, LAST_VIEWED),
            playlist_sharing: spoolframe_protocol::features::has(names, PLAYLISTS)
                && spoolframe_protocol::features::has(names, PLAYLIST_SHARING),
        }
    }
}

/// Every playlist the user sees ([`Session::playlists`]), and the notes of
/// pinned ones that left their Pinned: show each once, then
/// [`Session::dismiss_pin_notes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistList {
    pub playlists: Vec<PlaylistSummary>,
    pub pin_notes: Vec<PinNote>,
}

/// What [`Session::update_playlist`] changes; `None` leaves it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaylistUpdate {
    pub name: Option<String>,
    /// Only ever wider than now.
    pub kinds: Option<Kinds>,
    pub visibility: Option<Visibility>,
}

/// A playlist change a server told this device of. Read again what is
/// shown, for any user signed in on that server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistEvent {
    /// The server's key.
    pub server: String,
    pub change: PlaylistChange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaylistChange {
    /// A playlist the user sees was made or changed.
    Changed { id: String, revision: u64 },
    /// A playlist the user saw is no longer theirs to see.
    Gone { id: String },
    /// Their Pinned, or its notes, changed: list them again.
    PinsChanged,
}

/// The reply was not the one the request has.
fn unexpected(reply: Reply) -> SdkError {
    SdkError::Protocol(format!("unexpected reply {reply:?}"))
}

impl Session {
    /// The regular features the server has that this SDK uses, as it
    /// listed them when this session was made or found.
    pub fn features(&self) -> Features {
        self.features
    }

    /// Send `request` if the server has playlists.
    async fn playlist_call(&self, request: Request) -> Result<Reply, SdkError> {
        if !self.features.playlists {
            return Err(SdkError::Unsupported {
                feature: PLAYLISTS.into(),
            });
        }
        self.call(request).await
    }

    /// Every playlist the user sees: theirs, shared with them, and
    /// server-wide, with whether and where each is pinned.
    pub async fn playlists(&self) -> Result<PlaylistList, SdkError> {
        match self.playlist_call(Request::ListPlaylists).await? {
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
        let request = Request::GetPlaylist {
            id: id.into(),
            offset,
            limit,
        };
        match self.playlist_call(request).await? {
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
        match self.playlist_call(request).await? {
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
        match self.playlist_call(request).await? {
            Reply::PlaylistInfo(summary) => Ok(summary),
            other => Err(unexpected(other)),
        }
    }

    /// Change a playlist's entries, as seen at `revision`: its new
    /// revision. [`SdkError::PlaylistConflict`] when someone changed the
    /// same entries since: "Someone changed this playlist; reload".
    pub async fn edit_playlist(
        &self,
        id: &str,
        revision: u64,
        edits: Vec<PlaylistEdit>,
    ) -> Result<u64, SdkError> {
        let request = Request::EditPlaylist {
            id: id.into(),
            revision,
            edits,
        };
        match self.playlist_call(request).await? {
            Reply::PlaylistEdited { revision } => Ok(revision),
            other => Err(unexpected(other)),
        }
    }

    pub async fn delete_playlist(&self, id: &str) -> Result<(), SdkError> {
        let request = Request::DeletePlaylist { id: id.into() };
        match self.playlist_call(request).await? {
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
        match self.playlist_call(request).await? {
            Reply::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    pub async fn unpin_playlist(&self, id: &str) -> Result<(), SdkError> {
        let request = Request::UnpinPlaylist { id: id.into() };
        match self.playlist_call(request).await? {
            Reply::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    /// Forget the pin notes [`Session::playlists`] gave: they were shown.
    pub async fn dismiss_pin_notes(&self) -> Result<(), SdkError> {
        match self.playlist_call(Request::DismissPinNotes).await? {
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
        next_of(self.playlist_call(request).await?)
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
        next_of(self.playlist_call(request).await?)
    }
}

impl Session {
    /// Send `request` if the server has Liked (PX-43).
    async fn liked_call(&self, request: Request) -> Result<Reply, SdkError> {
        if !self.features.liked {
            return Err(SdkError::Unsupported {
                feature: LIKED.into(),
            });
        }
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
        if !self.features.playlist_sharing {
            return Err(SdkError::Unsupported {
                feature: PLAYLIST_SHARING.into(),
            });
        }
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
        match self
            .playlist_call(Request::ImportPlexPlaylists { dry_run })
            .await?
        {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_s_list_names_the_features_this_sdk_uses() {
        assert!(Features::of(&[PLAYLISTS.to_owned()]).playlists);
        assert!(!Features::of(&[PLAYLISTS.to_owned()]).liked);
        assert!(Features::of(&[PLAYLISTS.to_owned(), LIKED.to_owned()]).liked);
        // Liked is a part of playlists: without them, none.
        assert!(!Features::of(&[LIKED.to_owned()]).liked);
        assert!(Features::of(&[LAST_VIEWED.to_owned()]).last_viewed);
        assert!(
            Features::of(&[PLAYLISTS.to_owned(), PLAYLIST_SHARING.to_owned()]).playlist_sharing
        );
        assert!(!Features::of(&[PLAYLIST_SHARING.to_owned()]).playlist_sharing);
        assert!(!Features::of(&["spoolframe:other".to_owned()]).playlists);
        assert_eq!(Features::of(&[]), Features::default());
    }
}
