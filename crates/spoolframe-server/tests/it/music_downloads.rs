//! Music offline (PX-51): tracks downloaded
//! in original quality or as AAC 256, what an artist, a playlist or Liked
//! holds against a cursor, and plays and likes made with no server.
//! Against a server with real ffmpeg and ffprobe, on tracks made at test
//! time.

use std::path::{Path, PathBuf};
use std::time::Duration;

use spoolframe_protocol::codec::read_frame;
use spoolframe_protocol::downloads::{
    DownloadInfo, DownloadSource, DownloadState, DownloadTracks, Followed, FollowedChange,
    FollowedTracks, OfflineLike, OfflinePlay, OfflineWatchOutcome,
};
use spoolframe_protocol::playlists::{Kinds, PlaylistEdit, Visibility};
use spoolframe_protocol::{Error, Event, Reply, Request, Response, SessionId};
use spoolframe_server::principal::{Actor, Principal};
use spoolframe_server::test_support::{album, artist, episode_media, track};
use spoolframe_types::{Container, MediaId, Platform};

use crate::downloads_wire::{
    PASSWORD, events, hello, probe, read_from, ready, server, settled, sha256,
};
use crate::support::{ServerOptions, TestClient, TestServer};

/// A connection from a music app on `platform`, signed in as `user`.
async fn device(server: &TestServer, platform: Platform, user: &str) -> (TestClient, SessionId) {
    let client = server.client().await;
    hello(&client, platform).await;
    let session = client.login(user, PASSWORD).await;
    (client, session)
}

/// Track `key` of the fake catalog's artist.
pub(crate) fn track_id(key: &str) -> MediaId {
    let album = if key == "35" { "34" } else { "31" };
    track(key, album, 1).id().clone()
}

/// Where track `key`'s file is.
pub(crate) fn track_file(server: &TestServer, key: &str) -> PathBuf {
    episode_media(&server.media, &track_id(key))
}

/// Make track `key`'s file with real ffmpeg: 6 s of white noise in
/// 16-bit stereo 44.1 kHz (`sample_fmt`, as the encoder takes it) with
/// its title and artist tagged, encoded as `codec` into `format`, and a
/// red cover where `cover`.
fn write_track(
    server: &TestServer,
    key: &str,
    (codec, sample_fmt): (&str, &str),
    format: &str,
    cover: bool,
) {
    let file = track_file(server, key);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let mut ffmpeg = std::process::Command::new("ffmpeg");
    ffmpeg
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i"])
        .arg("anoisesrc=color=white:sample_rate=44100:amplitude=0.5:duration=6");
    if cover {
        let jpg = server.dir.as_ref().join("cover.jpg");
        let made = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(["-f", "lavfi", "-i", "color=c=red:s=64x64", "-frames:v", "1"])
            .arg(&jpg)
            .status()
            .expect("ffmpeg runs");
        assert!(made.success(), "ffmpeg could not make the cover");
        ffmpeg
            .arg("-i")
            .arg(&jpg)
            .args(["-map", "0:a", "-map", "1:v"]);
        ffmpeg.args(["-c:v", "copy", "-disposition:v", "attached_pic"]);
    }
    let status = ffmpeg
        .args(["-ac", "2", "-sample_fmt", sample_fmt, "-c:a", codec])
        .args([
            "-metadata",
            "title=Aerodynamic",
            "-metadata",
            "artist=Daft Punk",
        ])
        .args(["-f", format])
        .arg(&file)
        .status()
        .expect("ffmpeg runs");
    assert!(status.success(), "ffmpeg could not make the track");
}

/// A FLAC with a cover.
pub(crate) fn write_flac(server: &TestServer, key: &str) {
    write_track(server, key, ("flac", "s16"), "flac", true);
}

/// A lossless file Apple's players do not take, as an APE is: WavPack,
/// which ffmpeg writes (it has no APE encoder) and which the server, as
/// for APE, names no codec of.
fn write_wavpack(server: &TestServer, key: &str) {
    write_track(server, key, ("wavpack", "s16p"), "wv", false);
}

async fn prepare(
    client: &TestClient,
    session: &SessionId,
    key: &str,
    source: DownloadSource,
) -> Response {
    client
        .call(
            Some(session),
            Request::PrepareDownload {
                id: track_id(key),
                source,
                tracks: DownloadTracks::All,
                device: None,
            },
        )
        .await
}

async fn prepared(
    client: &TestClient,
    session: &SessionId,
    key: &str,
    source: DownloadSource,
) -> DownloadInfo {
    match prepare(client, session, key, source).await {
        Response::Ok(Reply::Download(info)) => info,
        other => panic!("not prepared: {other:?}"),
    }
}

/// Track `key` downloaded in `source` by `client`, whole, kept in the
/// test's directory as `name`: its path, and what the server said of it.
async fn downloaded(
    server: &TestServer,
    client: &TestClient,
    session: &SessionId,
    key: &str,
    source: DownloadSource,
    name: &str,
) -> (PathBuf, DownloadInfo) {
    let info = prepared(client, session, key, source).await;
    let (_, sha) = ready(client, session, &info.id).await;
    let bytes = read_from(client, session, &info.id, 0, 4 << 20).await;
    assert_eq!(sha256(&bytes), sha, "the file read is not the one hashed");
    let file = server.dir.as_ref().join(name);
    std::fs::write(&file, &bytes).unwrap();
    (file, settled(client, session, &info.id).await)
}

/// The MD5 of `file`'s audio decoded to 32-bit samples: the same for two
/// files only when they hold the same sound, sample for sample.
fn decoded_md5(file: &Path) -> String {
    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(file)
        .args(["-map", "0:a:0", "-c:a", "pcm_s32le", "-f", "md5", "-"])
        .output()
        .expect("ffmpeg runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn container(info: &DownloadInfo) -> Option<Container> {
    info.file.as_ref().map(|f| f.container)
}

/// Prepared, a track says about how large it will be before it is
/// ready: a FLAC sent as it is, its file's size.
#[tokio::test]
async fn a_prepared_track_says_its_expected_size() {
    let server = server("music-expected-size", ServerOptions::default()).await;
    write_flac(&server, "32");
    let (laptop, session) = device(&server, Platform::Linux, "alice").await;
    let info = prepared(&laptop, &session, "32", DownloadSource::Original).await;
    assert_eq!(
        info.expected_size,
        Some(std::fs::metadata(track_file(&server, "32")).unwrap().len())
    );
    server.stop().await;
}

#[tokio::test]
async fn a_flac_is_sent_byte_identical_to_linux_and_to_apple() {
    let server = server("music-flac-as-is", ServerOptions::default()).await;
    write_flac(&server, "32");
    let source = std::fs::read(track_file(&server, "32")).unwrap();
    let mut got = Vec::new();
    for (platform, name) in [(Platform::Linux, "linux.flac"), (Platform::Ios, "ios.flac")] {
        let (laptop, session) = device(&server, platform, "alice").await;
        let (file, info) = downloaded(
            &server,
            &laptop,
            &session,
            "32",
            DownloadSource::Original,
            name,
        )
        .await;
        got.push((std::fs::read(&file).unwrap() == source, container(&info)));
    }
    assert_eq!(got, [(true, Some(Container::Flac)); 2]);
    server.stop().await;
}

#[tokio::test]
async fn a_lossless_file_apple_cannot_play_becomes_alac_sample_for_sample() {
    let server = server("music-wavpack-alac", ServerOptions::default()).await;
    write_wavpack(&server, "32");
    let (phone, session) = device(&server, Platform::Ios, "alice").await;
    let (file, info) = downloaded(
        &server,
        &phone,
        &session,
        "32",
        DownloadSource::Original,
        "phone.m4a",
    )
    .await;
    assert_eq!(
        (
            probe(&file, "stream=codec_name"),
            probe(&file, "format_tags=title"),
            container(&info),
            decoded_md5(&file) == decoded_md5(&track_file(&server, "32")),
        ),
        (
            vec!["alac".to_owned()],
            vec!["Aerodynamic".to_owned()],
            Some(Container::Mp4),
            true
        )
    );
    server.stop().await;
}

#[tokio::test]
async fn a_lossless_file_linux_cannot_play_becomes_flac_sample_for_sample() {
    let server = server("music-wavpack-flac", ServerOptions::default()).await;
    write_wavpack(&server, "32");
    let (laptop, session) = device(&server, Platform::Linux, "alice").await;
    let (file, info) = downloaded(
        &server,
        &laptop,
        &session,
        "32",
        DownloadSource::Original,
        "laptop.flac",
    )
    .await;
    assert_eq!(
        (
            probe(&file, "stream=codec_name"),
            container(&info),
            decoded_md5(&file) == decoded_md5(&track_file(&server, "32")),
        ),
        (vec!["flac".to_owned()], Some(Container::Flac), true)
    );
    server.stop().await;
}

#[tokio::test]
async fn aac_256_is_aac_near_256_kbit_with_its_tags_and_cover() {
    let server = server("music-aac-256", ServerOptions::default()).await;
    write_flac(&server, "32");
    let (phone, session) = device(&server, Platform::Ios, "alice").await;
    let (file, info) = downloaded(
        &server,
        &phone,
        &session,
        "32",
        DownloadSource::Aac256,
        "phone.m4a",
    )
    .await;
    let rate: u64 = probe(&file, "stream=bit_rate")[0].parse().unwrap();
    // ffmpeg's own AAC encoder aims at the rate and lands under it: about
    // 221 kbit/s on noise. Within 15% either way.
    assert_eq!(
        (
            probe(&file, "stream=codec_name:stream_disposition=attached_pic"),
            probe(&file, "format_tags=title"),
            container(&info),
            (217_600..=294_400).contains(&rate),
        ),
        (
            ["aac,0", "mjpeg,1"].map(String::from).to_vec(),
            vec!["Aerodynamic".to_owned()],
            Some(Container::Mp4),
            true
        ),
        "{rate} bit/s"
    );
    server.stop().await;
}

#[tokio::test]
async fn a_second_device_of_the_same_class_encodes_nothing() {
    let server = server("music-second-device", ServerOptions::default()).await;
    write_wavpack(&server, "32");
    let (phone, alice) = device(&server, Platform::Ios, "alice").await;
    let first = prepared(&phone, &alice, "32", DownloadSource::Original).await;
    let (size, sha256) = ready(&phone, &alice, &first.id).await;
    let tracks = server.store.optimized_tracks().await.unwrap();
    let (mac, bob) = device(&server, Platform::Macos, "bob").await;
    let second = prepared(&mac, &bob, "32", DownloadSource::Original).await;
    // Ready at once, from the same file: nothing waits, nothing converts.
    assert_eq!(
        (
            tracks.len(),
            second.state,
            server.store.optimized_tracks().await.unwrap(),
        ),
        (1, DownloadState::Ready { size, sha256 }, tracks.clone())
    );
    server.stop().await;
}

async fn changes(
    client: &TestClient,
    session: &SessionId,
    artists: Vec<MediaId>,
    playlists: Vec<String>,
    liked: bool,
    since: Option<String>,
) -> (Vec<FollowedChange>, String) {
    match client
        .call(
            Some(session),
            Request::MusicDownloadChanges {
                artists,
                playlists,
                liked,
                since,
            },
        )
        .await
    {
        Response::Ok(Reply::MusicDownloadChanges { changes, cursor }) => (changes, cursor),
        other => panic!("no changes: {other:?}"),
    }
}

fn tracks_of(change: &FollowedChange) -> &FollowedTracks {
    &change.tracks
}

fn tracks(keys: &[&str]) -> FollowedTracks {
    FollowedTracks::Tracks {
        tracks: keys.iter().map(|k| track_id(k)).collect(),
    }
}

#[tokio::test]
async fn an_album_the_library_gains_shows_up_for_its_followed_artist() {
    let server = TestServer::start_with(
        crate::support::fresh_dir("music-artist-gains").await,
        ServerOptions::default(),
    )
    .await;
    server.add_user("alice", PASSWORD, None).await;
    let homework = album("34").id().clone();
    server.catalog.not_yet(&homework);
    let (laptop, session) = device(&server, Platform::Linux, "alice").await;
    let daft_punk = artist().id().clone();
    let (first, cursor) = changes(
        &laptop,
        &session,
        vec![daft_punk.clone()],
        vec![],
        false,
        None,
    )
    .await;
    let (same, cursor) = changes(
        &laptop,
        &session,
        vec![daft_punk.clone()],
        vec![],
        false,
        Some(cursor),
    )
    .await;
    server.catalog.gain(&homework);
    let (gained, _) = changes(
        &laptop,
        &session,
        vec![daft_punk.clone()],
        vec![],
        false,
        Some(cursor),
    )
    .await;
    assert_eq!(
        (
            first[0].followed.clone(),
            tracks_of(&first[0]).clone(),
            tracks_of(&same[0]).clone(),
            tracks_of(&gained[0]).clone(),
        ),
        (
            Followed::Artist { id: daft_punk },
            tracks(&["32", "33"]),
            FollowedTracks::Unchanged,
            // Homework (1997) is the older album: first.
            tracks(&["35", "32", "33"]),
        )
    );
    server.stop().await;
}

/// Review 2026-10-20, finding 9: a cursor from an ask about the artist
/// alone still answers for Liked, asked about with it before.
#[tokio::test]
async fn a_cursor_from_an_ask_about_some_keeps_the_others() {
    let server = TestServer::start("music-cursor-subset").await;
    server.add_user("alice", PASSWORD, None).await;
    let (laptop, session) = device(&server, Platform::Linux, "alice").await;
    let daft_punk = artist().id().clone();
    let (_, both) = changes(
        &laptop,
        &session,
        vec![daft_punk.clone()],
        vec![],
        true,
        None,
    )
    .await;
    let (_, artist_alone) = changes(
        &laptop,
        &session,
        vec![daft_punk],
        vec![],
        false,
        Some(both),
    )
    .await;
    let (liked, _) = changes(&laptop, &session, vec![], vec![], true, Some(artist_alone)).await;
    assert_eq!(
        liked.iter().map(tracks_of).collect::<Vec<_>>(),
        [&FollowedTracks::Unchanged]
    );
    server.stop().await;
}

pub(crate) async fn alice(server: &TestServer) -> Actor {
    let user = server.store.user_by_name("alice").await.unwrap().unwrap();
    Actor {
        principal: Principal::LocalUser(user.id),
        admin: false,
        federated: false,
    }
}

#[tokio::test]
async fn a_track_added_to_or_removed_from_a_followed_playlist_shows_up() {
    let server = TestServer::start("music-playlist-changes").await;
    server.add_user("alice", PASSWORD, None).await;
    let actor = alice(&server).await;
    let playlists = &server.playlists;
    let made = playlists
        .create(
            &actor,
            "Road trip",
            Kinds {
                tracks: true,
                ..Kinds::default()
            },
            Visibility::Shared { with: vec![] },
        )
        .await
        .unwrap();
    let add = |key: &str| PlaylistEdit::Add {
        id: track_id(key),
        before: None,
    };
    let revision = playlists
        .edit(&actor, &made.id, made.revision, vec![add("32")])
        .await
        .unwrap()
        .0;
    let (laptop, session) = device(&server, Platform::Linux, "alice").await;
    let ask = |since| {
        changes(
            &laptop,
            &session,
            vec![],
            vec![made.id.clone()],
            false,
            since,
        )
    };
    let (first, cursor) = ask(None).await;
    let revision = playlists
        .edit(&actor, &made.id, revision, vec![add("35")])
        .await
        .unwrap()
        .0;
    let (added, cursor) = ask(Some(cursor)).await;
    let entry = server.store.playlist_entries(&made.id).await.unwrap()[0].entry_id;
    playlists
        .edit(
            &actor,
            &made.id,
            revision,
            vec![PlaylistEdit::Remove { entry }],
        )
        .await
        .unwrap();
    let (removed, _) = ask(Some(cursor)).await;
    assert_eq!(
        [&first[0], &added[0], &removed[0]].map(|c| c.tracks.clone()),
        [tracks(&["32"]), tracks(&["32", "35"]), tracks(&["35"])]
    );
    server.stop().await;
}

#[tokio::test]
async fn a_playlist_gone_and_liked_are_told_as_such() {
    let server = TestServer::start("music-gone-and-liked").await;
    server.add_user("alice", PASSWORD, None).await;
    let (laptop, session) = device(&server, Platform::Linux, "alice").await;
    let liked = laptop
        .call(
            Some(&session),
            Request::Like {
                items: vec![track_id("33")],
            },
        )
        .await;
    assert!(
        matches!(liked, Response::Ok(Reply::PlaylistInfo(_))),
        "{liked:?}"
    );
    let (answer, _) = changes(&laptop, &session, vec![], vec!["nope".into()], true, None).await;
    assert_eq!(
        answer,
        [
            FollowedChange {
                followed: Followed::Playlist { id: "nope".into() },
                tracks: FollowedTracks::Gone,
            },
            FollowedChange {
                followed: Followed::Liked,
                tracks: tracks(&["33"]),
            },
        ]
    );
    server.stop().await;
}

/// How many `LikedChanged` come on `events`, up to `want`, within `wait`.
async fn liked_changes(
    events: &mut iroh::endpoint::RecvStream,
    want: usize,
    wait: Duration,
) -> usize {
    let deadline = tokio::time::Instant::now() + wait;
    let mut told = 0;
    while told < want {
        match tokio::time::timeout_at(deadline, read_frame::<_, Event>(events)).await {
            Ok(Ok(Event::LikedChanged)) => told += 1,
            Ok(Ok(_)) => {}
            _ => break,
        }
    }
    told
}

/// A like and an unlike are told to each of the user's connections, the
/// one that made them too, and not to another user's.
#[tokio::test]
async fn liked_changed_is_told_to_each_of_the_user_s_apps() {
    let server = TestServer::start("music-liked-changed").await;
    for name in ["alice", "bob"] {
        server.add_user(name, PASSWORD, None).await;
    }
    let (phone, phone_session) = device(&server, Platform::Ios, "alice").await;
    let (bobs, bobs_session) = device(&server, Platform::Ios, "bob").await;
    let laptop = server.client().await;
    hello(&laptop, Platform::Linux).await;
    let session = laptop.login("alice", PASSWORD).await;
    // Each acts as its user once: from then on it is told.
    changes(&phone, &phone_session, vec![], vec![], true, None).await;
    changes(&bobs, &bobs_session, vec![], vec![], true, None).await;
    let (mut phone_events, mut bobs_events, mut laptop_events) = (
        events(&phone).await,
        events(&bobs).await,
        events(&laptop).await,
    );
    let items = vec![track_id("32")];
    let liked = laptop
        .call(
            Some(&session),
            Request::Like {
                items: items.clone(),
            },
        )
        .await;
    let unliked = laptop.call(Some(&session), Request::Unlike { items }).await;
    let to_phone = liked_changes(&mut phone_events, 2, Duration::from_secs(5)).await;
    // Sent to every connection at once: the others' are out by now.
    let wait = Duration::from_millis(500);
    assert_eq!(
        (
            matches!(liked, Response::Ok(Reply::PlaylistInfo(_))),
            matches!(unliked, Response::Ok(Reply::PlaylistInfo(_))),
            to_phone,
            liked_changes(&mut bobs_events, 1, wait).await,
            liked_changes(&mut laptop_events, 2, wait).await,
        ),
        (true, true, 2, 0, 2)
    );
    server.stop().await;
}

/// Review 2026-10-20, finding 6: a connection signed in as bob and out
/// as alice is told nothing of alice's likes. (Signed out of every user,
/// the device's connection is closed.)
#[tokio::test]
async fn a_connection_signed_out_of_a_user_is_told_nothing_of_their_likes() {
    let server = TestServer::start("music-liked-switched").await;
    for name in ["alice", "bob"] {
        server.add_user(name, PASSWORD, None).await;
    }
    let (shared, first) = device(&server, Platform::Ios, "alice").await;
    changes(&shared, &first, vec![], vec![], true, None).await;
    let mut shared_events = events(&shared).await;
    let bob = shared.login("bob", PASSWORD).await;
    changes(&shared, &bob, vec![], vec![], true, None).await;
    assert!(matches!(
        shared.call(Some(&first), Request::Logout).await,
        Response::Ok(Reply::LoggedOut)
    ));
    let (phone, alice) = device(&server, Platform::Ios, "alice").await;
    let liked = phone
        .call(
            Some(&alice),
            Request::Like {
                items: vec![track_id("32")],
            },
        )
        .await;
    assert!(
        matches!(liked, Response::Ok(Reply::PlaylistInfo(_))),
        "{liked:?}"
    );
    assert_eq!(
        liked_changes(&mut shared_events, 1, Duration::from_millis(500)).await,
        0
    );
    server.stop().await;
}

async fn sync(
    client: &TestClient,
    session: &SessionId,
    device_now: i64,
    plays: Vec<OfflinePlay>,
    liked: Vec<OfflineLike>,
) -> (Vec<OfflineWatchOutcome>, Vec<OfflineWatchOutcome>) {
    match client
        .call(
            Some(session),
            Request::SyncOfflinePlays {
                device_now,
                plays,
                liked,
            },
        )
        .await
    {
        Response::Ok(Reply::OfflinePlaysSynced { plays, liked }) => (plays, liked),
        other => panic!("not synced: {other:?}"),
    }
}

#[tokio::test]
async fn a_play_made_offline_reaches_the_server_once_at_its_time() {
    let server = TestServer::start("music-offline-plays").await;
    server.add_user("alice", PASSWORD, None).await;
    let (laptop, session) = device(&server, Platform::Linux, "alice").await;
    let now = server.clock.unix_secs();
    // The device's clock is 100 s fast; it played the track an hour ago.
    let device_now = now + 100;
    let play = OfflinePlay {
        track: track_id("32"),
        at: device_now - 3600,
        played_ms: 240_000,
    };
    let first = sync(&laptop, &session, device_now, vec![play.clone()], vec![]).await;
    // The server read its clock in between: a second may have ticked
    // since `now`.
    let after = server.clock.unix_secs();
    let again = sync(&laptop, &session, device_now + 5, vec![play], vec![]).await;
    let plays = server
        .store
        .all_plays(alice(&server).await.principal)
        .await
        .unwrap();
    assert_eq!(
        (
            first.0,
            again.0,
            plays
                .iter()
                .map(|p| (p.media_id.clone(), p.counted, p.path.clone()))
                .collect::<Vec<_>>(),
        ),
        (
            vec![OfflineWatchOutcome::Applied],
            vec![OfflineWatchOutcome::Applied],
            vec![(track_id("32").to_string(), true, "offline".to_owned())],
        )
    );
    // An hour before the server's now, not the device's 100 s fast one.
    let ended_at = plays[0].ended_at;
    assert!(
        (now - 3600..=after - 3600).contains(&ended_at),
        "ended at {ended_at}, not an hour before {now}..={after}"
    );
    server.stop().await;
}

/// Whether `key` is in alice's Liked now.
async fn is_liked(server: &TestServer, key: &str) -> bool {
    let principal = alice(server).await.principal.to_string();
    server
        .store
        .liked_media(&principal)
        .await
        .unwrap()
        .contains(&track_id(key).to_string())
}

#[tokio::test]
async fn the_newest_liked_change_wins_whichever_device_made_it() {
    let server = TestServer::start("music-offline-likes").await;
    server.add_user("alice", PASSWORD, None).await;
    let (laptop, session) = device(&server, Platform::Linux, "alice").await;
    let liked = laptop
        .call(
            Some(&session),
            Request::Like {
                items: vec![track_id("32")],
            },
        )
        .await;
    assert!(
        matches!(liked, Response::Ok(Reply::PlaylistInfo(_))),
        "{liked:?}"
    );
    let unlike = |at| OfflineLike {
        track: track_id("32"),
        liked: false,
        at,
    };
    server.clock.advance(Duration::from_secs(60));
    let now = server.clock.unix_secs();
    // Unliked on the phone before the laptop liked it: the like stays.
    let older = sync(&laptop, &session, now, vec![], vec![unlike(now - 120)]).await;
    let kept = is_liked(&server, "32").await;
    // Unliked on the phone after: it goes.
    let newer = sync(&laptop, &session, now, vec![], vec![unlike(now - 30)]).await;
    let gone = !is_liked(&server, "32").await;
    // A like from before that unlike, sent late: still unliked.
    let late = sync(
        &laptop,
        &session,
        now,
        vec![],
        vec![OfflineLike {
            track: track_id("32"),
            liked: true,
            at: now - 45,
        }],
    )
    .await;
    assert_eq!(
        (
            older.1,
            kept,
            newer.1,
            gone,
            late.1,
            is_liked(&server, "32").await
        ),
        (
            vec![OfflineWatchOutcome::Older],
            true,
            vec![OfflineWatchOutcome::Applied],
            true,
            vec![OfflineWatchOutcome::Older],
            false
        )
    );
    server.stop().await;
}

#[tokio::test]
async fn a_film_is_refused_aac_256_and_a_track_a_profile() {
    let server = server("music-wrong-sources", ServerOptions::default()).await;
    write_flac(&server, "32");
    crate::downloads::write_film(&server);
    let (laptop, session) = device(&server, Platform::Linux, "alice").await;
    let film = laptop
        .call(
            Some(&session),
            Request::PrepareDownload {
                id: crate::support::movie().id().clone(),
                source: DownloadSource::Aac256,
                tracks: DownloadTracks::All,
                device: None,
            },
        )
        .await;
    let track = prepare(
        &laptop,
        &session,
        "32",
        DownloadSource::Profile {
            name: "Mobile".into(),
        },
    )
    .await;
    assert!(
        matches!(film, Response::Err(Error::InvalidInput { .. }))
            && matches!(track, Response::Err(Error::InvalidInput { .. })),
        "{film:?} {track:?}"
    );
    server.stop().await;
}
