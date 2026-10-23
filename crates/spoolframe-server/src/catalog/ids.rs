//! Which Plex server an ID is from (P3-11).
//!
//! The first Plex server's IDs are what they always were: the Plex key
//! after the namespace (`plex:movie:12`, `plex:2`,
//! `plex:/library/metadata/12/thumb/3`). Another server's carry its id in
//! front of the key (`plex:movie:cabin:12`, `plex:cabin:2`,
//! `plex:cabin:/library/metadata/12/thumb/3`). A Plex key never has a `:`
//! in it, so a key with one names a server; the IDs clients and the
//! database already hold keep working, and clients, which treat IDs as
//! opaque, need nothing new.

use spoolframe_catalog::{Found, FoundGroup, ItemWithChildren};
use spoolframe_types::{
    Album, Artist, Artwork, ArtworkId, CastMember, Episode, Item, LibraryId, MediaId, Movie,
    Person, PersonId, Season, Show, Track,
};

use crate::config::FIRST_PLEX_SERVER;

/// The server a key names, and the key Plex knows.
///
/// The first server's keys carry no prefix, so `plex:12` is not its key
/// `12` under a second spelling (B-11): served as one, it would be a
/// second key for that item's watch state. It stays whole, and the first
/// server, whose keys are digits, has nothing under it.
pub fn split(key: &str) -> (&str, &str) {
    match key.split_once(':') {
        Some((server, local)) if server != FIRST_PLEX_SERVER => (server, local),
        _ => (FIRST_PLEX_SERVER, key),
    }
}

/// `key` of a thing on `server`, as Spoolframe's IDs carry it.
fn join(server: &str, key: &str) -> String {
    if server == FIRST_PLEX_SERVER {
        key.to_owned()
    } else {
        format!("{server}:{key}")
    }
}

/// The server `id` is from, and the ID Plex's catalog knows it by.
pub fn media_local(id: &MediaId) -> (&str, MediaId) {
    let (server, key) = split(id.key());
    let local = MediaId::new(id.namespace(), id.kind(), key).unwrap_or_else(|_| id.clone());
    (server, local)
}

pub fn library_local(id: &LibraryId) -> (&str, LibraryId) {
    let (server, key) = split(id.key());
    let local = LibraryId::new(id.namespace(), key).unwrap_or_else(|_| id.clone());
    (server, local)
}

pub fn artwork_local(id: &ArtworkId) -> (&str, ArtworkId) {
    let (server, key) = split(id.key());
    let local = ArtworkId::new(id.namespace(), key).unwrap_or_else(|_| id.clone());
    (server, local)
}

pub fn person_local(id: &PersonId) -> (&str, PersonId) {
    let (server, key) = split(id.key());
    let local = PersonId::new(id.namespace(), key).unwrap_or_else(|_| id.clone());
    (server, local)
}

pub fn person(server: &str, id: PersonId) -> PersonId {
    if server == FIRST_PLEX_SERVER {
        return id;
    }
    PersonId::new(id.namespace(), join(server, id.key())).expect("a key with a prefix")
}

/// A fetched item and its children, every ID in them naming `server`.
pub fn item_with_children(server: &str, found: ItemWithChildren) -> ItemWithChildren {
    ItemWithChildren {
        item: item(server, found.item),
        children: found
            .children
            .into_iter()
            .map(|c| item(server, c))
            .collect(),
        ..found
    }
}

/// What a search on `server` found, every ID in it naming that server.
pub fn found(server: &str, found: Found) -> Found {
    Found {
        groups: found
            .groups
            .into_iter()
            .map(|g| FoundGroup {
                items: g.items.into_iter().map(|i| item(server, i)).collect(),
                people: g
                    .people
                    .into_iter()
                    .map(|p| Person {
                        id: person(server, p.id),
                        ..p
                    })
                    .collect(),
                ..g
            })
            .collect(),
        titles: found
            .titles
            .into_iter()
            .map(|(id, title)| (media(server, id), title))
            .collect(),
    }
}

pub fn media(server: &str, id: MediaId) -> MediaId {
    if server == FIRST_PLEX_SERVER {
        return id;
    }
    MediaId::new(id.namespace(), id.kind(), join(server, id.key())).expect("a key with a prefix")
}

pub fn library(server: &str, id: LibraryId) -> LibraryId {
    if server == FIRST_PLEX_SERVER {
        return id;
    }
    LibraryId::new(id.namespace(), join(server, id.key())).expect("a key with a prefix")
}

fn artwork_id(server: &str, id: ArtworkId) -> ArtworkId {
    if server == FIRST_PLEX_SERVER {
        return id;
    }
    ArtworkId::new(id.namespace(), join(server, id.key())).expect("a key with a prefix")
}

fn artwork(server: &str, artwork: Vec<Artwork>) -> Vec<Artwork> {
    artwork
        .into_iter()
        .map(|a| Artwork {
            id: artwork_id(server, a.id),
            kind: a.kind,
        })
        .collect()
}

/// An item's cast from `server`: each person's id lists their items
/// there, and their photo is fetched from there.
fn cast(server: &str, cast: Vec<CastMember>) -> Vec<CastMember> {
    cast.into_iter()
        .map(|c| CastMember {
            id: person(server, c.id),
            photo: c.photo.map(|p| artwork_id(server, p)),
            ..c
        })
        .collect()
}

/// `item` from `server`, with every ID in it (its own, its parents', its
/// artwork's, its cast's) naming that server.
pub fn item(server: &str, item: Item) -> Item {
    if server == FIRST_PLEX_SERVER {
        return item;
    }
    let m = |id| media(server, id);
    match item {
        Item::Movie(v) => Item::Movie(Movie {
            id: m(v.id),
            cast: cast(server, v.cast),
            artwork: artwork(server, v.artwork),
            ..v
        }),
        Item::Show(v) => Item::Show(Show {
            id: m(v.id),
            cast: cast(server, v.cast),
            artwork: artwork(server, v.artwork),
            ..v
        }),
        Item::Season(v) => Item::Season(Season {
            id: m(v.id),
            show_id: m(v.show_id),
            artwork: artwork(server, v.artwork),
            ..v
        }),
        Item::Episode(v) => Item::Episode(Episode {
            id: m(v.id),
            show_id: m(v.show_id),
            season_id: m(v.season_id),
            cast: cast(server, v.cast),
            artwork: artwork(server, v.artwork),
            ..v
        }),
        Item::Artist(v) => Item::Artist(Artist {
            id: m(v.id),
            artwork: artwork(server, v.artwork),
            ..v
        }),
        Item::Album(v) => Item::Album(Album {
            id: m(v.id),
            artist_id: m(v.artist_id),
            artwork: artwork(server, v.artwork),
            ..v
        }),
        Item::Track(v) => Item::Track(Track {
            id: m(v.id),
            album_id: m(v.album_id),
            artist_id: m(v.artist_id),
            artwork: artwork(server, v.artwork),
            ..v
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_types::{
        ArtworkKind, CastMember, MediaKind, Namespace, PersonRole, PlaybackState,
    };

    #[test]
    fn the_first_server_s_ids_are_as_they_were() {
        let id: MediaId = "plex:episode:229060".parse().unwrap();
        assert_eq!(media_local(&id), ("plex", id.clone()));
        assert_eq!(media("plex", id.clone()), id);
        let library: LibraryId = "plex:2".parse().unwrap();
        assert_eq!(library_local(&library), ("plex", library.clone()));
        let art: ArtworkId = "plex:/library/metadata/1/thumb/2".parse().unwrap();
        assert_eq!(artwork_local(&art), ("plex", art.clone()));
    }

    /// B-11: the first server's ids carry no prefix, so `plex:movie:plex:12`
    /// is not its item 12 under a second spelling. Served as one, it was
    /// a second key for that item's watch state.
    #[test]
    fn the_first_server_s_name_is_not_a_prefix() {
        let alias: MediaId = "plex:movie:plex:12".parse().unwrap();
        assert_eq!(media_local(&alias), ("plex", alias.clone()));
    }

    #[test]
    fn another_server_s_ids_carry_its_id_both_ways() {
        let local: MediaId = "plex:episode:12".parse().unwrap();
        let ours = media("cabin", local.clone());
        assert_eq!(ours.to_string(), "plex:episode:cabin:12");
        assert_eq!(media_local(&ours), ("cabin", local));
        let library = library("cabin", "plex:3".parse().unwrap());
        assert_eq!(library.to_string(), "plex:cabin:3");
        assert_eq!(library_local(&library).0, "cabin");
        let art: ArtworkId = "plex:cabin:/library/metadata/1/thumb/2".parse().unwrap();
        assert_eq!(
            artwork_local(&art),
            ("cabin", "plex:/library/metadata/1/thumb/2".parse().unwrap())
        );
    }

    #[test]
    fn a_person_s_id_names_their_server_both_ways() {
        let local: PersonId = "plex:actor-134469".parse().unwrap();
        assert_eq!(person("plex", local.clone()), local);
        let ours = person("cabin", local.clone());
        assert_eq!(ours.to_string(), "plex:cabin:actor-134469");
        assert_eq!(person_local(&ours), ("cabin", local));
    }

    #[test]
    fn every_id_in_an_item_names_its_server() {
        let id = |kind, key: &str| MediaId::new(Namespace::Plex, kind, key).unwrap();
        let episode = Item::Episode(Episode {
            id: id(MediaKind::Episode, "3"),
            show_id: id(MediaKind::Show, "1"),
            show_title: None,
            season_id: id(MediaKind::Season, "2"),
            season: 1,
            episode: 1,
            title: "Pilot".into(),
            summary: None,
            content_rating: None,
            year: None,
            air_date: None,
            duration: None,
            cast: Vec::new(),
            artwork: vec![Artwork {
                id: "plex:/library/metadata/3/thumb/9".parse().unwrap(),
                kind: ArtworkKind::Thumb,
            }],
            playback: PlaybackState::default(),
        });
        let Item::Episode(e) = item("cabin", episode) else {
            unreachable!()
        };
        assert_eq!(e.id.to_string(), "plex:episode:cabin:3");
        assert_eq!(e.show_id.to_string(), "plex:show:cabin:1");
        assert_eq!(e.season_id.to_string(), "plex:season:cabin:2");
        assert_eq!(
            e.artwork[0].id.to_string(),
            "plex:cabin:/library/metadata/3/thumb/9"
        );
    }

    /// A cast member's id lists their items on the server the item is
    /// from, and their photo is fetched from it.
    #[test]
    fn a_cast_member_s_id_and_photo_name_the_item_s_server() {
        let movie = Item::Movie(Movie {
            id: MediaId::new(Namespace::Plex, MediaKind::Movie, "3").unwrap(),
            title: "Alien".into(),
            year: None,
            release_date: None,
            summary: None,
            content_rating: None,
            duration: None,
            cast: vec![CastMember {
                id: "plex:actor-13434".parse().unwrap(),
                name: "Sigourney Weaver".into(),
                role: PersonRole::Actor,
                character: Some("Ripley".into()),
                photo: Some("plex:/metadata-static/9/people/9e3c.jpg".parse().unwrap()),
            }],
            artwork: Vec::new(),
            playback: PlaybackState::default(),
        });
        let Item::Movie(m) = item("cabin", movie) else {
            unreachable!()
        };
        assert_eq!(m.cast[0].id.to_string(), "plex:cabin:actor-13434");
        assert_eq!(
            m.cast[0].photo.as_ref().map(ToString::to_string).as_deref(),
            Some("plex:cabin:/metadata-static/9/people/9e3c.jpg")
        );
    }

    #[test]
    fn a_track_s_cover_names_its_server() {
        let id = |kind, key: &str| MediaId::new(Namespace::Plex, kind, key).unwrap();
        let track = Item::Track(Track {
            id: id(MediaKind::Track, "3"),
            album_id: id(MediaKind::Album, "2"),
            artist_id: id(MediaKind::Artist, "1"),
            artist_name: Some("Mara Quell".into()),
            album_title: Some("Harbour Lights".into()),
            title: "Low Tide".into(),
            track: Some(2),
            disc: Some(1),
            album_track_count: None,
            codec: None,
            bit_depth: None,
            sample_rate: None,
            duration: None,
            artwork: vec![Artwork {
                id: "plex:/library/metadata/2/thumb/9".parse().unwrap(),
                kind: ArtworkKind::Poster,
            }],
            playback: PlaybackState::default(),
        });
        let Item::Track(t) = item("cabin", track) else {
            unreachable!()
        };
        assert_eq!(t.album_id.to_string(), "plex:album:cabin:2");
        assert_eq!(
            t.artwork[0].id.to_string(),
            "plex:cabin:/library/metadata/2/thumb/9"
        );
    }
}
