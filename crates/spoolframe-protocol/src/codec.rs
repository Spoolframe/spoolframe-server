//! Frames: a big-endian `u32` length, then that many bytes of CBOR.

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The largest frame a peer accepts. Requests and replies are metadata; a
/// page of items is far smaller. The limit keeps a peer from making the
/// other allocate without bound.
pub const MAX_FRAME: u32 = 1 << 20;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("stream error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame of {len} bytes is over the {MAX_FRAME}-byte limit")]
    TooLarge { len: u64 },
    #[error("cannot encode: {0}")]
    Encode(String),
    #[error("cannot decode: {0}")]
    Decode(String),
}

/// Encode `value` as one frame's payload.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    let mut bytes = Vec::new();
    ciborium::into_writer(value, &mut bytes).map_err(|e| CodecError::Encode(e.to_string()))?;
    Ok(bytes)
}

/// Decode one frame's payload.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CodecError> {
    ciborium::from_reader(bytes).map_err(|e| CodecError::Decode(e.to_string()))
}

/// The `type` of a request payload, even one this build cannot decode, so
/// the server can name it in [`crate::Error::Unsupported`].
pub fn request_type(envelope: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Envelope {
        request: Tagged,
    }
    #[derive(serde::Deserialize)]
    struct Tagged {
        #[serde(rename = "type")]
        kind: String,
    }
    decode::<Envelope>(envelope).ok().map(|e| e.request.kind)
}

/// Write `value` as one frame. One over [`MAX_FRAME`] is refused before
/// anything is written, so the writer can send something else instead.
pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<(), CodecError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = encode(value)?;
    let len = u32::try_from(bytes.len())
        .ok()
        .filter(|&len| len <= MAX_FRAME)
        .ok_or(CodecError::TooLarge {
            len: bytes.len() as u64,
        })?;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(&bytes).await?;
    Ok(())
}

/// Read one frame's payload, refusing an oversized one before reading it.
pub async fn read_frame_bytes<R>(reader: &mut R) -> Result<Vec<u8>, CodecError>
where
    R: AsyncRead + Unpin,
{
    let len = reader.read_u32().await?;
    if len > MAX_FRAME {
        return Err(CodecError::TooLarge { len: len.into() });
    }
    let mut bytes = vec![0; len as usize];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}

pub async fn read_frame<R, T>(reader: &mut R) -> Result<T, CodecError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    decode(&read_frame_bytes(reader).await?)
}
