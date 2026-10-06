//! Passkeys (P3-05): the WebAuthn relying party and its ceremonies.
//!
//! A passkey is bound to a relying-party ID, a host the browser checks: the
//! host of `admin.public_url`, where the web UI is served, or `localhost`
//! without one (the listener reached directly or over an SSH tunnel). It
//! comes from the configuration, never from a request's headers. WebAuthn
//! runs only in a secure context, so a `public_url` that is plain HTTP on
//! another host than `localhost`, or names an address, has no passkeys.
//!
//! Passkeys are discoverable (the authenticator stores them, so signing in
//! needs no username), always with user verification, and nothing here
//! narrows where one may live or how it is reached: no authenticator
//! attachment, no transports, no hints. The device's own store, a password
//! manager, a security key and a phone over the browser's QR code (the
//! hybrid transport) are all the browser's to offer.
//!
//! A ceremony's challenge is kept here, in memory, for
//! [`CEREMONY_LIFETIME`]: a restart drops it, and the browser begins again.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use webauthn_rs::prelude::{
    CreationChallengeResponse, DiscoverableAuthentication, DiscoverableKey, Passkey,
    PasskeyRegistration, PublicKeyCredential, RegisterPublicKeyCredential,
    RequestChallengeResponse, Url, Uuid, Webauthn, WebauthnBuilder, WebauthnError,
};
use webauthn_rs_proto::{
    AuthenticatorSelectionCriteria, ResidentKeyRequirement, UserVerificationPolicy,
};

use spoolframe_protocol::ServerId;

use crate::admin_http::Origins;
use crate::auth::{Auth, SessionToken};
use crate::store::passkeys::{PasskeyError, PasskeyRecord, passkey_name};
use crate::store::users::{LocalUserId, UserRecord};
use crate::store::{Store, StoreError};

/// How long a begun ceremony can be finished: the time WebAuthn gives the
/// person at the browser's prompt.
pub const CEREMONY_LIFETIME: Duration = Duration::from_secs(5 * 60);

/// Sign-ins that may be begun and not yet finished. Anyone can begin one,
/// so they are counted: past this, a new one waits for an old one to end.
pub const MAX_SIGN_INS: usize = 10_000;

/// Sign-ins one source (a remote address) may have begun and not finished
/// (PX-10): a person signs in once at a time, so one source cannot fill
/// [`MAX_SIGN_INS`]. A caller with no source (loopback, or a proxy not in
/// `admin.trusted_proxies`) is not counted apart.
pub const MAX_SIGN_INS_PER_SOURCE: usize = 16;

/// What the relying party is called in a browser's prompt.
const RP_NAME: &str = "Spoolframe";

/// The relying-party ID without an `admin.public_url`.
const LOCALHOST: &str = "localhost";

#[derive(Debug, thiserror::Error)]
pub enum CeremonyError {
    /// This server cannot offer passkeys; why.
    #[error("{0}")]
    Unavailable(String),
    /// Finishing what was not begun, or was begun too long ago.
    #[error("that passkey request is over; begin again")]
    NoCeremony,
    /// What the browser sent does not finish the registration.
    #[error("the passkey could not be added: {0}")]
    Refused(String),
    #[error(transparent)]
    Passkey(#[from] PasskeyError),
    #[error("{0}")]
    Internal(String),
}

/// Why a passkey signed nobody in.
#[derive(Debug, thiserror::Error)]
pub enum SignInError {
    /// The one answer a caller gets; the log has the reason.
    #[error("that passkey did not sign in")]
    Refused,
    #[error("too many failed logins; try again in {retry_after:?}")]
    RateLimited { retry_after: Duration },
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Internal(String),
}

impl From<StoreError> for SignInError {
    fn from(e: StoreError) -> Self {
        SignInError::Internal(e.to_string())
    }
}

impl From<StoreError> for CeremonyError {
    fn from(e: StoreError) -> Self {
        CeremonyError::Passkey(e.into())
    }
}

/// What the limit on failed logins counts a passkey of `user`'s under,
/// besides its source: apart from their password's, so neither uses up
/// the other's tries.
pub(crate) fn limit_key(user: LocalUserId) -> String {
    format!("passkey:{user}")
}

/// The host passkeys are bound to.
pub(crate) struct RelyingParty {
    pub(crate) webauthn: Webauthn,
    pub(crate) rp_id: String,
    pub(crate) origin: String,
    /// Whether an assertion may come without a user handle, its passkey's
    /// user then the one its credential id is stored as. A browser always
    /// sends the handle of a discoverable passkey, and WebAuthn wants it
    /// checked; the apps' webauthn-authenticator-rs, on the Linux client,
    /// gets none from a phone over caBLE, and sends `None`.
    pub(crate) user_handle_optional: bool,
}

impl RelyingParty {
    /// The relying party `rp_id`, at `origin` only.
    pub(crate) fn at(rp_id: &str, origin: &str) -> Result<Self, String> {
        let url = Url::parse(origin).map_err(|e| format!("{origin:?} is not a URL: {e}"))?;
        let webauthn = WebauthnBuilder::new(rp_id, &url)
            .map(|builder| builder.rp_name(RP_NAME))
            .and_then(WebauthnBuilder::build)
            .map_err(|e| format!("passkeys cannot be bound to {origin}: {e}"))?;
        Ok(Self {
            webauthn,
            rp_id: rp_id.to_owned(),
            origin: origin.to_owned(),
            user_handle_optional: false,
        })
    }

    /// Verify what an authenticator made for the registration `state`:
    /// the passkey's credential id (base64url, as stored) and the
    /// credential as JSON.
    pub(crate) fn made(
        &self,
        made: &RegisterPublicKeyCredential,
        state: &PasskeyRegistration,
    ) -> Result<(String, String), CeremonyError> {
        let passkey = self
            .webauthn
            .finish_passkey_registration(made, state)
            .map_err(|e| CeremonyError::Refused(e.to_string()))?;
        let json = serde_json::to_string(&passkey)
            .map_err(|e| CeremonyError::Internal(format!("a credential as JSON: {e}")))?;
        Ok((URL_SAFE_NO_PAD.encode(passkey.cred_id()), json))
    }

    /// The user `signed` signs in, if it is a passkey of theirs made for
    /// this relying party and signed for the ceremony `begun` gives:
    /// `begun` is handed the passkey's credential id (base64url) and
    /// takes the ceremony's state, or says why there is none. A failure
    /// counts in `auth`'s limit on failed logins, under the passkey's user
    /// and `source`, and the log has its reason.
    pub(crate) async fn sign_in(
        &self,
        auth: &Auth,
        signed: &PublicKeyCredential,
        source: Option<&str>,
        begun: impl FnOnce(&str) -> Result<DiscoverableAuthentication, &'static str>,
    ) -> Result<UserRecord, SignInError> {
        let store = auth.store();
        let id = URL_SAFE_NO_PAD.encode(signed.get_credential_id());
        let record = store.passkey(&id).await?;
        // Counted as a failure before it is checked, as a password is. A
        // passkey nobody has is counted too, all under one name: there is
        // no one for those to lock out.
        let what = match &record {
            Some(record) => limit_key(record.user),
            None => "passkey:unknown".to_owned(),
        };
        let began = auth
            .attempt_begin(&what, source)
            .map_err(|retry_after| SignInError::RateLimited { retry_after })?;
        // Taken whatever comes next: a ceremony is tried once.
        let state = begun(&id);
        let refused = |why: &str| {
            tracing::warn!(
                passkey = record.as_ref().map(|r| r.name.as_str()),
                user = record.as_ref().map(|r| tracing::field::display(r.user)),
                rp_id = %self.rp_id,
                reason = why,
                "passkey sign-in refused"
            );
            SignInError::Refused
        };
        let Some(record) = &record else {
            return Err(refused("not a passkey of this server"));
        };
        let state = state.map_err(refused)?;
        if record.rp_id != self.rp_id {
            return Err(refused("the passkey was made for another host"));
        }
        // The user handle its authenticator stored with it is its user's.
        // Without one, where that is allowed, the credential id alone says
        // whose it is: ids are unique, and the signature is checked
        // against the key stored with it.
        if signed.response.user_handle.is_some() || !self.user_handle_optional {
            let handle = self
                .webauthn
                .identify_discoverable_authentication(signed)
                .map(|(handle, _)| handle.to_string())
                .map_err(|e| refused(&e.to_string()))?;
            if store.passkey_handle_user(&handle).await? != Some(record.user) {
                return Err(refused("the user handle is not the passkey's user's"));
            }
        }
        let mut passkey = credential(record).map_err(|e| SignInError::Internal(e.to_string()))?;
        let verified = self
            .webauthn
            .finish_discoverable_authentication(signed, state, &[DiscoverableKey::from(&passkey)])
            .map_err(|e| match e {
                WebauthnError::CredentialPossibleCompromise => refused(
                    "the signature counter went backwards: there may be a copy of this passkey",
                ),
                other => refused(&other.to_string()),
            })?;
        let Some(user) = store
            .user_by_id(record.user)
            .await
            .map_err(|e| SignInError::Internal(e.to_string()))?
            .filter(|user| user.disabled_at.is_none())
        else {
            return Err(refused("the user is disabled"));
        };
        passkey.update_credential(&verified);
        let json = serde_json::to_string(&passkey)
            .map_err(|e| SignInError::Internal(format!("a credential as JSON: {e}")))?;
        store.passkey_used(&record.credential_id, &json).await?;
        auth.attempt_succeeded(&what, source, began);
        Ok(user)
    }
}

/// A sign-in begun: its state, when it is over, and the source that
/// began it, if it has one.
type SignInBegun = (DiscoverableAuthentication, Instant, Option<String>);

/// The server's passkeys: its relying party, if it can be one, and the
/// ceremonies begun.
pub struct Passkeys {
    party: Result<RelyingParty, String>,
    /// The apps' relying party for this server (PX-14): its passkeys are
    /// listed here too, as usable in the apps.
    app_rp_id: String,
    /// Registrations begun, by the session (its token's hash) that began
    /// them: one at a time each.
    registrations: Mutex<HashMap<String, (PasskeyRegistration, Instant)>>,
    /// Sign-ins begun, by the ceremony id the browser was given.
    sign_ins: Mutex<HashMap<String, SignInBegun>>,
    /// First admins' registrations begun (P3-01), by the ceremony id the
    /// browser was given: there is no session, or user, to key them by.
    setups: Mutex<HashMap<String, (Setup, Instant)>>,
}

/// A first admin's registration, begun.
struct Setup {
    state: PasskeyRegistration,
    username: String,
    /// The user handle the passkey is made with: the admin's, once they
    /// exist.
    handle: Uuid,
}

/// A passkey that finished a first admin's registration, to be kept with
/// the admin it makes ([`crate::store::Store::add_first_admin_with_passkey`]).
pub struct SetupPasskey {
    /// The username it was begun for.
    pub username: String,
    pub handle: String,
    pub credential_id: String,
    /// webauthn-rs' credential, as JSON.
    pub credential: String,
    pub rp_id: String,
}

impl std::fmt::Debug for Passkeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Passkeys")
            .field("rp_id", &self.rp_id())
            .finish_non_exhaustive()
    }
}

impl Passkeys {
    /// The relying party for browsers coming from `origins`, on the server
    /// whose id is `server_id`.
    pub fn new(origins: &Origins, server_id: &ServerId) -> Self {
        Self::at(origins, server_id, None)
    }

    /// As [`Passkeys::new`], for the admin listener on `port`: without a
    /// public URL, a browser's page is `http://localhost:<port>`.
    pub fn at(origins: &Origins, server_id: &ServerId, port: Option<u16>) -> Self {
        Self {
            party: relying_party(origins, port),
            app_rp_id: server_id.rp_id(),
            registrations: Mutex::default(),
            sign_ins: Mutex::default(),
            setups: Mutex::default(),
        }
    }

    fn party(&self) -> Result<&RelyingParty, CeremonyError> {
        self.party
            .as_ref()
            .map_err(|why| CeremonyError::Unavailable(why.clone()))
    }

    /// The relying-party ID and the origin it is served at, or why there
    /// is none.
    pub fn host(&self) -> Result<(&str, &str), CeremonyError> {
        let party = self.party()?;
        Ok((&party.rp_id, &party.origin))
    }

    /// The relying-party ID, if the server can be one.
    pub fn rp_id(&self) -> Option<&str> {
        self.party.as_ref().ok().map(|party| party.rp_id.as_str())
    }

    /// The apps' relying party for this server (PX-14).
    pub fn app_rp_id(&self) -> &str {
        &self.app_rp_id
    }

    /// Why the server has no passkeys, if it has none.
    pub fn unavailable(&self) -> Option<&str> {
        self.party.as_ref().err().map(String::as_str)
    }

    /// Begin adding a passkey for `user`, in the session whose token hashes
    /// to `session`: what the browser's `navigator.credentials.create`
    /// takes. A registration the session began before is dropped.
    pub async fn begin_registration(
        &self,
        store: &Store,
        session: &str,
        user: &UserRecord,
    ) -> Result<CreationChallengeResponse, CeremonyError> {
        let party = self.party()?;
        let handle = store
            .passkey_user_handle(user.id, &Uuid::new_v4().to_string())
            .await?;
        let handle = Uuid::parse_str(&handle)
            .map_err(|e| CeremonyError::Internal(format!("a stored user handle: {e}")))?;
        // The authenticator that already holds one of these is not asked
        // for a second.
        let existing = store
            .list_passkeys(user.id)
            .await?
            .iter()
            .filter(|p| p.rp_id == party.rp_id)
            .map(|p| credential(p).map(|c| c.cred_id().clone()))
            .collect::<Result<Vec<_>, _>>()?;
        let display_name = user.display_name.as_deref().unwrap_or(&user.username);
        let (mut options, state) = party
            .webauthn
            .start_passkey_registration(handle, &user.username, display_name, Some(existing))
            .map_err(|e| CeremonyError::Internal(e.to_string()))?;
        stored_on_the_authenticator(&mut options);
        let mut registrations = self
            .registrations
            .lock()
            .expect("never held across a panic");
        let now = Instant::now();
        registrations.retain(|_, (_, expires_at)| *expires_at > now);
        registrations.insert(session.to_owned(), (state, now + CEREMONY_LIFETIME));
        Ok(options)
    }

    /// Finish the registration `session` began, with what the browser
    /// made, and keep the passkey as `name`.
    pub async fn finish_registration(
        &self,
        store: &Store,
        session: &str,
        user: &UserRecord,
        name: &str,
        made: &RegisterPublicKeyCredential,
    ) -> Result<PasskeyRecord, CeremonyError> {
        let party = self.party()?;
        // Before the challenge is used up: a name can be typed again.
        passkey_name(name)?;
        let state = self
            .registrations
            .lock()
            .expect("never held across a panic")
            .remove(session)
            .filter(|(_, expires_at)| *expires_at > Instant::now())
            .map(|(state, _)| state)
            .ok_or(CeremonyError::NoCeremony)?;
        let (id, json) = party.made(made, &state)?;
        let record = store
            .add_passkey(user.id, &id, name, &json, &party.rp_id)
            .await?;
        tracing::info!(username = %user.username, passkey = %record.name, rp_id = %party.rp_id, "passkey added");
        Ok(record)
    }

    /// Begin registering the first admin, `username`, who does not exist
    /// yet (P3-01): the ceremony's id, and what the browser's
    /// `navigator.credentials.create` takes. Whether setup is open is the
    /// caller's to check.
    pub fn begin_setup(
        &self,
        username: &str,
    ) -> Result<(String, CreationChallengeResponse), CeremonyError> {
        let party = self.party()?;
        let handle = Uuid::new_v4();
        let (mut options, state) = party
            .webauthn
            .start_passkey_registration(handle, username, username, None)
            .map_err(|e| CeremonyError::Internal(e.to_string()))?;
        stored_on_the_authenticator(&mut options);
        let mut setups = self.setups.lock().expect("never held across a panic");
        let now = Instant::now();
        // Not counted as sign-ins are: only who has the setup code begins
        // one.
        setups.retain(|_, (_, expires_at)| *expires_at > now);
        let ceremony = SessionToken::generate().0;
        let setup = Setup {
            state,
            username: username.to_owned(),
            handle,
        };
        setups.insert(ceremony.clone(), (setup, now + CEREMONY_LIFETIME));
        Ok((ceremony, options))
    }

    /// Finish the first admin's registration `ceremony` with what the
    /// browser made: the passkey, not kept yet. A ceremony is tried once.
    pub fn finish_setup(
        &self,
        ceremony: &str,
        made: &RegisterPublicKeyCredential,
    ) -> Result<SetupPasskey, CeremonyError> {
        let party = self.party()?;
        let setup = self
            .setups
            .lock()
            .expect("never held across a panic")
            .remove(ceremony)
            .filter(|(_, expires_at)| *expires_at > Instant::now())
            .map(|(setup, _)| setup)
            .ok_or(CeremonyError::NoCeremony)?;
        let (credential_id, credential) = party.made(made, &setup.state)?;
        Ok(SetupPasskey {
            username: setup.username,
            handle: setup.handle.to_string(),
            credential_id,
            credential,
            rp_id: party.rp_id.clone(),
        })
    }

    /// Begin a sign-in with no username: the ceremony's id, and what the
    /// browser's `navigator.credentials.get` takes. No credential is
    /// listed, so the browser offers the passkeys it finds for this host,
    /// on the device or on a phone.
    ///
    /// At most [`MAX_SIGN_INS_PER_SOURCE`] are held for one `source`.
    pub fn begin_sign_in(
        &self,
        source: Option<&str>,
    ) -> Result<(String, RequestChallengeResponse), SignInError> {
        let party = self
            .party
            .as_ref()
            .map_err(|why| SignInError::Unavailable(why.clone()))?;
        let (mut options, state) = party
            .webauthn
            .start_discoverable_authentication()
            .map_err(|e| SignInError::Internal(e.to_string()))?;
        // webauthn-rs asks for the browser's autofill here ("conditional"
        // mediation), which shows no prompt. This sign-in is a button: the
        // browser's own prompt, with its "use a phone" QR code.
        options.mediation = None;
        let mut sign_ins = self.sign_ins.lock().expect("never held across a panic");
        let now = Instant::now();
        sign_ins.retain(|_, (_, expires_at, _)| *expires_at > now);
        let from_source = source.map_or(0, |source| {
            sign_ins
                .values()
                .filter(|(_, _, from)| from.as_deref() == Some(source))
                .count()
        });
        if sign_ins.len() >= MAX_SIGN_INS || from_source >= MAX_SIGN_INS_PER_SOURCE {
            return Err(SignInError::RateLimited {
                retry_after: CEREMONY_LIFETIME,
            });
        }
        let ceremony = SessionToken::generate().0;
        sign_ins.insert(
            ceremony.clone(),
            (state, now + CEREMONY_LIFETIME, source.map(str::to_owned)),
        );
        Ok((ceremony, options))
    }

    /// Finish the sign-in `ceremony` with what the browser's passkey
    /// signed: the user it signs in. A failure counts in `auth`'s limit on
    /// failed logins, under the passkey's user and `source`.
    pub async fn finish_sign_in(
        &self,
        auth: &Auth,
        ceremony: &str,
        signed: &PublicKeyCredential,
        source: Option<&str>,
    ) -> Result<UserRecord, SignInError> {
        let party = self
            .party
            .as_ref()
            .map_err(|why| SignInError::Unavailable(why.clone()))?;
        party
            .sign_in(auth, signed, source, |_| {
                self.sign_ins
                    .lock()
                    .expect("never held across a panic")
                    .remove(ceremony)
                    .filter(|(_, expires_at, _)| *expires_at > Instant::now())
                    .map(|(state, _, _)| state)
                    .ok_or("the sign-in was not begun, or is over")
            })
            .await
    }

    /// Say in the log which stored passkeys this server's host no longer
    /// serves: made under another relying-party ID (before
    /// `admin.public_url` changed, or, for an app's, before each server
    /// had its own, PX-14), no browser or app offers them here.
    pub async fn log_orphans(&self, store: &Store) -> Result<(), StoreError> {
        // With no relying party at all, every web passkey is one.
        let rp_id = self.rp_id().unwrap_or_default();
        for passkey in store
            .passkeys_of_other_hosts(rp_id, &self.app_rp_id)
            .await?
        {
            tracing::warn!(
                username = %passkey.username,
                passkey = %passkey.name,
                made_for = %passkey.rp_id,
                now = %rp_id,
                "this passkey was made for another host and cannot sign in here"
            );
        }
        Ok(())
    }
}

/// The relying party for `origins`, with the admin listener on `port`,
/// or why the server cannot be one.
fn relying_party(origins: &Origins, port: Option<u16>) -> Result<RelyingParty, String> {
    let (origin, any_port) = match (origins, port) {
        // On the listener's own host a browser has a secure context only
        // as `localhost`, and its page is at the listener's port (PX-09
        // finding 28): taking any port let a page on another localhost
        // port (a dev server, another app) have a passkey sign for this
        // relying party and relay it here. A tunnel keeps the port.
        (Origins::SameHost, Some(port)) => (format!("http://{LOCALHOST}:{port}"), false),
        // Its port not known (tests that bind port 0): any.
        (Origins::SameHost, None) => (format!("http://{LOCALHOST}"), true),
        (Origins::Exactly(origin), _) => (origin.clone(), false),
    };
    let url = Url::parse(&origin).map_err(|e| format!("{origin:?} is not a URL: {e}"))?;
    let Some(rp_id) = url.domain() else {
        return Err(format!(
            "passkeys need a host name, and admin.public_url ({origin}) has an address"
        ));
    };
    if url.scheme() != "https" && rp_id != LOCALHOST {
        return Err(format!(
            "passkeys need HTTPS, and admin.public_url ({origin}) is plain HTTP"
        ));
    }
    let webauthn = WebauthnBuilder::new(rp_id, &url)
        .map(|builder| builder.rp_name(RP_NAME).allow_any_port(any_port))
        .and_then(WebauthnBuilder::build)
        .map_err(|e| format!("passkeys cannot be bound to {origin}: {e}"))?;
    Ok(RelyingParty {
        webauthn,
        rp_id: rp_id.to_owned(),
        origin,
        user_handle_optional: false,
    })
}

/// Ask for a passkey the authenticator stores (a discoverable credential:
/// signing in needs no username), wherever that authenticator is.
/// webauthn-rs asks for one it need not store, which a security key then
/// does not, and which no sign-in without a username would find.
pub(crate) fn stored_on_the_authenticator(options: &mut CreationChallengeResponse) {
    options.public_key.authenticator_selection = Some(AuthenticatorSelectionCriteria {
        authenticator_attachment: None,
        resident_key: Some(ResidentKeyRequirement::Required),
        require_resident_key: true,
        user_verification: UserVerificationPolicy::Required,
    });
    options.public_key.hints = None;
}

/// A stored passkey as webauthn-rs verifies against it.
pub(crate) fn credential(record: &PasskeyRecord) -> Result<Passkey, CeremonyError> {
    serde_json::from_str(&record.credential)
        .map_err(|e| CeremonyError::Internal(format!("a stored credential: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn party(origins: Origins) -> Passkeys {
        Passkeys::new(&origins, &ServerId::from_bytes([0; 10]))
    }

    #[test]
    fn the_relying_party_is_the_public_url_s_host() {
        let passkeys = party(Origins::Exactly("https://media.example.org".into()));
        assert_eq!(
            passkeys.host().unwrap(),
            ("media.example.org", "https://media.example.org")
        );
        // Another port is part of the origin, not of the relying-party ID.
        let passkeys = party(Origins::Exactly("https://media.example.org:8443".into()));
        assert_eq!(
            passkeys.host().unwrap(),
            ("media.example.org", "https://media.example.org:8443")
        );
    }

    #[test]
    fn without_a_public_url_it_is_localhost() {
        let passkeys = party(Origins::SameHost);
        assert_eq!(passkeys.host().unwrap(), ("localhost", "http://localhost"));
        // Behind a proxy that is reached as localhost, too.
        let passkeys = party(Origins::Exactly("http://localhost:8080".into()));
        assert_eq!(passkeys.rp_id(), Some("localhost"));
    }

    #[test]
    fn plain_http_elsewhere_and_an_address_have_no_passkeys() {
        let http = party(Origins::Exactly("http://media.lan".into()));
        assert_eq!(http.rp_id(), None);
        assert!(http.unavailable().unwrap().contains("HTTPS"), "{http:?}");
        assert!(matches!(http.host(), Err(CeremonyError::Unavailable(_))));
        let address = party(Origins::Exactly("https://192.0.2.7".into()));
        assert!(
            address.unavailable().unwrap().contains("host name"),
            "{address:?}"
        );
    }
}
