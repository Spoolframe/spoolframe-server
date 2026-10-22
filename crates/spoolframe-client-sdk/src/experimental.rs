//! Experimental features (PX-49): what an app asks a server for in its
//! `Hello` ([`crate::ClientOptions::experimental`]), and what the server
//! turned on for the connection ([`crate::Session::experimental`]).
//!
//! An experimental feature may change or go away in any release, the
//! server's or the SDK's, without the protocol's version moving. A server
//! that does not know one, or has it off, does not turn it on, and its
//! calls answer as off without asking it.

pub use spoolframe_protocol::experimental::HttpPlaybackRefusal;
use spoolframe_protocol::experimental::{FEDERATION, HTTP_PLAYBACK};

/// The experimental features an app asks for, or a server turned on. None
/// by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Experimental {
    /// A playback's HLS, or its file, over plain HTTP from the server's
    /// own port, for a player with no Iroh: an AirPlay receiver
    /// ([`crate::Session::http_playback_url`]).
    pub http_playback: bool,
    /// Server-to-server federation (P8-01): pairing the server with
    /// another by that one's invite ([`crate::Session::pair_server`]),
    /// and making `federation` invites
    /// ([`crate::InviteKind::Federation`]). An admin's, on a server with
    /// `experimental.federation` on.
    pub federation: bool,
}

impl Experimental {
    /// The names a `Hello` carries.
    pub(crate) fn names(self) -> Vec<String> {
        let mut names = Vec::new();
        if self.http_playback {
            names.push(HTTP_PLAYBACK.to_owned());
        }
        if self.federation {
            names.push(FEDERATION.to_owned());
        }
        names
    }

    /// What `names`, from a server's answer, turned on. A name this build
    /// does not know is ignored.
    pub(crate) fn of(names: &[String]) -> Self {
        Self {
            http_playback: names.iter().any(|n| n == HTTP_PLAYBACK),
            federation: names.iter().any(|n| n == FEDERATION),
        }
    }
}

/// Where a player with no Iroh fetches a playback, or why the server gives
/// no such place ([`crate::Session::http_playback_url`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpPlayback {
    /// `http://<address>:<port>/<token>/master.m3u8` for HLS, `.../file`
    /// for direct play (with byte ranges). It works until the playback
    /// ends or nothing fetches from it for a while; a playback the SDK
    /// started again (a resume) needs a new one.
    Url(String),
    /// Why not. [`HttpPlaybackRefusal::Off`] also when the feature was
    /// not turned on for this connection: the SDK does not ask then.
    Refused(HttpPlaybackRefusal),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_is_asked_for_by_default_and_unknown_names_turn_nothing_on() {
        assert!(Experimental::default().names().is_empty());
        let asked = Experimental {
            http_playback: true,
            federation: true,
        };
        assert_eq!(
            asked.names(),
            vec![HTTP_PLAYBACK.to_owned(), FEDERATION.to_owned()]
        );
        assert_eq!(Experimental::of(&asked.names()), asked);
        assert_eq!(
            Experimental::of(&["teleport".to_owned()]),
            Experimental::default()
        );
    }
}
