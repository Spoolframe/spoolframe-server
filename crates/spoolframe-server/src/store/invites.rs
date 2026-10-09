//! Invites (P6-01): codes that make an account, kept by their hash.
//!
//! Redeeming one uses it up and makes its user in one transaction, the
//! use taken first: of two redemptions of an invite's last use, one makes
//! its user and the other finds none left. A redemption that makes no
//! user (a taken username) gives its use back, as it never took it.

use std::fmt;
use std::str::FromStr;

use spoolframe_types::Role;
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
    /// Makes a user on this server: the only kind made so far.
    CreateUser,
    /// Moves an account from another server here (P7-05).
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
    pub created_at: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum InviteError {
    /// A kind this server does not make yet.
    #[error("{0} invites arrive with P7 and P8; this server makes create-user invites only")]
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
    /// It would make a user with `role` now.
    Usable {
        role: Role,
    },
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
        created_at: row.try_get("created_at")?,
    })
}

impl Store {
    /// Keep a new invite. A kind other than `create-user` is refused
    /// before anything is written ([`InviteError::NotYet`]).
    pub async fn create_invite(&self, invite: NewInvite<'_>) -> Result<InviteRecord, InviteError> {
        if invite.kind != InviteKind::CreateUser {
            return Err(InviteError::NotYet(invite.kind));
        }
        let now = self.now();
        let id = Uuid::now_v7().hyphenated().to_string();
        sqlx::query(
            "INSERT INTO invites (id, kind, capability_hash, role, note, uses, uses_left, \
                                  expires_at, created_by, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8, ?9)",
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
        .execute(&self.pool)
        .await?;
        Ok(InviteRecord {
            id,
            kind: invite.kind,
            role: invite.role,
            note: invite.note.map(str::to_owned),
            uses: invite.uses,
            uses_left: invite.uses,
            expires_at: invite.expires_at,
            created_by: invite.created_by.to_owned(),
            created_at: now,
        })
    }

    /// The invites that can still make an account, newest first: not
    /// revoked, not expired, not used up.
    pub async fn live_invites(&self) -> Result<Vec<InviteRecord>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, kind, role, note, uses, uses_left, expires_at, created_by, created_at \
             FROM invites \
             WHERE revoked_at IS NULL AND expires_at > ?1 AND uses_left > 0 \
             ORDER BY created_at DESC, id DESC",
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
        let mut tx = self.pool.begin().await?;
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
    Ok(if revoked || kind != InviteKind::CreateUser.as_str() {
        InviteState::Invalid
    } else if expires_at <= now {
        InviteState::Expired
    } else if uses_left <= 0 {
        InviteState::Exhausted
    } else {
        InviteState::Usable {
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

    /// The other kinds are known and refused: nothing is kept, so nothing
    /// can be half-redeemed or quietly made a `create-user` invite.
    #[tokio::test]
    async fn the_kinds_of_later_phases_are_refused_and_nothing_is_kept() {
        let store = test_store("invite-kinds").await;
        for kind in [InviteKind::PortableAccount, InviteKind::Federation] {
            let refused = store
                .create_invite(NewInvite {
                    kind,
                    ..new_invite(&store, "h1", 1)
                })
                .await;
            assert!(
                matches!(refused, Err(InviteError::NotYet(k)) if k == kind),
                "{refused:?}"
            );
        }
        assert!(store.live_invites().await.unwrap().is_empty());
        assert_eq!(
            store.invite_state("h1").await.unwrap(),
            InviteState::Invalid
        );
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
