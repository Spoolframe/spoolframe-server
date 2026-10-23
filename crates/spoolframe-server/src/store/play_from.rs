//! The server each user plays a title from (P8-05), by the title's kind
//! and external ids: what `SetPlayFrom` keeps, and a search's hits and an
//! item's copies read.

use spoolframe_types::MediaKind;

use super::{Store, StoreError};

/// What the user chose for a title: this server (`None`), or a paired
/// server by its server id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayFrom {
    pub server: Option<String>,
    /// Unix seconds.
    pub chosen_at: i64,
}

/// One of a user's choices, as kept: under one id of a title of `kind`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chosen {
    pub kind: String,
    pub external_id: String,
    pub choice: PlayFrom,
}

impl Store {
    /// `principal` plays the title of `kind` with `title_ids` from `server`
    /// (a paired server's id, or `None` for this one), in place of any
    /// earlier choice under those ids.
    pub async fn set_play_from(
        &self,
        principal: &str,
        kind: MediaKind,
        title_ids: &[String],
        server: Option<&str>,
    ) -> Result<(), StoreError> {
        // One statement: it takes the writer when it starts.
        sqlx::query(
            "INSERT INTO federation_play_from (principal, kind, external_id, server, chosen_at) \
             SELECT ?1, ?2, value, ?3, ?4 FROM json_each(?5) WHERE true \
             ON CONFLICT (principal, kind, external_id) DO UPDATE SET \
                 server = excluded.server, chosen_at = excluded.chosen_at",
        )
        .bind(principal)
        .bind(kind.as_str())
        .bind(server.unwrap_or(""))
        .bind(self.now())
        .bind(serde_json::to_string(title_ids).expect("strings serialize"))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every choice `principal` made: read once for a search or an item,
    /// and each title takes the newest of its own ([`chosen`]). A user
    /// chooses by hand, title by title, so they are few.
    pub async fn play_from_choices(&self, principal: &str) -> Result<Vec<Chosen>, StoreError> {
        let rows: Vec<(String, String, String, i64)> = sqlx::query_as(
            "SELECT kind, external_id, server, chosen_at FROM federation_play_from \
             WHERE principal = ?1",
        )
        .bind(principal)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(kind, external_id, server, chosen_at)| {
                let server = (!server.is_empty()).then_some(server);
                Chosen {
                    kind,
                    external_id,
                    choice: PlayFrom { server, chosen_at },
                }
            })
            .collect())
    }
}

/// The newest of `choices` kept for a title of `kind` under one of
/// `title`'s ids.
pub fn chosen<'a>(
    choices: &'a [Chosen],
    kind: MediaKind,
    title: &[String],
) -> Option<&'a PlayFrom> {
    choices
        .iter()
        .filter(|c| c.kind == kind.as_str() && title.contains(&c.external_id))
        .map(|c| &c.choice)
        .max_by_key(|choice| choice.chosen_at)
}

#[cfg(test)]
mod tests {
    use super::super::tests::test_store;
    use super::*;

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| (*s).to_owned()).collect()
    }

    /// A choice is the user's own, found by any of the title's ids, and a
    /// later one under any of them replaces it.
    #[tokio::test]
    async fn a_title_plays_from_the_server_last_chosen_for_it() {
        let store = test_store("play-from").await;
        let title = ids(&["tmdb://348", "imdb://tt0078748"]);
        store
            .set_play_from(
                "local:bob",
                MediaKind::Movie,
                &title,
                Some("aaaaaaaaaaaaaaaa"),
            )
            .await
            .unwrap();
        let choices = store.play_from_choices("local:bob").await.unwrap();
        assert_eq!(
            chosen(&choices, MediaKind::Movie, &ids(&["imdb://tt0078748"]))
                .and_then(|c| c.server.as_deref()),
            Some("aaaaaaaaaaaaaaaa")
        );
        assert!(
            store
                .play_from_choices("local:alice")
                .await
                .unwrap()
                .is_empty()
        );
        store
            .set_play_from("local:bob", MediaKind::Movie, &title, None)
            .await
            .unwrap();
        let choices = store.play_from_choices("local:bob").await.unwrap();
        assert_eq!(
            chosen(&choices, MediaKind::Movie, &title).map(|c| c.server.clone()),
            Some(None)
        );
    }

    /// TMDB numbers films and shows apart: a film's choice is not the
    /// show's with the same number, and the show's does not replace it.
    #[tokio::test]
    async fn a_film_and_a_show_with_one_number_choose_apart() {
        let store = test_store("play-from-kind").await;
        let title = ids(&["tmdb://348"]);
        store
            .set_play_from(
                "local:bob",
                MediaKind::Movie,
                &title,
                Some("aaaaaaaaaaaaaaaa"),
            )
            .await
            .unwrap();
        store
            .set_play_from(
                "local:bob",
                MediaKind::Show,
                &title,
                Some("bbbbbbbbbbbbbbbb"),
            )
            .await
            .unwrap();
        let choices = store.play_from_choices("local:bob").await.unwrap();
        let server = |kind| chosen(&choices, kind, &title).and_then(|c| c.server.clone());
        assert_eq!(
            server(MediaKind::Movie).as_deref(),
            Some("aaaaaaaaaaaaaaaa")
        );
        assert_eq!(server(MediaKind::Show).as_deref(), Some("bbbbbbbbbbbbbbbb"));
        assert_eq!(server(MediaKind::Episode), None);
    }
}
