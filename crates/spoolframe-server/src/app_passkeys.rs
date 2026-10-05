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
//! and once: a registration's is kept under the session that began it, a
//! sign-in's under the device's endpoint, so another device can finish
//! neither. Each server issues its own, so an assertion made for one is
//! of no use at another, and a passkey made at one is unknown elsewhere.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use spoolframe_protocol::{
    APP_ORIGIN, APP_RP_ID, FinishPasskeyRegistration, FinishPasskeySignIn, PasskeyCreation,
    PasskeyRequest,
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
use crate::store::users::{UserRecord, normalize_username};

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
            party: RelyingParty::at(APP_RP_ID, APP_ORIGIN)
                .expect("spoolframe.app at https://spoolframe.app is a relying party"),
            lifetime,
            registrations: Mutex::default(),
            sign_ins: Mutex::default(),
        }
    }

    /// Begin making an app passkey for `user` on `endpoint`, in the session
    /// whose token hashes to `session`, on the server called
    /// `server_name`. A registration the session began before is dropped.
    /// Whether the session signed in recently enough is the caller's to
    /// check.
    pub async fn begin_registration(
        &self,
        store: &Store,
        session: &str,
        endpoint: &str,
        user: &UserRecord,
        server_name: &str,
    ) -> Result<PasskeyCreation, CeremonyError> {
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
        let mut registrations = self
            .registrations
            .lock()
            .expect("never held across a panic");
        let now = Instant::now();
        registrations.retain(|_, (_, expires_at)| *expires_at > now);
        let registration = Registration {
            endpoint: endpoint.to_owned(),
            state,
        };
        registrations.insert(session.to_owned(), (registration, now + self.lifetime));
        Ok(PasskeyCreation {
            challenge: options.public_key.challenge.to_vec(),
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
            .filter(|(r, expires_at)| *expires_at > Instant::now() && r.endpoint == endpoint)
            .map(|(r, _)| r.state)
            .ok_or(CeremonyError::NoCeremony)?;
        let (id, json) = self.party.made(&registered(made), &state)?;
        let record = store
            .add_passkey(user.id, &id, name, &json, APP_RP_ID)
            .await?;
        tracing::info!(username = %user.username, passkey = %record.name, "app passkey added");
        Ok(record)
    }

    /// Begin a sign-in on `endpoint`: for `username`'s app passkeys, or,
    /// without one, for any the platform finds. A sign-in the endpoint
    /// began before is dropped. A username that is nobody's, or whose
    /// user has no app passkey, lists none.
    pub async fn begin_sign_in(
        &self,
        store: &Store,
        endpoint: &str,
        username: Option<&str>,
    ) -> Result<PasskeyRequest, SignInError> {
        let allowed = match username.map(normalize_username) {
            Some(Ok(username)) => store.passkey_ids_of(&username, APP_RP_ID).await?,
            Some(Err(_)) | None => Vec::new(),
        };
        let allow_credential_ids = allowed
            .iter()
            .map(|id| URL_SAFE_NO_PAD.decode(id))
            .collect::<Result<_, _>>()
            .map_err(|e| SignInError::Internal(format!("a stored credential id: {e}")))?;
        let (options, state) = self
            .party
            .webauthn
            .start_discoverable_authentication()
            .map_err(|e| SignInError::Internal(e.to_string()))?;
        let mut sign_ins = self.sign_ins.lock().expect("never held across a panic");
        let now = Instant::now();
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
        Ok(PasskeyRequest {
            challenge: options.public_key.challenge.to_vec(),
            allow_credential_ids,
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
                    .filter(|(_, expires_at)| *expires_at > Instant::now())
                    .ok_or("the sign-in was not begun on this device, or is over")?;
                if !sign_in.allowed.is_empty() && !sign_in.allowed.iter().any(|a| a == id) {
                    return Err("the passkey is not one the sign-in asked for");
                }
                Ok(sign_in.state)
            })
            .await
    }
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
            user_handle: Some(signed.user_handle.clone().into()),
        },
        extensions: Default::default(),
        type_: "public-key".to_owned(),
    }
}
