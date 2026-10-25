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
//! A redemption registers its session before it uses its token up, and
//! opens it after only if nothing ended it meanwhile
//! ([`RemoteSessions::register`]): a revocation that commits just after the
//! token is used up finds the session to end, and the download sessions
//! a mint counts include it from before its token stops being counted
//! unused (the security review's F8-02 and F8-05).
//!
//! Play token sessions are bounded (F8-06): a pair holds at most its
//! setting's worth at once, and the server [`MAX_PLAY_SESSIONS`], those
//! being redeemed among them; and one with no playback open ends
//! [`PLAY_MAX_AGE`] after it opened, however it is used
//! ([`RemoteSessions::end_unplayed`]). A session that plays lives while
//! it is used, however long the film.
//!
//! A download token's session (P8-07) reaches one item's download alone,
//! and lives as long as its file is kept after a read
//! ([`DOWNLOAD_IDLE`]): a download is made and read over hours. Its
//! download's cancelling ends it too, and its use never keeps it past
//! [`DOWNLOAD_MAX_AGE`]: a session the user's server forgot to end holds a
//! place under the download token caps no longer than that.

use std::collections::{HashMap, HashSet};
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

/// How long a play token's session lives at the most with no playback
/// open (F8-06): a play starts moments after its token is redeemed, so
/// one past this with none is a client holding it, not playing. Once it
/// is past, the next check with no playback open ends it.
pub const PLAY_MAX_AGE: Duration = Duration::from_secs(60 * 60);

/// The most play token sessions this server holds at once, every pair's
/// together (F8-06): each pair is held to its own setting
/// (`federation.play_sessions_per_server`) too, and this bounds them all.
pub const MAX_PLAY_SESSIONS: u64 = 10_000;

/// The most play token sessions open at once: of one pair, and in all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayRoom {
    pub per_peer: u64,
    pub total: u64,
}

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

/// What a request a remote session allows reaches
/// ([`RemoteSessionUser::allows`]): what the check that its item is still
/// where its token placed it (F8-04) is held to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// Its one item, by id: a start, or a change of its subtitles'
    /// timing. Served only while the item is in the library the token was
    /// for, checked as each is served (X8-03): a timing change of an item
    /// that left its library is a change for everyone who still has it.
    Item,
    /// A playback it started, a download it prepared, or the session
    /// itself: begun while the item was placed, and checked where each is
    /// served. A playback already running finishes after its item moves,
    /// as agreed; a download's reads are checked at each read (C8-03).
    Held,
}

impl RemoteSessionUser {
    pub fn principal(&self) -> Principal {
        Principal::RemoteUser {
            server: self.server,
            user: self.user,
        }
    }

    /// What the session may ask `request` for, if it may at all; anything
    /// else is `Unauthenticated`, as for no session. A play token's
    /// (P8-04): `StartPlayback` of its one item, the controls and reads of
    /// its own playback (which only the session that started a playback
    /// may use, checked where each is served), and a subtitle's timing on
    /// that item (P8-07). A download token's (P8-07): its item's
    /// `DownloadOptions` and `PrepareDownload` (not a device's, whose
    /// token is of one download to read), and `DownloadStatus` and
    /// `CancelDownload` of the downloads it
    /// [reaches](RemoteSessionUser::reaches_download). Either may
    /// `Logout`: a paired server ends a session so.
    ///
    /// What names the item is [`Reach::Item`], held to where the item is
    /// now; what names a playback, a download or the session itself is
    /// [`Reach::Held`]. A request added here later is one or the other,
    /// never checked less than the item's.
    pub fn allows(&self, request: &Request) -> Option<Reach> {
        match self.scope {
            TokenScope::Play => match request {
                Request::StartPlayback { id, .. }
                | Request::SyncSubtitle { id, .. }
                | Request::UseOriginalSubtitleTiming { id, .. }
                | Request::SetSubtitleOffset { id, .. } => {
                    (*id == self.item).then_some(Reach::Item)
                }
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
                | Request::Logout => Some(Reach::Held),
                _ => None,
            },
            TokenScope::Download => match request {
                Request::DownloadOptions { id, .. } | Request::PrepareDownload { id, .. } => {
                    (*id == self.item && self.download.is_none()).then_some(Reach::Item)
                }
                Request::DownloadStatus { .. }
                | Request::CancelDownload { .. }
                | Request::Logout => Some(Reach::Held),
                _ => None,
            },
            TokenScope::Unknown => None,
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
    /// When it was registered: a play token's with no playback ends
    /// [`PLAY_MAX_AGE`] after.
    opened: Instant,
    /// Registered for a redemption still using its token up: counted and
    /// ended as an open session is, but no one's to use yet.
    pending: bool,
}

impl Open {
    /// Used at `now`: kept a lifetime more, but never past when it ends.
    fn touch(&mut self, now: Instant) {
        self.until = now + self.lifetime;
        if let Some(ends) = self.ends {
            self.until = self.until.min(ends);
        }
    }
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

    /// Keep only the sessions `keep` says, by hash.
    fn retain(&mut self, keep: impl Fn(&str, &Open) -> bool) {
        self.by_token.retain(|h, o| keep(h, o));
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

    /// Register a session for `user` on `endpoint` (hex), which no one
    /// can use until it is [opened](Pending::open): counted, and ended as
    /// an open session is, from now. Dropped unopened, it goes. A download
    /// token's lives [`DOWNLOAD_IDLE`] past each use, a play token's this
    /// store's lifetime. `None` for a play token's past `room`: its pair,
    /// or this server, holds as many as it may (F8-06). A download
    /// token's is held to the download token limits as it is minted.
    pub fn register(
        &self,
        user: RemoteSessionUser,
        endpoint: &str,
        room: PlayRoom,
    ) -> Option<Pending<'_>> {
        let (lifetime, max_age) = match user.scope {
            TokenScope::Download => (DOWNLOAD_IDLE.max(self.lifetime), Some(DOWNLOAD_MAX_AGE)),
            _ => (self.lifetime, None),
        };
        let token = SessionToken::generate();
        let now = self.clock.now();
        let mut open = self.open.lock();
        open.retain(|_, o| o.until > now);
        if user.scope == TokenScope::Play {
            let (of_peer, total) = open
                .by_token
                .values()
                .filter(|o| o.user.scope == TokenScope::Play)
                .fold((0, 0), |(of_peer, total), o| {
                    (of_peer + u64::from(o.user.peer == user.peer), total + 1)
                });
            if of_peer >= room.per_peer || total >= room.total {
                return None;
            }
        }
        open.insert(
            token.hash(),
            Open {
                user,
                endpoint: endpoint.to_owned(),
                until: now + lifetime,
                lifetime,
                ends: max_age.map(|age| now + age),
                opened: now,
                pending: true,
            },
        );
        Some(Pending {
            sessions: self,
            token: Some(token),
        })
    }

    /// The user of session `token`, if it is live and bound to `endpoint`:
    /// kept a lifetime more.
    pub fn who(&self, token: &str, endpoint: &str) -> Option<RemoteSessionUser> {
        let hash = SessionToken(token.to_owned()).hash();
        let now = self.clock.now();
        let mut open = self.open.lock();
        let found = open.by_token.get_mut(&hash).filter(|o| !o.pending)?;
        if found.until <= now {
            open.retain(|_, o| o.until > now);
            return None;
        }
        if found.endpoint != endpoint {
            return None;
        }
        found.touch(now);
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
            .filter(|o| o.until > now && !o.pending)
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
                .any(|o| o.until > now && !o.pending)
        })
    }

    /// The pair of a live session `endpoint` (hex) holds, the first by its
    /// row if it holds several: what its connections are counted under
    /// (C8-04).
    pub fn endpoint_pair(&self, endpoint: &str) -> Option<String> {
        let now = self.clock.now();
        let open = self.open.lock();
        open.by_endpoint
            .get(endpoint)?
            .iter()
            .filter_map(|h| open.by_token.get(h))
            .filter(|o| o.until > now && !o.pending)
            .map(|o| &o.user.peer)
            .min()
            .cloned()
    }

    /// End session `token`: whether it was open.
    pub fn end(&self, token: &str) -> bool {
        let hash = SessionToken(token.to_owned()).hash();
        self.open.lock().remove(&hash)
    }

    /// How many live sessions `counted` says of, those registered and not
    /// open yet among them.
    pub fn count(&self, counted: impl Fn(&RemoteSessionUser) -> bool) -> u64 {
        let now = self.clock.now();
        self.open
            .lock()
            .by_token
            .values()
            .filter(|o| o.until > now && counted(&o.user))
            .count() as u64
    }

    /// Two counts of the live sessions in one walk: how many `counted`
    /// says the first of, and how many the second.
    pub fn count_both(&self, counted: impl Fn(&RemoteSessionUser) -> (bool, bool)) -> (u64, u64) {
        let now = self.clock.now();
        self.open
            .lock()
            .by_token
            .values()
            .filter(|o| o.until > now)
            .fold((0, 0), |(first, second), o| {
                let (a, b) = counted(&o.user);
                (first + u64::from(a), second + u64::from(b))
            })
    }

    /// The pair, item and library of every session, each once: what a
    /// catalog change checks again (F8-04).
    pub fn places(&self) -> HashSet<(String, MediaId, String)> {
        self.open
            .lock()
            .by_token
            .values()
            .map(|o| {
                (
                    o.user.peer.clone(),
                    o.user.item.clone(),
                    o.user.library.clone(),
                )
            })
            .collect()
    }

    /// End every session whose user `ended` says: how many ended.
    pub fn end_where(&self, ended: impl Fn(&RemoteSessionUser) -> bool) -> usize {
        self.end_matching(|_, o| !ended(&o.user))
    }

    /// End every session but those `keep` says of, by hash and session:
    /// how many ended.
    fn end_matching(&self, keep: impl Fn(&str, &Open) -> bool) -> usize {
        let mut open = self.open.lock();
        let before = open.by_token.len();
        open.retain(keep);
        before - open.by_token.len()
    }

    /// End every play token session opened [`PLAY_MAX_AGE`] ago or more
    /// with no playback open, which `playing` says by the session's hash
    /// (F8-06): how many ended. One that plays lives on while it is used.
    pub fn end_unplayed(&self, playing: impl Fn(&str) -> bool) -> usize {
        let now = self.clock.now();
        self.end_matching(|hash, o| {
            o.user.scope != TokenScope::Play
                || o.pending
                || now.duration_since(o.opened) < PLAY_MAX_AGE
                || playing(hash)
        })
    }

    /// End every session bound to `endpoint` (hex) whose user `ended`
    /// says: how many ended.
    pub fn end_bound_to(
        &self,
        endpoint: &str,
        ended: impl Fn(&RemoteSessionUser) -> bool,
    ) -> usize {
        self.end_matching(|_, o| o.endpoint != endpoint || !ended(&o.user))
    }
}

/// A session [registered](RemoteSessions::register) for a redemption,
/// not open yet.
#[derive(Debug)]
pub struct Pending<'a> {
    sessions: &'a RemoteSessions,
    /// Taken as it opens.
    token: Option<SessionToken>,
}

impl Pending<'_> {
    /// Open the session as `user`, the redeemed token's: its bearer, kept
    /// a lifetime from now. `None` if it was ended since it was
    /// registered: its grant revoked, its pair unpaired or moved to a new
    /// key.
    pub fn open(mut self, user: RemoteSessionUser) -> Option<SessionToken> {
        let token = self.token.take()?;
        let now = self.sessions.clock.now();
        let mut open = self.sessions.open.lock();
        let found = open
            .by_token
            .get_mut(&token.hash())
            .filter(|o| o.pending && o.until > now)?;
        found.user = user;
        found.pending = false;
        found.until = now + found.lifetime;
        if let Some(ends) = found.ends {
            found.until = found.until.min(ends);
        }
        Some(token)
    }
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            self.sessions.open.lock().remove(&token.hash());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOMY: PlayRoom = PlayRoom {
        per_peer: 100,
        total: 100,
    };

    /// A session for `user` on `endpoint`, registered and opened.
    fn open(sessions: &RemoteSessions, user: RemoteSessionUser, endpoint: &str) -> SessionToken {
        sessions
            .register(user.clone(), endpoint, ROOMY)
            .unwrap()
            .open(user)
            .unwrap()
    }

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
        let token = open(&sessions, user("p", "plex:1"), "ab");
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
        let one = open(&sessions, user("p", "plex:1"), "ab");
        let two = open(&sessions, user("p", "plex:2"), "ab");
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
        let play = open(&sessions, user("p", "plex:1"), "ab");
        let download = open(
            &sessions,
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
        let download = open(
            &sessions,
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
        let one = open(&sessions, user("p", "plex:1"), "ab");
        let two = open(&sessions, user("p", "plex:1"), "ab");
        assert!(sessions.end(&one.0));
        assert!(!sessions.end(&one.0), "ended once");
        assert!(sessions.owner(&one.0).is_none());
        assert!(sessions.owner(&two.0).is_some());
        assert!(sessions.endpoint_has("ab"));
    }

    /// A registered session is counted, and ended as an open one is, but
    /// is no one's until it opens: ended meanwhile, it opens no more;
    /// dropped unopened, it goes.
    #[test]
    fn a_registered_session_is_counted_and_ended_but_used_by_no_one() {
        let sessions = RemoteSessions::new(Clock::default(), Duration::from_secs(60));
        let pending = sessions.register(user("p", "plex:1"), "ab", ROOMY).unwrap();
        assert_eq!(sessions.count(|_| true), 1);
        assert!(!sessions.endpoint_has("ab"), "no one's yet");
        assert_eq!(sessions.end_where(|u| u.peer == "p"), 1);
        assert!(pending.open(user("p", "plex:1")).is_none());
        drop(sessions.register(user("p", "plex:1"), "ab", ROOMY));
        assert_eq!(sessions.count(|_| true), 0);
        let token = open(&sessions, user("p", "plex:1"), "ab");
        assert!(sessions.who(&token.0, "ab").is_some());
    }

    /// A key moved on ends the sessions bound to the old one alone.
    #[test]
    fn ending_an_endpoint_s_sessions_leaves_the_others() {
        let sessions = RemoteSessions::new(Clock::default(), Duration::from_secs(60));
        let old = open(&sessions, user("p", "plex:1"), "ab");
        let device = open(&sessions, user("p", "plex:1"), "cd");
        let other = open(&sessions, user("q", "plex:1"), "ab");
        assert_eq!(sessions.end_bound_to("ab", |u| u.peer == "p"), 1);
        assert!(sessions.owner(&old.0).is_none());
        assert!(sessions.owner(&device.0).is_some());
        assert!(sessions.owner(&other.0).is_some());
    }

    /// A pair holds at most its room's play sessions, those registered
    /// among them, and the server its total; a download session is not
    /// a play's, and an ended one makes room (F8-06).
    #[test]
    fn play_sessions_are_held_to_their_room() {
        let sessions = RemoteSessions::new(Clock::default(), Duration::from_secs(60));
        let room = PlayRoom {
            per_peer: 2,
            total: 3,
        };
        let register = |peer: &str| {
            sessions
                .register(user(peer, "plex:1"), "ab", room)
                .map(|pending| pending.open(user(peer, "plex:1")).unwrap())
        };
        let first = register("p").unwrap();
        let pending = sessions.register(user("p", "plex:1"), "ab", room).unwrap();
        assert!(register("p").is_none(), "the pair's two, one registered");
        let download = RemoteSessionUser {
            scope: TokenScope::Download,
            ..user("p", "plex:1")
        };
        assert!(sessions.register(download, "ab", room).is_some());
        assert!(register("q").is_some(), "another pair's");
        assert!(register("r").is_none(), "the server's three");
        drop(pending);
        assert!(sessions.end(&first.0));
        assert!(register("p").is_some());
    }

    /// A play session with no playback ends [`PLAY_MAX_AGE`] after it
    /// opened, however often it is used; one that plays lives on while
    /// it is used, and a younger one, and a download's, are left
    /// (F8-06).
    #[test]
    fn a_play_session_with_no_playback_ends_at_its_most() {
        let clock = Clock::default();
        let sessions = RemoteSessions::new(clock.clone(), IDLE);
        let idle = open(&sessions, user("p", "plex:1"), "ab");
        let playing = open(&sessions, user("p", "plex:1"), "ab");
        let download = open(
            &sessions,
            RemoteSessionUser {
                scope: TokenScope::Download,
                ..user("p", "plex:1")
            },
            "ab",
        );
        let step = IDLE - Duration::from_secs(1);
        let mut used = Duration::ZERO;
        while used < PLAY_MAX_AGE {
            clock.advance(step);
            used += step;
            for token in [&idle, &playing, &download] {
                assert!(sessions.who(&token.0, "ab").is_some(), "{used:?}");
            }
        }
        let young = open(&sessions, user("p", "plex:1"), "ab");
        let plays = playing.hash();
        assert_eq!(sessions.end_unplayed(|hash| hash == plays), 1);
        assert!(sessions.who(&idle.0, "ab").is_none());
        for token in [&playing, &download, &young] {
            assert!(sessions.who(&token.0, "ab").is_some());
        }
    }
}
