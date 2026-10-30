//! Playlists (feature `spoolframe:playlists`): ordered
//! lists of movies, episodes and tracks a user makes, shares, pins and
//! plays.
//!
//! An entry is one place in a playlist, named by its [`EntryId`]: the same
//! item may be in a playlist more than once, as two entries. Edits name
//! entries, never items, so two editors moving the same film each move
//! their own.

use serde::{Deserialize, Serialize};
use spoolframe_types::{ArtworkId, MediaId, MediaKind};

/// One place in a playlist. Never reused, in any playlist.
pub type EntryId = u64;

/// The largest cover a `SetPlaylistArt` takes, in bytes: past it,
/// `ImageTooLarge`, and the bytes are not read.
pub const MAX_ART_BYTES: u64 = 20 << 20;

/// The most pixels a cover may have: past it, `ImageTooLarge`,
/// from the picture's header, before it is decoded.
pub const MAX_ART_PIXELS: u64 = 40_000_000;

/// The kinds of item a playlist may hold, ticked when it is made. They
/// can change later only to allow more.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct Kinds {
    #[cfg_attr(feature = "uniffi", uniffi(default = false))]
    pub movies: bool,
    #[cfg_attr(feature = "uniffi", uniffi(default = false))]
    pub episodes: bool,
    #[cfg_attr(feature = "uniffi", uniffi(default = false))]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PlaylistShare {
    /// As `WhoAmI` gives it: `local:<uuid>`, or `portable:` and 64 hex
    /// digits.
    pub user_id: String,
    /// They may add, remove and move its entries; never rename it, change
    /// its kinds or sharing, or delete it.
    pub can_edit: bool,
    /// What they are shown as; `None` for someone no longer here (shown
    /// by `user_id`). Read by no request: a share is named by `user_id`
    /// alone, and a client sends `null`.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub display_name: Option<String>,
    /// Whose account they are. Read by no request: a client sends `null`.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub kind: Option<UserKind>,
}

impl PlaylistShare {
    /// A share with `user_id`, as a request names it: no name, no kind.
    pub fn new(user_id: impl Into<String>, can_edit: bool) -> Self {
        Self {
            user_id: user_id.into(),
            can_edit,
            display_name: None,
            kind: None,
        }
    }
}

/// Whose account a user is (`spoolframe:playlist-sharing`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum UserKind {
    /// An account on this server: `local:<uuid>`.
    Local,
    /// A portable user granted access here: `portable:` and 64 hex
    /// digits.
    Portable,
    /// One this build does not know.
    #[serde(other)]
    Unknown,
}

/// Someone a playlist can be shared with (`ListShareCandidates`): an
/// enabled account here, or a portable user granted access.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareCandidate {
    /// As [`PlaylistShare::user_id`] takes it.
    pub user_id: String,
    /// What they are shown as: an account's display name, else its
    /// username; a portable user's name, else their fingerprint.
    pub display_name: String,
    /// An account's username; `None` for a portable user, who has none
    /// here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    pub kind: UserKind,
}

/// One page of whom a playlist can be shared with
/// (`ListShareCandidates`), by display name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareCandidatePage {
    pub candidates: Vec<ShareCandidate>,
    /// Where `candidates` start in the whole list.
    pub offset: u32,
    /// How many match in all.
    pub total: u32,
}

/// How a playlist stands to whoever asks: what a client lists its
/// playlists apart by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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

/// A built-in playlist: every user has one of each, made by the
/// server, never renamed, re-kinded, shared or deleted. Music only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum Builtin {
    /// The tracks the user liked (`Like`), newest first unless they
    /// moved them. Its entries change by `Like` and `Unlike`, and by
    /// `EditPlaylist`'s removes and moves; an add there is refused.
    Liked,
    /// Every track the user may see, worked out when read or played, so
    /// a newly scanned album is in it at once. Read-only.
    All,
    /// One this build does not know: shown as any read-only playlist.
    #[serde(other)]
    Unknown,
}

/// A heart: whether an item is in the user's Liked. A track's is
/// `All` or `None`; an album's is `All` when every track it has now is
/// liked, `Some` when some are, `None` when none are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum Liked {
    All,
    Some,
    None,
    /// One this build does not know: shown as `None`.
    #[serde(other)]
    Unknown,
}

/// A playlist as one user sees it, without its entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
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
    /// Which built-in playlist it is, or `None` for one a user
    /// made: what a client lists Liked and All first by, and hides
    /// rename, share and delete for.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub builtin: Option<Builtin>,
    /// What its owner is shown as: what a row says "Alice's" by. `None`
    /// for an owner no longer here.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub owner_name: Option<String>,
    /// Its picture, fetched as any artwork is: the cover an editor uploaded;
    /// else a 2×2 mosaic of the first four different album covers or
    /// posters in it; one alone when it has fewer; a placeholder when it
    /// has none. Liked's and All's are fixed tiles. Changes when any of
    /// that does.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub artwork: Option<ArtworkId>,
    /// `artwork` is a cover an editor uploaded, which `RemovePlaylistArt`
    /// takes away; false for a mosaic, a placeholder or a built-in tile.
    pub custom_art: bool,
}

/// Whose a playlist is, as its card says under its name
/// ([`PlaylistSummary::facts`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaylistOwner {
    /// Liked or All, the server's own.
    BuiltIn(Builtin),
    /// The user's, shared with `shared_with` people (none: personal).
    Yours { shared_with: usize },
    /// Someone else's, shared with the user: by `owner` where the server
    /// names them.
    SharedWithYou {
        owner: Option<String>,
        can_edit: bool,
    },
    /// Every user's; `can_edit` for an admin.
    ServerWide { can_edit: bool },
}

/// What a playlist's card says of it, worked out once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistFacts {
    pub owner: PlaylistOwner,
    pub kinds: Kinds,
    /// Tracks and nothing else: it plays as music does, with no place
    /// kept and no watched to skip.
    pub music_only: bool,
    pub pinned: bool,
}

impl PlaylistSummary {
    /// Whose it is and what it holds, for its card. A kind of owner from
    /// a newer server reads as shared with the user: someone else's.
    pub fn facts(&self) -> PlaylistFacts {
        let owner = match (self.builtin, self.owner_kind, &self.visibility) {
            (Some(builtin), _, _) => PlaylistOwner::BuiltIn(builtin),
            (None, PlaylistOwnerKind::Mine, Visibility::Shared { with }) => PlaylistOwner::Yours {
                shared_with: with.len(),
            },
            (None, PlaylistOwnerKind::Mine, _) => PlaylistOwner::Yours { shared_with: 0 },
            (None, PlaylistOwnerKind::ServerWide, _) => PlaylistOwner::ServerWide {
                can_edit: self.can_manage,
            },
            (None, PlaylistOwnerKind::SharedWithMe | PlaylistOwnerKind::Unknown, _) => {
                PlaylistOwner::SharedWithYou {
                    owner: self.owner_name.clone(),
                    can_edit: self.can_edit,
                }
            }
        };
        PlaylistFacts {
            owner,
            kinds: self.kinds,
            music_only: self.kinds.tracks && !self.kinds.movies && !self.kinds.episodes,
            pinned: self.pinned.is_some(),
        }
    }
}

/// A playlist holding an item, as the item's details list it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PlaylistRef {
    pub id: String,
    pub name: String,
    pub owner_kind: PlaylistOwnerKind,
    /// As [`PlaylistSummary::builtin`]. All is never listed here: it
    /// holds every track.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub builtin: Option<Builtin>,
    /// What its owner is shown as, as [`PlaylistSummary::owner_name`]:
    /// what an item's row says "Alice's" by. `None` for an owner no
    /// longer here.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub owner_name: Option<String>,
    /// Its picture, as [`PlaylistSummary::artwork`]: an uploaded cover,
    /// else the mosaic of its first four, so an item's page can show the
    /// playlists holding it as it shows seasons.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub artwork: Option<ArtworkId>,
}

/// One entry of a playlist, with its item as the catalog has it now and
/// the user's watch state on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PlaylistEntry {
    pub entry: EntryId,
    pub item: spoolframe_types::Item,
    /// Still in the library, but its file cannot be read now (a drive
    /// offline, a file moved): shown, counted, skipped when playing.
    pub unavailable: bool,
    /// A track in the user's Liked: its heart.
    pub liked: bool,
}

/// One page of a playlist's entries (`GetPlaylist`). Items the user may
/// not see are neither listed nor counted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PlaylistPage {
    pub playlist: PlaylistSummary,
    pub entries: Vec<PlaylistEntry>,
    /// Where `entries` start in the whole list.
    pub offset: u32,
    /// How many entries the whole list has.
    pub total: u32,
    /// How many of them can be played now: `total` less the unavailable.
    /// All's is `total`: its files are looked at only as they
    /// are listed and played, not all at every read.
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PlaySettings {
    /// Play in a shuffled order: every playable entry once per round.
    #[cfg_attr(feature = "uniffi", uniffi(default = false))]
    pub shuffle: bool,
    pub repeat: Repeat,
    /// Pass entries whose item the user has watched.
    #[cfg_attr(feature = "uniffi", uniffi(default = false))]
    pub skip_watched: bool,
}

/// What plays again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum Repeat {
    /// The end of the playlist ends it. (Each takes the Linux app's own
    /// name too, as its saved queues have it.)
    #[default]
    #[serde(alias = "Off")]
    Off,
    /// The same entry, over and over.
    #[serde(alias = "One")]
    One,
    /// The whole playlist again after the last entry; shuffled, the same
    /// round again.
    #[serde(alias = "All")]
    All,
    /// A kind this build does not know: played as `Off`.
    #[serde(other)]
    Unknown,
}

/// The entry to play now (`PlayPlaylist`, `NextInPlaylist`), with its
/// item, the user's watch state on it (to resume where they were) and its
/// intro and credits markers (to skip the credits).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PlaylistNext {
    pub entry: EntryId,
    pub item: spoolframe_types::Item,
    pub markers: spoolframe_types::Markers,
    /// A track in the user's Liked: the now-playing heart.
    pub liked: bool,
}

/// What importing the user's Plex playlists made, or would make
/// (`ImportPlexPlaylists`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlexImport {
    /// One per Plex playlist with at least one item found here; and one
    /// named "Liked" for the tracks rated in Plex that were liked here,
    /// its `entries` those newly liked.
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PinNote {
    pub playlist_id: String,
    /// Its name when it left.
    pub name: String,
    pub reason: PinNoteReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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
    fn summary(owner_kind: PlaylistOwnerKind, visibility: Visibility) -> PlaylistSummary {
        PlaylistSummary {
            id: "p".into(),
            name: "Heists".into(),
            kinds: Kinds {
                tracks: true,
                ..Kinds::default()
            },
            owner_kind,
            visibility,
            can_edit: false,
            can_manage: false,
            revision: 1,
            pinned: None,
            builtin: None,
            owner_name: Some("Alice".into()),
            artwork: None,
            custom_art: false,
        }
    }

    #[test]
    fn whose_it_is_reads_from_the_summary() {
        let shared = Visibility::Shared {
            with: vec![PlaylistShare::new("local:b", false)],
        };
        let mine = summary(PlaylistOwnerKind::Mine, shared).facts();
        assert_eq!(mine.owner, PlaylistOwner::Yours { shared_with: 1 });
        assert!(mine.music_only);
        let theirs = summary(PlaylistOwnerKind::SharedWithMe, Visibility::Personal).facts();
        assert_eq!(
            theirs.owner,
            PlaylistOwner::SharedWithYou {
                owner: Some("Alice".into()),
                can_edit: false
            }
        );
        let newer = summary(PlaylistOwnerKind::Unknown, Visibility::Personal).facts();
        assert!(matches!(newer.owner, PlaylistOwner::SharedWithYou { .. }));
        let mut liked = summary(PlaylistOwnerKind::Mine, Visibility::Personal);
        liked.builtin = Some(Builtin::Liked);
        assert_eq!(liked.facts().owner, PlaylistOwner::BuiltIn(Builtin::Liked));
    }

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
    fn a_builtin_or_heart_of_a_newer_build_reads_as_unknown() {
        let builtin: Builtin = serde_json::from_str(r#""recent""#).unwrap();
        assert_eq!(builtin, Builtin::Unknown);
        let liked: Liked = serde_json::from_str(r#""most""#).unwrap();
        assert_eq!(liked, Liked::Unknown);
        assert_eq!(serde_json::to_string(&Liked::Some).unwrap(), r#""some""#);
    }

    /// A summary without `builtin` and the rest, as a server before the
    /// reset to revision 1 could send it, is refused.
    #[test]
    fn a_summary_without_builtin_is_refused() {
        let json = serde_json::json!({
            "id": "x", "name": "Heists",
            "kinds": { "movies": false, "episodes": true, "tracks": false },
            "owner_kind": "mine", "visibility": { "type": "personal" },
            "can_edit": true, "can_manage": true, "revision": 1, "pinned": null,
        });
        let error = serde_json::from_value::<PlaylistSummary>(json).unwrap_err();
        assert!(
            error.to_string().contains("missing field `builtin`"),
            "{error}"
        );
    }

    /// A share without its name and kind keys is refused; a request's
    /// share, which names neither, sends both as `null`.
    #[test]
    fn a_share_names_its_name_and_kind_or_null() {
        assert!(
            serde_json::from_str::<PlaylistShare>(r#"{"user_id":"local:x","can_edit":true}"#)
                .is_err()
        );
        let sent = serde_json::to_value(PlaylistShare::new("local:x", false)).unwrap();
        assert!(sent["display_name"].is_null(), "{sent}");
        assert!(sent["kind"].is_null(), "{sent}");
        assert_eq!(
            serde_json::from_value::<PlaylistShare>(sent).unwrap(),
            PlaylistShare::new("local:x", false)
        );
        let kind: UserKind = serde_json::from_str(r#""federated""#).unwrap();
        assert_eq!(kind, UserKind::Unknown);
    }

    #[test]
    fn an_edit_of_a_newer_build_reads_as_unknown() {
        let edit: PlaylistEdit = serde_json::from_str(r#"{"type":"shuffle_all"}"#).unwrap();
        assert_eq!(edit, PlaylistEdit::Unknown);
    }
}
