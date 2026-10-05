//! Mandatory revision-2 envelopes and bounded multipart replies.
use spoolframe_protocol::codec::{self, CodecError, MAX_FRAME, MAX_REPLY};
use spoolframe_protocol::rpc::{RpcRequest, RpcResponse, ServerInstance};
use spoolframe_protocol::{Reply, Request, RequestEnvelope, Response};

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
