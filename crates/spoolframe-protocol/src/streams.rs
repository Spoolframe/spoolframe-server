//! What each QUIC stream on a connection is for (ARCHITECTURE §33). The
//! first byte on a stream says.

/// The first byte of every bidirectional stream the client opens.
pub mod bi {
    /// One request frame from the client, one response frame back.
    pub const RPC: u8 = 1;
    /// One image: an `ArtworkRequest` frame, then an `ArtworkResponse`
    /// frame and the image's bytes (P1-05).
    pub const ARTWORK: u8 = 2;
    /// One range read: a `MediaRead` frame, then a `MediaReadResponse`
    /// frame and the bytes (P1-08).
    pub const MEDIA: u8 = 3;
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
