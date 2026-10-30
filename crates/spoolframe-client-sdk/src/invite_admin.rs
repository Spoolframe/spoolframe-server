//! Making invites in the app: an admin's "Invites" screen, with the web
//! UI's fields, limits and list. The web UI and the apps make the
//! same invites, and each lists and revokes the other's.
//!
//! Only an admin's session may ([`SdkError::AdminRequired`] otherwise;
//! [`SdkError::NotForPortableUsers`] for a portable guest). Making and
//! revoking also need a recent confirmation: on
//! [`SdkError::RecentSignInRequired`], confirm with
//! [`Session::confirm_password`] or a passkey
//! ([`Session::begin_confirm_passkey`]), then ask again. Listing does not.

use std::time::{Duration, SystemTime};

use spoolframe_protocol::invite_admin::{self as wire, CreateInvite};
use spoolframe_protocol::{Reply, Request};
use spoolframe_types::serde_time::unix_seconds;
use spoolframe_types::{Platform, Role};

use crate::session::unexpected;
use crate::{SdkError, Session};

/// What redeeming an invite does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum InviteKind {
    /// Makes an account on the server.
    CreateUser,
    /// Lets someone with a portable identity watch, with no account made.
    PortableAccount,
    /// Pairs another server with this one (experimental): made
    /// only with [`crate::Experimental::federation`] on for the session
    /// ([`SdkError::InvalidInput`] otherwise). That server's admin redeems
    /// it ([`Session::pair_server`]).
    Federation,
    /// A kind this build does not know, from a newer server: listed, never
    /// made.
    Unknown,
}

/// An invite to make. Each `None` leaves it to the server, as the web UI
/// does: one use, 7 days, the consumer role.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct InviteSpec {
    pub kind: InviteKind,
    /// Who it is for, for the admins' list: at most 64 characters, one
    /// line.
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub note: Option<String>,
    /// How many times it may be redeemed: 1 to 100.
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub uses: Option<u32>,
    /// How long it lasts: an hour to 30 days, to the second.
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub expires_in: Option<Duration>,
    /// The role the accounts it makes get: only for
    /// [`InviteKind::CreateUser`]. Any other kind with a role but the
    /// consumer's is [`SdkError::InvalidInput`].
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub role: Option<Role>,
}

/// An invite just made: its code shows only now, as the server keeps only
/// its hash.
#[derive(Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MintedInvite {
    pub id: String,
    pub kind: InviteKind,
    /// As people read and type it: `QW7K-2MFP-XR4T`.
    pub code: String,
    /// `spoolframe://invite?…` with the code: what the QR code shows.
    pub link: String,
    /// The same invite as `https://spoolframe.app/i#…`, the
    /// invite in its fragment: what the share sheet sends. The apps open
    /// it as a universal link, and every `parse_link` reads it.
    pub share_link: String,
    /// What to copy and send, for "Add a server" on the other device:
    /// [`Self::share_link`], as the web UI copies it.
    pub copy_string: String,
    pub expires_at: SystemTime,
    /// How many times it may be redeemed.
    pub uses: u32,
}

impl std::fmt::Debug for MintedInvite {
    // The code is a capability until it is used up: keep it out of logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MintedInvite")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("code", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("uses", &self.uses)
            .finish()
    }
}

/// Who made an invite.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct InviteCreator {
    /// What to show: the admin's display name, or "server CLI".
    pub display_name: String,
    /// The admin's username; `None` for an invite made with the server's
    /// CLI, which signs in as no one.
    pub username: Option<String>,
}

/// Where an invite was made.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum InviteOrigin {
    /// The web UI's admin pages. Also an invite the server has no app
    /// origin recorded for, unless the CLI made it.
    WebAdmin,
    /// The server's command line.
    Cli,
    /// An app, by the device it ran on.
    Device {
        name: Option<String>,
        platform: Platform,
        /// It is this device: "on this computer".
        this_device: bool,
    },
    /// A place this build does not know, from a newer server.
    Unknown,
}

/// An invite on the admins' list, without its code. Only invites that can
/// still be redeemed are listed: a revoked, expired or used-up one is
/// gone from it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct InviteSummary {
    /// What [`Session::revoke_invite`] takes.
    pub id: String,
    pub kind: InviteKind,
    pub note: Option<String>,
    /// The role its accounts get; the consumer's for a portable-account
    /// invite.
    pub role: Role,
    /// How many times it may be redeemed in all ("3 of 4 left").
    pub uses: u32,
    /// How many times it has been.
    pub used: u32,
    pub expires_at: SystemTime,
    pub created_at: SystemTime,
    pub created_by: InviteCreator,
    pub created_from: InviteOrigin,
}

impl Session {
    /// Make an invite as the web UI's Invites page does. An admin's only,
    /// after a recent confirmation ([`Session::confirm_password`]).
    /// [`SdkError::InvalidInput`] for a field out of its limits, and for a
    /// federation invite unless the session has
    /// [`crate::Experimental::federation`] on.
    pub async fn create_invite(&self, spec: InviteSpec) -> Result<MintedInvite, SdkError> {
        let kind = wire::InviteKind::try_from(spec.kind)
            .map_err(|()| SdkError::InvalidInput("pick what the invite is for".into()))?;
        let request = Request::CreateInvite(CreateInvite {
            kind,
            note: spec.note,
            uses: spec.uses,
            expires_in_secs: spec.expires_in.map(|d| d.as_secs()),
            role: spec.role,
        });
        match self.call(request).await? {
            Reply::InviteCreated(created) => Ok(MintedInvite {
                share_link: created.share_link.clone(),
                copy_string: created.share_link.clone(),
                id: created.invite.id,
                kind: created.invite.kind.into(),
                code: created.code,
                link: created.link,
                expires_at: unix_seconds::from_wire(created.invite.expires_at),
                uses: created.invite.uses,
            }),
            other => Err(unexpected(other)),
        }
    }

    /// The server's invites that can still be redeemed, newest first, made
    /// here, in another app or in the web UI. An admin's only; no
    /// confirmation needed.
    pub async fn invites(&self) -> Result<Vec<InviteSummary>, SdkError> {
        match self.call(Request::ListInvites).await? {
            Reply::Invites { invites } => {
                let this_device = self.client.id();
                Ok(invites
                    .into_iter()
                    .map(|invite| summary(invite, &this_device))
                    .collect())
            }
            other => Err(unexpected(other)),
        }
    }

    /// Revoke invite `id`: its code stops working, and what it already
    /// made stays. An admin's only, after a recent confirmation.
    /// [`SdkError::NotFound`] for one no longer on the list.
    pub async fn revoke_invite(&self, id: &str) -> Result<(), SdkError> {
        self.done(Request::RevokeInvite { id: id.to_owned() }).await
    }
}

impl From<wire::InviteKind> for InviteKind {
    fn from(kind: wire::InviteKind) -> Self {
        match kind {
            wire::InviteKind::CreateUser => InviteKind::CreateUser,
            wire::InviteKind::PortableAccount => InviteKind::PortableAccount,
            wire::InviteKind::Federation => InviteKind::Federation,
            wire::InviteKind::Unknown => InviteKind::Unknown,
        }
    }
}

/// The kind to ask for: [`InviteKind::Unknown`] is none.
impl TryFrom<InviteKind> for wire::InviteKind {
    type Error = ();

    fn try_from(kind: InviteKind) -> Result<Self, ()> {
        match kind {
            InviteKind::CreateUser => Ok(Self::CreateUser),
            InviteKind::PortableAccount => Ok(Self::PortableAccount),
            InviteKind::Federation => Ok(Self::Federation),
            InviteKind::Unknown => Err(()),
        }
    }
}

fn summary(invite: wire::InviteSummary, this_device: &str) -> InviteSummary {
    let created_from = match invite.created_from {
        wire::InviteOrigin::WebAdmin => InviteOrigin::WebAdmin,
        wire::InviteOrigin::Cli => InviteOrigin::Cli,
        wire::InviteOrigin::Device {
            endpoint_id,
            name,
            platform,
        } => InviteOrigin::Device {
            name,
            platform,
            this_device: endpoint_id.eq_ignore_ascii_case(this_device),
        },
        wire::InviteOrigin::Unknown => InviteOrigin::Unknown,
    };
    InviteSummary {
        id: invite.id,
        kind: invite.kind.into(),
        note: invite.note,
        role: invite.role,
        uses: invite.uses,
        used: invite.uses.saturating_sub(invite.uses_left),
        expires_at: unix_seconds::from_wire(invite.expires_at),
        created_at: unix_seconds::from_wire(invite.created_at),
        created_by: InviteCreator {
            display_name: invite.created_by.display_name,
            username: invite.created_by.username,
        },
        created_from,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every kind crosses to the wire and back; `Unknown` is never asked
    /// for, and what the wire does not know reads as it.
    #[test]
    fn every_kind_round_trips_through_the_wire() {
        for kind in [
            InviteKind::CreateUser,
            InviteKind::PortableAccount,
            InviteKind::Federation,
        ] {
            let wire = wire::InviteKind::try_from(kind).unwrap();
            assert_eq!(InviteKind::from(wire), kind);
        }
        assert_eq!(wire::InviteKind::try_from(InviteKind::Unknown), Err(()));
        assert_eq!(
            InviteKind::from(wire::InviteKind::Unknown),
            InviteKind::Unknown
        );
    }

    fn listed(created_from: wire::InviteOrigin) -> wire::InviteSummary {
        wire::InviteSummary {
            id: "0192".into(),
            kind: wire::InviteKind::CreateUser,
            note: Some("Mum".into()),
            role: Role::Consumer,
            uses: 4,
            uses_left: 3,
            expires_at: 1_800_000_000,
            created_at: 1_700_000_000,
            created_by: wire::InviteCreator {
                username: Some("andrey".into()),
                display_name: "Andrey".into(),
            },
            created_from,
        }
    }

    #[test]
    fn an_invite_made_on_this_device_says_so() {
        let mine = "ab".repeat(32);
        let made_here = summary(
            listed(wire::InviteOrigin::Device {
                endpoint_id: mine.to_uppercase(),
                name: Some("desk".into()),
                platform: Platform::Linux,
            }),
            &mine,
        );
        assert_eq!(
            made_here.created_from,
            InviteOrigin::Device {
                name: Some("desk".into()),
                platform: Platform::Linux,
                this_device: true,
            }
        );
        let made_elsewhere = summary(
            listed(wire::InviteOrigin::Device {
                endpoint_id: "cd".repeat(32),
                name: None,
                platform: Platform::Ios,
            }),
            &mine,
        );
        assert!(matches!(
            made_elsewhere.created_from,
            InviteOrigin::Device {
                this_device: false,
                ..
            }
        ));
    }

    #[test]
    fn used_counts_the_uses_taken() {
        let invite = summary(listed(wire::InviteOrigin::WebAdmin), "");
        assert_eq!((invite.uses, invite.used), (4, 1));
        assert_eq!(invite.created_by.display_name, "Andrey");
        assert_eq!(invite.expires_at, unix_seconds::from_wire(1_800_000_000));
    }
}
