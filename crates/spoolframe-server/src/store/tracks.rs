//! Track preferences and overrides (P1-20). The choosing is in
//! `crate::tracks`; this is storage.

use spoolframe_types::TrackPreferences;

use super::{Store, StoreError, now};
use crate::principal::Principal;
use crate::tracks::TrackOverride;

/// A stored document that does not parse. Only this build writes them, so
/// it is a bug or a hand edit; reading falls back to the defaults.
fn parse<T: serde::de::DeserializeOwned>(what: &str, json: &str) -> Option<T> {
    serde_json::from_str(json)
        .inspect_err(|e| tracing::warn!(what, error = %e, "ignoring an unreadable stored document"))
        .ok()
}

impl Store {
    /// The principal's preferences; the defaults if they set none.
    pub async fn track_preferences(
        &self,
        principal: Principal,
    ) -> Result<TrackPreferences, StoreError> {
        let json: Option<String> =
            sqlx::query_scalar("SELECT preferences FROM track_preferences WHERE principal = ?1")
                .bind(principal.to_string())
                .fetch_optional(&self.pool)
                .await?;
        Ok(json
            .and_then(|j| parse("track preferences", &j))
            .unwrap_or_default())
    }

    pub async fn set_track_preferences(
        &self,
        principal: Principal,
        preferences: &TrackPreferences,
    ) -> Result<(), StoreError> {
        let json = serde_json::to_string(preferences).expect("preferences always serialize");
        sqlx::query(
            "INSERT INTO track_preferences (principal, preferences, updated_at) \
             VALUES (?1, ?2, ?3) \
             ON CONFLICT (principal) DO UPDATE SET \
               preferences = excluded.preferences, updated_at = excluded.updated_at",
        )
        .bind(principal.to_string())
        .bind(json)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// What the principal picked for `scope` (a show or a movie), if
    /// anything.
    pub async fn track_override(
        &self,
        principal: Principal,
        scope: &str,
    ) -> Result<Option<TrackOverride>, StoreError> {
        let json: Option<String> = sqlx::query_scalar(
            "SELECT override FROM track_overrides WHERE principal = ?1 AND scope = ?2",
        )
        .bind(principal.to_string())
        .bind(scope)
        .fetch_optional(&self.pool)
        .await?;
        Ok(json.and_then(|j| parse("a track override", &j)))
    }

    pub async fn set_track_override(
        &self,
        principal: Principal,
        scope: &str,
        override_: &TrackOverride,
    ) -> Result<(), StoreError> {
        let json = serde_json::to_string(override_).expect("overrides always serialize");
        sqlx::query(
            "INSERT INTO track_overrides (principal, scope, override, updated_at) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (principal, scope) DO UPDATE SET \
               override = excluded.override, updated_at = excluded.updated_at",
        )
        .bind(principal.to_string())
        .bind(scope)
        .bind(json)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn clear_track_override(
        &self,
        principal: Principal,
        scope: &str,
    ) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM track_overrides WHERE principal = ?1 AND scope = ?2")
            .bind(principal.to_string())
            .bind(scope)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use spoolframe_types::{SubtitleMode, SubtitleRole};

    use super::super::tests::state_dir;
    use super::*;
    use crate::store::users::LocalUserId;
    use crate::tracks::SubtitleChoice;

    const ALICE: Principal = Principal::LocalUser(LocalUserId(1));
    const BOB: Principal = Principal::LocalUser(LocalUserId(2));

    #[tokio::test]
    async fn preferences_are_per_principal_with_defaults() {
        let store = Store::open(&state_dir("track-preferences")).await.unwrap();
        let mut always = TrackPreferences::default();
        always.subtitles.mode = SubtitleMode::Always;
        store.set_track_preferences(ALICE, &always).await.unwrap();
        assert_eq!(store.track_preferences(ALICE).await.unwrap(), always);
        assert_eq!(
            store.track_preferences(BOB).await.unwrap(),
            TrackPreferences::default()
        );
    }

    #[tokio::test]
    async fn an_override_is_kept_cleared_and_scoped() {
        let store = Store::open(&state_dir("track-overrides")).await.unwrap();
        let sdh = TrackOverride {
            audio: None,
            subtitle: Some(SubtitleChoice::Track {
                language: Some("eng".into()),
                role: SubtitleRole::Sdh,
            }),
        };
        let show = "plex:show:10";
        store.set_track_override(ALICE, show, &sdh).await.unwrap();
        assert_eq!(store.track_override(ALICE, show).await.unwrap(), Some(sdh));
        assert_eq!(store.track_override(BOB, show).await.unwrap(), None);
        assert_eq!(
            store.track_override(ALICE, "plex:movie:1").await.unwrap(),
            None
        );
        store.clear_track_override(ALICE, show).await.unwrap();
        assert_eq!(store.track_override(ALICE, show).await.unwrap(), None);
    }
}
