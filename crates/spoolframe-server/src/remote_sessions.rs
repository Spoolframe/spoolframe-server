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
//!
//! A download token's session (P8-07) reaches one item's download alone,
//! and lives as long as its file is kept after a read
//! ([`DOWNLOAD_IDLE`]): a download is made and read over hours. Its
//! download's cancelling ends it too, and its use never keeps it past
//! [`DOWNLOAD_MAX_AGE`]: a session the user's server forgot to end holds a
//! place under the download token caps no longer than that.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use spoolframe_protocol::Request;
use spoolframe_protocol::federation::TokenScope;
use spoolframe_types::MediaId;

use crate::auth::SessionToken;
use crate::clock::Clock;
use crate::principal::{Principal, RemoteServerId};

/// How long a session is kept past its last use: a play token's lifetime.
/// A paused player reports once a minute, which keeps it.
pub const IDLE: Duration = Duration::from_secs(5 * 60);

/// How long a download token's session (P8-07) is kept past its last use:
/// as long as the download's file is kept past its last read.
pub const DOWNLOAD_IDLE: Duration = crate::downloads::KEPT_AFTER_READ;

/// How long a download token's session (P8-07) lives at the most, used or
/// not: a film is made and read well within it, and the user's server
/// opens another when it needs one.
pub const DOWNLOAD_MAX_AGE: Duration = Duration::from_secs(3 * 24 * 60 * 60);

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
    /// A download token's one download, by this server's id (P8-07): what
    /// a device's own token reads. `None` for a play token, and for the
    /// token the user's server prepares the download with.
    pub download: Option<String>,
}

impl RemoteSessionUser {
    pub fn principal(&self) -> Principal {
        Principal::RemoteUser {
            server: self.server,
            user: self.user,
        }
    }

    /// Whether the session may ask `request` at all; anything else is
    /// `Unauthenticated`, as for no session. A play token's (P8-04):
    /// `StartPlayback` of its one item, the controls and reads of its own
    /// playback (which only the session that started a playback may use,
    /// checked where each is served), and a subtitle's timing on that item
    /// (P8-07). A download token's (P8-07): its item's `DownloadOptions`
    /// and `PrepareDownload` (not a device's, whose token is of one
    /// download to read), and `DownloadStatus` and `CancelDownload` of the
    /// downloads it [reaches](RemoteSessionUser::reaches_download). Either
    /// may `Logout`: a paired server ends a session so.
    pub fn allows(&self, request: &Request) -> bool {
        match self.scope {
            TokenScope::Play => match request {
                Request::StartPlayback { id, .. }
                | Request::SyncSubtitle { id, .. }
                | Request::UseOriginalSubtitleTiming { id, .. }
                | Request::SetSubtitleOffset { id, .. } => *id == self.item,
                Request::UpdatePlaybackProgress { .. }
                | Request::ChangeSubtitle { .. }
                | Request::ChangeQuality { .. }
                | Request::SeekPlayback { .. }
                | Request::StopPlayback { .. }
                | Request::PlaybackProgressSequence { .. }
                | Request::PlaybackRevision { .. }
                | Request::GetManifest { .. }
                | Request::ReadSubtitle { .. }
                | Request::ReadSubtitleSegment { .. }
                | Request::Logout => true,
                _ => false,
            },
            TokenScope::Download => match request {
                Request::DownloadOptions { id, .. } | Request::PrepareDownload { id, .. } => {
                    *id == self.item && self.download.is_none()
                }
                Request::DownloadStatus { .. }
                | Request::CancelDownload { .. }
                | Request::Logout => true,
                _ => false,
            },
            TokenScope::Unknown => false,
        }
    }

    /// Whether the session reaches download `id`, of item `media_id`
    /// (P8-07): a download token's reaches its item's, and only its one
    /// download when it names one. A play token's reaches none.
    pub fn reaches_download(&self, id: &str, media_id: &str) -> bool {
        self.scope == TokenScope::Download
            && media_id == self.item.to_string()
            && self.download.as_deref().is_none_or(|one| one == id)
    }
}

#[derive(Debug)]
struct Open {
    user: RemoteSessionUser,
    /// The endpoint it is bound to, hex.
    endpoint: String,
    until: Instant,
    /// How long each use keeps it.
    lifetime: Duration,
    /// When it ends however it is used; `None` for a play token's.
    ends: Option<Instant>,
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

    /// Open a session for `user` on `endpoint` (hex): its bearer. A
    /// download token's lives [`DOWNLOAD_IDLE`] past each use, a play
    /// token's this store's lifetime.
    pub fn open(&self, user: RemoteSessionUser, endpoint: &str) -> SessionToken {
        let (lifetime, max_age) = match user.scope {
            TokenScope::Download => (DOWNLOAD_IDLE.max(self.lifetime), Some(DOWNLOAD_MAX_AGE)),
            _ => (self.lifetime, None),
        };
        let token = SessionToken::generate();
        let now = self.clock.now();
        let mut open = self.open.lock();
        open.retain(|o| o.until > now);
        open.insert(
            token.hash(),
            Open {
                user,
                endpoint: endpoint.to_owned(),
                until: now + lifetime,
                lifetime,
                ends: max_age.map(|age| now + age),
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
        found.until = now + found.lifetime;
        if let Some(ends) = found.ends {
            found.until = found.until.min(ends);
        }
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

    /// How many live sessions `counted` says of.
    pub fn count(&self, counted: impl Fn(&RemoteSessionUser) -> bool) -> u64 {
        let now = self.clock.now();
        self.open
            .lock()
            .by_token
            .values()
            .filter(|o| o.until > now && counted(&o.user))
            .count() as u64
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
            download: None,
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

    /// A download token's session lives a day past its last use, not a
    /// play's minutes, and is counted among the download sessions.
    #[test]
    fn a_download_session_lives_while_its_file_is_kept() {
        let clock = Clock::default();
        let sessions = RemoteSessions::new(clock.clone(), Duration::from_secs(60));
        let play = sessions.open(user("p", "plex:1"), "ab");
        let download = sessions.open(
            RemoteSessionUser {
                scope: TokenScope::Download,
                ..user("p", "plex:1")
            },
            "ab",
        );
        assert_eq!(sessions.count(|u| u.scope == TokenScope::Download), 1);
        clock.advance(Duration::from_secs(61));
        assert!(sessions.who(&play.0, "ab").is_none(), "a play's minutes");
        assert!(sessions.who(&download.0, "ab").is_some());
        clock.advance(DOWNLOAD_IDLE - Duration::from_secs(1));
        assert!(sessions.who(&download.0, "ab").is_some(), "kept by its use");
        clock.advance(DOWNLOAD_IDLE);
        assert!(sessions.who(&download.0, "ab").is_none());
        assert_eq!(sessions.count(|u| u.scope == TokenScope::Download), 0);
    }

    /// However much a download token's session is used, it ends
    /// [`DOWNLOAD_MAX_AGE`] after it opened.
    #[test]
    fn a_download_session_ends_at_its_most_however_used() {
        let clock = Clock::default();
        let sessions = RemoteSessions::new(clock.clone(), Duration::from_secs(60));
        let download = sessions.open(
            RemoteSessionUser {
                scope: TokenScope::Download,
                ..user("p", "plex:1")
            },
            "ab",
        );
        let step = Duration::from_secs(60 * 60);
        let mut used = Duration::ZERO;
        while used + step < DOWNLOAD_MAX_AGE {
            clock.advance(step);
            used += step;
            assert!(sessions.who(&download.0, "ab").is_some(), "{used:?}");
        }
        clock.advance(DOWNLOAD_MAX_AGE - used);
        assert!(sessions.who(&download.0, "ab").is_none());
        assert_eq!(sessions.count(|u| u.scope == TokenScope::Download), 0);
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
