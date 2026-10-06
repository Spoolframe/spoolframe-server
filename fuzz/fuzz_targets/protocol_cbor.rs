//! CBOR payloads from a peer, decoded as each message type, and what
//! decodes encoded again: a value the codec accepts must not make it
//! panic on the way back out.
#![no_main]

use libfuzzer_sys::fuzz_target;
use spoolframe_protocol::RequestEnvelope;
use spoolframe_protocol::codec;
use spoolframe_protocol::resource::{ResourceRequest, ResourceResponse};
use spoolframe_protocol::rpc::{RpcRequest, RpcResponse};

fuzz_target!(|data: &[u8]| {
    let _ = codec::request_type(data);
    let _ = codec::rpc_request_type(data);
    if let Ok(request) = codec::decode::<RpcRequest>(data) {
        let again = codec::encode(&request).expect("a decoded request encodes");
        let back: RpcRequest = codec::decode(&again).expect("an encoded request decodes");
        assert_eq!(back, request);
    }
    if let Ok(envelope) = codec::decode::<RequestEnvelope>(data) {
        let _ = codec::encode(&envelope);
    }
    if let Ok(response) = codec::decode::<RpcResponse>(data) {
        let _ = codec::encode_limited(&response, codec::MAX_REPLY);
    }
    if let Ok(request) = codec::decode::<ResourceRequest>(data) {
        let again = codec::encode(&request).expect("a decoded request encodes");
        let back: ResourceRequest = codec::decode(&again).expect("an encoded request decodes");
        assert_eq!(back, request);
    }
    if let Ok(response) = codec::decode::<ResourceResponse>(data) {
        let _ = codec::encode(&response);
    }
});
