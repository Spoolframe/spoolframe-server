//! What this server last gave each pair of each library it shares with it
//! (P8-02): the state a sync is diffed against, by cursor, and the
//! artwork that state lets the pair fetch. And the external ids read for
//! each item, kept with the hash of the rest of the item.
//!
//! At most two states are kept per pair and library: the one the pair
//! said it has, and the one last sent, which it has only once it names it
//! in its next sync.

use std::collections::HashMap;

use super::{Store, StoreError};

/// An item's external ids, read from the catalog when the rest of it
/// hashed to `shallow`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptIds {
    pub item: String,
    /// The library walked.
    pub library: String,
    pub shallow: String,
    pub external_ids: Vec<String>,
}

/// One item of a state: its id, its hash, and the images it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentItem {
    pub item: String,
    pub hash: String,
    pub artwork: Vec<String>,
}

impl Store {
    /// The items of pair `peer`'s state `cursor` of `library`, by id, with
    /// their hashes; `None` when no such state is kept.
    pub async fn sync_state(
        &self,
        peer: &str,
        library: &str,
        cursor: &str,
    ) -> Result<Option<HashMap<String, String>>, StoreError> {
        let mut conn = self.pool.acquire().await?;
        let kept: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM federation_sync_states \
                            WHERE peer = ?1 AND library = ?2 AND cursor = ?3)",
        )
        .bind(peer)
        .bind(library)
        .bind(cursor)
        .fetch_one(&mut *conn)
        .await?;
        if !kept {
            return Ok(None);
        }
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT item, hash FROM federation_sync_items \
             WHERE peer = ?1 AND library = ?2 AND cursor = ?3",
        )
        .bind(peer)
        .bind(library)
        .bind(cursor)
        .fetch_all(&mut *conn)
        .await?;
        Ok(Some(rows.into_iter().collect()))
    }

    /// Keep `items` as pair `peer`'s state `cursor` of `library`, beside
    /// `since` if the pair named one this server keeps; every other state
    /// of the pair's library goes. With `cursor` equal to `since`, nothing
    /// is added: the pair has it, and only the others go.
    pub async fn keep_sync_state(
        &self,
        peer: &str,
        library: &str,
        since: Option<&str>,
        cursor: &str,
        items: &[SentItem],
    ) -> Result<(), StoreError> {
        let mut tx = self.begin_write().await?;
        sqlx::query(
            "DELETE FROM federation_sync_states \
             WHERE peer = ?1 AND library = ?2 AND cursor IS NOT ?3 AND cursor IS NOT ?4",
        )
        .bind(peer)
        .bind(library)
        .bind(since)
        .bind(cursor)
        .execute(&mut *tx)
        .await?;
        if since != Some(cursor) {
            sqlx::query(
                "INSERT INTO federation_sync_states (peer, library, cursor, made_at) \
                 VALUES (?1, ?2, ?3, ?4)",
            )
            .bind(peer)
            .bind(library)
            .bind(cursor)
            .bind(self.now())
            .execute(&mut *tx)
            .await?;
            // Every item and image in one statement each, from a JSON
            // array: a library's state is thousands of rows.
            let rows: Vec<(&str, &str)> = items
                .iter()
                .map(|i| (i.item.as_str(), i.hash.as_str()))
                .collect();
            sqlx::query(
                "INSERT INTO federation_sync_items (peer, library, cursor, item, hash) \
                 SELECT ?1, ?2, ?3, value ->> 0, value ->> 1 FROM json_each(?4)",
            )
            .bind(peer)
            .bind(library)
            .bind(cursor)
            .bind(serde_json::to_string(&rows).expect("strings serialize"))
            .execute(&mut *tx)
            .await?;
            let artwork: std::collections::BTreeSet<&str> = items
                .iter()
                .flat_map(|i| i.artwork.iter().map(String::as_str))
                .collect();
            sqlx::query(
                "INSERT INTO federation_sync_artwork (peer, library, cursor, artwork) \
                 SELECT ?1, ?2, ?3, value FROM json_each(?4)",
            )
            .bind(peer)
            .bind(library)
            .bind(cursor)
            .bind(serde_json::to_string(&artwork).expect("strings serialize"))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Forget every state of pair `peer`'s, or of only its `library`:
    /// what a revoked grant or an unpairing leaves nothing of.
    pub async fn forget_sync_states(
        &self,
        peer: &str,
        library: Option<&str>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "DELETE FROM federation_sync_states WHERE peer = ?1 AND (?2 IS NULL OR library = ?2)",
        )
        .bind(peer)
        .bind(library)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The libraries whose kept states, of pair `peer`'s, name image
    /// `artwork`: where it was sent from.
    pub async fn sent_artwork_libraries(
        &self,
        peer: &str,
        artwork: &str,
    ) -> Result<Vec<String>, StoreError> {
        Ok(sqlx::query_scalar(
            "SELECT DISTINCT library FROM federation_sync_artwork \
             WHERE peer = ?1 AND artwork = ?2",
        )
        .bind(peer)
        .bind(artwork)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The kept external ids of `library`'s items, by item: each with the
    /// hash of the rest of it when they were read. Read once per walk of
    /// the library, not once per item, and not the other libraries'.
    pub async fn external_ids_of(
        &self,
        library: &str,
    ) -> Result<HashMap<String, (String, Vec<String>)>, StoreError> {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT item, shallow, external_ids FROM federation_external_ids WHERE library = ?1",
        )
        .bind(library)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(item, shallow, ids)| {
                Some((item, (shallow, serde_json::from_str(&ids).ok()?)))
            })
            .collect())
    }

    /// Keep each of `read`'s external ids for its item, read when the
    /// rest of it hashed as said: a walk's, in one transaction.
    pub async fn keep_all_external_ids(&self, read: &[KeptIds]) -> Result<(), StoreError> {
        if read.is_empty() {
            return Ok(());
        }
        let rows: Vec<(&str, &str, &str, String)> = read
            .iter()
            .map(|k| {
                let ids = serde_json::to_string(&k.external_ids).expect("strings serialize");
                (k.item.as_str(), k.library.as_str(), k.shallow.as_str(), ids)
            })
            .collect();
        sqlx::query(
            "INSERT INTO federation_external_ids (item, library, shallow, external_ids) \
             SELECT value ->> 0, value ->> 1, value ->> 2, value ->> 3 FROM json_each(?1) \
             WHERE true \
             ON CONFLICT (item) DO UPDATE SET library = excluded.library, \
                 shallow = excluded.shallow, external_ids = excluded.external_ids",
        )
        .bind(serde_json::to_string(&rows).expect("strings serialize"))
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::federation::NewPeer;
    use super::super::tests::{TestStore, test_store};
    use super::*;

    async fn paired(test: &str) -> (TestStore, String) {
        let store = test_store(test).await;
        let peer = store
            .add_peer(
                NewPeer {
                    server_id: "mzxw6ytbojtg633c",
                    endpoint_id: &"0b".repeat(32),
                    display_name: "Bob's server",
                    addrs: &[],
                    relays: &[],
                },
                "alice",
            )
            .await
            .unwrap();
        (store, peer.id)
    }

    fn sent(item: &str, hash: &str) -> SentItem {
        SentItem {
            item: item.into(),
            hash: hash.into(),
            artwork: vec![format!("{item}/poster")],
        }
    }

    /// A state is kept beside the one the pair has; naming the newer one
    /// drops the older; an unknown cursor has no state.
    #[tokio::test]
    async fn the_state_named_and_the_one_sent_are_kept_and_no_others() {
        let (store, peer) = paired("fed-sync-state").await;
        store
            .keep_sync_state(&peer, "plex:1", None, "c1", &[sent("a", "1")])
            .await
            .unwrap();
        store
            .keep_sync_state(&peer, "plex:1", Some("c1"), "c2", &[sent("a", "2")])
            .await
            .unwrap();
        let state = |cursor| {
            let store = &store;
            let peer = peer.clone();
            async move { store.sync_state(&peer, "plex:1", cursor).await.unwrap() }
        };
        assert_eq!(state("c1").await.unwrap()["a"], "1");
        assert_eq!(state("c2").await.unwrap()["a"], "2");
        // The pair has c2 now: c1 goes, and nothing is added.
        store
            .keep_sync_state(&peer, "plex:1", Some("c2"), "c2", &[])
            .await
            .unwrap();
        assert_eq!(state("c1").await, None);
        assert_eq!(state("c2").await.unwrap()["a"], "2");
        assert_eq!(state("nope").await, None);
        assert_eq!(
            store
                .sent_artwork_libraries(&peer, "a/poster")
                .await
                .unwrap(),
            ["plex:1"]
        );
        // A snapshot leaves only itself.
        store
            .keep_sync_state(&peer, "plex:1", None, "c3", &[])
            .await
            .unwrap();
        assert_eq!(state("c2").await, None);
        assert!(
            store
                .sent_artwork_libraries(&peer, "a/poster")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn forgotten_states_are_gone_with_their_artwork() {
        let (store, peer) = paired("fed-sync-forget").await;
        for library in ["plex:1", "plex:2"] {
            store
                .keep_sync_state(&peer, library, None, "c1", &[sent("a", "1")])
                .await
                .unwrap();
        }
        store
            .forget_sync_states(&peer, Some("plex:1"))
            .await
            .unwrap();
        assert_eq!(
            store
                .sent_artwork_libraries(&peer, "a/poster")
                .await
                .unwrap(),
            ["plex:2"]
        );
        store.forget_sync_states(&peer, None).await.unwrap();
        assert_eq!(store.sync_state(&peer, "plex:2", "c1").await.unwrap(), None);
    }

    /// A walk's external ids are kept together, each with the hash of
    /// the rest of its item; one read again replaces what was kept. A walk
    /// reads its own library's.
    #[tokio::test]
    async fn external_ids_are_kept_with_the_rest_of_the_item_s_hash() {
        let store = test_store("fed-external-ids").await;
        let kept = |item: &str, shallow: &str, ids: &[&str]| KeptIds {
            item: item.into(),
            library: "plex:1".into(),
            shallow: shallow.into(),
            external_ids: ids.iter().map(|&i| i.to_owned()).collect(),
        };
        store
            .keep_all_external_ids(&[
                kept("plex:movie:1", "s1", &["tmdb://348"]),
                kept("plex:movie:2", "s1", &[]),
                KeptIds {
                    library: "plex:2".into(),
                    ..kept("plex:movie:3", "s1", &["tmdb://1"])
                },
            ])
            .await
            .unwrap();
        store
            .keep_all_external_ids(&[kept("plex:movie:1", "s2", &["tmdb://348", "imdb://tt1"])])
            .await
            .unwrap();
        let all = store.external_ids_of("plex:1").await.unwrap();
        assert_eq!(all.len(), 2, "the walked library's alone");
        assert_eq!(
            all["plex:movie:1"],
            (
                "s2".to_owned(),
                vec!["tmdb://348".to_owned(), "imdb://tt1".to_owned()]
            )
        );
        assert_eq!(all["plex:movie:2"], ("s1".to_owned(), Vec::new()));
    }
}
