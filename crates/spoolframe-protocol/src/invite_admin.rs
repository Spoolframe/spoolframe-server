//! Making and managing invites from an app: the client protocol's
//! admin requests `CreateInvite`, `ListInvites` and `RevokeInvite`. These
//! are the same invites, with the same limits, as the web UI makes; the
//! web UI and the apps see each other's.
//!
//! Only an admin's session may ask (`AdminRequired` otherwise), and only
//! for an account here (`NotForPortableUsers` for a portable guest).
//! Creating and revoking also need the admin to have confirmed it is them
//! recently, as the web UI's admin changes do: a fresh `ConfirmPassword`
//! or passkey confirmation, else `RecentSignInRequired`. Listing does
//! not.

use std::fmt;

use serde::{Deserialize, Serialize};
use spoolframe_types::{Platform, Role};

/// What redeeming an invite does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InviteKind {
    /// Makes an account here.
    CreateUser,
    /// Lets a portable identity watch here, with no account made.
    PortableAccount,
    /// Pairs another server with this one (experimental): made
    /// only for a client that named the `federation` feature, on a
    /// server that has it on.
    Federation,
    /// A kind this build does not know, from a newer server.
    #[serde(other)]
    Unknown,
}

/// An invite to make (`CreateInvite`). Each `None` is the server's
/// default, as for the web UI's `inviteCreate`: one use, 7 days, the
/// consumer role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateInvite {
    pub kind: InviteKind,
    /// Who it is for, for the admins' list: at most 64 characters, one
    /// line.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub note: Option<String>,
    /// How many times it may be redeemed: 1 to 100.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub uses: Option<u32>,
    /// How long it lasts from now, in seconds: an hour to 30 days.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub expires_in_secs: Option<u64>,
    /// The role the accounts it makes get; only for `create-user`. A
    /// `portable-account` invite with any role but `consumer` is
    /// `InvalidInput`: a portable guest has no account here to hold one.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub role: Option<Role>,
}

/// Who made an invite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteCreator {
    /// The admin's username; `None` for one made with the server's CLI,
    /// which signs in as no one.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub username: Option<String>,
    /// What to show: the admin's display name (their username when they
    /// have none, or when the account is gone), or "server CLI".
    pub display_name: String,
}

/// Where an invite was made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InviteOrigin {
    /// The web UI's admin pages. Also any invite made before the server
    /// recorded where invites come from, except the CLI's.
    WebAdmin,
    /// `spoolframe-server invite create`.
    Cli,
    /// An app, on the device it ran on, as its `Hello` named it.
    Device {
        /// The device's `EndpointId`, hex: an app compares it with its own
        /// to say "on this device".
        endpoint_id: String,
        #[serde(deserialize_with = "spoolframe_types::required::option")]
        name: Option<String>,
        platform: Platform,
    },
    /// A place this build does not know, from a newer server.
    #[serde(other)]
    Unknown,
}

/// An invite as admins see it: never its code, which the server keeps
/// only as a hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteSummary {
    /// What `RevokeInvite` takes.
    pub id: String,
    pub kind: InviteKind,
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub note: Option<String>,
    /// The role its accounts get; `consumer` for a `portable-account` one.
    pub role: Role,
    /// How many times it may be redeemed in all, and how many it still
    /// may.
    pub uses: u32,
    pub uses_left: u32,
    /// Unix seconds.
    pub expires_at: i64,
    /// Unix seconds.
    pub created_at: i64,
    pub created_by: InviteCreator,
    pub created_from: InviteOrigin,
}

/// A new invite (`CreateInvite`), with its code: shown this once.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteCreated {
    pub invite: InviteSummary,
    /// As people read and type it: `QW7K-2MFP-XR4T`.
    pub code: String,
    /// The invite link ([`crate::invite`]), the code in it: the
    /// `spoolframe://` form, what the QR code shows.
    pub link: String,
    /// The same invite as an https link on spoolframe.app
    /// ([`crate::invite::Invite::share_link`]): what the share
    /// sheet sends and Copy copies.
    pub share_link: String,
}

impl fmt::Debug for InviteCreated {
    // The code is a capability until it is used up: keep it out of logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InviteCreated")
            .field("invite", &self.invite)
            .field("code", &"<redacted>")
            .field("link", &"<redacted>")
            .field("share_link", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_origin_from_a_newer_server_reads_as_unknown() {
        let origin: InviteOrigin =
            serde_json::from_str(r#"{"type":"federated_peer","server":"x"}"#).unwrap();
        assert_eq!(origin, InviteOrigin::Unknown);
    }

    #[test]
    fn a_kind_from_a_newer_server_reads_as_unknown() {
        let kind: InviteKind = serde_json::from_str(r#""household""#).unwrap();
        assert_eq!(kind, InviteKind::Unknown);
    }

    #[test]
    fn a_device_origin_round_trips() {
        let origin = InviteOrigin::Device {
            endpoint_id: "ab".repeat(32),
            name: Some("Andrey's iPhone".into()),
            platform: Platform::Ios,
        };
        let json = serde_json::to_string(&origin).unwrap();
        assert_eq!(serde_json::from_str::<InviteOrigin>(&json).unwrap(), origin);
    }

    fn created() -> InviteCreated {
        InviteCreated {
            invite: InviteSummary {
                id: "id".into(),
                kind: InviteKind::CreateUser,
                note: None,
                role: Role::Consumer,
                uses: 1,
                uses_left: 1,
                expires_at: 1,
                created_at: 0,
                created_by: InviteCreator {
                    username: Some("andrey".into()),
                    display_name: "Andrey".into(),
                },
                created_from: InviteOrigin::WebAdmin,
            },
            code: "QW7K-2MFP-XR4T".into(),
            link: "spoolframe://invite?server=x&cap=QW7K2MFPXR4T".into(),
            share_link: "https://spoolframe.app/i#server=x&cap=QW7K2MFPXR4T".into(),
        }
    }

    #[test]
    fn the_code_and_link_stay_out_of_debug() {
        let shown = format!("{:?}", created());
        assert!(!shown.contains("QW7K"), "{shown}");
    }

    /// PX-45: a created invite without its share link, as a server
    /// before the reset to revision 1 could send it, is refused.
    #[test]
    fn a_created_invite_without_a_share_link_is_refused() {
        let mut sent = serde_json::to_value(created()).unwrap();
        sent.as_object_mut().unwrap().remove("share_link").unwrap();
        let error = serde_json::from_value::<InviteCreated>(sent).unwrap_err();
        assert!(
            error.to_string().contains("missing field `share_link`"),
            "{error}"
        );
    }
}
