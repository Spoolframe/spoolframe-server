//! A user's role on a server (P2-09): what clients and the web UI show and
//! switch on.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Manages other users and the server.
    Admin,
    /// Watches, and manages only their own account.
    #[default]
    Consumer,
    /// A role this build does not know, from a newer server. Treat it as
    /// the least it could be: no more than a consumer.
    #[serde(other)]
    Unknown,
}

impl Role {
    /// The name on the wire and in the server's database.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Consumer => "consumer",
            Role::Unknown => "unknown",
        }
    }

    pub fn is_admin(self) -> bool {
        self == Role::Admin
    }
}

impl std::str::FromStr for Role {
    type Err = String;

    /// `admin` or `consumer`, as the CLI takes them.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "admin" => Ok(Role::Admin),
            "consumer" => Ok(Role::Consumer),
            other => Err(format!("{other:?} is not a role: use admin or consumer")),
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_role_from_a_newer_server_is_unknown() {
        let role: Role = serde_json::from_str("\"moderator\"").unwrap();
        assert_eq!(role, Role::Unknown);
        assert!(!role.is_admin());
    }

    #[test]
    fn roles_read_as_the_cli_writes_them() {
        assert_eq!("Admin".parse::<Role>(), Ok(Role::Admin));
        assert_eq!("consumer".parse::<Role>(), Ok(Role::Consumer));
        assert!("unknown".parse::<Role>().is_err());
    }
}
