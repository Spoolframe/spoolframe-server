//! Invites: finding, saving and joining a server from its link.
//!
//! The link names the server's key, so the key is pinned as it is, with
//! nothing trusted on first use; the server id is learned at `Hello` and
//! pinned with it. The link's addresses are hints: they join the
//! addresses this device knows as told ones, and Iroh still
//! proves the key whatever path it takes. A link naming a key this device
//! does not have saved is another server, whatever its name; one
//! whose server id is a saved server's under another key is that server
//! with a new key, [`SdkError::IdentityChanged`], as for any connect.
//!
//! An app's flow:
//!
//! 1. [`Client::read_invite`] when the link is pasted or opened: nothing
//!    is sent. [`SdkError::NotAnInvite`] goes under the paste field.
//! 2. [`Client::connect_invite`]: who answers, for "Is this the server you
//!    were invited to?" (its fingerprint, and the server id its passkeys
//!    are for). Nothing is saved. Or, for "Got an invite?",
//!    [`Client::look_up_invite`]: the same, and what the code is (valid,
//!    expired, used, revoked or no match), with nothing used up. A code
//!    typed in without a link is looked up at a server with
//!    [`Client::look_up_invite_code`]; [`parse_code`] checks it first.
//! 3. With a code, the join form, then [`Client::redeem_invite`]: the
//!    account, the server saved, and a session. Without one, or when a
//!    redemption is refused and the user picks "Save server only",
//!    [`Client::save_invite_server`].
//!
//! Until then the device is no one's, and the server's own relay
//! carries only its owner's and friends'. A link with a code makes the
//! device its holder: from the first connection to the last step
//! it reaches the link's relays with the code's token
//! ([`spoolframe_protocol::relay_access::invite_token`]), which says to
//! the relay whom the invite is for. Once redeemed, or the server saved
//! without it, the token is dropped: the session says who the device is.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use iroh::{EndpointId, RelayConfig, RelayUrl};
use spoolframe_protocol::invite::{Invite, InviteLinkKind, MAX_HINTS, normalize_code, parse_link};
use spoolframe_protocol::relay_access::invite_token;
use spoolframe_protocol::{
    BeginInvitePasskey, FinishPasskeyRegistration, InviteLookupOutcome, LookupInvite,
    PasskeyCreation, RedeemInvite, RedeemInviteWithPasskey, RedeemPortableInvite, Reply, Request,
    Response, ServerId, fingerprint,
};
use spoolframe_types::Role;

use crate::client::{Client, newest_first, parse_id, same_server};
use crate::connection::ServerConnection;
use crate::gate::{Gate, InvitePasskeys, Invited};
use crate::invite_admin::{InviteKind, time};
use crate::portable::PortableUserId;
use crate::servers::{self, SavedServer};
use crate::session::{Session, SignInMethod};
use crate::{SdkError, ServerEntry};

/// The code of account invite `invite`; one without only finds the
/// server.
fn code_of(invite: &Invite) -> Result<String, SdkError> {
    invite.code.clone().ok_or_else(|| {
        SdkError::InvalidInput(
            "this invite has no code: it only finds the server; sign in instead".into(),
        )
    })
}

/// What an invite link says, read without a word to the server: for the
/// "Add a server" summary and the join form ("Joins as a consumer ·
/// expires Oct 14").
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct InviteInfo {
    /// The server's key, hex: what the link pins.
    pub endpoint_id: String,
    /// The key's fingerprint, for "Is this the server you were
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
    /// What the code does, as the link says: makes an account
    /// ([`Client::redeem_invite`]), or grants a portable identity access
    /// ([`Client::redeem_invite_portable`]), or pairs another server
    /// (experimental: an admin of that server redeems it, with
    /// [`crate::Session::pair_server`] there). A hint for which form to
    /// show; the server answers the other with
    /// [`SdkError::WrongInviteKind`].
    pub kind: InviteLinkKind,
    /// This key is a saved server already: the link only adds its hints.
    pub saved: bool,
}

/// What an invite's code is, as its server tells it before joining
/// ([`Client::look_up_invite`]). Looking never uses it up.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum InviteOutcome {
    /// It can be redeemed now: with [`Client::redeem_invite`] for
    /// [`InviteKind::CreateUser`], [`Client::redeem_invite_portable`] for
    /// [`InviteKind::PortableAccount`].
    Valid {
        kind: InviteKind,
        /// The role the account gets; the consumer's for a portable one.
        role: Role,
        expires_at: SystemTime,
        /// The display name of the admin who made it ("Invited by
        /// Andrey"); `None` when they have none, or the server's CLI made
        /// it.
        inviter: Option<String>,
    },
    /// It expired at `at`. Offer "Save server only".
    Expired { at: SystemTime },
    /// Every use it had is used. Offer "Save server only".
    Used,
    /// An admin revoked it. Offer "Save server only".
    Revoked,
    /// No invite has this code: check what was typed. Each counts as a
    /// wrong guess; after a few, [`SdkError::RateLimited`].
    NoMatch,
    /// An outcome this SDK does not know, from a newer server.
    Unknown,
}

/// An invite looked up: who answered, and what its code is.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct InviteLookup {
    /// The server that answered, as [`Client::connect_invite`] says it:
    /// its name, fingerprint and server id, for "Is this the server you
    /// were invited to?". Not saved by the lookup.
    pub server: ServerEntry,
    pub outcome: InviteOutcome,
    /// The invite as a link, which joining takes
    /// ([`Client::redeem_invite`]): the link looked up, or for a code
    /// typed in, one made of the server that answered (its addresses,
    /// relays and lookup, so a server reached only through them is
    /// joined through them too), what it said of the code, and the code.
    pub link: String,
}

/// `typed` as an invite's code, normalized (`QW7K2MFPXR4T`), whatever
/// case, dashes or spaces it was typed with. Nothing is sent.
/// [`SdkError::NotAnInviteCode`] for anything that cannot be one, a link
/// included: [`Client::look_up_invite`] takes a link.
pub fn parse_code(typed: &str) -> Result<String, SdkError> {
    normalize_code(typed).ok_or(SdkError::NotAnInviteCode)
}

fn outcome_of(outcome: InviteLookupOutcome) -> InviteOutcome {
    match outcome {
        InviteLookupOutcome::Valid {
            kind,
            role,
            expires_at,
            inviter,
            ..
        } => InviteOutcome::Valid {
            kind: kind.into(),
            role,
            expires_at: time(expires_at),
            inviter,
        },
        InviteLookupOutcome::Expired { at } => InviteOutcome::Expired { at: time(at) },
        InviteLookupOutcome::Used => InviteOutcome::Used,
        InviteLookupOutcome::Revoked => InviteOutcome::Revoked,
        InviteLookupOutcome::NoMatch => InviteOutcome::NoMatch,
        InviteLookupOutcome::Unknown => InviteOutcome::Unknown,
    }
}

/// Code `code`, typed for `server`, as an invite link: the server's
/// name, `addrs` and relays (at most [`MAX_HINTS`] of each, as a link is
/// read), its pkarr `lookup`, and what the server said of the code.
fn code_link(
    server: &ServerEntry,
    addrs: Vec<SocketAddr>,
    lookup: Option<String>,
    code: String,
    outcome: &InviteOutcome,
) -> String {
    let mut invite = Invite {
        server: server.endpoint_id.clone(),
        name: Some(server.name.clone()),
        addrs: addrs.into_iter().take(MAX_HINTS).collect(),
        relays: server.relays.iter().take(MAX_HINTS).cloned().collect(),
        lookup,
        code: Some(code),
        ..Invite::default()
    };
    if let InviteOutcome::Valid {
        kind,
        role,
        expires_at,
        ..
    } = outcome
    {
        invite.role = Some(*role);
        invite.expires_at = expires_at
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs());
        invite.kind = match kind {
            InviteKind::PortableAccount => InviteLinkKind::PortableAccount,
            InviteKind::Federation => InviteLinkKind::Federation,
            InviteKind::CreateUser | InviteKind::Unknown => InviteLinkKind::CreateUser,
        };
    }
    invite.link()
}

/// Why an invite's code will not work, as an app words it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum InviteRefusal {
    /// No invite has the code, or it was revoked.
    Invalid,
    Expired,
    /// Every use it had is used.
    Used,
    Revoked,
    /// It is for the other kind of join: an account, or a portable
    /// identity.
    WrongKind,
}

impl InviteRefusal {
    /// What a lookup said, as a refusal; `None` for a code that works, or
    /// an outcome this build does not know (joining finds out).
    pub fn of_outcome(outcome: &InviteOutcome) -> Option<Self> {
        match outcome {
            InviteOutcome::Expired { .. } => Some(Self::Expired),
            InviteOutcome::Used => Some(Self::Used),
            InviteOutcome::Revoked => Some(Self::Revoked),
            InviteOutcome::NoMatch => Some(Self::Invalid),
            InviteOutcome::Valid { .. } | InviteOutcome::Unknown => None,
        }
    }

    /// What a redemption's `error` says of the code; `None` for one that
    /// is not about it.
    pub fn of_error(error: &SdkError) -> Option<Self> {
        match error {
            SdkError::InviteInvalid => Some(Self::Invalid),
            SdkError::InviteExpired => Some(Self::Expired),
            SdkError::InviteExhausted => Some(Self::Used),
            SdkError::WrongInviteKind => Some(Self::WrongKind),
            _ => None,
        }
    }

    /// The error redeeming such a code is.
    fn error(self) -> SdkError {
        match self {
            Self::Invalid | Self::Revoked => SdkError::InviteInvalid,
            Self::Expired => SdkError::InviteExpired,
            Self::Used => SdkError::InviteExhausted,
            Self::WrongKind => SdkError::WrongInviteKind,
        }
    }
}

/// What an invite's check leads to once its server is reached
/// ([`invite_next`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum InviteNext {
    /// The account form: username, display name, password
    /// ([`Client::redeem_invite`]).
    MakeAccount,
    /// Join as one of this device's identities
    /// ([`Client::redeem_invite_portable`]).
    JoinAsIdentity,
    /// A guest invite, with no identity here to join as: make one first,
    /// or open it on the phone that holds one.
    NeedsIdentity,
    /// A link that only finds the server: save it
    /// ([`Client::save_invite_server`]), then sign in.
    SaveServer,
    /// A server invite: an admin pairs the open server with it
    /// ([`crate::Session::pair_server`]), never a join.
    PairServer,
    /// The code will not work: say why, and offer "Save server only".
    Refused { why: InviteRefusal },
}

/// What invite `info` leads to: by what the server said of its code in
/// `lookup` first, else by what the link says. `identities_here`: this
/// device holds a portable identity that can join (one whose root is
/// here). The Apple and Linux apps each decided this alone.
pub fn invite_next(
    info: &InviteInfo,
    lookup: Option<&InviteLookup>,
    identities_here: bool,
) -> InviteNext {
    if let Some(why) = lookup.and_then(|l| InviteRefusal::of_outcome(&l.outcome)) {
        return InviteNext::Refused { why };
    }
    match info.kind {
        InviteLinkKind::Federation => InviteNext::PairServer,
        InviteLinkKind::PortableAccount if identities_here => InviteNext::JoinAsIdentity,
        InviteLinkKind::PortableAccount => InviteNext::NeedsIdentity,
        InviteLinkKind::CreateUser if info.has_code => InviteNext::MakeAccount,
        InviteLinkKind::CreateUser => InviteNext::SaveServer,
    }
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
        Ok(self.connect_invited(&invite, id).await?.0)
    }

    /// The server `invite` names, reached at its hints, and its entry.
    /// Nothing is saved.
    async fn connect_invited(
        &self,
        invite: &Invite,
        id: EndpointId,
    ) -> Result<(ServerEntry, Arc<ServerConnection>), SdkError> {
        let (conn, saved) = self.reach_checked(invite, id).await?;
        let pinned = saved.iter().find(|s| s.endpoint_id == invite.server);
        Ok((self.entry(id, &conn, pinned), conn))
    }

    /// "Got an invite?" with invite `link`: connect as
    /// [`Client::connect_invite`] does, and ask the server what the link's
    /// code is. Nothing is saved, and nothing used up.
    ///
    /// Refusals: [`SdkError::NotAnInvite`]; [`SdkError::InvalidInput`] for
    /// a link with no code (it only finds the server);
    /// [`SdkError::RateLimited`] after too many codes that match nothing;
    /// and `connect_invite`'s.
    pub async fn look_up_invite(&self, link: &str) -> Result<InviteLookup, SdkError> {
        let (invite, id) = parsed(link)?;
        let Some(code) = invite.code.clone() else {
            return Err(SdkError::InvalidInput(
                "this invite has no code: it only finds the server".into(),
            ));
        };
        let (server, _) = self.connect_invited(&invite, id).await?;
        let outcome = self.look_up(id, code).await?;
        Ok(InviteLookup {
            server,
            outcome,
            link: invite.link(),
        })
    }

    /// "Got an invite?" with a code typed in, for server `endpoint_id`: a
    /// saved one, or one heard on the LAN or reached at `addrs`, which
    /// join the addresses known as a link's do (none needed for a saved
    /// server). Connects as [`Client::connect`] does, and asks the server
    /// what code `typed` is. Nothing is saved, and nothing used up.
    ///
    /// Refusals: [`SdkError::NotAnInviteCode`] ([`parse_code`]), before
    /// anything is sent; [`Client::look_up_invite`]'s, and
    /// [`Client::connect`]'s.
    pub async fn look_up_invite_code(
        &self,
        endpoint_id: &str,
        addrs: Vec<SocketAddr>,
        typed: &str,
    ) -> Result<InviteLookup, SdkError> {
        let code = parse_code(typed)?;
        let id = parse_id(endpoint_id)?;
        self.hint_addrs(id, &addrs);
        let server = self.connect(&id.to_string()).await?;
        let outcome = self.look_up(id, code.clone()).await?;
        let lookup = servers::load(self.inner.storage.as_ref())
            .await?
            .into_iter()
            .find(|s| s.endpoint_id == server.endpoint_id)
            .and_then(|s| s.lookup);
        let addrs = if addrs.is_empty() {
            server.addrs.clone()
        } else {
            addrs
        };
        let link = code_link(&server, addrs, lookup, code, &outcome);
        Ok(InviteLookup {
            server,
            outcome,
            link,
        })
    }

    /// Join with a code typed in, for server `endpoint_id` (as for
    /// [`Client::look_up_invite_code`]): the code is looked up, then
    /// redeemed as [`Client::redeem_invite`] redeems a link, with nothing
    /// else to type. A code the lookup says will not work is refused as
    /// redeeming it would be ([`SdkError::InviteInvalid`],
    /// [`SdkError::InviteExpired`], [`SdkError::InviteExhausted`]); one for
    /// a portable identity is [`SdkError::WrongInviteKind`].
    pub async fn redeem_invite_code(
        &self,
        endpoint_id: &str,
        addrs: Vec<SocketAddr>,
        typed: &str,
        username: &str,
        display_name: Option<&str>,
        password: &str,
    ) -> Result<Session, SdkError> {
        let lookup = self.look_up_invite_code(endpoint_id, addrs, typed).await?;
        match InviteRefusal::of_outcome(&lookup.outcome) {
            Some(refusal) => Err(refusal.error()),
            None => match lookup.outcome {
                InviteOutcome::Valid {
                    kind: InviteKind::PortableAccount | InviteKind::Federation,
                    ..
                } => Err(SdkError::WrongInviteKind),
                _ => {
                    self.redeem_invite(&lookup.link, username, display_name, password)
                        .await
                }
            },
        }
    }

    /// Ask server `id` what invite `code` is.
    async fn look_up(&self, id: EndpointId, code: String) -> Result<InviteOutcome, SdkError> {
        let request = Request::LookupInvite(LookupInvite { code });
        match self.call(id, None, request).await? {
            Response::Ok(Reply::InviteLookup { outcome }) => Ok(outcome_of(outcome)),
            Response::Ok(other) => Err(SdkError::Protocol(format!(
                "LookupInvite answered {other:?}"
            ))),
            Response::Err(e) => Err(SdkError::from_server(e)),
        }
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
                server.connected_over(conn.selected_addr());
                hint(&mut server, invite, now);
                tracing::info!(id = %invite.server, name = server.name, "pinned a server from an invite");
                saved.push(server.clone());
                server
            }
        };
        self.look_up_fallback(&server);
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
        let code = code_of(&invite)?;
        // Before an account is made: a key claiming a saved server's id
        // is refused now, not after.
        self.reach_checked(&invite, id).await?;
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
                ..
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

    /// Feature `G`'s calls for invite `link`'s server, before an account:
    /// the server is reached, and they are [`SdkError::FeatureOff`] if it
    /// did not name `G`. Nothing else is sent. For an account with no
    /// password, [`Invited<InvitePasskeys>`]; off, offer a password
    /// ([`Client::redeem_invite`]).
    pub async fn invited<G: Gate>(&self, link: &str) -> Result<Invited<G>, SdkError> {
        let (invite, id) = parsed(link)?;
        let conn = self.reach(&invite, id).await?;
        if !conn.info.features.contains(G::FEATURE) {
            return Err(SdkError::FeatureOff {
                feature: G::FEATURE,
            });
        }
        Ok(Invited::new(self.clone(), link.to_owned(), invite, id))
    }

    /// Join the server guest invite `link` names as portable identity
    /// `identity`, on the phone that holds its root: the phone
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
        self.reach_checked(&invite, id).await?;
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
        self.hint_addrs(id, &invite.addrs);
        if let Some(url) = &invite.lookup {
            self.look_up_at(url);
        }
        self.hold_token(invite).await;
        // Named by the invite: dialled even with no address or relay, the
        // lookup (or a hint learned since) the way to it.
        self.inner.invite_relays.lock().entry(id).or_default();
        if !invite.relays.is_empty() {
            // Acting on an invite whose link names relays is consent to
            // them: the server is reached through them for the rest of the
            // run, whatever the client's relays, and saving it
            // (`save_invited`) keeps that as its choice.
            self.inner.relay_policy.choose(id, Some(true));
            let mut invited = self.inner.invite_relays.lock();
            let known = invited.entry(id).or_default();
            *known = newest_first(&invite.relays, known, MAX_HINTS);
        }
        self.link(id).connection(self).await
    }

    /// `addrs` join the addresses server `id` is dialled at, newest first;
    /// they stay for as long as the app runs.
    fn hint_addrs(&self, id: EndpointId, addrs: &[SocketAddr]) {
        if addrs.is_empty() {
            return;
        }
        let mut manual = self.inner.manual.lock();
        let known = manual.remove(&id).unwrap_or_default();
        manual.insert(id, newest_first(addrs, &known, MAX_HINTS));
    }
}

impl Client {
    /// The link's relays, as this device reaches them with `invite`'s
    /// token: none for a link without a code. Whatever the client's
    /// relays: redeeming the invite is consent to them (see `reach`).
    fn token_relays(&self, invite: &Invite) -> Option<(String, Vec<RelayUrl>)> {
        let token = invite_token(invite.code.as_deref()?, self.inner.endpoint.id().as_bytes())?;
        let urls = invite
            .relays
            .iter()
            .take(MAX_HINTS)
            .filter_map(|relay| relay.parse().ok())
            .collect();
        Some((token, urls))
    }

    /// Reach `invite`'s relays with its token: its holder relays
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

impl Client {
    /// `invite`'s server reached at key `id`, and the saved servers: a key
    /// not saved that answers with a saved server's server id is
    /// [`SdkError::IdentityChanged`], before anything is sent to it.
    async fn reach_checked(
        &self,
        invite: &Invite,
        id: EndpointId,
    ) -> Result<(Arc<ServerConnection>, Vec<SavedServer>), SdkError> {
        let conn = self.reach(invite, id).await?;
        let saved = servers::load(self.inner.storage.as_ref()).await?;
        if !saved.iter().any(|s| s.endpoint_id == invite.server) {
            refuse_another_key(&saved, id, &conn)?;
        }
        Ok((conn, saved))
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
            server_id: old.server_id.clone(),
        }),
        None => Ok(()),
    }
}

/// `invite`'s hints, joining `server`'s: its addresses as told ones, its
/// lookup, and its relays beside those the server told (not into a list
/// the user edited), chosen for it.
fn hint(server: &mut SavedServer, invite: &Invite, now: u64) {
    server.hinted(invite.addrs.iter().copied(), now);
    if invite.lookup.is_some() {
        server.lookup = invite.lookup.clone();
    }
    if invite.relays.is_empty() {
        return;
    }
    server.use_relays = Some(true);
    if server.relays_edited {
        return;
    }
    for relay in &invite.relays {
        if !server.relays.contains(relay) {
            server.relays.push(relay.clone());
        }
    }
}

impl Invited<InvitePasskeys> {
    /// Begin making an account with the invite's code and an app passkey,
    /// with no password: what the platform's passkey API makes the passkey
    /// with, for `username`, shown as `display_name` or the username.
    /// Nothing is made or saved yet; finish with
    /// [`Invited::redeem_invite_passkey`] within 5 minutes, from this
    /// device.
    ///
    /// Refusals as for [`Client::redeem_invite`]: a username someone has is
    /// [`SdkError::UsernameTaken`] now, though one may still take it before
    /// the finish.
    pub async fn begin_invite_passkey(
        &self,
        username: &str,
        display_name: Option<&str>,
    ) -> Result<PasskeyCreation, SdkError> {
        let client = &self.client;
        let (invite, id) = (&self.invite, self.id);
        let code = code_of(invite)?;
        // A key claiming a saved server's id is refused before a passkey is
        // made for it.
        let (conn, _) = client.reach_checked(invite, id).await?;
        let server_id: ServerId = conn.info.server_id.parse().map_err(|_| {
            SdkError::Protocol(format!("{:?} is not a server id", conn.info.server_id))
        })?;
        let request = Request::BeginInvitePasskey(BeginInvitePasskey {
            code,
            username: username.to_owned(),
            display_name: display_name.map(str::to_owned),
        });
        match client.call(id, None, request).await? {
            Response::Ok(Reply::PasskeyRegistrationBegun(begun)) => {
                Ok(begun.for_server(id.as_bytes(), &server_id))
            }
            Response::Ok(other) => Err(SdkError::Protocol(format!(
                "BeginInvitePasskey answered {other:?}"
            ))),
            Response::Err(e) => Err(SdkError::from_server(e)),
        }
    }

    /// Finish it with the passkey the platform made (`made.name` names
    /// this device, as for [`Session::finish_passkey_registration`]): the
    /// account is made, and its session kept, as [`Client::redeem_invite`]
    /// keeps one; `remember` trusts this device for them, as for
    /// [`Client::sign_in`]. The server is saved once the account is made;
    /// a refused redemption saves nothing.
    ///
    /// A passkey that does not verify, a registration over or begun on
    /// another device, or another invite's link is
    /// [`SdkError::InvalidCredentials`]: begin again. The invite's
    /// refusals are [`Client::redeem_invite`]'s, and a username taken since
    /// the beginning is [`SdkError::UsernameTaken`]; neither uses the
    /// invite up.
    pub async fn redeem_invite_passkey(
        &self,
        made: FinishPasskeyRegistration,
        remember: bool,
    ) -> Result<Session, SdkError> {
        let (invite, _) = parsed(&self.link)?;
        let redeemed = self.redeem_passkey(made, remember).await;
        self.client.drop_token(&invite).await;
        redeemed
    }

    async fn redeem_passkey(
        &self,
        made: FinishPasskeyRegistration,
        remember: bool,
    ) -> Result<Session, SdkError> {
        let client = &self.client;
        let (invite, id) = (&self.invite, self.id);
        let code = code_of(invite)?;
        client.reach_checked(invite, id).await?;
        let request = Request::RedeemInviteWithPasskey(RedeemInviteWithPasskey {
            code,
            finish: made,
            remember,
        });
        let (token, remembered) = match client.call(id, None, request).await? {
            Response::Ok(Reply::LoggedIn {
                session,
                remembered,
                ..
            }) => (session, remembered),
            Response::Ok(other) => {
                return Err(SdkError::Protocol(format!(
                    "RedeemInviteWithPasskey answered {other:?}"
                )));
            }
            Response::Err(e) => return Err(SdkError::from_server(e)),
        };
        client.save_invite_server(&self.link).await?;
        client
            .keep_session(id, token, Some(SignInMethod::Passkey), remembered)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_is_read_however_it_is_typed() {
        for typed in ["QW7K-2MFP-XR4T", "qw7k 2mfp xr4t", " Qw7K2mFpxR4t "] {
            assert_eq!(
                parse_code(typed),
                Ok("QW7K2MFPXR4T".to_owned()),
                "{typed:?}"
            );
        }
    }

    /// A link is not a bare code, even one with a code in it: the app
    /// looks it up with [`Client::look_up_invite`].
    #[test]
    fn a_link_or_a_damaged_code_is_not_a_code() {
        let link = "spoolframe://invite?server=ab&cap=QW7K-2MFP-XR4T";
        for typed in ["", "QW7K-2MFP", "QW7K-2MFP-XR40", "QW7K-2MFP-XR4TT", link] {
            assert_eq!(
                parse_code(typed),
                Err(SdkError::NotAnInviteCode),
                "{typed:?}"
            );
        }
    }

    #[test]
    fn each_outcome_reads_with_its_times() {
        use spoolframe_protocol::InviteLookupOutcome as Wire;
        use spoolframe_protocol::invite_admin::InviteKind as WireKind;
        let at = UNIX_EPOCH + Duration::from_secs(1_792_003_200);
        assert_eq!(
            outcome_of(Wire::Valid {
                kind: WireKind::PortableAccount,
                role: Role::Consumer,
                expires_at: 1_792_003_200,
                inviter: Some("Andrey".into()),
                fingerprint: None,
            }),
            InviteOutcome::Valid {
                kind: InviteKind::PortableAccount,
                role: Role::Consumer,
                expires_at: at,
                inviter: Some("Andrey".into()),
            }
        );
        assert_eq!(
            outcome_of(Wire::Expired { at: 1_792_003_200 }),
            InviteOutcome::Expired { at }
        );
        for (wire, read) in [
            (Wire::Used, InviteOutcome::Used),
            (Wire::Revoked, InviteOutcome::Revoked),
            (Wire::NoMatch, InviteOutcome::NoMatch),
            (Wire::Unknown, InviteOutcome::Unknown),
        ] {
            assert_eq!(outcome_of(wire), read);
        }
    }

    fn info(kind: InviteLinkKind, has_code: bool) -> InviteInfo {
        InviteInfo {
            endpoint_id: "ab".into(),
            fingerprint: String::new(),
            name: None,
            addrs: Vec::new(),
            relays: Vec::new(),
            has_code,
            expires_at: None,
            role: None,
            kind,
            saved: false,
        }
    }

    #[test]
    fn an_invite_leads_by_what_its_link_says() {
        use InviteLinkKind::*;
        let next =
            |kind, has_code, identities| invite_next(&info(kind, has_code), None, identities);
        assert_eq!(
            [
                next(CreateUser, true, false),
                next(CreateUser, false, false),
                next(PortableAccount, true, true),
                next(PortableAccount, true, false),
                next(Federation, true, false),
            ],
            [
                InviteNext::MakeAccount,
                InviteNext::SaveServer,
                InviteNext::JoinAsIdentity,
                InviteNext::NeedsIdentity,
                InviteNext::PairServer,
            ]
        );
    }

    #[test]
    fn what_the_server_said_of_the_code_comes_first() {
        let server = ServerEntry {
            endpoint_id: "ab".into(),
            name: "Home Media".into(),
            addrs: Vec::new(),
            advertised_addrs: Vec::new(),
            relays: Vec::new(),
            relays_edited: false,
            advertised_at: None,
            fingerprint: String::new(),
            server_id: None,
            pinned: false,
            verified: false,
            on_network: false,
            pinned_at: None,
            verified_at: None,
            last_connected_at: None,
            use_relays: None,
        };
        let lookup = InviteLookup {
            server,
            outcome: InviteOutcome::NoMatch,
            link: String::new(),
        };
        assert_eq!(
            invite_next(
                &info(InviteLinkKind::CreateUser, true),
                Some(&lookup),
                false
            ),
            InviteNext::Refused {
                why: InviteRefusal::Invalid
            }
        );
    }

    #[test]
    fn a_redemptions_refusal_is_read_as_the_lookups() {
        assert_eq!(
            [
                SdkError::InviteInvalid,
                SdkError::InviteExpired,
                SdkError::InviteExhausted,
                SdkError::WrongInviteKind,
                SdkError::UsernameTaken,
            ]
            .iter()
            .map(InviteRefusal::of_error)
            .collect::<Vec<_>>(),
            [
                Some(InviteRefusal::Invalid),
                Some(InviteRefusal::Expired),
                Some(InviteRefusal::Used),
                Some(InviteRefusal::WrongKind),
                None,
            ]
        );
    }
}
