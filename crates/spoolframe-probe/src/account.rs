//! The signed-in checks: what a consumer account may and may not do, as
//! that account, over live QUIC and the web listener.
//!
//! Safe on a real server by construction:
//! - The run signs in once over QUIC, from its own fresh key, and at most
//!   once on the web. A refused sign-in is never tried again: a wrong
//!   password counts towards the name's lockout.
//! - What it expects to be refused is harmless if it is not: admin reads
//!   only (never an admin mutation), and requests that name ids no server
//!   has (a passkey, a library, an item made up from the run's key).
//! - Nothing changes the account: no password, PIN, passkey, profile or
//!   watch state. The probe never enters a PIN: a profile locked on a new
//!   device skips the checks that need it.
//! - Every session it makes is signed out at the end ([`AccountState::finish`]),
//!   past the budget if need be, and what the account still lists is
//!   reported.

use std::sync::Arc;
use std::time::Duration;

use iroh::Endpoint;
use iroh::endpoint::{ConnectionError, VarInt};
use serde_json::{Value, json};
use spoolframe_protocol::{Error, LoginPassword, Reply, Request, Response, UserSummary, close};
use spoolframe_types::{LibraryId, MediaId, MediaKind, Namespace, Role};
use tokio::sync::Mutex;
use tokio::time::timeout;

use crate::checks::{Findings, NO_CREDIT, Outcome};
use crate::web::{self, Answer, WebSession};
use crate::wire::{Conn, Probe, Problem, bind, describe};
use crate::{Account, Config};

/// How long a signed-out connection is given to be closed: the server's
/// `SIGN_OUT_DRAIN` (2 s) and a reply's time.
const SIGN_OUT_WAIT: Duration = Duration::from_secs(5);

/// The device name the probe's sign-in shows in the account's devices.
const DEVICE_NAME: &str = "spoolframe-probe";

/// Admin reads a consumer must be refused, with the params each takes
/// (`admin.userList` is `admin_userList` on the wire: jsonrpsee joins a
/// namespace with `_`).
/// Reads only: were one answered, nothing on the server would change.
fn admin_reads() -> [(&'static str, Value); 9] {
    [
        ("admin_userList", json!([])),
        ("admin_serverStatus", json!([])),
        ("admin_settings", json!([])),
        ("admin_setupState", json!([])),
        ("admin_libraries", json!([])),
        ("admin_activity", json!([])),
        ("admin_playbacks", json!([])),
        ("admin_telemetryStatus", json!([])),
        (
            "admin_logs",
            json!([{ "after": null, "level": null, "text": null, "limit": 1 }]),
        ),
    ]
}

/// The run's QUIC sign-in as the account.
pub(crate) struct Signed {
    /// The account's own fresh key: not the run's, which the stranger
    /// checks may have locked out of signing in (`sign-in-limits`).
    endpoint: Endpoint,
    conn: Conn,
    session: String,
    user: UserSummary,
}

enum Quic {
    NotYet,
    In(Arc<Signed>),
    /// `session-reread` signed it out.
    Out,
    Refused(String),
}

enum Browser {
    NotYet,
    In(Arc<WebSession>),
    Refused(String),
}

/// Why a signed-in check cannot run.
pub(crate) enum NotSigned {
    Skip(String),
    Problem(Problem),
}

impl From<Problem> for NotSigned {
    fn from(p: Problem) -> Self {
        NotSigned::Problem(p)
    }
}

/// A check's way out when it cannot run.
macro_rules! ready {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(NotSigned::Skip(why)) => return Ok(Outcome::skip(&why)),
            Err(NotSigned::Problem(p)) => return Err(p),
        }
    };
}

/// The account to sign in as, if the run was given one.
fn asked(config: &Config) -> Result<&Account, NotSigned> {
    config.account.as_ref().ok_or_else(|| {
        NotSigned::Skip(
            "not asked for: pass --username and --password-file (a consumer account)".into(),
        )
    })
}

/// The run's sessions as the account, made when a check first needs one.
pub(crate) struct AccountState {
    quic: Mutex<Quic>,
    web: Mutex<Browser>,
    /// The key the account signed in from, once it has.
    key: parking_lot::Mutex<Option<Endpoint>>,
    /// What was done as the account, for the report.
    log: parking_lot::Mutex<Vec<String>>,
}

impl AccountState {
    pub fn new() -> Self {
        Self {
            quic: Mutex::new(Quic::NotYet),
            web: Mutex::new(Browser::NotYet),
            key: parking_lot::Mutex::default(),
            log: parking_lot::Mutex::default(),
        }
    }

    fn note(&self, line: String) {
        self.log.lock().push(line);
    }

    /// The QUIC session, signing in the first time.
    async fn signed(&self, probe: &Probe, config: &Config) -> Result<Arc<Signed>, NotSigned> {
        let account = asked(config)?;
        let mut quic = self.quic.lock().await;
        match &*quic {
            Quic::In(signed) => return Ok(signed.clone()),
            Quic::Out => return Err(NotSigned::Skip("signed out already".into())),
            Quic::Refused(why) => {
                return Err(NotSigned::Skip(format!("the sign-in failed: {why}")));
            }
            Quic::NotYet => {}
        }
        let endpoint = bind()
            .await
            .map_err(|e| Problem::Failed(format!("cannot bind the account's key: {e}")))?;
        *self.key.lock() = Some(endpoint.clone());
        let (conn, _) = probe
            .connect_hello_with(&endpoint, Some(DEVICE_NAME.into()))
            .await?;
        let login = Request::LoginPassword(LoginPassword {
            username: account.username.clone(),
            password: account.password.clone(),
            remember: false,
        });
        let session = match probe.request(&conn, None, login).await? {
            // Never described: the reply holds the session token.
            Response::Ok(Reply::LoggedIn {
                session,
                remembered,
            }) => {
                self.note(format!(
                    "QUIC: signed in as {:?} from a fresh key {} (remembered {remembered})",
                    account.username,
                    endpoint.id().fmt_short()
                ));
                session.0
            }
            other => {
                let why = format!(
                    "LoginPassword as {:?}: {}",
                    account.username,
                    describe(&other)
                );
                self.note(format!("QUIC: {why}"));
                *quic = Quic::Refused(why.clone());
                // Not tried again, by this check or any other.
                return Err(NotSigned::Problem(Problem::Failed(why)));
            }
        };
        let user = match probe.request(&conn, Some(&session), Request::WhoAmI).await {
            Ok(Response::Ok(Reply::WhoAmI(user))) => user,
            other => {
                let why = match other {
                    Ok(response) => describe(&response),
                    Err(p) => problem(p),
                };
                let why = format!("WhoAmI after signing in: {why}");
                // Signed in all the same: out again at once.
                let out = match logout(probe, &endpoint, &conn, &session).await {
                    Ok(done) => done,
                    Err(e) => format!("NOT signed out: {e}"),
                };
                self.note(format!("QUIC: {why}; {out}"));
                *quic = Quic::Refused(why.clone());
                return Err(NotSigned::Problem(Problem::Failed(why)));
            }
        };
        let signed = Arc::new(Signed {
            endpoint,
            conn,
            session,
            user,
        });
        *quic = Quic::In(signed.clone());
        Ok(signed)
    }

    /// As [`Self::signed`], for a check that needs the profile unlocked.
    async fn unlocked(&self, probe: &Probe, config: &Config) -> Result<Arc<Signed>, NotSigned> {
        let signed = self.signed(probe, config).await?;
        if signed.user.locked {
            return Err(NotSigned::Skip(
                "the profile is PIN-locked on a new device, and the probe enters no PIN".into(),
            ));
        }
        Ok(signed)
    }

    /// The web session, signing in the first time.
    async fn browser(&self, probe: &Probe, config: &Config) -> Result<Arc<WebSession>, NotSigned> {
        let account = asked(config)?;
        let Some(site) = &account.web else {
            return Err(NotSigned::Skip("no web listener given (--web)".into()));
        };
        let mut web = self.web.lock().await;
        match &*web {
            Browser::In(session) => return Ok(session.clone()),
            Browser::Refused(why) => {
                return Err(NotSigned::Skip(format!("the web sign-in failed: {why}")));
            }
            Browser::NotYet => {}
        }
        match web::login(probe, site, &account.username, &account.password).await {
            Ok(session) => {
                self.note(format!(
                    "web: signed in as {:?} at {}",
                    account.username, site.url
                ));
                let session = Arc::new(session);
                *web = Browser::In(session.clone());
                Ok(session)
            }
            Err(Problem::Failed(why)) => {
                self.note(format!("web: {why}"));
                *web = Browser::Refused(why.clone());
                Err(NotSigned::Problem(Problem::Failed(why)))
            }
            Err(stop) => Err(stop.into()),
        }
    }

    /// Sign out whatever is still signed in, and say what the account
    /// still lists. Run after the checks, whatever happened in them.
    pub async fn finish(&self, probe: &Probe, config: &Config) -> Vec<String> {
        probe.cleaning_up();
        let quic = std::mem::replace(&mut *self.quic.lock().await, Quic::Out);
        if let Quic::In(signed) = quic {
            let line = match logout(probe, &signed.endpoint, &signed.conn, &signed.session).await {
                Ok(done) => format!("QUIC: {done}"),
                Err(why) => format!("QUIC: NOT signed out: {why}"),
            };
            self.note(line);
        }
        let web = std::mem::replace(&mut *self.web.lock().await, Browser::NotYet);
        if let Browser::In(session) = web {
            self.left_behind(probe, &session).await;
            let line = match session.logout(probe).await {
                Ok(204) => "web: signed out (POST /auth/logout: 204)".to_owned(),
                Ok(status) => format!("web: POST /auth/logout answered {status}"),
                Err(Problem::Failed(why)) => format!("web: NOT signed out: {why}"),
                Err(Problem::Stop(stop)) => format!("web: NOT signed out: {stop}"),
            };
            self.note(line);
        } else if config.account.as_ref().is_some_and(|a| a.web.is_none()) {
            self.note(
                "the account's sessions and devices were not listed: no web listener given".into(),
            );
        }
        let key = self.key.lock().take();
        if let Some(key) = key {
            key.close().await;
        }
        std::mem::take(&mut *self.log.lock())
    }

    /// What the account lists from its web session, every QUIC session of
    /// the run's ended by now: the key it signed in from is marked.
    async fn left_behind(&self, probe: &Probe, session: &WebSession) {
        let own = self
            .key
            .lock()
            .as_ref()
            .map(|k| k.id().to_string())
            .unwrap_or_default();
        match session.call(probe, "me_sessions", json!([])).await {
            Ok(Answer::Result(Value::Array(sessions))) => {
                let probe_sessions = sessions
                    .iter()
                    .filter(|s| s["endpointId"].as_str() == Some(own.as_str()))
                    .count();
                self.note(format!(
                    "me.sessions: {} live, {probe_sessions} of them from this run's QUIC key, \
                     1 of them this web session",
                    sessions.len()
                ));
            }
            Ok(other) => self.note(format!("me.sessions: {}", other.describe())),
            Err(Problem::Failed(why)) => self.note(format!("me.sessions: {why}")),
            Err(Problem::Stop(stop)) => self.note(format!("me.sessions: {stop}")),
        }
        match session.call(probe, "me_devices", json!([])).await {
            Ok(Answer::Result(Value::Array(devices))) => {
                let listed: Vec<String> = devices
                    .iter()
                    .map(|d| {
                        let id = d["endpointId"].as_str().unwrap_or("?");
                        let name = d["name"].as_str().unwrap_or("(no name)");
                        let mark = if id == own { " (this run's)" } else { "" };
                        format!("{}… {name:?}{mark}", &id[..id.len().min(10)])
                    })
                    .collect();
                self.note(format!(
                    "me.devices: {} listed: {}",
                    devices.len(),
                    listed.join(", ")
                ));
            }
            Ok(other) => self.note(format!("me.devices: {}", other.describe())),
            Err(Problem::Failed(why)) => self.note(format!("me.devices: {why}")),
            Err(Problem::Stop(stop)) => self.note(format!("me.devices: {stop}")),
        }
    }
}

/// `Logout` for `session` on `conn` or, if that is gone, on a new
/// connection from the same key.
async fn logout(
    probe: &Probe,
    endpoint: &Endpoint,
    conn: &Conn,
    session: &str,
) -> Result<String, String> {
    let response = match probe.request(conn, Some(session), Request::Logout).await {
        Ok(r) => r,
        Err(_) => {
            let (conn, _) = probe
                .connect_hello_from(endpoint)
                .await
                .map_err(|e| format!("cannot reconnect: {}", problem(e)))?;
            probe
                .request(&conn, Some(session), Request::Logout)
                .await
                .map_err(problem)?
        }
    };
    match response {
        Response::Ok(Reply::LoggedOut) => Ok("signed out (Logout: LoggedOut)".into()),
        other => Err(format!("Logout: {}", describe(&other))),
    }
}

fn problem(p: Problem) -> String {
    match p {
        Problem::Failed(why) => why,
        Problem::Stop(stop) => stop.to_string(),
    }
}

/// A key no server has, from the run's own: for made-up ids.
fn made_up(probe: &Probe) -> String {
    let id = probe.id().to_string();
    format!("spoolframe-probe-{}", &id[..16])
}

/// consumer-admin-refused: the account is a consumer on both sides, and
/// the web listener's admin reads refuse it with `FORBIDDEN`. The
/// protocol has no admin-only request (authz-matrix): admin work is the
/// web listener's `admin.*` methods.
pub(crate) async fn consumer_admin_refused(
    probe: &Probe,
    config: &Config,
    state: &AccountState,
) -> Result<Outcome, Problem> {
    let mut f = Findings::default();
    let signed = ready!(state.signed(probe, config).await);
    f.expect(
        signed.user.role == Role::Consumer,
        format!("QUIC WhoAmI: role {:?}", signed.user.role),
    );
    f.note("the protocol has no admin-only request (authz-matrix); admin.* is web only".into());
    let web = match state.browser(probe, config).await {
        Ok(web) => web,
        Err(NotSigned::Skip(why)) => {
            f.note(format!("admin.* not checked: {why}"));
            return Ok(f.done());
        }
        Err(NotSigned::Problem(Problem::Failed(why))) => {
            f.expect(false, why);
            return Ok(f.done());
        }
        Err(NotSigned::Problem(stop)) => return Err(stop),
    };
    match web.call(probe, "me_whoAmI", json!([])).await? {
        Answer::Result(user) => f.expect(
            user["role"] == "consumer",
            format!("me.whoAmI: role {}", user["role"]),
        ),
        other => f.expect(false, format!("me.whoAmI: {}", other.describe())),
    }
    for (method, params) in admin_reads() {
        let answer = web.call(probe, method, params).await?;
        f.expect(
            matches!(answer, Answer::Error { code, .. } if code == web::code::FORBIDDEN),
            format!("{method}: {}", answer.describe()),
        );
    }
    Ok(f.done())
}

/// session-bound (G3): the session, sent from another key,
/// is `Unauthenticated`, and still works from its own.
pub(crate) async fn session_bound(
    probe: &Probe,
    config: &Config,
    state: &AccountState,
) -> Result<Outcome, Problem> {
    let mut f = Findings::default();
    let signed = ready!(state.unlocked(probe, config).await);
    f.note(format!(
        "the session is sent from the run's own key {}, not the account's {}",
        probe.id().fmt_short(),
        signed.endpoint.id().fmt_short()
    ));
    {
        let (conn, _) = probe.connect_hello().await?;
        for (what, request) in [
            ("WhoAmI", Request::WhoAmI),
            ("Libraries", Request::Libraries),
        ] {
            f.expect_error(
                probe,
                &conn,
                &format!("{what} with the session, from the other key"),
                Some(&signed.session),
                request,
                |e| *e == Error::Unauthenticated,
            )
            .await?;
        }
    }
    let response = probe
        .request(&signed.conn, Some(&signed.session), Request::WhoAmI)
        .await?;
    f.expect(
        matches!(&response, Response::Ok(Reply::WhoAmI(u)) if u.username == signed.user.username),
        format!("WhoAmI from its own key after: {}", short_who(&response)),
    );
    Ok(f.done())
}

/// visibility: what `Libraries` lists for the account, and that a library
/// or an item on a Plex server the server does not have is `NotFound`.
pub(crate) async fn visibility(
    probe: &Probe,
    config: &Config,
    state: &AccountState,
) -> Result<Outcome, Problem> {
    let mut f = Findings::default();
    let signed = ready!(state.unlocked(probe, config).await);
    let response = probe
        .request(&signed.conn, Some(&signed.session), Request::Libraries)
        .await?;
    match &response {
        Response::Ok(Reply::Libraries { libraries }) => {
            let names: Vec<String> = libraries
                .iter()
                .map(|l| format!("{:?} ({}, {:?})", l.title, l.id, l.kind))
                .collect();
            f.note(format!(
                "Libraries: {}: {}",
                libraries.len(),
                if names.is_empty() {
                    "none".into()
                } else {
                    names.join(", ")
                }
            ));
        }
        other => f.expect(false, format!("Libraries: {}", describe(other))),
    }
    // A server key no server has: refused before any catalog is asked.
    let key = format!("{}:1", made_up(probe));
    let library = LibraryId::new(Namespace::Plex, key.clone())
        .map_err(|e| Problem::Failed(format!("a made-up library id: {e}")))?;
    let item = MediaId::new(Namespace::Plex, MediaKind::Movie, key)
        .map_err(|e| Problem::Failed(format!("a made-up media id: {e}")))?;
    f.expect_error(
        probe,
        &signed.conn,
        &format!("Items of {library}, not listed"),
        Some(&signed.session),
        Request::Items {
            library,
            offset: 0,
            limit: 1,
        },
        |e| matches!(e, Error::NotFound { .. }),
    )
    .await?;
    f.expect_error(
        probe,
        &signed.conn,
        &format!("Item {item}, not listed"),
        Some(&signed.session),
        Request::Item { id: item },
        |e| matches!(e, Error::NotFound { .. }),
    )
    .await?;
    f.note(
        "not checked: an item of a section the server does not serve, on a Plex server \
         it has; only an admin can list such a section"
            .into(),
    );
    Ok(f.done())
}

/// recent-sign-in (RS): on a session signed in moments ago, a request
/// that needs a recent sign-in gets past that check. `RemovePasskey` of a
/// passkey id no account has: `NotFound` past RS, nothing removed.
pub(crate) async fn recent_sign_in(
    probe: &Probe,
    config: &Config,
    state: &AccountState,
) -> Result<Outcome, Problem> {
    let mut f = Findings::default();
    let signed = ready!(state.unlocked(probe, config).await);
    let id = made_up(probe);
    let response = probe
        .request(
            &signed.conn,
            Some(&signed.session),
            Request::RemovePasskey { id: id.clone() },
        )
        .await?;
    f.expect(
        matches!(response, Response::Err(Error::NotFound { .. })),
        format!(
            "RemovePasskey {id:?} (no such passkey) on a fresh session: {} (RecentSignInRequired \
             would mean RS refused a fresh sign-in)",
            describe(&response)
        ),
    );
    f.note(
        "not checked live: the refusal 10 min after signing in (the run does not wait); \
         in-process: the server's recent-sign-in tests"
            .into(),
    );
    Ok(f.done())
}

/// signed-in-streams: the signed-in connection takes more bidirectional
/// streams at once than a stranger's limit. Opened unwritten, as
/// `bi-streams` does, up to twice that limit.
pub(crate) async fn signed_in_streams(
    probe: &Probe,
    config: &Config,
    state: &AccountState,
) -> Result<Outcome, Problem> {
    let mut f = Findings::default();
    let signed = ready!(state.signed(probe, config).await);
    let stranger = config.expect.bi_streams;
    let want = 2 * stranger;
    let mut held = Vec::new();
    for _ in 0..want {
        probe.spend()?;
        match timeout(NO_CREDIT, signed.conn.open_bi()).await {
            Ok(Ok(stream)) => held.push(stream),
            Ok(Err(e)) => {
                f.note(format!("open_bi #{}: {e}", held.len() + 1));
                break;
            }
            Err(_) => break,
        }
    }
    f.expect(
        held.len() as u32 > stranger,
        format!(
            "{} of {want} bidirectional streams opened at once on the signed-in connection \
             (a stranger's limit is {stranger})",
            held.len()
        ),
    );
    drop(held);
    let response = probe
        .request(&signed.conn, Some(&signed.session), Request::WhoAmI)
        .await?;
    f.expect(
        matches!(response, Response::Ok(Reply::WhoAmI(_))),
        format!("WhoAmI once they are closed: {}", short_who(&response)),
    );
    Ok(f.done())
}

/// session-reread (G3, re-read on every request): `Ping` and `WhoAmI`
/// work while signed in; `Logout` closes the device's connection
/// `SIGNED_OUT`, and the session is `Unauthenticated` after, from the
/// same key. The run's QUIC session ends here.
pub(crate) async fn session_reread(
    probe: &Probe,
    config: &Config,
    state: &AccountState,
) -> Result<Outcome, Problem> {
    let mut f = Findings::default();
    let signed = ready!(state.signed(probe, config).await);
    let ping = probe
        .request(&signed.conn, Some(&signed.session), Request::Ping)
        .await?;
    f.expect(
        ping == Response::Ok(Reply::Pong),
        format!("Ping with the session: {}", describe(&ping)),
    );
    for n in 1..=2 {
        let who = probe
            .request(&signed.conn, Some(&signed.session), Request::WhoAmI)
            .await?;
        f.expect(
            matches!(&who, Response::Ok(Reply::WhoAmI(u)) if u.username == signed.user.username),
            format!("WhoAmI #{n}: {}", short_who(&who)),
        );
    }
    let out = probe
        .request(&signed.conn, Some(&signed.session), Request::Logout)
        .await?;
    // From here the session is over whatever the rest finds: `finish`
    // does not sign it out again.
    *state.quic.lock().await = Quic::Out;
    let logged_out = out == Response::Ok(Reply::LoggedOut);
    f.expect(logged_out, format!("Logout: {}", describe(&out)));
    state.note(if logged_out {
        "QUIC: signed out by session-reread (Logout: LoggedOut)".into()
    } else {
        format!("QUIC: Logout in session-reread answered {}", describe(&out))
    });
    let want = VarInt::from_u32(close::SIGNED_OUT);
    match timeout(SIGN_OUT_WAIT, signed.conn.closed()).await {
        Ok(ConnectionError::ApplicationClosed(c)) => f.expect(
            c.error_code == want,
            format!(
                "the connection was closed with code {} (SIGNED_OUT is {want})",
                c.error_code
            ),
        ),
        Ok(other) => f.expect(false, format!("the connection ended otherwise: {other}")),
        Err(_) => f.expect(
            false,
            format!("the connection was still open {SIGN_OUT_WAIT:?} after Logout"),
        ),
    }
    let (conn, _) = probe.connect_hello_from(&signed.endpoint).await?;
    f.expect_error(
        probe,
        &conn,
        "WhoAmI with the signed-out session, same key, new connection",
        Some(&signed.session),
        Request::WhoAmI,
        |e| *e == Error::Unauthenticated,
    )
    .await?;
    Ok(f.done())
}

/// A `WhoAmI` reply, short: who and how, never more.
fn short_who(response: &Response) -> String {
    match response {
        Response::Ok(Reply::WhoAmI(u)) => {
            format!(
                "WhoAmI({:?}, {:?}, locked {})",
                u.username, u.role, u.locked
            )
        }
        other => describe(other),
    }
}
