//! Playlists, Liked and All.

use spoolframe_client_sdk::types::MediaId;
use spoolframe_client_sdk::{
    Kinds, PlaySettings, PlaylistEdit, PlaylistEntry, PlaylistList, PlaylistNext, PlaylistPage,
    PlaylistSummary, PlaylistUpdate, Visibility,
};

use crate::error::SpoolError;
use crate::session::SpoolSession;

#[uniffi::export]
impl SpoolSession {
    /// The user's playlists, and the notes of ones pinned that went away.
    pub async fn playlists(&self) -> Result<PlaylistList, SpoolError> {
        self.call(|s| async move { s.playlists().await }).await
    }

    /// `limit` (200 at most) entries of playlist `id` from `offset`.
    pub async fn playlist(
        &self,
        id: String,
        offset: u32,
        limit: u32,
    ) -> Result<PlaylistPage, SpoolError> {
        self.call(|s| async move { s.playlist(&id, offset, limit).await })
            .await
    }

    /// A playlist of the user's.
    pub async fn create_playlist(
        &self,
        name: String,
        kinds: Kinds,
        visibility: Visibility,
    ) -> Result<PlaylistSummary, SpoolError> {
        self.call(|s| async move { s.create_playlist(&name, kinds, visibility).await })
            .await
    }

    /// Rename, widen or reshare: each `None` left as it is.
    pub async fn update_playlist(
        &self,
        id: String,
        update: PlaylistUpdate,
    ) -> Result<PlaylistSummary, SpoolError> {
        self.call(|s| async move { s.update_playlist(&id, update).await })
            .await
    }

    /// `edits` on the playlist as seen at `revision`: its new revision,
    /// and the entries its adds made when the server says (for Undo).
    /// `PlaylistConflict` when someone changed the same entries since.
    pub async fn edit_playlist(
        &self,
        id: String,
        revision: u64,
        edits: Vec<PlaylistEdit>,
    ) -> Result<spoolframe_client_sdk::PlaylistEdited, SpoolError> {
        self.call(|s| async move { s.edit_playlist(&id, revision, edits).await })
            .await
    }

    pub async fn delete_playlist(&self, id: String) -> Result<(), SpoolError> {
        self.call(|s| async move { s.delete_playlist(&id).await })
            .await
    }

    pub async fn pin_playlist(&self, id: String, position: u32) -> Result<(), SpoolError> {
        self.call(|s| async move { s.pin_playlist(&id, position).await })
            .await
    }

    pub async fn unpin_playlist(&self, id: String) -> Result<(), SpoolError> {
        self.call(|s| async move { s.unpin_playlist(&id).await })
            .await
    }

    pub async fn dismiss_pin_notes(&self) -> Result<(), SpoolError> {
        self.call(|s| async move { s.dismiss_pin_notes().await })
            .await
    }

    /// The entry to play, from entry `from` or the user's place; `None`
    /// when nothing is left.
    pub async fn play_playlist(
        &self,
        id: String,
        from: Option<u64>,
        settings: PlaySettings,
    ) -> Result<Option<PlaylistNext>, SpoolError> {
        self.call(|s| async move { s.play_playlist(&id, from, settings).await })
            .await
    }

    /// What plays after entry `after`; `None` at the end.
    pub async fn next_in_playlist(
        &self,
        id: String,
        after: u64,
    ) -> Result<Option<PlaylistNext>, SpoolError> {
        self.call(|s| async move { s.next_in_playlist(&id, after).await })
            .await
    }

    /// What plays next, without playing it: at most `limit` (100) after
    /// the user's place, in the order `next_in_playlist` gives them (the
    /// shuffled round's while shuffling).
    pub async fn upcoming_in_playlist(
        &self,
        id: String,
        limit: u32,
    ) -> Result<Vec<PlaylistEntry>, SpoolError> {
        self.call(|s| async move { s.upcoming_in_playlist(&id, limit).await })
            .await
    }

    /// Keep `settings` as how the user plays the playlist, playing
    /// nothing: the place and the round stay.
    pub async fn set_playlist_settings(
        &self,
        id: String,
        settings: PlaySettings,
    ) -> Result<(), SpoolError> {
        self.call(|s| async move { s.set_playlist_settings(&id, settings).await })
            .await
    }

    /// Heart tracks or albums: Liked after.
    pub async fn like(&self, ids: Vec<MediaId>) -> Result<PlaylistSummary, SpoolError> {
        self.call(|s| async move { s.like(&ids).await }).await
    }

    pub async fn unlike(&self, ids: Vec<MediaId>) -> Result<PlaylistSummary, SpoolError> {
        self.call(|s| async move { s.unlike(&ids).await }).await
    }
}
