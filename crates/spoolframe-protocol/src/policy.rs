//! Request retry semantics and wait budgets shared by clients and servers.
//! These do not change message encoding or protocol negotiation.

use crate::Request;
use std::time::Duration;

/// Shared server waits and client allowances. Each is how long the server
/// may take before it answers, not counting the round trip.
pub mod wait {
    use std::time::Duration;

    /// `AwaitDeviceSignIn` (the server's `device_sign_in::AWAIT`).
    pub const SIGN_IN: Duration = Duration::from_secs(25);
    /// A read past what an encode has written, or of a subtitle still
    /// being read as text (the server's `READ_WAIT`).
    pub const READ: Duration = Duration::from_secs(10);
    /// A request that may run ffprobe on the file (`Prober::DEFAULT_TIMEOUT`).
    pub const PROBE: Duration = Duration::from_secs(30);
    /// Starting a playback, or changing its subtitle: a probe, and for HLS
    /// of a copied video every keyframe of the file, which the server
    /// bounds only at 10 minutes. Two minutes is longer than a player
    /// should spin; past it the user is better told.
    pub const PLAYBACK: Duration = Duration::from_secs(120);
    /// A request the server answers from a subtitle site: a
    /// search, or a sign-in, a download and the file. Each call to the
    /// site has 10 seconds.
    pub const SUBTITLE_SITE: Duration = Duration::from_secs(30);
}

/// How long the server may hold `request` before it answers.
pub fn server_wait(request: &Request) -> Duration {
    match request {
        Request::AwaitDeviceSignIn { .. } => wait::SIGN_IN,
        Request::ReadSubtitle { .. } | Request::ReadSubtitleSegment { .. } => wait::READ,
        Request::Tracks { .. }
        | Request::PlaybackPlan { .. }
        | Request::SetTrackOverride { .. }
        // Each probes the file to find the track.
        | Request::SyncSubtitle { .. }
        | Request::UseOriginalSubtitleTiming { .. }
        | Request::SetSubtitleOffset { .. }
        // Each probes the file to plan the download.
        | Request::DownloadOptions { .. }
        | Request::PrepareDownload { .. }
        // An artist's albums are read one by one from the catalog.
        | Request::MusicDownloadChanges { .. } => wait::PROBE,
        Request::SearchSubtitles { .. } | Request::DownloadSubtitle { .. } => wait::SUBTITLE_SITE,
        Request::StartPlayback { .. }
        | Request::ChangeSubtitle { .. }
        | Request::ChangeQuality { .. } => wait::PLAYBACK,
        _ => Duration::ZERO,
    }
}

/// Whether `request` may be sent a second time, on a new connection, when
/// the first one died before it was answered. Reads and idempotent connection
/// setup are safe:
/// one that changes something may have been done before its answer was
/// lost, and doing it twice is not the same as once (a sign-in approved
/// twice, a PIN tried twice). Every request is named, so a new one is
/// decided here when it is added.
pub fn retry_safe(request: &Request) -> bool {
    match request {
        Request::Hello(_)
        | Request::ActivateProfile
        | Request::PlaybackProgressSequence { .. }
        | Request::AuthMethods
        | Request::Ping
        | Request::Libraries
        | Request::Items { .. }
        | Request::ItemFilterValues { .. }
        | Request::Item { .. }
        | Request::NextEpisode { .. }
        | Request::ShowNext { .. }
        | Request::Search(_)
        | Request::PersonItems { .. }
        | Request::Tracks { .. }
        | Request::WhoAmI
        | Request::ListSessions
        | Request::PlaybackPlan { .. }
        | Request::PlaybackRevision { .. }
        | Request::ListDevices
        | Request::ListPasskeys
        | Request::GetPlexLink
        | Request::GetManifest { .. }
        | Request::ReadSubtitle { .. }
        | Request::ReadSubtitleSegment { .. }
        | Request::GetPreviews { .. }
        | Request::Home
        | Request::TrustedProfiles
        | Request::GetTrackPreferences
        // A second challenge only takes the place of the first, which
        // stays bounded per session and user.
        | Request::TieBegin
        // Public data read, and an offer that replaces itself.
        | Request::IdentityRecord { .. }
        | Request::IdentityRecordByHandle { .. }
        | Request::OfferPairing(_)
        | Request::ListInvites
        // Asked again, the same playback's live token, or a new
        // one bound to the same playback.
        | Request::HttpPlaybackUrl { .. }
        // Reads. A read may drop entries whose items left the
        // library, which a second read would drop too.
        | Request::ListPlaylists
        | Request::GetPlaylist { .. }
        // Liking what is liked, or unliking what is not, changes
        // nothing.
        | Request::Like { .. }
        | Request::Unlike { .. }
        // A read.
        | Request::ListShareCandidates { .. }
        // A read.
        | Request::RecentlyPlayed { .. }
        // Where a paired server is, read.
        | Request::RemoteHints { .. }
        // A read; settings saved twice are saved once, and shuffle turned
        // on twice shuffles once.
        | Request::UpcomingInPlaylist { .. }
        | Request::SetPlaylistSettings { .. }
        // Taken away twice is taken away once.
        | Request::RemovePlaylistArt { .. }
        // A second sync joins the first; original timing or an
        // offset set twice is set once.
        | Request::SyncSubtitle { .. }
        | Request::UseOriginalSubtitleTiming { .. }
        | Request::SetSubtitleOffset { .. }
        // A search is a read; the same result is never downloaded
        // twice (the second gets the first's track); a download removed
        // twice is removed once.
        | Request::SearchSubtitles { .. }
        | Request::DownloadSubtitle { .. }
        | Request::RemoveSubtitleDownload { .. }
        // Reads; the same request prepared twice is one download,
        // under one id; a download cancelled twice is cancelled once (the
        // second says `NotFound`).
        | Request::DownloadOptions { .. }
        | Request::PrepareDownload { .. }
        | Request::DownloadStatus { .. }
        | Request::CancelDownload { .. }
        // Newest wins: an entry written once is older the second time.
        | Request::SyncOfflineWatch { .. }
        // A read; a play is recorded once by its track and time,
        // and a like is newest-wins as a watch is.
        | Request::MusicDownloadChanges { .. }
        | Request::SyncOfflinePlays { .. }
        // A row shown twice is shown once.
        | Request::SetServerRow { .. }
        // A server chosen twice is chosen once.
        | Request::SetPlayFrom { .. } => true,
        // Signing in: a failed password counts, a challenge is replaced,
        // an approved sign-in is collected once.
        Request::LoginPassword(_)
        | Request::BeginDeviceSignIn { .. }
        | Request::AwaitDeviceSignIn { .. }
        | Request::BeginPasskeySignIn { .. }
        | Request::FinishPasskeySignIn { .. }
        | Request::BeginPortableSignIn
        | Request::FinishPortableSignIn(_)
        // An invite makes an account or a grant and uses itself up; a
        // passkey's registration is replaced.
        | Request::RedeemInvite(_)
        | Request::BeginInvitePasskey(_)
        | Request::RedeemInviteWithPasskey(_)
        | Request::RedeemPortableInvite(_)
        // A fetch counts against the code's tries, an answer is
        // taken once, and a collected answer is gone.
        | Request::FetchPairing { .. }
        | Request::AnswerPairing { .. }
        | Request::CollectPairing { .. }
        // A resume makes a session; trust changes end sessions.
        | Request::ResumeProfile { .. }
        | Request::SetDeviceTrust { .. }
        | Request::ForgetDevice { .. }
        | Request::Logout
        // Playback: each starts, stops or restarts an encode.
        | Request::StartPlayback { .. }
        | Request::StopPlayback { .. }
        | Request::SeekPlayback { .. }
        | Request::ChangeSubtitle { .. }
        | Request::ChangeQuality { .. }
        // PINs: a wrong one counts towards the lock-out.
        | Request::SetPin { .. }
        | Request::ClearPin { .. }
        | Request::UnlockProfile { .. }
        | Request::SetPinRequired { .. }
        | Request::BeginPasskeyRegistration
        | Request::FinishPasskeyRegistration(_)
        | Request::RenamePasskey { .. }
        | Request::RemovePasskey { .. }
        // A wrong current password counts; a new one ends sessions.
        | Request::ChangePassword(_)
        // Confirming: a wrong password or passkey counts, a challenge is
        // replaced or used up.
        | Request::ConfirmPassword(_)
        | Request::BeginConfirmPasskey
        | Request::FinishConfirmPasskey(_)
        // Looking up a wrong code consumes a failure-limit attempt too.
        | Request::LookupDeviceSignIn { .. }
        | Request::LookupInvite(_)
        | Request::ApproveDeviceSignIn { .. }
        | Request::DenyDeviceSignIn { .. }
        | Request::UpdatePlaybackProgress { .. }
        // As a playback's progress.
        | Request::UpdateWatchProgress { .. }
        | Request::MarkWatched { .. }
        | Request::MarkUnwatched { .. }
        | Request::HideFromContinueWatching { .. }
        | Request::UnhideFromContinueWatching { .. }
        | Request::SetTrackPreferences { .. }
        | Request::SetTrackOverride { .. }
        | Request::ClearTrackOverride { .. }
        // A tie uses its nonce up; severing one is a change.
        | Request::TieFinish { .. }
        | Request::Untie { .. }
        // A second invite would be a second code; a revoke is a change.
        | Request::CreateInvite(_)
        | Request::RevokeInvite { .. }
        // A pairing takes the other server's use.
        | Request::PairServer(_)
        // Sharing is a change an admin confirmed.
        | Request::ShareLibraries(_)
        // A play token works once.
        | Request::RedeemPlayToken(_)
        // Each mints a token and counts a play.
        | Request::RemoteGrant { .. }
        // Leaving ends every session; a second try finds none.
        | Request::LeaveServer
        // A second create is a second playlist; an edit's adds
        // would be added twice; pins move.
        | Request::CreatePlaylist { .. }
        | Request::UpdatePlaylist { .. }
        | Request::EditPlaylist { .. }
        | Request::DeletePlaylist { .. }
        | Request::PinPlaylist { .. }
        | Request::UnpinPlaylist { .. }
        | Request::DismissPinNotes
        // Each moves the user's place on.
        | Request::PlayPlaylist { .. }
        | Request::NextInPlaylist { .. }
        // Imported twice is made twice.
        | Request::ImportPlexPlaylists { .. }
        | Request::Unknown => false,
    }
}
