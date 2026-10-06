//! The frame reader on a stream from a peer: a length, then that much
//! CBOR. What the server reads for every RPC and resource request, and
//! the client for every reply (`spoolframe-protocol` codec.rs).
#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use spoolframe_protocol::codec::{self, MAX_FRAME};
use spoolframe_protocol::resource::{ResourceRequest, ResourceResponse};
use spoolframe_protocol::rpc::{RpcRequest, RpcResponse};
use tokio::runtime::Runtime;

fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime")
    })
}

fuzz_target!(|data: &[u8]| {
    runtime().block_on(async {
        // The server: one request frame per stream, RPC or resource.
        let mut stream = data;
        if let Ok(bytes) = codec::read_frame_bytes_within(&mut stream, MAX_FRAME).await {
            let _ = codec::rpc_request_type(&bytes);
            let _ = codec::decode::<RpcRequest>(&bytes);
            let _ = codec::decode::<ResourceRequest>(&bytes);
        }
        // The client: a multipart reply, and a resource answer.
        let _ = codec::read_reply::<_, RpcResponse>(&mut &data[..]).await;
        let _ = codec::read_frame::<_, ResourceResponse>(&mut &data[..]).await;
        let _ = codec::read_frame_within::<_, RpcRequest>(&mut &data[..], 4096).await;
    });
});
