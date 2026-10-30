//! Subtitles from subtitle sites on demand: OpenSubtitles, SubDL or both,
//! as the server's admin set, searched and downloaded by the server, with its own
//! key for each, only when someone asks.
//!
//! [`Session::search_subtitles`] lists what the sites have for an item, in
//! one list, best first: found by the file's hash (made for this release),
//! then by rating and downloads; machine-translated and AI-made after
//! people's, each marked; ones removed before as bad last, marked
//! `rejected`. Each result names its site (`SubtitleResult::source`,
//! `opensubtitles` or `subdl`: show it as "OpenSubtitles" or "SubDL"),
//! and `SubtitleSearch::sources` says how each site's part went: one out
//! of quota or down leaves the others' results.
//! [`Session::download_subtitle`] makes one a track of the item, for
//! everyone on the server, at once: the reply has its place in the
//! item's subtitles and the track itself, and the downloads left today.
//! One not found by the hash is synced in the background (its
//! `SubtitleTrack::timing` says "Syncing" until then). A downloaded track
//! has `SubtitleTrack::download` on [`Session::tracks`]: which site it
//! came from, who downloaded it, and whether this user may remove it
//! ([`Session::remove_subtitle_download`]). A server with no site set up
//! answers `NoSubtitleSource` (hide "Find subtitles…"); a quota used up,
//! `QuotaExhausted` with whose it is and, for an account, its `site`.

use spoolframe_protocol::{Reply, Request};
use spoolframe_types::{
    BitmapPreference, ForcedPreference, MediaId, SdhPreference, SubtitleDownloaded, SubtitleSearch,
    TrackPreferences,
};

use crate::{SdkError, Session};

impl Session {
    /// Change some of this user's track preferences with `change`, the
    /// others as read: [`Session::set_subtitle_search`] says what that
    /// loses.
    async fn change_track_preferences(
        &self,
        change: impl FnOnce(&mut TrackPreferences),
    ) -> Result<(), SdkError> {
        let mut preferences = self.track_preferences().await?.preferences;
        change(&mut preferences);
        self.set_track_preferences(&preferences).await
    }

    /// Keep `sdh` and `forced` as this user's subtitle search
    /// preferences, every other track preference as it is.
    ///
    /// The wire has only a whole write, so this and
    /// [`Session::set_picture_subtitles`] read the preferences and write
    /// them all back, as [`Session::set_track_preferences`] does; neither
    /// is a change of its fields alone:
    ///
    /// - a change another device makes to another preference in between
    ///   is lost;
    /// - a value this build does not know, from a newer server or app,
    ///   is read as its fallback (an unknown `SubtitleMode` as
    ///   `ForcedOnly`, an unknown `BitmapPreference` as `Text`) and
    ///   written back as that, replacing the newer value.
    pub async fn set_subtitle_search(
        &self,
        sdh: SdhPreference,
        forced: ForcedPreference,
    ) -> Result<(), SdkError> {
        self.change_track_preferences(|p| {
            p.subtitles.sdh = sdh;
            p.subtitles.forced = forced;
        })
        .await
    }

    /// Read picture subtitles (PGS, VobSub, DVB) as text, or show them as
    /// pictures, by `bitmap`, the others as they are: a read and a whole
    /// write, with what that loses ([`Session::set_subtitle_search`]).
    pub async fn set_picture_subtitles(&self, bitmap: BitmapPreference) -> Result<(), SdkError> {
        self.change_track_preferences(|p| p.subtitles.bitmap = bitmap)
            .await
    }

    /// Search for item `id`'s subtitles in `language` (`en`, `pt-BR`), or
    /// with `None` in the languages the server offers first. Kept a day
    /// on the server: asking again costs nothing.
    pub async fn search_subtitles(
        &self,
        id: &MediaId,
        language: Option<&str>,
    ) -> Result<SubtitleSearch, SdkError> {
        match self
            .call(Request::SearchSubtitles {
                id: id.clone(),
                language: language.map(str::to_owned),
            })
            .await?
        {
            Reply::SubtitleResults(found) => Ok(found),
            other => Err(SdkError::Protocol(format!("unexpected reply {other:?}"))),
        }
    }

    /// Download result `result` (its `SubtitleResult::id`, which names
    /// its site) of a search for item `id`: a track of the item for
    /// everyone, usable at once. The same result again gives the same
    /// track.
    pub async fn download_subtitle(
        &self,
        id: &MediaId,
        result: &str,
    ) -> Result<SubtitleDownloaded, SdkError> {
        match self
            .call(Request::DownloadSubtitle {
                id: id.clone(),
                result: result.to_owned(),
            })
            .await?
        {
            Reply::SubtitleDownloaded(got) => Ok(got),
            other => Err(SdkError::Protocol(format!("unexpected reply {other:?}"))),
        }
    }

    /// Remove item `id`'s downloaded subtitle `download`
    /// (`SubtitleDownload::id`), for everyone: its downloader or an admin
    /// may.
    pub async fn remove_subtitle_download(
        &self,
        id: &MediaId,
        download: &str,
    ) -> Result<(), SdkError> {
        match self
            .call(Request::RemoveSubtitleDownload {
                id: id.clone(),
                download: download.to_owned(),
            })
            .await?
        {
            Reply::Done => Ok(()),
            other => Err(SdkError::Protocol(format!("unexpected reply {other:?}"))),
        }
    }
}
