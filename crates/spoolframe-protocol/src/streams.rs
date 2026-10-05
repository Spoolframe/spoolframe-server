//! What each QUIC stream on a connection is for (ARCHITECTURE §33). The
//! first byte on a stream says.

/// The first byte of every bidirectional stream the client opens.
pub mod bi {
    /// One request frame, then a bounded multipart RPC response.
    pub const RPC: u8 = 1;
    /// A `ResourceRequest`, `ResourceResponse`, and bounded raw body.
    pub const RESOURCE: u8 = 2;
}

/// The first byte of every unidirectional stream the server opens.
pub mod uni {
    /// Event frames for the life of the connection, opened after `Hello`.
    pub const EVENTS: u8 = 1;
}

/// QUIC stream reset codes.
pub mod reset {
    /// The first byte named a stream kind this peer does not serve.
    pub const UNKNOWN_STREAM_KIND: u32 = 1;
}
