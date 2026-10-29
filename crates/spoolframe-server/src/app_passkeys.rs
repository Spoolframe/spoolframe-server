//! Passkeys in the apps (P3-19): a second relying party, this server's
//! own under `spoolframe.app` (PX-14), `<server id>.spoolframe.app`, that
//! needs nothing configured.
//!
//! A platform's passkey API (Apple's first) works only for a domain the
//! app is associated with when it is built, and a self-hosted server's own
//! domain is not known then. So the apps make and use their passkeys for
//! a subdomain of `spoolframe.app`, named by the server id
//! ([`ServerId::rp_id`]), at the origin ([`ServerId::origin`]) the
//! platform writes into `clientDataJSON`. One association file at
//! `spoolframe.app` covers every subdomain. The ceremonies run over
//! `spoolframe/client/1`; the server never contacts `spoolframe.app`. It
//! does not depend on `admin.public_url`, so a LAN-only server, whose web
//! UI has no passkeys, has them in the apps.
//!
//! Every server once shared `spoolframe.app` itself. A security key with
//! passkeys for two servers then answered a sign-in that listed none with
//! both, and the server whose passkey the platform did not take refused
//! it. Passkeys stored for the shared relying party
//! ([`spoolframe_protocol::LEGACY_APP_RP_ID`]) are kept, listed as app
//! passkeys that do not sign in here, so their users see to add them
//! again; they sign no one in, as one made for another host never does.
//!
//! App passkeys are kept with the web UI's ([`crate::passkeys`]), under
//! this server's app relying-party ID, which is what marks them: the
//! same list in My profile, the same removal and last-method rule, the
//! same user handle (random, one per user and server) and the same limit
//! on failed sign-ins.
//!
//! A challenge is the device's that asked for it, for [`CEREMONY_LIFETIME`]
//! and once: a registration's and a confirmation's (P3-07) are kept under
//! the session that began them, with its endpoint, a sign-in's under the
//! device's endpoint, so another device can finish none, and a sign-in's
//! challenge confirms nothing.
//!
//! The device is sent a nonce, not the challenge: the challenge is
//! [`app_passkey_challenge`] of this server's endpoint id and the nonce,
//! and the app's SDK makes it with the id it dialled (PX-09, finding 2).
//! The relying party does not do that: it comes from the server id a
//! server names, and a server may name this one's. So without the
//! challenge, a server that claims this server's id, begins a ceremony
//! here and hands the request to an app as its own would have the app
//! sign for here. With it, the app signs for the server it talks to, and
//! this one refuses that. A passkey made at one server is unknown at
//! another.
//!
//! Anyone can begin a sign-in, so it lists no passkeys, even for a
//! username: those would tell anyone a user's credential ids. Passkeys are
//! made discoverable, so the platform finds them.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;
use serde::de::DeserializeOwned;
use spoolframe_protocol::{
    FinishPasskeyRegistration, FinishPasskeySignIn, NONCE_LEN, PasskeyRegistrationBegun,
    PasskeySignInBegun, ServerId, app_passkey_challenge,
};
use webauthn_rs::prelude::{
    CredentialID, DiscoverableAuthentication, PasskeyRegistration, PublicKeyCredential,
    RegisterPublicKeyCredential, Uuid,
};
use webauthn_rs_proto::{AuthenticatorAssertionResponseRaw, AuthenticatorAttestationResponseRaw};

use crate::auth::Auth;
use crate::passkeys::{
    CEREMONY_LIFETIME, CeremonyError, MAX_SIGN_INS, RelyingParty, SignInError, credential,
};
use crate::store::Store;
use crate::store::passkeys::{PasskeyRecord, passkey_name};
use crate::store::users::{LocalUserId, UserRecord, normalize_username};

/// The apps' relying party, and the ceremonies begun with it.
pub struct AppPasskeys {
    /// The server's id, which `ServerInfo` names it by.
    server_id: ServerId,
    /// `<server id>.spoolframe.app`, at its origin only.
    party: RelyingParty,
    /// How long a begun ceremony can be finished.
    lifetime: Duration,
    /// Registrations begun, by the session (its token's hash) that began
    /// them, with its endpoint: one at a time each.
    registrations: Mutex<HashMap<String, (Registration, Instant)>>,
    /// Sign-ins begun, by the endpoint that began them: one at a time
    /// each.
    sign_ins: Mutex<HashMap<String, (SignIn, Instant)>>,
    /// Confirmations begun (P3-07), by the session (its token's hash)
    /// that began them, with its endpoint: one at a time each. Only a
    /// session begins one, so they are as many as the sessions at most.
    confirms: Mutex<HashMap<String, (Confirm, Instant)>>,
    /// Accounts begun with an invite and a passkey (feature
    /// `spoolframe:invite-passkeys`), by the endpoint that began them: one
    /// at a time each, as sign-ins.
    invites: Mutex<HashMap<String, (InviteRegistration, Instant)>>,
    /// What ceremonies expire by (PX-09).
    clock: crate::clock::Clock,
}

struct Confirm {
    endpoint: String,
    sign_in: SignIn,
}

struct Registration {
    endpoint: String,
    state: PasskeyRegistration,
}

/// A registration [`AppPasskeys::start`] began.
struct Started {
    /// The user handle, as the options carry it.
    user_id: Vec<u8>,
    nonce: Vec<u8>,
    state: PasskeyRegistration,
}

/// An account to make with an invite once its passkey is: who it is to
/// be, and the invite's code (its hash), which the finish names again.
struct InviteRegistration {
    code_hash: String,
    user: LocalUserId,
    username: String,
    display_name: Option<String>,
    handle: String,
    state: PasskeyRegistration,
}

/// An account to make with an invite and its passkey, made and verified
/// ([`AppPasskeys::finish_invite_registration`]): what
/// [`Store::redeem_invite_with_passkey`] keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvitePasskey {
    /// The invite's code's hash.
    pub code_hash: String,
    /// The id the account is made with: chosen at the beginning.
    pub user: LocalUserId,
    pub username: String,
    pub display_name: Option<String>,
    /// The WebAuthn user handle the passkey was made with.
    pub handle: String,
    pub credential_id: String,
    /// The credential, as JSON.
    pub credential: String,
    /// As [`passkey_name`] gives it.
    pub name: String,
    pub rp_id: String,
}

struct SignIn {
    state: DiscoverableAuthentication,
    /// The passkeys (base64url credential ids) it asked for; empty: any.
    allowed: Vec<String>,
}

impl std::fmt::Debug for AppPasskeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppPasskeys")
            .field("rp_id", &self.party.rp_id)
            .finish_non_exhaustive()
    }
}

impl AppPasskeys {
    /// The app passkeys of the server whose id is `server_id`.
    pub fn new(server_id: ServerId) -> Self {
        Self::with_lifetime(server_id, CEREMONY_LIFETIME)
    }

    /// As [`AppPasskeys::new`], with ceremonies that last `lifetime`
    /// (tests shorten it).
    pub fn with_lifetime(server_id: ServerId, lifetime: Duration) -> Self {
        Self {
            party: RelyingParty {
                user_handle_optional: true,
                ..RelyingParty::at(&server_id.rp_id(), &server_id.origin()).expect(
                    "a server id is a DNS label, so its subdomain of spoolframe.app is a \
                     relying party",
                )
            },
            server_id,
            lifetime,
            registrations: Mutex::default(),
            sign_ins: Mutex::default(),
            confirms: Mutex::default(),
            invites: Mutex::default(),
            clock: crate::clock::Clock::default(),
        }
    }

    /// The server's id.
    pub fn server_id(&self) -> &ServerId {
        &self.server_id
    }

    /// The relying party of this server's app passkeys.
    pub fn rp_id(&self) -> &str {
        &self.party.rp_id
    }

    /// Ceremonies expire by `clock` (tests move it instead of sleeping).
    pub fn with_clock(mut self, clock: crate::clock::Clock) -> Self {
        self.clock = clock;
        self
    }

    /// A registration of a passkey with user handle `handle`, named
    /// `user_name` and shown as `display_name`, for this server's endpoint
    /// id `server`; `exclude`, the passkeys the user has here already.
    fn start(
        &self,
        server: &[u8; 32],
        handle: Uuid,
        user_name: &str,
        display_name: &str,
        exclude: Option<Vec<CredentialID>>,
    ) -> Result<Started, String> {
        let (options, state) = self
            .party
            .webauthn
            .start_passkey_registration(handle, user_name, display_name, exclude)
            .map_err(|e| e.to_string())?;
        let nonce = nonce();
        let state = with_challenge(state, "rs", &app_passkey_challenge(server, &nonce))?;
        Ok(Started {
            user_id: options.public_key.user.id.to_vec(),
            nonce,
            state,
        })
    }

    /// Room in `ceremonies` for one `endpoint` begins at `now`, the
    /// expired ones dropped. Anyone can make an endpoint, so they are
    /// counted, as the web's are: one more, when they are all taken, is
    /// refused.
    fn room_for<T>(
        &self,
        ceremonies: &mut HashMap<String, (T, Instant)>,
        endpoint: &str,
        now: Instant,
    ) -> Result<(), SignInError> {
        ceremonies.retain(|_, (_, expires_at)| *expires_at > now);
        if !ceremonies.contains_key(endpoint) && ceremonies.len() >= MAX_SIGN_INS {
            return Err(SignInError::RateLimited {
                retry_after: self.lifetime,
            });
        }
        Ok(())
    }

    /// Begin making an app passkey for `user` on `endpoint`, in the session
    /// whose token hashes to `session`, on the server called
    /// `server_name`, whose endpoint id is `server`. A registration the
    /// session began before is dropped. Whether the session signed in
    /// recently enough is the caller's to check.
    pub async fn begin_registration(
        &self,
        store: &Store,
        server: &[u8; 32],
        session: &str,
        endpoint: &str,
        user: &UserRecord,
        server_name: &str,
    ) -> Result<PasskeyRegistrationBegun, CeremonyError> {
        let handle = store
            .passkey_user_handle(user.id, &Uuid::new_v4().to_string())
            .await?;
        let handle = Uuid::parse_str(&handle)
            .map_err(|e| CeremonyError::Internal(format!("a stored user handle: {e}")))?;
        let existing = store
            .list_passkeys(user.id)
            .await?
            .iter()
            .filter(|p| p.rp_id == self.party.rp_id)
            .map(|p| credential(p).map(|c| c.cred_id().clone()))
            .collect::<Result<Vec<_>, _>>()?;
        let exclude_credential_ids = existing.iter().map(|id| id.to_vec()).collect();
        // The relying party is an opaque id: a password manager shows the
        // account's name, and the server's name in it says which server it
        // is. The display name is the user's own.
        let display_name = user.display_name.as_deref().unwrap_or(&user.username);
        let user_name = format!("{} @ {server_name}", user.username);
        let Started {
            user_id,
            nonce,
            state,
        } = self
            .start(server, handle, &user_name, display_name, Some(existing))
            .map_err(CeremonyError::Internal)?;
        let mut registrations = self.registrations.lock();
        let now = self.clock.now();
        registrations.retain(|_, (_, expires_at)| *expires_at > now);
        let registration = Registration {
            endpoint: endpoint.to_owned(),
            state,
        };
        registrations.insert(session.to_owned(), (registration, now + self.lifetime));
        Ok(PasskeyRegistrationBegun {
            nonce,
            user_id,
            user_name,
            display_name: display_name.to_owned(),
            exclude_credential_ids,
        })
    }

    /// Finish the registration `session` began on `endpoint`, with what the
    /// app's platform made, and keep the passkey as `user`'s.
    pub async fn finish_registration(
        &self,
        store: &Store,
        session: &str,
        endpoint: &str,
        user: &UserRecord,
        made: &FinishPasskeyRegistration,
    ) -> Result<PasskeyRecord, CeremonyError> {
        // Before the challenge is used up: a name can be sent again.
        let name = passkey_name(&made.name)?;
        let state = self
            .registrations
            .lock()
            .remove(session)
            .filter(|(r, expires_at)| *expires_at > self.clock.now() && r.endpoint == endpoint)
            .map(|(r, _)| r.state)
            .ok_or(CeremonyError::NoCeremony)?;
        let (id, json) = self.party.made(&registered(made), &state)?;
        let record = store
            .add_passkey(user.id, &id, name, &json, &self.party.rp_id)
            .await?;
        tracing::info!(username = %user.username, passkey = %record.name, "app passkey added");
        Ok(record)
    }

    /// Begin making account `username` (normalized), shown as
    /// `display_name`, on `endpoint` with this server's endpoint id
    /// `server`, called `server_name`, with the invite whose code hashes to
    /// `code_hash` and an app passkey (feature
    /// `spoolframe:invite-passkeys`). The account's id and user handle are
    /// chosen now; nothing is kept but the ceremony. The
    /// invite and the username are the caller's to check. A registration
    /// the endpoint began before is dropped.
    pub fn begin_invite_registration(
        &self,
        server: &[u8; 32],
        endpoint: &str,
        code_hash: String,
        username: String,
        display_name: Option<String>,
        server_name: &str,
    ) -> Result<PasskeyRegistrationBegun, SignInError> {
        let handle = Uuid::new_v4();
        let shown = display_name.as_deref().unwrap_or(&username).to_owned();
        let user_name = format!("{username} @ {server_name}");
        let Started {
            user_id,
            nonce,
            state,
        } = self
            .start(server, handle, &user_name, &shown, None)
            .map_err(SignInError::Internal)?;
        let mut invites = self.invites.lock();
        let now = self.clock.now();
        self.room_for(&mut invites, endpoint, now)?;
        let registration = InviteRegistration {
            code_hash,
            user: LocalUserId::generate(),
            username,
            display_name,
            handle: handle.to_string(),
            state,
        };
        invites.insert(endpoint.to_owned(), (registration, now + self.lifetime));
        Ok(PasskeyRegistrationBegun {
            nonce,
            user_id,
            user_name,
            display_name: shown,
            exclude_credential_ids: Vec::new(),
        })
    }

    /// Finish the account `endpoint` began with the invite whose code
    /// hashes to `code_hash`, with what the app's platform made: what to
    /// make it with. Once: the registration is taken whether or not the
    /// passkey verifies.
    pub fn finish_invite_registration(
        &self,
        endpoint: &str,
        code_hash: &str,
        made: &FinishPasskeyRegistration,
    ) -> Result<InvitePasskey, CeremonyError> {
        // Before the registration is taken: a name can be sent again.
        let name = passkey_name(&made.name)?.to_owned();
        let begun = self
            .invites
            .lock()
            .remove(endpoint)
            .filter(|(r, expires_at)| *expires_at > self.clock.now() && r.code_hash == code_hash)
            .map(|(r, _)| r)
            .ok_or(CeremonyError::NoCeremony)?;
        let (credential_id, credential) = self.party.made(&registered(made), &begun.state)?;
        Ok(InvitePasskey {
            code_hash: begun.code_hash,
            user: begun.user,
            username: begun.username,
            display_name: begun.display_name,
            handle: begun.handle,
            credential_id,
            credential,
            name,
            rp_id: self.party.rp_id.clone(),
        })
    }

    /// Begin a sign-in on `endpoint`, with this server's endpoint id
    /// `server`: only `username`'s app passkeys finish it, or, without
    /// one, any. It lists none, whoever `username` is: anyone can ask. A
    /// sign-in the endpoint began before is dropped. A username that is
    /// nobody's, or whose user has no app passkey, is as none.
    pub async fn begin_sign_in(
        &self,
        store: &Store,
        server: &[u8; 32],
        endpoint: &str,
        username: Option<&str>,
    ) -> Result<PasskeySignInBegun, SignInError> {
        let allowed = match username.map(normalize_username) {
            Some(Ok(username)) => store.passkey_ids_of(&username, &self.party.rp_id).await?,
            Some(Err(_)) | None => Vec::new(),
        };
        let (_, state) = self
            .party
            .webauthn
            .start_discoverable_authentication()
            .map_err(|e| SignInError::Internal(e.to_string()))?;
        let nonce = nonce();
        let state = with_challenge(state, "ast", &app_passkey_challenge(server, &nonce))
            .map_err(SignInError::Internal)?;
        let mut sign_ins = self.sign_ins.lock();
        let now = self.clock.now();
        self.room_for(&mut sign_ins, endpoint, now)?;
        sign_ins.insert(
            endpoint.to_owned(),
            (SignIn { state, allowed }, now + self.lifetime),
        );
        Ok(PasskeySignInBegun {
            nonce,
            allow_credential_ids: Vec::new(),
        })
    }

    /// Finish the sign-in `endpoint` began with what the passkey signed:
    /// the user it signs in. A failure counts in `auth`'s limit on failed
    /// logins, under the passkey's user and the endpoint.
    pub async fn finish_sign_in(
        &self,
        auth: &Auth,
        endpoint: &str,
        signed: &FinishPasskeySignIn,
    ) -> Result<UserRecord, SignInError> {
        self.party
            .sign_in(auth, &asserted(signed), Some(endpoint), |id| {
                let (sign_in, _) = self
                    .sign_ins
                    .lock()
                    .remove(endpoint)
                    .filter(|(_, expires_at)| *expires_at > self.clock.now())
                    .ok_or("the sign-in was not begun on this device, or is over")?;
                if !sign_in.allowed.is_empty() && !sign_in.allowed.iter().any(|a| a == id) {
                    return Err("the passkey is not one the sign-in asked for");
                }
                Ok(sign_in.state)
            })
            .await
    }

    /// Begin confirming it is `user` (P3-07), in the session whose token
    /// hashes to `session`, on `endpoint`, with this server's endpoint id
    /// `server`: a nonce for their app passkeys, listed, and only theirs:
    /// the session is theirs. A confirmation the session began before is
    /// dropped. `None` if they have no app passkey: an empty list would
    /// let the platform offer any.
    pub async fn begin_confirm(
        &self,
        store: &Store,
        server: &[u8; 32],
        session: &str,
        endpoint: &str,
        user: LocalUserId,
    ) -> Result<Option<PasskeySignInBegun>, SignInError> {
        let allowed: Vec<String> = store
            .list_passkeys(user)
            .await?
            .into_iter()
            .filter(|p| p.rp_id == self.party.rp_id)
            .map(|p| p.credential_id)
            .collect();
        if allowed.is_empty() {
            return Ok(None);
        }
        let allow_credential_ids = allowed
            .iter()
            .map(|id| URL_SAFE_NO_PAD.decode(id))
            .collect::<Result<_, _>>()
            .map_err(|e| SignInError::Internal(format!("a stored credential id: {e}")))?;
        let (_, state) = self
            .party
            .webauthn
            .start_discoverable_authentication()
            .map_err(|e| SignInError::Internal(e.to_string()))?;
        let nonce = nonce();
        let state = with_challenge(state, "ast", &app_passkey_challenge(server, &nonce))
            .map_err(SignInError::Internal)?;
        let mut confirms = self.confirms.lock();
        let now = self.clock.now();
        confirms.retain(|_, (_, expires_at)| *expires_at > now);
        let confirm = Confirm {
            endpoint: endpoint.to_owned(),
            sign_in: SignIn { state, allowed },
        };
        confirms.insert(session.to_owned(), (confirm, now + self.lifetime));
        Ok(Some(PasskeySignInBegun {
            nonce,
            allow_credential_ids,
        }))
    }

    /// Finish the confirmation `session` began on `endpoint` with what the
    /// passkey signed: the user it is, which the caller compares with the
    /// session's. Only a passkey the challenge listed passes, so another
    /// user's never does. A failure counts in `auth`'s limit on failed
    /// logins, as a passkey sign-in's does.
    pub async fn finish_confirm(
        &self,
        auth: &Auth,
        session: &str,
        endpoint: &str,
        signed: &FinishPasskeySignIn,
    ) -> Result<UserRecord, SignInError> {
        self.party
            .sign_in(auth, &asserted(signed), Some(endpoint), |id| {
                let (confirm, _) = self
                    .confirms
                    .lock()
                    .remove(session)
                    .filter(|(c, expires_at)| {
                        *expires_at > self.clock.now() && c.endpoint == endpoint
                    })
                    .ok_or("the confirmation was not begun in this session, or is over")?;
                if !confirm.sign_in.allowed.iter().any(|a| a == id) {
                    return Err("the passkey is not one the confirmation asked for");
                }
                Ok(confirm.sign_in.state)
            })
            .await
    }
}

/// A fresh nonce for a ceremony.
fn nonce() -> Vec<u8> {
    let mut nonce = vec![0; NONCE_LEN];
    getrandom::fill(&mut nonce).expect("the OS gives random bytes");
    nonce
}

/// A ceremony's `state` from webauthn-rs, with `challenge` in the place of
/// the random one it made: webauthn-rs takes no challenge of the caller's.
/// Its state is JSON with the challenge, base64url, at
/// `<inner>.challenge`; one that is not is an error, never a state with
/// the old challenge.
///
/// That layout is webauthn-rs's own, not its API (CR-11 L59). The test
/// `tests::an_app_passkey_is_made_and_signs_in_with_the_challenge_the_app_derives`
/// runs a registration and a sign-in through this with a soft token: bump
/// `webauthn-rs` in the workspace `Cargo.toml` only with it passing.
fn with_challenge<T: Serialize + DeserializeOwned>(
    state: T,
    inner: &str,
    challenge: &[u8],
) -> Result<T, String> {
    let mut json =
        serde_json::to_value(state).map_err(|e| format!("a ceremony's state as JSON: {e}"))?;
    let Some(slot @ serde_json::Value::String(_)) = json
        .get_mut(inner)
        .and_then(|state| state.get_mut("challenge"))
    else {
        return Err(format!("a ceremony's state has no {inner}.challenge"));
    };
    *slot = serde_json::Value::String(URL_SAFE_NO_PAD.encode(challenge));
    serde_json::from_value(json).map_err(|e| format!("a ceremony's state from JSON: {e}"))
}

/// What the app sent as webauthn-rs takes it: the browser's JSON, without
/// a browser.
fn registered(made: &FinishPasskeyRegistration) -> RegisterPublicKeyCredential {
    RegisterPublicKeyCredential {
        id: URL_SAFE_NO_PAD.encode(&made.credential_id),
        raw_id: made.credential_id.clone().into(),
        response: AuthenticatorAttestationResponseRaw {
            attestation_object: made.attestation_object.clone().into(),
            client_data_json: made.client_data_json.clone().into(),
            transports: None,
        },
        type_: "public-key".to_owned(),
        extensions: Default::default(),
    }
}

fn asserted(signed: &FinishPasskeySignIn) -> PublicKeyCredential {
    PublicKeyCredential {
        id: URL_SAFE_NO_PAD.encode(&signed.credential_id),
        raw_id: signed.credential_id.clone().into(),
        response: AuthenticatorAssertionResponseRaw {
            authenticator_data: signed.authenticator_data.clone().into(),
            client_data_json: signed.client_data_json.clone().into(),
            signature: signed.signature.clone().into(),
            user_handle: signed.user_handle.clone().map(Into::into),
        },
        extensions: Default::default(),
        type_: "public-key".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use webauthn_authenticator_rs::AuthenticatorBackend as _;
    use webauthn_authenticator_rs::softtoken::SoftToken;
    use webauthn_rs::prelude::{DiscoverableKey, Passkey, Url};

    use super::*;

    /// This server's endpoint id, as the app knows it.
    const SERVER: [u8; 32] = [0x5a; 32];

    /// `value` again, through its JSON: a ceremony's state is used up by
    /// finishing it, and webauthn-rs gives it no `Clone`.
    fn copy<T: Serialize + DeserializeOwned>(value: &T) -> T {
        serde_json::from_value(serde_json::to_value(value).unwrap()).unwrap()
    }

    /// CR-11 L59: [`with_challenge`] writes into webauthn-rs's private
    /// ceremony state. This runs a registration and a sign-in through it,
    /// with the soft token signing the challenge the app derives
    /// ([`app_passkey_challenge`]), never the one webauthn-rs made. A
    /// webauthn-rs whose state no longer has `rs.challenge` or
    /// `ast.challenge`, or no longer checks the one there, fails here
    /// rather than every app passkey ceremony in production. Confirming
    /// (P3-07) uses the sign-in's state type and slot.
    #[test]
    fn an_app_passkey_is_made_and_signs_in_with_the_challenge_the_app_derives() {
        let party = RelyingParty::at("l59.spoolframe.app", "https://l59.spoolframe.app").unwrap();
        let origin = Url::parse(&party.origin).unwrap();
        let mut token = SoftToken::new(true).unwrap().0;

        // Registration: the state takes the derived challenge.
        let handle = Uuid::new_v4();
        let (creation, state) = party
            .webauthn
            .start_passkey_registration(handle, "alice", "Alice", None)
            .unwrap();
        let challenge = app_passkey_challenge(&SERVER, &nonce());
        let unchanged = copy(&state);
        let state = with_challenge(state, "rs", &challenge).unwrap();
        let mut options = creation.public_key;
        options.challenge = challenge.clone().into();
        let made = token
            .perform_register(origin.clone(), options, 60_000)
            .unwrap();
        let made = registered(&FinishPasskeyRegistration {
            credential_id: made.raw_id.to_vec(),
            attestation_object: made.response.attestation_object.to_vec(),
            client_data_json: made.response.client_data_json.to_vec(),
            name: "phone".to_owned(),
        });
        // The challenge is load-bearing: the state webauthn-rs made
        // refuses what was signed for the derived one.
        assert!(party.made(&made, &unchanged).is_err());
        let (_, json) = party.made(&made, &state).unwrap();
        let passkey: Passkey = serde_json::from_str(&json).unwrap();

        // Sign-in: the same, with the discoverable ceremony's state.
        let (_, state) = party.webauthn.start_discoverable_authentication().unwrap();
        let challenge = app_passkey_challenge(&SERVER, &nonce());
        let unchanged = copy(&state);
        let state = with_challenge(state, "ast", &challenge).unwrap();
        let options = serde_json::from_value(json!({
            "challenge": URL_SAFE_NO_PAD.encode(&challenge),
            "rpId": party.rp_id,
            "allowCredentials": [{
                "type": "public-key",
                "id": URL_SAFE_NO_PAD.encode(passkey.cred_id()),
            }],
            "userVerification": "required",
        }))
        .unwrap();
        let signed = token.perform_auth(origin, options, 60_000).unwrap();
        let signed = asserted(&FinishPasskeySignIn {
            credential_id: signed.raw_id.to_vec(),
            authenticator_data: signed.response.authenticator_data.to_vec(),
            client_data_json: signed.response.client_data_json.to_vec(),
            signature: signed.response.signature.to_vec(),
            user_handle: Some(handle.as_bytes().to_vec()),
        });
        let keys = [DiscoverableKey::from(&passkey)];
        assert!(
            party
                .webauthn
                .finish_discoverable_authentication(&signed, unchanged, &keys)
                .is_err()
        );
        party
            .webauthn
            .finish_discoverable_authentication(&signed, state, &keys)
            .unwrap();
    }

    /// A state without the slot is an error, never a state with the
    /// challenge webauthn-rs made.
    #[test]
    fn a_state_without_the_challenge_slot_is_an_error() {
        let party = RelyingParty::at("l59.spoolframe.app", "https://l59.spoolframe.app").unwrap();
        let (_, state) = party.webauthn.start_discoverable_authentication().unwrap();
        let err = with_challenge(state, "rs", &[0; 32])
            .map(|_| ())
            .unwrap_err();
        assert!(err.contains("rs.challenge"), "{err}");
    }
}
