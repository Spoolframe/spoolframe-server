//! Frames: a big-endian `u32` length, then that many bytes of CBOR.

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The largest frame a peer accepts. Requests and replies are metadata; a
/// page of items is far smaller. The limit keeps a peer from making the
/// other allocate without bound.
pub const MAX_FRAME: u32 = 1 << 20;

/// RPC replies can span frames, but never exceed this aggregate budget.
pub const MAX_REPLY: usize = 64 << 20;

/// Encode without allocating past `limit`, including for an oversized value.
pub fn encode_limited<T: Serialize>(value: &T, limit: usize) -> Result<Vec<u8>, CodecError> {
    struct Bounded {
        bytes: Vec<u8>,
        limit: usize,
        exceeded: bool,
    }
    impl std::io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit - self.bytes.len() {
                self.exceeded = true;
                return Err(std::io::Error::other("message limit"));
            }
            let needed = self.bytes.len() + bytes.len();
            if needed > self.bytes.capacity() {
                let capacity = needed
                    .max(self.bytes.capacity().saturating_mul(2))
                    .min(self.limit);
                self.bytes
                    .try_reserve_exact(capacity - self.bytes.len())
                    .map_err(std::io::Error::other)?;
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut buffer = Bounded {
        bytes: Vec::new(),
        limit,
        exceeded: false,
    };
    ciborium::into_writer(value, &mut buffer).map_err(|error| {
        if buffer.exceeded {
            CodecError::TooLarge {
                len: limit as u64 + 1,
            }
        } else {
            CodecError::Encode(error.to_string())
        }
    })?;
    Ok(buffer.bytes)
}

/// RPC reply: total encoded length, followed by length-prefixed chunks.
/// The complete message is checked before any bytes are written.
pub async fn write_reply<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    value: &T,
) -> Result<(), CodecError> {
    let encoded = encode_limited(value, MAX_REPLY)?;
    writer.write_u64(encoded.len() as u64).await?;
    for chunk in encoded.chunks(MAX_FRAME as usize) {
        writer.write_u32(chunk.len() as u32).await?;
        writer.write_all(chunk).await?;
    }
    Ok(())
}

pub async fn read_reply<R: AsyncRead + Unpin, T: DeserializeOwned>(
    reader: &mut R,
) -> Result<T, CodecError> {
    let length = reader.read_u64().await?;
    if length == 0 || length > MAX_REPLY as u64 {
        return Err(CodecError::TooLarge { len: length });
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length as usize)
        .map_err(|e| CodecError::Decode(e.to_string()))?;
    while bytes.len() < length as usize {
        let chunk = reader.read_u32().await?;
        if chunk == 0 || chunk > MAX_FRAME || chunk as usize > length as usize - bytes.len() {
            return Err(CodecError::Decode("invalid RPC reply chunk length".into()));
        }
        let offset = bytes.len();
        bytes.resize(offset + chunk as usize, 0);
        reader.read_exact(&mut bytes[offset..]).await?;
    }
    decode(&bytes)
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("stream error: {0}")]
    Io(#[from] std::io::Error),
    #[error("message of {len} bytes exceeds its size limit")]
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

/// The most characters of a request's `type` that [`request_type`] and
/// [`rpc_request_type`] read: a client chooses it, up to a whole frame of
/// it, and every type the protocol has is far shorter (C-12).
pub const MAX_REQUEST_TYPE: usize = 64;

/// The `type` of a request payload, even one this build cannot decode, so
/// the server can name it in [`crate::Error::Unsupported`].
pub fn request_type(envelope: &[u8]) -> Option<String> {
    scan::text_at(envelope, &["request", "type"])
}

/// Extract a request tag from the mandatory RPC wrapper, including unknown tags.
///
/// The server names every request with this before it is authenticated, so
/// it reads the frame only as far as the tag and keeps nothing of what it
/// steps over (finding 50): decoding the frame whole cost about 50 times
/// its size in heap.
pub fn rpc_request_type(bytes: &[u8]) -> Option<String> {
    scan::text_at(bytes, &["envelope", "request", "type"])
}

/// Reading one text value out of CBOR without decoding what is around it.
mod scan {
    use ciborium_ll::{Decoder, Header};

    type Reader<'a> = Decoder<&'a [u8]>;

    /// How deep arrays and maps that are stepped over may nest: ciborium's
    /// own limit, so whatever the typed decode reads can be named.
    const DEPTH: usize = 256;

    /// The bytes read at a time from a text or byte string.
    const CHUNK: usize = 64;

    /// The text under `path`, a key in each of the nested maps in turn: at
    /// most [`super::MAX_REQUEST_TYPE`] characters of it.
    pub(super) fn text_at(bytes: &[u8], path: &[&str]) -> Option<String> {
        let mut reader = Decoder::from(bytes);
        for key in path {
            enter(&mut reader, key)?;
        }
        match header(&mut reader)? {
            Header::Text(len) => text(&mut reader, len, super::MAX_REQUEST_TYPE),
            _ => None,
        }
    }

    /// The next header that is not a tag: a tag only annotates the item
    /// after it.
    fn header(reader: &mut Reader<'_>) -> Option<Header> {
        loop {
            match reader.pull().ok()? {
                Header::Tag(_) => continue,
                header => return Some(header),
            }
        }
    }

    /// Read into the map that comes next, up to the value of `key` in it.
    fn enter(reader: &mut Reader<'_>, key: &str) -> Option<()> {
        let Header::Map(len) = header(reader)? else {
            return None;
        };
        let mut left = len;
        loop {
            match &mut left {
                Some(0) => return None,
                Some(n) => *n -= 1,
                None => {}
            }
            let found = match header(reader)? {
                Header::Break if len.is_none() => return None,
                // One character past the key is enough to tell it apart.
                Header::Text(n) => text(reader, n, key.chars().count() + 1)?.as_str() == key,
                other => {
                    skip(reader, other, DEPTH)?;
                    false
                }
            };
            if found {
                return Some(());
            }
            let value = header(reader)?;
            skip(reader, value, DEPTH)?;
        }
    }

    /// The first `max` characters of the text whose header was `len`; the
    /// rest of it is read and dropped.
    fn text(reader: &mut Reader<'_>, len: Option<usize>, max: usize) -> Option<String> {
        let mut kept = String::new();
        let mut count = 0;
        let mut chunk = [0; CHUNK];
        let mut segments = reader.text(len);
        while let Some(mut segment) = segments.pull().ok()? {
            while let Some(part) = segment.pull(&mut chunk).ok()? {
                for c in part.chars().take(max - count) {
                    kept.push(c);
                    count += 1;
                }
            }
        }
        Some(kept)
    }

    /// Step over the item whose header was `header`, keeping none of it.
    fn skip(reader: &mut Reader<'_>, header: Header, depth: usize) -> Option<()> {
        let depth = depth.checked_sub(1)?;
        match header {
            Header::Text(len) => drop_text(reader, len),
            Header::Bytes(len) => drop_bytes(reader, len),
            Header::Array(len) => items(reader, len, depth),
            Header::Map(Some(len)) => items(reader, Some(len.checked_mul(2)?), depth),
            Header::Map(None) => items(reader, None, depth),
            Header::Break => None,
            // A number or a simple value is all header.
            _ => Some(()),
        }
    }

    /// Step over `len` items, or up to a break when `len` is `None`.
    fn items(reader: &mut Reader<'_>, len: Option<usize>, depth: usize) -> Option<()> {
        match len {
            // Each item is at least a byte, so a claimed length past the
            // frame ends at the frame's end.
            Some(len) => {
                for _ in 0..len {
                    let item = header(reader)?;
                    skip(reader, item, depth)?;
                }
            }
            None => loop {
                match header(reader)? {
                    Header::Break => break,
                    item => skip(reader, item, depth)?,
                }
            },
        }
        Some(())
    }

    // Kept out of `skip`, so a deep nest does not carry a buffer per level.
    #[inline(never)]
    fn drop_text(reader: &mut Reader<'_>, len: Option<usize>) -> Option<()> {
        let mut chunk = [0; CHUNK];
        let mut segments = reader.text(len);
        while let Some(mut segment) = segments.pull().ok()? {
            while segment.pull(&mut chunk).ok()?.is_some() {}
        }
        Some(())
    }

    #[inline(never)]
    fn drop_bytes(reader: &mut Reader<'_>, len: Option<usize>) -> Option<()> {
        let mut chunk = [0; CHUNK];
        let mut segments = reader.bytes(len);
        while let Some(mut segment) = segments.pull().ok()? {
            while segment.pull(&mut chunk).ok()?.is_some() {}
        }
        Some(())
    }
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
    read_frame_bytes_within(reader, MAX_FRAME).await
}

/// As [`read_frame_bytes`], refusing a frame longer than `max` (at most
/// [`MAX_FRAME`]) before anything is allocated for it.
pub async fn read_frame_bytes_within<R>(reader: &mut R, max: u32) -> Result<Vec<u8>, CodecError>
where
    R: AsyncRead + Unpin,
{
    let len = reader.read_u32().await?;
    if len > max.min(MAX_FRAME) {
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

/// As [`read_frame`], refusing a frame longer than `max`.
pub async fn read_frame_within<R, T>(reader: &mut R, max: u32) -> Result<T, CodecError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    decode(&read_frame_bytes_within(reader, max).await?)
}
