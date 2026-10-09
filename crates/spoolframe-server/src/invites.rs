//! Invites (P6-01): the codes an admin makes, and redeeming one.
//!
//! A code is of the setup code's class ([`spoolframe_protocol::invite`]):
//! 12 characters, about 59 bits. It is kept only as its BLAKE3 hash, as a
//! session token is, and shown once, when it is made.
//!
//! Redeeming is a sign-in a stranger makes, so it is guarded as one. The
//! caller draws on the strangers' budget first (PX-10); then the code is
//! counted as a wrong guess before it is checked (A-13), and only a code
//! some invite has, revoked or not, takes the guess back. Expired or used
//! up, it was still the right code: telling so helps no one guess.
//!
//! Before redeeming, the holder proves the code to the server's relay
//! with a token (P6-08, [`holder_tier`]), guarded as a redemption is: a
//! token that matches no live invite is a wrong guess at a code.

use spoolframe_protocol::RedeemInvite;
use spoolframe_protocol::invite::{CODE_ALPHABET, CODE_LEN, display_code, normalize_code};
use spoolframe_protocol::relay_access::Tier;
use spoolframe_types::Role;

use crate::auth::{Auth, PasswordError, hash_password};
use crate::store::StoreError;
use crate::store::invites::{InviteKind, InviteState, Redeemed};
use crate::store::users::{LocalUserId, UserError, display_name, normalize_username};

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
    // Per device, as device sign-in codes are: endpoints are free, so
    // what holds guessing from many to a crawl is the strangers' budget,
    // which the caller drew on before this.
    let guess = auth
        .guess(format!("invite@{endpoint}"), Some(endpoint.to_owned()))
        .map_err(|retry_after| RedeemError::RateLimited { retry_after })?;
    // A code that cannot be one is a wrong one, and stays counted.
    let Some(code) = InviteCode::parse(&request.code) else {
        return Err(RedeemError::Invalid);
    };
    let hash = code.hash();
    let state = auth.store().invite_state(&hash).await?;
    match state {
        InviteState::Invalid => return Err(RedeemError::Invalid),
        InviteState::Expired => {
            auth.guessed_right(guess);
            return Err(RedeemError::Expired);
        }
        InviteState::Exhausted => {
            auth.guessed_right(guess);
            return Err(RedeemError::Exhausted);
        }
        InviteState::Usable { .. } => auth.guessed_right(guess),
    }
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

/// The tier the holder of an invite of `kind` relays as on the server's
/// own relay (P6-08), and the user's decision: the kind alone says. A
/// `create-user` invite is for an account here, whatever its role: an
/// owner's. A `portable-account` (P7) or `federation` (P8) invite is for
/// a user or a server from elsewhere: a friend's.
pub fn holder_tier_of(kind: InviteKind) -> Tier {
    match kind {
        InviteKind::CreateUser => Tier::Owner,
        InviteKind::PortableAccount | InviteKind::Federation => Tier::Friend,
    }
}

/// The tier the holder of a live invite relays as, if `token` is theirs
/// as `endpoint` (P6-08): [`holder_tier_of`] the invite's kind, else
/// `None`. For a device that is no one's yet, so it draws on the
/// strangers' budget first (PX-10), and counts as a guess at a code as
/// [`redeem`] does (A-13): a token that matches no live invite (revoked,
/// expired, used up, or never one) stays counted. Every live invite is
/// checked, in constant time each, whether one matched or not.
pub async fn holder_tier(auth: &Auth, endpoint: &iroh::EndpointId, token: &str) -> Option<Tier> {
    use spoolframe_protocol::relay_access::{invite_mac, parse_invite_token};
    let source = endpoint.to_string();
    if let Err(wait) = auth.stranger_attempt() {
        tracing::debug!(endpoint = %source, ?wait, "an invite token not checked: strangers wait");
        return None;
    }
    let Ok(guess) = auth.guess(format!("invite@{source}"), Some(source.clone())) else {
        tracing::debug!(endpoint = %source, "an invite token not checked: too many wrong codes");
        return None;
    };
    let mac = blake3::Hash::from_bytes(parse_invite_token(token)?);
    let invites = match auth.store().live_invite_hashes().await {
        Ok(invites) => invites,
        Err(e) => {
            tracing::warn!(error = %e, "cannot check an invite token");
            return None;
        }
    };
    let mut matched = None;
    for (hash, kind) in invites {
        let Ok(hash) = blake3::Hash::from_hex(&hash) else {
            continue;
        };
        // blake3::Hash compares in constant time.
        let expected = blake3::Hash::from_bytes(invite_mac(hash.as_bytes(), endpoint.as_bytes()));
        if expected == mac {
            matched = Some(holder_tier_of(kind));
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
