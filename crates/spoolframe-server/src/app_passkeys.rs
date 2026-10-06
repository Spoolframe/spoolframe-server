//! Passkeys in the apps (P3-19): a second relying party, [`APP_RP_ID`],
//! that every server shares and that needs nothing configured.
//!
//! A platform's passkey API (Apple's first) works only for a domain the
//! app is associated with when it is built, and a self-hosted server's own
//! domain is not known then. So the apps make and use their passkeys for
//! `spoolframe.app`, at the origin [`APP_ORIGIN`] the platform writes into
//! `clientDataJSON`. The ceremonies run over `spoolframe/client/1`; the
//! server never contacts `spoolframe.app`. It does not depend on
//! `admin.public_url`, so a LAN-only server, whose web UI has no passkeys,
//! has them in the apps.
//!
//! App passkeys are kept with the web UI's ([`crate::passkeys`]), under
//! the relying-party ID `spoolframe.app`, which is what marks them: the
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
//! `clientDataJSON` names no server, as the origin is every server's; so
//! without that, a server that begins a ceremony here and hands the
//! request to an app as its own would have the app sign for here. With
//! it, the app signs for the server it talks to, and this one refuses
//! that. A passkey made at one server is unknown at another.
//!
//! Anyone can begin a sign-in, so it lists no passkeys, even for a
//! username: those would tell anyone a user's credential ids. Passkeys are
//! made discoverable, so the platform finds them.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use password_hash::rand_core::{OsRng, RngCore};
use serde::Serialize;
use serde::de::DeserializeOwned;
use spoolframe_protocol::{
    APP_ORIGIN, APP_RP_ID, FinishPasskeyRegistration, FinishPasskeySignIn, NONCE_LEN,
    PasskeyRegistrationBegun, PasskeySignInBegun, app_passkey_challenge,
};
use webauthn_rs::prelude::{
    DiscoverableAuthentication, PasskeyRegistration, PublicKeyCredential,
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

impl Default for AppPasskeys {
    fn default() -> Self {
        Self::with_lifetime(CEREMONY_LIFETIME)
    }
}

impl AppPasskeys {
    /// As the default, with ceremonies that last `lifetime` (tests shorten
    /// it).
    pub fn with_lifetime(lifetime: Duration) -> Self {
        Self {
            party: RelyingParty {
                user_handle_optional: true,
                ..RelyingParty::at(APP_RP_ID, APP_ORIGIN)
                    .expect("spoolframe.app at https://spoolframe.app is a relying party")
            },
            lifetime,
            registrations: Mutex::default(),
            sign_ins: Mutex::default(),
            confirms: Mutex::default(),
            clock: crate::clock::Clock::default(),
        }
    }

    /// Ceremonies expire by `clock` (tests move it instead of sleeping).
    pub fn with_clock(mut self, clock: crate::clock::Clock) -> Self {
        self.clock = clock;
        self
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
            .filter(|p| p.rp_id == APP_RP_ID)
            .map(|p| credential(p).map(|c| c.cred_id().clone()))
            .collect::<Result<Vec<_>, _>>()?;
        let exclude_credential_ids = existing.iter().map(|id| id.to_vec()).collect();
        // One keychain may hold app passkeys of several servers, all for
        // spoolframe.app: the server's name tells them apart.
        let display_name = user.display_name.as_deref().unwrap_or(&user.username);
        let user_name = format!("{} on {server_name}", user.username);
        let display_name = format!("{display_name} on {server_name}");
        let (options, state) = self
            .party
            .webauthn
            .start_passkey_registration(handle, &user_name, &display_name, Some(existing))
            .map_err(|e| CeremonyError::Internal(e.to_string()))?;
        let nonce = nonce();
        let state = with_challenge(state, "rs", &app_passkey_challenge(server, &nonce))
            .map_err(CeremonyError::Internal)?;
        let mut registrations = self
            .registrations
            .lock()
            .expect("never held across a panic");
        let now = self.clock.now();
        registrations.retain(|_, (_, expires_at)| *expires_at > now);
        let registration = Registration {
            endpoint: endpoint.to_owned(),
            state,
        };
        registrations.insert(session.to_owned(), (registration, now + self.lifetime));
        Ok(PasskeyRegistrationBegun {
            nonce,
            user_id: options.public_key.user.id.to_vec(),
            user_name,
            display_name,
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
            .expect("never held across a panic")
            .remove(session)
            .filter(|(r, expires_at)| *expires_at > self.clock.now() && r.endpoint == endpoint)
            .map(|(r, _)| r.state)
            .ok_or(CeremonyError::NoCeremony)?;
        let (id, json) = self.party.made(&registered(made), &state)?;
        let record = store
            .add_passkey(user.id, &id, name, &json, APP_RP_ID)
            .await?;
        tracing::info!(username = %user.username, passkey = %record.name, "app passkey added");
        Ok(record)
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
            Some(Ok(username)) => store.passkey_ids_of(&username, APP_RP_ID).await?,
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
        let mut sign_ins = self.sign_ins.lock().expect("never held across a panic");
        let now = self.clock.now();
        sign_ins.retain(|_, (_, expires_at)| *expires_at > now);
        // Anyone can make an endpoint: they are counted, as the web's are.
        if !sign_ins.contains_key(endpoint) && sign_ins.len() >= MAX_SIGN_INS {
            return Err(SignInError::RateLimited {
                retry_after: self.lifetime,
            });
        }
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
                    .expect("never held across a panic")
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
            .filter(|p| p.rp_id == APP_RP_ID)
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
        let mut confirms = self.confirms.lock().expect("never held across a panic");
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
                    .expect("never held across a panic")
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
    OsRng.fill_bytes(&mut nonce);
    nonce
}

/// A ceremony's `state` from webauthn-rs, with `challenge` in the place of
/// the random one it made: webauthn-rs takes no challenge of the caller's.
/// Its state is JSON with the challenge, base64url, at
/// `<inner>.challenge`; one that is not is an error, never a state with
/// the old challenge.
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
