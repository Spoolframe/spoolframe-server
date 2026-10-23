//! Federation (P8): experimental, on a session whose server turned
//! [`crate::Experimental::federation`] on.
//!
//! Pairing the server with another (P8-01): an admin of this server (SB)
//! pastes, or scans, the other's (SA's) `federation` invite, and this
//! server redeems it, exactly as the web UI's Federation page would.
//! Before asking, an app looks the code up on SA
//! ([`crate::Client::look_up_invite`]): the lookup's server is SA, its
//! fingerprint SA's, and its outcome says the kind and who made it, for
//! the admin to check. A non-admin has nothing to pair with: offer to
//! send the invite to the server's admin instead. Then the admin shares
//! libraries of this server with the other ([`Session::share_libraries`]).
//!
//! Browsing a paired server (P8-02): SA's libraries come with SB's own
//! from [`Session::libraries`], each with its server
//! ([`spoolframe_types::Library::server`]: name, server id, online), and
//! their items under `fed:` ids. [`crate::Client::libraries_events`] says
//! when to list them again. Home gains a row per paired server
//! ([`spoolframe_protocol::HomeScreen::servers`]), which the user turns
//! off and on ([`Session::set_server_row`]).
//!
//! Watching (P8-03): SB, the user's home, keeps their progress on SA's
//! items. A paired server's item plays from that server (P8-04), and its
//! progress goes here, by item ([`Session::update_watch_progress`]).

use std::time::Duration;

use spoolframe_protocol::federation::{PairServer, PeerServer, ShareLibraries};
use spoolframe_protocol::{Reply, Request};
use spoolframe_types::{LibraryId, MediaId, Namespace};

use crate::session::unexpected;
use crate::{SdkError, Session};

/// A server told that its libraries changed (P8-02): a paired server came
/// online or went offline, or shared or stopped sharing a library. List
/// them again ([`Session::libraries`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibrariesChanged {
    /// The server's key: the one whose libraries changed.
    pub server: String,
}

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
        self.federated()?;
        let request = Request::PairServer(PairServer {
            link: link.to_owned(),
        });
        match self.call(request).await? {
            Reply::ServerPaired { server } => Ok(server),
            other => Err(unexpected(other)),
        }
    }

    /// Share `libraries` of this session's server with the paired server
    /// `server_id` (P8-03), as an app offers once [`Session::pair_server`]
    /// paired them: the libraries shared. Every user of the paired server
    /// may browse and watch them; a library shared already stays shared.
    /// An admin's only, recently confirmed, as pairing is.
    /// [`SdkError::NotFound`] for a server not paired;
    /// [`SdkError::InvalidInput`] for a library the server does not serve,
    /// and then none is shared. [`SdkError::Unsupported`] without
    /// [`crate::Experimental::federation`], and nothing is sent.
    pub async fn share_libraries(
        &self,
        server_id: &str,
        libraries: &[LibraryId],
    ) -> Result<Vec<LibraryId>, SdkError> {
        self.federated()?;
        let request = Request::ShareLibraries(ShareLibraries {
            server_id: server_id.to_owned(),
            libraries: libraries.to_vec(),
        });
        match self.call(request).await? {
            Reply::LibrariesShared { libraries } => Ok(libraries),
            other => Err(unexpected(other)),
        }
    }

    /// Show or hide paired server `server_id`'s own row on the user's Home
    /// (P8-02): its items stay in Continue watching either way.
    /// [`SdkError::NotFound`] for a server not paired;
    /// [`SdkError::Unsupported`] without
    /// [`crate::Experimental::federation`].
    pub async fn set_server_row(&self, server_id: &str, shown: bool) -> Result<(), SdkError> {
        self.federated()?;
        let request = Request::SetServerRow {
            server_id: server_id.to_owned(),
            shown,
        };
        self.done(request).await
    }

    /// Tell this session's server, the user's home, where the user got to
    /// in `id`, a paired server's item that plays from that server rather
    /// than from a playback here (P8-03). Report as a playback's progress
    /// is reported: now and then while playing, and on pause or stop. The
    /// paired server keeps nothing of it. [`SdkError::InvalidInput`] for an
    /// id that is not a paired server's; [`SdkError::NotFound`] once it is
    /// no longer available. [`SdkError::Unsupported`] without
    /// [`crate::Experimental::federation`]. Neither is sent.
    pub async fn update_watch_progress(
        &self,
        id: &MediaId,
        position: Duration,
    ) -> Result<(), SdkError> {
        self.federated()?;
        if id.namespace() != Namespace::Federated {
            return Err(SdkError::InvalidInput(format!(
                "{id} is not a paired server's item"
            )));
        }
        let request = Request::UpdateWatchProgress {
            id: id.clone(),
            position_ms: u64::try_from(position.as_millis()).unwrap_or(u64::MAX),
        };
        self.done(request).await
    }

    /// `Ok` when the server turned federation on for this session.
    fn federated(&self) -> Result<(), SdkError> {
        if self.experimental().federation {
            Ok(())
        } else {
            Err(SdkError::Unsupported {
                feature: spoolframe_protocol::experimental::FEDERATION.to_owned(),
            })
        }
    }
}
