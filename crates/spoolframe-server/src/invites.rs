//! Invites (P6-01): the codes an admin makes, and redeeming one.
//!
//! A code is of the setup code's class ([`spoolframe_protocol::invite`]):
//! 12 characters, about 59 bits. It is kept only as its BLAKE3 hash, as a
//! session token is, and shown once, when it is made.
//!
//! Redeeming is a sign-in a stranger makes, so it is guarded as one. The
//! caller draws on the strangers' budget first (PX-10); then the code is
//! counted as a wrong guess before it is checked (A-13), and only a code
//! of an invite that was not revoked takes the guess back: a redemption
//! says `invalid` for a revoked invite as for no invite, so it counts them
//! alike. Expired or used up, it was still the right code: telling so
//! helps no one guess.
//!
//! Looking a code up before joining ([`look_up`]) is guarded as a
//! redemption is, on the same count: it is as good a guess. It does tell
//! a revoked invite from none, so it takes the guess back for a revoked one.
//!
//! Before redeeming, the holder proves the code to the server's relay
//! with a token (P6-08, [`holder_tier`]), guarded as a redemption is: a
//! token that matches no live invite is a wrong guess at a code.

use spoolframe_protocol::invite::{
    CODE_ALPHABET, CODE_LEN, Invite, InviteLinkKind, MAX_HINTS, display_code, normalize_code,
};
use spoolframe_protocol::relay_access::Tier;
use spoolframe_protocol::{PortableUserId, RedeemInvite};
use spoolframe_types::Role;

use crate::announce::Told;
use crate::auth::{Auth, PasswordError, hash_password};
use crate::store::invites::{
    InviteError, InviteKind, InviteLookup, InviteOrigin, InviteRecord, InviteState, NewInvite,
    PortableRedeemed, Redeemed,
};
use crate::store::users::{LocalUserId, UserError, display_name, normalize_username};
use crate::store::{Store, StoreError};

/// An invite's code, normalized (`QW7K2MFPXR4T`).
#[derive(Clone, PartialEq, Eq)]
pub struct InviteCode(String);

impl std::fmt::Debug for InviteCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InviteCode(<redacted>)")
    }
}

impl InviteCode {
    /// A fresh random code.
    pub fn generate() -> Self {
        use password_hash::rand_core::{OsRng, RngCore};
        let code = (0..CODE_LEN)
            .map(|_| {
                // 2^32 is not a multiple of 30: drawing again past the
                // last whole multiple keeps every character as likely.
                let limit = u32::MAX - u32::MAX % CODE_ALPHABET.len() as u32;
                let n = loop {
                    let n = OsRng.next_u32();
                    if n < limit {
                        break n;
                    }
                };
                CODE_ALPHABET[n as usize % CODE_ALPHABET.len()] as char
            })
            .collect();
        Self(code)
    }

    /// A code as typed; `None` if it cannot be one.
    pub fn parse(typed: &str) -> Option<Self> {
        normalize_code(typed).map(Self)
    }

    /// What the store keys the invite by.
    pub fn hash(&self) -> String {
        blake3::hash(self.0.as_bytes()).to_hex().to_string()
    }

    /// As shown and carried in the link: `QW7K-2MFP-XR4T`.
    pub fn shown(&self) -> String {
        display_code(&self.0)
    }

    /// Normalized, as the link's `Invite::code` holds it.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An invite an admin asks for (P6-01), in the web UI (`inviteCreate`) or
/// an app (`CreateInvite`, PX-44): both make the same invites, with the
/// same limits. Each `None` is the default: one use, 7 days, the consumer
/// role.
#[derive(Debug, Clone, Copy)]
pub struct InviteRequest<'a> {
    pub kind: InviteKind,
    pub note: Option<&'a str>,
    pub uses: Option<u32>,
    /// How long it lasts from now, in seconds.
    pub expires_secs: Option<u64>,
    pub role: Option<Role>,
}

/// Why no invite was made.
#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    /// A field out of its limits; the message says which.
    #[error("{0}")]
    Invalid(String),
    /// A kind not made yet ([`InviteError::NotYet`]), or the store.
    #[error(transparent)]
    Invite(#[from] InviteError),
}

/// An invite just made, and its code: the code is shown this once.
#[derive(Debug)]
pub struct Created {
    pub record: InviteRecord,
    pub code: InviteCode,
}

impl Created {
    /// The invite its links are made of: `server`'s ([`server_link`])
    /// with the code, its expiry, and what redeeming it does.
    pub fn invite(&self, mut server: Invite) -> Invite {
        server.code = Some(self.code.as_str().to_owned());
        server.expires_at = u64::try_from(self.record.expires_at).ok();
        match self.record.kind {
            InviteKind::CreateUser => server.role = Some(self.record.role),
            InviteKind::PortableAccount => server.kind = InviteLinkKind::PortableAccount,
            InviteKind::Federation => server.kind = InviteLinkKind::Federation,
        }
        server
    }
}

/// Make the invite `request` asks for, as `created_by` from `origin`, once
/// every field is within its limits. A `federation` one only with a
/// `federation` (`experimental.federation`, P8-01).
pub async fn create(
    store: &Store,
    request: InviteRequest<'_>,
    created_by: &str,
    origin: &InviteOrigin,
    federation: Option<&crate::federation::Federation>,
) -> Result<Created, CreateError> {
    use spoolframe_admin_api::invite::{
        DEFAULT_EXPIRES_SECS, DEFAULT_USES, MAX_EXPIRES_SECS, MAX_NOTE_CHARS, MAX_USES,
        MIN_EXPIRES_SECS,
    };
    if request.kind == InviteKind::Federation && federation.is_none() {
        return Err(InviteError::NotYet(request.kind).into());
    }
    let uses = request.uses.unwrap_or(DEFAULT_USES);
    if !(1..=MAX_USES).contains(&uses) {
        return Err(CreateError::Invalid(format!(
            "an invite makes 1 to {MAX_USES} accounts"
        )));
    }
    let expires = request.expires_secs.unwrap_or(DEFAULT_EXPIRES_SECS);
    if !(MIN_EXPIRES_SECS..=MAX_EXPIRES_SECS).contains(&expires) {
        return Err(CreateError::Invalid(
            "an invite lasts an hour to 30 days".into(),
        ));
    }
    let role = request
        .kind
        .resolve_role(request.role)
        .map_err(|why| CreateError::Invalid(why.into()))?;
    let note = request.note.map(str::trim).filter(|n| !n.is_empty());
    if note.is_some_and(|n| n.chars().count() > MAX_NOTE_CHARS || n.chars().any(char::is_control)) {
        return Err(CreateError::Invalid(format!(
            "a note is at most {MAX_NOTE_CHARS} characters, on one line"
        )));
    }
    let code = InviteCode::generate();
    let hash = code.hash();
    let expires_at = store.clock().unix_secs() + i64::try_from(expires).unwrap_or(i64::MAX);
    let record = store
        .create_invite(NewInvite {
            kind: request.kind,
            capability_hash: &hash,
            role,
            note,
            uses,
            expires_at,
            created_by,
            origin,
        })
        .await?;
    tracing::info!(
        id = %record.id,
        kind = %record.kind,
        %role,
        uses,
        by = %created_by,
        from = origin_name(origin),
        "invite made"
    );
    Ok(Created { record, code })
}

fn origin_name(origin: &InviteOrigin) -> &'static str {
    match origin {
        InviteOrigin::Web => "web",
        InviteOrigin::Cli => "cli",
        InviteOrigin::Device { .. } => "device",
    }
}

/// What an invite link says of this server (P6-01), before a code is put
/// in it: its key `endpoint_id`, its `name`, and the addresses and relays
/// `told` tells a signed-in device. It reads the interfaces, off the
/// async threads.
pub async fn server_link(endpoint_id: String, name: String, told: Option<Told>) -> Invite {
    let (addrs, relays, lookup) = match told {
        Some(told) => {
            tokio::task::spawn_blocking(move || (told.addrs(), told.relays(), told.lookup()))
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(
                        error = %e,
                        "cannot read the addresses for an invite link; it names none"
                    );
                    Default::default()
                })
        }
        None => Default::default(),
    };
    Invite {
        server: endpoint_id,
        name: Some(name),
        addrs: addrs.into_iter().take(MAX_HINTS).collect(),
        relays,
        lookup,
        ..Default::default()
    }
}

/// Why a redemption made no account.
#[derive(Debug, thiserror::Error)]
pub enum RedeemError {
    /// No invite has the code, or it was revoked; counted as a wrong guess.
    #[error("that invite code is not valid")]
    Invalid,
    #[error("the invite has expired")]
    Expired,
    #[error("the invite has been used up")]
    Exhausted,
    #[error("that username is taken")]
    UsernameTaken,
    /// A username, display name or password that is not acceptable.
    #[error("{0}")]
    InvalidInput(String),
    /// The code is a live invite's, of another kind than redeemed (P7-05).
    #[error("that invite is of another kind")]
    WrongKind,
    /// Too many wrong codes from this device.
    #[error("too many wrong invite codes")]
    RateLimited { retry_after: std::time::Duration },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("cannot hash the password: {0}")]
    Hash(String),
}

impl From<UserError> for RedeemError {
    fn from(e: UserError) -> Self {
        match e {
            UserError::Exists(_) => RedeemError::UsernameTaken,
            UserError::Store(e) => RedeemError::Store(e),
            other => RedeemError::InvalidInput(other.to_string()),
        }
    }
}

/// Make the account `request` asks for with its invite, for a device whose
/// guesses are counted as `endpoint`'s: its id and role. Nothing is made,
/// and the invite keeps its uses, unless this answers `Ok`.
pub async fn redeem(
    auth: &Auth,
    endpoint: &str,
    request: &RedeemInvite,
) -> Result<(LocalUserId, Role), RedeemError> {
    let hash = usable_code(auth, endpoint, &request.code, InviteKind::CreateUser).await?;
    // The cheap checks before the password's hash.
    normalize_username(&request.username)?;
    display_name(request.display_name.as_deref())?;
    let phc = hash_password(&request.password)
        .await
        .map_err(|e| match e {
            PasswordError::TooShort => RedeemError::InvalidInput(e.to_string()),
            PasswordError::Hash(e) => RedeemError::Hash(e),
        })?;
    let redeemed = auth
        .store()
        .redeem_invite(
            &hash,
            &request.username,
            request.display_name.as_deref(),
            &phc,
        )
        .await?;
    match redeemed {
        Redeemed::User { id, role } => Ok((id, role)),
        Redeemed::UsernameTaken => Err(RedeemError::UsernameTaken),
        // Revoked, expired or used up since it was looked at.
        Redeemed::Refused(InviteState::Expired) => Err(RedeemError::Expired),
        Redeemed::Refused(InviteState::Exhausted) => Err(RedeemError::Exhausted),
        Redeemed::Refused(_) => Err(RedeemError::Invalid),
    }
}

/// The hash of `typed`, a code of a live invite of `kind`, from a device
/// whose guesses are counted as `endpoint`'s.
///
/// Per device, as device sign-in codes are: endpoints are free, so what
/// holds guessing from many to a crawl is the strangers' budget, which
/// the caller drew on before this. The guess is counted before the code
/// is looked at, and taken back for any code a live-or-spent invite has:
/// expired, used up or of another kind, it was still the right code. A
/// revoked invite is `Invalid` here, as no invite is, and stays counted.
pub(crate) async fn usable_code(
    auth: &Auth,
    endpoint: &str,
    typed: &str,
    kind: InviteKind,
) -> Result<String, RedeemError> {
    let guess = code_guess(auth, endpoint)?;
    // A code that cannot be one is a wrong one, and stays counted.
    let Some(code) = InviteCode::parse(typed) else {
        return Err(RedeemError::Invalid);
    };
    let hash = code.hash();
    let state = auth.store().invite_state(&hash).await?;
    if state != InviteState::Invalid {
        auth.guessed_right(guess);
    }
    match state {
        InviteState::Invalid => Err(RedeemError::Invalid),
        InviteState::Expired => Err(RedeemError::Expired),
        InviteState::Exhausted => Err(RedeemError::Exhausted),
        InviteState::Usable { kind: found, .. } if found != kind => Err(RedeemError::WrongKind),
        InviteState::Usable { .. } => Ok(hash),
    }
}

/// A guess at an invite's code from `endpoint`, counted as wrong until
/// taken back: redemptions and lookups share the one count.
fn code_guess(auth: &Auth, endpoint: &str) -> Result<crate::auth::Guess, RedeemError> {
    auth.guess(format!("invite@{endpoint}"), Some(endpoint.to_owned()))
        .map_err(|retry_after| RedeemError::RateLimited { retry_after })
}

/// What the invite whose code is `typed` is (`LookupInvite`), for a
/// device whose guesses are counted as `endpoint`'s. Nothing is used up.
///
/// Counted as [`redeem`] counts a code, on the same count: a code no
/// invite has, or that cannot be one, stays a wrong guess. Unlike a
/// redemption, which says `invalid` for a revoked invite too and counts it,
/// a lookup tells a revoked invite apart, so it takes the guess back for it
/// as for an expired or used up one: all were the right code.
pub async fn look_up(
    auth: &Auth,
    endpoint: &str,
    typed: &str,
) -> Result<InviteLookup, RedeemError> {
    let guess = code_guess(auth, endpoint)?;
    let Some(code) = InviteCode::parse(typed) else {
        return Ok(InviteLookup::NoMatch);
    };
    let found = auth.store().look_up_invite(&code.hash()).await?;
    if found != InviteLookup::NoMatch {
        auth.guessed_right(guess);
    }
    Ok(found)
}

/// Grant `portable` access here with the `portable-account` invite whose
/// code is `typed` (P7-05), from a device whose guesses are counted as
/// `endpoint`'s, calling them `display`. Who `portable` is was proved
/// before this ([`crate::portable::check_presentation`]). Nothing is
/// granted, and the invite keeps its uses, unless this answers `Ok`; one
/// granted here already keeps their grant, and the invite its use.
pub async fn redeem_portable(
    auth: &Auth,
    endpoint: &str,
    typed: &str,
    portable: PortableUserId,
    display: Option<&str>,
) -> Result<PortableRedeemed, RedeemError> {
    let hash = usable_code(auth, endpoint, typed, InviteKind::PortableAccount).await?;
    let display = display_name(display)?;
    match auth
        .store()
        .redeem_portable_invite(&hash, portable, display)
        .await?
    {
        // Revoked, expired or used up since it was looked at.
        PortableRedeemed::Refused(InviteState::Expired) => Err(RedeemError::Expired),
        PortableRedeemed::Refused(InviteState::Exhausted) => Err(RedeemError::Exhausted),
        PortableRedeemed::Refused(_) => Err(RedeemError::Invalid),
        done => Ok(done),
    }
}

/// The tier the holder of an invite of `kind` relays as on the server's
/// own relay (P6-08), and the user's decision: the kind alone says. A
/// `create-user` invite is for an account here, whatever its role: an
/// owner's. A `portable-account` (P7) or `federation` (P8) invite is for
/// a user or a server from elsewhere: a friend's.
pub fn holder_tier_of(kind: InviteKind) -> Tier {
    if kind.makes_account() {
        Tier::Owner
    } else {
        Tier::Friend
    }
}

/// The tier the holder of a live invite relays as, if `token` is theirs
/// as `endpoint` (P6-08): [`holder_tier_of`] the invite's kind, else
/// `None`. Guarded as `mac_matches` says, as [`redeem`] is (A-13).
pub async fn holder_tier(auth: &Auth, endpoint: &iroh::EndpointId, token: &str) -> Option<Tier> {
    let mac = spoolframe_protocol::relay_access::parse_invite_token(token)?;
    mac_matches(
        auth,
        endpoint,
        "invite",
        mac,
        auth.store().live_invite_hashes(),
    )
    .await
    .map(holder_tier_of)
}

/// What the secret whose hash among `hashes` makes `mac` for `endpoint`
/// says, if one does: a relay token's check (P6-08, P8-04), for a device
/// that is no one's yet. It draws on the strangers' budget first (PX-10),
/// and counts as a guess at `label` from that endpoint, taken back only
/// when it matches: a token that matches nothing (revoked, expired, used
/// up, or never one) stays counted. Every hash is checked, in constant
/// time each, whether one matched or not; `hashes` is read only once the
/// budget allows.
pub(crate) async fn mac_matches<T: Copy>(
    auth: &Auth,
    endpoint: &iroh::EndpointId,
    label: &str,
    mac: [u8; 32],
    hashes: impl std::future::Future<Output = Result<Vec<(String, T)>, StoreError>>,
) -> Option<T> {
    use spoolframe_protocol::relay_access::invite_mac;
    let source = endpoint.to_string();
    if let Err(wait) = auth.stranger_attempt() {
        tracing::debug!(endpoint = %source, ?wait, label, "a relay token not checked: strangers wait");
        return None;
    }
    let Ok(guess) = auth.guess(format!("{label}@{source}"), Some(source.clone())) else {
        tracing::debug!(endpoint = %source, label, "a relay token not checked: too many wrong guesses");
        return None;
    };
    let hashes = match hashes.await {
        Ok(hashes) => hashes,
        Err(e) => {
            tracing::warn!(error = %e, label, "cannot check a relay token");
            return None;
        }
    };
    let mac = blake3::Hash::from_bytes(mac);
    let mut matched = None;
    for (hash, said) in hashes {
        let Ok(hash) = blake3::Hash::from_hex(&hash) else {
            continue;
        };
        // blake3::Hash compares in constant time.
        let expected = blake3::Hash::from_bytes(invite_mac(hash.as_bytes(), endpoint.as_bytes()));
        if expected == mac {
            matched = Some(said);
        }
    }
    if matched.is_some() {
        auth.guessed_right(guess);
    }
    matched
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_code_is_a_code_and_reads_back_as_shown() {
        let code = InviteCode::generate();
        assert_eq!(InviteCode::parse(&code.shown()), Some(code.clone()));
        assert_ne!(InviteCode::generate(), code);
    }

    #[test]
    fn a_code_hashes_the_same_however_it_is_typed() {
        let typed = InviteCode::parse("qw7k 2mfp-xr4t").unwrap();
        assert_eq!(
            typed.hash(),
            InviteCode::parse("QW7K2MFPXR4T").unwrap().hash()
        );
        assert_ne!(typed.hash(), InviteCode::generate().hash());
    }

    /// P6-08: the kind alone says the holder's tier.
    #[test]
    fn an_account_here_is_an_owner_s_and_one_from_elsewhere_a_friend_s() {
        assert_eq!(holder_tier_of(InviteKind::CreateUser), Tier::Owner);
        assert_eq!(holder_tier_of(InviteKind::PortableAccount), Tier::Friend);
        assert_eq!(holder_tier_of(InviteKind::Federation), Tier::Friend);
    }

    #[test]
    fn the_code_stays_out_of_debug_output() {
        let code = InviteCode::parse("QW7K2MFPXR4T").unwrap();
        assert!(!format!("{code:?}").contains("QW7K"));
    }
}
