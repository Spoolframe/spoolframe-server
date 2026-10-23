//! Items between the catalog and federation/1 (P8-02): what the sharing
//! server sends of an item ([`remote`]), and what the caching server
//! makes of it, every id named as its origin's ([`local`]).
//!
//! What crosses: titles, the hierarchy, years and dates, summaries,
//! ratings, durations, counts, genres, artwork ids and external ids. What
//! does not: cast (person ids are the origin's, and a person's items are
//! not shared), a track's audio facts (the origin plans every play), and
//! playback state (the caching server keeps its own users').

use std::time::Duration;

use spoolframe_protocol::federation::{RemoteArtwork, RemoteArtworkKind, RemoteItem, RemoteKind};
use spoolframe_types::{
    Album, Artist, Artwork, ArtworkId, ArtworkKind, CalendarDate, Episode, Item, MediaId,
    MediaKind, Movie, PlaybackState, Season, Show, Track,
};

fn artwork(artwork: &[Artwork]) -> Vec<RemoteArtwork> {
    artwork
        .iter()
        .map(|a| RemoteArtwork {
            id: a.id.to_string(),
            kind: match a.kind {
                ArtworkKind::Poster => RemoteArtworkKind::Poster,
                ArtworkKind::Background => RemoteArtworkKind::Background,
                ArtworkKind::Thumb => RemoteArtworkKind::Thumb,
            },
        })
        .collect()
}

fn ms(duration: Option<Duration>) -> Option<u64> {
    duration.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn date(date: Option<CalendarDate>) -> Option<String> {
    date.map(|d| d.to_string())
}

/// `item` as federation/1 carries it, with `external_ids`, and hashed.
pub fn remote(item: &Item, external_ids: Vec<String>) -> RemoteItem {
    let blank = |id: &MediaId, kind, title: &str| RemoteItem {
        id: id.to_string(),
        kind,
        title: title.to_owned(),
        parent: None,
        grandparent: None,
        parent_title: None,
        grandparent_title: None,
        index: None,
        parent_index: None,
        year: None,
        date: None,
        summary: None,
        content_rating: None,
        duration_ms: None,
        count: None,
        genres: Vec::new(),
        artwork: Vec::new(),
        external_ids: Vec::new(),
        hash: String::new(),
    };
    let remote = match item {
        Item::Movie(m) => RemoteItem {
            year: m.year,
            date: date(m.release_date),
            summary: m.summary.clone(),
            content_rating: m.content_rating.clone(),
            duration_ms: ms(m.duration),
            artwork: artwork(&m.artwork),
            ..blank(&m.id, RemoteKind::Movie, &m.title)
        },
        Item::Show(s) => RemoteItem {
            year: s.year,
            date: date(s.last_aired),
            summary: s.summary.clone(),
            content_rating: s.content_rating.clone(),
            count: s.season_count,
            genres: s.genres.clone(),
            artwork: artwork(&s.artwork),
            ..blank(&s.id, RemoteKind::Show, &s.title)
        },
        Item::Season(s) => RemoteItem {
            parent: Some(s.show_id.to_string()),
            parent_title: s.show_title.clone(),
            index: Some(s.season),
            artwork: artwork(&s.artwork),
            ..blank(&s.id, RemoteKind::Season, &s.title)
        },
        Item::Episode(e) => RemoteItem {
            parent: Some(e.season_id.to_string()),
            grandparent: Some(e.show_id.to_string()),
            parent_title: e.show_title.clone(),
            index: Some(e.episode),
            parent_index: Some(e.season),
            year: e.year,
            date: date(e.air_date),
            summary: e.summary.clone(),
            content_rating: e.content_rating.clone(),
            duration_ms: ms(e.duration),
            artwork: artwork(&e.artwork),
            ..blank(&e.id, RemoteKind::Episode, &e.title)
        },
        Item::Artist(a) => RemoteItem {
            summary: a.summary.clone(),
            artwork: artwork(&a.artwork),
            ..blank(&a.id, RemoteKind::Artist, &a.name)
        },
        Item::Album(a) => RemoteItem {
            parent: Some(a.artist_id.to_string()),
            parent_title: a.artist_name.clone(),
            year: a.year,
            count: a.track_count,
            artwork: artwork(&a.artwork),
            ..blank(&a.id, RemoteKind::Album, &a.title)
        },
        Item::Track(t) => RemoteItem {
            parent: Some(t.album_id.to_string()),
            grandparent: Some(t.artist_id.to_string()),
            parent_title: t.album_title.clone(),
            grandparent_title: t.artist_name.clone(),
            index: t.track,
            parent_index: t.disc,
            count: t.album_track_count,
            duration_ms: ms(t.duration),
            artwork: artwork(&t.artwork),
            ..blank(&t.id, RemoteKind::Track, &t.title)
        },
    };
    RemoteItem {
        external_ids,
        ..remote
    }
    .hashed()
}

/// The kind a remote kind is here; `None` for one this build does not
/// know.
pub fn kind(kind: RemoteKind) -> Option<MediaKind> {
    Some(match kind {
        RemoteKind::Movie => MediaKind::Movie,
        RemoteKind::Show => MediaKind::Show,
        RemoteKind::Season => MediaKind::Season,
        RemoteKind::Episode => MediaKind::Episode,
        RemoteKind::Artist => MediaKind::Artist,
        RemoteKind::Album => MediaKind::Album,
        RemoteKind::Track => MediaKind::Track,
        RemoteKind::Unknown => return None,
    })
}

/// Origin `origin`'s item `id`, of `kind`, as this server names it.
pub fn media_id(origin: &str, kind: MediaKind, id: &str) -> Option<MediaId> {
    let remote: MediaId = id.parse().ok()?;
    (remote.kind() == kind)
        .then(|| MediaId::federated(origin, &remote).ok())
        .flatten()
}

/// Origin `origin`'s artwork `id`, as this server names it.
pub fn artwork_id(origin: &str, id: &str) -> Option<ArtworkId> {
    ArtworkId::federated(origin, &id.parse().ok()?).ok()
}

fn local_artwork(origin: &str, artwork: &[RemoteArtwork]) -> Vec<Artwork> {
    artwork
        .iter()
        .filter_map(|a| {
            Some(Artwork {
                id: artwork_id(origin, &a.id)?,
                kind: match a.kind {
                    RemoteArtworkKind::Poster => ArtworkKind::Poster,
                    RemoteArtworkKind::Background => ArtworkKind::Background,
                    RemoteArtworkKind::Thumb => ArtworkKind::Thumb,
                    RemoteArtworkKind::Unknown => return None,
                },
            })
        })
        .collect()
}

/// `remote`, from server `origin`, as this server serves it: every id in
/// it the origin's ([`MediaId::federated`]), no playback state. `None`
/// for an item this build cannot read: an unknown kind, an id that is
/// not one, a parent missing where its kind has one.
pub fn local(origin: &str, remote: &RemoteItem) -> Option<Item> {
    let kind = kind(remote.kind)?;
    let id = media_id(origin, kind, &remote.id)?;
    let parent = |kind| media_id(origin, kind, remote.parent.as_deref()?);
    let grandparent = |kind| media_id(origin, kind, remote.grandparent.as_deref()?);
    let date = remote.date.as_deref().and_then(|d| d.parse().ok());
    let duration = remote.duration_ms.map(Duration::from_millis);
    let artwork = local_artwork(origin, &remote.artwork);
    let title = remote.title.clone();
    Some(match kind {
        MediaKind::Movie => Item::Movie(Movie {
            id,
            title,
            year: remote.year,
            release_date: date,
            summary: remote.summary.clone(),
            content_rating: remote.content_rating.clone(),
            duration,
            cast: Vec::new(),
            artwork,
            playback: PlaybackState::default(),
        }),
        MediaKind::Show => Item::Show(Show {
            id,
            title,
            year: remote.year,
            summary: remote.summary.clone(),
            content_rating: remote.content_rating.clone(),
            season_count: remote.count,
            genres: remote.genres.clone(),
            last_aired: date,
            cast: Vec::new(),
            artwork,
        }),
        MediaKind::Season => Item::Season(Season {
            id,
            show_id: parent(MediaKind::Show)?,
            show_title: remote.parent_title.clone(),
            season: remote.index.unwrap_or_default(),
            title,
            artwork,
        }),
        MediaKind::Episode => Item::Episode(Episode {
            id,
            show_id: grandparent(MediaKind::Show)?,
            show_title: remote.parent_title.clone(),
            season_id: parent(MediaKind::Season)?,
            season: remote.parent_index.unwrap_or_default(),
            episode: remote.index.unwrap_or_default(),
            title,
            summary: remote.summary.clone(),
            content_rating: remote.content_rating.clone(),
            year: remote.year,
            air_date: date,
            duration,
            cast: Vec::new(),
            artwork,
            playback: PlaybackState::default(),
        }),
        MediaKind::Artist => Item::Artist(Artist {
            id,
            name: title,
            summary: remote.summary.clone(),
            artwork,
        }),
        MediaKind::Album => Item::Album(Album {
            id,
            artist_id: parent(MediaKind::Artist)?,
            artist_name: remote.parent_title.clone(),
            title,
            year: remote.year,
            track_count: remote.count,
            artwork,
        }),
        MediaKind::Track => Item::Track(Track {
            id,
            album_id: parent(MediaKind::Album)?,
            artist_id: grandparent(MediaKind::Artist)?,
            artist_name: remote.grandparent_title.clone(),
            album_title: remote.parent_title.clone(),
            title,
            track: remote.index,
            disc: remote.parent_index,
            album_track_count: remote.count,
            codec: None,
            bit_depth: None,
            sample_rate: None,
            duration,
            artwork,
            playback: PlaybackState::default(),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{album, artist, episode, movie, show, track};

    const ORIGIN: &str = "mzxw6ytbojtg633c";

    /// What the origin's item becomes here: the same item, every id the
    /// origin's.
    fn federated(item: Item) -> Item {
        let f = |id: &MediaId| MediaId::federated(ORIGIN, id).unwrap();
        let a = |artwork: Vec<Artwork>| {
            artwork
                .into_iter()
                .map(|a| Artwork {
                    id: ArtworkId::federated(ORIGIN, &a.id).unwrap(),
                    ..a
                })
                .collect()
        };
        match item {
            Item::Movie(m) => Item::Movie(Movie {
                id: f(&m.id),
                cast: Vec::new(),
                artwork: a(m.artwork),
                ..m
            }),
            Item::Show(s) => Item::Show(Show {
                id: f(&s.id),
                cast: Vec::new(),
                artwork: a(s.artwork),
                ..s
            }),
            Item::Season(s) => Item::Season(Season {
                id: f(&s.id),
                show_id: f(&s.show_id),
                artwork: a(s.artwork),
                ..s
            }),
            Item::Episode(e) => Item::Episode(Episode {
                id: f(&e.id),
                show_id: f(&e.show_id),
                season_id: f(&e.season_id),
                cast: Vec::new(),
                artwork: a(e.artwork),
                ..e
            }),
            Item::Artist(x) => Item::Artist(Artist {
                id: f(&x.id),
                artwork: a(x.artwork),
                ..x
            }),
            Item::Album(x) => Item::Album(Album {
                id: f(&x.id),
                artist_id: f(&x.artist_id),
                artwork: a(x.artwork),
                ..x
            }),
            Item::Track(t) => Item::Track(Track {
                id: f(&t.id),
                album_id: f(&t.album_id),
                artist_id: f(&t.artist_id),
                artwork: a(t.artwork),
                ..t
            }),
        }
    }

    /// Every kind crosses whole but for what is not sent (cast, a track's
    /// audio facts), and comes back named as the origin's.
    #[test]
    fn every_kind_crosses_and_comes_back_as_the_origin_s() {
        let mut with_poster = movie();
        if let Item::Movie(m) = &mut with_poster {
            m.artwork.push(Artwork {
                id: crate::test_support::poster(),
                kind: ArtworkKind::Poster,
            });
        }
        for item in [
            with_poster,
            show(),
            episode("12", "11", 1, 1),
            artist(),
            album("31"),
            track("33", "31", 2),
        ] {
            let sent = remote(&item, vec!["tmdb://348".into()]);
            assert_eq!(sent.external_ids, ["tmdb://348"]);
            assert_eq!(sent.hash, sent.digest());
            assert_eq!(local(ORIGIN, &sent), Some(federated(item)));
        }
    }

    /// What this build cannot read is left out, not guessed at.
    #[test]
    fn an_item_this_build_cannot_read_is_left_out() {
        let sent = remote(&episode("12", "11", 1, 1), Vec::new());
        assert_eq!(
            local(
                ORIGIN,
                &RemoteItem {
                    kind: RemoteKind::Unknown,
                    ..sent.clone()
                }
            ),
            None
        );
        assert_eq!(
            local(
                ORIGIN,
                &RemoteItem {
                    parent: None,
                    ..sent.clone()
                }
            ),
            None
        );
        assert_eq!(
            local(
                ORIGIN,
                &RemoteItem {
                    id: "plex:movie:12".into(),
                    ..sent
                }
            ),
            None,
            "an id of another kind"
        );
    }
}
