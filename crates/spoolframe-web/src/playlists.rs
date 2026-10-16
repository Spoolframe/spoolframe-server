//! Playlists (PX-40): what the pages say of them, which ones an item can
//! be added to, and the edits a move or the sharing dialog makes. Plain
//! logic, tested on the host.

use spoolframe_admin_api::{
    CatalogKind, PlaylistEntryInfo, PlaylistInfo, PlaylistKinds, PlaylistOwner, PlaylistShare,
    PlaylistSharing, PlexPlaylistImport, ShareableUser,
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
/// playlist.
pub fn add_targets(list: &[PlaylistInfo], kind: CatalogKind) -> Vec<PlaylistInfo> {
    list.iter()
        .filter(|p| p.can_edit && takes(p.kinds, kind))
        .cloned()
        .collect()
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
        }
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
                owner_kind: "mine".into()
            }]
        );
        assert!(held_in(&serde_json::json!({ "type": "item" })).is_empty());
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
