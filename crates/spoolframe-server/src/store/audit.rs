//! The audit list (PX-42): what was done, by whom, through what, kept for
//! the admin pages.

use super::{Store, StoreError};

/// One thing done, to record.
#[derive(Debug, Clone, Copy)]
pub struct NewAuditEvent<'a> {
    /// A user's id.
    pub actor: &'a str,
    /// An MCP client's name, or `web`.
    pub client: &'a str,
    pub action: &'a str,
    pub target: Option<&'a str>,
    /// `ok`, or why not.
    pub outcome: &'a str,
}

/// One thing done, as recorded, with the actor's username when they are
/// still a user here.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct AuditRecord {
    pub id: i64,
    pub at: i64,
    pub actor: String,
    pub actor_name: Option<String>,
    pub client: String,
    pub action: String,
    pub target: Option<String>,
    pub outcome: String,
}

impl Store {
    pub async fn record_audit(&self, event: NewAuditEvent<'_>) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO audit_events (at, actor, client, action, target, outcome) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(self.now())
        .bind(event.actor)
        .bind(event.client)
        .bind(event.action)
        .bind(event.target)
        .bind(event.outcome)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// A page of the audit list, newest first: `limit` from `offset`, and
    /// how many there are.
    pub async fn audit_page(
        &self,
        offset: u32,
        limit: u32,
    ) -> Result<(Vec<AuditRecord>, u32), StoreError> {
        let rows = sqlx::query_as(
            "SELECT a.id, a.at, a.actor, u.username AS actor_name, a.client, a.action, \
                    a.target, a.outcome \
             FROM audit_events a LEFT JOIN users u ON u.id = a.actor \
             ORDER BY a.id DESC LIMIT ?1 OFFSET ?2",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        let (total,): (i64,) = sqlx::query_as("SELECT count(*) FROM audit_events")
            .fetch_one(&self.pool)
            .await?;
        Ok((rows, u32::try_from(total).unwrap_or(u32::MAX)))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::test_store;
    use super::*;

    #[tokio::test]
    async fn the_audit_list_is_newest_first_and_paged() {
        let store = test_store("audit").await;
        let id = store
            .add_user("root", None, "phc", spoolframe_types::Role::Admin)
            .await
            .unwrap()
            .to_string();
        for action in ["disable_user", "end_session", "stop_playback"] {
            store
                .record_audit(NewAuditEvent {
                    actor: &id,
                    client: "Claude",
                    action,
                    target: Some("bob"),
                    outcome: "ok",
                })
                .await
                .unwrap();
        }
        let (page, total) = store.audit_page(0, 2).await.unwrap();
        assert_eq!(total, 3);
        let actions: Vec<&str> = page.iter().map(|r| r.action.as_str()).collect();
        assert_eq!(actions, ["stop_playback", "end_session"]);
        assert_eq!(page[0].actor_name.as_deref(), Some("root"));
        let (rest, _) = store.audit_page(2, 2).await.unwrap();
        assert_eq!(rest[0].action, "disable_user");
    }
}
