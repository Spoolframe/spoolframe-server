//! Playlists (PX-40): what the pages say of them, which ones an item can
//! be added to, and the edits a move or the sharing dialog makes; and the
//! hearts and the built-in Liked and All (PX-43). Plain logic, tested on
//! the host.

use spoolframe_admin_api::{
    CatalogKind, PlaylistBuiltin, PlaylistEntryInfo, PlaylistInfo, PlaylistKinds, PlaylistOwner,
    PlaylistShare, PlaylistSharing, PlexPlaylistImport, ShareableUser,
};

use crate::logic::count;

/// Whether a playlist of `kinds` takes an item of `kind`: a show or season
/// adds its episodes, an artist or album its tracks, as the server adds
/// them.
pub fn takes(kinds: PlaylistKinds, kind: CatalogKind) -> bool {
    match kind {
        CatalogKind::Movie => kinds.movies,
        CatalogKind::Show | CatalogKind::Season | CatalogKind::Episode => kinds.episodes,
        CatalogKind::Artist | CatalogKind::Album | CatalogKind::Track => kinds.tracks,
    }
}

/// The playlists "Add to playlist" offers for an item of `kind`: those the
/// caller may change whose kinds take it, so a film never sees a music
/// playlist. Never Liked or All: a track goes into Liked by its heart.
pub fn add_targets(list: &[PlaylistInfo], kind: CatalogKind) -> Vec<PlaylistInfo> {
    list.iter()
        .filter(|p| p.can_edit && p.builtin.is_none() && takes(p.kinds, kind))
        .cloned()
        .collect()
}

/// The largest cover the server takes (PX-53): a larger file is refused
/// here, before it is sent.
pub const MAX_COVER_BYTES: u64 = 20 << 20;

/// What a picture chosen as a cover may be: the four the server decodes.
pub const COVER_ACCEPT: &str = "image/jpeg,image/png,image/webp,image/heic,image/heif,.heic,.heif";

/// `text` for a URL's query: everything but letters, digits and `-_.~`
/// escaped.
fn query_part(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Where playlist `id`'s picture is at `size` (PX-53). With `art`, the
/// picture the page shows, the browser keeps it until it changes.
pub fn cover_src(id: &str, art: Option<&str>, size: &str) -> String {
    let mut src = format!("/playlists/art?id={}&size={size}", query_part(id));
    if let Some(art) = art {
        src.push_str("&art=");
        src.push_str(&query_part(art));
    }
    src
}

/// Where a cover is uploaded for playlist `id`.
pub fn cover_upload(id: &str) -> String {
    format!("/playlists/art?id={}", query_part(id))
}

/// Whether the user may change `p`'s cover: who edits it, and never Liked
/// or All, whose tiles are fixed.
pub fn may_change_cover(p: &PlaylistInfo) -> bool {
    p.can_edit && p.builtin.is_none()
}

/// Why a file of `bytes` is not sent as a cover, if it is not.
pub fn cover_refused(bytes: u64) -> Option<String> {
    (bytes > MAX_COVER_BYTES).then(|| {
        format!(
            "That picture is {:.1} MB; a cover is at most 20 MB.",
            bytes as f64 / 1e6
        )
    })
}

/// What a built-in playlist is, under its name.
pub fn builtin_line(builtin: PlaylistBuiltin) -> &'static str {
    match builtin {
        PlaylistBuiltin::Liked => "The tracks you liked, newest first",
        PlaylistBuiltin::All => "Every track you can play, as the library has it now",
    }
}

/// What an empty playlist's page says.
pub fn empty_text(builtin: Option<PlaylistBuiltin>, admin: bool) -> &'static str {
    match builtin {
        Some(PlaylistBuiltin::Liked) => {
            "Nothing liked yet. Like a track or an album with its heart, here or in the apps."
        }
        Some(PlaylistBuiltin::All) => "There's no music in the libraries yet.",
        None if admin => {
            "Nothing in it yet. Add films, episodes and music from the Spoolframe apps, or from an item's page in Libraries."
        }
        None => "Nothing in it yet. Add films, episodes and music from the Spoolframe apps.",
    }
}

/// A track's or an album's heart (PX-43).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heart {
    /// The track is liked; every track of the album is.
    Full,
    /// Some of the album's tracks are liked.
    Half,
    Empty,
}

impl Heart {
    /// The heart in an item's details (`CatalogItem::reply`'s `liked`):
    /// `None` for an item that has none, and from a server without
    /// hearts.
    pub fn of_reply(reply: &serde_json::Value) -> Option<Heart> {
        Some(match reply.get("liked")?.as_str()? {
            "all" => Heart::Full,
            "some" => Heart::Half,
            _ => Heart::Empty,
        })
    }

    /// A track's, from whether it is liked.
    pub fn of_track(liked: bool) -> Heart {
        if liked { Heart::Full } else { Heart::Empty }
    }

    /// Whether a click likes (`me.like`) rather than unlikes: a half
    /// heart likes the album's other tracks.
    pub fn likes(self) -> bool {
        self != Heart::Full
    }

    /// What a click on it does, in words, for an item of `kind`.
    pub fn action(self, kind: CatalogKind) -> &'static str {
        let album = kind == CatalogKind::Album;
        match (self, album) {
            (Heart::Full, false) => "Unlike",
            (Heart::Full, true) => "Unlike every track",
            (Heart::Half, _) => "Like the other tracks",
            (Heart::Empty, false) => "Like",
            (Heart::Empty, true) => "Like every track",
        }
    }

    /// The button's word beside the heart.
    pub fn word(self) -> &'static str {
        match self {
            Heart::Full => "Liked",
            Heart::Half => "Partly liked",
            Heart::Empty => "Like",
        }
    }
}

/// The album's tracks an item's details say are liked, by id.
pub fn liked_children(reply: &serde_json::Value) -> Vec<String> {
    reply
        .get("liked_children")
        .and_then(|c| serde_json::from_value(c.clone()).ok())
        .unwrap_or_default()
}

/// What "Add to playlist" says when no playlist takes an item.
pub fn no_target_text(kind: CatalogKind) -> String {
    let what = match kind {
        CatalogKind::Movie => "movies",
        CatalogKind::Show | CatalogKind::Season | CatalogKind::Episode => "episodes",
        CatalogKind::Artist | CatalogKind::Album | CatalogKind::Track => "music",
    };
    format!("None of the playlists you can change takes {what}. Make one under My playlists.")
}

/// "Movies, episodes and music".
pub fn kinds_line(kinds: PlaylistKinds) -> String {
    let names: Vec<&str> = [
        (kinds.movies, "movies"),
        (kinds.episodes, "episodes"),
        (kinds.tracks, "music"),
    ]
    .into_iter()
    .filter_map(|(on, name)| on.then_some(name))
    .collect();
    let line = match names.as_slice() {
        [] => return "Nothing".into(),
        [one] => (*one).to_owned(),
        [first @ .., last] => format!("{} and {last}", first.join(", ")),
    };
    let mut chars = line.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => line,
    }
}

/// The header's count, "18 episodes · 17 playable": its unit the one kind
/// it holds, else "item". The playable part only when some are not.
pub fn count_line(kinds: PlaylistKinds, total: u32, playable: u32) -> String {
    let unit = match (kinds.movies, kinds.episodes, kinds.tracks) {
        (true, false, false) => "movie",
        (false, true, false) => "episode",
        (false, false, true) => "track",
        _ => "item",
    };
    let all = count(u64::from(total), unit);
    if playable < total {
        format!("{all} · {playable} playable")
    } else {
        all
    }
}

/// Who sees it, in words, naming the users `candidates` knows.
pub fn sharing_line(sharing: &PlaylistSharing, candidates: &[ShareableUser]) -> String {
    match sharing {
        PlaylistSharing::Personal => "Only you".into(),
        PlaylistSharing::ServerWide => "Everyone on this server".into(),
        // Someone else's, shared with the caller: whom else is not theirs
        // to know.
        PlaylistSharing::Shared { with } if with.is_empty() => "Shared with you".into(),
        PlaylistSharing::Shared { with } => {
            let names: Vec<String> = with
                .iter()
                .map(|s| name_of(&s.user_id, candidates))
                .collect();
            match names.as_slice() {
                [one] => format!("You and {one}"),
                [a, b] => format!("You, {a} and {b}"),
                [a, rest @ ..] => format!("You, {a} and {} others", rest.len()),
                [] => unreachable!("an empty share is handled above"),
            }
        }
    }
}

/// What a user id is shown as: their name, or "someone" for one no
/// longer here.
pub fn name_of(user_id: &str, candidates: &[ShareableUser]) -> String {
    candidates
        .iter()
        .find(|c| c.user_id == user_id)
        .map_or_else(|| "someone".into(), |c| c.display_name.clone())
}

/// One user's access in the sharing dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    None,
    View,
    Edit,
}

impl Access {
    pub const ALL: [Access; 3] = [Access::None, Access::View, Access::Edit];

    pub fn value(self) -> &'static str {
        match self {
            Access::None => "none",
            Access::View => "view",
            Access::Edit => "edit",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Access::None => "Not shared",
            Access::View => "Can view",
            Access::Edit => "Can edit",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "view" => Access::View,
            "edit" => Access::Edit,
            _ => Access::None,
        }
    }
}

/// The dialog's rows: every candidate, with what they have now. Users the
/// playlist is shared with who are no longer candidates are kept, as
/// "someone", so saving does not drop them silently.
pub fn share_rows(
    sharing: &PlaylistSharing,
    candidates: &[ShareableUser],
) -> Vec<(ShareableUser, Access)> {
    let with: &[PlaylistShare] = match sharing {
        PlaylistSharing::Shared { with } => with,
        _ => &[],
    };
    let access = |id: &str| match with.iter().find(|s| s.user_id == id) {
        Some(s) if s.can_edit => Access::Edit,
        Some(_) => Access::View,
        None => Access::None,
    };
    let mut rows: Vec<(ShareableUser, Access)> = candidates
        .iter()
        .map(|c| (c.clone(), access(&c.user_id)))
        .collect();
    for s in with {
        if !candidates.iter().any(|c| c.user_id == s.user_id) {
            rows.push((
                ShareableUser {
                    user_id: s.user_id.clone(),
                    display_name: "someone no longer here".into(),
                },
                access(&s.user_id),
            ));
        }
    }
    rows
}

/// The sharing the dialog's rows make: personal when nobody is ticked.
pub fn sharing_of(rows: &[(ShareableUser, Access)]) -> PlaylistSharing {
    let with: Vec<PlaylistShare> = rows
        .iter()
        .filter(|(_, a)| *a != Access::None)
        .map(|(u, a)| PlaylistShare {
            user_id: u.user_id.clone(),
            can_edit: *a == Access::Edit,
        })
        .collect();
    if with.is_empty() {
        PlaylistSharing::Personal
    } else {
        PlaylistSharing::Shared { with }
    }
}

/// The three lists My playlists shows: the caller's, those shared with
/// them, and the server-wide ones.
pub fn sections(list: &[PlaylistInfo]) -> [Vec<PlaylistInfo>; 3] {
    let of = |owner: PlaylistOwner| -> Vec<PlaylistInfo> {
        list.iter().filter(|p| p.owner == owner).cloned().collect()
    };
    [
        of(PlaylistOwner::Mine),
        of(PlaylistOwner::SharedWithMe),
        of(PlaylistOwner::ServerWide),
    ]
}

/// Where entry `at` of the page `entries` goes to move one place up
/// (`up`) or down: the entry it goes before, `None` for the end. `None`
/// when it cannot move that way on this page; `more_after` says whether
/// pages follow it, so "the end" of this page is not the playlist's.
pub fn move_target(
    entries: &[PlaylistEntryInfo],
    at: usize,
    up: bool,
    more_after: bool,
) -> Option<Option<u64>> {
    if at >= entries.len() {
        return None;
    }
    if up {
        let before = at.checked_sub(1)?;
        Some(Some(entries[before].entry))
    } else {
        if at + 1 >= entries.len() {
            return None;
        }
        match entries.get(at + 2) {
            Some(e) => Some(Some(e.entry)),
            None if more_after => None,
            None => Some(None),
        }
    }
}

/// A playlist holding an item, as the item's details list it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct HeldIn {
    pub id: String,
    pub name: String,
    pub owner_kind: String,
    /// `liked` for Liked (PX-43).
    #[serde(default)]
    pub builtin: Option<String>,
    /// Whose it is, for one shared with the caller.
    #[serde(default)]
    pub owner_name: Option<String>,
    /// Its picture (PX-53): an uploaded cover, else its mosaic.
    #[serde(default)]
    pub artwork: Option<String>,
}

/// Whether an item's page lists the playlists holding it among its
/// children, as a show's seasons are, rather than in a card of their
/// own: a show's, whose playlists are runs of its episodes as a season
/// is.
pub fn held_as_seasons(kind: CatalogKind) -> bool {
    kind == CatalogKind::Show
}

/// The line under a playlist's name where it is listed as a season is:
/// whose it is.
pub fn held_line(held: &HeldIn) -> String {
    match (held.owner_kind.as_str(), &held.owner_name) {
        ("mine", _) => "Your playlist".into(),
        ("server_wide", _) => "Server-wide playlist".into(),
        (_, Some(name)) => format!("{name}'s playlist"),
        _ => "Shared playlist".into(),
    }
}

/// The playlists an item's details (`CatalogItem::reply`) say hold it;
/// none when the server left them out.
pub fn held_in(reply: &serde_json::Value) -> Vec<HeldIn> {
    reply
        .get("playlists")
        .and_then(|p| serde_json::from_value(p.clone()).ok())
        .unwrap_or_default()
}

/// The kinds a playlist may be widened to: those it takes stay ticked.
pub fn widened(was: PlaylistKinds, asked: PlaylistKinds) -> PlaylistKinds {
    PlaylistKinds {
        movies: was.movies || asked.movies,
        episodes: was.episodes || asked.episodes,
        tracks: was.tracks || asked.tracks,
    }
}

/// What a Plex import made, or would make, in one line.
pub fn import_line(import: &PlexPlaylistImport, dry_run: bool) -> String {
    let entries: u32 = import.made.iter().map(|m| m.entries).sum();
    let lists = count(import.made.len() as u64, "playlist");
    let items = count(u64::from(entries), "item");
    let missed = import.unmapped.len();
    let made = if dry_run {
        format!("{lists} with {items} would be made.")
    } else {
        format!("{lists} with {items} made.")
    };
    match missed {
        0 => made,
        1 => format!("{made} 1 thing didn't map."),
        n => format!("{made} {n} things didn't map."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPISODES: PlaylistKinds = PlaylistKinds {
        movies: false,
        episodes: true,
        tracks: false,
    };
    const MUSIC: PlaylistKinds = PlaylistKinds {
        movies: false,
        episodes: false,
        tracks: true,
    };

    fn playlist(
        name: &str,
        kinds: PlaylistKinds,
        owner: PlaylistOwner,
        can_edit: bool,
    ) -> PlaylistInfo {
        PlaylistInfo {
            id: name.to_lowercase(),
            name: name.into(),
            kinds,
            owner,
            sharing: PlaylistSharing::Personal,
            can_edit,
            can_manage: can_edit,
            revision: 0,
            builtin: None,
            artwork: None,
            custom_art: false,
        }
    }

    #[test]
    fn a_cover_s_address_names_the_playlist_and_the_picture() {
        assert_eq!(
            cover_src("0192-ab", Some("image:playlist/0f3a"), "thumb"),
            "/playlists/art?id=0192-ab&size=thumb&art=image%3Aplaylist%2F0f3a"
        );
        assert_eq!(
            cover_src("x&y", None, "medium"),
            "/playlists/art?id=x%26y&size=medium"
        );
        assert_eq!(cover_upload("a b"), "/playlists/art?id=a%20b");
    }

    #[test]
    fn only_an_editor_changes_a_cover_and_never_liked_s() {
        let mut p = playlist("Heists", EPISODES, PlaylistOwner::Mine, true);
        assert!(may_change_cover(&p));
        p.builtin = Some(PlaylistBuiltin::Liked);
        assert!(!may_change_cover(&p));
        let viewer = playlist("Heists", EPISODES, PlaylistOwner::SharedWithMe, false);
        assert!(!may_change_cover(&viewer));
    }

    #[test]
    fn a_cover_past_20_mb_is_refused_before_it_is_sent() {
        assert_eq!(cover_refused(20 << 20), None);
        assert!(cover_refused(25_000_000).unwrap().contains("25.0 MB"));
    }

    fn user(id: &str, name: &str) -> ShareableUser {
        ShareableUser {
            user_id: id.into(),
            display_name: name.into(),
        }
    }

    #[test]
    fn a_film_is_offered_no_music_playlist_nor_one_it_cannot_change() {
        let list = [
            playlist("Heists", EPISODES, PlaylistOwner::Mine, true),
            playlist(
                "Films",
                PlaylistKinds {
                    movies: true,
                    ..EPISODES
                },
                PlaylistOwner::Mine,
                true,
            ),
            playlist("Road trip", MUSIC, PlaylistOwner::Mine, true),
            playlist(
                "Everyone's films",
                PlaylistKinds {
                    movies: true,
                    ..MUSIC
                },
                PlaylistOwner::ServerWide,
                false,
            ),
        ];
        let names = |kind| -> Vec<String> {
            add_targets(&list, kind)
                .into_iter()
                .map(|p| p.name)
                .collect()
        };
        assert_eq!(names(CatalogKind::Movie), ["Films"]);
        // A season adds its episodes; an album its tracks.
        assert_eq!(names(CatalogKind::Season), ["Heists", "Films"]);
        assert_eq!(names(CatalogKind::Album), ["Road trip"]);
    }

    #[test]
    fn kinds_are_named_in_one_line() {
        assert_eq!(kinds_line(EPISODES), "Episodes");
        assert_eq!(
            kinds_line(PlaylistKinds {
                movies: true,
                episodes: true,
                tracks: true
            }),
            "Movies, episodes and music"
        );
        assert_eq!(kinds_line(PlaylistKinds::default()), "Nothing");
    }

    #[test]
    fn the_header_counts_playable_entries_only_when_some_are_not() {
        assert_eq!(count_line(EPISODES, 18, 17), "18 episodes · 17 playable");
        assert_eq!(count_line(EPISODES, 1, 1), "1 episode");
        assert_eq!(
            count_line(
                PlaylistKinds {
                    movies: true,
                    ..MUSIC
                },
                3,
                3
            ),
            "3 items"
        );
    }

    #[test]
    fn sharing_is_said_with_names() {
        let people = [user("local:b", "Bob"), user("local:c", "Carol")];
        let shared = |ids: &[&str]| PlaylistSharing::Shared {
            with: ids
                .iter()
                .map(|id| PlaylistShare {
                    user_id: (*id).into(),
                    can_edit: false,
                })
                .collect(),
        };
        assert_eq!(
            sharing_line(&PlaylistSharing::Personal, &people),
            "Only you"
        );
        assert_eq!(sharing_line(&shared(&["local:b"]), &people), "You and Bob");
        assert_eq!(
            sharing_line(&shared(&["local:b", "local:gone"]), &people),
            "You, Bob and someone"
        );
        assert_eq!(sharing_line(&shared(&[]), &people), "Shared with you");
        assert_eq!(
            sharing_line(&PlaylistSharing::ServerWide, &people),
            "Everyone on this server"
        );
    }

    #[test]
    fn the_sharing_dialog_reads_and_writes_each_user_s_access() {
        let people = [user("local:b", "Bob"), user("local:c", "Carol")];
        let now = PlaylistSharing::Shared {
            with: vec![
                PlaylistShare {
                    user_id: "local:c".into(),
                    can_edit: true,
                },
                PlaylistShare {
                    user_id: "local:gone".into(),
                    can_edit: false,
                },
            ],
        };
        let mut rows = share_rows(&now, &people);
        let access: Vec<(&str, Access)> =
            rows.iter().map(|(u, a)| (u.user_id.as_str(), *a)).collect();
        assert_eq!(
            access,
            [
                ("local:b", Access::None),
                ("local:c", Access::Edit),
                ("local:gone", Access::View)
            ]
        );
        rows[0].1 = Access::View;
        rows[2].1 = Access::None;
        assert_eq!(
            sharing_of(&rows),
            PlaylistSharing::Shared {
                with: vec![
                    PlaylistShare {
                        user_id: "local:b".into(),
                        can_edit: false
                    },
                    PlaylistShare {
                        user_id: "local:c".into(),
                        can_edit: true
                    },
                ]
            }
        );
        for row in &mut rows {
            row.1 = Access::None;
        }
        assert_eq!(sharing_of(&rows), PlaylistSharing::Personal);
        assert_eq!(Access::parse(Access::Edit.value()), Access::Edit);
    }

    #[test]
    fn my_playlists_are_listed_apart_from_shared_and_server_wide_ones() {
        let list = [
            playlist("A", EPISODES, PlaylistOwner::ServerWide, false),
            playlist("B", EPISODES, PlaylistOwner::Mine, true),
            playlist("C", EPISODES, PlaylistOwner::SharedWithMe, false),
        ];
        let [mine, shared, everyone] = sections(&list);
        assert_eq!(mine[0].name, "B");
        assert_eq!(shared[0].name, "C");
        assert_eq!(everyone[0].name, "A");
    }

    fn entries(ids: &[u64]) -> Vec<PlaylistEntryInfo> {
        // Any item: only the entries' ids are read.
        let film = spoolframe_admin_api::Item::Show(spoolframe_admin_api::Show {
            id: "plex:show:1".parse().unwrap(),
            title: "Brooklyn Nine-Nine".into(),
            year: Some(2013),
            summary: None,
            content_rating: None,
            season_count: None,
            genres: Vec::new(),
            last_aired: None,
            cast: Vec::new(),
            artwork: Vec::new(),
        });
        ids.iter()
            .map(|&entry| PlaylistEntryInfo {
                entry,
                item: film.clone(),
                unavailable: false,
                liked: false,
            })
            .collect()
    }

    #[test]
    fn moving_an_entry_names_the_one_it_goes_before() {
        let list = entries(&[10, 20, 30]);
        assert_eq!(move_target(&list, 1, true, false), Some(Some(10)));
        assert_eq!(move_target(&list, 0, true, false), None);
        assert_eq!(move_target(&list, 0, false, false), Some(Some(30)));
        // Down from the next to last is to the end, of the last page.
        assert_eq!(move_target(&list, 1, false, false), Some(None));
        assert_eq!(move_target(&list, 1, false, true), None);
        assert_eq!(move_target(&list, 2, false, false), None);
    }

    /// A show's page lists its playlists after its seasons, each with
    /// its cover and a word on whose it is; any other item keeps the card.
    #[test]
    fn a_show_s_playlists_are_rows_after_its_seasons() {
        let reply = serde_json::json!({
            "type": "item",
            "playlists": [
                { "id": "p1", "name": "Heists", "owner_kind": "mine",
                  "artwork": "image:playlist/0123456789abcdef0123456789abcdef" },
                { "id": "p2", "name": "Crew picks", "owner_kind": "server_wide" },
                { "id": "p3", "name": "Bob's", "owner_kind": "shared_with_me",
                  "owner_name": "Bob" },
            ],
        });
        let held = held_in(&reply);
        assert_eq!(
            held[0].artwork.as_deref(),
            Some("image:playlist/0123456789abcdef0123456789abcdef")
        );
        assert_eq!(held[1].artwork, None);
        assert_eq!(
            held.iter().map(held_line).collect::<Vec<_>>(),
            ["Your playlist", "Server-wide playlist", "Bob's playlist"]
        );
        assert!(held_as_seasons(CatalogKind::Show));
        for kind in [
            CatalogKind::Movie,
            CatalogKind::Season,
            CatalogKind::Episode,
            CatalogKind::Album,
            CatalogKind::Artist,
        ] {
            assert!(!held_as_seasons(kind), "{kind:?}");
        }
    }

    #[test]
    fn an_item_s_details_list_the_playlists_holding_it() {
        let reply = serde_json::json!({
            "type": "item",
            "playlists": [{ "id": "p1", "name": "Heists", "owner_kind": "mine" }],
        });
        assert_eq!(
            held_in(&reply),
            [HeldIn {
                id: "p1".into(),
                name: "Heists".into(),
                owner_kind: "mine".into(),
                builtin: None,
                owner_name: None,
                artwork: None,
            }]
        );
        assert!(held_in(&serde_json::json!({ "type": "item" })).is_empty());
    }

    #[test]
    fn liked_and_all_are_never_offered_to_add_to() {
        let mut liked = playlist("Liked", MUSIC, PlaylistOwner::Mine, true);
        liked.builtin = Some(PlaylistBuiltin::Liked);
        let list = [
            liked,
            playlist("Road trip", MUSIC, PlaylistOwner::Mine, true),
        ];
        let names: Vec<String> = add_targets(&list, CatalogKind::Track)
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["Road trip"]);
    }

    #[test]
    fn a_heart_is_read_from_an_item_s_details() {
        let album = serde_json::json!({
            "liked": "some",
            "liked_children": ["plex:track:32"],
        });
        assert_eq!(Heart::of_reply(&album), Some(Heart::Half));
        assert_eq!(liked_children(&album), ["plex:track:32"]);
        let film = serde_json::json!({ "type": "item" });
        assert_eq!(Heart::of_reply(&film), None);
        assert!(liked_children(&film).is_empty());
        assert_eq!(
            Heart::of_reply(&serde_json::json!({ "liked": "all" })),
            Some(Heart::Full)
        );
    }

    #[test]
    fn a_full_heart_unlikes_and_any_other_likes() {
        assert!(!Heart::Full.likes());
        assert!(Heart::Half.likes());
        assert!(Heart::Empty.likes());
        assert_eq!(
            Heart::Half.action(CatalogKind::Album),
            "Like the other tracks"
        );
        assert_eq!(Heart::Full.action(CatalogKind::Track), "Unlike");
        assert_eq!(Heart::of_track(true), Heart::Full);
    }

    #[test]
    fn a_builtin_s_empty_page_says_how_it_fills() {
        assert!(empty_text(Some(PlaylistBuiltin::Liked), false).contains("heart"));
        assert!(empty_text(None, true).contains("Libraries"));
        assert!(!empty_text(None, false).contains("Libraries"));
    }

    #[test]
    fn kinds_only_widen() {
        assert_eq!(
            widened(EPISODES, MUSIC),
            PlaylistKinds {
                movies: false,
                episodes: true,
                tracks: true
            }
        );
    }

    #[test]
    fn an_import_is_summed_up() {
        let import = PlexPlaylistImport {
            made: vec![
                spoolframe_admin_api::PlexPlaylistMade {
                    name: "Heists".into(),
                    playlist_id: None,
                    entries: 2,
                },
                spoolframe_admin_api::PlexPlaylistMade {
                    name: "Road trip".into(),
                    playlist_id: None,
                    entries: 1,
                },
            ],
            unmapped: vec![spoolframe_admin_api::PlexPlaylistUnmapped {
                playlist: "Heists".into(),
                title: Some("A trailer".into()),
                reason: "a clip is not something Spoolframe plays".into(),
            }],
            notes: Vec::new(),
        };
        assert_eq!(
            import_line(&import, true),
            "2 playlists with 3 items would be made. 1 thing didn't map."
        );
    }
}
