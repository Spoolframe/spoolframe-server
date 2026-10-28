//! What the bridge reads a playback through, with no SDK types. An app
//! reads over the client SDK (the `sdk` feature gives
//! `SdkSource`); the server reads its own segments.

use std::future::Future;

use tokio::sync::watch;

use crate::Error;

/// Which HLS playlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ManifestKind {
    /// The top playlist, naming the variants.
    Multivariant,
    /// A variant's segments.
    Media,
    /// The subtitle rendition's WebVTT segments.
    Subtitles,
}

/// Part of a segment, read whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub bytes: Vec<u8>,
    /// The segment's bytes written so far: all of it once `complete`.
    pub size: u64,
    /// The encode has finished it: no more bytes will come.
    pub complete: bool,
    pub epoch: u32,
}

/// A WebVTT subtitle segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subtitle {
    pub bytes: Vec<u8>,
}

/// A playback, read. Called from many requests at once.
pub trait Source: Send + Sync + 'static {
    /// What [`Self::changes`] carries. The bridge only asks whether it
    /// changed.
    type Snapshot: Send + Sync + 'static;
    /// A segment's answer, its bytes arriving.
    type Segment: Segment;

    /// The playback after each change: a seek, a subtitle change, or a
    /// resume behind the player's back. A segment read begun before a
    /// change is not continued after it.
    fn changes(&self) -> watch::Receiver<Self::Snapshot>;

    /// Whether the playback was planned again since it was served. The app
    /// then serves the new plan under a new token.
    fn replanned(&self) -> bool;

    /// Up to `length` bytes of a direct playback's file from `offset`; none
    /// past its end.
    fn read(&self, offset: u64, length: u64)
    -> impl Future<Output = Result<Vec<u8>, Error>> + Send;

    /// An HLS playlist. For a media playlist, `variant` is the ladder rung
    /// (0 when there is one encode). It is 0 for the other kinds.
    fn manifest(
        &self,
        kind: ManifestKind,
        variant: u32,
    ) -> impl Future<Output = Result<String, Error>> + Send;

    /// Up to `length` bytes of HLS segment `n` (0 is the init segment) of
    /// rung `variant` from `offset`. A read past what is encoded may wait,
    /// and may come back empty.
    fn read_segment(
        &self,
        variant: u32,
        n: u32,
        offset: u64,
        length: u64,
    ) -> impl Future<Output = Result<Chunk, Error>> + Send;

    /// [`Self::read_segment`], its bytes handed on as they arrive.
    fn segment_stream(
        &self,
        variant: u32,
        n: u32,
        offset: u64,
        length: u64,
    ) -> impl Future<Output = Result<Self::Segment, Error>> + Send;

    /// HLS subtitle segment `n` (from 1).
    fn read_subtitle_segment(&self, n: u32)
    -> impl Future<Output = Result<Subtitle, Error>> + Send;
}

/// One answer for a segment, its bytes arriving: its size and whether it is
/// whole first, then the body piece by piece.
pub trait Segment: Send + Sized + 'static {
    /// The bytes this answer carries.
    fn sent(&self) -> u64;
    /// The segment's bytes written so far: all of it once [`Self::complete`].
    fn size(&self) -> u64;
    /// The encode has finished the segment.
    fn complete(&self) -> bool;
    fn epoch(&self) -> u32;
    /// The next piece, as much as arrived; `None` once all `sent()` bytes
    /// did. [`Error::Resumed`](crate::Error::Resumed) or [`Error::Restarted`](crate::Error::Restarted) if
    /// the encode was replaced since it opened; any other error part-way,
    /// read on with [`Self::reopen`].
    fn next(&mut self) -> impl Future<Output = Result<Option<Vec<u8>>, Error>> + Send;
    /// Read on from where this answer stopped, up to `length` bytes, from
    /// the encode it was opened in or not at all.
    fn reopen(&self, length: u64) -> impl Future<Output = Result<Self, Error>> + Send;
}
