//! The source over the client SDK's `ManagedPlayback` (feature `sdk`).

use spoolframe_client_sdk::{self as sdk, SdkError};
use tokio::sync::watch;

use crate::source::{Chunk, ManifestKind, Segment, Source, Subtitle};
use crate::{Error, Kind};

impl Kind {
    /// How a bridge serves `playback`, as its plan says: the file as it
    /// is, as the MIME type its container plays as ([`mime`]), or the
    /// server's HLS from the playback's epoch. `None` for a plan a bridge
    /// cannot serve: one the device cannot play, or a progressive
    /// conversion.
    pub fn of(playback: &sdk::Playback) -> Option<Self> {
        match &playback.plan {
            sdk::PlaybackPlan::Direct { container, .. } => {
                Some(Self::direct(playback.size, mime(*container)))
            }
            sdk::PlaybackPlan::Converted {
                delivery: sdk::types::Delivery::Hls,
                ..
            } => Some(Self::Hls {
                epoch: std::sync::Mutex::new(playback.epoch),
            }),
            _ => None,
        }
    }
}

/// The MIME type a file in `container` is served as
/// ([`Container::mime`](sdk::types::Container::mime)).
pub fn mime(container: sdk::types::Container) -> &'static str {
    container.mime()
}

/// A playback read over the SDK. A playback the server ends (idle, a
/// restart) is started again behind it by the SDK, and the read goes on.
#[derive(Clone, Debug)]
pub struct SdkSource {
    playback: sdk::ManagedPlayback,
    /// The plan this source serves. The SDK plans an Auto playback again
    /// behind it (a slow link, a share of the uplink that moved);
    /// one that differs is served under a new path for the player.
    plan: sdk::PlaybackPlan,
}

impl SdkSource {
    /// Serve `playback` planned as `plan`: the plan of the snapshot its
    /// player was given ([`sdk::ManagedPlayback::snapshot_with_generation`]).
    pub fn new(playback: sdk::ManagedPlayback, plan: sdk::PlaybackPlan) -> Self {
        Self { playback, plan }
    }

    pub fn playback(&self) -> &sdk::ManagedPlayback {
        &self.playback
    }
}

impl From<SdkError> for Error {
    fn from(error: SdkError) -> Self {
        match error {
            SdkError::PlaybackClosed | SdkError::PlaybackExpired => Error::Expired,
            SdkError::PlaybackStopped => Error::Stopped,
            SdkError::PlaybackRestarted { epoch } => Error::Restarted { epoch },
            SdkError::PlaybackResumed { at } => Error::Resumed { at },
            SdkError::Replanned => Error::Replanned,
            SdkError::SubtitleNotReady { done_percent } => Error::NotReady { done_percent },
            // Rewritten while read: asked for again, as one not ready is.
            SdkError::SubtitleChanged => Error::NotReady { done_percent: 0 },
            SdkError::NotFound(_) => Error::NotFound,
            other => Error::Other(other.to_string()),
        }
    }
}

impl From<ManifestKind> for sdk::ManifestKind {
    fn from(kind: ManifestKind) -> Self {
        match kind {
            ManifestKind::Multivariant => Self::Multivariant,
            ManifestKind::Media => Self::Media,
            ManifestKind::Subtitles => Self::Subtitles,
        }
    }
}

impl Source for SdkSource {
    type Snapshot = sdk::Playback;
    type Segment = SdkSegment;

    fn changes(&self) -> watch::Receiver<sdk::Playback> {
        self.playback.changes()
    }

    fn replanned(&self) -> bool {
        !self.playback.is_planned(&self.plan)
    }

    async fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>, Error> {
        Ok(self.playback.read(offset, length).await?)
    }

    async fn manifest(&self, kind: ManifestKind, variant: u32) -> Result<String, Error> {
        Ok(self.playback.manifest(kind.into(), variant).await?)
    }

    async fn read_segment(
        &self,
        variant: u32,
        n: u32,
        offset: u64,
        length: u64,
    ) -> Result<Chunk, Error> {
        let chunk = self
            .playback
            .read_segment(variant, n, offset, length)
            .await?;
        Ok(Chunk {
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
    ) -> Result<SdkSegment, Error> {
        Ok(SdkSegment(
            self.playback
                .segment_stream(variant, n, offset, length)
                .await?,
        ))
    }

    async fn read_subtitle_segment(&self, n: u32) -> Result<Subtitle, Error> {
        let subtitle = self.playback.read_subtitle_segment(n).await?;
        Ok(Subtitle {
            bytes: subtitle.bytes,
        })
    }
}

/// A segment's answer over the SDK.
#[derive(Debug)]
pub struct SdkSegment(sdk::SegmentStream);

impl Segment for SdkSegment {
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

    async fn next(&mut self) -> Result<Option<Vec<u8>>, Error> {
        Ok(self.0.next().await?)
    }

    async fn reopen(&self, length: u64) -> Result<Self, Error> {
        Ok(Self(self.0.reopen(length).await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdk::types::{Container, Delivery};

    fn playback(plan: sdk::PlaybackPlan, epoch: u32) -> sdk::Playback {
        sdk::Playback {
            handle: sdk::PlaybackHandle("p".into()),
            plan,
            size: 2048,
            chosen: None,
            epoch,
            markers: Vec::new(),
            duration: None,
        }
    }

    fn direct(container: Container) -> sdk::PlaybackPlan {
        sdk::PlaybackPlan::Direct {
            container,
            video: None,
            audio: None,
            subtitle: None,
            source_codec: None,
        }
    }

    fn converted(delivery: Delivery) -> sdk::PlaybackPlan {
        sdk::PlaybackPlan::Converted {
            delivery,
            container: Container::Mp4,
            video: None,
            audio: None,
            subtitle: None,
            ladder: Vec::new(),
            reasons: Vec::new(),
            source_codec: None,
        }
    }

    #[test]
    fn a_direct_plan_is_its_file_as_its_container_plays() {
        let kind = Kind::of(&playback(direct(Container::Mp4), 0)).unwrap();
        assert!(matches!(&kind, Kind::Direct { size: 2048, mime } if mime == "video/mp4"));
        assert_eq!(kind.entry(), "file");
    }

    #[test]
    fn a_file_is_typed_as_the_server_types_it_and_an_unknown_one_sniffed() {
        let types = [
            Container::Matroska,
            Container::MpegTs,
            Container::Flac,
            Container::Unknown,
        ]
        .map(mime);
        assert_eq!(
            types,
            [
                "video/x-matroska",
                "video/mp2t",
                "audio/flac",
                "application/octet-stream"
            ]
        );
    }

    #[test]
    fn an_hls_plan_is_its_master_playlist_from_its_epoch() {
        let kind = Kind::of(&playback(converted(Delivery::Hls), 3)).unwrap();
        assert!(matches!(&kind, Kind::Hls { epoch } if *epoch.lock().unwrap() == 3));
        assert_eq!(kind.entry(), "master.m3u8");
    }

    #[test]
    fn a_plan_a_bridge_cannot_serve_is_none() {
        assert!(Kind::of(&playback(converted(Delivery::Progressive), 0)).is_none());
        let unsupported = sdk::PlaybackPlan::Unsupported {
            reasons: vec!["audio 0".into()],
        };
        assert!(Kind::of(&playback(unsupported, 0)).is_none());
    }
}
