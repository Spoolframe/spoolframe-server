//! Regular features (PX-16): named sets of messages both sides use only
//! when both list them. This is the slice playlists (PX-40) need; the rest
//! of PX-16 (the update advisory, a test server's mask) comes with it.
//!
//! A client lists every regular feature it is built with in
//! [`Hello::features`]. A server lists those it has: its answer to `Hello`
//! ([`ServerInfo::features`]) only the pre-auth ones, and every reply that
//! grants a session (`LoggedIn`), and `WhoAmI`, all of them. Each side
//! then uses only what both list: the server sends no reply field or event
//! of a feature the client did not list, and a client sends no request of
//! one the server did not. A name either side does not know is ignored,
//! never an error, and no feature is required.
//!
//! Unlike [`crate::experimental`] features, regular ones are always on in
//! a build that has them: neither an operator nor a user turns them off.
//!
//! [`Hello::features`]: crate::Hello::features
//! [`ServerInfo::features`]: crate::ServerInfo::features

/// Playlists (PX-40): `ListPlaylists` and the rest of [`crate::playlists`]'
/// requests, `Item`'s `playlists`, and the `PlaylistChanged`,
/// `PlaylistGone` and `PinsChanged` events.
pub const PLAYLISTS: &str = "spoolframe:playlists";

/// Liked and All (PX-43), on top of [`PLAYLISTS`]: the `Like` and
/// `Unlike` requests; `Item`'s `liked` and `liked_children`, and the
/// `liked` of a playlist's entries and of the entry playing; a playlist
/// summary's and reference's `builtin`; and the built-in Liked and All
/// playlists themselves. A session whose client did not list it is sent
/// none of these, and is never shown Liked or All, as it would not know
/// them for built-ins and would offer to rename or delete them. The first
/// feature added instead of a protocol version: an app built before it
/// keeps working against a server that has it.
pub const LIKED: &str = "spoolframe:liked";

/// When each item was last viewed: `PlaybackState::last_viewed_at`
/// (Unix seconds) on Home's `continue_watching` and `next_up` items and
/// on `Item`'s item and children, from the user's last progress report
/// on it from any device (or its last mark as watched or unwatched). What
/// a client needs to show only what was watched lately, such as the Apple
/// TV's Top Shelf, which a guess from the device's own playing would get
/// wrong. A session whose client did not list it is never sent the field.
pub const LAST_VIEWED: &str = "spoolframe:last-viewed";

/// Every regular feature this build knows.
pub const ALL: &[&str] = &[PLAYLISTS, LIKED, LAST_VIEWED];

/// The ones that change what a client can do before it has a session:
/// what the answer to `Hello` lists. None yet.
pub const PRE_AUTH: &[&str] = &[];

/// The most feature names a server keeps of a `Hello`; the rest are
/// dropped, as each name is cut to `MAX_HELLO_TEXT` characters.
pub const MAX_LISTED: usize = 32;

/// Whether `listed`, a peer's list, has `feature`.
pub fn has(listed: &[String], feature: &str) -> bool {
    listed.iter().any(|name| name == feature)
}

/// `ours` as names, for a message.
pub fn names(ours: &[&str]) -> Vec<String> {
    ours.iter().map(|name| (*name).to_owned()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_feature_is_had_only_by_its_exact_name() {
        let listed = vec!["spoolframe:playlists".to_owned(), "other:x".to_owned()];
        assert!(has(&listed, PLAYLISTS));
        assert!(!has(&["spoolframe:playlist".to_owned()], PLAYLISTS));
        assert!(!has(&[], PLAYLISTS));
    }

    #[test]
    fn playlists_are_not_pre_auth() {
        assert!(ALL.contains(&PLAYLISTS));
        assert!(!PRE_AUTH.contains(&PLAYLISTS));
    }

    #[test]
    fn last_viewed_is_a_feature_and_not_pre_auth() {
        assert!(ALL.contains(&LAST_VIEWED));
        assert!(!PRE_AUTH.contains(&LAST_VIEWED));
    }

    #[test]
    fn liked_is_a_feature_and_not_pre_auth() {
        assert!(ALL.contains(&LIKED));
        assert!(!PRE_AUTH.contains(&LIKED));
    }
}
