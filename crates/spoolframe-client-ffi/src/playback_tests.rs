//! The app's side of playback: a playback the caller never received
//! closes its token (and URL), a player that reads the bytes itself reads
//! them under its token, and a served playback's URL has the shape the
//! player depends on. The bridge's own tests are in spoolframe-hls-bridge.

use super::*;
use sdk::{BitmapSubtitles, Chunk, PlaybackBackend, PlaybackChange, PlaybackHandle, SdkError};
use spoolframe_client_sdk::types::Container;

struct Backend;

#[async_trait::async_trait]
impl PlaybackBackend for Backend {
    async fn start(
        &self,
        _: &MediaId,
        _: Option<Selection>,
        _: sdk::PlaybackOptions,
    ) -> Result<sdk::Playback, SdkError> {
        Ok(snapshot())
    }
    async fn read(&self, _: &PlaybackHandle, _: u64, length: u64) -> Result<Vec<u8>, SdkError> {
        Ok(vec![1; length as usize])
    }
    async fn read_chunk(
        &self,
        _: &PlaybackHandle,
        epoch: u32,
        _: u64,
        _: u64,
    ) -> Result<Chunk, SdkError> {
        Ok(Chunk {
            bytes: vec![epoch as u8],
            size: 2,
            complete: true,
            epoch,
        })
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
        Ok(Chunk {
            bytes: vec![epoch as u8],
            size: 2,
            complete: true,
            epoch,
        })
    }
    async fn seek(&self, _: &PlaybackHandle, _: Duration) -> Result<u32, SdkError> {
        Ok(1)
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
        handle: PlaybackHandle("test-playback".into()),
        plan: PlaybackPlan::Direct {
            container: Container::Mp4,
            video: None,
            audio: None,
            subtitle: None,
            source_codec: None,
        },
        size: 2,
        chosen: None,
        epoch: 0,
        markers: vec![],
    }
}

fn managed() -> sdk::ManagedPlayback {
    sdk::ManagedPlayback::adopt(
        Arc::new(Backend),
        "plex:movie:1".parse().unwrap(),
        None,
        sdk::PlaybackOptions::default(),
        snapshot(),
    )
}

#[tokio::test]
async fn an_abandoned_completed_result_closes_its_token() {
    let opened = open_under_token(managed(), None, Default::default(), None);
    let token = opened.0.as_ref().unwrap().token.clone();
    assert!(registered(&token).is_some());
    drop(opened);
    assert!(registered(&token).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_opened_playback_has_no_url_and_its_file_reads_under_its_token() {
    let start = open_under_token(managed(), None, Default::default(), None).take();
    let reader = crate::reader::reader(&start.token, None).unwrap();
    let bytes = tokio::task::spawn_blocking(move || reader.read(0, 2))
        .await
        .unwrap();
    assert_eq!((start.url, bytes), (None, Ok(vec![1, 1])));
    unregister(&start.token);
}

#[tokio::test]
async fn an_opened_file_reads_as_its_size_and_its_container_s_type() {
    let start = open_under_token(managed(), None, Default::default(), None).take();
    let reader = crate::reader::reader(&start.token, None).unwrap();
    assert_eq!(
        (reader.size(), reader.content_type()),
        (Some(2), "video/mp4".to_owned())
    );
    unregister(&start.token);
}

#[tokio::test]
async fn a_reopened_playback_keeps_its_subtitle_menu() {
    let start = open_under_token(managed(), None, Default::default(), None).take();
    let entry = SubtitleEntry {
        index: 2,
        name: "English".into(),
        language: Some("en".into()),
        forced: false,
    };
    let stream = registered(&start.token).unwrap().stream.clone().unwrap();
    stream.set_menu(vec![entry.clone().into()]);
    let old = unregister(&start.token).unwrap();
    let new = reopen(&old).await.unwrap().take();
    let menu = registered(&new.token)
        .unwrap()
        .stream
        .as_ref()
        .unwrap()
        .menu();
    assert_eq!(menu, Some(vec![MenuEntry::from(entry)]));
    unregister(&new.token);
}

#[tokio::test]
async fn a_playback_the_bridge_does_not_serve_is_open_under_its_token() {
    let mut playback = snapshot();
    playback.plan = PlaybackPlan::Converted {
        delivery: Delivery::Progressive,
        container: Container::Matroska,
        video: None,
        audio: None,
        subtitle: None,
        ladder: Vec::new(),
        reasons: Vec::new(),
        source_codec: None,
    };
    let managed = sdk::ManagedPlayback::adopt(
        Arc::new(Backend),
        "plex:movie:1".parse().unwrap(),
        None,
        sdk::PlaybackOptions::default(),
        playback,
    );
    let start = open_under_token(managed, None, Default::default(), None).take();
    let stream = registered(&start.token).map(|p| p.stream.is_some());
    assert_eq!(stream, Some(false));
    unregister(&start.token);
}

#[test]
fn a_closed_token_has_no_reader() {
    assert!(matches!(
        crate::reader::reader("no-such-token", None),
        Err(SpoolError::PlaybackExpired)
    ));
}

#[cfg(feature = "hls-bridge")]
#[tokio::test]
async fn a_direct_playback_is_served_as_its_file_on_loopback() {
    let opened = open_under_token(managed(), None, Default::default(), None);
    let start = run(crate::bridge::serve_opened(opened))
        .await
        .map(RegisteredPlayback::take)
        .unwrap();
    let url = start.url.clone().unwrap();
    assert!(
        !start.hls
            && url.starts_with("http://127.0.0.1:")
            && url.ends_with(&format!("/{}/file", start.token)),
        "{url}"
    );
    unregister(&start.token);
}
