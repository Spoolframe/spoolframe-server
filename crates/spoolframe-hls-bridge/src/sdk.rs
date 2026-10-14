//! The source over the client SDK's `ManagedPlayback`, as the Apple app's
//! bridge read it (feature `sdk`).

use spoolframe_client_sdk::{self as sdk, SdkError};
use tokio::sync::watch;

use crate::Error;
use crate::source::{Chunk, ManifestKind, Segment, Source, Subtitle};

/// A playback read over the SDK. A playback the server ends (idle, a
/// restart) is started again behind it by the SDK, and the read goes on.
#[derive(Clone, Debug)]
pub struct SdkSource {
    playback: sdk::ManagedPlayback,
    /// The plan this source serves. The SDK plans an Auto playback again
    /// behind it (PX-46: a slow link, a share of the uplink that moved);
    /// one that differs is served under a new path for the player.
    plan: sdk::PlaybackPlan,
}

impl SdkSource {
    /// Serve `playback` as it is planned now.
    pub fn new(playback: sdk::ManagedPlayback) -> Self {
        let plan = playback.snapshot().plan;
        Self { playback, plan }
    }

    pub fn playback(&self) -> &sdk::ManagedPlayback {
        &self.playback
    }

    /// The plan it was served with.
    pub fn plan(&self) -> &sdk::PlaybackPlan {
        &self.plan
    }
}

impl From<SdkError> for Error {
    fn from(error: SdkError) -> Self {
        match error {
            SdkError::PlaybackClosed => Error::PlaybackClosed,
            SdkError::PlaybackExpired => Error::PlaybackExpired,
            SdkError::PlaybackStopped => Error::PlaybackStopped,
            SdkError::PlaybackRestarted { epoch } => Error::PlaybackRestarted { epoch },
            SdkError::PlaybackResumed { at } => Error::PlaybackResumed { at },
            SdkError::SubtitleNotReady { done_percent } => Error::SubtitleNotReady { done_percent },
            SdkError::SubtitleChanged => Error::SubtitleChanged,
            SdkError::InvalidInput(message) => Error::InvalidInput(message),
            SdkError::Server(message) => Error::Server(message),
            SdkError::Protocol(message) => Error::Protocol(message),
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
        self.playback.snapshot().plan != self.plan
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
