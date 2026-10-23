//! Who state belongs to (ARCHITECTURE §55). Server-side only: clients
//! learn who they are from `WhoAmI`.

use std::fmt;

use spoolframe_protocol::server_id::SERVER_ID_LEN;
use spoolframe_protocol::{PortableUserId, ServerId};

use crate::store::grants::PortableSessionUser;
use crate::store::sessions::SessionUser;
use crate::store::users::LocalUserId;

/// The owner of watch state and, later, other per-person data: a local
/// user, or a portable user granted access here (P7-06, §50). Users of
/// other servers (P8) arrive as another variant, which is why every table
/// and function is keyed on this rather than on a `LocalUserId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Principal {
    LocalUser(LocalUserId),
    PortableUser(PortableUserId),
    /// A paired server's local user, as that server vouched for them
    /// (P8-03): `user` is their stable id there. For tokens and logs on
    /// the server whose item they play; that server keeps no watch state,
    /// playlists or likes for them, and lists none of them to its admins.
    RemoteUser {
        server: RemoteServerId,
        user: uuid::Uuid,
    },
}

/// A paired server's [`ServerId`], as a [`Principal::RemoteUser`] holds
/// it: its 16 characters, so a principal stays `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RemoteServerId([u8; SERVER_ID_LEN]);

impl From<&ServerId> for RemoteServerId {
    fn from(id: &ServerId) -> Self {
        let mut bytes = [0; SERVER_ID_LEN];
        bytes.copy_from_slice(id.as_str().as_bytes());
        Self(bytes)
    }
}

impl RemoteServerId {
    /// The server id it holds.
    pub fn server_id(&self) -> ServerId {
        ServerId::parse(self.as_str()).expect("made from a server id")
    }

    fn as_str(&self) -> &str {
        // Made from a checked server id: ASCII.
        std::str::from_utf8(&self.0).unwrap_or_default()
    }
}

impl fmt::Display for RemoteServerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for Principal {
    /// The key it is stored under, and `WhoAmI`'s `user_id`:
    /// `local:0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4c` (P3-20), or
    /// `portable:` and the identity's 64 hex digits, or (P8-03)
    /// `remote:`, the paired server's id and the user's id there:
    /// `remote:mzxw6ytbojtg633c:0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4c`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Principal::LocalUser(id) => write!(f, "local:{id}"),
            Principal::PortableUser(id) => write!(f, "portable:{id}"),
            Principal::RemoteUser { server, user } => {
                write!(f, "remote:{server}:{}", user.hyphenated())
            }
        }
    }
}

impl std::str::FromStr for Principal {
    type Err = InvalidPrincipal;

    /// What [`Principal`]'s `Display` writes.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(id) = s.strip_prefix("local:") {
            return id
                .parse()
                .map(Principal::LocalUser)
                .map_err(|_| InvalidPrincipal);
        }
        if let Some(id) = s.strip_prefix("portable:") {
            return id
                .parse()
                .map(Principal::PortableUser)
                .map_err(|_| InvalidPrincipal);
        }
        if let Some(rest) = s.strip_prefix("remote:") {
            let (server, user) = rest.split_once(':').ok_or(InvalidPrincipal)?;
            let server = ServerId::parse(server).map_err(|_| InvalidPrincipal)?;
            let user = uuid::Uuid::try_parse(user).map_err(|_| InvalidPrincipal)?;
            return Ok(Principal::RemoteUser {
                server: RemoteServerId::from(&server),
                user,
            });
        }
        Err(InvalidPrincipal)
    }
}

/// Text that names no principal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not a user id")]
pub struct InvalidPrincipal;

/// Who is acting, with the rights they act with: what every call into a
/// service that judges access takes (playlists, PX-40), whether it comes
/// from a client's session or, later, an MCP tool acting for them (PX-42).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Actor {
    pub principal: Principal,
    /// An admin here. Never a portable user.
    pub admin: bool,
    /// Acting from a client that named the experimental `federation`
    /// feature (P8-02): a paired server's items are listed to them, in
    /// playlists and Liked as anywhere. Any other caller sees none.
    pub federated: bool,
}

/// Whose a live endpoint session is: a local user's, or a granted
/// portable user's (P7-06). The tied case is a local user's: a credential
/// for a tied identity signs in as the account (§58).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Who {
    Local(SessionUser),
    Portable(PortableSessionUser),
}

impl Who {
    pub fn principal(&self) -> Principal {
        match self {
            Who::Local(user) => Principal::LocalUser(user.user),
            Who::Portable(user) => Principal::PortableUser(user.user),
        }
    }

    /// Them, acting with their rights.
    pub fn actor(&self) -> Actor {
        Actor {
            principal: self.principal(),
            admin: matches!(self, Who::Local(user) if user.role == spoolframe_types::Role::Admin),
            federated: false,
        }
    }

    /// The local account, for what only an account here has (its
    /// password, PIN, passkeys, devices, sessions list, ties, approving
    /// sign-ins); `None` for a portable user, who has none of them.
    pub fn local(&self) -> Option<&SessionUser> {
        match self {
            Who::Local(user) => Some(user),
            Who::Portable(_) => None,
        }
    }

    /// The name sessions are kept by on a device: the username, or a
    /// portable user's id.
    pub fn username(&self) -> String {
        match self {
            Who::Local(user) => user.username.clone(),
            Who::Portable(user) => user.user.to_string(),
        }
    }

    /// The session was opened with a one-time password: it may only
    /// change the password, or sign out. Never a portable user's.
    pub fn password_change_only(&self) -> bool {
        matches!(self, Who::Local(user) if user.password_change_only)
    }

    pub fn display_name(&self) -> &str {
        match self {
            Who::Local(user) => &user.display_name,
            Who::Portable(user) => &user.display_name,
        }
    }

    /// What the logs call them: the username, or `portable:` and the
    /// identity's fingerprint.
    pub fn log_name(&self) -> String {
        match self {
            Who::Local(user) => user.username.clone(),
            Who::Portable(user) => format!("portable:{}", user.user.fingerprint()),
        }
    }

    /// Whether this device asks for their PIN: never for a portable user,
    /// who has none here.
    pub fn pin_required(&self) -> bool {
        match self {
            Who::Local(user) => user.pin_required,
            Who::Portable(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_principal_reads_back_from_what_it_writes() {
        let local = Principal::LocalUser(LocalUserId::from_uuid(uuid::Uuid::from_u128(7)));
        let portable = Principal::PortableUser(PortableUserId::from_bytes([9; 32]).unwrap());
        let remote = Principal::RemoteUser {
            server: RemoteServerId::from(&ServerId::parse("mzxw6ytbojtg633c").unwrap()),
            user: uuid::Uuid::from_u128(7),
        };
        for principal in [local, portable, remote] {
            assert_eq!(principal.to_string().parse::<Principal>(), Ok(principal));
        }
        assert_eq!(
            remote.to_string(),
            "remote:mzxw6ytbojtg633c:00000000-0000-0000-0000-000000000007"
        );
        for text in [
            "local:",
            "local:7",
            "portable:ab",
            "remote:x",
            "remote:mzxw6ytbojtg633c",
            "remote:MZXW6YTBOJTG633C:00000000-0000-0000-0000-000000000007",
            "remote:mzxw6ytbojtg633c:7",
            "",
        ] {
            assert_eq!(text.parse::<Principal>(), Err(InvalidPrincipal), "{text}");
        }
    }
}
