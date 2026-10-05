//! Who state belongs to (ARCHITECTURE §55). Server-side only: clients
//! learn who they are from `WhoAmI`.

use std::fmt;

use crate::store::users::LocalUserId;

/// The owner of watch state and, later, other per-person data. Only local
/// users exist in Phase 1; portable users (P8) and users of other servers
/// (P7) arrive as new variants, which is why every table and function is
/// keyed on this rather than on a `LocalUserId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Principal {
    LocalUser(LocalUserId),
}

impl fmt::Display for Principal {
    /// The key it is stored under, and `WhoAmI`'s `user_id`:
    /// `local:0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4c` (P3-20).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Principal::LocalUser(id) => write!(f, "local:{id}"),
        }
    }
}
