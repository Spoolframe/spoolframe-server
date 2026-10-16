//! Playlists (PX-40, feature `spoolframe:playlists`): ordered
//! lists of movies, episodes and tracks a user makes, shares, pins and
//! plays.
//!
//! An entry is one place in a playlist, named by its [`EntryId`]: the same
//! item may be in a playlist more than once, as two entries. Edits name
//! entries, never items, so two editors moving the same film each move
//! their own.

use serde::{Deserialize, Serialize};
use spoolframe_types::{MediaId, MediaKind};

/// One place in a playlist. Never reused, in any playlist.
pub type EntryId = u64;

/// The kinds of item a playlist may hold, ticked when it is made. They
/// can change later only to allow more.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Kinds {
    pub movies: bool,
    pub episodes: bool,
    pub tracks: bool,
}

impl Kinds {
    /// Whether an item of `kind` may be added. A season or show adds its
    /// episodes, an album or artist its tracks: they are allowed when
    /// those are.
    pub fn allows(self, kind: MediaKind) -> bool {
        match kind {
            MediaKind::Movie => self.movies,
            MediaKind::Episode | MediaKind::Season | MediaKind::Show => self.episodes,
            MediaKind::Track | MediaKind::Album | MediaKind::Artist => self.tracks,
        }
    }

    /// Ticks none: not a playlist's kinds.
    pub fn is_empty(self) -> bool {
        !(self.movies || self.episodes || self.tracks)
    }

    /// Whether `to` keeps every kind these allow: the only change a
    /// playlist's kinds may make.
    pub fn widen_only(self, to: Kinds) -> bool {
        (!self.movies || to.movies)
            && (!self.episodes || to.episodes)
            && (!self.tracks || to.tracks)
    }
}

/// An item's kind, as `KindNotAllowed` names the one refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Movie,
    Show,
    Season,
    Episode,
    Artist,
    Album,
    Track,
    #[serde(other)]
    Unknown,
}

impl From<MediaKind> for ItemKind {
    fn from(kind: MediaKind) -> Self {
        match kind {
            MediaKind::Movie => ItemKind::Movie,
            MediaKind::Show => ItemKind::Show,
            MediaKind::Season => ItemKind::Season,
            MediaKind::Episode => ItemKind::Episode,
            MediaKind::Artist => ItemKind::Artist,
            MediaKind::Album => ItemKind::Album,
            MediaKind::Track => ItemKind::Track,
        }
    }
}

/// One change to a playlist's entries (`EditPlaylist`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlaylistEdit {
    /// Add `id` before entry `before`, or at the end when `None` (or when
    /// that entry is gone by the time the edit is redone on a newer
    /// revision). A movie, an episode or a track adds itself; a season its
    /// episodes, a show every episode in airing order (specials last), an
    /// album its tracks in disc and track order, an artist each album's in
    /// turn.
    Add {
        id: MediaId,
        #[serde(deserialize_with = "spoolframe_types::required::option")]
        before: Option<EntryId>,
    },
    /// Take the entry out.
    Remove { entry: EntryId },
    /// Put the entry before `before`, or at the end when `None` (or when
    /// that entry is gone by the time the edit is redone).
    Move {
        entry: EntryId,
        #[serde(deserialize_with = "spoolframe_types::required::option")]
        before: Option<EntryId>,
    },
    /// An edit this build does not know; refused with `InvalidInput`.
    #[serde(other)]
    Unknown,
}

impl PlaylistEdit {
    /// The existing entry it changes, if any: an add changes none.
    pub fn touches(&self) -> Option<EntryId> {
        match self {
            PlaylistEdit::Remove { entry } | PlaylistEdit::Move { entry, .. } => Some(*entry),
            PlaylistEdit::Add { .. } | PlaylistEdit::Unknown => None,
        }
    }
}

/// Who sees a playlist, as whoever manages it sets it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Visibility {
    /// Its owner alone.
    Personal,
    /// Its owner and these users. Listed only to whoever manages the
    /// playlist; anyone else sees it empty.
    Shared { with: Vec<PlaylistShare> },
    /// Every user of the server, portable guests included; admins edit
    /// it. Only an admin makes one.
    ServerWide,
    /// A kind this build does not know; refused with `InvalidInput`.
    #[serde(other)]
    Unknown,
}

/// One user a playlist is shared with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistShare {
    /// As `WhoAmI` gives it: `local:<uuid>`, or `portable:` and 64 hex
    /// digits.
    pub user_id: String,
    /// They may add, remove and move its entries; never rename it, change
    /// its kinds or sharing, or delete it.
    pub can_edit: bool,
}

/// How a playlist stands to whoever asks: what a client lists its
/// playlists apart by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaylistOwnerKind {
    /// Theirs, personal or shared by them.
    Mine,
    /// Someone else's, shared with them.
    SharedWithMe,
    /// Seen by every user.
    ServerWide,
    #[serde(other)]
    Unknown,
}

/// A playlist as one user sees it, without its entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistSummary {
    pub id: String,
    pub name: String,
    pub kinds: Kinds,
    pub owner_kind: PlaylistOwnerKind,
    pub visibility: Visibility,
    /// They may change its entries (`EditPlaylist`).
    pub can_edit: bool,
    /// They may rename it, change its kinds and sharing
    /// (`UpdatePlaylist`), and delete it.
    pub can_manage: bool,
    /// What an `EditPlaylist` names; raised by every change.
    pub revision: u64,
    /// Its place in their Pinned (0 first), or `None` when not pinned.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub pinned: Option<u32>,
}

/// A playlist holding an item, as the item's details list it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistRef {
    pub id: String,
    pub name: String,
    pub owner_kind: PlaylistOwnerKind,
}

/// One entry of a playlist, with its item as the catalog has it now and
/// the user's watch state on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistEntry {
    pub entry: EntryId,
    pub item: spoolframe_types::Item,
    /// Still in the library, but its file cannot be read now (a drive
    /// offline, a file moved): shown, counted, skipped when playing.
    pub unavailable: bool,
}

/// One page of a playlist's entries (`GetPlaylist`). Items the user may
/// not see are neither listed nor counted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistPage {
    pub playlist: PlaylistSummary,
    pub entries: Vec<PlaylistEntry>,
    /// Where `entries` start in the whole list.
    pub offset: u32,
    /// How many entries the whole list has.
    pub total: u32,
    /// How many of them can be played now: `total` less the unavailable.
    pub playable: u32,
    /// The user's place: the entry playing or last played, which
    /// `PlayPlaylist` without `from` goes on from; `None` to start at the
    /// top.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub place: Option<EntryId>,
    /// How the user last played it.
    pub settings: PlaySettings,
}

/// How a user plays a playlist, kept per user and playlist.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlaySettings {
    /// Play in a shuffled order: every playable entry once per round.
    pub shuffle: bool,
    pub repeat: Repeat,
    /// Pass entries whose item the user has watched.
    pub skip_watched: bool,
}

/// What plays again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Repeat {
    /// The end of the playlist ends it.
    #[default]
    Off,
    /// The same entry, over and over.
    One,
    /// The whole playlist: a new round after the last entry.
    All,
    /// A kind this build does not know: played as `Off`.
    #[serde(other)]
    Unknown,
}

/// The entry to play now (`PlayPlaylist`, `NextInPlaylist`), with its
/// item, the user's watch state on it (to resume where they were) and its
/// intro and credits markers (to skip the credits, PX-06).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistNext {
    pub entry: EntryId,
    pub item: spoolframe_types::Item,
    pub markers: Vec<spoolframe_types::Marker>,
}

/// What importing the user's Plex playlists made, or would make
/// (`ImportPlexPlaylists`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlexImport {
    /// One per Plex playlist with at least one item found here.
    pub made: Vec<PlexImported>,
    /// The items, or whole playlists, not found here, and why.
    pub unmapped: Vec<PlexUnmapped>,
    /// Plex servers that could not be read for the user, and why.
    pub notes: Vec<String>,
}

/// One Plex playlist imported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlexImported {
    /// Its name in Plex, and here.
    pub name: String,
    /// The playlist made; `None` on a dry run.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub playlist_id: Option<String>,
    /// How many entries it has.
    pub entries: u32,
}

/// Something of a Plex playlist not imported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlexUnmapped {
    /// The Plex playlist's name.
    pub playlist: String,
    /// The item's title; `None` for the whole playlist.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub title: Option<String>,
    pub reason: String,
}

/// A pinned playlist that left the user's Pinned: say so once, then
/// `DismissPinNotes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinNote {
    pub playlist_id: String,
    /// Its name when it left.
    pub name: String,
    pub reason: PinNoteReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinNoteReason {
    /// It is no longer shared with them.
    Unshared,
    /// It was deleted.
    Deleted,
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPISODES: Kinds = Kinds {
        movies: false,
        episodes: true,
        tracks: false,
    };

    #[test]
    fn an_episodes_playlist_takes_seasons_and_shows_but_no_film_or_album() {
        assert!(EPISODES.allows(MediaKind::Episode));
        assert!(EPISODES.allows(MediaKind::Season));
        assert!(EPISODES.allows(MediaKind::Show));
        assert!(!EPISODES.allows(MediaKind::Movie));
        assert!(!EPISODES.allows(MediaKind::Album));
    }

    #[test]
    fn kinds_widen_and_never_narrow() {
        let more = Kinds {
            movies: true,
            ..EPISODES
        };
        assert!(EPISODES.widen_only(more));
        assert!(EPISODES.widen_only(EPISODES));
        assert!(!more.widen_only(EPISODES));
    }

    #[test]
    fn an_edit_of_a_newer_build_reads_as_unknown() {
        let edit: PlaylistEdit = serde_json::from_str(r#"{"type":"shuffle_all"}"#).unwrap();
        assert_eq!(edit, PlaylistEdit::Unknown);
    }
}
