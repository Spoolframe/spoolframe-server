//! Playlists as the screens show them, worked out once (the Linux app's
//! core/playlists.rs): the Playlists section's groups and Pinned, what
//! "Add to playlist" offers and why one is greyed, a drag's edit, the
//! sharing editor's changes, where Continue starts, hearts, a new
//! playlist's checks and pin notes shown once. Each answer is data an app
//! words itself.

use std::collections::HashSet;
use std::time::Duration;

use spoolframe_protocol::playlists::{
    Builtin, EntryId, Kinds, Liked, PinNote, PlaylistEdit, PlaylistEntry, PlaylistOwnerKind,
    PlaylistPage, PlaylistShare, PlaylistSummary, Visibility,
};
use spoolframe_types::{Item, MediaId, MediaKind};

use super::PlaylistList;

/// The Playlists section's groups, each in the server's order
/// ([`PlaylistList::sections`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sections {
    /// Liked and All, first: the server lists them first, Liked before
    /// All.
    pub builtin: Vec<PlaylistSummary>,
    /// "Yours".
    pub mine: Vec<PlaylistSummary>,
    /// "Shared with you".
    pub shared_with_me: Vec<PlaylistSummary>,
    /// "Everyone's".
    pub server_wide: Vec<PlaylistSummary>,
}

impl PlaylistList {
    /// The playlists apart by whose each is. A kind from a newer server
    /// goes with the shared ones: someone else's, seen by this user.
    pub fn sections(&self) -> Sections {
        let mut sections = Sections::default();
        for playlist in &self.playlists {
            let group = match (playlist.builtin, playlist.owner_kind) {
                (Some(_), _) => &mut sections.builtin,
                (None, PlaylistOwnerKind::Mine) => &mut sections.mine,
                (None, PlaylistOwnerKind::ServerWide) => &mut sections.server_wide,
                (None, PlaylistOwnerKind::SharedWithMe | PlaylistOwnerKind::Unknown) => {
                    &mut sections.shared_with_me
                }
            };
            group.push(playlist.clone());
        }
        sections
    }

    /// The user's Pinned, in their order.
    pub fn pinned(&self) -> Vec<PlaylistSummary> {
        pinned(&self.playlists).into_iter().cloned().collect()
    }

    /// Where a pin newly made goes: last.
    pub fn new_pin_position(&self) -> u32 {
        self.playlists.iter().filter(|p| p.pinned.is_some()).count() as u32
    }
}

fn pinned(list: &[PlaylistSummary]) -> Vec<&PlaylistSummary> {
    let mut pinned: Vec<_> = list.iter().filter(|p| p.pinned.is_some()).collect();
    pinned.sort_by_key(|p| p.pinned);
    pinned
}

/// The pin a drag within Pinned makes: the playlist at `from` (in
/// [`PlaylistList::pinned`]'s order) dropped on the row at `to` takes
/// that row's place. Its id and the position to pin it at; `None` when it
/// stays.
pub fn pin_drop(pinned: &[PlaylistSummary], from: usize, to: usize) -> Option<(String, u32)> {
    let playlist = pinned.get(from)?;
    let to = to.min(pinned.len() - 1);
    (to != from).then(|| (playlist.id.clone(), to as u32))
}

/// `list` with playlist `id` pinned at `position`, as the server will
/// have it: shown at once, before its answer. Positions are made 0, 1, …
/// in the new order.
pub fn with_pin_at(list: &mut [PlaylistSummary], id: &str, position: u32) {
    let mut order: Vec<String> = pinned(list)
        .into_iter()
        .filter(|p| p.id != id)
        .map(|p| p.id.clone())
        .collect();
    let at = (position as usize).min(order.len());
    order.insert(at, id.to_owned());
    for playlist in list.iter_mut() {
        playlist.pinned = order
            .iter()
            .position(|pinned| *pinned == playlist.id)
            .map(|at| at as u32);
    }
}

/// The edit that moves the entry at `from` in `entries` (as shown) so it
/// ends at `to`: before whatever is then after it, or to the end. `None`
/// when it stays where it is.
pub fn move_edit(entries: &[EntryId], from: usize, to: usize) -> Option<PlaylistEdit> {
    if from >= entries.len() {
        return None;
    }
    let mut order = entries.to_vec();
    let entry = order.remove(from);
    let to = to.min(order.len());
    if to == from {
        return None;
    }
    order.insert(to, entry);
    Some(PlaylistEdit::Move {
        entry,
        before: order.get(to + 1).copied(),
    })
}

/// How "Add to playlist" offers a playlist that takes the item's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offer {
    /// One click adds.
    Adds,
    /// Shared with the user to read: greyed.
    ReadOnly,
    /// Server-wide, and the user is no admin: greyed.
    AdminsEdit,
    /// Liked: a track gets in by its heart, never by an add.
    UseHeart,
    /// All: it holds every track already.
    HasEverything,
}

/// A playlist "Add to playlist" lists, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddTarget {
    pub playlist: PlaylistSummary,
    pub offer: Offer,
}

/// The playlists "Add to playlist" lists for an item of `kind`: only
/// those whose kinds take it (a film never sees a music playlist), those
/// the user can add to first, each group in the server's order.
pub fn add_targets(list: &[PlaylistSummary], kind: MediaKind) -> Vec<AddTarget> {
    let mut targets: Vec<_> = list
        .iter()
        .filter(|p| p.kinds.allows(kind))
        .map(|p| {
            let offer = match (p.builtin, p.can_edit, p.owner_kind) {
                (Some(Builtin::Liked), _, _) => Offer::UseHeart,
                (Some(_), _, _) => Offer::HasEverything,
                (None, true, _) => Offer::Adds,
                (None, false, PlaylistOwnerKind::ServerWide) => Offer::AdminsEdit,
                (None, false, _) => Offer::ReadOnly,
            };
            AddTarget {
                playlist: p.clone(),
                offer,
            }
        })
        .collect();
    targets.sort_by_key(|t| t.offer != Offer::Adds);
    targets
}

/// The kinds a playlist made from an item's "New playlist…" starts with:
/// those that take the item.
pub fn kinds_for(kind: MediaKind) -> Kinds {
    Kinds {
        movies: kind == MediaKind::Movie,
        episodes: matches!(
            kind,
            MediaKind::Episode | MediaKind::Season | MediaKind::Show
        ),
        tracks: matches!(
            kind,
            MediaKind::Track | MediaKind::Album | MediaKind::Artist
        ),
    }
}

/// Whether a tap on a heart showing `liked` likes (true) or un-likes: a
/// full heart un-likes, a half or empty one likes.
pub fn heart_likes(liked: Liked) -> bool {
    !matches!(liked, Liked::All)
}

/// An album's heart from which of its `tracks` are `liked`: full when
/// every one is, half when some are.
pub fn album_heart(tracks: &[MediaId], liked: &HashSet<MediaId>) -> Liked {
    heart_of(
        tracks.iter().filter(|t| liked.contains(*t)).count(),
        tracks.len(),
    )
}

/// The heart for `liked` liked of `of` tracks.
pub(crate) fn heart_of(liked: usize, of: usize) -> Liked {
    match liked {
        0 => Liked::None,
        n if n == of => Liked::All,
        _ => Liked::Some,
    }
}

/// What can have a heart: tracks and albums.
pub fn can_be_liked(kind: MediaKind) -> bool {
    matches!(kind, MediaKind::Track | MediaKind::Album)
}

/// What a playlist's page counts in its header ([`page_counts`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageCounts {
    Empty,
    /// By kind, when the page has every entry.
    ByKind {
        films: usize,
        episodes: usize,
        tracks: usize,
        playable: Option<u32>,
    },
    /// A longer playlist: how many items.
    Items {
        total: u32,
        playable: Option<u32>,
    },
}

/// A playlist page's header counts. `playable` is said only when fewer
/// than all can play.
pub fn page_counts(page: &PlaylistPage) -> PageCounts {
    if page.total == 0 {
        return PageCounts::Empty;
    }
    let playable = (page.playable < page.total).then_some(page.playable);
    if page.offset == 0 && page.entries.len() as u32 == page.total {
        let count = |kind| {
            page.entries
                .iter()
                .filter(|e| e.item.id().kind() == kind)
                .count()
        };
        PageCounts::ByKind {
            films: count(MediaKind::Movie),
            episodes: count(MediaKind::Episode),
            tracks: count(MediaKind::Track),
            playable,
        }
    } else {
        PageCounts::Items {
            total: page.total,
            playable,
        }
    }
}

/// The entry Continue starts at, of those on the page: the user's place,
/// or once its item is watched or cannot be read, the next that can play
/// and is not watched. `None` without a place, when Continue is Play from
/// the top.
pub fn continue_entry(page: &PlaylistPage) -> Option<EntryId> {
    let place = page.place?;
    let at = page.entries.iter().position(|e| e.entry == place)?;
    let plays = |e: &&PlaylistEntry| !e.unavailable && !e.item.is_watched();
    page.entries[at..].iter().find(plays).map(|e| e.entry)
}

/// The line under an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryNote {
    /// The file cannot be read: skipped.
    Unavailable,
    /// Continue starts here.
    UpNext,
    Watched,
    Length(Duration),
}

/// The line under `entry`, `continues` being [`continue_entry`]'s; `None`
/// when there is nothing to say.
pub fn entry_note(entry: &PlaylistEntry, continues: Option<EntryId>) -> Option<EntryNote> {
    if entry.unavailable {
        return Some(EntryNote::Unavailable);
    }
    if Some(entry.entry) == continues {
        return Some(EntryNote::UpNext);
    }
    if entry.item.is_watched() {
        return Some(EntryNote::Watched);
    }
    let length = match &entry.item {
        Item::Movie(m) => m.duration,
        Item::Episode(e) => e.duration,
        Item::Track(t) => t.duration,
        _ => None,
    };
    length.map(EntryNote::Length)
}

/// Why "New playlist" cannot be sent yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewPlaylistError {
    /// No name, or spaces alone.
    NoName,
    /// No kind ticked.
    NoKind,
}

/// What "New playlist" sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPlaylist {
    pub name: String,
    pub kinds: Kinds,
    pub visibility: Visibility,
}

/// What "New playlist" sends from what its dialog has: a trimmed name,
/// at least one kind, and who sees it.
pub fn new_playlist(
    name: &str,
    kinds: Kinds,
    server_wide: bool,
) -> Result<NewPlaylist, NewPlaylistError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(NewPlaylistError::NoName);
    }
    if kinds.is_empty() {
        return Err(NewPlaylistError::NoKind);
    }
    let visibility = match server_wide {
        true => Visibility::ServerWide,
        false => Visibility::Personal,
    };
    Ok(NewPlaylist {
        name: name.to_owned(),
        kinds,
        visibility,
    })
}

/// What the editor shows of a playlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorControls {
    /// Edit itself, the entries' handles and Remove.
    pub edit: bool,
    /// The side panel: name, kinds, sharing, Delete.
    pub panel: bool,
    /// Server-wide among the sharing choices: an admin's.
    pub server_wide: bool,
}

pub fn editor_controls(playlist: &PlaylistSummary, admin: bool) -> EditorControls {
    EditorControls {
        edit: playlist.can_edit || playlist.can_manage,
        panel: playlist.can_manage,
        server_wide: playlist.can_manage && admin,
    }
}

/// A change made in the editor's Sharing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareChange {
    Personal,
    ServerWide,
    /// Whether this user may edit the entries.
    CanEdit {
        user: String,
        can_edit: bool,
    },
    StopSharing {
        user: String,
    },
    /// Someone picked to share with; someone it is shared with already
    /// keeps their place, with this access.
    Add {
        user: String,
        can_edit: bool,
    },
}

/// Who sees the playlist after `change`, from `now`. Shared with nobody
/// is Personal.
pub fn share_change(now: &Visibility, change: &ShareChange) -> Visibility {
    let shares = || match now {
        Visibility::Shared { with } => with.clone(),
        _ => Vec::new(),
    };
    let with: Vec<PlaylistShare> = match change {
        ShareChange::Personal => return Visibility::Personal,
        ShareChange::ServerWide => return Visibility::ServerWide,
        ShareChange::CanEdit { user, can_edit } => shares()
            .into_iter()
            .map(|share| match share.user_id == *user {
                true => PlaylistShare {
                    can_edit: *can_edit,
                    ..share
                },
                false => share,
            })
            .collect(),
        ShareChange::StopSharing { user } => shares()
            .into_iter()
            .filter(|share| share.user_id != *user)
            .collect(),
        ShareChange::Add { user, can_edit } => {
            let mut with = shares();
            match with.iter_mut().find(|share| share.user_id == *user) {
                Some(share) => share.can_edit = *can_edit,
                None => with.push(PlaylistShare::new(user.clone(), *can_edit)),
            }
            with
        }
    };
    match with.is_empty() {
        true => Visibility::Personal,
        false => Visibility::Shared { with },
    }
}

/// The pin notes shown this session: the server keeps a note until some
/// device dismisses it, so a list read again before the dismissal lands
/// carries it once more, and it is not shown twice.
#[derive(Debug, Default)]
pub struct PinNotesShown(HashSet<String>);

impl PinNotesShown {
    /// The notes in `notes` not shown yet, now counted as shown.
    pub fn fresh(&mut self, notes: &[PinNote]) -> Vec<PinNote> {
        notes
            .iter()
            .filter(|note| self.0.insert(note.playlist_id.clone()))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_protocol::playlists::PinNoteReason;
    use spoolframe_types::{Episode, Namespace, PlaybackState};

    fn summary(id: &str, owner_kind: PlaylistOwnerKind, kinds: Kinds) -> PlaylistSummary {
        PlaylistSummary {
            id: id.into(),
            name: id.into(),
            kinds,
            owner_kind,
            visibility: Visibility::Personal,
            can_edit: owner_kind == PlaylistOwnerKind::Mine,
            can_manage: owner_kind == PlaylistOwnerKind::Mine,
            revision: 1,
            pinned: None,
            builtin: None,
            owner_name: None,
            artwork: None,
            custom_art: false,
        }
    }

    fn episodes() -> Kinds {
        Kinds {
            episodes: true,
            ..Kinds::default()
        }
    }

    fn music() -> Kinds {
        Kinds {
            tracks: true,
            ..Kinds::default()
        }
    }

    fn ids(list: &[PlaylistSummary]) -> Vec<&str> {
        list.iter().map(|p| p.id.as_str()).collect()
    }

    fn list(playlists: Vec<PlaylistSummary>) -> PlaylistList {
        PlaylistList {
            playlists,
            pin_notes: Vec::new(),
        }
    }

    #[test]
    fn the_section_groups_playlists_by_whose_they_are() {
        let mut liked = summary("liked", PlaylistOwnerKind::Mine, music());
        liked.builtin = Some(Builtin::Liked);
        let sections = list(vec![
            liked,
            summary("mine", PlaylistOwnerKind::Mine, episodes()),
            summary("theirs", PlaylistOwnerKind::SharedWithMe, episodes()),
            summary("newer", PlaylistOwnerKind::Unknown, episodes()),
            summary("all of us", PlaylistOwnerKind::ServerWide, episodes()),
        ])
        .sections();
        assert_eq!(ids(&sections.builtin), ["liked"]);
        assert_eq!(ids(&sections.mine), ["mine"]);
        assert_eq!(ids(&sections.shared_with_me), ["theirs", "newer"]);
        assert_eq!(ids(&sections.server_wide), ["all of us"]);
    }

    fn pinned_list() -> Vec<PlaylistSummary> {
        let pin = |id, at| PlaylistSummary {
            pinned: at,
            ..summary(id, PlaylistOwnerKind::Mine, episodes())
        };
        vec![
            pin("a", Some(1)),
            pin("b", None),
            pin("c", Some(0)),
            pin("d", Some(2)),
        ]
    }

    #[test]
    fn pinned_follows_the_users_order_and_a_new_pin_goes_last() {
        let list = list(pinned_list());
        assert_eq!(ids(&list.pinned()), ["c", "a", "d"]);
        assert_eq!(list.new_pin_position(), 3);
    }

    #[test]
    fn a_pin_dropped_on_the_first_row_goes_first_here_and_on_the_server() {
        let mut all = pinned_list();
        let pinned = list(all.clone()).pinned();
        let (id, at) = pin_drop(&pinned, 2, 0).unwrap();
        assert_eq!((id.as_str(), at), ("d", 0));
        with_pin_at(&mut all, &id, at);
        assert_eq!(ids(&list(all).pinned()), ["d", "c", "a"]);
    }

    #[test]
    fn a_pin_dropped_on_itself_sends_nothing_and_past_the_last_goes_last() {
        let pinned = list(pinned_list()).pinned();
        assert_eq!(pin_drop(&pinned, 1, 1), None);
        assert_eq!(pin_drop(&pinned, 0, 9), Some(("c".into(), 2)));
    }

    #[test]
    fn a_drag_moves_the_entry_before_the_one_then_after_it() {
        let entries = [1, 2, 3, 4];
        assert_eq!(
            move_edit(&entries, 0, 2),
            Some(PlaylistEdit::Move {
                entry: 1,
                before: Some(4)
            })
        );
        assert_eq!(
            move_edit(&entries, 1, 3),
            Some(PlaylistEdit::Move {
                entry: 2,
                before: None
            })
        );
        assert_eq!(move_edit(&entries, 2, 2), None);
    }

    #[test]
    fn a_film_is_offered_no_episode_or_music_playlist() {
        let lists = [
            summary("eps", PlaylistOwnerKind::Mine, episodes()),
            summary("music", PlaylistOwnerKind::Mine, music()),
        ];
        assert!(add_targets(&lists, MediaKind::Movie).is_empty());
    }

    #[test]
    fn read_only_and_server_wide_ones_are_offered_greyed_last_saying_why() {
        let mut liked = summary("liked", PlaylistOwnerKind::Mine, music());
        liked.builtin = Some(Builtin::Liked);
        let mut all = summary("all", PlaylistOwnerKind::Mine, music());
        all.builtin = Some(Builtin::All);
        let lists = [
            summary("theirs", PlaylistOwnerKind::SharedWithMe, music()),
            summary("everyone", PlaylistOwnerKind::ServerWide, music()),
            liked,
            all,
            summary("mine", PlaylistOwnerKind::Mine, music()),
        ];
        let offers: Vec<_> = add_targets(&lists, MediaKind::Album)
            .into_iter()
            .map(|t| (t.playlist.id, t.offer))
            .collect();
        assert_eq!(
            offers,
            [
                ("mine".into(), Offer::Adds),
                ("theirs".into(), Offer::ReadOnly),
                ("everyone".into(), Offer::AdminsEdit),
                ("liked".into(), Offer::UseHeart),
                ("all".into(), Offer::HasEverything),
            ]
        );
    }

    #[test]
    fn a_new_playlist_from_an_item_takes_its_kind() {
        assert_eq!(kinds_for(MediaKind::Season), episodes());
        assert_eq!(kinds_for(MediaKind::Album), music());
    }

    fn track_id(key: &str) -> MediaId {
        MediaId::new(Namespace::Plex, MediaKind::Track, key).unwrap()
    }

    #[test]
    fn an_albums_heart_is_full_half_or_empty_by_its_tracks() {
        let tracks = [track_id("1"), track_id("2")];
        let liked = |keys: &[&str]| keys.iter().map(|k| track_id(k)).collect::<HashSet<_>>();
        assert_eq!(album_heart(&tracks, &liked(&["1", "2"])), Liked::All);
        assert_eq!(album_heart(&tracks, &liked(&["2"])), Liked::Some);
        assert_eq!(album_heart(&tracks, &liked(&[])), Liked::None);
        assert!(!heart_likes(Liked::All));
        assert!(heart_likes(Liked::Some));
        assert!(can_be_liked(MediaKind::Album) && !can_be_liked(MediaKind::Movie));
    }

    fn episode(n: u64, watched: bool, unavailable: bool) -> PlaylistEntry {
        let id = |kind, key: String| MediaId::new(Namespace::Plex, kind, key).unwrap();
        PlaylistEntry {
            entry: n,
            item: Item::Episode(Episode {
                id: id(MediaKind::Episode, n.to_string()),
                show_id: id(MediaKind::Show, "s".into()),
                show_title: None,
                season_id: id(MediaKind::Season, "1".into()),
                season: 1,
                episode: n as u32,
                title: format!("E{n}"),
                summary: None,
                content_rating: None,
                year: None,
                air_date: None,
                duration: Some(Duration::from_secs(60)),
                cast: Vec::new(),
                artwork: Vec::new(),
                playback: PlaybackState {
                    watched,
                    ..PlaybackState::default()
                },
            }),
            unavailable,
            liked: false,
        }
    }

    fn page(entries: Vec<PlaylistEntry>, place: Option<EntryId>) -> PlaylistPage {
        let total = entries.len() as u32;
        PlaylistPage {
            playlist: summary("p", PlaylistOwnerKind::Mine, episodes()),
            playable: entries.iter().filter(|e| !e.unavailable).count() as u32,
            entries,
            offset: 0,
            total,
            place,
            settings: Default::default(),
        }
    }

    #[test]
    fn continue_starts_at_the_place_and_passes_it_once_watched() {
        let fresh = page(
            vec![episode(1, false, false), episode(2, false, false)],
            Some(1),
        );
        assert_eq!(continue_entry(&fresh), Some(1));
        let done = page(
            vec![
                episode(1, true, false),
                episode(2, false, true),
                episode(3, true, false),
                episode(4, false, false),
            ],
            Some(1),
        );
        assert_eq!(continue_entry(&done), Some(4));
        assert_eq!(continue_entry(&page(Vec::new(), None)), None);
        assert_eq!(
            entry_note(&done.entries[3], Some(4)),
            Some(EntryNote::UpNext)
        );
        assert_eq!(
            entry_note(&done.entries[1], Some(4)),
            Some(EntryNote::Unavailable)
        );
        assert_eq!(
            entry_note(&done.entries[0], Some(4)),
            Some(EntryNote::Watched)
        );
    }

    #[test]
    fn the_header_counts_by_kind_and_says_what_is_playable() {
        let counts = page_counts(&page(
            vec![episode(1, false, false), episode(2, false, true)],
            None,
        ));
        assert_eq!(
            counts,
            PageCounts::ByKind {
                films: 0,
                episodes: 2,
                tracks: 0,
                playable: Some(1)
            }
        );
        assert_eq!(page_counts(&page(Vec::new(), None)), PageCounts::Empty);
    }

    #[test]
    fn a_new_playlist_needs_a_name_and_a_kind() {
        assert_eq!(
            new_playlist("  ", episodes(), false),
            Err(NewPlaylistError::NoName)
        );
        assert_eq!(
            new_playlist("Heists", Kinds::default(), false),
            Err(NewPlaylistError::NoKind)
        );
        let made = new_playlist(" Heists ", episodes(), true).unwrap();
        assert_eq!(
            (made.name.as_str(), made.visibility),
            ("Heists", Visibility::ServerWide)
        );
    }

    #[test]
    fn the_editor_shows_what_each_user_may_change() {
        let mine = summary("m", PlaylistOwnerKind::Mine, episodes());
        assert_eq!(
            editor_controls(&mine, false),
            EditorControls {
                edit: true,
                panel: true,
                server_wide: false
            }
        );
        let mut theirs = summary("t", PlaylistOwnerKind::SharedWithMe, episodes());
        theirs.can_edit = true;
        assert_eq!(
            editor_controls(&theirs, true),
            EditorControls {
                edit: true,
                panel: false,
                server_wide: false
            }
        );
    }

    #[test]
    fn a_share_can_be_added_made_an_editor_or_stopped_and_the_last_makes_it_personal() {
        let add = |user: &str, can_edit| ShareChange::Add {
            user: user.into(),
            can_edit,
        };
        let one = share_change(&Visibility::Personal, &add("b", false));
        let two = share_change(&one, &add("c", true));
        let again = share_change(&two, &add("b", true));
        let Visibility::Shared { with } = &again else {
            panic!("shared");
        };
        let shown: Vec<_> = with
            .iter()
            .map(|s| (s.user_id.as_str(), s.can_edit))
            .collect();
        assert_eq!(shown, [("b", true), ("c", true)], "b keeps their place");
        let editor = share_change(
            &again,
            &ShareChange::CanEdit {
                user: "c".into(),
                can_edit: false,
            },
        );
        let stop = |v: &Visibility, user: &str| {
            share_change(v, &ShareChange::StopSharing { user: user.into() })
        };
        assert_eq!(stop(&stop(&editor, "b"), "c"), Visibility::Personal);
        assert_eq!(
            share_change(&again, &ShareChange::ServerWide),
            Visibility::ServerWide
        );
    }

    #[test]
    fn a_pin_note_is_shown_once_though_the_list_is_read_again() {
        let note = PinNote {
            playlist_id: "x".into(),
            name: "Christmas films".into(),
            reason: PinNoteReason::Unshared,
        };
        let mut shown = PinNotesShown::default();
        assert_eq!(shown.fresh(std::slice::from_ref(&note)).len(), 1);
        assert!(shown.fresh(&[note]).is_empty());
    }
}
