//! The RPC stream of `spoolframe/client/1`. Every request goes in this
//! wrapper: a peer that does not know it fails to decode the request, so
//! it never loses the wrapper's guarantees without notice.

use crate::{PlaybackHandle, RequestEnvelope, Response};
use serde::{Deserialize, Serialize};

/// A process incarnation and its clock at the time of the reply. Operation
/// expiry uses this clock, not the client's wall clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInstance {
    pub id: String,
    pub time_ms: u64,
}

/// One invocation, retained across transport retries. An expired invocation
/// or one from another server incarnation must never execute again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    pub id: String,
    pub instance: String,
    pub expires_at_ms: u64,
}

pub const OPERATION_WINDOW_MS: u64 = 15 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcRequest {
    pub envelope: RequestEnvelope,
    pub operation: Option<Operation>,
    pub expected_revision: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybackRevision {
    pub handle: PlaybackHandle,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcReply {
    pub response: Response,
    pub playback: Option<PlaybackRevision>,
}

impl From<Response> for RpcReply {
    fn from(response: Response) -> Self {
        Self {
            response,
            playback: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcResponse {
    pub server: ServerInstance,
    pub reply: RpcReply,
}

#[cfg(test)]
mod tests {
    //! Mandatory revision-2 envelopes and bounded multipart replies.
    use crate::codec::{self, CodecError, MAX_FRAME, MAX_REPLY};
    use crate::rpc::{RpcRequest, RpcResponse, ServerInstance};
    use crate::{Reply, Request, RequestEnvelope, Response};

    #[test]
    fn a_revision_one_envelope_is_not_a_revision_two_request() {
        let old = RequestEnvelope {
            session: None,
            request: Request::Ping,
        };
        assert!(codec::decode::<RpcRequest>(&codec::encode(&old).unwrap()).is_err());
    }

    #[tokio::test]
    async fn a_large_reply_round_trips_across_bounded_chunks() {
        let reply = RpcResponse {
            server: ServerInstance {
                id: "process".into(),
                time_ms: 1,
            },
            reply: Response::Ok(Reply::Manifest {
                text: "x".repeat(MAX_FRAME as usize * 2),
            })
            .into(),
        };
        let mut bytes = Vec::new();
        codec::write_reply(&mut bytes, &reply).await.unwrap();
        let mut offset = 8;
        while offset < bytes.len() {
            let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap());
            assert!(length > 0 && length <= MAX_FRAME);
            offset += 4 + length as usize;
        }
        assert_eq!(offset, bytes.len());
        assert_eq!(
            codec::read_reply::<_, RpcResponse>(&mut bytes.as_slice())
                .await
                .unwrap(),
            reply
        );
    }

    /// Review 2026-10-12: the whole announced length was reserved as soon
    /// as it was read, so a reply announcing 64 MiB and sending 100 bytes
    /// held 64 MiB.
    #[tokio::test]
    async fn a_reply_that_stalls_holds_little_more_than_what_arrived() {
        use std::future::Future;
        use std::task::{Context, Waker};
        use tokio::io::AsyncWriteExt;
        let (mut reader, mut writer) = tokio::io::duplex(1 << 16);
        // The largest reply announced, and 100 bytes of its first frame
        // sent. The writer stays open: the read waits for the rest.
        let head = [
            (MAX_REPLY as u64).to_be_bytes().as_slice(),
            MAX_FRAME.to_be_bytes().as_slice(),
            &[0; 100],
        ]
        .concat();
        writer.write_all(&head).await.unwrap();
        let mut bytes = Vec::new();
        {
            let read = std::pin::pin!(codec::read_reply_bytes(&mut reader, &mut bytes, MAX_REPLY));
            // One poll reads all that was sent, then waits.
            let polled = read.poll(&mut Context::from_waker(Waker::noop()));
            assert!(polled.is_pending());
        }
        assert!(
            bytes.capacity() <= 64 << 10,
            "{} bytes held for 100 received",
            bytes.capacity()
        );
        drop(writer);
    }

    #[tokio::test]
    async fn oversized_and_malformed_replies_are_refused_before_allocation_or_decode() {
        for length in [0, MAX_REPLY as u64 + 1, u64::MAX] {
            assert!(matches!(
                codec::read_reply::<_, Response>(&mut length.to_be_bytes().as_slice()).await,
                Err(CodecError::TooLarge { .. })
            ));
        }
        for (length, chunk) in [(10u64, 0u32), (10, 11), (MAX_REPLY as u64, MAX_FRAME + 1)] {
            let bytes = [
                length.to_be_bytes().as_slice(),
                chunk.to_be_bytes().as_slice(),
            ]
            .concat();
            assert!(matches!(
                codec::read_reply::<_, Response>(&mut bytes.as_slice()).await,
                Err(CodecError::Decode(_))
            ));
        }
        let truncated = [
            10u64.to_be_bytes().as_slice(),
            10u32.to_be_bytes().as_slice(),
            b"short",
        ]
        .concat();
        assert!(matches!(
            codec::read_reply::<_, Response>(&mut truncated.as_slice()).await,
            Err(CodecError::Io(_))
        ));
        let mut bytes = Vec::new();
        assert!(matches!(
            codec::write_reply(&mut bytes, &"x".repeat(MAX_REPLY + 1)).await,
            Err(CodecError::TooLarge { .. })
        ));
        assert!(bytes.is_empty());
    }
}
