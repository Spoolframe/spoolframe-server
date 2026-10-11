//! Invites (P6-01): codes that make an account, or grant a portable
//! identity access (P7-05), kept by their hash.
//!
//! Redeeming one uses it up and makes its user in one transaction, the
//! use taken first: of two redemptions of an invite's last use, one makes
//! its user and the other finds none left. A redemption that makes no
//! user (a taken username) gives its use back, as it never took it.

use std::fmt;
use std::str::FromStr;

use spoolframe_protocol::PortableUserId;
use spoolframe_types::{Platform, Role};
use sqlx::Row;
use uuid::Uuid;

use super::users::{
    LocalUserId, UserError, add_password, display_name, is_unique_violation, normalize_username,
    role, stored,
};
use super::{Store, StoreError};

/// What redeeming an invite does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteKind {
    /// Makes a user on this server.
    CreateUser,
    /// Grants a portable identity access here (P7-05): no account is
    /// made.
    PortableAccount,
    /// Pairs another server with this one (P8).
    Federation,
}

impl InviteKind {
    /// The name in the database and on the admin API.
    pub fn as_str(self) -> &'static str {
        match self {
            InviteKind::CreateUser => "create-user",
            InviteKind::PortableAccount => "portable-account",
            InviteKind::Federation => "federation",
        }
    }
}

impl fmt::Display for InviteKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for InviteKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "create-user" => Ok(InviteKind::CreateUser),
            "portable-account" => Ok(InviteKind::PortableAccount),
            "federation" => Ok(InviteKind::Federation),
            other => Err(format!(
                "no invite kind {other:?}: create-user, portable-account or federation"
            )),
        }
    }
}

/// Where an invite was made (PX-44).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InviteOrigin {
    /// The web UI's admin pages; every invite from before PX-44 but the
    /// CLI's.
    Web,
    /// `spoolframe-server invite create`.
    Cli,
    /// An app, on the device whose `Hello` gave its name and platform.
    Device {
        /// Hex `EndpointId`.
        endpoint_id: String,
        name: Option<String>,
        platform: Platform,
    },
}

impl InviteOrigin {
    /// Its name in the database.
    fn as_str(&self) -> &'static str {
        match self {
            InviteOrigin::Web => "web",
            InviteOrigin::Cli => "cli",
            InviteOrigin::Device { .. } => "device",
        }
    }
}

/// An invite to make: everything but its id and when it was made.
#[derive(Debug, Clone, Copy)]
pub struct NewInvite<'a> {
    pub kind: InviteKind,
    /// The BLAKE3 hash of its code, hex.
    pub capability_hash: &'a str,
    pub role: Role,
    pub note: Option<&'a str>,
    pub uses: u32,
    /// Unix seconds.
    pub expires_at: i64,
    pub created_by: &'a str,
    pub origin: &'a InviteOrigin,
}

/// An invite as the admin sees it: never its code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteRecord {
    pub id: String,
    pub kind: InviteKind,
    pub role: Role,
    pub note: Option<String>,
    pub uses: u32,
    pub uses_left: u32,
    pub expires_at: i64,
    pub created_by: String,
    /// `created_by`'s display name, if they are a user here who has one.
    pub created_by_display_name: Option<String>,
    pub created_at: i64,
    pub origin: InviteOrigin,
}

#[derive(Debug, thiserror::Error)]
pub enum InviteError {
    /// A kind this server does not make yet.
    #[error(
        "{0} invites arrive with P8; this server makes create-user and portable-account invites"
    )]
    NotYet(InviteKind),
    #[error("no live invite {0:?}")]
    NoSuchInvite(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<sqlx::Error> for InviteError {
    fn from(e: sqlx::Error) -> Self {
        InviteError::Store(e.into())
    }
}

/// Where an invite stands, by its code's hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteState {
    /// No invite has that code, or it was revoked: the same to a caller.
    Invalid,
    Expired,
    Exhausted,
    /// It can be redeemed now: it is of `kind`, and a `create-user` one
    /// makes a user with `role`.
    Usable {
        kind: InviteKind,
        role: Role,
    },
}

/// What a portable-account redemption did (P7-05).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortableRedeemed {
    /// The grant was made, and a use taken.
    Granted,
    /// The identity had a grant here already: nothing changed, and the
    /// invite keeps its use.
    AlreadyGranted,
    /// Nothing was made: the invite is not usable (or of another kind),
    /// as this says.
    Refused(InviteState),
}

/// What a redemption did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redeemed {
    /// The user was made, with the invite's role, and a use taken.
    User { id: LocalUserId, role: Role },
    /// Nothing was made: the invite is not usable, as this says.
    Refused(InviteState),
    /// Nothing was made, and the invite keeps its use.
    UsernameTaken,
}

fn origin(row: &sqlx::sqlite::SqliteRow) -> Result<InviteOrigin, sqlx::Error> {
    let origin: String = row.try_get("origin")?;
    Ok(match origin.as_str() {
        "cli" => InviteOrigin::Cli,
        "device" => InviteOrigin::Device {
            endpoint_id: row
                .try_get::<Option<String>, _>("origin_endpoint_id")?
                .unwrap_or_default(),
            name: row.try_get("origin_device_name")?,
            platform: row
                .try_get::<Option<String>, _>("origin_platform")?
                .map_or(Platform::Unknown, |p| Platform::from_name(&p)),
        },
        _ => InviteOrigin::Web,
    })
}

fn record(row: &sqlx::sqlite::SqliteRow) -> Result<InviteRecord, sqlx::Error> {
    let kind: String = row.try_get("kind")?;
    Ok(InviteRecord {
        id: row.try_get("id")?,
        kind: kind
            .parse()
            .map_err(|e: String| sqlx::Error::Decode(e.into()))?,
        role: role(row.try_get("role")?),
        note: row.try_get("note")?,
        uses: row.try_get("uses")?,
        uses_left: row.try_get("uses_left")?,
        expires_at: row.try_get("expires_at")?,
        created_by: row.try_get("created_by")?,
        created_by_display_name: row.try_get("created_by_display_name")?,
        created_at: row.try_get("created_at")?,
        origin: origin(row)?,
    })
}

impl Store {
    /// Keep a new invite. A `federation` one is refused before anything
    /// is written ([`InviteError::NotYet`]): P8 makes it.
    pub async fn create_invite(&self, invite: NewInvite<'_>) -> Result<InviteRecord, InviteError> {
        if invite.kind == InviteKind::Federation {
            return Err(InviteError::NotYet(invite.kind));
        }
        let now = self.now();
        let id = Uuid::now_v7().hyphenated().to_string();
        let (endpoint_id, device_name, platform) = match invite.origin {
            InviteOrigin::Device {
                endpoint_id,
                name,
                platform,
            } => (
                Some(endpoint_id.as_str()),
                name.as_deref(),
                Some(platform.as_str()),
            ),
            InviteOrigin::Web | InviteOrigin::Cli => (None, None, None),
        };
        sqlx::query(
            "INSERT INTO invites (id, kind, capability_hash, role, note, uses, uses_left, \
                                  expires_at, created_by, created_at, origin, \
                                  origin_endpoint_id, origin_device_name, origin_platform) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        )
        .bind(&id)
        .bind(invite.kind.as_str())
        .bind(invite.capability_hash)
        .bind(stored(invite.role))
        .bind(invite.note)
        .bind(invite.uses)
        .bind(invite.expires_at)
        .bind(invite.created_by)
        .bind(now)
        .bind(invite.origin.as_str())
        .bind(endpoint_id)
        .bind(device_name)
        .bind(platform)
        .execute(&self.pool)
        .await?;
        let created_by_display_name: Option<String> =
            sqlx::query_scalar("SELECT display_name FROM users WHERE username = ?1")
                .bind(invite.created_by)
                .fetch_optional(&self.pool)
                .await?
                .flatten();
        Ok(InviteRecord {
            id,
            kind: invite.kind,
            role: invite.role,
            note: invite.note.map(str::to_owned),
            uses: invite.uses,
            uses_left: invite.uses,
            expires_at: invite.expires_at,
            created_by: invite.created_by.to_owned(),
            created_by_display_name,
            created_at: now,
            origin: invite.origin.clone(),
        })
    }

    /// The invites that can still make an account, newest first: not
    /// revoked, not expired, not used up.
    pub async fn live_invites(&self) -> Result<Vec<InviteRecord>, StoreError> {
        // With the display name of the user who made each, if they are
        // one here (usernames are kept lowercased, as `created_by` is).
        let rows = sqlx::query(
            "SELECT invites.id, invites.kind, invites.role, invites.note, invites.uses, \
                    invites.uses_left, invites.expires_at, invites.created_by, \
                    u.display_name AS created_by_display_name, invites.created_at, \
                    invites.origin, invites.origin_endpoint_id, invites.origin_device_name, \
                    invites.origin_platform \
             FROM invites LEFT JOIN users u ON u.username = invites.created_by \
             WHERE invites.revoked_at IS NULL AND invites.expires_at > ?1 \
               AND invites.uses_left > 0 \
             ORDER BY invites.created_at DESC, invites.id DESC",
        )
        .bind(self.now())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(record).collect::<Result<_, _>>()?)
    }

    /// The invites that can still be redeemed, as their codes' hashes and
    /// their kinds: what an invite holder's token is checked against
    /// (P6-08). Every kind, as the relay carries the holder of each.
    pub async fn live_invite_hashes(&self) -> Result<Vec<(String, InviteKind)>, StoreError> {
        let rows = sqlx::query(
            "SELECT capability_hash, kind FROM invites \
             WHERE revoked_at IS NULL AND expires_at > ?1 AND uses_left > 0",
        )
        .bind(self.now())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|row| {
                let kind: String = row.try_get("kind")?;
                Ok((
                    row.try_get("capability_hash")?,
                    kind.parse()
                        .map_err(|e: String| sqlx::Error::Decode(e.into()))?,
                ))
            })
            .collect::<Result<_, sqlx::Error>>()?)
    }

    /// Revoke invite `id`: its unused code stops working. The accounts it
    /// made stay. [`InviteError::NoSuchInvite`] for one that is not live.
    pub async fn revoke_invite(&self, id: &str) -> Result<(), InviteError> {
        let revoked = sqlx::query(
            "UPDATE invites SET revoked_at = ?2 \
             WHERE id = ?1 AND revoked_at IS NULL AND expires_at > ?2 AND uses_left > 0",
        )
        .bind(id)
        .bind(self.now())
        .execute(&self.pool)
        .await?;
        if revoked.rows_affected() == 0 {
            return Err(InviteError::NoSuchInvite(id.to_owned()));
        }
        Ok(())
    }

    /// Where the invite whose code hashes to `capability_hash` stands now.
    pub async fn invite_state(&self, capability_hash: &str) -> Result<InviteState, StoreError> {
        Ok(state(
            &mut *self.pool.acquire().await?,
            capability_hash,
            self.now(),
        )
        .await?)
    }

    /// Make user `username` with the invite whose code hashes to
    /// `capability_hash`, and take one of its uses, in one transaction.
    /// `phc` is their password's hash.
    pub async fn redeem_invite(
        &self,
        capability_hash: &str,
        username: &str,
        display: Option<&str>,
        phc: &str,
    ) -> Result<Redeemed, UserError> {
        let name = normalize_username(username)?;
        let display = display_name(display)?;
        let now = self.now();
        let mut tx = self.begin_write().await?;
        // The use is taken first: a write, so a second redemption waits
        // for this one's transaction and then sees what it left.
        let taken = sqlx::query(
            "UPDATE invites SET uses_left = uses_left - 1 \
             WHERE capability_hash = ?1 AND kind = 'create-user' AND revoked_at IS NULL \
               AND expires_at > ?2 AND uses_left > 0 \
             RETURNING role",
        )
        .bind(capability_hash)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(taken) = taken else {
            let refused = state(&mut tx, capability_hash, now).await?;
            tx.rollback().await?;
            return Ok(Redeemed::Refused(refused));
        };
        let role = role(taken.try_get("role")?);
        let id = LocalUserId::generate();
        let inserted = sqlx::query(
            "INSERT INTO users (id, username, display_name, role, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(id)
        .bind(&name)
        .bind(display)
        .bind(stored(role))
        .bind(now)
        .execute(&mut *tx)
        .await;
        match inserted {
            Ok(_) => {}
            Err(e) if is_unique_violation(&e) => {
                tx.rollback().await?;
                return Ok(Redeemed::UsernameTaken);
            }
            Err(e) => return Err(e.into()),
        }
        add_password(&mut tx, id, phc, now).await?;
        tx.commit().await?;
        Ok(Redeemed::User { id, role })
    }
}

impl Store {
    /// Grant portable user `portable` access here, called `display`, with
    /// the `portable-account` invite whose code hashes to
    /// `capability_hash`, and take one of its uses, in one transaction.
    /// Someone granted here already keeps their grant, and the invite its
    /// use. Someone whose grant was revoked is let back only by an invite
    /// made after the revocation: an older one is
    /// [`InviteState::Invalid`] to them, and keeps its use.
    ///
    /// The use is taken first: a write, so the transaction holds the
    /// database's write lock from its first statement, and a redemption
    /// racing it waits, then sees what it left, rather than failing.
    pub async fn redeem_portable_invite(
        &self,
        capability_hash: &str,
        portable: PortableUserId,
        display: Option<&str>,
    ) -> Result<PortableRedeemed, StoreError> {
        let now = self.now();
        let id = portable.to_string();
        let mut tx = self.begin_write().await?;
        let taken: Option<(String, i64)> = sqlx::query_as(
            "UPDATE invites SET uses_left = uses_left - 1 \
             WHERE capability_hash = ?1 AND kind = 'portable-account' AND revoked_at IS NULL \
               AND expires_at > ?2 AND uses_left > 0 \
             RETURNING id, created_at",
        )
        .bind(capability_hash)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((invite_id, created_at)) = taken else {
            let refused = state(&mut tx, capability_hash, now).await?;
            tx.rollback().await?;
            return Ok(PortableRedeemed::Refused(refused));
        };
        let grant: Option<Option<i64>> = sqlx::query_scalar(
            "SELECT revoked_at FROM portable_grants WHERE portable_user_id = ?1",
        )
        .bind(&id)
        .fetch_optional(&mut *tx)
        .await?;
        match grant {
            Some(None) => {
                tx.rollback().await?;
                return Ok(PortableRedeemed::AlreadyGranted);
            }
            Some(Some(revoked_at)) if created_at <= revoked_at => {
                tx.rollback().await?;
                return Ok(PortableRedeemed::Refused(InviteState::Invalid));
            }
            _ => {}
        }
        let granted = sqlx::query(
            "INSERT INTO portable_grants (portable_user_id, display_name, granted_at, invite_id) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (portable_user_id) DO UPDATE SET \
               display_name = excluded.display_name, granted_at = excluded.granted_at, \
               invite_id = excluded.invite_id, revoked_at = NULL \
             WHERE portable_grants.revoked_at IS NOT NULL",
        )
        .bind(&id)
        .bind(display)
        .bind(now)
        .bind(&invite_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if granted == 0 {
            tx.rollback().await?;
            return Ok(PortableRedeemed::AlreadyGranted);
        }
        tx.commit().await?;
        Ok(PortableRedeemed::Granted)
    }
}

/// Where the invite whose code hashes to `capability_hash` stands at
/// `now`, read on `conn`. A revoked one is [`InviteState::Invalid`], as
/// no invite is.
async fn state(
    conn: &mut sqlx::SqliteConnection,
    capability_hash: &str,
    now: i64,
) -> Result<InviteState, sqlx::Error> {
    let row = sqlx::query(
        "SELECT kind, role, revoked_at IS NOT NULL AS revoked, expires_at, uses_left \
         FROM invites WHERE capability_hash = ?1",
    )
    .bind(capability_hash)
    .fetch_optional(conn)
    .await?;
    let Some(row) = row else {
        return Ok(InviteState::Invalid);
    };
    let kind: String = row.try_get("kind")?;
    let revoked: bool = row.try_get("revoked")?;
    let expires_at: i64 = row.try_get("expires_at")?;
    let uses_left: i64 = row.try_get("uses_left")?;
    let Ok(kind) = kind.parse::<InviteKind>() else {
        return Ok(InviteState::Invalid);
    };
    Ok(if revoked {
        InviteState::Invalid
    } else if expires_at <= now {
        InviteState::Expired
    } else if uses_left <= 0 {
        InviteState::Exhausted
    } else {
        InviteState::Usable {
            kind,
            role: role(row.try_get("role")?),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{TestStore, test_store};
    use super::*;

    const DAY: i64 = 24 * 3600;

    fn new_invite<'a>(store: &Store, hash: &'a str, uses: u32) -> NewInvite<'a> {
        NewInvite {
            kind: InviteKind::CreateUser,
            capability_hash: hash,
            role: Role::Consumer,
            note: Some("Mum's iPad"),
            uses,
            expires_at: store.now() + 7 * DAY,
            created_by: "root",
            origin: &InviteOrigin::Web,
        }
    }

    async fn store_with(test: &str, hash: &str, uses: u32) -> TestStore {
        let store = test_store(test).await;
        store
            .create_invite(new_invite(&store, hash, uses))
            .await
            .unwrap();
        store
    }

    async fn redeem(store: &Store, hash: &str, username: &str) -> Redeemed {
        store
            .redeem_invite(hash, username, Some("Someone"), "$argon2id$x")
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_new_invite_is_listed_with_its_uses_and_who_made_it() {
        let store = store_with("invite-listed", "h1", 4).await;
        let listed = store.live_invites().await.unwrap();
        assert_eq!(listed.len(), 1);
        let invite = &listed[0];
        assert_eq!(invite.kind, InviteKind::CreateUser);
        assert_eq!(invite.role, Role::Consumer);
        assert_eq!(invite.note.as_deref(), Some("Mum's iPad"));
        assert_eq!((invite.uses, invite.uses_left), (4, 4));
        assert_eq!(invite.created_by, "root");
        // Made a moment before: a second may have turned since.
        let expected = store.now() + 7 * DAY;
        assert!(
            (expected - 1..=expected).contains(&invite.expires_at),
            "expires {} for {expected}",
            invite.expires_at
        );
    }

    #[tokio::test]
    async fn redeeming_makes_the_user_with_the_invite_s_role_and_takes_a_use() {
        let store = test_store("invite-redeem").await;
        store
            .create_invite(NewInvite {
                role: Role::Admin,
                ..new_invite(&store, "h1", 2)
            })
            .await
            .unwrap();
        let Redeemed::User { id, role } = redeem(&store, "h1", "Mum").await else {
            panic!("no user made");
        };
        assert_eq!(role, Role::Admin);
        let user = store.user_by_id(id).await.unwrap().unwrap();
        assert_eq!(user.username, "mum");
        assert_eq!(user.display_name.as_deref(), Some("Someone"));
        assert_eq!(user.role, Role::Admin);
        assert!(user.has_password);
        assert_eq!(store.live_invites().await.unwrap()[0].uses_left, 1);
    }

    /// The ticket's race: two redemptions of the last use at once.
    #[tokio::test]
    async fn the_last_use_redeemed_twice_at_once_makes_exactly_one_user() {
        let store = store_with("invite-race", "h1", 1).await;
        let (a, b) = tokio::join!(redeem(&store, "h1", "anna"), redeem(&store, "h1", "bob"));
        let made = [a, b]
            .iter()
            .filter(|r| matches!(r, Redeemed::User { .. }))
            .count();
        assert_eq!(made, 1, "{a:?} {b:?}");
        assert!(
            [a, b].contains(&Redeemed::Refused(InviteState::Exhausted)),
            "{a:?} {b:?}"
        );
        assert_eq!(store.user_count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_used_up_invite_is_exhausted_and_leaves_the_list() {
        let store = store_with("invite-used-up", "h1", 1).await;
        assert!(matches!(
            redeem(&store, "h1", "anna").await,
            Redeemed::User { .. }
        ));
        assert_eq!(
            redeem(&store, "h1", "bob").await,
            Redeemed::Refused(InviteState::Exhausted)
        );
        assert_eq!(
            store.invite_state("h1").await.unwrap(),
            InviteState::Exhausted
        );
        assert!(store.live_invites().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_expired_invite_makes_no_one_and_leaves_the_list() {
        let store = test_store("invite-expired").await;
        store
            .create_invite(NewInvite {
                expires_at: store.now() - 1,
                ..new_invite(&store, "h1", 1)
            })
            .await
            .unwrap();
        assert_eq!(
            redeem(&store, "h1", "anna").await,
            Redeemed::Refused(InviteState::Expired)
        );
        assert_eq!(store.user_count().await.unwrap(), 0);
        assert!(store.live_invites().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_revoked_invite_is_as_invalid_as_no_invite() {
        let store = store_with("invite-revoked", "h1", 3).await;
        let id = store.live_invites().await.unwrap()[0].id.clone();
        store.revoke_invite(&id).await.unwrap();
        for hash in ["h1", "no-such-hash"] {
            assert_eq!(
                redeem(&store, hash, "anna").await,
                Redeemed::Refused(InviteState::Invalid),
                "{hash}"
            );
            assert_eq!(
                store.invite_state(hash).await.unwrap(),
                InviteState::Invalid
            );
        }
        assert_eq!(store.user_count().await.unwrap(), 0);
        assert!(store.live_invites().await.unwrap().is_empty());
        assert!(matches!(
            store.revoke_invite(&id).await,
            Err(InviteError::NoSuchInvite(_))
        ));
    }

    #[tokio::test]
    async fn a_taken_username_makes_no_one_and_keeps_the_use() {
        let store = store_with("invite-taken", "h1", 1).await;
        store
            .add_user("anna", None, "$argon2id$x", Role::Consumer)
            .await
            .unwrap();
        assert_eq!(redeem(&store, "h1", "Anna").await, Redeemed::UsernameTaken);
        assert_eq!(store.live_invites().await.unwrap()[0].uses_left, 1);
        assert_eq!(store.user_count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn an_invalid_username_keeps_the_use() {
        let store = store_with("invite-bad-name", "h1", 1).await;
        assert!(matches!(
            store
                .redeem_invite("h1", "not a name!", None, "$argon2id$x")
                .await,
            Err(UserError::InvalidUsername(_))
        ));
        assert_eq!(store.live_invites().await.unwrap()[0].uses_left, 1);
    }

    /// Federation is known and refused: nothing is kept, so nothing can
    /// be half-redeemed or quietly made another kind of invite.
    #[tokio::test]
    async fn federation_invites_are_refused_and_nothing_is_kept() {
        let store = test_store("invite-kinds").await;
        let refused = store
            .create_invite(NewInvite {
                kind: InviteKind::Federation,
                ..new_invite(&store, "h1", 1)
            })
            .await;
        assert!(
            matches!(refused, Err(InviteError::NotYet(InviteKind::Federation))),
            "{refused:?}"
        );
        assert!(store.live_invites().await.unwrap().is_empty());
        assert_eq!(
            store.invite_state("h1").await.unwrap(),
            InviteState::Invalid
        );
    }

    async fn portable_store(test: &str, uses: u32) -> TestStore {
        let store = test_store(test).await;
        store
            .create_invite(NewInvite {
                kind: InviteKind::PortableAccount,
                ..new_invite(&store, "p1", uses)
            })
            .await
            .unwrap();
        store
    }

    /// P7-05: a grant, a use taken, and no user made.
    #[tokio::test]
    async fn a_portable_invite_grants_and_makes_no_user() {
        use crate::store::portable::tests::user;
        let store = portable_store("invite-portable", 2).await;
        assert_eq!(
            store
                .redeem_portable_invite("p1", user(7), Some("Sam"))
                .await
                .unwrap(),
            PortableRedeemed::Granted
        );
        let grant = store.grant_of(user(7)).await.unwrap().unwrap();
        assert_eq!(grant.display_name.as_deref(), Some("Sam"));
        let invite = &store.live_invites().await.unwrap()[0];
        assert_eq!(grant.invite_id.as_deref(), Some(invite.id.as_str()));
        assert_eq!(invite.uses_left, 1);
        assert_eq!(store.user_count().await.unwrap(), 0);
        // Again: nothing changes, and the use is kept.
        assert_eq!(
            store
                .redeem_portable_invite("p1", user(7), Some("Other"))
                .await
                .unwrap(),
            PortableRedeemed::AlreadyGranted
        );
        assert_eq!(store.live_invites().await.unwrap()[0].uses_left, 1);
        // The last use, then none.
        store
            .redeem_portable_invite("p1", user(8), None)
            .await
            .unwrap();
        assert_eq!(
            store
                .redeem_portable_invite("p1", user(9), None)
                .await
                .unwrap(),
            PortableRedeemed::Refused(InviteState::Exhausted)
        );
    }

    /// P7-05 review: a revoked guest is let back only by an invite made
    /// after the revocation; an older one keeps its use.
    #[tokio::test]
    async fn a_revoked_guest_needs_an_invite_made_after_the_revocation() {
        use crate::store::portable::tests::user;
        let store = portable_store("invite-portable-tombstone", 3).await;
        store
            .redeem_portable_invite("p1", user(7), None)
            .await
            .unwrap();
        store.clock().advance(std::time::Duration::from_secs(5));
        assert!(store.revoke_grant(user(7)).await.unwrap());
        assert_eq!(
            store
                .redeem_portable_invite("p1", user(7), None)
                .await
                .unwrap(),
            PortableRedeemed::Refused(InviteState::Invalid)
        );
        assert_eq!(store.live_invites().await.unwrap()[0].uses_left, 2);
        // Another identity still redeems the old invite.
        assert_eq!(
            store
                .redeem_portable_invite("p1", user(8), None)
                .await
                .unwrap(),
            PortableRedeemed::Granted
        );
        store.clock().advance(std::time::Duration::from_secs(5));
        store
            .create_invite(NewInvite {
                kind: InviteKind::PortableAccount,
                ..new_invite(&store, "p2", 1)
            })
            .await
            .unwrap();
        assert_eq!(
            store
                .redeem_portable_invite("p2", user(7), None)
                .await
                .unwrap(),
            PortableRedeemed::Granted
        );
        assert!(store.grant_of(user(7)).await.unwrap().is_some());
    }

    /// Neither kind's code redeems the other way, and neither use goes.
    #[tokio::test]
    async fn a_code_redeems_only_as_its_kind() {
        use crate::store::portable::tests::user;
        let store = portable_store("invite-wrong-kind", 1).await;
        store
            .create_invite(new_invite(&store, "h1", 1))
            .await
            .unwrap();
        assert_eq!(
            redeem(&store, "p1", "anna").await,
            Redeemed::Refused(InviteState::Usable {
                kind: InviteKind::PortableAccount,
                role: Role::Consumer
            })
        );
        assert!(matches!(
            store
                .redeem_portable_invite("h1", user(7), None)
                .await
                .unwrap(),
            PortableRedeemed::Refused(InviteState::Usable {
                kind: InviteKind::CreateUser,
                ..
            })
        ));
        assert!(
            store
                .live_invites()
                .await
                .unwrap()
                .iter()
                .all(|i| i.uses_left == 1)
        );
        assert_eq!(store.user_count().await.unwrap(), 0);
        assert!(store.grants().await.unwrap().is_empty());
    }

    #[test]
    fn every_kind_reads_back_from_its_name() {
        for kind in [
            InviteKind::CreateUser,
            InviteKind::PortableAccount,
            InviteKind::Federation,
        ] {
            assert_eq!(kind.as_str().parse::<InviteKind>(), Ok(kind));
        }
        assert!("relay".parse::<InviteKind>().is_err());
    }
}
