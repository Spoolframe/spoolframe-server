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

use spoolframe_protocol::RedeemInvite;
use spoolframe_protocol::invite::{CODE_ALPHABET, CODE_LEN, display_code, normalize_code};
use spoolframe_types::Role;

use crate::auth::{Auth, PasswordError, hash_password};
use crate::store::StoreError;
use crate::store::invites::{InviteState, Redeemed};
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

    #[test]
    fn the_code_stays_out_of_debug_output() {
        let code = InviteCode::parse("QW7K2MFPXR4T").unwrap();
        assert!(!format!("{code:?}").contains("QW7K"));
    }
}
