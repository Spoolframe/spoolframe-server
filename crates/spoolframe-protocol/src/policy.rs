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
    /// A request the server answers from a subtitle site (PX-02): a
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
        // PX-03: each probes the file to find the track.
        | Request::SyncSubtitle { .. }
        | Request::UseOriginalSubtitleTiming { .. }
        | Request::SetSubtitleOffset { .. }
        // PX-38: each probes the file to plan the download.
        | Request::DownloadOptions { .. }
        | Request::PrepareDownload { .. } => wait::PROBE,
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
        | Request::Item { .. }
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
        // stays bounded per session and user (P7-04).
        | Request::TieBegin
        // P7-08: public data read, and an offer that replaces itself.
        | Request::IdentityRecord { .. }
        | Request::IdentityRecordByHandle { .. }
        | Request::OfferPairing(_)
        | Request::ListInvites
        // PX-49: asked again, the same playback's live token, or a new
        // one bound to the same playback.
        | Request::HttpPlaybackUrl { .. }
        // PX-40: reads. A read may drop entries whose items left the
        // library, which a second read would drop too.
        | Request::ListPlaylists
        | Request::GetPlaylist { .. }
        // PX-43: liking what is liked, or unliking what is not, changes
        // nothing.
        | Request::Like { .. }
        | Request::Unlike { .. }
        // A read.
        | Request::ListShareCandidates { .. }
        // PX-52: a read.
        | Request::RecentlyPlayed { .. }
        // A read; settings saved twice are saved once, and shuffle turned
        // on twice shuffles once.
        | Request::UpcomingInPlaylist { .. }
        | Request::SetPlaylistSettings { .. }
        // Taken away twice is taken away once (PX-53).
        | Request::RemovePlaylistArt { .. }
        // PX-03: a second sync joins the first; original timing or an
        // offset set twice is set once.
        | Request::SyncSubtitle { .. }
        | Request::UseOriginalSubtitleTiming { .. }
        | Request::SetSubtitleOffset { .. }
        // PX-02: a search is a read; the same result is never downloaded
        // twice (the second gets the first's track); a download removed
        // twice is removed once.
        | Request::SearchSubtitles { .. }
        | Request::DownloadSubtitle { .. }
        | Request::RemoveSubtitleDownload { .. }
        // PX-38: reads; the same request prepared twice is one download,
        // under one id; a download cancelled twice is cancelled once (the
        // second says `NotFound`).
        | Request::DownloadOptions { .. }
        | Request::PrepareDownload { .. }
        | Request::DownloadStatus { .. }
        | Request::CancelDownload { .. }
        // Newest wins: an entry written once is older the second time.
        | Request::SyncOfflineWatch { .. } => true,
        // Signing in: a failed password counts, a challenge is replaced,
        // an approved sign-in is collected once.
        Request::LoginPassword(_)
        | Request::BeginDeviceSignIn { .. }
        | Request::AwaitDeviceSignIn { .. }
        | Request::BeginPasskeySignIn { .. }
        | Request::FinishPasskeySignIn { .. }
        | Request::BeginPortableSignIn
        | Request::FinishPortableSignIn(_)
        // An invite makes an account or a grant and uses itself up.
        | Request::RedeemInvite(_)
        | Request::RedeemPortableInvite(_)
        // P7-08: a fetch counts against the code's tries, an answer is
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
        | Request::ApproveDeviceSignIn { .. }
        | Request::DenyDeviceSignIn { .. }
        | Request::UpdatePlaybackProgress { .. }
        | Request::MarkWatched { .. }
        | Request::MarkUnwatched { .. }
        | Request::HideFromContinueWatching { .. }
        | Request::UnhideFromContinueWatching { .. }
        | Request::SetTrackPreferences { .. }
        | Request::SetTrackOverride { .. }
        | Request::ClearTrackOverride { .. }
        // A tie uses its nonce up; severing one is a change (P7-04).
        | Request::TieFinish { .. }
        | Request::Untie { .. }
        // A second invite would be a second code; a revoke is a change
        // (PX-44).
        | Request::CreateInvite(_)
        | Request::RevokeInvite { .. }
        // Leaving ends every session; a second try finds none (PX-47).
        | Request::LeaveServer
        // PX-40: a second create is a second playlist; an edit's adds
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
