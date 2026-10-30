//! What a scan found in each backdrop (PX-64): `crate::faces` scans,
//! this keeps it.

use spoolframe_protocol::ArtworkFace;
use spoolframe_types::ArtworkId;

use super::{Store, StoreError};

impl Store {
    /// The faces in backdrop `id`, none if it has none; `None` until it
    /// is scanned.
    pub async fn artwork_faces(
        &self,
        id: &ArtworkId,
    ) -> Result<Option<Vec<ArtworkFace>>, StoreError> {
        let row: Option<String> =
            sqlx::query_scalar("SELECT faces FROM artwork_faces WHERE artwork_id = ?1")
                .bind(id.to_string())
                .fetch_optional(&self.pool)
                .await?;
        // Only `keep_artwork_faces` writes them.
        Ok(row.and_then(|faces| serde_json::from_str(&faces).ok()))
    }

    /// Keep what a scan of `id` found. A scan already kept stands: an id
    /// names one image, so another's answer is the same.
    pub async fn keep_artwork_faces(
        &self,
        id: &ArtworkId,
        faces: &[ArtworkFace],
    ) -> Result<(), StoreError> {
        let faces = serde_json::to_string(faces).expect("numbers serialize");
        sqlx::query(
            "INSERT OR IGNORE INTO artwork_faces (artwork_id, faces, scanned_at) \
             VALUES (?1, ?2, unixepoch())",
        )
        .bind(id.to_string())
        .bind(faces)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use spoolframe_types::Namespace;

    use super::super::tests::test_store;
    use super::*;

    fn id(n: u32) -> ArtworkId {
        ArtworkId::new(Namespace::Plex, format!("/library/metadata/{n}/art/1")).unwrap()
    }

    const FACE: ArtworkFace = ArtworkFace {
        left: 0.25,
        right: 0.5,
        top: 0.125,
        bottom: 0.375,
        eyes: 0.25,
    };

    #[tokio::test]
    async fn no_row_is_not_scanned_and_an_empty_scan_is_kept_as_none_found() {
        let store = test_store("artwork-faces-none").await;
        assert_eq!(store.artwork_faces(&id(1)).await.unwrap(), None);
        store.keep_artwork_faces(&id(1), &[]).await.unwrap();
        assert_eq!(store.artwork_faces(&id(1)).await.unwrap(), Some(Vec::new()));
        assert_eq!(store.artwork_faces(&id(2)).await.unwrap(), None);
    }

    #[tokio::test]
    async fn faces_come_back_as_they_went_in() {
        let store = test_store("artwork-faces-kept").await;
        // A value with no short decimal, to show nothing is rounded.
        let exact = ArtworkFace {
            top: 0.1234567,
            ..FACE
        };
        store
            .keep_artwork_faces(&id(1), &[FACE, exact])
            .await
            .unwrap();
        assert_eq!(
            store.artwork_faces(&id(1)).await.unwrap(),
            Some(vec![FACE, exact])
        );
    }

    #[tokio::test]
    async fn the_first_scan_of_an_id_stands() {
        let store = test_store("artwork-faces-first").await;
        store.keep_artwork_faces(&id(1), &[FACE]).await.unwrap();
        store.keep_artwork_faces(&id(1), &[]).await.unwrap();
        assert_eq!(store.artwork_faces(&id(1)).await.unwrap(), Some(vec![FACE]));
    }
}
