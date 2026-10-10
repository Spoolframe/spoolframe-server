//! Making invites in the app (PX-44): an admin's "Invites" screen, with
//! the web UI's fields, limits and list. The web UI and the apps make the
//! same invites, and each lists and revokes the other's.
//!
//! Only an admin's session may ([`SdkError::AdminRequired`] otherwise;
//! [`SdkError::NotForPortableUsers`] for a portable guest). Making and
//! revoking also need a recent confirmation: on
//! [`SdkError::RecentSignInRequired`], confirm with
//! [`Session::confirm_password`] or a passkey
//! ([`Session::begin_confirm_passkey`]), then ask again. Listing does not.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use spoolframe_protocol::invite_admin::{self as wire, CreateInvite};
use spoolframe_protocol::{Reply, Request};
use spoolframe_types::{Platform, Role};

use crate::{SdkError, Session};

/// What redeeming an invite does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InviteKind {
    /// Makes an account on the server (P6-01).
    CreateUser,
    /// Lets someone with a portable identity watch, with no account made
    /// (P7-05).
    PortableAccount,
    /// Pairs another server with this one (P8-01): the server refuses to
    /// make one until then ([`SdkError::InvalidInput`]).
    Federation,
    /// A kind this build does not know, from a newer server: listed, never
    /// made.
    Unknown,
}

/// An invite to make. Each `None` leaves it to the server, as the web UI
/// does: one use, 7 days, the consumer role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteSpec {
    pub kind: InviteKind,
    /// Who it is for, for the admins' list: at most 64 characters, one
    /// line.
    pub note: Option<String>,
    /// How many times it may be redeemed: 1 to 100.
    pub uses: Option<u32>,
    /// How long it lasts: an hour to 30 days, to the second.
    pub expires_in: Option<Duration>,
    /// The role the accounts it makes get: only for
    /// [`InviteKind::CreateUser`]. Any other kind with a role but the
    /// consumer's is [`SdkError::InvalidInput`].
    pub role: Option<Role>,
}

/// An invite just made: its code shows only now, as the server keeps only
/// its hash.
#[derive(Clone, PartialEq, Eq)]
pub struct MintedInvite {
    pub id: String,
    pub kind: InviteKind,
    /// As people read and type it: `QW7K-2MFP-XR4T`.
    pub code: String,
    /// `spoolframe://invite?…` with the code: what the QR code shows.
    pub link: String,
    /// What to copy and send, for "Add a server" on the other device: the
    /// link, as the web UI copies it.
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
pub struct InviteCreator {
    /// What to show: the admin's display name, or "server CLI".
    pub display_name: String,
    /// The admin's username; `None` for an invite made with the server's
    /// CLI, which signs in as no one.
    pub username: Option<String>,
}

/// Where an invite was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InviteOrigin {
    /// The web UI's admin pages; also every invite from before PX-44
    /// other than the CLI's.
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
    /// after a recent confirmation (see the [module docs](self)).
    /// [`SdkError::InvalidInput`] for a field out of its limits, and for a
    /// federation invite until P8 builds them.
    pub async fn create_invite(&self, spec: InviteSpec) -> Result<MintedInvite, SdkError> {
        let kind = match spec.kind {
            InviteKind::CreateUser => wire::InviteKind::CreateUser,
            InviteKind::PortableAccount => wire::InviteKind::PortableAccount,
            InviteKind::Federation => wire::InviteKind::Federation,
            InviteKind::Unknown => {
                return Err(SdkError::InvalidInput("pick what the invite is for".into()));
            }
        };
        let request = Request::CreateInvite(CreateInvite {
            kind,
            note: spec.note,
            uses: spec.uses,
            expires_in_secs: spec.expires_in.map(|d| d.as_secs()),
            role: spec.role,
        });
        match self.call(request).await? {
            Reply::InviteCreated(created) => Ok(MintedInvite {
                id: created.invite.id,
                kind: kind_of(created.invite.kind),
                code: created.code,
                copy_string: created.link.clone(),
                link: created.link,
                expires_at: time(created.invite.expires_at),
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
        match self
            .call(Request::RevokeInvite { id: id.to_owned() })
            .await?
        {
            Reply::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }
}

fn unexpected(reply: Reply) -> SdkError {
    SdkError::Protocol(format!("unexpected reply {reply:?}"))
}

fn kind_of(kind: wire::InviteKind) -> InviteKind {
    match kind {
        wire::InviteKind::CreateUser => InviteKind::CreateUser,
        wire::InviteKind::PortableAccount => InviteKind::PortableAccount,
        wire::InviteKind::Federation => InviteKind::Federation,
        wire::InviteKind::Unknown => InviteKind::Unknown,
    }
}

/// Unix seconds as a time; one before 1970 (none is) as 1970.
fn time(secs: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(u64::try_from(secs).unwrap_or(0))
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
        kind: kind_of(invite.kind),
        note: invite.note,
        role: invite.role,
        uses: invite.uses,
        used: invite.uses.saturating_sub(invite.uses_left),
        expires_at: time(invite.expires_at),
        created_at: time(invite.created_at),
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
        assert_eq!(
            invite.expires_at,
            UNIX_EPOCH + Duration::from_secs(1_800_000_000)
        );
    }
}
