//! A conversion's HLS read by name with no bridge server, as Media3 reads
//! it: the playlists, a segment, and a run of ranges from one fetch.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use spoolframe_client_sdk::types::{Container, Delivery};
use spoolframe_client_sdk::{
    self as sdk, Chunk, ManifestKind, PlaybackBackend, PlaybackChange, PlaybackHandle,
    PlaybackPlan, SdkError, Selection,
};

use super::*;
use crate::playback::{
    Registered, RegisteredPlayback, SpoolPlayback, open_under_token, unregister,
};

const SEGMENT: &[u8] = b"0123456789";

/// A conversion's server: two playlists and one segment, each read
/// counted.
#[derive(Default)]
struct Hls {
    reads: AtomicUsize,
    /// Segment reads never answer: a server encoding slowly.
    slow: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl PlaybackBackend for Hls {
    async fn start(
        &self,
        _: &sdk::types::MediaId,
        _: Option<Selection>,
        _: sdk::PlaybackOptions,
    ) -> Result<sdk::Playback, SdkError> {
        Ok(snapshot())
    }
    async fn read(&self, _: &PlaybackHandle, _: u64, _: u64) -> Result<Vec<u8>, SdkError> {
        Err(SdkError::Busy)
    }
    async fn read_chunk(
        &self,
        _: &PlaybackHandle,
        _: u32,
        _: u64,
        _: u64,
    ) -> Result<Chunk, SdkError> {
        Err(SdkError::Busy)
    }
    async fn read_segment(
        &self,
        _: &PlaybackHandle,
        epoch: u32,
        _: u32,
        _: u32,
        _: u64,
        _: u64,
    ) -> Result<Chunk, SdkError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        if self.slow.load(Ordering::Relaxed) {
            std::future::pending::<()>().await;
        }
        Ok(Chunk {
            bytes: SEGMENT.to_vec(),
            size: SEGMENT.len() as u64,
            complete: true,
            epoch,
        })
    }
    async fn manifest(
        &self,
        _: &PlaybackHandle,
        kind: ManifestKind,
        _: u32,
    ) -> Result<String, SdkError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        Ok(match kind {
            ManifestKind::Multivariant => {
                "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000000\nmedia.m3u8\n".into()
            }
            _ => "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\nseg1.m4s\n#EXT-X-ENDLIST\n"
                .into(),
        })
    }
    async fn seek(&self, _: &PlaybackHandle, _: Duration) -> Result<u32, SdkError> {
        Ok(0)
    }
    /// Planned again: another container.
    async fn change_quality(
        &self,
        _: &PlaybackHandle,
        _: sdk::PlaybackQuality,
        _: &sdk::Replan,
    ) -> Result<PlaybackChange, SdkError> {
        let mut plan = snapshot().plan;
        if let PlaybackPlan::Converted { container, .. } = &mut plan {
            *container = Container::Matroska;
        }
        Ok(PlaybackChange {
            plan,
            size: 0,
            epoch: 1,
        })
    }
    async fn subtitle(
        &self,
        _: &PlaybackHandle,
        _: Option<u32>,
        _: &sdk::Replan,
    ) -> Result<PlaybackChange, SdkError> {
        Err(SdkError::Busy)
    }
    async fn progress(&self, _: &PlaybackHandle, _: Duration) -> Result<(), SdkError> {
        Ok(())
    }
    async fn stop(&self, _: &PlaybackHandle) -> Result<(), SdkError> {
        Ok(())
    }
    fn link_meter(&self) -> Option<std::sync::Arc<spoolframe_client_sdk::LinkMeter>> {
        None
    }
    fn shares(
        &self,
    ) -> Option<tokio::sync::broadcast::Receiver<spoolframe_client_sdk::PlaybackShare>> {
        None
    }
    async fn set_track_override(
        &self,
        _: &spoolframe_client_sdk::types::MediaId,
        _: spoolframe_client_sdk::Selection,
    ) -> Result<(), spoolframe_client_sdk::SdkError> {
        Err(spoolframe_client_sdk::SdkError::InvalidInput(
            "no track choices here".into(),
        ))
    }
    async fn read_subtitle(
        &self,
        _: &spoolframe_client_sdk::PlaybackHandle,
    ) -> Result<spoolframe_client_sdk::Subtitle, spoolframe_client_sdk::SdkError> {
        Err(spoolframe_client_sdk::SdkError::InvalidInput(
            "no subtitle files here".into(),
        ))
    }
    fn subtitle_timings(
        &self,
    ) -> Option<(
        String,
        tokio::sync::broadcast::Receiver<spoolframe_client_sdk::SubtitleTimingEvent>,
    )> {
        None
    }
    fn route(&self) -> Option<spoolframe_client_sdk::RemoteMode> {
        None
    }
}

fn snapshot() -> sdk::Playback {
    sdk::Playback {
        handle: PlaybackHandle("test-hls".into()),
        plan: PlaybackPlan::Converted {
            delivery: Delivery::Hls,
            container: Container::Mp4,
            video: None,
            audio: None,
            subtitle: None,
            ladder: Vec::new(),
            reasons: Vec::new(),
            source_codec: None,
        },
        size: 0,
        chosen: None,
        epoch: 0,
        markers: vec![],
        duration: None,
    }
}

/// A conversion opened under a token, and its server's read count.
fn opened() -> (Arc<Registered>, Arc<Hls>) {
    let backend = Arc::new(Hls::default());
    let managed = sdk::ManagedPlayback::adopt(
        backend.clone(),
        "plex:movie:1".parse().unwrap(),
        None,
        sdk::PlaybackOptions::default(),
        snapshot(),
    );
    let opened =
        RegisteredPlayback::take(open_under_token(managed, None, Default::default(), None));
    (opened.playback, backend)
}

fn text(fetched: &Fetched) -> String {
    String::from_utf8_lossy(&fetched.bytes).into_owned()
}

#[tokio::test]
async fn a_conversion_s_playlists_read_by_name_name_their_resources_relatively() {
    let (playback, _) = opened();
    let master = fetch(playback.clone(), "master.m3u8".into()).await.unwrap();
    let media = fetch(playback.clone(), "media.m3u8".into()).await.unwrap();
    assert_eq!(
        (master.content_type, text(&media).contains("\nseg1.m4s\n")),
        ("application/vnd.apple.mpegurl", true)
    );
    unregister(&playback.token);
}

#[tokio::test]
async fn a_conversion_s_segment_reads_by_name() {
    let (playback, _) = opened();
    let segment = fetch(playback.clone(), "seg1.m4s".into()).await.unwrap();
    assert_eq!(
        (segment.content_type, segment.bytes.as_slice()),
        ("video/iso.segment", SEGMENT)
    );
    unregister(&playback.token);
}

#[tokio::test]
async fn a_run_of_ranges_in_one_segment_reads_it_from_the_server_once() {
    let (playback, server) = opened();
    fetch(playback.clone(), "seg1.m4s".into()).await.unwrap();
    let mut read = Vec::new();
    for offset in [0, 4, 8] {
        read.extend(read_range(&playback, "seg1.m4s".into(), offset, 4).unwrap());
    }
    assert_eq!(
        (read.as_slice(), server.reads.load(Ordering::Relaxed)),
        (SEGMENT, 1)
    );
    unregister(&playback.token);
}

#[tokio::test]
async fn a_segment_opened_again_reads_from_the_server_once() {
    let (playback, server) = opened();
    open(&playback, "seg1.m4s".into()).unwrap();
    let again = open(&playback, "seg1.m4s".into()).unwrap();
    assert_eq!(
        (again.bytes.as_slice(), server.reads.load(Ordering::Relaxed)),
        (SEGMENT, 1)
    );
    unregister(&playback.token);
}

#[tokio::test]
async fn a_playlist_opened_again_is_read_again() {
    let (playback, server) = opened();
    open(&playback, "media.m3u8".into()).unwrap();
    open(&playback, "media.m3u8".into()).unwrap();
    assert_eq!(server.reads.load(Ordering::Relaxed), 2);
    unregister(&playback.token);
}

#[tokio::test]
async fn a_subtitle_opened_again_is_read_again() {
    let (playback, _) = opened();
    let first = open(&playback, "empty.vtt".into()).unwrap();
    let again = open(&playback, "empty.vtt".into()).unwrap();
    assert!(!Arc::ptr_eq(&first, &again));
    unregister(&playback.token);
}

#[tokio::test]
async fn a_range_of_another_resource_reads_that_one() {
    let (playback, server) = opened();
    fetch(playback.clone(), "seg1.m4s".into()).await.unwrap();
    let playlist = read_range(&playback, "media.m3u8".into(), 0, 7).unwrap();
    assert_eq!(
        (playlist.as_slice(), server.reads.load(Ordering::Relaxed)),
        (&b"#EXTM3U"[..], 2)
    );
    unregister(&playback.token);
}

#[tokio::test]
async fn a_resource_its_playback_does_not_have_is_not_found() {
    let (playback, _) = opened();
    assert!(matches!(
        fetch(playback.clone(), "../file".into()).await,
        Err(SpoolError::NotFound { .. })
    ));
    unregister(&playback.token);
}

#[tokio::test]
async fn ranges_of_two_resources_opened_at_once_read_each_from_the_server_once() {
    let (playback, server) = opened();
    let segment = fetch(playback.clone(), "seg1.m4s".into()).await.unwrap();
    let playlist = fetch(playback.clone(), "media.m3u8".into()).await.unwrap();
    let mut read = (Vec::new(), Vec::new());
    for offset in [0, 4, 8] {
        read.0
            .extend(read_range(&playback, "seg1.m4s".into(), offset, 4).unwrap());
        read.1
            .extend(read_range(&playback, "media.m3u8".into(), offset, 4).unwrap());
    }
    assert_eq!(
        (
            read.0 == segment.bytes,
            read.1 == playlist.bytes[..12],
            server.reads.load(Ordering::Relaxed)
        ),
        (true, true, 2)
    );
    unregister(&playback.token);
}

#[tokio::test]
async fn ending_a_playback_ends_a_read_waiting_on_a_slow_segment() {
    let (playback, server) = opened();
    server.slow.store(true, Ordering::Relaxed);
    let reading = tokio::spawn(fetch(playback.clone(), "seg1.m4s".into()));
    while server.reads.load(Ordering::Relaxed) == 0 {
        tokio::task::yield_now().await;
    }
    unregister(&playback.token);
    let ended = tokio::time::timeout(Duration::from_secs(5), reading).await;
    let ended = ended.map(|joined| joined.unwrap().err());
    assert_eq!(ended.ok(), Some(Some(SpoolError::PlaybackExpired)));
}

#[tokio::test]
async fn a_resource_asked_after_a_replan_says_to_follow_it() {
    let (playback, _) = opened();
    let managed = registered(&playback.token).unwrap().managed.clone();
    managed
        .change_quality(sdk::PlaybackQuality::Auto, Duration::ZERO)
        .await
        .unwrap();
    assert!(matches!(
        fetch(playback.clone(), "media.m3u8".into()).await,
        Err(SpoolError::Replanned)
    ));
    unregister(&playback.token);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_playback_replanned_behind_the_player_says_so_once_then_reads_the_new_plan() {
    let backend = Arc::new(Hls::default());
    let managed = sdk::ManagedPlayback::adopt(
        backend.clone(),
        "plex:movie:1".parse().unwrap(),
        None,
        sdk::PlaybackOptions::default(),
        snapshot(),
    );
    let playback = SpoolPlayback::new(open_under_token(
        managed.clone(),
        None,
        Default::default(),
        None,
    ));
    let before = playback.token();
    managed
        .change_quality(sdk::PlaybackQuality::Auto, Duration::ZERO)
        .await
        .unwrap();
    let reading = playback.clone();
    let (first, again) = tokio::task::spawn_blocking(move || {
        let first = reading.clone().open_resource("media.m3u8".into());
        (first, reading.open_resource("media.m3u8".into()))
    })
    .await
    .unwrap();
    let container = match playback.info().plan {
        PlaybackPlan::Converted { container, .. } => Some(container),
        _ => None,
    };
    assert_eq!(
        (
            first,
            again.is_ok(),
            playback.token() != before,
            registered(&before).is_none(),
            container
        ),
        (
            Err(SpoolError::Replanned),
            true,
            true,
            true,
            Some(Container::Matroska)
        )
    );
}
