//! Development revision 3 of the `/1` RPC stream. The wrapper is mandatory;
//! old binaries fail decoding instead of silently losing the new guarantees.

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
