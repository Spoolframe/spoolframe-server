//! The session user's Plex account link, as clients may read it.
//! Linking and unlinking are the web UI's (`<web_url>/profile`): a client
//! only shows the state, and never sees the account's token.

use serde::{Deserialize, Serialize};

/// A linked Plex account: the user's watch state syncs with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlexLink {
    /// The Plex account's username and email, as plex.tv said when it was
    /// linked; both `None` for a link the server's config file makes.
    pub username: Option<String>,
    pub email: Option<String>,
    /// Linked by the server's config file, as the Plex servers' owner:
    /// only that file can unlink it, not the web UI.
    pub by_config: bool,
    /// When it was linked, Unix seconds; `None` for the config's.
    pub linked_at: Option<i64>,
    /// The user's last sync run since the server started, if any.
    pub last_sync: Option<PlexSyncSummary>,
    /// A sync run of theirs is going on now.
    pub syncing: bool,
}

/// What one sync run did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlexSyncSummary {
    /// When it ended, Unix seconds.
    pub at: i64,
    /// The libraries synced.
    pub libraries: u32,
    /// Items whose state was taken from Plex, sent to Plex, left as they
    /// were, and that failed.
    pub pulled: u32,
    pub pushed: u32,
    pub skipped: u32,
    pub failed: u32,
}
