//! Who a playlist is or can be shared with, by name: the one list the admin API's
//! `me.playlistShareCandidates` and the client protocol's
//! `ListShareCandidates` both read, and the names a summary gives its
//! owner and its shares.

use std::collections::HashMap;

use spoolframe_protocol::playlists::{ShareCandidate, UserKind};

use super::PlaylistsError;
use crate::principal::Principal;
use crate::store::Store;
use crate::store::users::UserError;

/// Someone a playlist can name: an account here, or a portable user
/// granted access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    pub principal: Principal,
    /// An account's display name, else its username; a portable user's
    /// name, else their fingerprint.
    pub display_name: String,
    /// An account's username; a portable user has none here.
    pub username: Option<String>,
    /// A disabled account is still named on the shares it has, and is
    /// never offered for a new one.
    pub enabled: bool,
}

impl Person {
    pub fn kind(&self) -> UserKind {
        kind_of(self.principal)
    }

    /// Whether `query`, lowercased, is in their display name or username,
    /// in any case. An empty one is in everyone's.
    fn matches(&self, query: &str) -> bool {
        self.display_name.to_lowercase().contains(query)
            || self
                .username
                .as_ref()
                .is_some_and(|u| u.to_lowercase().contains(query))
    }

    pub fn candidate(&self) -> ShareCandidate {
        ShareCandidate {
            user_id: self.principal.to_string(),
            display_name: self.display_name.clone(),
            username: self.username.clone(),
            kind: self.kind(),
        }
    }
}

/// Whose account `principal` is.
pub fn kind_of(principal: Principal) -> UserKind {
    match principal {
        Principal::LocalUser(_) => UserKind::Local,
        Principal::PortableUser(_) => UserKind::Portable,
        // Never shared with (P8-03): `exists` says no such user here.
        Principal::RemoteUser { .. } => UserKind::Unknown,
    }
}

/// Everyone here: every account, disabled ones too, then every portable
/// user with a live grant.
pub async fn everyone(store: &Store) -> Result<Vec<Person>, PlaylistsError> {
    let mut people: Vec<Person> = store
        .list_users()
        .await
        .map_err(|e| match e {
            UserError::Store(e) => PlaylistsError::Store(e),
            other => PlaylistsError::InvalidInput(other.to_string()),
        })?
        .into_iter()
        .map(|u| Person {
            principal: Principal::LocalUser(u.id),
            display_name: u.display_name.unwrap_or_else(|| u.username.clone()),
            username: Some(u.username),
            enabled: u.disabled_at.is_none(),
        })
        .collect();
    people.extend(store.grants().await?.into_iter().map(|g| Person {
        principal: Principal::PortableUser(g.portable),
        display_name: g.shown_name(),
        username: None,
        enabled: true,
    }));
    Ok(people)
}

/// Whom `caller` can share a playlist with: every enabled account but
/// theirs, and every portable user granted access but them, by display
/// name in any case.
pub async fn share_candidates(
    store: &Store,
    caller: Principal,
) -> Result<Vec<Person>, PlaylistsError> {
    let mut candidates: Vec<Person> = everyone(store)
        .await?
        .into_iter()
        .filter(|p| p.enabled && p.principal != caller)
        .collect();
    candidates.sort_by_cached_key(|c| c.display_name.to_lowercase());
    Ok(candidates)
}

/// `candidates` whose display name or username holds `query`, in any
/// case; an empty `query` keeps everyone.
pub fn matching(candidates: Vec<Person>, query: &str) -> Vec<Person> {
    let query = query.trim().to_lowercase();
    candidates
        .into_iter()
        .filter(|p| p.matches(&query))
        .collect()
}

/// Names by principal, as stored (`local:…`, `portable:…`): what a
/// summary names its owner and shares by.
#[derive(Debug, Default)]
pub struct Names(HashMap<String, String>);

impl Names {
    pub async fn of(store: &Store) -> Result<Self, PlaylistsError> {
        Ok(Self(
            everyone(store)
                .await?
                .into_iter()
                .map(|p| (p.principal.to_string(), p.display_name))
                .collect(),
        ))
    }

    /// `principal`'s name; `None` for someone no longer here.
    pub fn of_principal(&self, principal: &str) -> Option<String> {
        self.0.get(principal).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::users::LocalUserId;

    fn person(name: &str, username: Option<&str>) -> Person {
        Person {
            principal: Principal::LocalUser(LocalUserId::generate()),
            display_name: name.into(),
            username: username.map(Into::into),
            enabled: true,
        }
    }

    #[test]
    fn a_query_matches_the_name_or_username_in_any_case() {
        let people = vec![
            person("Alice Liddell", Some("alice")),
            person("Bob", Some("rabbit")),
            person("Carol", None),
        ];
        let names = |found: Vec<Person>| -> Vec<String> {
            found.into_iter().map(|p| p.display_name).collect()
        };
        assert_eq!(names(matching(people.clone(), "LIDD")), ["Alice Liddell"]);
        assert_eq!(names(matching(people.clone(), "Rab")), ["Bob"]);
        assert_eq!(names(matching(people.clone(), " ")).len(), 3);
        assert_eq!(names(matching(people, "")).len(), 3);
    }
}
