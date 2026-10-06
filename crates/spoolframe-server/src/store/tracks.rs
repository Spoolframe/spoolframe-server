//! Track preferences and overrides (P1-20). The choosing is in
//! `crate::tracks`; this is storage.

use spoolframe_types::TrackPreferences;

use super::{Store, StoreError};
use crate::principal::Principal;
use crate::tracks::TrackOverride;

/// A stored document that does not parse. Only this build writes them, so
/// it is a bug or a hand edit; reading falls back to the defaults.
fn parse<T: serde::de::DeserializeOwned>(what: &str, json: &str) -> Option<T> {
    serde_json::from_str(json)
        .inspect_err(|e| tracing::warn!(what, error = %e, "ignoring an unreadable stored document"))
        .ok()
}

/// A stored `TrackPreferences` row, which may be from a build before some
/// of its fields existed (PX-13's `subtitles.bitmap`, for one).
///
/// The wire type has no serde defaults (PX-15), so the row is laid over
/// the defaults first: each key the row has wins, each it lacks takes
/// `TrackPreferences::default()`'s. This keeps the defaults in one place,
/// the `Default` impls, rather than in a migration's JSON literal or in a
/// second set of structs that must follow every new field. Rows are
/// written whole by `set_track_preferences`, so a read rewrites nothing.
fn parse_preferences(json: &str) -> Option<TrackPreferences> {
    let stored: serde_json::Value = parse("track preferences", json)?;
    let mut full =
        serde_json::to_value(TrackPreferences::default()).expect("preferences always serialize");
    overlay(&mut full, stored);
    parse("track preferences", &full.to_string())
}

/// `top` over `base`: objects merge key by key, anything else replaces.
fn overlay(base: &mut serde_json::Value, top: serde_json::Value) {
    match (base, top) {
        (serde_json::Value::Object(base), serde_json::Value::Object(top)) => {
            for (key, value) in top {
                match base.get_mut(&key) {
                    Some(slot) => overlay(slot, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, top) => *base = top,
    }
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
        Ok(json.and_then(|j| parse_preferences(&j)).unwrap_or_default())
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
        .bind(self.now())
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
        .bind(self.now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every override the principal has, most recently made first: the
    /// scope (a show's or a movie's id) and what was picked.
    pub async fn track_overrides(
        &self,
        principal: Principal,
    ) -> Result<Vec<(String, TrackOverride)>, StoreError> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT scope, override FROM track_overrides WHERE principal = ?1 \
             ORDER BY updated_at DESC, scope",
        )
        .bind(principal.to_string())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(scope, json)| Some((scope, parse("a track override", &json)?)))
            .collect())
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

    use super::super::tests::test_store;
    use super::*;
    use crate::store::users::LocalUserId;
    use crate::tracks::SubtitleChoice;

    const ALICE: Principal = Principal::LocalUser(LocalUserId::from_uuid(uuid::Uuid::from_u128(1)));
    const BOB: Principal = Principal::LocalUser(LocalUserId::from_uuid(uuid::Uuid::from_u128(2)));

    #[tokio::test]
    async fn preferences_are_per_principal_with_defaults() {
        let store = test_store("track-preferences").await;
        let mut always = TrackPreferences::default();
        always.subtitles.mode = SubtitleMode::Always;
        store.set_track_preferences(ALICE, &always).await.unwrap();
        assert_eq!(store.track_preferences(ALICE).await.unwrap(), always);
        assert_eq!(
            store.track_preferences(BOB).await.unwrap(),
            TrackPreferences::default()
        );
    }

    /// Put `json` in `principal`'s row as an older build wrote it.
    async fn plant_preferences(store: &Store, principal: Principal, json: &str) {
        sqlx::query(
            "INSERT INTO track_preferences (principal, preferences, updated_at) \
             VALUES (?1, ?2, 0)",
        )
        .bind(principal.to_string())
        .bind(json)
        .execute(&store.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_row_from_before_picture_subtitles_reads_with_the_new_default() {
        let store = test_store("track-preferences-pre-px13").await;
        // PX-13 added `subtitles.bitmap`; a row written before it has none.
        plant_preferences(
            &store,
            ALICE,
            r#"{"audio":{"languages":["jpn"],"avoid":[]},
                "subtitles":{"mode":"always","languages":["eng"],"sdh":"avoid"}}"#,
        )
        .await;
        let mut expected = TrackPreferences::default();
        expected.audio.languages = vec!["jpn".into()];
        expected.audio.avoid = vec![];
        expected.subtitles.mode = SubtitleMode::Always;
        expected.subtitles.languages = vec!["eng".into()];
        expected.subtitles.sdh = spoolframe_types::SdhPreference::Avoid;
        assert_eq!(store.track_preferences(ALICE).await.unwrap(), expected);
    }

    #[tokio::test]
    async fn a_partial_row_keeps_what_it_has_and_takes_the_rest_from_the_defaults() {
        let store = test_store("track-preferences-partial").await;
        plant_preferences(&store, ALICE, r#"{"audio":{"languages":["eng"]}}"#).await;
        plant_preferences(&store, BOB, "{}").await;
        let mut expected = TrackPreferences::default();
        expected.audio.languages = vec!["eng".into()];
        assert_eq!(store.track_preferences(ALICE).await.unwrap(), expected);
        assert_eq!(
            store.track_preferences(BOB).await.unwrap(),
            TrackPreferences::default()
        );
    }

    #[tokio::test]
    async fn a_row_that_is_not_preferences_reads_as_the_defaults() {
        let store = test_store("track-preferences-garbage").await;
        plant_preferences(&store, ALICE, r#"{"audio":"loud"}"#).await;
        assert_eq!(
            store.track_preferences(ALICE).await.unwrap(),
            TrackPreferences::default()
        );
    }

    #[tokio::test]
    async fn an_override_is_kept_cleared_and_scoped() {
        let store = test_store("track-overrides").await;
        let sdh = TrackOverride {
            audio: None,
            subtitle: Some(SubtitleChoice::Track {
                language: Some("eng".into()),
                role: SubtitleRole::Sdh,
            }),
        };
        let show = "plex:show:10";
        store.set_track_override(ALICE, show, &sdh).await.unwrap();
        assert_eq!(
            store.track_override(ALICE, show).await.unwrap(),
            Some(sdh.clone())
        );
        assert_eq!(store.track_override(BOB, show).await.unwrap(), None);
        assert_eq!(
            store.track_override(ALICE, "plex:movie:1").await.unwrap(),
            None
        );
        assert_eq!(
            store.track_overrides(ALICE).await.unwrap(),
            [(show.to_owned(), sdh)]
        );
        assert_eq!(store.track_overrides(BOB).await.unwrap(), []);
        store.clear_track_override(ALICE, show).await.unwrap();
        assert_eq!(store.track_override(ALICE, show).await.unwrap(), None);
        assert_eq!(store.track_overrides(ALICE).await.unwrap(), []);
    }
}
