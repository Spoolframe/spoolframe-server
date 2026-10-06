//! Bounded, process-local mutation outcomes. Pending work survives the
//! requesting stream; expired IDs and IDs from earlier processes are refused.
//!
//! Each pool is shared, so no one may take much of it (PX-10, finding 6):
//! an entry reserves what its kind of reply can be, not the largest frame,
//! and each endpoint and each user may hold only a share of the pool.

use spoolframe_protocol::rpc::{OPERATION_WINDOW_MS, Operation, RpcReply};
use spoolframe_protocol::{Error, Request, Response};
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::watch;

const MAX_OPERATIONS: usize = 4096;
const MAX_BYTES: usize = 64 << 20;
/// The largest outcome kept: one frame.
const MAX_OUTCOME: usize = spoolframe_protocol::codec::MAX_FRAME as usize;

/// What a reply that is `Done`, a session, a code or an error takes:
/// a few hundred bytes, with room for an error's message.
const SMALL_REPLY: usize = 16 << 10;

/// What a passkey ceremony's options take: a challenge, and the ids of
/// the user's credentials (about 100 bytes each).
const CEREMONY_REPLY: usize = 64 << 10;

/// One endpoint, or one user, may hold this share of a pool's entries and
/// bytes (1/8): it can fill the pool only with seven others.
const OWNER_SHARE: usize = 8;

/// How long an anonymous outcome is kept once it is known: long enough for
/// a client's retry after a lost reply, short enough that sign-in attempts
/// cannot hold the anonymous pool for an operation's whole 15 minutes.
const ANONYMOUS_KEPT_MS: u64 = 30_000;

/// What the outcome of `request` may take, reserved while it runs.
/// Playback replies carry a plan and its tracks, and may be a whole frame.
pub(crate) fn reservation(request: &Request) -> usize {
    match request {
        Request::StartPlayback { .. }
        | Request::ChangeSubtitle { .. }
        | Request::SeekPlayback { .. } => MAX_OUTCOME,
        Request::BeginPasskeySignIn { .. }
        | Request::BeginPasskeyRegistration
        | Request::BeginConfirmPasskey => CEREMONY_REPLY,
        _ => SMALL_REPLY,
    }
}

#[derive(Debug)]
struct Entry {
    fingerprint: [u8; 32],
    expires: u64,
    /// What it reserves while pending, then what its outcome takes.
    bytes: usize,
    /// The user whose session made it; `None` in the anonymous pool.
    user: Option<String>,
    outcome: watch::Receiver<Option<RpcReply>>,
}

#[derive(Debug)]
pub(crate) struct Operations {
    entries: Mutex<HashMap<(String, String), Entry>>,
    max_operations: usize,
    max_bytes: usize,
    /// How long a known outcome is kept, if less than its operation's
    /// expiry.
    kept_ms: Option<u64>,
}

impl Default for Operations {
    fn default() -> Self {
        Self {
            entries: Mutex::default(),
            max_operations: MAX_OPERATIONS,
            max_bytes: MAX_BYTES,
            kept_ms: None,
        }
    }
}

pub(crate) struct Invocation {
    pub outcome: watch::Receiver<Option<RpcReply>>,
    pub execute: Option<watch::Sender<Option<RpcReply>>>,
}

/// Who an operation is counted against: the endpoint it came from, and
/// for a signed-in request the user of its session.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Owner<'a> {
    pub endpoint: &'a str,
    pub user: Option<&'a str>,
}

impl Operations {
    /// Anonymous sign-in outcomes cannot consume authenticated capacity.
    pub fn anonymous() -> Self {
        Self {
            max_operations: 512,
            max_bytes: 8 << 20,
            kept_ms: Some(ANONYMOUS_KEPT_MS),
            ..Self::default()
        }
    }

    /// Recover an existing invocation without admitting unauthenticated work.
    /// Exact replays (including logout) keep their outcome after session revocation.
    pub fn lookup(
        &self,
        endpoint: &str,
        instance: &str,
        operation: &Operation,
        fingerprint: [u8; 32],
        now: u64,
    ) -> Result<Option<Invocation>, Error> {
        Self::validate(instance, operation, now)?;
        let mut entries = self.entries.lock().expect("never poisoned");
        entries.retain(|_, entry| entry.expires > now);
        let Some(entry) = entries.get(&(endpoint.to_owned(), operation.id.clone())) else {
            return Ok(None);
        };
        if entry.fingerprint != fingerprint {
            return Err(Error::OperationConflict);
        }
        Ok(Some(Invocation {
            outcome: entry.outcome.clone(),
            execute: None,
        }))
    }

    fn validate(instance: &str, operation: &Operation, now: u64) -> Result<(), Error> {
        if operation.instance != instance {
            return Err(Error::ServerRestarted);
        }
        if operation.expires_at_ms <= now {
            return Err(Error::OperationExpired);
        }
        if operation.expires_at_ms > now.saturating_add(OPERATION_WINDOW_MS)
            || operation.id.is_empty()
            || operation.id.len() > 128
        {
            return Err(Error::InvalidInput {
                message: "invalid operation ID or expiry".into(),
            });
        }
        Ok(())
    }

    /// Whether `reserve` more bytes and one more entry fit `held`, the
    /// entries and bytes already held, within `ops` and `bytes`.
    fn fits(held: (usize, usize), reserve: usize, ops: usize, bytes: usize) -> bool {
        held.0 < ops && held.1 + reserve <= bytes
    }

    /// Admit `operation` for `owner`, reserving `reserve` bytes for its
    /// outcome ([`reservation`]).
    pub fn begin(
        &self,
        owner: Owner<'_>,
        instance: &str,
        operation: &Operation,
        fingerprint: [u8; 32],
        reserve: usize,
        now: u64,
    ) -> Result<Invocation, Error> {
        Self::validate(instance, operation, now)?;
        let key = (owner.endpoint.to_owned(), operation.id.clone());
        let mut entries = self.entries.lock().expect("never poisoned");
        entries.retain(|_, entry| entry.expires > now);
        if let Some(entry) = entries.get(&key) {
            if entry.fingerprint != fingerprint {
                return Err(Error::OperationConflict);
            }
            return Ok(Invocation {
                outcome: entry.outcome.clone(),
                execute: None,
            });
        }
        let reserve = reserve.min(MAX_OUTCOME);
        let held = |of: &dyn Fn(&(String, String), &Entry) -> bool| {
            entries
                .iter()
                .filter(|(key, entry)| of(key, entry))
                .fold((0, 0), |(n, bytes), (_, entry)| {
                    (n + 1, bytes + entry.bytes)
                })
        };
        let (share_ops, share_bytes) = (
            self.max_operations / OWNER_SHARE,
            self.max_bytes / OWNER_SHARE,
        );
        let room = Self::fits(
            held(&|_, _| true),
            reserve,
            self.max_operations,
            self.max_bytes,
        ) && Self::fits(
            held(&|(peer, _), _| peer == owner.endpoint),
            reserve,
            share_ops,
            share_bytes,
        ) && owner.user.is_none_or(|user| {
            Self::fits(
                held(&|_, entry| entry.user.as_deref() == Some(user)),
                reserve,
                share_ops,
                share_bytes,
            )
        });
        if !room {
            return Err(Error::RateLimited {
                retry_after_ms: 1000,
            });
        }
        let (send, outcome) = watch::channel(None);
        entries.insert(
            key,
            Entry {
                fingerprint,
                expires: operation.expires_at_ms,
                bytes: reserve,
                user: owner.user.map(str::to_owned),
                outcome: outcome.clone(),
            },
        );
        Ok(Invocation {
            outcome,
            execute: Some(send),
        })
    }

    /// The outcome of `endpoint`'s operation `id` is `reply`, at `now`. It
    /// is kept at its own size, which may be more than was reserved for
    /// it, up to a frame.
    pub fn finish(
        &self,
        endpoint: &str,
        id: &str,
        send: watch::Sender<Option<RpcReply>>,
        mut reply: RpcReply,
        now: u64,
    ) {
        let size = spoolframe_protocol::codec::encode_limited(&reply, MAX_OUTCOME)
            .map(|bytes| bytes.len());
        let bytes = match size {
            Ok(size) if size <= MAX_OUTCOME => size,
            _ => {
                reply = Response::Err(Error::ReplyTooLarge {
                    bytes: size.unwrap_or(MAX_OUTCOME + 1) as u64,
                })
                .into();
                SMALL_REPLY
            }
        };
        let mut entries = self.entries.lock().expect("never poisoned");
        if let Some(entry) = entries.get_mut(&(endpoint.to_owned(), id.to_owned())) {
            entry.bytes = bytes;
            if let Some(kept) = self.kept_ms {
                entry.expires = entry.expires.min(now.saturating_add(kept));
            }
        }
        send.send_replace(Some(reply));
    }
}

pub(crate) fn now_ms() -> u64 {
    // Wall-clock corrections must not make an expired and evicted ID valid
    // again. Anchor once, then advance only with the monotonic process clock.
    static CLOCK: std::sync::OnceLock<(u64, std::time::Instant)> = std::sync::OnceLock::new();
    let (base, started) = CLOCK.get_or_init(|| {
        let base = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_millis()
            .try_into()
            .expect("Unix milliseconds fit in u64");
        (base, std::time::Instant::now())
    });
    base.saturating_add(started.elapsed().as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_protocol::Reply;

    fn operation() -> Operation {
        Operation {
            id: "request-1".into(),
            instance: "process-1".into(),
            expires_at_ms: 10_000,
        }
    }

    /// An anonymous request from `endpoint`.
    fn at(endpoint: &str) -> Owner<'_> {
        Owner {
            endpoint,
            user: None,
        }
    }

    /// `user`'s request from `endpoint`.
    fn of<'a>(endpoint: &'a str, user: &'a str) -> Owner<'a> {
        Owner {
            endpoint,
            user: Some(user),
        }
    }

    fn change_subtitle() -> usize {
        reservation(&Request::ChangeSubtitle {
            handle: spoolframe_protocol::PlaybackHandle("h".into()),
            subtitle: None,
            bitmap: Default::default(),
            position_ms: 0,
        })
    }

    fn mark_watched() -> usize {
        reservation(&Request::MarkWatched {
            id: spoolframe_types::MediaId::new(
                spoolframe_types::Namespace::Plex,
                spoolframe_types::MediaKind::Movie,
                "1",
            )
            .unwrap(),
        })
    }

    #[test]
    fn one_user_s_pending_mutations_leave_room_for_another_s_mark_watched() {
        let cache = Operations::default();
        let mut op = operation();
        // User A's subtitle changes, queued behind one handle's lock.
        let mut pending = Vec::new();
        for i in 0..64 {
            op.id = format!("a-{i}");
            pending.push(cache.begin(
                of("endpoint-a", "a"),
                "process-1",
                &op,
                [1; 32],
                change_subtitle(),
                1,
            ));
        }
        op.id = "b-watched".into();
        assert!(
            cache
                .begin(
                    of("endpoint-b", "b"),
                    "process-1",
                    &op,
                    [1; 32],
                    mark_watched(),
                    1
                )
                .is_ok(),
            "user B's MarkWatched was refused"
        );
    }

    #[test]
    fn a_user_on_many_devices_holds_only_a_user_s_share() {
        let cache = Operations::default();
        let mut op = operation();
        let mut admitted = 0;
        for i in 0..64 {
            op.id = format!("a-{i}");
            let endpoint = format!("device-{i}");
            if cache
                .begin(
                    of(&endpoint, "a"),
                    "process-1",
                    &op,
                    [1; 32],
                    change_subtitle(),
                    1,
                )
                .is_ok()
            {
                admitted += 1;
            }
        }
        assert_eq!(admitted, MAX_BYTES / OWNER_SHARE / MAX_OUTCOME);
        op.id = "b".into();
        assert!(
            cache
                .begin(
                    of("endpoint-b", "b"),
                    "process-1",
                    &op,
                    [1; 32],
                    change_subtitle(),
                    1
                )
                .is_ok()
        );
    }

    #[test]
    fn eight_pending_device_sign_ins_leave_room_for_a_login() {
        let anonymous = Operations::anonymous();
        let mut op = operation();
        let await_sign_in = reservation(&Request::AwaitDeviceSignIn {
            request: "r".into(),
        });
        let mut pending = Vec::new();
        for i in 0..8 {
            op.id = format!("await-{i}");
            let endpoint = format!("tv-{i}");
            pending.push(anonymous.begin(
                at(&endpoint),
                "process-1",
                &op,
                [1; 32],
                await_sign_in,
                1,
            ));
        }
        op.id = "login".into();
        let login = reservation(&Request::LoginPassword(
            spoolframe_protocol::LoginPassword {
                username: "alice".into(),
                password: "correct horse".into(),
                remember: false,
            },
        ));
        assert!(
            anonymous
                .begin(at("phone"), "process-1", &op, [1; 32], login, 1)
                .is_ok(),
            "a new LoginPassword was refused"
        );
    }

    #[test]
    fn a_finished_anonymous_outcome_is_replayed_briefly_then_dropped() {
        let anonymous = Operations::anonymous();
        let mut op = operation();
        op.expires_at_ms = 1 + OPERATION_WINDOW_MS;
        let invocation = anonymous
            .begin(at("peer"), "process-1", &op, [1; 32], SMALL_REPLY, 1)
            .unwrap();
        anonymous.finish(
            "peer",
            &op.id,
            invocation.execute.unwrap(),
            Response::Err(Error::InvalidCredentials).into(),
            1,
        );
        let replayed = |now| {
            anonymous
                .lookup("peer", "process-1", &op, [1; 32], now)
                .unwrap()
                .is_some()
        };
        assert!(replayed(1 + 29_000), "not replayed within 30 s");
        assert!(!replayed(1 + 31_000), "still held after 30 s");
    }

    #[test]
    fn a_signed_in_outcome_is_kept_until_its_operation_expires() {
        let cache = Operations::default();
        let mut op = operation();
        op.expires_at_ms = 1 + OPERATION_WINDOW_MS;
        let invocation = cache
            .begin(of("peer", "a"), "process-1", &op, [1; 32], SMALL_REPLY, 1)
            .unwrap();
        cache.finish(
            "peer",
            &op.id,
            invocation.execute.unwrap(),
            Response::Ok(Reply::Done).into(),
            1,
        );
        assert!(
            cache
                .lookup("peer", "process-1", &op, [1; 32], OPERATION_WINDOW_MS)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn anonymous_capacity_is_isolated_and_replays_still_work_when_full() {
        let anonymous = Operations::anonymous();
        let authenticated = Operations::default();
        let mut op = operation();
        // Completed failures still count against the anonymous endpoint quota.
        for i in 0..anonymous.max_operations / OWNER_SHARE {
            op.id = i.to_string();
            let invocation = anonymous
                .begin(at("peer"), "process-1", &op, [1; 32], SMALL_REPLY, 1)
                .unwrap();
            anonymous.finish(
                "peer",
                &op.id,
                invocation.execute.unwrap(),
                Response::Err(Error::Unauthenticated).into(),
                1,
            );
        }
        op.id = "overflow".into();
        assert!(matches!(
            anonymous.begin(at("peer"), "process-1", &op, [1; 32], SMALL_REPLY, 1),
            Err(Error::RateLimited { .. })
        ));
        assert!(
            authenticated
                .begin(of("peer", "a"), "process-1", &op, [1; 32], SMALL_REPLY, 1)
                .is_ok()
        );
        op.id = "0".into();
        assert!(
            anonymous
                .lookup("peer", "process-1", &op, [1; 32], 1)
                .unwrap()
                .unwrap()
                .execute
                .is_none()
        );
        assert!(matches!(
            anonymous.lookup("peer", "process-1", &op, [2; 32], 1),
            Err(Error::OperationConflict)
        ));
        // Independent endpoints can fill only the anonymous pool.
        let mut pending = Vec::new();
        for i in 0..64 {
            let endpoint = format!("other-{i}");
            pending.push(anonymous.begin(at(&endpoint), "process-1", &op, [1; 32], MAX_OUTCOME, 1));
        }
        assert!(pending.iter().any(Result::is_err));
        assert!(
            authenticated
                .begin(
                    of("legitimate", "b"),
                    "process-1",
                    &op,
                    [1; 32],
                    SMALL_REPLY,
                    1
                )
                .is_ok()
        );
    }

    #[tokio::test]
    async fn simultaneous_duplicates_share_the_pending_and_completed_outcome() {
        let cache = Operations::default();
        let op = operation();
        let owner = of("endpoint", "a");
        let first = cache
            .begin(owner, "process-1", &op, [1; 32], SMALL_REPLY, 1)
            .unwrap();
        let mut duplicate = cache
            .begin(owner, "process-1", &op, [1; 32], SMALL_REPLY, 2)
            .unwrap();
        assert!(duplicate.execute.is_none());
        let reply: RpcReply = Response::Ok(Reply::Done).into();
        cache.finish("endpoint", &op.id, first.execute.unwrap(), reply.clone(), 2);
        duplicate.outcome.changed().await.unwrap();
        assert_eq!(*duplicate.outcome.borrow(), Some(reply.clone()));
        let replay = cache
            .begin(owner, "process-1", &op, [1; 32], SMALL_REPLY, 3)
            .unwrap();
        assert!(replay.execute.is_none());
        assert_eq!(*replay.outcome.borrow(), Some(reply));
    }

    #[test]
    fn an_id_cannot_change_payload_or_outlive_its_process_and_expiry() {
        let cache = Operations::default();
        let op = operation();
        let owner = of("endpoint", "a");
        let _pending = cache
            .begin(owner, "process-1", &op, [1; 32], SMALL_REPLY, 1)
            .unwrap();
        assert!(matches!(
            cache.begin(owner, "process-1", &op, [2; 32], SMALL_REPLY, 1),
            Err(Error::OperationConflict)
        ));
        assert!(matches!(
            cache.begin(owner, "process-2", &op, [1; 32], SMALL_REPLY, 1),
            Err(Error::ServerRestarted)
        ));
        assert!(matches!(
            cache.begin(
                owner,
                "process-1",
                &op,
                [1; 32],
                SMALL_REPLY,
                op.expires_at_ms
            ),
            Err(Error::OperationExpired)
        ));
        assert!(
            cache
                .begin(
                    of("another-endpoint", "a"),
                    "process-1",
                    &op,
                    [1; 32],
                    SMALL_REPLY,
                    1
                )
                .unwrap()
                .execute
                .is_some()
        );
    }

    #[test]
    fn admission_never_evicts_an_unexpired_invocation() {
        let cache = Operations::default();
        let mut op = operation();
        let mut pending = Vec::new();
        // Eight users on eight devices fill the pool between them.
        let per_owner = MAX_BYTES / OWNER_SHARE / MAX_OUTCOME;
        let owners: Vec<_> = (0..OWNER_SHARE)
            .map(|i| (format!("endpoint-{i}"), format!("user-{i}")))
            .collect();
        for (endpoint, user) in &owners {
            for i in 0..per_owner {
                op.id = format!("{endpoint}-{i}");
                pending.push(
                    cache
                        .begin(
                            of(endpoint, user),
                            "process-1",
                            &op,
                            [1; 32],
                            MAX_OUTCOME,
                            1,
                        )
                        .unwrap(),
                );
            }
        }
        op.id = "overflow".into();
        assert!(matches!(
            cache.begin(
                of("endpoint-new", "user-new"),
                "process-1",
                &op,
                [1; 32],
                SMALL_REPLY,
                1
            ),
            Err(Error::RateLimited { .. })
        ));
        op.id = "endpoint-0-0".into();
        assert!(
            cache
                .begin(
                    of("endpoint-0", "user-0"),
                    "process-1",
                    &op,
                    [1; 32],
                    MAX_OUTCOME,
                    1
                )
                .unwrap()
                .execute
                .is_none()
        );
        op.id = "fresh".into();
        op.expires_at_ms += OPERATION_WINDOW_MS;
        assert!(
            cache
                .begin(
                    of("endpoint-new", "user-new"),
                    "process-1",
                    &op,
                    [1; 32],
                    SMALL_REPLY,
                    10_001
                )
                .unwrap()
                .execute
                .is_some()
        );
    }
}
