//! Home's "Recently played" row (feature `spoolframe:recently-played-row`),
//! through the SDK: the source each start path sends is kept with its
//! play, and `RecentlyPlayedRow` makes one row of every kind from the
//! plays, as the user's preferences say.
//!
//! The music server has tracks 32 and 33 on album 31 and track 35 on
//! album 34 ([`crate::sdk_music::music_server`]); its catalog also has
//! film 1, and show 10 with episodes 12 and 13 in season 11 and 15 in
//! season 14.

use std::sync::Arc;
use std::time::Duration;

use spoolframe_client_sdk::gate::RecentlyPlayedRow;
use spoolframe_client_sdk::{
    Client, MemoryStorage, On, PlaySource, PlaybackOptions, RecentCard, RecentMoviesMode,
    RecentMusicMode, RecentShowsMode, RecentlyPlayedPreferences, Session,
};
use spoolframe_protocol::playlists::{Kinds, Visibility};
use spoolframe_server::principal::Principal;
use spoolframe_server::store::plays::{NewPlay, PlayRow, PlayUpdate, PlayedKind};
use spoolframe_test_dir::TestDir;
use spoolframe_types::{MediaId, MediaKind, Namespace};

use crate::downloads_wire::PASSWORD;
use crate::music_downloads::track_id;
use crate::sdk_music::{alice, music_server, options};
use crate::support::{TestServer, pinned_sdk_client};

async fn device(server: &TestServer, downloads: &TestDir) -> Client {
    let device = spoolframe_client_sdk::Device {
        name: "Den PC".into(),
        client_name: "sdk-recently-played-test".into(),
        client_version: "0".into(),
        platform: spoolframe_client_sdk::types::Platform::Linux,
        capabilities: spoolframe_client_sdk::types::ClientCapabilities {
            audio: [spoolframe_types::AudioCodec::Flac].into(),
            containers: [spoolframe_types::Container::Flac].into(),
            ..Default::default()
        },
    };
    let storage = Arc::new(MemoryStorage::default());
    pinned_sdk_client(server, &storage, device, options(downloads.path())).await
}

/// The server, Alice's session on it, and the device's client.
async fn signed_in(test: &str) -> (TestServer, Session, Client, TestDir) {
    let server = music_server(test).await;
    let downloads = TestDir::new(&format!("spoolframe-{test}")).await;
    let client = device(&server, &downloads).await;
    let session = alice(&server, &client).await;
    (server, session, client, downloads)
}

async fn principal(server: &TestServer, name: &str) -> Principal {
    let user = server.store.user_by_name(name).await.unwrap().unwrap();
    Principal::LocalUser(user.id)
}

fn media(kind: MediaKind, key: &str) -> String {
    MediaId::new(Namespace::Plex, kind, key)
        .unwrap()
        .to_string()
}

/// What a play is of, as the store keeps it.
enum Of<'a> {
    Movie,
    /// An episode of show 10, in a season (`None`: a play from before
    /// seasons were kept).
    Episode(&'a str, Option<&'a str>),
    Track(&'a str, Option<PlaySource>),
}

/// A counted play by Alice, a second after the one before.
async fn played(server: &TestServer, of: Of<'_>) {
    server.clock.advance(Duration::from_secs(1));
    let who = principal(server, "alice").await;
    let (media_id, kind, show, season, album, source) = match of {
        Of::Movie => (
            media(MediaKind::Movie, "1"),
            PlayedKind::Movie,
            None,
            None,
            None,
            None,
        ),
        Of::Episode(key, season) => (
            media(MediaKind::Episode, key),
            PlayedKind::Episode,
            Some(media(MediaKind::Show, "10")),
            season.map(|s| media(MediaKind::Season, s)),
            None,
            None,
        ),
        Of::Track(key, source) => (
            track_id(key).to_string(),
            PlayedKind::Track,
            None,
            None,
            Some(media(
                MediaKind::Album,
                if key == "35" { "34" } else { "31" },
            )),
            source,
        ),
    };
    let row = server
        .store
        .open_play(
            who,
            NewPlay {
                media_id: &media_id,
                kind,
                show_id: show.as_deref(),
                album_id: album.as_deref(),
                artist_id: None,
                external_ids: &[],
                duration_ms: Some(240_000),
                endpoint_id: None,
                device_name: None,
                playback_id: None,
                source: source.as_ref(),
                season_id: season.as_deref(),
            },
        )
        .await
        .unwrap();
    let counted = PlayUpdate {
        played_ms: 240_000,
        counted: true,
        over_http: false,
        close: true,
    };
    server.store.update_play(row, counted).await.unwrap();
}

fn row_api(session: &Session) -> On<RecentlyPlayedRow> {
    session.feature::<RecentlyPlayedRow>().unwrap()
}

/// Each card as `kind:id`, newest first.
async fn cards(session: &Session) -> Vec<String> {
    row_api(session)
        .recently_played_row(50)
        .await
        .unwrap()
        .iter()
        .map(|card| match card {
            RecentCard::Movie { movie } => format!("movie:{}", movie.id),
            RecentCard::Show { show } => format!("show:{}", show.id),
            RecentCard::Season { season } => format!("season:{}", season.id),
            RecentCard::Episode { episode } => format!("episode:{}", episode.id),
            RecentCard::Album { album } => format!("album:{}", album.id),
            RecentCard::Playlist { playlist } => format!("playlist:{}", playlist.name),
            RecentCard::Track { track } => format!("track:{}", track.id),
            RecentCard::Unknown => "unknown".into(),
        })
        .collect()
}

async fn set(session: &Session, preferences: RecentlyPlayedPreferences) {
    row_api(session)
        .set_recently_played_preferences(preferences)
        .await
        .unwrap();
}

fn tracks() -> Kinds {
    Kinds {
        tracks: true,
        ..Kinds::default()
    }
}

/// Alice's playlist "Road trip", and her plays of music: track 35 from
/// album 34, track 33 from the playlist, and track 32 queued by hand.
async fn three_sources(server: &TestServer, session: &Session) {
    let road_trip = session
        .create_playlist("Road trip", tracks(), Visibility::Personal)
        .await
        .unwrap();
    let from_album = PlaySource::Album {
        id: media(MediaKind::Album, "34").parse().unwrap(),
    };
    played(server, Of::Track("35", Some(from_album))).await;
    let from_playlist = PlaySource::Playlist { id: road_trip.id };
    played(server, Of::Track("33", Some(from_playlist))).await;
    played(server, Of::Track("32", Some(PlaySource::Queued))).await;
}

/// Every kind, oldest first: film 1, episode 12 (season 11), track 32
/// queued, episode 13 (season 11), and episode 15 from before seasons
/// were kept.
async fn every_kind(server: &TestServer) {
    played(server, Of::Movie).await;
    played(server, Of::Episode("12", Some("11"))).await;
    played(server, Of::Track("32", Some(PlaySource::Queued))).await;
    played(server, Of::Episode("13", Some("11"))).await;
    played(server, Of::Episode("15", None)).await;
}

#[tokio::test]
async fn the_defaults_are_by_source_each_episode_and_each_film() {
    let (_server, session, _client, _dir) = signed_in("row-defaults").await;
    assert_eq!(
        row_api(&session)
            .recently_played_preferences()
            .await
            .unwrap(),
        RecentlyPlayedPreferences {
            music: RecentMusicMode::Sources,
            shows: RecentShowsMode::Episode,
            movies: RecentMoviesMode::Movie,
        }
    );
}

/// By source, the default: the track queued by hand, the playlist, then
/// the album, newest first.
#[tokio::test]
async fn by_source_each_play_shows_where_it_was_started() {
    let (server, session, _client, _dir) = signed_in("row-sources").await;
    three_sources(&server, &session).await;
    assert_eq!(
        cards(&session).await,
        [
            "track:plex:track:32",
            "playlist:Road trip",
            "album:plex:album:34"
        ]
    );
}

#[tokio::test]
async fn by_track_the_same_plays_are_each_track() {
    let (server, session, _client, _dir) = signed_in("row-tracks").await;
    three_sources(&server, &session).await;
    set(
        &session,
        RecentlyPlayedPreferences {
            music: RecentMusicMode::Tracks,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        cards(&session).await,
        [
            "track:plex:track:32",
            "track:plex:track:33",
            "track:plex:track:35"
        ]
    );
}

#[tokio::test]
async fn by_album_the_same_plays_fold_into_their_albums() {
    let (server, session, _client, _dir) = signed_in("row-albums").await;
    three_sources(&server, &session).await;
    set(
        &session,
        RecentlyPlayedPreferences {
            music: RecentMusicMode::Albums,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        cards(&session).await,
        ["album:plex:album:31", "album:plex:album:34"]
    );
}

/// A play from before sources were kept has none: it shows its album.
#[tokio::test]
async fn a_play_with_no_source_shows_its_album() {
    let (server, session, _client, _dir) = signed_in("row-old-play").await;
    played(&server, Of::Track("32", None)).await;
    assert_eq!(cards(&session).await, ["album:plex:album:31"]);
}

/// A play from Bob's playlist, which Alice does not see, shows the
/// track's album to her.
#[tokio::test]
async fn a_playlist_not_seen_shows_the_play_s_album() {
    let (server, session, client, _dir) = signed_in("row-unseen").await;
    let bob = client
        .sign_in(&server.server.id().to_string(), "bob", PASSWORD, false)
        .await
        .unwrap();
    let bobs = bob
        .create_playlist("Bob's", tracks(), Visibility::Personal)
        .await
        .unwrap();
    let from_bobs = PlaySource::Playlist { id: bobs.id };
    played(&server, Of::Track("35", Some(from_bobs))).await;
    assert_eq!(cards(&session).await, ["album:plex:album:34"]);
}

/// Every kind in one row, newest first, by the defaults.
#[tokio::test]
async fn the_row_mixes_every_kind() {
    let (server, session, _client, _dir) = signed_in("row-mixed").await;
    every_kind(&server).await;
    assert_eq!(
        cards(&session).await,
        [
            "episode:plex:episode:15",
            "episode:plex:episode:13",
            "track:plex:track:32",
            "episode:plex:episode:12",
            "movie:plex:movie:1"
        ]
    );
}

/// The same plays by season: season 11 once, at episode 13, and season
/// 14 looked up for a play that kept none.
#[tokio::test]
async fn by_season_each_season_is_one_card() {
    let (server, session, _client, _dir) = signed_in("row-seasons").await;
    every_kind(&server).await;
    set(
        &session,
        RecentlyPlayedPreferences {
            shows: RecentShowsMode::Season,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        cards(&session).await,
        [
            "season:plex:season:14",
            "season:plex:season:11",
            "track:plex:track:32",
            "movie:plex:movie:1"
        ]
    );
}

/// The same plays by show: one card, at its latest episode.
#[tokio::test]
async fn by_show_the_show_is_one_card() {
    let (server, session, _client, _dir) = signed_in("row-shows").await;
    every_kind(&server).await;
    set(
        &session,
        RecentlyPlayedPreferences {
            shows: RecentShowsMode::Show,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        cards(&session).await,
        [
            "show:plex:show:10",
            "track:plex:track:32",
            "movie:plex:movie:1"
        ]
    );
}

/// Shows hidden: films and music stay in the row.
#[tokio::test]
async fn one_kind_hidden_leaves_the_others() {
    let (server, session, _client, _dir) = signed_in("row-one-hidden").await;
    every_kind(&server).await;
    set(
        &session,
        RecentlyPlayedPreferences {
            shows: RecentShowsMode::Hidden,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        cards(&session).await,
        ["track:plex:track:32", "movie:plex:movie:1"]
    );
}

/// Films hidden: the films' cards go, and nothing else.
#[tokio::test]
async fn films_hidden_leave_shows_and_music() {
    let (server, session, _client, _dir) = signed_in("row-films-hidden").await;
    every_kind(&server).await;
    set(
        &session,
        RecentlyPlayedPreferences {
            movies: RecentMoviesMode::Hidden,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(cards(&session).await.len(), 4);
}

/// Every kind hidden: the row is empty. The plays are recorded and
/// counted all the same, and the old Recently played lists them.
#[tokio::test]
async fn every_kind_hidden_is_an_empty_row_and_plays_still_count() {
    let (server, session, _client, _dir) = signed_in("row-all-hidden").await;
    let hidden = RecentlyPlayedPreferences {
        music: RecentMusicMode::Hidden,
        shows: RecentShowsMode::Hidden,
        movies: RecentMoviesMode::Hidden,
    };
    set(&session, hidden).await;
    every_kind(&server).await;
    played(&server, Of::Track("32", None)).await;
    let row = cards(&session).await;
    let rows = server
        .store
        .all_plays(principal(&server, "alice").await)
        .await
        .unwrap();
    let detail = session.item(&track_id("32")).await.unwrap();
    let recent = session.recently_played(None, 0, 50).await.unwrap();
    assert_eq!(
        (
            row.len(),
            rows.iter().filter(|r| r.counted).count(),
            detail.item.playback().map(|p| p.play_count),
            recent.total
        ),
        (0, 6, Some(2), 5)
    );
}

/// The plays of Alice once there are `n`: a play is written in the
/// background after its start.
async fn plays_of(server: &TestServer, who: Principal, n: usize) -> Vec<PlayRow> {
    for _ in 0..250 {
        let rows = server.store.all_plays(who).await.unwrap();
        if rows.len() == n {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("never {n} plays");
}

/// A playback started with a source keeps it with its play: from an
/// album, from a playlist, queued by hand, and from nowhere.
#[tokio::test]
async fn a_playback_s_source_is_kept_with_its_play() {
    let (server, session, _client, _dir) = signed_in("row-start").await;
    let sources = [
        Some(PlaySource::Album {
            id: media(MediaKind::Album, "31").parse().unwrap(),
        }),
        Some(PlaySource::Playlist { id: "p1".into() }),
        Some(PlaySource::Queued),
        None,
    ];
    for source in sources.iter().cloned() {
        let playback = session
            .open_playback(
                &track_id("32"),
                None,
                PlaybackOptions {
                    source,
                    ..PlaybackOptions::default()
                },
            )
            .await
            .unwrap();
        playback.close().await.unwrap();
    }
    let rows = plays_of(&server, principal(&server, "alice").await, 4).await;
    let kept: Vec<_> = rows
        .iter()
        .map(|r| (r.source_kind.as_deref(), r.source_id.as_deref()))
        .collect();
    assert_eq!(
        kept,
        [
            (Some("album"), Some("plex:album:31")),
            (Some("playlist"), Some("p1")),
            (Some("queued"), None),
            (None, None),
        ]
    );
}
