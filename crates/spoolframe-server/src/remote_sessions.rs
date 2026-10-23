//! The sessions a paired server's play token opens (P8-04): one of the
//! pair's users, on one device, for one item's playback and nothing else.
//!
//! A token (P8-03) is redeemed once (`RedeemPlayToken`); what it opens is a
//! session like a sign-in's, bound to the device's endpoint, whose user is
//! `Principal::RemoteUser`. It is kept in memory only: a restart of this
//! server ends every one, with the playbacks they hold, and the device goes
//! back to its home server for a new token. Each use keeps it a play
//! token's lifetime more; a revoked grant, or the pair unpaired, ends it at
//! once ([`RemoteSessions::end_where`]).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use spoolframe_protocol::federation::TokenScope;
use spoolframe_types::MediaId;

use crate::auth::SessionToken;
use crate::clock::Clock;
use crate::principal::{Principal, RemoteServerId};

/// How long a session is kept past its last use: a play token's lifetime.
/// A paused player reports once a minute, which keeps it.
pub const IDLE: Duration = Duration::from_secs(5 * 60);

/// Who a play token's session acts as, and what it may reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSessionUser {
    /// The paired server the user is at home on.
    pub server: RemoteServerId,
    /// The user's stable id there.
    pub user: uuid::Uuid,
    /// What their server shows them as: for the logs and the Playback
    /// page, never an admin list of users.
    pub display_name: String,
    /// The pair's row here.
    pub peer: String,
    /// The shared library the item is in: a revoked grant ends the session.
    pub library: String,
    /// The one item it opens.
    pub item: MediaId,
    pub scope: TokenScope,
    /// Redeemed by the pair's own endpoint, which plays on to the device
    /// (proxy mode), rather than by the device.
    pub proxied: bool,
    /// The user's track preferences on their own server, as its assertion
    /// gave them: what a play with no selection chooses tracks by.
    pub preferences: Option<spoolframe_types::TrackPreferences>,
}

impl RemoteSessionUser {
    pub fn principal(&self) -> Principal {
        Principal::RemoteUser {
            server: self.server,
            user: self.user,
        }
    }
}

#[derive(Debug)]
struct Open {
    user: RemoteSessionUser,
    /// The endpoint it is bound to, hex.
    endpoint: String,
    until: Instant,
}

/// Every play token's session, by its token's hash.
#[derive(Debug)]
pub struct RemoteSessions {
    clock: Clock,
    lifetime: Duration,
    open: parking_lot::Mutex<Kept>,
}

/// The sessions, and their hashes by the endpoint each is bound to: what
/// the relay asks of every endpoint it carries, without a walk of all.
#[derive(Debug, Default)]
struct Kept {
    by_token: HashMap<String, Open>,
    by_endpoint: HashMap<String, Vec<String>>,
}

impl Kept {
    fn insert(&mut self, hash: String, open: Open) {
        self.by_endpoint
            .entry(open.endpoint.clone())
            .or_default()
            .push(hash.clone());
        self.by_token.insert(hash, open);
    }

    /// End session `hash`: whether it was open.
    fn remove(&mut self, hash: &str) -> bool {
        let Some(open) = self.by_token.remove(hash) else {
            return false;
        };
        if let Some(hashes) = self.by_endpoint.get_mut(&open.endpoint) {
            hashes.retain(|h| h != hash);
            if hashes.is_empty() {
                self.by_endpoint.remove(&open.endpoint);
            }
        }
        true
    }

    /// Keep only the sessions `keep` says.
    fn retain(&mut self, keep: impl Fn(&Open) -> bool) {
        self.by_token.retain(|_, o| keep(o));
        let by_token = &self.by_token;
        self.by_endpoint.retain(|_, hashes| {
            hashes.retain(|h| by_token.contains_key(h));
            !hashes.is_empty()
        });
    }
}

impl RemoteSessions {
    /// Sessions on `clock`, each kept `lifetime` past its last use.
    pub fn new(clock: Clock, lifetime: Duration) -> Self {
        Self {
            clock,
            lifetime,
            open: parking_lot::Mutex::default(),
        }
    }

    /// Open a session for `user` on `endpoint` (hex): its bearer.
    pub fn open(&self, user: RemoteSessionUser, endpoint: &str) -> SessionToken {
        let token = SessionToken::generate();
        let now = self.clock.now();
        let mut open = self.open.lock();
        open.retain(|o| o.until > now);
        open.insert(
            token.hash(),
            Open {
                user,
                endpoint: endpoint.to_owned(),
                until: now + self.lifetime,
            },
        );
        token
    }

    /// The user of session `token`, if it is live and bound to `endpoint`:
    /// kept a lifetime more.
    pub fn who(&self, token: &str, endpoint: &str) -> Option<RemoteSessionUser> {
        let hash = SessionToken(token.to_owned()).hash();
        let now = self.clock.now();
        let mut open = self.open.lock();
        let found = open.by_token.get_mut(&hash)?;
        if found.until <= now {
            open.retain(|o| o.until > now);
            return None;
        }
        if found.endpoint != endpoint {
            return None;
        }
        found.until = now + self.lifetime;
        Some(found.user.clone())
    }

    /// Whose session `token` is, and the endpoint it is bound to, while it
    /// is live.
    pub fn owner(&self, token: &str) -> Option<(RemoteSessionUser, String)> {
        let hash = SessionToken(token.to_owned()).hash();
        let now = self.clock.now();
        self.open
            .lock()
            .by_token
            .get(&hash)
            .filter(|o| o.until > now)
            .map(|o| (o.user.clone(), o.endpoint.clone()))
    }

    /// Whether `endpoint` (hex) holds a live session.
    pub fn endpoint_has(&self, endpoint: &str) -> bool {
        let now = self.clock.now();
        let open = self.open.lock();
        open.by_endpoint.get(endpoint).is_some_and(|hashes| {
            hashes
                .iter()
                .filter_map(|h| open.by_token.get(h))
                .any(|o| o.until > now)
        })
    }

    /// End session `token`: whether it was open.
    pub fn end(&self, token: &str) -> bool {
        let hash = SessionToken(token.to_owned()).hash();
        self.open.lock().remove(&hash)
    }

    /// End every session whose user `ended` says: how many ended.
    pub fn end_where(&self, ended: impl Fn(&RemoteSessionUser) -> bool) -> usize {
        let mut open = self.open.lock();
        let before = open.by_token.len();
        open.retain(|o| !ended(&o.user));
        before - open.by_token.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(peer: &str, library: &str) -> RemoteSessionUser {
        RemoteSessionUser {
            server: RemoteServerId::from(
                &spoolframe_protocol::ServerId::parse("mzxw6ytbojtg633c").unwrap(),
            ),
            user: uuid::Uuid::from_u128(7),
            display_name: "Bob".into(),
            peer: peer.into(),
            library: library.into(),
            item: "plex:movie:1".parse().unwrap(),
            scope: TokenScope::Play,
            proxied: false,
            preferences: None,
        }
    }

    /// A session works from its endpoint alone, a lifetime past its last
    /// use, and not after.
    #[test]
    fn a_session_works_from_its_device_while_it_is_used() {
        let clock = Clock::default();
        let sessions = RemoteSessions::new(clock.clone(), Duration::from_secs(60));
        let token = sessions.open(user("p", "plex:1"), "ab");
        assert!(sessions.who(&token.0, "cd").is_none(), "another device");
        clock.advance(Duration::from_secs(50));
        assert!(sessions.who(&token.0, "ab").is_some());
        clock.advance(Duration::from_secs(50));
        assert!(sessions.who(&token.0, "ab").is_some(), "kept by its use");
        assert!(sessions.endpoint_has("ab"));
        clock.advance(Duration::from_secs(61));
        assert!(sessions.who(&token.0, "ab").is_none());
        assert!(!sessions.endpoint_has("ab"));
    }

    /// A revoked grant ends its library's sessions alone.
    #[test]
    fn ending_a_library_s_sessions_leaves_the_others() {
        let sessions = RemoteSessions::new(Clock::default(), Duration::from_secs(60));
        let one = sessions.open(user("p", "plex:1"), "ab");
        let two = sessions.open(user("p", "plex:2"), "ab");
        assert_eq!(sessions.end_where(|u| u.library == "plex:1"), 1);
        assert!(sessions.owner(&one.0).is_none());
        assert!(sessions.owner(&two.0).is_some());
    }

    /// A paired server ends a play it proxies by ending its session: that
    /// one alone, its device's others kept.
    #[test]
    fn ending_a_session_leaves_its_device_s_others() {
        let sessions = RemoteSessions::new(Clock::default(), Duration::from_secs(60));
        let one = sessions.open(user("p", "plex:1"), "ab");
        let two = sessions.open(user("p", "plex:1"), "ab");
        assert!(sessions.end(&one.0));
        assert!(!sessions.end(&one.0), "ended once");
        assert!(sessions.owner(&one.0).is_none());
        assert!(sessions.owner(&two.0).is_some());
        assert!(sessions.endpoint_has("ab"));
    }
}
