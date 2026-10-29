//! A controllable backend exercises HTTP readers without a server or sleeps.
use super::*;
use spoolframe_client_sdk::{self as sdk, SdkError};
use crate::{MenuEntry, SdkSource};
use sdk::{PlaybackBackend, PlaybackHandle, PlaybackOptions, Playback, Selection, BitmapSubtitles, PlaybackChange, Chunk, types};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default)]
struct Backend {
    hold: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl PlaybackBackend for Backend {
    async fn start(&self, _: &types::MediaId, _: Option<Selection>, _: PlaybackOptions) -> Result<Playback, SdkError> { Ok(snapshot()) }
    async fn read(&self, _: &PlaybackHandle, _: u64, length: u64) -> Result<Vec<u8>, SdkError> { Ok(vec![1; length as usize]) }
    async fn read_chunk(&self, _: &PlaybackHandle, epoch: u32, _: u64, _: u64) -> Result<Chunk, SdkError> {
        Ok(Chunk { bytes: vec![epoch as u8], size: 2, complete: true, epoch })
    }
    async fn read_segment(&self, _: &PlaybackHandle, epoch: u32, _: u32, _: u32, offset: u64, _: u64) -> Result<Chunk, SdkError> {
        if self.hold.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(Chunk { bytes: if offset < 2 { vec![epoch as u8] } else { vec![] }, size: 2, complete: true, epoch })
    }
    async fn seek(&self, _: &PlaybackHandle, _: Duration) -> Result<u32, SdkError> { Ok(1) }
    async fn subtitle(&self, _: &PlaybackHandle, _: Option<u32>, _: BitmapSubtitles, _: Duration) -> Result<PlaybackChange, SdkError> { Err(SdkError::Busy) }
    async fn progress(&self, _: &PlaybackHandle, _: Duration) -> Result<(), SdkError> { Ok(()) }
    async fn stop(&self, _: &PlaybackHandle) -> Result<(), SdkError> { Ok(()) }
}

fn snapshot() -> Playback {
    Playback {
        handle: PlaybackHandle("test-playback".into()),
        plan: sdk::PlaybackPlan::Direct { container: types::Container::Mp4, video: None, audio: None, subtitle: None, source_codec: None },
        size: 2, chosen: None, epoch: 0, markers: vec![],
    }
}

fn stream(backend: Arc<Backend>) -> Arc<Stream<SdkSource>> {
    let playback = sdk::ManagedPlayback::adopt(backend, "plex:movie:1".parse().unwrap(), None, PlaybackOptions::default(), snapshot());
    Arc::new(Stream::new(SdkSource::new(playback), Kind::hls(), None, Default::default()))
}

async fn body(stream: Arc<Stream<SdkSource>>) -> Body {
    let opened = open_segment(&stream, 0, 1).await.unwrap();
    streamed(stream, 0, 1, opened, false, std::time::Instant::now()).into_body()
}

#[tokio::test]
async fn overlapping_reader_cannot_continue_old_encode_after_new_reader_opens() {
    let stream = stream(Arc::new(Backend::default()));
    let mut old = body(stream.clone()).await;
    assert_eq!(old.frame().await.unwrap().unwrap().into_data().unwrap().as_ref(), &[0]);
    stream.playback.playback().seek(Duration::from_secs(2405)).await.unwrap();
    let new = body(stream.clone()).await;
    assert!(old.frame().await.unwrap().is_err(), "old reader must not append the new encode");
    assert_eq!(new.collect().await.unwrap().to_bytes().as_ref(), &[1, 1]);
}

#[tokio::test]
async fn replacement_during_delayed_reopen_rejects_response() {
    let backend = Arc::new(Backend::default());
    let stream = stream(backend.clone());
    let mut old = body(stream.clone()).await;
    old.frame().await.unwrap().unwrap();
    backend.hold.store(true, Ordering::SeqCst);
    let reading = tokio::spawn(async move { old.frame().await.unwrap() });
    backend.entered.notified().await;
    stream.playback.playback().seek(Duration::from_secs(2456)).await.unwrap();
    // A second reader of the SAME rung/segment overwrites the SDK's shared
    // segment_reads entry; the HTTP reader's own revision must still reject it.
    let _new = body(stream.clone()).await;
    backend.release.notify_one();
    assert!(reading.await.unwrap().is_err());
}

#[tokio::test]
async fn retiring_url_cancels_a_delayed_reopen() {
    let backend = Arc::new(Backend::default());
    let stream = stream(backend.clone());
    let mut old = body(stream.clone()).await;
    old.frame().await.unwrap().unwrap();
    backend.hold.store(true, Ordering::SeqCst);
    let reading = tokio::spawn(async move { old.frame().await.unwrap() });
    backend.entered.notified().await;
    stream.retired.send_replace(true);
    assert!(tokio::time::timeout(Duration::from_secs(2), reading).await.unwrap().unwrap().is_err());
}

#[tokio::test]
async fn a_stream_its_caller_keeps_is_the_one_served() {
    let bridge = Bridge::new();
    let kept = stream(Arc::new(Backend::default()));
    let token = bridge.register_shared(kept.clone());
    let entry = MenuEntry { index: 1, name: "English".into(), language: Some("en".into()), forced: false };
    bridge.set_menu(&token, vec![entry.clone()]);
    assert_eq!((kept.menu(), bridge.stream(&token).is_some_and(|s| Arc::ptr_eq(&s, &kept))), (Some(vec![entry]), true));
}
