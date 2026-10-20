//! Who each connection acts as: the users of the sessions it made
//! requests with, by its stable id. A connection carries any number of
//! sessions (§15); each one is noted when a request with it is
//! authenticated, and let go when it ends (signed out, revoked, or the
//! user disabled) or the connection closes. Every feature that tells a
//! user's connections of something asks this one record, so a
//! connection signed out as alice and in as bob is told of bob alone
//! (review 2026-10-20, finding 6).

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use spoolframe_protocol::SessionId;

use crate::principal::Principal;

#[derive(Debug, Clone, Default)]
pub(crate) struct ConnectionUsers(Arc<Mutex<Users>>);

#[derive(Debug, Default)]
struct Users {
    /// Each connection's sessions, and whose each is.
    sessions: HashMap<usize, HashMap<String, Principal>>,
    /// The connections each user acts on: what is asked most.
    connections: HashMap<Principal, HashSet<usize>>,
}

impl Users {
    /// Connection `id` no longer acts as `principal` through one session;
    /// it still does if another of its sessions is theirs.
    fn let_go(&mut self, id: usize, principal: Principal) {
        let still = self
            .sessions
            .get(&id)
            .is_some_and(|sessions| sessions.values().any(|p| *p == principal));
        if still {
            return;
        }
        if let Some(ids) = self.connections.get_mut(&principal) {
            ids.remove(&id);
            if ids.is_empty() {
                self.connections.remove(&principal);
            }
        }
    }
}

impl ConnectionUsers {
    /// Connection `id` made a request with `session`, which is
    /// `principal`'s.
    pub(crate) fn note(&self, id: usize, session: &SessionId, principal: Principal) {
        let mut users = self.0.lock();
        let before = users
            .sessions
            .entry(id)
            .or_default()
            .insert(session.0.clone(), principal);
        users.connections.entry(principal).or_default().insert(id);
        if let Some(before) = before.filter(|before| *before != principal) {
            users.let_go(id, before);
        }
    }

    /// Let go of `session`, which has ended, on every connection.
    pub(crate) fn forget(&self, session: &str) {
        let mut users = self.0.lock();
        let gone: Vec<(usize, Principal)> = users
            .sessions
            .iter_mut()
            .filter_map(|(id, sessions)| sessions.remove(session).map(|p| (*id, p)))
            .collect();
        for (id, principal) in gone {
            users.let_go(id, principal);
        }
    }

    /// Connection `id` closed.
    pub(crate) fn remove(&self, id: usize) {
        let mut users = self.0.lock();
        let Some(sessions) = users.sessions.remove(&id) else {
            return;
        };
        for principal in sessions.into_values() {
            users.let_go(id, principal);
        }
    }

    /// The users connection `id` acts as.
    pub(crate) fn who(&self, id: usize) -> Vec<Principal> {
        let users = self.0.lock();
        let Some(sessions) = users.sessions.get(&id) else {
            return Vec::new();
        };
        let who: HashSet<Principal> = sessions.values().copied().collect();
        who.into_iter().collect()
    }

    /// The connections that act as `principal`.
    pub(crate) fn of(&self, principal: &Principal) -> Vec<usize> {
        self.0
            .lock()
            .connections
            .get(principal)
            .map(|ids| ids.iter().copied().collect())
            .unwrap_or_default()
    }

    /// The sessions noted, each once.
    fn sessions(&self) -> HashSet<String> {
        self.0
            .lock()
            .sessions
            .values()
            .flat_map(|sessions| sessions.keys().cloned())
            .collect()
    }
}

/// Let go of each session noted once it has ended: each time sessions may
/// have ended (revoked, a user disabled), and every
/// [`crate::admission::RECHECK`], until the store is gone. A session that
/// cannot be looked up is kept until the next look. Looked up one by one:
/// a session is an endpoint's or a portable user's, in two tables.
pub(crate) async fn forget_ended_sessions(auth: crate::auth::Auth, users: ConnectionUsers) {
    let mut ended = auth.store().sessions_ended();
    loop {
        tokio::select! {
            changed = ended.changed() => if changed.is_err() { return },
            () = tokio::time::sleep(crate::admission::RECHECK) => {}
        }
        for session in users.sessions() {
            match auth.session_owner(&session).await {
                Ok(Some(owner)) if owner.live => {}
                Ok(_) => users.forget(&session),
                Err(e) => tracing::warn!(error = %e, "cannot check a listening session"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn principal(n: u128) -> Principal {
        Principal::LocalUser(crate::store::users::LocalUserId::from_uuid(
            uuid::Uuid::from_u128(n),
        ))
    }

    fn session(n: u8) -> SessionId {
        SessionId(format!("session-{n}"))
    }

    fn sorted(mut ids: Vec<usize>) -> Vec<usize> {
        ids.sort_unstable();
        ids
    }

    /// Each user's connections, and none once they close.
    #[test]
    fn a_user_s_connections_are_those_with_a_session_of_theirs() {
        let users = ConnectionUsers::default();
        let (alice, bob) = (principal(1), principal(2));
        users.note(1, &session(1), alice);
        users.note(2, &session(2), bob);
        users.note(2, &session(3), alice);
        assert_eq!(sorted(users.of(&alice)), [1, 2]);
        users.remove(2);
        assert_eq!((users.of(&alice), users.of(&bob)), (vec![1], vec![]));
    }

    /// A connection signed out as alice and in as bob acts as bob alone.
    #[test]
    fn a_session_that_ended_is_let_go_on_its_connection() {
        let users = ConnectionUsers::default();
        let (alice, bob) = (principal(1), principal(2));
        users.note(1, &session(1), alice);
        users.forget(&session(1).0);
        users.note(1, &session(2), bob);
        assert_eq!(
            (users.of(&alice), users.of(&bob), users.who(1)),
            (vec![], vec![1], vec![bob])
        );
    }

    /// Two sessions of one user on a connection: it acts as them until
    /// both end.
    #[test]
    fn a_user_is_let_go_with_their_last_session_on_a_connection() {
        let users = ConnectionUsers::default();
        let alice = principal(1);
        users.note(1, &session(1), alice);
        users.note(1, &session(2), alice);
        users.forget(&session(1).0);
        assert_eq!(users.of(&alice), [1]);
        users.forget(&session(2).0);
        assert!(users.of(&alice).is_empty());
    }
}
