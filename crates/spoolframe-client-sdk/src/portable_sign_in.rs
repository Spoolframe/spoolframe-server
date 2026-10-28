//! Signing in to a server as a portable identity, with this device's
//! credential for it; and the servers a pairing hands a device,
//! so a TV paired with a guest signs in where the guest's phone does.
//!
//! An app's flow:
//!
//! 1. On the phone, which holds the root: [`crate::Portable::create`],
//!    then [`Client::redeem_invite_portable`] with a guest invite's link.
//!    The phone signs itself a credential, proves it, and is
//!    signed in; the server is saved.
//! 2. On the TV, guests allowed: [`Client::start_pairing`]; the phone
//!    scans it ([`Client::scan_pairing`]) and authorizes. The phone sends
//!    the servers it signs in to as the identity; the TV saves them, and
//!    [`crate::Paired::servers`] lists them.
//! 3. On the TV: [`Client::sign_in_portable`] on each. Once a session
//!    ends, [`crate::SavedProfile::portable`] signs it in again, with no
//!    password.

use iroh::EndpointId;
use spoolframe_protocol::pairing::{MAX_ADDRS, MAX_PAIRED_SERVERS, MAX_RELAYS, PairedServer};
use spoolframe_protocol::{PortableSignIn, Reply, Request, Response, SessionId};

use crate::SdkError;
use crate::client::{Client, parse_id, same_server};
use crate::portable::PortableUserId;
use crate::servers::{self, SavedServer};
use crate::session::{self, Session, SignInMethod};

impl Client {
    /// Sign in to server `endpoint_id` as portable identity `identity`,
    /// with this device's credential for it (from a pairing, or the
    /// root's phone's own), and keep the session. The server need only
    /// be pinned, not verified: one a pairing handed is pinned unverified
    /// ([`crate::Paired::servers`]), and the credential proves the
    /// identity whichever server answers to the pinned key. Local-first,
    /// as the server decides: the account the identity is tied to there, else
    /// the identity itself where it was granted access (a guest invite).
    ///
    /// [`SdkError::InvalidCredentials`] for every refusal the server
    /// gives: no grant there, a credential it does not take, a revoked
    /// key. Before anything is sent: [`SdkError::NotPinned`];
    /// [`SdkError::UnknownIdentity`] with no credential here;
    /// [`SdkError::Credential`] for one past its time (pair again);
    /// [`SdkError::DeviceKeyRevoked`].
    pub async fn sign_in_portable(
        &self,
        endpoint_id: &str,
        identity: PortableUserId,
    ) -> Result<Session, SdkError> {
        let server = parse_id(endpoint_id)?;
        self.require_pinned(server).await?;
        // The root's phone keeps its own credential fresh; any other
        // device proves with what it was given, and `prove` says why not.
        match self.portable().own_credential(identity).await {
            Ok(_) | Err(SdkError::UnknownIdentity(_) | SdkError::Credential(_)) => {}
            Err(e) => return Err(e),
        }
        let sign_in = self.portable_sign_in(server, identity, None).await?;
        let token = logged_in(
            self.call(server, None, Request::FinishPortableSignIn(sign_in))
                .await?,
        )?;
        self.keep_session_as(
            server,
            token,
            Some(SignInMethod::Portable),
            false,
            Some(identity),
        )
        .await
    }

    /// What proves `identity` to `server`: a nonce it begins now, this
    /// device key's signature over it, the credential, and the newest
    /// state this device has, which the server takes in first.
    pub(crate) async fn portable_sign_in(
        &self,
        server: EndpointId,
        identity: PortableUserId,
        display_name: Option<&str>,
    ) -> Result<PortableSignIn, SdkError> {
        let nonce = match self
            .call(server, None, Request::BeginPortableSignIn)
            .await?
        {
            Response::Ok(Reply::PortableSignInBegun { nonce, .. }) => nonce,
            Response::Ok(other) => {
                return Err(SdkError::Protocol(format!(
                    "BeginPortableSignIn answered {other:?}"
                )));
            }
            Response::Err(e) => return Err(SdkError::from_server(e)),
        };
        let portable = self.portable();
        // The id this device dialled, never one a server named.
        let (credential, proof) = portable.prove(identity, server.as_bytes(), &nonce).await?;
        // Every head: the newest, and a rival it does not cover (at most
        // MAX_SIGN_IN_STATES), so a server learns both branches' changes.
        let states = portable.identity_states(identity).await?;
        Ok(PortableSignIn {
            credential,
            proof,
            states,
            display_name: display_name.map(str::to_owned),
        })
    }

    /// The saved servers this device signs in to as `identity`: each
    /// verified here (its fingerprint confirmed, or pinned from an invite
    /// link), with a session kept for it, live or ended. What a phone
    /// hands a device it pairs, at most [`MAX_PAIRED_SERVERS`]. A saved
    /// server with no server id pinned is left out, as the device could
    /// not pin it whole.
    pub(crate) async fn paired_servers(
        &self,
        identity: PortableUserId,
    ) -> Result<Vec<PairedServer>, SdkError> {
        let storage = self.inner.storage.as_ref();
        let mut found = Vec::new();
        for server in servers::load(storage).await? {
            if !server.verified {
                continue;
            }
            let (Some(server_id), Ok(id)) = (&server.server_id, parse_id(&server.endpoint_id))
            else {
                continue;
            };
            let signs_in = session::load(storage, &id)
                .await?
                .iter()
                .any(|s| s.portable == Some(identity));
            if !signs_in {
                continue;
            }
            found.push(PairedServer {
                endpoint_id: server.endpoint_id.clone(),
                server_id: server_id.clone(),
                name: server.name.clone(),
                addrs: server.addrs().into_iter().take(MAX_ADDRS).collect(),
                relays: server.relays.iter().take(MAX_RELAYS).cloned().collect(),
            });
            if found.len() == MAX_PAIRED_SERVERS {
                break;
            }
        }
        Ok(found)
    }

    /// Save the servers a pairing's phone handed this device: pinned as
    /// it names them, key and server id, but not verified: the guest's
    /// phone vouches for them, not this device's owner. A server saved
    /// here already is left exactly as it is, its pin, its verification
    /// and its addresses: a guest adds nothing to a pin the owner made.
    /// One whose server id is a saved server's under another key is left
    /// out: that is [`SdkError::IdentityChanged`]'s to raise, on a
    /// connect the user makes. The ones saved, or saved already.
    pub(crate) async fn save_paired_servers(
        &self,
        handed: Vec<PairedServer>,
    ) -> Result<Vec<PairedServer>, SdkError> {
        let storage = self.inner.storage.as_ref();
        let _saving = self.inner.saving.lock().await;
        let mut saved = servers::load(storage).await?;
        let now = servers::now();
        let mut kept = Vec::new();
        for server in handed
            .into_iter()
            .take(MAX_PAIRED_SERVERS)
            .filter_map(PairedServer::within_bounds)
        {
            let Ok(id) = parse_id(&server.endpoint_id) else {
                continue;
            };
            if !saved.iter().any(|s| s.endpoint_id == server.endpoint_id) {
                if same_server(&saved, id, &server.server_id).is_some() {
                    tracing::warn!(
                        id = %server.endpoint_id,
                        "a pairing handed a server whose id a saved server has under \
                         another key: left out"
                    );
                    continue;
                }
                let mut pinned = SavedServer::pinned_now(
                    server.name.clone(),
                    server.endpoint_id.clone(),
                    server.server_id.clone(),
                    Vec::new(),
                    false,
                );
                pinned.last_connected_at = None;
                pinned.hinted(server.addrs.iter().copied(), now);
                add_relays(&mut pinned, &server.relays);
                tracing::info!(id = %server.endpoint_id, name = pinned.name, "pinned a server a pairing handed");
                saved.push(pinned);
            }
            kept.push(server);
        }
        servers::save(storage, &saved).await?;
        drop(_saving);
        self.inner.discovery.changed();
        Ok(kept)
    }
}

fn add_relays(server: &mut SavedServer, relays: &[String]) {
    for relay in relays {
        if !server.relays.contains(relay) {
            server.relays.push(relay.clone());
        }
    }
}

/// The session a sign-in answered with.
pub(crate) fn logged_in(response: Response) -> Result<SessionId, SdkError> {
    match response {
        Response::Ok(Reply::LoggedIn { session, .. }) => Ok(session),
        Response::Ok(other) => Err(SdkError::Protocol(format!(
            "a portable sign-in answered {other:?}"
        ))),
        Response::Err(e) => Err(SdkError::from_server(e)),
    }
}
