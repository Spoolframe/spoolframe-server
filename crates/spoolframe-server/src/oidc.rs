//! Single sign-on with OpenID Connect (P3-04, ARCHITECTURE §24A).
//!
//! The server is the provider's confidential client: the authorization
//! code flow with PKCE, `state` and `nonce`, the browser sent back to
//! `admin.public_url` + [`CALLBACK`]. The ID token is checked here with
//! `openidconnect`: its signature against the keys in the provider's
//! discovery document (asymmetric algorithms only, never the client
//! secret), its issuer, its audience, its expiry and its nonce.
//!
//! A provider never makes or finds accounts. An identity (issuer and
//! subject) signs in only the local user who linked it from My profile
//! while signed in; email is shown, never matched.
//!
//! Each flow begun is held here, in memory, for [`FLOW_LIFETIME`], under
//! its `state`, and is good once. It is bound to the browser that began
//! it by a random value in a cookie of its own, so a callback carried to
//! another browser (a link sent to someone) does nothing there.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use openidconnect::core::{
    CoreAuthDisplay, CoreAuthenticationFlow, CoreClaimName, CoreClaimType, CoreClient,
    CoreClientAuthMethod, CoreGrantType, CoreJsonWebKey, CoreJsonWebKeyType,
    CoreJweContentEncryptionAlgorithm, CoreJweKeyManagementAlgorithm, CoreJwsSigningAlgorithm,
    CoreResponseMode, CoreResponseType, CoreSubjectIdentifierType,
};
use openidconnect::{
    AdditionalProviderMetadata, AuthType, AuthorizationCode, ClientId, ClientSecret, CsrfToken,
    DiscoveryError, IssuerUrl, JwsSigningAlgorithm, Nonce, PkceCodeChallenge, PkceCodeVerifier,
    ProviderMetadata, RedirectUrl, Scope, TokenResponse,
};
use password_hash::rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};

use crate::config::OidcConfig;
use crate::config::secrets::Secrets;
use crate::store::users::LocalUserId;

/// Where a provider sends the browser back to, under `admin.public_url`.
pub const CALLBACK: &str = "/auth/oidc/callback";

/// How long a flow waits for its callback: time to sign in at the
/// provider, typing a password and a second factor.
pub const FLOW_LIFETIME: Duration = Duration::from_secs(10 * 60);

/// The most flows held at once. Beginning one needs no session (signing
/// in), so this bounds what strangers can make the server keep.
pub const MAX_FLOWS: usize = 10_000;

/// How long a provider's discovery document and keys are used before they
/// are fetched again: sign-ins begun at once (or a stranger's flood of
/// them) ask the provider once.
const DISCOVERY_FRESH: Duration = Duration::from_secs(60);

/// How long the provider is waited for, each request.
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(10);

/// What a flow is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Purpose {
    /// Signing in: the identity's linked user gets a session, and the
    /// browser goes back to `return_to` (the page the sign-in began on:
    /// the device page a TV's QR code opens, P3-07), else to the start
    /// page; or, failed, to it, else to the login page. Kept with the
    /// flow, never read from the callback.
    SignIn { return_to: Option<ReturnTo> },
    /// Linking the identity to `user`, signed in in the session whose
    /// token hash is `session` (P3-03's My profile).
    Link { user: LocalUserId, session: String },
    /// Proving again, in session `session`, that its user is who they are
    /// (P3-03's recent sign-in), with an identity linked to them.
    Confirm { user: LocalUserId, session: String },
}

/// A page of the web UI to go back to after a sign-in: a path on this
/// server, with its query, and never anywhere else. A browser takes `//x`
/// (and `/\x`, and `/<tab>/x`, which it reads as `//x`) as another host,
/// so only a path that starts with one `/`, of visible ASCII without a
/// backslash, is one; and it must still be a path here once resolved as a
/// browser resolves it (`/../..//x` is `//x`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReturnTo(String);

impl ReturnTo {
    /// The longest taken.
    const MAX: usize = 2048;

    pub fn parse(text: &str) -> Option<Self> {
        let plain = text.len() <= Self::MAX
            && text.starts_with('/')
            && !text.starts_with("//")
            && text.bytes().all(|b| b.is_ascii_graphic() && b != b'\\');
        if !plain {
            return None;
        }
        let here = url::Url::parse("http://here.invalid/").expect("a fixed URL");
        let resolved = here.join(text).ok()?;
        let stays = resolved.origin() == here.origin() && !resolved.path().starts_with("//");
        stays.then(|| Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Who the provider says signed in, its ID token checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The provider's name, as configured.
    pub provider: String,
    pub issuer: String,
    pub subject: String,
    /// Its username at the provider, or else its email, to recognise it
    /// by.
    pub display: Option<String>,
}

/// A flow begun: where to send the browser, and the value its cookie
/// carries.
#[derive(Clone)]
pub struct Begun {
    pub url: String,
    pub browser: String,
}

impl std::fmt::Debug for Begun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Begun")
            .field("url", &self.url)
            .field("browser", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OidcError {
    /// No provider by that name.
    #[error("there is no sign-in provider called {0:?}")]
    NoSuchProvider(String),
    /// The server cannot be a client now: why, for an admin to fix.
    #[error("{0}")]
    NotConfigured(String),
    /// The provider did not answer, or not as a provider should: SSO
    /// fails, nothing else does (§24F).
    #[error("{provider} can't be reached right now")]
    Unavailable { provider: String, why: String },
    /// No flow under that `state` for this browser: never begun, over,
    /// used, or begun in another browser.
    #[error("that sign-in is over or was begun elsewhere; start again")]
    NoFlow,
    /// The ID token did not check out: a wrong signature, audience,
    /// issuer or nonce, or it expired. Logged with why; one answer.
    #[error("{provider}'s answer could not be trusted")]
    Refused { provider: String, why: String },
    /// Too many flows begun and not finished.
    #[error("too many sign-ins in progress; try again in a few minutes")]
    Busy,
}

/// One configured provider.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Provider {
    name: String,
    issuer: String,
    client_id: String,
    /// Asked for besides `openid`, which is always.
    scopes: Vec<String>,
    /// The file the secret is in: the config's `client_secret_file`, or
    /// the secret kept in the state directory.
    secret: PathBuf,
}

/// What the discovery document says beyond OpenID Connect Discovery:
/// the PKCE methods (RFC 8414), which openidconnect does not read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Pkce {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    code_challenge_methods_supported: Option<Vec<String>>,
}

impl AdditionalProviderMetadata for Pkce {}

/// A provider's discovery document: where everything about it comes
/// from, nothing of it configured.
type Metadata = ProviderMetadata<
    Pkce,
    CoreAuthDisplay,
    CoreClientAuthMethod,
    CoreClaimName,
    CoreClaimType,
    CoreGrantType,
    CoreJweContentEncryptionAlgorithm,
    CoreJweKeyManagementAlgorithm,
    CoreJsonWebKey,
    CoreResponseMode,
    CoreResponseType,
    CoreSubjectIdentifierType,
>;

/// The scopes asked for when the config names none.
pub const DEFAULT_SCOPES: [&str; 3] = ["openid", "profile", "email"];

/// How the server proves to `metadata`'s token endpoint that it is the
/// client: `client_secret_basic` where the provider offers it (or says
/// nothing, which means it, by OpenID Connect Discovery), else
/// `client_secret_post`; `None` when it offers neither.
fn token_auth(metadata: &Metadata) -> Option<AuthType> {
    let Some(offered) = metadata.token_endpoint_auth_methods_supported() else {
        return Some(AuthType::BasicAuth);
    };
    if offered.contains(&CoreClientAuthMethod::ClientSecretBasic) {
        Some(AuthType::BasicAuth)
    } else if offered.contains(&CoreClientAuthMethod::ClientSecretPost) {
        Some(AuthType::RequestBody)
    } else {
        None
    }
}

/// Whether the provider takes S256 PKCE: it lists it, or lists no
/// methods at all. One that lists only `plain` is refused: its challenge
/// would be the verifier itself.
fn takes_s256(metadata: &Metadata) -> bool {
    metadata
        .additional_metadata()
        .code_challenge_methods_supported
        .as_ref()
        .is_none_or(|methods| methods.iter().any(|m| m == "S256"))
}

/// A client of a provider, as its discovery document makes it.
type Client = CoreClient<
    openidconnect::EndpointSet,
    openidconnect::EndpointNotSet,
    openidconnect::EndpointNotSet,
    openidconnect::EndpointNotSet,
    openidconnect::EndpointMaybeSet,
    openidconnect::EndpointMaybeSet,
>;

/// A flow waiting for its callback.
struct Flow {
    provider: String,
    purpose: Purpose,
    /// The hash of the value in the browser's cookie.
    browser: String,
    pkce: PkceCodeVerifier,
    nonce: Nonce,
    /// The provider's discovery document as it was at the start: its
    /// endpoints and keys.
    metadata: Metadata,
    began: Instant,
}

/// The server's SSO: its providers, and the flows in progress.
pub struct Oidc {
    providers: RwLock<Vec<Provider>>,
    /// `admin.public_url` + [`CALLBACK`]; `None` without a public URL,
    /// and then there is no SSO.
    redirect: Option<RedirectUrl>,
    /// Where secrets set in the web UI are; `None` in tests without.
    secrets: Option<Secrets>,
    http: reqwest::Client,
    flows: Mutex<HashMap<String, Flow>>,
    lifetime: Duration,
    /// Each issuer's discovery document, and when it was fetched.
    discovered: Mutex<HashMap<String, (Instant, Metadata)>>,
}

impl std::fmt::Debug for Oidc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Oidc")
            .field("providers", &self.names())
            .field("redirect", &self.redirect)
            .finish_non_exhaustive()
    }
}

impl Default for Oidc {
    /// No providers: no SSO.
    fn default() -> Self {
        Self::new(None, None)
    }
}

/// A hash of a browser's cookie value, as a flow keeps it.
fn browser_hash(value: &str) -> String {
    blake3::hash(format!("spoolframe oidc browser {value}").as_bytes())
        .to_hex()
        .to_string()
}

/// 32 random bytes, hex.
fn random() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether an ID token signed with `alg` may be trusted: with a key from
/// the provider's key set. Not with the client secret (HMAC), which the
/// server holds itself, and never unsigned.
fn trusted(alg: &CoreJwsSigningAlgorithm) -> bool {
    matches!(alg.key_type(), Some(t) if t != CoreJsonWebKeyType::Symmetric)
}

impl Oidc {
    /// SSO for a web UI at `public_url` (`admin.public_url`), with the
    /// secrets set in the web UI in `secrets`. No providers until
    /// [`Oidc::set_providers`].
    pub fn new(public_url: Option<&str>, secrets: Option<Secrets>) -> Self {
        let redirect = public_url.and_then(|base| {
            RedirectUrl::new(format!("{}{CALLBACK}", base.trim_end_matches('/'))).ok()
        });
        let http = reqwest::Client::builder()
            // A provider's redirect is not followed: the URLs it gives are
            // where the requests go (openidconnect's advice, against SSRF).
            .redirect(reqwest::redirect::Policy::none())
            .timeout(PROVIDER_TIMEOUT)
            .build()
            .expect("a client with no TLS roots of its own builds");
        Self {
            providers: RwLock::default(),
            redirect,
            secrets,
            http,
            flows: Mutex::default(),
            lifetime: FLOW_LIFETIME,
            discovered: Mutex::default(),
        }
    }

    /// Flows live `lifetime` (tests shorten it).
    pub fn with_flow_lifetime(mut self, lifetime: Duration) -> Self {
        self.lifetime = lifetime;
        self
    }

    /// The providers from now on, as the config has them (checked
    /// already): a live setting.
    pub fn set_providers(&self, config: &[OidcConfig]) {
        let providers = config
            .iter()
            .map(|p| Provider {
                name: p.name.trim().to_owned(),
                issuer: p.issuer.clone(),
                client_id: p.client_id.clone(),
                scopes: p.scopes().into_iter().filter(|s| s != "openid").collect(),
                secret: match (&p.client_secret_file, &self.secrets) {
                    (Some(file), _) => file.clone(),
                    (None, Some(secrets)) => secrets.path(&p.secret_name()),
                    // Nowhere to keep one: a test without a state dir.
                    (None, None) => PathBuf::from("/nonexistent"),
                },
            })
            .collect();
        *self.providers.write().expect("never poisoned") = providers;
    }

    /// The providers' names, in the config's order: none without a
    /// public URL, since no provider could send the browser back.
    pub fn names(&self) -> Vec<String> {
        if self.redirect.is_none() {
            return Vec::new();
        }
        self.providers
            .read()
            .expect("never poisoned")
            .iter()
            .map(|p| p.name.clone())
            .collect()
    }

    /// The name of the provider whose issuer is `issuer`, if one is
    /// configured.
    pub fn provider_of(&self, issuer: &str) -> Option<String> {
        self.providers
            .read()
            .expect("never poisoned")
            .iter()
            .find(|p| p.issuer == issuer)
            .map(|p| p.name.clone())
    }

    fn provider(&self, name: &str) -> Result<Provider, OidcError> {
        if self.redirect.is_none() {
            return Err(OidcError::NotConfigured(
                "single sign-on needs admin.public_url, where the provider sends the browser back"
                    .into(),
            ));
        }
        self.providers
            .read()
            .expect("never poisoned")
            .iter()
            .find(|p| p.name == name)
            .cloned()
            .ok_or_else(|| OidcError::NoSuchProvider(name.to_owned()))
    }

    /// The client of `provider` as `metadata` describes it, with its
    /// secret read now (it may have been changed or renewed).
    fn client(
        &self,
        provider: &Provider,
        metadata: Metadata,
    ) -> Result<(Client, Vec<CoreJwsSigningAlgorithm>), OidcError> {
        let secret = std::fs::read_to_string(&provider.secret)
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                // The path says where to put it; the secret is never shown.
                tracing::warn!(
                    provider = provider.name,
                    path = %provider.secret.display(),
                    "no client secret for this sign-in provider"
                );
                OidcError::NotConfigured(format!(
                    "{} has no client secret on this server",
                    provider.name
                ))
            })?;
        let algs: Vec<CoreJwsSigningAlgorithm> = metadata
            .id_token_signing_alg_values_supported()
            .iter()
            .filter(|alg| trusted(alg))
            .cloned()
            .collect();
        let redirect = self.redirect.clone().expect("checked by provider()");
        let not_configured = |why: &str| {
            tracing::warn!(
                provider = provider.name,
                why,
                "sign-in provider cannot be used"
            );
            OidcError::NotConfigured(format!("{}: {why}", provider.name))
        };
        if !takes_s256(&metadata) {
            return Err(not_configured(
                "its discovery document offers no S256 PKCE, only plain",
            ));
        }
        let auth = token_auth(&metadata).ok_or_else(|| {
            not_configured(
                "its token endpoint takes neither client_secret_basic nor client_secret_post",
            )
        })?;
        let client = CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(provider.client_id.clone()),
            Some(ClientSecret::new(secret)),
        )
        .set_auth_type(auth)
        .set_redirect_uri(redirect);
        if algs.is_empty() {
            return Err(OidcError::Unavailable {
                provider: provider.name.clone(),
                why: "it signs ID tokens with no algorithm the server trusts".into(),
            });
        }
        Ok((client, algs))
    }

    /// The provider's discovery document and keys, fetched within the
    /// last [`DISCOVERY_FRESH`].
    async fn discover(&self, provider: &Provider) -> Result<Metadata, OidcError> {
        if let Some((at, metadata)) = self
            .discovered
            .lock()
            .expect("never poisoned")
            .get(&provider.issuer)
            && at.elapsed() < DISCOVERY_FRESH
        {
            return Ok(metadata.clone());
        }
        let metadata = self.fetch_discovery(provider).await?;
        self.discovered
            .lock()
            .expect("never poisoned")
            .insert(provider.issuer.clone(), (Instant::now(), metadata.clone()));
        Ok(metadata)
    }

    async fn fetch_discovery(&self, provider: &Provider) -> Result<Metadata, OidcError> {
        let unavailable = |why: String| {
            tracing::warn!(
                provider = provider.name,
                why,
                "sign-in provider unavailable"
            );
            OidcError::Unavailable {
                provider: provider.name.clone(),
                why,
            }
        };
        let issuer =
            IssuerUrl::new(provider.issuer.clone()).map_err(|e| unavailable(e.to_string()))?;
        Metadata::discover_async(issuer, &self.http)
            .await
            .map_err(|e| match e {
                // The document is someone else's: the issuer configured is
                // not the one it names (OpenID Connect Discovery §4.3).
                DiscoveryError::Validation(why) => {
                    tracing::warn!(
                        provider = provider.name,
                        why,
                        "sign-in provider's discovery document refused"
                    );
                    OidcError::NotConfigured(format!(
                        "{}'s discovery document does not match its configured issuer: {why}",
                        provider.name
                    ))
                }
                other => unavailable(error_chain(&other)),
            })
    }

    /// Begin a flow with `provider` for `purpose`: where to send the
    /// browser, and what its cookie must carry back.
    pub async fn begin(&self, provider: &str, purpose: Purpose) -> Result<Begun, OidcError> {
        let provider = self.provider(provider)?;
        let metadata = self.discover(&provider).await?;
        let (client, _) = self.client(&provider, metadata.clone())?;
        let (challenge, pkce) = PkceCodeChallenge::new_random_sha256();
        let (url, state, nonce) = client
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .add_scopes(provider.scopes.iter().cloned().map(Scope::new))
            .set_pkce_challenge(challenge)
            .url();
        let browser = random();
        let mut flows = self.flows.lock().expect("never poisoned");
        let now = Instant::now();
        flows.retain(|_, f| now.duration_since(f.began) < self.lifetime);
        if flows.len() >= MAX_FLOWS {
            return Err(OidcError::Busy);
        }
        flows.insert(
            state.secret().clone(),
            Flow {
                provider: provider.name.clone(),
                purpose,
                browser: browser_hash(&browser),
                pkce,
                nonce,
                metadata,
                began: now,
            },
        );
        Ok(Begun {
            url: url.to_string(),
            browser,
        })
    }

    /// The flow under `state`, taken (a state is good once), if `browser`
    /// (its cookie's value) is the one that began it and it is not over.
    /// A callback from another browser leaves the flow for its own.
    fn take(&self, state: &str, browser: Option<&str>) -> Result<Flow, OidcError> {
        let mut flows = self.flows.lock().expect("never poisoned");
        let ours = flows
            .get(state)
            .is_some_and(|f| browser.is_some_and(|b| browser_hash(b) == f.browser));
        if !ours {
            return Err(OidcError::NoFlow);
        }
        let flow = flows.remove(state).expect("just looked at");
        if flow.began.elapsed() >= self.lifetime {
            return Err(OidcError::NoFlow);
        }
        Ok(flow)
    }

    /// The provider said no (`error` in the callback): the flow is over.
    /// What it was for.
    pub fn decline(&self, state: &str, browser: Option<&str>) -> Result<Purpose, OidcError> {
        let flow = self.take(state, browser)?;
        tracing::info!(
            provider = flow.provider,
            "the sign-in provider did not sign in"
        );
        Ok(flow.purpose)
    }

    /// Finish the flow under `state` with the provider's `code`: the code
    /// exchanged (with the PKCE verifier and the client secret), and the
    /// ID token checked. What it was for, and who signed in.
    pub async fn finish(
        &self,
        state: &str,
        code: &str,
        browser: Option<&str>,
    ) -> Result<(Purpose, Identity), (Option<Purpose>, OidcError)> {
        let flow = self.take(state, browser).map_err(|e| (None, e))?;
        let purpose = flow.purpose.clone();
        self.exchange(flow, code)
            .await
            .map(|identity| (purpose.clone(), identity))
            .map_err(|e| (Some(purpose), e))
    }

    async fn exchange(&self, flow: Flow, code: &str) -> Result<Identity, OidcError> {
        let provider = self.provider(&flow.provider)?;
        let name = provider.name.clone();
        let (client, algs) = self.client(&provider, flow.metadata)?;
        let unavailable = |why: String| {
            tracing::warn!(provider = name, why, "sign-in provider unavailable");
            OidcError::Unavailable {
                provider: name.clone(),
                why,
            }
        };
        let refused = |why: String| {
            tracing::warn!(
                provider = name,
                why,
                "a sign-in provider's ID token was refused"
            );
            OidcError::Refused {
                provider: name.clone(),
                why,
            }
        };
        let request = client
            .exchange_code(AuthorizationCode::new(code.to_owned()))
            .map_err(|e| unavailable(error_chain(&e)))?
            .set_pkce_verifier(flow.pkce);
        let tokens = request
            .request_async(&self.http)
            .await
            .map_err(|e| unavailable(error_chain(&e)))?;
        let id_token = tokens
            .id_token()
            .ok_or_else(|| refused("the token response has no ID token".into()))?;
        let verifier = client.id_token_verifier().set_allowed_algs(algs);
        let claims = id_token
            .claims(&verifier, &flow.nonce)
            .map_err(|e| refused(e.to_string()))?;
        // Shown, never matched: both can change at the provider.
        let display = claims
            .preferred_username()
            .map(|u| u.as_str().to_owned())
            .or_else(|| claims.email().map(|e| e.as_str().to_owned()));
        Ok(Identity {
            provider: provider.name,
            issuer: claims.issuer().as_str().to_owned(),
            subject: claims.subject().as_str().to_owned(),
            display,
        })
    }
}

/// An error with its sources, for the log: "Request failed: …: connection
/// refused".
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_path_on_this_server_is_a_page_to_return_to() {
        for page in [
            "/auth/device?code=BCDF-GHJK",
            "/",
            "/profile",
            // Encoded, a slash is only a character of the path.
            "/auth/device?next=%2F%2Fevil.example",
        ] {
            assert_eq!(
                ReturnTo::parse(page).as_ref().map(ReturnTo::as_str),
                Some(page)
            );
        }
        for hostile in [
            "//evil.example",
            "///evil.example",
            "/\\evil.example",
            "\\/evil.example",
            "/\t/evil.example",
            "/\n/evil.example",
            "https://evil.example/",
            "javascript:alert(1)",
            "evil.example",
            "",
            "/a b",
            "/caf\u{e9}",
            "/../..//evil.example",
            "/./\\/evil.example",
        ] {
            assert_eq!(ReturnTo::parse(hostile), None, "{hostile:?}");
        }
        assert_eq!(ReturnTo::parse(&format!("/{}", "a".repeat(4096))), None);
    }

    #[test]
    fn only_a_provider_s_own_keys_are_trusted() {
        assert!(trusted(&CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256));
        assert!(trusted(&CoreJwsSigningAlgorithm::EcdsaP256Sha256));
        assert!(trusted(&CoreJwsSigningAlgorithm::EdDsa));
        assert!(!trusted(&CoreJwsSigningAlgorithm::HmacSha256));
        assert!(!trusted(&CoreJwsSigningAlgorithm::None));
    }

    /// A discovery document with these token auth and PKCE lists.
    fn document(auth: Option<&[&str]>, pkce: Option<&[&str]>) -> Metadata {
        let mut doc = serde_json::json!({
            "issuer": "https://id.example.org",
            "authorization_endpoint": "https://id.example.org/authorize",
            "token_endpoint": "https://id.example.org/token",
            "jwks_uri": "https://id.example.org/jwks",
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["RS256"],
        });
        if let Some(auth) = auth {
            doc["token_endpoint_auth_methods_supported"] = serde_json::json!(auth);
        }
        if let Some(pkce) = pkce {
            doc["code_challenge_methods_supported"] = serde_json::json!(pkce);
        }
        serde_json::from_value(doc).unwrap()
    }

    #[test]
    fn the_token_endpoint_is_called_as_the_provider_says_basic_first() {
        let auth = |offered| token_auth(&document(offered, None));
        assert!(matches!(auth(None), Some(AuthType::BasicAuth)));
        assert!(matches!(
            auth(Some(&["client_secret_post", "client_secret_basic"])),
            Some(AuthType::BasicAuth)
        ));
        assert!(matches!(
            auth(Some(&["client_secret_post"])),
            Some(AuthType::RequestBody)
        ));
        assert!(auth(Some(&["private_key_jwt"])).is_none());
    }

    #[test]
    fn pkce_is_s256_and_a_provider_with_only_plain_is_refused() {
        let s256 = |offered| takes_s256(&document(None, offered));
        assert!(s256(Some(&["S256", "plain"])));
        assert!(s256(None));
        assert!(!s256(Some(&["plain"])));
    }

    #[test]
    fn without_a_public_url_there_are_no_providers() {
        let config = OidcConfig {
            name: "Pocket ID".into(),
            issuer: "https://id.example.org".into(),
            client_id: "x".into(),
            client_secret_file: None,
            scopes: None,
        };
        let without = Oidc::new(None, None);
        without.set_providers(std::slice::from_ref(&config));
        assert!(without.names().is_empty());
        let with = Oidc::new(Some("https://media.example.org/"), None);
        with.set_providers(&[config]);
        assert_eq!(with.names(), ["Pocket ID"]);
        assert_eq!(
            with.redirect.as_ref().unwrap().as_str(),
            "https://media.example.org/auth/oidc/callback"
        );
    }
}
