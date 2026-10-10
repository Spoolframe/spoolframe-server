//! Who state belongs to (ARCHITECTURE §55). Server-side only: clients
//! learn who they are from `WhoAmI`.

use std::fmt;

use spoolframe_protocol::PortableUserId;

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
}

impl fmt::Display for Principal {
    /// The key it is stored under, and `WhoAmI`'s `user_id`:
    /// `local:0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4c` (P3-20), or
    /// `portable:` and the identity's 64 hex digits.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Principal::LocalUser(id) => write!(f, "local:{id}"),
            Principal::PortableUser(id) => write!(f, "portable:{id}"),
        }
    }
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
