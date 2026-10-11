//! The checks. Each one is a stranger doing one thing the server documents
//! a limit for, with well-formed frames only, and what the server must
//! answer. Each ends with a `Ping`, to show the connection or the server
//! is still fine after it.

use std::time::{Duration, Instant};

use iroh::endpoint::{Connection, ConnectionError, ReadError, VarInt};
use spoolframe_protocol::streams::{bi, reset};
use spoolframe_protocol::{
    BitmapSubtitles, Error, LoginPassword, PROTOCOL_VERSION, RedeemInvite, Reply, Request,
    Response, ServerInfo, close,
};
use spoolframe_types::{MediaId, MediaKind, Namespace};
use tokio::io::AsyncWriteExt;
use tokio::time::{sleep, timeout};

use crate::account::{self, AccountState};
use crate::wire::{Conn, Probe, Problem, bind, describe, hello, hello_for};
use crate::{Config, Status};

/// How long an open the server gives no credit for is given. Credit comes
/// in the handshake, so an open either returns at once or never.
pub(crate) const NO_CREDIT: Duration = Duration::from_secs(1);

/// How long the probe's own closed connections are given to leave the
/// server's count before the stranger-cap check counts.
const SETTLE: Duration = Duration::from_secs(1);

/// One check's verdict and what it saw.
pub(crate) struct Outcome {
    pub status: Status,
    pub evidence: Vec<String>,
}

impl Outcome {
    pub(crate) fn skip(why: &str) -> Self {
        Self {
            status: Status::Skip,
            evidence: vec![why.to_owned()],
        }
    }
}

/// Lines of evidence, each one as expected or not: the check passes when
/// all are.
#[derive(Default)]
pub(crate) struct Findings {
    lines: Vec<String>,
    failed: bool,
}

impl Findings {
    pub(crate) fn expect(&mut self, ok: bool, line: String) {
        self.failed |= !ok;
        self.lines.push(if ok {
            line
        } else {
            format!("UNEXPECTED: {line}")
        });
    }

    pub(crate) fn note(&mut self, line: String) {
        self.lines.push(line);
    }

    /// `request` on `conn`, with `session`, is answered with an error
    /// `matches` accepts.
    pub(crate) async fn expect_error(
        &mut self,
        probe: &Probe,
        conn: &Connection,
        what: &str,
        session: Option<&str>,
        request: Request,
        matches: impl Fn(&Error) -> bool,
    ) -> Result<(), Problem> {
        let response = probe.request(conn, session, request).await?;
        let ok = matches!(&response, Response::Err(e) if matches(e));
        self.expect(ok, format!("{what}: {}", describe(&response)));
        Ok(())
    }

    /// A `Ping` on `conn` is still answered `Pong`.
    async fn still_usable(
        &mut self,
        probe: &Probe,
        conn: &Connection,
        after: &str,
    ) -> Result<(), Problem> {
        match probe.request(conn, None, Request::Ping).await {
            Ok(response) => {
                let ok = response == Response::Ok(Reply::Pong);
                self.expect(ok, format!("Ping after {after}: {}", describe(&response)));
                Ok(())
            }
            Err(Problem::Failed(why)) => {
                self.expect(false, format!("Ping after {after}: {why}"));
                Ok(())
            }
            Err(stop) => Err(stop),
        }
    }

    pub(crate) fn done(self) -> Outcome {
        Outcome {
            status: if self.failed {
                Status::Fail
            } else {
                Status::Pass
            },
            evidence: self.lines,
        }
    }
}

/// A good `Hello` and a `Ping`: the server is there to probe.
pub(crate) async fn preflight(probe: &Probe) -> Result<ServerInfo, Problem> {
    let (conn, info) = probe.connect_hello().await?;
    match probe.request(&conn, None, Request::Ping).await? {
        Response::Ok(Reply::Pong) => Ok(info),
        other => Err(Problem::Failed(format!(
            "Ping after Hello: {}",
            describe(&other)
        ))),
    }
}

/// The checks, in the order they run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    HelloVersion,
    Unauthenticated,
    UniStream,
    BiStreams,
    FrameSize,
    FirstFrame,
    SignInLimits,
    InviteLimits,
    ConsumerAdminRefused,
    SessionBound,
    Visibility,
    RecentSignIn,
    SignedInStreams,
    PlaybackOwnership,
    SessionReread,
    StrangerCap,
    AnonymousPool,
}

impl Check {
    pub const ALL: [Check; 17] = [
        Check::HelloVersion,
        Check::Unauthenticated,
        Check::UniStream,
        Check::BiStreams,
        Check::FrameSize,
        Check::FirstFrame,
        Check::SignInLimits,
        Check::InviteLimits,
        // As the account (`--username`), one QUIC sign-in for all of them.
        Check::ConsumerAdminRefused,
        Check::SessionBound,
        Check::Visibility,
        Check::RecentSignIn,
        Check::SignedInStreams,
        // Only when asked (`--check-playback`); device A is the account's
        // sign-in, so before session-reread signs it out.
        Check::PlaybackOwnership,
        Check::SessionReread,
        // It fills the server's stranger slots; the anonymous-pool check
        // after it meets a server with them free again.
        Check::StrangerCap,
        // Last: it holds strangers' shares of the anonymous operation pool
        // for a while (before finding 48's fix that blocked every anonymous
        // sign-in).
        Check::AnonymousPool,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Check::HelloVersion => "hello-version",
            Check::Unauthenticated => "unauthenticated",
            Check::UniStream => "uni-stream",
            Check::BiStreams => "bi-streams",
            Check::FrameSize => "frame-size",
            Check::FirstFrame => "first-frame",
            Check::SignInLimits => "sign-in-limits",
            Check::InviteLimits => "invite-limits",
            Check::ConsumerAdminRefused => "consumer-admin-refused",
            Check::SessionBound => "session-bound",
            Check::Visibility => "visibility",
            Check::RecentSignIn => "recent-sign-in",
            Check::SignedInStreams => "signed-in-streams",
            Check::PlaybackOwnership => "playback-ownership",
            Check::SessionReread => "session-reread",
            Check::StrangerCap => "stranger-cap",
            Check::AnonymousPool => "anonymous-pool",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Check::HelloVersion => "Hello with the wrong PROTOCOL_VERSION is refused",
            Check::Unauthenticated => {
                "requests that need a session are Unauthenticated without one"
            }
            Check::UniStream => "a stranger cannot open a unidirectional stream",
            Check::BiStreams => "a stranger gets no more bidirectional streams than the limit",
            Check::FrameSize => {
                "a stranger's well-formed request above the frame limit is Malformed"
            }
            Check::FirstFrame => "a stream that sends nothing after its kind byte is reset",
            Check::SignInLimits => "wrong passwords for a made-up user reach RateLimited",
            Check::InviteLimits => "wrong invite codes reach RateLimited",
            Check::ConsumerAdminRefused => "a consumer is refused the admin reads, FORBIDDEN",
            Check::SessionBound => "the session from another endpoint key is Unauthenticated",
            Check::Visibility => {
                "what Libraries lists, and an unlisted library or item is NotFound"
            }
            Check::RecentSignIn => "a fresh session passes the recent sign-in check",
            Check::SignedInStreams => {
                "a signed-in connection gets more streams than a stranger's limit"
            }
            Check::PlaybackOwnership => {
                "a playback's handle is refused to another device and another user"
            }
            Check::SessionReread => {
                "the session works till Logout, which closes SIGNED_OUT and ends it"
            }
            Check::StrangerCap => "the stranger connection past the cap is closed BUSY",
            Check::AnonymousPool => {
                "strangers holding the anonymous pool cannot refuse another stranger's login"
            }
        }
    }

    pub(crate) async fn run(
        self,
        probe: &Probe,
        config: &Config,
        state: &AccountState,
    ) -> Result<Outcome, Problem> {
        match self {
            Check::HelloVersion => hello_version(probe).await,
            Check::Unauthenticated => unauthenticated(probe).await,
            Check::UniStream => uni_stream(probe).await,
            Check::BiStreams => bi_streams(probe, config).await,
            Check::FrameSize => frame_size(probe, config).await,
            Check::FirstFrame => first_frame(probe, config).await,
            Check::SignInLimits if config.check_limits => sign_in_limits(probe, config).await,
            Check::SignInLimits => Ok(Outcome::skip(
                "not asked for: pass --check-limits (failed logins for a made-up username)",
            )),
            Check::InviteLimits if config.check_limits => invite_limits(probe, config).await,
            Check::InviteLimits => Ok(Outcome::skip(
                "not asked for: pass --check-limits (wrong invite codes)",
            )),
            Check::StrangerCap if config.check_cap => stranger_cap(probe, config).await,
            Check::StrangerCap => Ok(Outcome::skip(
                "not asked for: pass --check-cap (opens cap+1 connections)",
            )),
            Check::AnonymousPool if config.check_pool => anonymous_pool(probe, config).await,
            Check::AnonymousPool => Ok(Outcome::skip(
                "not asked for: pass --check-pool (strangers holding the anonymous pool)",
            )),
            Check::ConsumerAdminRefused => {
                account::consumer_admin_refused(probe, config, state).await
            }
            Check::SessionBound => account::session_bound(probe, config, state).await,
            Check::Visibility => account::visibility(probe, config, state).await,
            Check::RecentSignIn => account::recent_sign_in(probe, config, state).await,
            Check::SignedInStreams => account::signed_in_streams(probe, config, state).await,
            Check::PlaybackOwnership => {
                crate::playback::playback_ownership(probe, config, state).await
            }
            Check::SessionReread => account::session_reread(probe, config, state).await,
        }
    }
}

/// Check 1: a `Hello` from another protocol revision is refused, and not
/// kept: the connection still has to say a good one.
async fn hello_version(probe: &Probe) -> Result<Outcome, Problem> {
    let mut f = Findings::default();
    let conn = probe.connect().await?;
    let wrong = PROTOCOL_VERSION + 1000;
    f.expect_error(
        probe,
        &conn,
        &format!("Hello with protocol_version {wrong} (the server's is {PROTOCOL_VERSION})"),
        None,
        hello_for(wrong, None),
        |e| matches!(e, Error::Unsupported { .. }),
    )
    .await?;
    f.expect_error(
        probe,
        &conn,
        "Ping after the refused Hello",
        None,
        Request::Ping,
        |e| *e == Error::HandshakeRequired,
    )
    .await?;
    let response = probe.request(&conn, None, hello(None)).await?;
    f.expect(
        matches!(response, Response::Ok(Reply::ServerInfo(_))),
        format!("a good Hello after it: {}", describe(&response)),
    );
    f.still_usable(probe, &conn, "the good Hello").await?;
    Ok(f.done())
}

/// Check 2: a sample of the requests that need a session (reads and a
/// mutation), sent with none.
async fn unauthenticated(probe: &Probe) -> Result<Outcome, Problem> {
    let mut f = Findings::default();
    let (conn, _) = probe.connect_hello().await?;
    let id = MediaId::new(Namespace::Plex, MediaKind::Movie, "1").expect("a valid media id");
    let sample = [
        ("Libraries", Request::Libraries),
        ("Item", Request::Item { id: id.clone() }),
        (
            "StartPlayback",
            Request::StartPlayback {
                id,
                selection: None,
                start_ms: None,
                bitmap: BitmapSubtitles::Auto,
                quality: Default::default(),
                display: None,
            },
        ),
    ];
    for (what, request) in sample {
        f.expect_error(
            probe,
            &conn,
            &format!("{what}, no session"),
            None,
            request,
            |e| *e == Error::Unauthenticated,
        )
        .await?;
    }
    f.still_usable(probe, &conn, "the refused requests").await?;
    Ok(f.done())
}

/// Check 3a: the server takes no unidirectional stream from a client
/// (events go the other way), so a stranger gets no credit for one.
/// Nothing is written on it if it opens.
async fn uni_stream(probe: &Probe) -> Result<Outcome, Problem> {
    let mut f = Findings::default();
    let (conn, _) = probe.connect_hello().await?;
    probe.spend()?;
    match timeout(NO_CREDIT, conn.open_uni()).await {
        Err(_) => f.expect(
            true,
            format!("open_uni: no stream credit within {NO_CREDIT:?} (refused)"),
        ),
        Ok(Err(e)) => f.expect(true, format!("open_uni: refused: {e}")),
        Ok(Ok(mut send)) => {
            // Opened: a server that resets it still keeps the limit.
            let _ = send.finish();
            match timeout(NO_CREDIT, send.stopped()).await {
                Ok(Ok(Some(code))) => f.expect(
                    true,
                    format!("open_uni: opened, then stopped by the server with code {code}"),
                ),
                other => f.expect(
                    false,
                    format!("open_uni: a stranger opened a uni stream and it was kept ({other:?})"),
                ),
            }
        }
    }
    f.still_usable(probe, &conn, "the uni stream").await?;
    Ok(f.done())
}

/// Check 3b: a stranger has `bi_streams` bidirectional streams at once
/// and not one more. On a connection that has used none, so every credit
/// is in.
async fn bi_streams(probe: &Probe, config: &Config) -> Result<Outcome, Problem> {
    let mut f = Findings::default();
    let conn = probe.connect().await?;
    let limit = config.expect.bi_streams;
    let mut held = Vec::new();
    for _ in 0..=limit {
        probe.spend()?;
        match timeout(NO_CREDIT, conn.open_bi()).await {
            Ok(Ok(stream)) => held.push(stream),
            Ok(Err(e)) => {
                f.note(format!("open_bi #{}: {e}", held.len() + 1));
                break;
            }
            Err(_) => break,
        }
    }
    f.expect(
        held.len() == limit as usize,
        format!(
            "{} of {} bidirectional streams opened at once (limit {limit})",
            held.len(),
            limit + 1
        ),
    );
    // Dropped unwritten, they are finished, and their credit comes back.
    drop(held);
    let response = probe.request(&conn, None, hello(None)).await?;
    f.expect(
        matches!(response, Response::Ok(Reply::ServerInfo(_))),
        format!("Hello once they are closed: {}", describe(&response)),
    );
    f.still_usable(probe, &conn, "the held streams").await?;
    Ok(f.done())
}

/// Check 3c: a real request (a `Hello` with a long `device_name`) encoded
/// past the stranger frame limit, and within the protocol's own, is
/// refused by its length, `Malformed`; the same connection carries on.
async fn frame_size(probe: &Probe, config: &Config) -> Result<Outcome, Problem> {
    let mut f = Findings::default();
    let (conn, _) = probe.connect_hello().await?;
    let max = config.expect.max_frame as usize;
    let request = hello(Some(format!("spoolframe-probe {}", "x".repeat(max + 1024))));
    let size = probe.encode(None, request.clone())?.len();
    f.note(format!(
        "a Hello whose device_name makes it {size} bytes encoded (limit {max})"
    ));
    let response = probe.request(&conn, None, request).await?;
    f.expect(
        matches!(response, Response::Err(Error::Malformed { .. })),
        format!("its reply: {}", describe(&response)),
    );
    f.still_usable(probe, &conn, "the long request").await?;
    Ok(f.done())
}

/// Check 3d: a stream that sends its kind byte and then nothing (the
/// server cannot see a stream before its first byte) is reset with
/// `FIRST_FRAME_TIMEOUT` once `network.first_frame_timeout_secs` passes.
async fn first_frame(probe: &Probe, config: &Config) -> Result<Outcome, Problem> {
    let mut f = Findings::default();
    let (conn, _) = probe.connect_hello().await?;
    let (mut send, mut recv) = probe.open(&conn).await?;
    if let Err(e) = send.write_u8(bi::RPC).await {
        return Err(Problem::Failed(format!("cannot write the kind byte: {e}")));
    }
    let began = Instant::now();
    let mut buf = [0u8; 16];
    let read = timeout(config.first_frame_wait, recv.read(&mut buf)).await;
    let waited = began.elapsed();
    let want = VarInt::from_u32(reset::FIRST_FRAME_TIMEOUT);
    match read {
        Ok(Err(ReadError::Reset(code))) => f.expect(
            code == want,
            format!(
                "the stream was reset with code {code} after {:.1} s (FIRST_FRAME_TIMEOUT is {want})",
                waited.as_secs_f64()
            ),
        ),
        Ok(other) => f.expect(
            false,
            format!(
                "the stream ended otherwise after {:.1} s: {other:?}",
                waited.as_secs_f64()
            ),
        ),
        Err(_) => f.expect(
            false,
            format!(
                "the stream was still open after {:.1} s",
                config.first_frame_wait.as_secs_f64()
            ),
        ),
    }
    drop((send, recv));
    probe.answered();
    f.still_usable(probe, &conn, "the reset").await?;
    Ok(f.done())
}

/// Check 5: failed logins for a username no server has, made from a fresh
/// key of the check's own: `failures` are refused as wrong, the next is
/// `RateLimited`. Never a real username. Not the run's key: the failures
/// lock their source for the failure window, and the anonymous-pool
/// check's logins from the run's key would then be `RateLimited` too
/// (the third review's G-16).
async fn sign_in_limits(probe: &Probe, config: &Config) -> Result<Outcome, Problem> {
    let username = format!("spoolframe-probe-{}", &probe.id().to_string()[..16]);
    let note = format!("username {username:?}, made up for this run");
    until_limited(probe, config, note, Error::InvalidCredentials, |attempt| {
        Request::LoginPassword(LoginPassword {
            username: username.clone(),
            password: format!("not-the-password-{attempt}"),
            remember: false,
        })
    })
    .await
}

/// Wrong invite codes (P6-07), as [`sign_in_limits`]: `failures` are
/// `InviteInvalid`, the next `RateLimited`. Each guess is counted before
/// the code is looked at (A-13), so guessing a code is as slow as guessing
/// a password. The codes are well formed and no server made them: nothing
/// is redeemed.
async fn invite_limits(probe: &Probe, config: &Config) -> Result<Outcome, Problem> {
    let note = "codes made up for this run".to_owned();
    until_limited(probe, config, note, Error::InviteInvalid, |attempt| {
        Request::RedeemInvite(RedeemInvite {
            // Well formed, and one of 30^12 a server makes at random:
            // never one it made.
            code: format!("2222-2222-222{}", char::from(b'A' + (attempt % 26) as u8)),
            username: "spoolframe-probe".into(),
            display_name: None,
            password: "not-a-password-it-keeps".into(),
        })
    })
    .await
}

/// `config.expect.failures` requests made by `attempt`, from a key of the
/// check's own, each refused `wrong`; then one more, `RateLimited`.
async fn until_limited(
    probe: &Probe,
    config: &Config,
    note: String,
    wrong: Error,
    attempt: impl Fn(u32) -> Request,
) -> Result<Outcome, Problem> {
    let key = bind()
        .await
        .map_err(|e| Problem::Failed(format!("cannot bind the check's key: {e}")))?;
    let outcome = async {
        let mut f = Findings::default();
        let (conn, _) = probe.connect_hello_from(&key).await?;
        f.note(note);
        let failures = config.expect.failures;
        for n in 1..=failures + 1 {
            let response = probe.request(&conn, None, attempt(n)).await?;
            let ok = if n <= failures {
                response == Response::Err(wrong.clone())
            } else {
                matches!(response, Response::Err(Error::RateLimited { .. }))
            };
            f.expect(ok, format!("attempt {n}: {}", describe(&response)));
        }
        f.still_usable(probe, &conn, "the refused attempts").await?;
        Ok(f.done())
    }
    .await;
    key.close().await;
    outcome
}

/// Check 4: stranger connections, one at a time and each with a `Hello`,
/// until one is closed `BUSY` or `cap`+1 are held. All are closed after.
async fn stranger_cap(probe: &Probe, config: &Config) -> Result<Outcome, Problem> {
    let mut f = Findings::default();
    let cap = config.expect.cap as usize;
    sleep(SETTLE).await;
    let mut held: Vec<Conn> = Vec::new();
    let mut busy_at = None;
    while held.len() <= cap {
        sleep(config.cap_pace).await;
        let conn = probe.connect().await?;
        match probe.request(&conn, None, hello(None)).await {
            Ok(Response::Ok(Reply::ServerInfo(_))) => held.push(conn),
            Ok(other) => {
                return Err(Problem::Failed(format!(
                    "connection {}: Hello answered {}",
                    held.len() + 1,
                    describe(&other)
                )));
            }
            Err(Problem::Failed(why)) => match timeout(probe.reply_timeout, conn.closed()).await {
                Ok(ConnectionError::ApplicationClosed(c))
                    if c.error_code == VarInt::from_u32(close::BUSY) =>
                {
                    // The refusal is the server's answer.
                    probe.answered();
                    busy_at = Some(held.len() + 1);
                    break;
                }
                other => {
                    return Err(Problem::Failed(format!(
                        "connection {}: {why}; closed {other:?}",
                        held.len() + 1
                    )));
                }
            },
            Err(stop) => return Err(stop),
        }
    }
    let admitted = held.len();
    drop(held);
    match busy_at {
        Some(n) if n == cap + 1 => f.expect(
            true,
            format!("{admitted} connections admitted; connection {n} closed BUSY (cap {cap})"),
        ),
        // Strangers the probe does not know of hold the rest.
        Some(n) => f.expect(
            true,
            format!(
                "{admitted} connections admitted; connection {n} closed BUSY (cap {cap}: {} \
                 other stranger connections were held)",
                cap + 1 - n
            ),
        ),
        None => f.expect(
            false,
            format!("{admitted} connections admitted, past the cap of {cap}"),
        ),
    }
    // The closes reach the server, and its count goes down.
    sleep(SETTLE).await;
    let (conn, _) = probe.connect_hello().await?;
    f.still_usable(probe, &conn, "closing the connections")
        .await?;
    Ok(f.done())
}

/// Check 6: strangers together cannot fill the anonymous operation pool
/// (PX-09 finding 6's residual, finding 48, the second review's M-1): while
/// `pool_spammers` strangers each try to hold their share of it with refused
/// `ResumeProfile` attempts (before finding 48's fix each refused outcome was
/// kept 30 s; now none is) a further stranger's wrong login
/// still reaches auth (`InvalidCredentials`), not the pool's `RateLimited`.
/// After they stop, the pool drains and a login reaches auth again.
///
/// Each spammer is a fresh endpoint key of its own: the run's budget counts
/// its connection, and the spammer's own budget bounds its streams.
async fn anonymous_pool(probe: &Probe, config: &Config) -> Result<Outcome, Problem> {
    let mut f = Findings::default();
    let id = probe.id().to_string();
    let hex = &id[..8];
    // A wrong login for a username no account has, unique per attempt, so
    // the failure limit never counts one name twice: `InvalidCredentials`
    // means the login reached auth; `RateLimited` with `retry_after_ms` of
    // 1000 means the pool refused it before that.
    let login = |n: u64| {
        Request::LoginPassword(LoginPassword {
            username: format!("spoolframe-probe-pool-{n}-{hex}"),
            password: "not-the-password".into(),
            remember: false,
        })
    };
    let resume = Request::ResumeProfile {
        user_id: format!("local:spoolframe-probe-pool-{hex}"),
    };

    // Baseline: a wrong login reaches auth before anyone spams.
    {
        let (conn, _) = probe.connect_hello().await?;
        let response = probe.request(&conn, None, login(0)).await?;
        f.expect(
            response == Response::Err(Error::InvalidCredentials),
            format!("baseline login: {}", describe(&response)),
        );
    }

    // The spammers: fresh endpoint keys, each sending a resume the server
    // refuses `Unauthenticated` at about 3 a second, for `pool_seconds`.
    let spammers = config.pool_spammers;
    let seconds = config.pool_seconds;
    f.note(format!(
        "{spammers} strangers sending ResumeProfile for a user id no account has, \
         for {seconds} s (before finding 48's fix each refused outcome held a pool entry 30 s)"
    ));
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut handles = Vec::new();
    for i in 0..spammers {
        probe.spend()?; // the run's budget counts the spammer's connection
        let target = config.target.clone();
        let resume = resume.clone();
        let budget = seconds as u32 * 4 + 4;
        handles.push(tokio::spawn(async move {
            let spammer = Probe::new(target, budget, Duration::from_secs(10))
                .await
                .map_err(|e| e.to_string())?;
            let (conn, _) = spammer
                .connect_hello()
                .await
                .map_err(|e| format!("{e:?}"))?;
            let mut sent = 0u64;
            while Instant::now() < deadline {
                match spammer.request(&conn, None, resume.clone()).await {
                    // Refused: the attempt still left its outcome in the pool.
                    Ok(Response::Err(Error::Unauthenticated)) => sent += 1,
                    // The spammer's share, or the pool, is full: expected once
                    // it holds its share; the entries expire and free room.
                    Ok(Response::Err(Error::RateLimited {
                        retry_after_ms: 1000,
                    })) => sent += 1,
                    Ok(other) => {
                        return Ok(format!("spammer {i}: unexpected {}", describe(&other)));
                    }
                    Err(e) => return Ok(format!("spammer {i}: stopped: {e:?}")),
                }
                sleep(Duration::from_millis(330)).await;
            }
            Ok::<String, String>(format!("spammer {i}: {sent} refused resumes sent"))
        }));
    }

    // While they spam, past the time their shares take to fill: a fresh
    // stranger's wrong login must still reach auth.
    sleep(Duration::from_secs(seconds * 2 / 3)).await;
    for n in 1..=3u64 {
        let (conn, _) = probe.connect_hello().await?;
        let response = probe.request(&conn, None, login(n)).await?;
        f.expect(
            response == Response::Err(Error::InvalidCredentials),
            format!("login {n} while the pool is held: {}", describe(&response)),
        );
        sleep(Duration::from_secs(5)).await;
    }

    // Stop the spammers; the pool drains as their entries expire.
    for h in handles {
        match h.await {
            Ok(Ok(line)) => f.note(line),
            Ok(Err(why)) => f.note(format!("a spammer stopped early: {why}")),
            Err(e) => return Err(Problem::Failed(format!("a spammer task failed: {e}"))),
        }
    }

    // Recovery: a login reaches auth again once the pool has drained.
    let drain = Instant::now() + config.pool_drain;
    let mut n = 10u64;
    loop {
        let (conn, _) = probe.connect_hello().await?;
        let response = probe.request(&conn, None, login(n)).await?;
        n += 1;
        if response == Response::Err(Error::InvalidCredentials) {
            f.expect(
                true,
                format!("login after the spammers stopped: {}", describe(&response)),
            );
            break;
        }
        f.note(format!(
            "login while the pool drains: {}",
            describe(&response)
        ));
        if Instant::now() >= drain {
            f.expect(
                false,
                "a login never reached auth again after the spammers stopped".to_owned(),
            );
            break;
        }
        sleep(Duration::from_secs(5)).await;
    }
    Ok(f.done())
}
