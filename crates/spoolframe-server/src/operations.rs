//! Bounded, process-local mutation outcomes. Pending work survives the
//! requesting stream; expired IDs and IDs from earlier processes are refused.
//!
//! Each pool is shared, so no one may take much of it (PX-10, finding 6):
//! an entry reserves what its kind of reply can be, not the largest frame,
//! and each endpoint and each user may hold only a share of the pool.

use parking_lot::Mutex;
use spoolframe_protocol::rpc::{OPERATION_WINDOW_MS, Operation, RpcReply};
use spoolframe_protocol::{Error, Reply, Request, Response};
use std::collections::HashMap;
use std::sync::Arc;
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
/// An outcome that changed nothing is not kept at all ([`changed_nothing`]).
const ANONYMOUS_KEPT_MS: u64 = 30_000;

/// Whether an anonymous request's `reply` left nothing that a retry could
/// not do again: a refusal or failure, or a device's wait still pending.
/// A retry after a lost reply is then answered by running it again, which
/// refuses, fails or waits again; a wrong password is counted again, which
/// errs on the safe side. Kept, such outcomes let strangers hold the
/// anonymous pool with requests that cost them nothing (finding 48).
fn changed_nothing(reply: &RpcReply) -> bool {
    matches!(
        reply.response,
        Response::Err(_) | Response::Ok(Reply::DeviceSignInPending)
    )
}

/// What the outcome of `request` may take, reserved while it runs.
/// Playback replies carry a plan and its tracks, and may be a whole frame.
pub(crate) fn reservation(request: &Request) -> usize {
    match request {
        Request::StartPlayback { .. }
        | Request::ChangeSubtitle { .. }
        | Request::ChangeQuality { .. }
        | Request::SeekPlayback { .. } => MAX_OUTCOME,
        Request::BeginPasskeySignIn { .. }
        | Request::BeginPasskeyRegistration
        | Request::BeginConfirmPasskey => CEREMONY_REPLY,
        _ => SMALL_REPLY,
    }
}

/// The pools a request's operation can be held in. Anonymous requests
/// are made before any session is checked, so they cannot take signed-in
/// capacity; and they are held in two pools of their own (finding 48):
///
/// - sign-ins a stranger makes only with a token from the bucket all
///   strangers share (`LoginPassword`, `Begin`/`FinishPasskeySignIn`,
///   `RedeemInvite`, `Begin`/`FinishPortableSignIn`,
///   `RedeemPortableInvite`, and P7-08's `IdentityRecord`,
///   `IdentityRecordByHandle`, `OfferPairing`,
///   `FetchPairing` and `AnswerPairing`), which the bucket's
///   rate keeps from filling theirs;
/// - requests that draw on no rate limit (`BeginDeviceSignIn`,
///   `AwaitDeviceSignIn`, `ResumeProfile`, `CollectPairing`), held by what
///   they wait on: a
///   device's few pending sign-ins, each waited on once at a time
///   ([`crate::device_sign_in::DeviceSignIns::wait`]), and a trusted
///   device's resume.
///
/// Endpoint ids are free, so an endpoint's share alone does not keep
/// strangers from filling a pool between them; this keeps a flood of the
/// one kind from taking the other's room.
#[derive(Debug, Clone)]
pub(crate) struct Pools {
    signed_in: Arc<Operations>,
    sign_in: Arc<Operations>,
    device: Arc<Operations>,
}

impl Default for Pools {
    fn default() -> Self {
        Self {
            signed_in: Arc::default(),
            sign_in: Arc::new(Operations::anonymous()),
            device: Arc::new(Operations::anonymous()),
        }
    }
}

/// Whether `request` draws on the strangers' sign-in budget (PX-10; P6-01,
/// P7-05, P7-06, P7-08): the sign-ins a stranger makes, and the public
/// identity data and pairing codes guessed only at that rate. They are
/// also the anonymous sign-in pool's ([`Pools::of`]); one list for both.
pub fn on_strangers_budget(request: &Request) -> bool {
    matches!(
        request,
        Request::LoginPassword(_)
            | Request::BeginPasskeySignIn { .. }
            | Request::FinishPasskeySignIn { .. }
            | Request::RedeemInvite(_)
            | Request::BeginPortableSignIn
            | Request::FinishPortableSignIn(_)
            | Request::RedeemPortableInvite(_)
            | Request::IdentityRecord { .. }
            | Request::IdentityRecordByHandle { .. }
            | Request::OfferPairing(_)
            | Request::FetchPairing { .. }
            | Request::AnswerPairing { .. }
    )
}

impl Pools {
    /// The pool `request`'s operation is held in, and whether it is an
    /// anonymous one.
    pub fn of(&self, request: &Request) -> (&Arc<Operations>, bool) {
        if on_strangers_budget(request) {
            return (&self.sign_in, true);
        }
        match request {
            Request::BeginDeviceSignIn { .. }
            | Request::AwaitDeviceSignIn { .. }
            | Request::ResumeProfile { .. }
            // P7-08: the offering device's own, polled, off the budget.
            | Request::CollectPairing { .. } => (&self.device, true),
            _ => (&self.signed_in, false),
        }
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
        let mut entries = self.entries.lock();
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
        let mut entries = self.entries.lock();
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
        let user_held = owner
            .user
            .map(|user| held(&|_, entry| entry.user.as_deref() == Some(user)));
        // The pool's last share is for users holding nothing (finding 49):
        // eight users at their shares would fill it, and users are few, so
        // one more always has room for its first. Endpoints are not
        // reserved for: they are free, and any number could ask.
        let (pool_ops, pool_bytes) = match user_held {
            Some((n, _)) if n > 0 => (
                self.max_operations - share_ops,
                self.max_bytes - share_bytes,
            ),
            _ => (self.max_operations, self.max_bytes),
        };
        let room = Self::fits(held(&|_, _| true), reserve, pool_ops, pool_bytes)
            && Self::fits(
                held(&|(peer, _), _| peer == owner.endpoint),
                reserve,
                share_ops,
                share_bytes,
            )
            && user_held.is_none_or(|held| Self::fits(held, reserve, share_ops, share_bytes));
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

    /// Whether the bytes `entries` hold fit this pool, and the bytes
    /// `endpoint` and `user` hold fit their shares of it.
    fn within(
        &self,
        entries: &HashMap<(String, String), Entry>,
        endpoint: &str,
        user: Option<&str>,
    ) -> bool {
        let share = self.max_bytes / OWNER_SHARE;
        let bytes = |of: &dyn Fn(&str, &Entry) -> bool| -> usize {
            entries
                .iter()
                .filter(|((peer, _), entry)| of(peer, entry))
                .map(|(_, entry)| entry.bytes)
                .sum()
        };
        bytes(&|_, _| true) <= self.max_bytes
            && bytes(&|peer, _| peer == endpoint) <= share
            && user
                .is_none_or(|user| bytes(&|_, entry| entry.user.as_deref() == Some(user)) <= share)
    }

    /// The outcome of `endpoint`'s operation `id` is `reply`, at `now`. It
    /// is kept at its own size, which may be more than was reserved for
    /// it, up to a frame, while the pool and its owners' shares still have
    /// room for that; otherwise it is not kept.
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
        let key = (endpoint.to_owned(), id.to_owned());
        let mut entries = self.entries.lock();
        match self.kept_ms {
            // Requests waiting on it hold its outcome already.
            Some(_) if changed_nothing(&reply) => {
                entries.remove(&key);
            }
            kept => {
                if let Some(entry) = entries.get_mut(&key) {
                    let reserved = std::mem::replace(&mut entry.bytes, bytes);
                    if let Some(kept) = kept {
                        entry.expires = entry.expires.min(now.saturating_add(kept));
                    }
                    let user = entry.user.clone();
                    // Admission counted what it reserved; an outcome kept
                    // larger than that past the pool's bytes would hold
                    // them to expiry (finding 55). Only replies whose size
                    // the requester chooses outgrow their reservation (a
                    // passkey ceremony's credential ids), and running one
                    // of those again on a retry is harmless.
                    if bytes > reserved && !self.within(&entries, endpoint, user.as_deref()) {
                        entries.remove(&key);
                    }
                }
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

    /// One list says which requests draw on the strangers' budget, and the
    /// anonymous sign-in pool holds exactly those.
    #[test]
    fn the_strangers_budget_and_the_sign_in_pool_are_one_list() {
        assert!(on_strangers_budget(&Request::BeginPortableSignIn));
        assert!(on_strangers_budget(&Request::BeginPasskeySignIn {
            username: None
        }));
        assert!(!on_strangers_budget(&Request::BeginDeviceSignIn {
            remember: false
        }));
        assert!(!on_strangers_budget(&Request::Ping));
        assert!(!on_strangers_budget(&Request::AuthMethods));
    }

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
            display: None,
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

    fn login() -> Request {
        Request::LoginPassword(spoolframe_protocol::LoginPassword {
            username: "alice".into(),
            password: "correct horse".into(),
            remember: false,
        })
    }

    /// A sign-in's outcome: a session, which a retry must not open again.
    fn logged_in() -> RpcReply {
        Response::Ok(Reply::LoggedIn {
            session: spoolframe_protocol::SessionId("session".into()),
            remembered: false,
            password_change_required: false,
            features: Vec::new(),
        })
        .into()
    }

    fn await_sign_in() -> Request {
        Request::AwaitDeviceSignIn {
            request: "r".into(),
        }
    }

    fn begin_confirm_passkey() -> usize {
        reservation(&Request::BeginConfirmPasskey)
    }

    /// `BeginConfirmPasskey`'s outcome for a user with `passkeys` passkeys
    /// whose credential ids are 1 KiB each: nothing caps how many a user
    /// has, so it can be far more than the 64 KiB its kind reserves.
    fn ceremony(passkeys: usize) -> RpcReply {
        Response::Ok(Reply::PasskeySignInBegun(
            spoolframe_protocol::passkeys::PasskeySignInBegun {
                nonce: vec![0; 32],
                allow_credential_ids: vec![vec![7; 1024]; passkeys],
            },
        ))
        .into()
    }

    /// The bytes `cache`'s entries hold.
    fn held_bytes(cache: &Operations) -> usize {
        cache.entries.lock().values().map(|entry| entry.bytes).sum()
    }

    /// `users` users' `BeginConfirmPasskey`s, each from an endpoint of its
    /// own, all admitted while none has finished, then each finished with
    /// `reply`.
    fn confirm_at_once(cache: &Operations, users: usize, reply: &RpcReply) {
        let mut op = operation();
        let begun: Vec<_> = (0..users)
            .map(|i| {
                let user = format!("user-{i}");
                op.id = format!("{user}-confirm");
                let invocation = cache
                    .begin(
                        of(&user, &user),
                        "process-1",
                        &op,
                        [1; 32],
                        begin_confirm_passkey(),
                        1,
                    )
                    .unwrap();
                (user, op.id.clone(), invocation)
            })
            .collect();
        for (user, id, invocation) in begun {
            cache.finish(&user, &id, invocation.execute.unwrap(), reply.clone(), 1);
        }
    }

    /// Third review G-6 (finding 55): an outcome is admitted at what its
    /// kind reserves and kept at its own size, up to a frame, so outcomes
    /// larger than they reserved held more than the pool's bytes.
    #[test]
    fn outcomes_larger_than_reserved_never_hold_more_than_the_pool() {
        let cache = Operations::default();
        confirm_at_once(&cache, 100, &ceremony(900));
        let held = held_bytes(&cache);
        assert!(held <= MAX_BYTES, "{held} bytes held of {MAX_BYTES}");
    }

    /// One that did not fit is not kept: a retry runs it again, which for
    /// a passkey ceremony makes a new nonce.
    #[test]
    fn an_outcome_that_does_not_fit_the_pool_runs_again_on_retry() {
        let cache = Operations::default();
        confirm_at_once(&cache, 100, &ceremony(900));
        let mut op = operation();
        op.id = "user-99-confirm".into();
        let retry = cache
            .begin(
                of("user-99", "user-99"),
                "process-1",
                &op,
                [1; 32],
                begin_confirm_passkey(),
                2,
            )
            .unwrap();
        assert!(
            retry.execute.is_some(),
            "the retry was answered from a kept outcome"
        );
    }

    /// An invite's redemption (P6-01) is a sign-in on the strangers'
    /// budget: held in their sign-in pool, where a refusal is not kept.
    #[test]
    fn an_invite_s_redemption_is_held_as_an_anonymous_sign_in() {
        let pools = Pools::default();
        let redeem = Request::RedeemInvite(spoolframe_protocol::RedeemInvite {
            code: "QW7K-2MFP-XR4T".into(),
            username: "mum".into(),
            display_name: None,
            password: "correct horse".into(),
        });
        let (pool, anonymous) = pools.of(&redeem);
        assert!(anonymous);
        assert!(Arc::ptr_eq(pool, &pools.sign_in));
    }

    /// Independent review M-1 (finding 48): eight strangers, each keeping
    /// its endpoint's share of the anonymous pool with refused requests,
    /// held it all, and every other device's sign-in was refused.
    #[test]
    fn eight_strangers_refused_requests_leave_room_for_a_ninth_s_login() {
        let anonymous = Operations::anonymous();
        let mut op = operation();
        let share = anonymous.max_operations / OWNER_SHARE;
        for i in 0..OWNER_SHARE {
            let endpoint = format!("stranger-{i}");
            for j in 0..share {
                op.id = format!("{endpoint}-{j}");
                let invocation = anonymous
                    .begin(
                        at(&endpoint),
                        "process-1",
                        &op,
                        [1; 32],
                        reservation(&login()),
                        1,
                    )
                    .unwrap();
                anonymous.finish(
                    &endpoint,
                    &op.id,
                    invocation.execute.unwrap(),
                    Response::Err(Error::RateLimited {
                        retry_after_ms: 200,
                    })
                    .into(),
                    1,
                );
            }
        }
        op.id = "login".into();
        assert!(
            anonymous
                .begin(
                    at("fresh-device"),
                    "process-1",
                    &op,
                    [1; 32],
                    reservation(&login()),
                    2
                )
                .is_ok(),
            "a fresh device's LoginPassword was refused"
        );
    }

    /// An answer that a device sign-in is still pending changed nothing:
    /// asking again waits again, so it is not kept.
    #[test]
    fn a_pending_answer_to_a_device_s_wait_is_not_kept() {
        let anonymous = Operations::anonymous();
        let mut op = operation();
        op.expires_at_ms = 1 + OPERATION_WINDOW_MS;
        let invocation = anonymous
            .begin(
                at("tv"),
                "process-1",
                &op,
                [1; 32],
                reservation(&await_sign_in()),
                1,
            )
            .unwrap();
        anonymous.finish(
            "tv",
            &op.id,
            invocation.execute.unwrap(),
            Response::Ok(Reply::DeviceSignInPending).into(),
            1,
        );
        assert!(
            anonymous
                .lookup("tv", "process-1", &op, [1; 32], 2)
                .unwrap()
                .is_none()
        );
    }

    /// Requests that draw on no rate limit (a device's sign-in and its
    /// long polls, a trusted device's resume) are held apart from the
    /// sign-ins the strangers' bucket meters: waits kept up by a few
    /// strangers cannot take a login's room.
    #[test]
    fn device_sign_ins_waiting_leave_room_for_a_login() {
        let pools = Pools::default();
        let (device, _) = pools.of(&await_sign_in());
        let share = device.max_operations / OWNER_SHARE;
        let mut op = operation();
        let mut pending = Vec::new();
        for i in 0..OWNER_SHARE {
            let endpoint = format!("stranger-{i}");
            for j in 0..share {
                op.id = format!("{endpoint}-{j}");
                pending.push(
                    device
                        .begin(
                            at(&endpoint),
                            "process-1",
                            &op,
                            [1; 32],
                            reservation(&await_sign_in()),
                            1,
                        )
                        .unwrap(),
                );
            }
        }
        let (sign_in, anonymous) = pools.of(&login());
        assert!(anonymous);
        op.id = "login".into();
        assert!(
            sign_in
                .begin(
                    at("fresh-device"),
                    "process-1",
                    &op,
                    [1; 32],
                    reservation(&login()),
                    1
                )
                .is_ok(),
            "a fresh device's LoginPassword was refused"
        );
    }

    /// Independent review M-3 (finding 49): eight users, each holding a
    /// user's share, filled the pool, and every other user's mutations
    /// were refused.
    #[test]
    fn eight_users_full_shares_leave_room_for_a_ninth_s_mark_watched() {
        let cache = Operations::default();
        let mut op = operation();
        let mut pending = Vec::new();
        for i in 0..OWNER_SHARE {
            let (endpoint, user) = (format!("endpoint-{i}"), format!("user-{i}"));
            // Until the user's share, or the pool, refuses more.
            for j in 0.. {
                op.id = format!("{endpoint}-{j}");
                match cache.begin(
                    of(&endpoint, &user),
                    "process-1",
                    &op,
                    [1; 32],
                    change_subtitle(),
                    1,
                ) {
                    Ok(invocation) => pending.push(invocation),
                    Err(_) => break,
                }
            }
        }
        op.id = "watched".into();
        assert!(
            cache
                .begin(
                    of("endpoint-9", "user-9"),
                    "process-1",
                    &op,
                    [1; 32],
                    mark_watched(),
                    1
                )
                .is_ok(),
            "a ninth user's MarkWatched was refused"
        );
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
        anonymous.finish("peer", &op.id, invocation.execute.unwrap(), logged_in(), 1);
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
        // Completed sign-ins still count against the anonymous endpoint
        // quota; refusals do not (finding 48).
        for i in 0..anonymous.max_operations / OWNER_SHARE {
            op.id = i.to_string();
            let invocation = anonymous
                .begin(at("peer"), "process-1", &op, [1; 32], SMALL_REPLY, 1)
                .unwrap();
            anonymous.finish("peer", &op.id, invocation.execute.unwrap(), logged_in(), 1);
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
        // Seven users on seven devices at their shares, and eight more with
        // one each in the room kept for users holding nothing, fill the
        // pool between them.
        let per_owner = MAX_BYTES / OWNER_SHARE / MAX_OUTCOME;
        let owners: Vec<_> = (0..OWNER_SHARE - 1 + per_owner)
            .map(|i| (format!("endpoint-{i}"), format!("user-{i}")))
            .collect();
        for (n, (endpoint, user)) in owners.iter().enumerate() {
            let each = if n < OWNER_SHARE - 1 { per_owner } else { 1 };
            for i in 0..each {
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
