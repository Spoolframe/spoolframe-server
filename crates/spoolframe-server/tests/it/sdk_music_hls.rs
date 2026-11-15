//! Tracks kept from listening on Apple: an iPhone plays FLAC as an HLS
//! remux (fMP4 segments), which AVPlayer reads from the SDK's HLS bridge.
//! What the bridge serves of a track, its init segment and every media
//! segment in order, is kept as the track's file; a play that skipped a
//! segment keeps nothing; two plays at once (a gapless queue) each keep
//! their own track.

use std::sync::Arc;
use std::time::Duration;

use spoolframe_client_sdk::types::{ClientCapabilities, Platform};
use spoolframe_client_sdk::{
    Client, Device, ManagedPlayback, ManifestKind, MemoryStorage, OfflineMark, PlaybackOptions,
    PlaybackPlan, SdkError, Session,
};
use spoolframe_hls_bridge::{Bridge, Kind, Stream};
use spoolframe_test_dir::TestDir;
use spoolframe_types::{AudioCodec, Container, Delivery};

use crate::music_downloads::{track_file, track_id};
use crate::sdk_music::{alice, eventually, marks, music_server, offline, options};
use crate::support::{TestServer, pinned_sdk_client};

/// The iPhone: FLAC only in an MP4, over HLS, as AVPlayer plays it.
fn iphone() -> Device {
    Device {
        name: "Phone".into(),
        client_name: "sdk-music-hls-test".into(),
        client_version: "0".into(),
        platform: Platform::Ios,
        capabilities: ClientCapabilities {
            audio: [AudioCodec::Flac].into(),
            containers: [Container::Mp4].into(),
            delivery: vec![Delivery::Hls],
            ..ClientCapabilities::default()
        },
    }
}

/// Make track `key` 20 s of FLAC: several 6 s segments.
fn write_long_flac(server: &TestServer, key: &str) {
    let file = track_file(server, key);
    let status = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i"])
        .arg("anoisesrc=color=white:sample_rate=44100:amplitude=0.5:duration=20")
        .args(["-ac", "2", "-sample_fmt", "s16", "-c:a", "flac"])
        .arg(&file)
        .status()
        .expect("ffmpeg runs");
    assert!(status.success(), "ffmpeg could not make the track");
}

/// The bridge's source over the SDK's managed playback, as the app's
/// (`spoolframe_hls_bridge::SdkSource`) is: not that type, whose `sdk`
/// feature here would put the SDK in the server's own build.
struct Served(ManagedPlayback);

fn bridge_error(e: SdkError) -> spoolframe_hls_bridge::Error {
    spoolframe_hls_bridge::Error::Other(e.to_string())
}

impl spoolframe_hls_bridge::Source for Served {
    type Snapshot = spoolframe_client_sdk::Playback;
    type Segment = ServedSegment;

    fn changes(&self) -> tokio::sync::watch::Receiver<Self::Snapshot> {
        self.0.changes()
    }

    fn replanned(&self) -> bool {
        false
    }

    async fn read(
        &self,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, spoolframe_hls_bridge::Error> {
        self.0.read(offset, length).await.map_err(bridge_error)
    }

    async fn manifest(
        &self,
        kind: spoolframe_hls_bridge::ManifestKind,
        variant: u32,
    ) -> Result<String, spoolframe_hls_bridge::Error> {
        let kind = match kind {
            spoolframe_hls_bridge::ManifestKind::Multivariant => ManifestKind::Multivariant,
            spoolframe_hls_bridge::ManifestKind::Media => ManifestKind::Media,
            spoolframe_hls_bridge::ManifestKind::Subtitles => ManifestKind::Subtitles,
        };
        self.0.manifest(kind, variant).await.map_err(bridge_error)
    }

    async fn read_segment(
        &self,
        variant: u32,
        n: u32,
        offset: u64,
        length: u64,
    ) -> Result<spoolframe_hls_bridge::Chunk, spoolframe_hls_bridge::Error> {
        let chunk = self
            .0
            .read_segment(variant, n, offset, length)
            .await
            .map_err(bridge_error)?;
        Ok(spoolframe_hls_bridge::Chunk {
            bytes: chunk.bytes,
            size: chunk.size,
            complete: chunk.complete,
            epoch: chunk.epoch,
        })
    }

    async fn segment_stream(
        &self,
        variant: u32,
        n: u32,
        offset: u64,
        length: u64,
    ) -> Result<ServedSegment, spoolframe_hls_bridge::Error> {
        Ok(ServedSegment(
            self.0
                .segment_stream(variant, n, offset, length)
                .await
                .map_err(bridge_error)?,
        ))
    }

    async fn read_subtitle_segment(
        &self,
        n: u32,
    ) -> Result<spoolframe_hls_bridge::Subtitle, spoolframe_hls_bridge::Error> {
        let subtitle = self
            .0
            .read_subtitle_segment(n)
            .await
            .map_err(bridge_error)?;
        Ok(spoolframe_hls_bridge::Subtitle {
            bytes: subtitle.bytes,
        })
    }
}

struct ServedSegment(spoolframe_client_sdk::SegmentStream);

impl spoolframe_hls_bridge::Segment for ServedSegment {
    fn sent(&self) -> u64 {
        self.0.sent()
    }

    fn size(&self) -> u64 {
        self.0.size()
    }

    fn complete(&self) -> bool {
        self.0.complete()
    }

    fn epoch(&self) -> u32 {
        self.0.epoch()
    }

    async fn next(&mut self) -> Result<Option<Vec<u8>>, spoolframe_hls_bridge::Error> {
        self.0.next().await.map_err(bridge_error)
    }

    async fn reopen(&self, length: u64) -> Result<Self, spoolframe_hls_bridge::Error> {
        Ok(Self(self.0.reopen(length).await.map_err(bridge_error)?))
    }
}

/// A playback of track `key` served on `bridge`, at `base` (its URL
/// without the file name).
struct Play {
    playback: ManagedPlayback,
    token: String,
    base: String,
}

async fn open(session: &Session, bridge: &Bridge<Served>, addr: &str, key: &str) -> Play {
    let playback = session
        .open_playback(&track_id(key), None, PlaybackOptions::default())
        .await
        .unwrap();
    assert!(
        matches!(
            playback.snapshot().plan,
            PlaybackPlan::Converted {
                delivery: Delivery::Hls,
                ..
            }
        ),
        "{:?}",
        playback.snapshot().plan
    );
    let token = bridge.register(Stream::new(
        Served(playback.clone()),
        Kind::hls(),
        None,
        Default::default(),
    ));
    Play {
        playback,
        base: format!("http://{addr}/{token}"),
        token,
    }
}

/// A bridge served on a loopback port, and its address.
async fn bridge() -> (Bridge<Served>, String) {
    let bridge = Bridge::new();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let serving = bridge.clone();
    tokio::spawn(async move { serving.serve(listener).await });
    (bridge, addr)
}

async fn get(url: &str) -> Vec<u8> {
    let answer = reqwest::get(url).await.unwrap();
    assert!(answer.status().is_success(), "{url}: {}", answer.status());
    answer.bytes().await.unwrap().to_vec()
}

/// What AVPlayer reads first: the master playlist, then the media
/// playlist it names. The init segment's name, and each media segment's,
/// in order.
async fn playlists(play: &Play) -> (String, Vec<String>) {
    let master = String::from_utf8(get(&format!("{}/master.m3u8", play.base)).await).unwrap();
    let media = master
        .lines()
        .find(|l| !l.starts_with('#') && !l.trim().is_empty())
        .expect("a media playlist")
        .to_owned();
    let media = String::from_utf8(get(&format!("{}/{media}", play.base)).await).unwrap();
    let init = media
        .lines()
        .find_map(|l| l.strip_prefix("#EXT-X-MAP:URI=\""))
        .and_then(|l| l.split('"').next())
        .expect("an init segment")
        .to_owned();
    let segments: Vec<String> = media
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(str::to_owned)
        .collect();
    assert!(segments.len() >= 3, "{media}");
    (init, segments)
}

/// The kept file of track `key`, once it is kept (within 30 s).
async fn kept_file(client: &Client, key: &str) -> Option<Vec<u8>> {
    let kept = eventually(Duration::from_secs(30), || {
        client
            .downloads()
            .music_tracks()
            .iter()
            .any(|t| t.item.id().key() == key)
    })
    .await;
    if !kept {
        return None;
    }
    let track = client
        .downloads()
        .music_tracks()
        .into_iter()
        .find(|t| t.item.id().key() == key)?;
    assert_eq!(
        track.file.as_ref().map(|f| f.container),
        Some(Container::Mp4)
    );
    std::fs::read(track.file_path?).ok()
}

async fn iphone_client(server: &TestServer, dir: &TestDir) -> Client {
    pinned_sdk_client(
        server,
        &Arc::new(MemoryStorage::default()),
        iphone(),
        options(dir),
    )
    .await
}

/// Played through the bridge from its first segment to its last, the
/// track is kept: its file is the init segment and every media segment,
/// as served, an MP4 marked kept.
#[tokio::test]
async fn an_hls_remux_served_through_the_bridge_is_kept() {
    let server = music_server("sdk-music-hls-kept").await;
    write_long_flac(&server, "32");
    let dir = TestDir::new("spoolframe-sdk-music-hls-kept").await;
    let client = iphone_client(&server, &dir).await;
    let session = alice(&server, &client).await;
    let (bridge, addr) = bridge().await;
    let play = open(&session, &bridge, &addr, "32").await;
    let (init, segments) = playlists(&play).await;
    let mut served = get(&format!("{}/{init}", play.base)).await;
    for segment in &segments {
        served.extend(get(&format!("{}/{segment}", play.base)).await);
    }
    let kept = kept_file(&client, "32").await;
    play.playback.close().await.unwrap();
    assert_eq!(
        (
            kept.map(|k| k == served),
            marks(&offline(&client, &server, &session.user().user_id)),
        ),
        (Some(true), vec![("32".to_owned(), OfflineMark::Kept)])
    );
    client.close().await;
    server.stop().await;
}

/// A play that skipped a segment keeps nothing: what it read is written
/// to a part as it plays, and the part goes with the playback.
#[tokio::test]
async fn an_hls_play_that_skipped_a_segment_keeps_nothing() {
    let server = music_server("sdk-music-hls-skip").await;
    write_long_flac(&server, "32");
    let dir = TestDir::new("spoolframe-sdk-music-hls-skip").await;
    let client = iphone_client(&server, &dir).await;
    let session = alice(&server, &client).await;
    let (bridge, addr) = bridge().await;
    let play = open(&session, &bridge, &addr, "32").await;
    let (init, segments) = playlists(&play).await;
    let parts = || {
        std::fs::read_dir(dir.join("listening"))
            .map(|d| d.count())
            .unwrap_or(0)
    };
    get(&format!("{}/{init}", play.base)).await;
    get(&format!("{}/{}", play.base, segments[0])).await;
    // What it read so far is being kept, in a part.
    let writing = parts();
    for segment in &segments[2..] {
        // The second is skipped, as a seek past it would.
        get(&format!("{}/{segment}", play.base)).await;
    }
    // Closed and let go, as the app's player is: the playback goes.
    play.playback.close().await.unwrap();
    bridge.unregister(&play.token);
    drop(play);
    let no_part = eventually(Duration::from_secs(10), || parts() == 0).await;
    assert_eq!(
        (writing, no_part, client.downloads().music_tracks().len()),
        (1, true, 0)
    );
    client.close().await;
    server.stop().await;
}

/// Two tracks played at once, as a gapless queue opens the next early,
/// their segments read in turn: each is kept, its own segments alone.
#[tokio::test]
async fn two_overlapping_plays_each_keep_their_own_track() {
    let server = music_server("sdk-music-hls-two").await;
    write_long_flac(&server, "32");
    write_long_flac(&server, "33");
    let dir = TestDir::new("spoolframe-sdk-music-hls-two").await;
    let client = iphone_client(&server, &dir).await;
    let session = alice(&server, &client).await;
    let (bridge, addr) = bridge().await;
    let first = open(&session, &bridge, &addr, "32").await;
    let (first_init, first_segments) = playlists(&first).await;
    let mut first_served = get(&format!("{}/{first_init}", first.base)).await;
    first_served.extend(get(&format!("{}/{}", first.base, first_segments[0])).await);
    // The next track opens while the first plays.
    let next = session
        .open_playback_next(
            &track_id("33"),
            &first.playback,
            None,
            PlaybackOptions::default(),
        )
        .await
        .unwrap();
    let token = bridge.register(Stream::new(
        Served(next.clone()),
        Kind::hls(),
        None,
        Default::default(),
    ));
    let second = Play {
        playback: next,
        base: format!("http://{addr}/{token}"),
        token,
    };
    let (second_init, second_segments) = playlists(&second).await;
    let mut second_served = get(&format!("{}/{second_init}", second.base)).await;
    // In turn: one of each, then the rest of each.
    let longest = first_segments.len().max(second_segments.len());
    for i in 0..longest {
        if i > 0
            && let Some(segment) = first_segments.get(i)
        {
            first_served.extend(get(&format!("{}/{segment}", first.base)).await);
        }
        if let Some(segment) = second_segments.get(i) {
            second_served.extend(get(&format!("{}/{segment}", second.base)).await);
        }
    }
    let (first_kept, second_kept) = (
        kept_file(&client, "32").await,
        kept_file(&client, "33").await,
    );
    first.playback.close().await.unwrap();
    second.playback.close().await.unwrap();
    assert_ne!(first_served, second_served);
    assert_eq!(
        (
            first_kept.map(|k| k == first_served),
            second_kept.map(|k| k == second_served),
        ),
        (Some(true), Some(true))
    );
    client.close().await;
    server.stop().await;
}
