//! Subtitle search and download (`spoolframe:subtitle-search`), subtitle
//! timing (`spoolframe:subtitle-sync`), playlist covers and Recently
//! played.

use spoolframe_client_sdk::types::{
    ArtworkId, MediaId, SubtitleDownloaded, SubtitleSearch, SubtitleTiming,
};
use spoolframe_client_sdk::{ItemsPage, Kinds, PlaylistSummary};

use crate::error::SpoolError;
use crate::session::SpoolSession;

#[uniffi::export]
impl SpoolSession {
    /// Item `id`'s subtitles on the sites the server searches, in
    /// `language` or the server's first languages.
    pub async fn search_subtitles(
        &self,
        id: MediaId,
        language: Option<String>,
    ) -> Result<SubtitleSearch, SpoolError> {
        self.call(|s| async move { s.search_subtitles(&id, language.as_deref()).await })
            .await
    }

    /// Download search result `result` for item `id`: the new track's
    /// index, the quotas left.
    pub async fn download_subtitle(
        &self,
        id: MediaId,
        result: String,
    ) -> Result<SubtitleDownloaded, SpoolError> {
        self.call(|s| async move { s.download_subtitle(&id, &result).await })
            .await
    }

    pub async fn remove_subtitle_download(
        &self,
        id: MediaId,
        download: String,
    ) -> Result<(), SpoolError> {
        self.call(|s| async move { s.remove_subtitle_download(&id, &download).await })
            .await
    }

    /// Sync subtitle `track` to the file in the background: its timing,
    /// now syncing.
    pub async fn sync_subtitle(
        &self,
        id: MediaId,
        track: u32,
    ) -> Result<SubtitleTiming, SpoolError> {
        self.call(|s| async move { s.sync_subtitle(&id, track).await })
            .await
    }

    pub async fn use_original_subtitle_timing(
        &self,
        id: MediaId,
        track: u32,
    ) -> Result<SubtitleTiming, SpoolError> {
        self.call(|s| async move { s.use_original_subtitle_timing(&id, track).await })
            .await
    }

    /// Move subtitle `track` by `offset_ms` (later when positive), for
    /// everyone; 0 resets it.
    pub async fn set_subtitle_offset(
        &self,
        id: MediaId,
        track: u32,
        offset_ms: i64,
    ) -> Result<SubtitleTiming, SpoolError> {
        self.call(|s| async move { s.set_subtitle_offset(&id, track, offset_ms).await })
            .await
    }

    /// A cover for playlist `id` (JPEG, PNG, WebP or HEIC): its artwork.
    pub async fn set_playlist_art(
        &self,
        id: String,
        bytes: Vec<u8>,
    ) -> Result<ArtworkId, SpoolError> {
        self.call(|s| async move { s.set_playlist_art(&id, bytes).await })
            .await
    }

    /// Back to the server's cover.
    pub async fn remove_playlist_art(&self, id: String) -> Result<PlaylistSummary, SpoolError> {
        self.call(|s| async move { s.remove_playlist_art(&id).await })
            .await
    }

    /// What the user played lately, newest first; `kinds` (tracks only
    /// for music's), or every kind.
    pub async fn recently_played(
        &self,
        kinds: Option<Kinds>,
        offset: u32,
        limit: u32,
    ) -> Result<ItemsPage, SpoolError> {
        self.call(|s| async move { s.recently_played(kinds, offset, limit).await })
            .await
    }
}
