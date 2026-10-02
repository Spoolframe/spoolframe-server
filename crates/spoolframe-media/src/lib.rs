//! Spoolframe's own view of a media file (ARCHITECTURE §6, rule 2): it
//! runs ffprobe itself and never trusts Plex to say how a file plays.
//!
//! [`MediaDescriptor`] is the full, server-side description the playback
//! planner works from. [`MediaDescriptor::tracks`] is the part a client
//! sees.

mod descriptor;
mod external;
mod probe;

pub use probe::{ProbeError, Prober};

pub use descriptor::{
    AudioStream, MediaDescriptor, SubtitleKind, SubtitleSource, SubtitleStream, VideoStream,
};
