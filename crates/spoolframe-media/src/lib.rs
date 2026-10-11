//! Spoolframe's own view of a media file (ARCHITECTURE §6, rule 2): it
//! runs ffprobe itself and never trusts Plex to say how a file plays.
//!
//! [`MediaDescriptor`] is the full, server-side description the playback
//! planner works from. [`MediaDescriptor::tracks`] is the part a client
//! sees.

pub mod bitmap;
pub mod codecs;
pub mod decode;
mod descriptor;
pub mod dvb;
pub mod encoders;
mod external;
pub mod gpus;
pub mod harden;
pub mod keyframe_index;
pub mod ocr;
pub mod pgs;
mod probe;
pub mod roles;
pub mod sandbox;
pub mod vobsub;

pub use probe::{KeyframesFrom, ProbeError, Prober};

pub use descriptor::{
    AudioStream, DolbyVision, DvBase, MAX_DURATION, MediaDescriptor, SubtitleKind, SubtitleSource,
    SubtitleStream, VideoRange, VideoStream,
};
