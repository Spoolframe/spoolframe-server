//! Artwork over a resource stream ([`crate::streams::bi::RESOURCE`]).
//!
//! The client opens a bi stream, writes the kind byte and one
//! [`crate::resource::ResourceRequest`] containing [`ArtworkRequest`].
//! The server answers with [`crate::resource::ResourceResponse`] and exactly `length` raw
//! bytes of the image. Each image is its own stream, so a slow one never
//! holds up RPCs or media.

use serde::{Deserialize, Serialize};
use spoolframe_types::ArtworkId;

use crate::messages::{Error, SessionId};

/// The widths the server serves. A fixed set, not a free number: a free
/// width would let one client fill the cache with every width there is.
/// Clients pick the smallest that covers what they draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum ArtworkSize {
    /// 160 px wide.
    Thumb,
    /// 320 px wide.
    Small,
    /// 640 px wide.
    Medium,
    /// 1280 px wide.
    Large,
    /// As the catalog has it.
    Original,
    /// A size this build does not know.
    #[serde(other)]
    Unknown,
}

impl ArtworkSize {
    /// The width to scale down to; `None` for the original.
    pub fn width(self) -> Option<u32> {
        match self {
            ArtworkSize::Thumb => Some(160),
            ArtworkSize::Small => Some(320),
            ArtworkSize::Medium => Some(640),
            ArtworkSize::Large => Some(1280),
            ArtworkSize::Original | ArtworkSize::Unknown => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ArtworkSize::Thumb => "thumb",
            ArtworkSize::Small => "small",
            ArtworkSize::Medium => "medium",
            ArtworkSize::Large => "large",
            ArtworkSize::Original => "original",
            ArtworkSize::Unknown => "unknown",
        }
    }
}

/// A face found in a backdrop (experimental `smart-crop`). Each edge is a
/// fraction of the image's width or height, from its left or top, so it
/// holds at every [`ArtworkSize`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ArtworkFace {
    pub left: f32,
    pub right: f32,
    /// A box runs from about the brow to the chin: hair is above it.
    pub top: f32,
    pub bottom: f32,
    /// The eyes' height: the mean of the two.
    pub eyes: f32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtworkRequest {
    pub session: SessionId,
    pub id: ArtworkId,
    pub size: ArtworkSize,
}

/// What follows on the stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtworkReply {
    /// e.g. `image/jpeg`.
    pub content_type: String,
    /// How many bytes of image follow this frame.
    pub length: u64,
    /// Stable for this `(id, size)`: artwork ids change when the image
    /// does, so a client may keep its own copy under this key.
    pub etag: String,
    /// Experimental (`smart-crop`): the faces in a backdrop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub faces: Option<Vec<ArtworkFace>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtworkResponse {
    Ok(ArtworkReply),
    Err(Error),
}
