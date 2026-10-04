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

use spoolframe_catalog::{Found, FoundGroup};
use spoolframe_types::{
    Album, Artist, Artwork, ArtworkId, Episode, Item, LibraryId, MediaId, Movie, Person, PersonId,
    Season, Show, Track,
};

use crate::config::FIRST_PLEX_SERVER;

/// The server a key names, and the key Plex knows.
pub fn split(key: &str) -> (&str, &str) {
    match key.split_once(':') {
        Some((server, local)) => (server, local),
        None => (FIRST_PLEX_SERVER, key),
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

fn artwork(server: &str, artwork: Vec<Artwork>) -> Vec<Artwork> {
    artwork
        .into_iter()
        .map(|a| Artwork {
            id: if server == FIRST_PLEX_SERVER {
                a.id
            } else {
                ArtworkId::new(a.id.namespace(), join(server, a.id.key()))
                    .expect("a key with a prefix")
            },
            kind: a.kind,
        })
        .collect()
}

/// `item` from `server`, with every ID in it (its own, its parents', its
/// artwork's) naming that server.
pub fn item(server: &str, item: Item) -> Item {
    if server == FIRST_PLEX_SERVER {
        return item;
    }
    let m = |id| media(server, id);
    match item {
        Item::Movie(v) => Item::Movie(Movie {
            id: m(v.id),
            artwork: artwork(server, v.artwork),
            ..v
        }),
        Item::Show(v) => Item::Show(Show {
            id: m(v.id),
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
            ..v
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_types::{ArtworkKind, MediaKind, Namespace, PlaybackState};

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
            duration: None,
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
}
