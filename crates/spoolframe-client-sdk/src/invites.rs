//! Invites (P6-01): finding, saving and joining a server from its link.
//!
//! The link names the server's key, so the key is pinned as it is, with
//! nothing trusted on first use; the server id is learned at `Hello` and
//! pinned with it (PX-14). The link's addresses are hints: they join the
//! addresses this device knows as told ones (PX-12), and Iroh still
//! proves the key whatever path it takes. A link naming a key this device
//! does not have saved is another server, whatever its name (P1-13); one
//! whose server id is a saved server's under another key is that server
//! with a new key, [`SdkError::IdentityChanged`], as for any connect.
//!
//! An app's flow, as the design draws it:
//!
//! 1. [`Client::read_invite`] when the link is pasted or opened: nothing
//!    is sent. [`SdkError::NotAnInvite`] goes under the paste field.
//! 2. [`Client::connect_invite`]: who answers, for "Is this the server you
//!    were invited to?" (its fingerprint, and the server id its passkeys
//!    are for). Nothing is saved.
//! 3. With a code, the join form, then [`Client::redeem_invite`]: the
//!    account, the server saved, and a session. Without one, or when a
//!    redemption is refused and the user picks "Save server only",
//!    [`Client::save_invite_server`].
//!
//! Until then the device is no one's, and the server's own relay (P6-04)
//! carries only its owner's and friends'. A link with a code makes the
//! device its holder (P6-08): from the first connection to the last step
//! it reaches the link's relays with the code's token
//! ([`spoolframe_protocol::relay_access::invite_token`]), which says to
//! the relay whom the invite is for. Once redeemed, or the server saved
//! without it, the token is dropped: the session says who the device is.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use iroh::{EndpointId, RelayConfig, RelayUrl};
use spoolframe_protocol::invite::{Invite, InviteLinkKind, MAX_HINTS, parse_link};
use spoolframe_protocol::relay_access::invite_token;
use spoolframe_protocol::{
    RedeemInvite, RedeemPortableInvite, Reply, Request, Response, fingerprint,
};
use spoolframe_types::Role;

use crate::client::{Client, newest_first, parse_id, same_server};
use crate::connection::ServerConnection;
use crate::portable::PortableUserId;
use crate::servers::{self, SavedServer};
use crate::session::{Session, SignInMethod};
use crate::{SdkError, ServerEntry};

/// What an invite link says, read without a word to the server: for the
/// "Add a server" summary and the join form ("Joins as a consumer ·
/// expires Oct 14").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteInfo {
    /// The server's key, hex: what the link pins.
    pub endpoint_id: String,
    /// The key's fingerprint (P1-21), for "Is this the server you were
    /// invited to?".
    pub fingerprint: String,
    /// What the link calls the server; `None` if it does not say.
    pub name: Option<String>,
    /// Where the link says the server may be reached.
    pub addrs: Vec<SocketAddr>,
    /// The relays it may be reached through.
    pub relays: Vec<String>,
    /// The link has a code: redeeming it makes an account ("Includes an
    /// account"). Without one it only finds and saves the server.
    pub has_code: bool,
    /// When the code stops working, as the link says; a hint, the server
    /// decides.
    pub expires_at: Option<SystemTime>,
    /// The role the account gets, as the link says; a hint, the server
    /// decides.
    pub role: Option<Role>,
    /// What the code does, as the link says (P7-05): makes an account
    /// ([`Client::redeem_invite`]), or grants a portable identity access
    /// ([`Client::redeem_invite_portable`]). A hint for which form to
    /// show; the server answers the other with
    /// [`SdkError::WrongInviteKind`].
    pub kind: InviteLinkKind,
    /// This key is a saved server already: the link only adds its hints.
    pub saved: bool,
}

/// `link` read, and the key it names.
fn parsed(link: &str) -> Result<(Invite, EndpointId), SdkError> {
    let invite = parse_link(link).ok_or(SdkError::NotAnInvite)?;
    let id = parse_id(&invite.server).map_err(|_| SdkError::NotAnInvite)?;
    Ok((invite, id))
}

impl Client {
    /// What invite `link` says; nothing is sent or saved.
    /// [`SdkError::NotAnInvite`] for anything that is not one.
    pub async fn read_invite(&self, link: &str) -> Result<InviteInfo, SdkError> {
        let (invite, _) = parsed(link)?;
        let saved = servers::load(self.inner.storage.as_ref())
            .await?
            .iter()
            .any(|s| s.endpoint_id == invite.server);
        Ok(InviteInfo {
            fingerprint: fingerprint::of_hex(&invite.server).unwrap_or_default(),
            endpoint_id: invite.server,
            name: invite.name,
            addrs: invite.addrs,
            relays: invite.relays,
            has_code: invite.code.is_some(),
            expires_at: invite
                .expires_at
                .map(|secs| UNIX_EPOCH + Duration::from_secs(secs)),
            role: invite.role,
            kind: invite.kind,
            saved,
        })
    }

    /// Connect to the server invite `link` names, at its hints, and say who
    /// answered: its name, fingerprint and server id. Nothing is saved,
    /// and only `Hello` is sent. [`SdkError::IdentityChanged`] when it
    /// answers with a saved server's server id under another key.
    pub async fn connect_invite(&self, link: &str) -> Result<ServerEntry, SdkError> {
        let (invite, id) = parsed(link)?;
        let conn = self.reach(&invite, id).await?;
        let saved = servers::load(self.inner.storage.as_ref()).await?;
        let pinned = saved.iter().find(|s| s.endpoint_id == invite.server);
        if pinned.is_none() {
            refuse_another_key(&saved, id, &conn)?;
        }
        Ok(self.entry(id, &conn, pinned))
    }

    /// Save the server invite `link` names: "Save server only", and what a
    /// link without a code does. Its key is pinned as the link names it,
    /// with the server id it answers with, and counts as verified: the
    /// link came from the server's admin. A server saved already keeps its
    /// pin, and gains the link's hints.
    pub async fn save_invite_server(&self, link: &str) -> Result<ServerEntry, SdkError> {
        let (invite, id) = parsed(link)?;
        let saved = self.save_invited(&invite, id).await;
        self.drop_token(&invite).await;
        saved
    }

    async fn save_invited(&self, invite: &Invite, id: EndpointId) -> Result<ServerEntry, SdkError> {
        let conn = self.reach(invite, id).await?;
        let storage = self.inner.storage.as_ref();
        let _saving = self.inner.saving.lock().await;
        let mut saved = servers::load(storage).await?;
        let now = servers::now();
        let server = match saved.iter_mut().find(|s| s.endpoint_id == invite.server) {
            Some(server) => {
                if !server.verified {
                    server.set_verified(true);
                }
                hint(server, invite, now);
                server.clone()
            }
            None => {
                refuse_another_key(&saved, id, &conn)?;
                let mut server = SavedServer::pinned_now(
                    conn.info.name.clone(),
                    invite.server.clone(),
                    conn.info.server_id.clone(),
                    conn.reached(),
                    true,
                );
                hint(&mut server, invite, now);
                tracing::info!(id = %invite.server, name = server.name, "pinned a server from an invite");
                saved.push(server.clone());
                server
            }
        };
        servers::save(storage, &saved).await?;
        self.inner.discovery.changed();
        Ok(self.entry(id, &conn, Some(&server)))
    }

    /// Make an account with invite `link`'s code, as `username` (shown as
    /// `display_name`, or the username) with `password`, and keep its
    /// session. The server is saved once the account is made, as
    /// [`Client::save_invite_server`] saves it; a refused redemption saves
    /// nothing.
    ///
    /// Refusals: [`SdkError::InviteInvalid`], [`SdkError::InviteExpired`]
    /// and [`SdkError::InviteExhausted`] (offer "Save server only");
    /// [`SdkError::UsernameTaken`] and [`SdkError::InvalidInput`] (back to
    /// the form; the invite is still unused); [`SdkError::RateLimited`].
    /// A link with no code is [`SdkError::InvalidInput`]: sign in instead.
    pub async fn redeem_invite(
        &self,
        link: &str,
        username: &str,
        display_name: Option<&str>,
        password: &str,
    ) -> Result<Session, SdkError> {
        let (invite, _) = parsed(link)?;
        let redeemed = self.redeem(link, username, display_name, password).await;
        // Refused, the form is sent again, and reaches the relay with the
        // token again.
        self.drop_token(&invite).await;
        redeemed
    }

    async fn redeem(
        &self,
        link: &str,
        username: &str,
        display_name: Option<&str>,
        password: &str,
    ) -> Result<Session, SdkError> {
        let (invite, id) = parsed(link)?;
        let Some(code) = invite.code.clone() else {
            return Err(SdkError::InvalidInput(
                "this invite has no code: it only finds the server; sign in instead".into(),
            ));
        };
        let conn = self.reach(&invite, id).await?;
        // Before an account is made: a key claiming a saved server's id
        // is refused now, not after.
        let saved = servers::load(self.inner.storage.as_ref()).await?;
        if !saved.iter().any(|s| s.endpoint_id == invite.server) {
            refuse_another_key(&saved, id, &conn)?;
        }
        let request = Request::RedeemInvite(RedeemInvite {
            code,
            username: username.to_owned(),
            display_name: display_name.map(str::to_owned),
            password: password.to_owned(),
        });
        let (token, remembered) = match self.call(id, None, request).await? {
            Response::Ok(Reply::LoggedIn {
                session,
                remembered,
            }) => (session, remembered),
            Response::Ok(other) => {
                return Err(SdkError::Protocol(format!(
                    "RedeemInvite answered {other:?}"
                )));
            }
            Response::Err(e) => return Err(SdkError::from_server(e)),
        };
        self.save_invite_server(link).await?;
        self.keep_session(id, token, Some(SignInMethod::Password), remembered)
            .await
    }

    /// Join the server guest invite `link` names as portable identity
    /// `identity` (P7-05), on the phone that holds its root: the phone
    /// signs itself a credential if it has none fresh
    /// ([`crate::Portable::own_credential`]), proves it, and redeems the
    /// code. The server grants the identity access (no account is made),
    /// calls it `display_name`, and signs it in; the server is saved as
    /// [`Client::save_invite_server`] saves it, and the session kept, to
    /// sign in again with [`Client::sign_in_portable`]. A refused
    /// redemption saves nothing.
    ///
    /// Refusals: [`SdkError::InviteInvalid`], [`SdkError::InviteExpired`],
    /// [`SdkError::InviteExhausted`]; [`SdkError::WrongInviteKind`] for an
    /// account's invite ([`Client::redeem_invite`] it); a display name
    /// that is not acceptable [`SdkError::InvalidInput`];
    /// [`SdkError::InvalidCredentials`]; [`SdkError::RateLimited`]. An
    /// identity whose root is not here and that has no credential is
    /// [`SdkError::UnknownIdentity`].
    pub async fn redeem_invite_portable(
        &self,
        link: &str,
        identity: PortableUserId,
        display_name: Option<&str>,
    ) -> Result<Session, SdkError> {
        let (invite, _) = parsed(link)?;
        let redeemed = self.redeem_portable(link, identity, display_name).await;
        self.drop_token(&invite).await;
        redeemed
    }

    async fn redeem_portable(
        &self,
        link: &str,
        identity: PortableUserId,
        display_name: Option<&str>,
    ) -> Result<Session, SdkError> {
        let (invite, id) = parsed(link)?;
        let Some(code) = invite.code.clone() else {
            return Err(SdkError::InvalidInput(
                "this invite has no code: it only finds the server".into(),
            ));
        };
        let portable = self.portable();
        match portable.own_credential(identity).await {
            Ok(_) | Err(SdkError::UnknownIdentity(_) | SdkError::Credential(_)) => {}
            Err(e) => return Err(e),
        }
        let conn = self.reach(&invite, id).await?;
        let saved = servers::load(self.inner.storage.as_ref()).await?;
        if !saved.iter().any(|s| s.endpoint_id == invite.server) {
            refuse_another_key(&saved, id, &conn)?;
        }
        let sign_in = self.portable_sign_in(id, identity, display_name).await?;
        let request = Request::RedeemPortableInvite(RedeemPortableInvite { code, sign_in });
        let token = crate::portable_sign_in::logged_in(self.call(id, None, request).await?)?;
        self.save_invite_server(link).await?;
        self.keep_session_as(
            id,
            token,
            Some(SignInMethod::Portable),
            false,
            Some(identity),
        )
        .await
    }

    /// The connection to server `id`, dialled with `invite`'s hints too;
    /// they stay among the addresses to dial for as long as the app runs.
    async fn reach(
        &self,
        invite: &Invite,
        id: EndpointId,
    ) -> Result<Arc<ServerConnection>, SdkError> {
        if !invite.addrs.is_empty() {
            let mut manual = self.inner.manual.lock();
            let known = manual.remove(&id).unwrap_or_default();
            manual.insert(id, newest_first(&invite.addrs, &known, MAX_HINTS));
        }
        if let Some(url) = &invite.lookup {
            self.look_up_at(url);
        }
        self.hold_token(invite).await;
        // Named by the invite: dialled even with no address or relay, the
        // lookup (or a hint learned since) the way to it.
        self.inner.invite_relays.lock().entry(id).or_default();
        if !invite.relays.is_empty() {
            let mut invited = self.inner.invite_relays.lock();
            let known = invited.entry(id).or_default();
            *known = newest_first(&invite.relays, known, MAX_HINTS);
        }
        self.link(id).connection(self).await
    }
}

impl Client {
    /// The link's relays, as this device would reach them with `invite`'s
    /// token: none with relays off, or for a link without a code.
    fn token_relays(&self, invite: &Invite) -> Option<(String, Vec<RelayUrl>)> {
        if !self.inner.relays {
            return None;
        }
        let token = invite_token(invite.code.as_deref()?, self.inner.endpoint.id().as_bytes())?;
        let urls = invite
            .relays
            .iter()
            .take(MAX_HINTS)
            .filter_map(|relay| relay.parse().ok())
            .collect();
        Some((token, urls))
    }

    /// Reach `invite`'s relays with its token (P6-08): its holder relays
    /// as the invite's tier. In the endpoint's relay map, so each
    /// connection to them made from now on carries it; the map is
    /// otherwise empty, so they may also be the device's home relay for
    /// that long.
    async fn hold_token(&self, invite: &Invite) {
        let Some((token, urls)) = self.token_relays(invite) else {
            return;
        };
        for url in urls {
            let config = RelayConfig::new(url.clone(), None).with_auth_token(token.clone());
            self.inner
                .endpoint
                .insert_relay(url, Arc::new(config))
                .await;
        }
    }

    /// Stop reaching `invite`'s relays with its token: a connection made
    /// with it lasts until it closes, and the next one is made without.
    async fn drop_token(&self, invite: &Invite) {
        let Some((_, urls)) = self.token_relays(invite) else {
            return;
        };
        for url in urls {
            self.inner.endpoint.remove_relay(&url).await;
        }
    }
}

/// [`SdkError::IdentityChanged`] if `conn`, to unsaved key `id`, answered
/// with the server id of a server saved under another key.
fn refuse_another_key(
    saved: &[SavedServer],
    id: EndpointId,
    conn: &ServerConnection,
) -> Result<(), SdkError> {
    match same_server(saved, id, &conn.info.server_id) {
        Some(old) => Err(SdkError::IdentityChanged {
            saved: old.endpoint_id.clone(),
            now: id.to_string(),
        }),
        None => Ok(()),
    }
}

/// `invite`'s hints, joining `server`'s: its addresses as told ones, and
/// its relays beside those the server told.
fn hint(server: &mut SavedServer, invite: &Invite, now: u64) {
    server.hinted(invite.addrs.iter().copied(), now);
    if invite.lookup.is_some() {
        server.lookup = invite.lookup.clone();
    }
    for relay in &invite.relays {
        if !server.relays.contains(relay) {
            server.relays.push(relay.clone());
        }
    }
}
