//! Pairing the server with another (P8-01): experimental, on a session
//! whose server turned [`crate::Experimental::federation`] on. An admin of
//! this server (SB) pastes, or scans, the other's (SA's) `federation`
//! invite, and this server redeems it, exactly as the web UI's
//! Federation page would.
//!
//! Before asking, an app looks the code up on SA
//! ([`crate::Client::look_up_invite`]): the lookup's server is SA, its
//! fingerprint SA's, and its outcome says the kind and who made it, for
//! the admin to check. A non-admin has nothing to pair with: offer to
//! send the invite to the server's admin instead.

use spoolframe_protocol::federation::{PairServer, PeerServer};
use spoolframe_protocol::{Reply, Request};

use crate::{SdkError, Session};

impl Session {
    /// Pair this session's server with the one whose `federation` invite
    /// `link` is: the server paired, its key's fingerprint to show. An
    /// admin's only ([`SdkError::AdminRequired`]), after a recent
    /// confirmation ([`SdkError::RecentSignInRequired`]: confirm, then ask
    /// again). [`SdkError::Unsupported`] without
    /// [`crate::Experimental::federation`], and nothing is sent.
    ///
    /// The other server's refusal of the code is the invite's own error
    /// ([`SdkError::InviteInvalid`], [`SdkError::InviteExpired`],
    /// [`SdkError::InviteExhausted`], [`SdkError::WrongInviteKind`],
    /// [`SdkError::RateLimited`]); any other is
    /// [`SdkError::PairingRefused`]. [`SdkError::Unavailable`] when this
    /// server cannot reach the other; [`SdkError::InvalidInput`] for a
    /// link that is not one, has no code, or is this server's own.
    pub async fn pair_server(&self, link: &str) -> Result<PeerServer, SdkError> {
        if !self.experimental().federation {
            return Err(SdkError::Unsupported {
                feature: spoolframe_protocol::experimental::FEDERATION.to_owned(),
            });
        }
        let request = Request::PairServer(PairServer {
            link: link.to_owned(),
        });
        match self.call(request).await? {
            Reply::ServerPaired { server } => Ok(server),
            other => Err(SdkError::Protocol(format!("unexpected reply {other:?}"))),
        }
    }
}
