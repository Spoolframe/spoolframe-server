//! What an identity's settings page shows, read in one call: its roots,
//! the devices it gave credentials to, where its root is, and what can be
//! done with it here; and the guests signed in on this device with an
//! identity of their own. The Linux and Apple apps each read the same
//! five things and decided the same abilities.

use spoolframe_protocol::portable::{PortableUserId, RootKey};

use super::{GuestPairing, IssuedCredential, Portable, PortableIdentity, RootLocation};
use crate::SdkError;

/// What can be done with an identity on this device
/// ([`IdentityOverview::abilities`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityAbilities {
    /// Its root actions (pairing a device, revoking one, adding or
    /// removing a root, tying an account) can be signed here: its phrase
    /// root is here, or a security key that is one of its roots can be
    /// plugged in.
    pub sign: bool,
    /// Its phrase root can be synced to the user's other devices: the
    /// phrase root is here, and a security key is a root too, to shut out
    /// a lost device that synced it.
    pub sync: bool,
    /// It can join a guest invite, or be named in a pairing: its root, or
    /// its keys, are this device's own. A guest's cannot.
    pub join: bool,
}

/// A device an identity gave a credential to, as its settings list it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityDevice {
    pub credential: IssuedCredential,
    /// This device's own credential.
    pub this_device: bool,
}

/// An identity's settings page ([`Portable::overview`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityOverview {
    pub identity: PortableIdentity,
    /// Its id's fingerprint, to compare across devices.
    pub fingerprint: String,
    /// Its roots now: the phrase's and each security key's.
    pub roots: Vec<RootKey>,
    /// The devices with a credential from it not revoked, oldest first.
    pub devices: Vec<IdentityDevice>,
    pub location: RootLocation,
    /// When this device's credential runs out, in its last 14 days, Unix
    /// seconds: renew it.
    pub renewal_due: Option<i64>,
    pub abilities: IdentityAbilities,
}

/// Someone signed in on this device with an identity of their own
/// ([`Portable::guest_list`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guest {
    pub id: PortableUserId,
    /// What this device calls it, when it was told.
    pub name: Option<String>,
    pub pairing: GuestPairing,
}

/// [`IdentityAbilities`] of `identity` with `roots`.
pub(super) fn abilities(identity: &PortableIdentity, roots: &[RootKey]) -> IdentityAbilities {
    let key = roots
        .iter()
        .any(|r| matches!(r, RootKey::SecurityKey { .. }));
    let phrase = roots.iter().any(|r| matches!(r, RootKey::Phrase(_)));
    IdentityAbilities {
        sign: identity.has_root || key,
        sync: identity.has_root && phrase && key,
        join: identity.has_root || identity.key_rooted,
    }
}

impl Portable {
    /// Identity `id`'s settings page, from this device alone.
    /// [`SdkError::UnknownIdentity`] for one not here.
    pub async fn overview(&self, id: PortableUserId) -> Result<IdentityOverview, SdkError> {
        let identity = self
            .list()
            .await?
            .into_iter()
            .find(|i| i.id == id)
            .ok_or_else(|| SdkError::UnknownIdentity(id.to_string()))?;
        let roots = self.roots(id).await?.members;
        let own = self.device_key(id).await.ok();
        let mut devices: Vec<IdentityDevice> = self
            .issued(id)
            .await?
            .into_iter()
            .filter(|c| c.revoked_at.is_none())
            .map(|credential| IdentityDevice {
                this_device: Some(credential.device_key) == own,
                credential,
            })
            .collect();
        devices.sort_by_key(|d| d.credential.issued_at);
        Ok(IdentityOverview {
            fingerprint: id.fingerprint(),
            location: self.root_location(id).await?,
            renewal_due: self.renewal_due(id).await?,
            abilities: abilities(&identity, &roots),
            identity,
            roots,
            devices,
        })
    }

    /// Who is signed in on this device with an identity of their own: the
    /// guests paired here ([`Portable::guest_pairings`]), named as this
    /// device knows them. A security key identity's root is never here,
    /// so it counts among the guests; this device's own key identities are
    /// left out.
    pub async fn guest_list(&self) -> Result<Vec<Guest>, SdkError> {
        let known = self.list().await?;
        Ok(self
            .guest_pairings()
            .await?
            .into_iter()
            .filter(|p| !known.iter().any(|i| i.id == p.id && i.key_rooted))
            .map(|pairing| Guest {
                id: pairing.id,
                name: known
                    .iter()
                    .find(|i| i.id == pairing.id)
                    .map(|i| i.name.clone()),
                pairing,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(has_root: bool, key_rooted: bool) -> PortableIdentity {
        PortableIdentity {
            id: "ab".repeat(32).parse().unwrap(),
            name: "Sam".into(),
            name_set_at: spoolframe_types::serde_time::unix_seconds::from_wire(0),
            created_at: spoolframe_types::serde_time::unix_seconds::from_wire(0),
            has_root,
            key_rooted,
        }
    }

    fn key() -> RootKey {
        RootKey::SecurityKey {
            public_key: [4; 65],
            credential_id: vec![1],
        }
    }

    #[test]
    fn a_phrase_identity_with_its_root_here_does_all_but_sync() {
        let a = abilities(&identity(true, false), &[RootKey::Phrase([1; 32])]);
        assert_eq!(
            a,
            IdentityAbilities {
                sign: true,
                sync: false,
                join: true
            }
        );
    }

    #[test]
    fn a_phrase_root_with_a_key_too_may_sync() {
        let a = abilities(&identity(true, false), &[RootKey::Phrase([1; 32]), key()]);
        assert!(a.sync);
    }

    #[test]
    fn a_key_identity_signs_with_its_key_and_never_syncs() {
        let a = abilities(&identity(false, true), &[key()]);
        assert_eq!(
            a,
            IdentityAbilities {
                sign: true,
                sync: false,
                join: true
            }
        );
    }

    /// A guest's identity, with only a credential here.
    #[test]
    fn a_guest_does_nothing_but_sign_in() {
        let a = abilities(&identity(false, false), &[RootKey::Phrase([1; 32])]);
        assert_eq!(
            a,
            IdentityAbilities {
                sign: false,
                sync: false,
                join: false
            }
        );
    }

    #[tokio::test]
    async fn a_new_identity_is_its_phrase_root_here() {
        let portable = Portable::new(std::sync::Arc::new(crate::MemoryStorage::default()));
        let (id, _phrase) = portable.create("Sam").await.unwrap();
        let overview = portable.overview(id).await.unwrap();
        assert_eq!(
            (
                overview.identity.name.as_str(),
                overview.roots.len(),
                overview.location,
                overview.abilities.sign,
            ),
            ("Sam", 1, RootLocation::ThisDevice, true)
        );
    }

    #[tokio::test]
    async fn an_identity_not_here_is_unknown() {
        let portable = Portable::new(std::sync::Arc::new(crate::MemoryStorage::default()));
        assert!(matches!(
            portable.overview("ab".repeat(32).parse().unwrap()).await,
            Err(SdkError::UnknownIdentity(_))
        ));
    }
}
