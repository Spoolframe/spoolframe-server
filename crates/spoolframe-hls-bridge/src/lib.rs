//! A playback served over HTTP, for players that only take a URL: AVPlayer
//! in the Apple apps, and AirPlay receivers and TV players that fetch from
//! the server.
//!
//! A [`Bridge`] holds playbacks under unguessable tokens and answers
//! `/<token>/…` with what each one's [`Source`] reads:
//!
//! - Direct play: `/<token>/file`, with byte ranges.
//! - HLS: `master.m3u8` (the multivariant playlist), `media.m3u8`,
//!   `subtitles.m3u8`, `init.mp4` and `seg<n>.m4s` (segment 0 and n),
//!   `subs<n>.vtt`: the names the server's playlists use, so they need no
//!   rewriting. A quality ladder puts each rung's `media.m3u8`, `init.mp4`
//!   and segments under `v<n>/`; unprefixed names are rung 0.
//! - With subtitle tracks set ([`Bridge::set_menu`]), `master.m3u8` names
//!   every subtitle track of the item, so the system player's menu lists
//!   them: the one playing on the server's `subtitles.m3u8`, each other on
//!   `subtitles-<i>.m3u8`, an empty placeholder.
//!
//! A playlist may only name the playback's own resources, and a segment
//! may hold at most [`SEGMENT_MAX`] bytes.
//!
//! The crate reads no environment variables: pass its settings in
//! [`BridgeOptions`]. It logs through `tracing`.
//!
//! Listening is the caller's choice: the app binds 127.0.0.1, the server
//! its configured address. [`Bridge::serve`] takes a bound listener; the
//! same bridge may be served again on a new one after the first stops.
//!
//! Permissively licensed (MIT OR Apache-2.0) so proprietary clients can
//! link it; it never depends on an AGPL crate. The source over the client
//! SDK is behind the `sdk` feature.

// The moved code is kept as the app had it, unformatted, so it diffs
// cleanly against bc2eb28.
#[rustfmt::skip]
mod bridge;
mod error;
#[rustfmt::skip]
mod http;
#[cfg(feature = "sdk")]
mod sdk;
mod source;

pub use bridge::{Bridge, BridgeOptions, Kind, MenuEntry, RequestHook, Stream};
pub use error::Error;
pub use http::{Body, SEGMENT_MAX};
#[cfg(feature = "sdk")]
pub use sdk::{SdkSegment, SdkSource};
pub use source::{Chunk, ManifestKind, Segment, Source, Subtitle};
