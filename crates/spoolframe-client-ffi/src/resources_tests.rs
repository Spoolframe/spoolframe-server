//! A conversion's HLS read by name with no bridge server, as Media3 reads
//! it: the playlists, a segment, and a run of ranges from one fetch.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use spoolframe_client_sdk::types::{Container, Delivery};
use spoolframe_client_sdk::{
    self as sdk, BitmapSubtitles, Chunk, ManifestKind, PlaybackBackend, PlaybackChange,
    PlaybackHandle, PlaybackPlan, SdkError, Selection,
};

use super::*;
use crate::playback::{RegisteredPlayback, open_under_token, unregister};

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
        _: BitmapSubtitles,
        _: Duration,
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
        _: BitmapSubtitles,
        _: Duration,
    ) -> Result<PlaybackChange, SdkError> {
        Err(SdkError::Busy)
    }
    async fn progress(&self, _: &PlaybackHandle, _: Duration) -> Result<(), SdkError> {
        Ok(())
    }
    async fn stop(&self, _: &PlaybackHandle) -> Result<(), SdkError> {
        Ok(())
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
    }
}

/// A conversion opened under a token, and its server's read count.
fn opened() -> (String, Arc<Hls>) {
    let backend = Arc::new(Hls::default());
    let managed = sdk::ManagedPlayback::adopt(
        backend.clone(),
        "plex:movie:1".parse().unwrap(),
        None,
        sdk::PlaybackOptions::default(),
        snapshot(),
    );
    let start = RegisteredPlayback::take(open_under_token(managed, None, Default::default(), None));
    (start.token, backend)
}

fn text(fetched: &Fetched) -> String {
    String::from_utf8_lossy(&fetched.bytes).into_owned()
}

#[tokio::test]
async fn a_conversion_s_playlists_read_by_name_name_their_resources_relatively() {
    let (token, _) = opened();
    let master = fetch(token.clone(), "master.m3u8".into()).await.unwrap();
    let media = fetch(token.clone(), "media.m3u8".into()).await.unwrap();
    assert_eq!(
        (master.content_type, text(&media).contains("\nseg1.m4s\n")),
        ("application/vnd.apple.mpegurl", true)
    );
    unregister(&token);
}

#[tokio::test]
async fn a_conversion_s_segment_reads_by_name() {
    let (token, _) = opened();
    let segment = fetch(token.clone(), "seg1.m4s".into()).await.unwrap();
    assert_eq!(
        (segment.content_type, segment.bytes.as_slice()),
        ("video/iso.segment", SEGMENT)
    );
    unregister(&token);
}

#[tokio::test]
async fn a_run_of_ranges_in_one_segment_reads_it_from_the_server_once() {
    let (token, server) = opened();
    fetch(token.clone(), "seg1.m4s".into()).await.unwrap();
    let mut read = Vec::new();
    for offset in [0, 4, 8] {
        read.extend(
            read_range(token.clone(), "seg1.m4s".into(), offset, 4)
                .await
                .unwrap(),
        );
    }
    assert_eq!(
        (read.as_slice(), server.reads.load(Ordering::Relaxed)),
        (SEGMENT, 1)
    );
    unregister(&token);
}

#[tokio::test]
async fn a_segment_opened_again_reads_from_the_server_once() {
    let (token, server) = opened();
    open(token.clone(), "seg1.m4s".into()).await.unwrap();
    let again = open(token.clone(), "seg1.m4s".into()).await.unwrap();
    assert_eq!(
        (again.bytes.as_slice(), server.reads.load(Ordering::Relaxed)),
        (SEGMENT, 1)
    );
    unregister(&token);
}

#[tokio::test]
async fn a_playlist_opened_again_is_read_again() {
    let (token, server) = opened();
    open(token.clone(), "media.m3u8".into()).await.unwrap();
    open(token.clone(), "media.m3u8".into()).await.unwrap();
    assert_eq!(server.reads.load(Ordering::Relaxed), 2);
    unregister(&token);
}

#[tokio::test]
async fn a_subtitle_opened_again_is_read_again() {
    let (token, _) = opened();
    let first = open(token.clone(), "empty.vtt".into()).await.unwrap();
    let again = open(token.clone(), "empty.vtt".into()).await.unwrap();
    assert!(!Arc::ptr_eq(&first, &again));
    unregister(&token);
}

#[tokio::test]
async fn a_range_of_another_resource_reads_that_one() {
    let (token, server) = opened();
    fetch(token.clone(), "seg1.m4s".into()).await.unwrap();
    let playlist = read_range(token.clone(), "media.m3u8".into(), 0, 7)
        .await
        .unwrap();
    assert_eq!(
        (playlist.as_slice(), server.reads.load(Ordering::Relaxed)),
        (&b"#EXTM3U"[..], 2)
    );
    unregister(&token);
}

#[tokio::test]
async fn a_resource_its_playback_does_not_have_is_not_found() {
    let (token, _) = opened();
    assert!(matches!(
        fetch(token.clone(), "../file".into()).await,
        Err(SpoolError::NotFound { .. })
    ));
    unregister(&token);
}

#[tokio::test]
async fn ranges_of_two_resources_opened_at_once_read_each_from_the_server_once() {
    let (token, server) = opened();
    let segment = fetch(token.clone(), "seg1.m4s".into()).await.unwrap();
    let playlist = fetch(token.clone(), "media.m3u8".into()).await.unwrap();
    let mut read = (Vec::new(), Vec::new());
    for offset in [0, 4, 8] {
        read.0.extend(
            read_range(token.clone(), "seg1.m4s".into(), offset, 4)
                .await
                .unwrap(),
        );
        read.1.extend(
            read_range(token.clone(), "media.m3u8".into(), offset, 4)
                .await
                .unwrap(),
        );
    }
    assert_eq!(
        (
            read.0 == segment.bytes,
            read.1 == playlist.bytes[..12],
            server.reads.load(Ordering::Relaxed)
        ),
        (true, true, 2)
    );
    unregister(&token);
}

#[tokio::test]
async fn ending_a_playback_ends_a_read_waiting_on_a_slow_segment() {
    let (token, server) = opened();
    server.slow.store(true, Ordering::Relaxed);
    let reading = tokio::spawn(fetch(token.clone(), "seg1.m4s".into()));
    while server.reads.load(Ordering::Relaxed) == 0 {
        tokio::task::yield_now().await;
    }
    unregister(&token);
    let ended = tokio::time::timeout(Duration::from_secs(5), reading).await;
    let ended = ended.map(|joined| joined.unwrap().err());
    assert_eq!(ended.ok(), Some(Some(SpoolError::PlaybackExpired)));
}

#[tokio::test]
async fn a_resource_asked_after_a_replan_says_to_follow_it() {
    let (token, _) = opened();
    let managed = registered(&token).unwrap().managed.clone();
    managed
        .change_quality(sdk::PlaybackQuality::Auto, Duration::ZERO)
        .await
        .unwrap();
    assert!(matches!(
        fetch(token.clone(), "media.m3u8".into()).await,
        Err(SpoolError::Replanned)
    ));
    unregister(&token);
}
