//! What can go wrong, as an app needs to tell it apart.

use std::time::Duration;

use spoolframe_protocol::Error as ServerError;

use crate::pairing::PairingError;
use crate::portable::{CredentialError, PhraseError, StateError};
use crate::storage::StorageError;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SdkError {
    #[error("the operation belongs to an earlier server instance")]
    ServerRestarted,
    #[error("the operation expired; its outcome is no longer recoverable")]
    OperationExpired,
    #[error("the operation ID was already used with different arguments")]
    OperationConflict,
    #[error("the playback is closed")]
    PlaybackClosed,
    #[error("the media changed while recovering playback")]
    PlaybackChanged,
    #[error("{0}")]
    Unavailable(String),
    #[error("an older progress report was rejected")]
    StaleProgress,
    #[error("playback revision changed to {revision}")]
    RevisionConflict { revision: u64 },
    /// The app's storage failed. Show it; the SDK never works around it.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// The server cannot be reached right now.
    #[error("{0} cannot be reached")]
    Unreachable(String),
    /// The server did not answer in time ([`crate::Timeouts`]). Whether a
    /// request that changes something was done is not known: show the
    /// state again before offering to repeat it. A request that only
    /// reads has been tried again, once, on a new connection where the
    /// old one had died. Either way the next call works if the server is
    /// back: a dead connection has been dropped, and the next call dials.
    #[error("no answer in time: {0}")]
    TimedOut(String),
    /// Not saved, not heard on the LAN and no address given.
    #[error("no server {0} is known")]
    UnknownServer(String),
    /// A server must be pinned ([`crate::Client::pin`]) before anything but
    /// `Hello` is sent to it.
    #[error("server {0} is not saved yet; pin it first")]
    NotPinned(String),
    /// Saved server `saved` seems to answer with another key, `now`:
    /// its key was replaced, or this is a different server. It is said
    /// when `now` answers with `saved`'s pinned server id; for a saved
    /// server with no server id pinned yet, when `now` is heard under its
    /// name. Show both fingerprints; [`crate::Client::trust_new_identity`]
    /// if the user says so, which keeps the server id.
    #[error("server {saved} answered with another key, {now}")]
    IdentityChanged { saved: String, now: String },
    /// Saved server `endpoint_id` answered under its pinned key with
    /// another server id, `now`, than the one pinned, `saved`. A
    /// server never changes its id, and its app passkeys are bound to it,
    /// so this is refused, every time, and nothing is kept: unlike
    /// [`SdkError::IdentityChanged`] it cannot be accepted in place. Tell
    /// the user the server is not the one saved; [`crate::Client::forget`]
    /// it and add it again if they say it is theirs.
    #[error("server {endpoint_id} answered with server id {now}, not its own, {saved}")]
    ServerIdChanged {
        endpoint_id: String,
        saved: String,
        now: String,
    },
    /// The server and this SDK speak different revisions of the protocol,
    /// so they cannot talk: it refused this SDK's `Hello`, or answered it
    /// with another revision. `server_protocol` is the server's revision
    /// when it said it, `None` when it only refused: compare it with
    /// [`spoolframe_protocol::PROTOCOL_VERSION`] to tell whether the app
    /// (lower) or the server (higher) is the older one. Show "Update the
    /// app" (or the server); dialling again does not help, so the SDK
    /// stops reconnecting ([`crate::StateWatch::stopped`]).
    #[error("the server speaks another protocol revision; update the app or the server")]
    UpdateRequired { server_protocol: Option<u32> },
    /// The endpoint answered as another server than the one dialled.
    #[error("expected server {expected}, but {actual} answered")]
    WrongServer { expected: String, actual: String },
    /// The session is gone (revoked, expired, user disabled) and was
    /// forgotten; the user has to sign in again. Their profile stays in
    /// [`crate::Client::saved_profiles`]. It comes from the server's
    /// answer, or from the server closing the connection with
    /// [`spoolframe_protocol::close::SIGNED_OUT`]: then a call cut off by
    /// the close gets it, and every later call in a session of that
    /// server, without dialling ([`crate::Client::signed_out`]). Do not
    /// retry it; show "Who's watching" or the sign-in.
    #[error("sign in again")]
    SignInRequired,
    #[error("the username or password is wrong")]
    InvalidCredentials,
    /// The text is not a Spoolframe invite link: checked before
    /// anything is sent. Say so under the paste field.
    #[error("this isn't a Spoolframe invite")]
    NotAnInvite,
    /// The text is not an invite's code: 12 letters and digits, typed
    /// with any case, dashes or spaces ([`crate::parse_code`]). Checked
    /// before anything is sent. Say so under the code field.
    #[error("this isn't an invite code")]
    NotAnInviteCode,
    /// The invite's code is wrong, or its invite was revoked. Offer "Save
    /// server only" ([`crate::Client::save_invite_server`]).
    #[error("this invite doesn't work: its code is wrong or was revoked")]
    InviteInvalid,
    /// The invite expired. Offer "Save server only".
    #[error("this invite has expired")]
    InviteExpired,
    /// Every account the invite could make is made. Offer "Save server
    /// only".
    #[error("this invite has been used")]
    InviteExhausted,
    /// Someone has that username: back to the form, the name selected. The
    /// invite is still unused.
    #[error("that username is taken")]
    UsernameTaken,
    #[error("too many failed sign-ins; try again in {} s", retry_after.as_secs())]
    RateLimited { retry_after: Duration },
    /// One reason per stream that this device cannot play.
    #[error("cannot play: {}", reasons.join("; "))]
    CannotPlay { reasons: Vec<String> },
    /// The server ended the playback. Through [`crate::Session`], start
    /// it again and go on from where it was. [`crate::ManagedPlayback`]
    /// does that itself, and gives this only when it may not: more than
    /// [`crate::ManagedPlayback::MAX_RESUMES`] times in
    /// [`crate::ManagedPlayback::RESUME_WINDOW`].
    #[error("the playback has expired")]
    PlaybackExpired,
    /// The server's admin stopped the playback: it is over, and
    /// [`crate::ManagedPlayback`] does not start it again. Say it ended.
    #[error("the playback was stopped on the server")]
    PlaybackStopped,
    #[error("{0} not found")]
    NotFound(String),
    /// The server is converting as many files as it may; try
    /// again later, or pick tracks that play without converting.
    #[error("the server is busy converting other files")]
    Busy,
    /// The server plays at most `max` playbacks outside its home at once,
    /// and that many are playing: say so, and let the user try
    /// again when one ends. At home this never happens.
    #[error("the server plays at most {max} playbacks outside its home at once")]
    WanStreamsFull { max: u32 },
    /// The server holds as many devices with no session as it takes, and
    /// closed this connection ([`spoolframe_protocol::close::BUSY`]).
    /// Nothing is wrong with the network or the server: say it is
    /// busy, and let the user try again in a while; the SDK does not dial
    /// again by itself. A device that is signed in is always let in, so
    /// this comes before a sign-in, or once every session here has ended.
    #[error("the server is busy; try again later")]
    ServerFull,
    /// The conversion failed, with ffmpeg's reason.
    #[error("the conversion failed: {0}")]
    PlaybackFailed(String),
    /// A seek restarted the conversion: read `epoch` from offset 0.
    #[error("the playback restarted (epoch {epoch})")]
    PlaybackRestarted { epoch: u32 },
    /// The server had ended the playback (it idled out, the server
    /// restarted, the device was gone), and [`crate::ManagedPlayback`]
    /// started it again, with the same tracks, at `at` in the item. The
    /// same `ManagedPlayback` goes on with the new handle; its
    /// [`crate::ManagedPlayback::snapshot`] is the new playback.
    ///
    /// Only bytes that cannot continue across the restart give it: a
    /// progressive reader's ([`crate::ManagedPlayback::read_chunk`]),
    /// whose new encode begins at `at`, and an HLS segment read part-way
    /// ([`crate::ManagedPlayback::read_segment`] at an offset past 0).
    /// Build a new reader from the snapshot, from offset 0, and show the
    /// player at `at`; read the segment again from offset 0. Direct play,
    /// playlists, whole segments and controls go on with no error.
    #[error("the playback was started again at {} s", at.as_secs())]
    PlaybackResumed { at: Duration },
    /// This profile is locked on this device: ask for its PIN and
    /// [`crate::Session::unlock`]. The profile's [`crate::Session::user`]
    /// from [`crate::Client::profiles`] says `locked` before anything is
    /// shown.
    #[error("this profile is locked; enter its PIN")]
    ProfileLocked,
    /// Offline, this profile's PIN cannot be checked: this device keeps no
    /// hash of it, because no PIN has unlocked it online here with SDK
    /// 0.14.1 or newer. Offer the device owner's unlock ([`crate::Downloads::unlock_offline_by_owner`]), or
    /// unlock it online.
    #[error("this profile's PIN cannot be checked offline here yet")]
    NoOfflinePin,
    /// A value the call carried is not acceptable (a PIN that is not 4 to 8
    /// digits); the message says why.
    #[error("{0}")]
    InvalidInput(String),
    /// The call carried the user's account password and they have none:
    /// they sign in with a passkey or SSO only
    /// ([`crate::PinProof::Password`]). Nothing was counted as a failed
    /// sign-in. Offer another proof, or send them to the server's web UI
    /// ([`crate::Session::web_url`], when the server has one).
    #[error("this account has no password; use the PIN or the web UI")]
    NoPassword,
    /// The subtitle is still being read as text: ask again in a
    /// few seconds.
    #[error("the subtitle is still being read ({done_percent}% done)")]
    SubtitleNotReady { done_percent: u8 },
    /// The item's frame previews are being made: show the time
    /// alone, and ask again in a few seconds.
    #[error("the previews are being made ({done_percent}% done)")]
    PreviewsNotReady { done_percent: u8 },
    /// The item has no frame previews, and will not have: show the time
    /// alone, and do not ask again for it.
    #[error("this item has no previews")]
    NoPreviews,
    /// The subtitle was written again or replaced on the server while it
    /// was read, and again while it was read a second time. No
    /// part of it is returned: read it again.
    #[error("the subtitle changed while it was read")]
    SubtitleChanged,
    /// The server has no library yet: it is to be set up in its
    /// web UI. Show "This server is not set up yet" where the library
    /// would be; signing in works.
    #[error("this server is not set up yet")]
    NotSetUp,
    /// The session signed in too long ago for this: approving a device,
    /// adding or removing a passkey, no longer asking for the PIN. Confirm it is the user ([`crate::Session::confirm_password`]
    /// or [`crate::Session::begin_confirm_passkey`]), or sign in again,
    /// then ask again.
    #[error("sign in again first")]
    RecentSignInRequired,
    /// The passkey is the user's last way to sign in, and is kept
    /// ([`crate::Session::remove_passkey`]): add another passkey, or a
    /// password in the web UI, first.
    #[error("that is the last way to sign in to this account")]
    LastSignInMethod,
    /// The device sign-in was refused on the approving device.
    #[error("the sign-in was denied")]
    SignInDenied,
    /// The device sign-in is over (it timed out): begin another.
    #[error("the sign-in code has expired")]
    SignInExpired,
    /// The recovery phrase is not one: back to the entry, the
    /// reason under it.
    #[error(transparent)]
    Phrase(#[from] PhraseError),
    /// This device holds no portable identity with that id.
    #[error("no portable identity {0} on this device")]
    UnknownIdentity(String),
    /// A device credential that does not hold: not signed by the
    /// identity it names, out of date, or with a proof that fails.
    #[error(transparent)]
    Credential(#[from] CredentialError),
    /// An identity state that is not one.
    #[error(transparent)]
    IdentityState(#[from] StateError),
    /// The credential is for a device key this device does not hold for
    /// that identity: another device's.
    #[error("the credential is for another device")]
    NotThisDevice,
    /// The newest identity state this device has revokes the device key:
    /// it no longer acts as that identity.
    #[error("this device's key for the identity is revoked")]
    DeviceKeyRevoked,
    /// The security key (or the phrase root) asked to act is not one of
    /// the identity's roots now: never added, or removed. For a
    /// root action with a key, a key that holds none of the identity's
    /// credentials is this, not [`SecurityKeyError::NoCredential`]; so is
    /// [`Client::recover_with_key`]'s key that is not among the roots of
    /// the identity its credential names, and [`Portable::remove_root`]'s
    /// root that is not one now. A record, state or credential that came
    /// from elsewhere and is not signed by one of its identity's roots is
    /// [`Self::IdentityState`] or [`Self::Credential`] (`BadSignature`),
    /// never this: an app may say "this key isn't one of theirs".
    ///
    /// [`SecurityKeyError::NoCredential`]: crate::portable::SecurityKeyError::NoCredential
    /// [`Client::recover_with_key`]: crate::Client::recover_with_key
    /// [`Portable::remove_root`]: crate::Portable::remove_root
    #[error("that key is not one of the identity's roots")]
    NotARoot,
    /// Turning on the sync of a phrase identity's root was refused
    /// ([`Portable::enable_root_sync`]): which way, so the app says why.
    /// A phrase root the identity removed is [`Self::NotARoot`] instead;
    /// no root here, [`Self::UnknownIdentity`].
    ///
    /// [`Portable::enable_root_sync`]: crate::Portable::enable_root_sync
    #[error(transparent)]
    RootSync(#[from] crate::portable::RootSyncRefused),
    /// A security key did not do what was asked; which way, so
    /// the app says the right thing.
    #[error(transparent)]
    SecurityKey(#[from] crate::portable::SecurityKeyError),
    /// The tie's challenge is used or over: tie again from the
    /// start ([`crate::Session::tie`]).
    #[error("the tie took too long; try again")]
    TieChallengeExpired,
    /// Another account on the server is tied to that identity:
    /// one identity, one account per server. Sever that tie first.
    #[error("that identity is tied to another account on this server")]
    PortableIdTaken,
    /// The invite's code is right and of the other kind: a guest
    /// invite redeemed for an account ([`crate::Client::redeem_invite`]),
    /// or an account's redeemed as a guest
    /// ([`crate::Client::redeem_invite_portable`]). Nothing was used up:
    /// redeem it the other way ([`crate::InviteInfo::kind`] says which).
    #[error("this invite is of another kind")]
    WrongInviteKind,
    /// Signed in as a portable user, who has no account on this
    /// server: its password, PIN, passkeys, devices and sessions are an
    /// account's. Hide what asks for them.
    #[error("not available to a guest from another identity")]
    NotForPortableUsers,
    /// Only an admin may do this (making and revoking invites, for one), and
    /// the session's user is not one. Hide what asks for it.
    #[error("only an admin can do that")]
    AdminRequired,
    /// The session was signed in with a one-time password an admin reset
    /// to: it may only change the password ([`crate::Session::change_password`])
    /// or sign out. Show "Choose a new password".
    #[error("choose a new password first")]
    PasswordChangeRequired,
    /// [`crate::Session::leave_server`] from the last enabled admin, or
    /// the last enabled admin with a password: make someone else
    /// an admin, or give one a password, first. Nothing changed, and the
    /// server is still saved here.
    #[error("you are the only admin; make someone else an admin first")]
    LastAdmin,
    /// An experimental feature this call needs is not on for the
    /// session: the app did not ask for it
    /// ([`crate::ClientOptions::experimental`]), or the server did not
    /// turn it on ([`crate::Session::experimental`]). Nothing was sent.
    /// Hide what needs it.
    #[error("{feature} is not on for this session")]
    Unsupported { feature: String },
    /// The user sees the playlist and may not make this change:
    /// an edit on a read-only share, or a rename, sharing change or
    /// delete by anyone but its owner (or an admin, for a server-wide
    /// one).
    #[error("you cannot change this playlist")]
    PlaylistReadOnly,
    /// The playlist does not take items of this kind. Offer "Add
    /// to playlist" only for playlists whose kinds take the item.
    #[error("this playlist does not take a {kind:?}")]
    KindNotAllowed {
        kind: spoolframe_protocol::playlists::ItemKind,
    },
    /// Someone changed the playlist's same entries since the revision the
    /// edit was made on: "Someone changed this playlist; reload".
    /// `revision` is its revision now. Nothing changed.
    #[error("someone changed this playlist; reload")]
    PlaylistConflict { revision: u64 },
    /// Not the user's to do: a playlist's cover set or taken away
    /// by someone who may not edit it, or on Liked or All. Nothing
    /// changed.
    #[error("you are not allowed to do that")]
    NotAllowed,
    /// A cover over `max_bytes`, or over `max_pixels` by its header:
    /// not decoded. Nothing changed.
    #[error("the picture is too large: at most {max_bytes} bytes and {max_pixels} pixels")]
    ImageTooLarge { max_bytes: u64, max_pixels: u64 },
    /// Not a JPEG, PNG, WebP or HEIC picture the server can decode.
    /// Nothing changed.
    #[error("the picture cannot be read")]
    ImageUnreadable,
    /// The subtitle downloads of `whose` are used up for today:
    /// the server's account at subtitle site `site` (`opensubtitles`,
    /// `subdl`, as `SubtitleResult::source`), or the user's own daily
    /// limit (no `site`). `resets_at` (Unix seconds) is when more may be
    /// downloaded, when the server knows. Nothing was downloaded;
    /// searching still works.
    #[error("no subtitle downloads left today")]
    QuotaExhausted {
        whose: crate::types::QuotaOf,
        site: Option<String>,
        resets_at: Option<i64>,
    },
    /// The server has no subtitle site set up: hide "Find
    /// subtitles". Nothing was asked of any site.
    #[error("this server has no subtitle source set up")]
    NoSubtitleSource,
    /// Pairing this device with a portable identity, or a phone with a
    /// device, did not happen; [`PairingError`] says why.
    #[error(transparent)]
    Pairing(#[from] PairingError),
    /// A download's file was read before the server finished it:
    /// the download manager waits and asks again.
    #[error("the download is not ready")]
    DownloadNotReady,
    /// The downloads directory ([`crate::ClientOptions::downloads_dir`])
    /// could not be read or written, or none was set.
    #[error("downloads: {0}")]
    Downloads(String),
    /// Experimental (`federation`): the other server did not pair
    /// ([`crate::Session::pair_server`]), for `reason`. Nothing was kept.
    #[error("the other server did not pair: {reason}")]
    PairingRefused {
        reason: spoolframe_protocol::federation::PairingRefused,
    },
    /// Experimental (`federation`): the paired server a `fed:` item
    /// is from is offline to the user's server: it cannot be played now.
    #[error("the paired server the item is from is offline")]
    OriginOffline,
    /// Any other error the server answered with.
    #[error("server: {0}")]
    Server(String),
    /// The server's answer made no sense to this build.
    #[error("protocol: {0}")]
    Protocol(String),
}

impl SdkError {
    /// The connection failed under a request: it was not answered in
    /// time, or its stream broke. The connection may be dead.
    pub(crate) fn is_transport(&self) -> bool {
        matches!(self, SdkError::TimedOut(_) | SdkError::Unreachable(_))
    }

    /// The error for a server's answer. `Unauthenticated` is
    /// [`SdkError::SignInRequired`]; the caller forgets the session.
    pub(crate) fn from_server(error: ServerError) -> Self {
        match error {
            ServerError::ServerRestarted => Self::ServerRestarted,
            ServerError::OperationExpired => Self::OperationExpired,
            ServerError::OperationConflict => Self::OperationConflict,
            ServerError::Unavailable { message } => SdkError::Unavailable(message),
            ServerError::StaleProgress => SdkError::StaleProgress,
            ServerError::RevisionConflict { revision } => SdkError::RevisionConflict { revision },
            ServerError::Unauthenticated => SdkError::SignInRequired,
            ServerError::InvalidCredentials => SdkError::InvalidCredentials,
            ServerError::RateLimited { retry_after_ms } => SdkError::RateLimited {
                retry_after: Duration::from_millis(retry_after_ms),
            },
            ServerError::CannotPlay { reasons } => SdkError::CannotPlay { reasons },
            ServerError::PlaybackExpired => SdkError::PlaybackExpired,
            ServerError::PlaybackStopped => SdkError::PlaybackStopped,
            ServerError::NotFound { what } => SdkError::NotFound(what),
            ServerError::Busy => SdkError::Busy,
            ServerError::WanStreamsFull { max } => SdkError::WanStreamsFull { max },
            ServerError::PlaybackFailed { reason } => SdkError::PlaybackFailed(reason),
            ServerError::PlaybackRestarted { epoch } => SdkError::PlaybackRestarted { epoch },
            ServerError::ProfileLocked => SdkError::ProfileLocked,
            ServerError::InvalidInput { message } => SdkError::InvalidInput(message),
            ServerError::NoPassword => SdkError::NoPassword,
            ServerError::SubtitleNotReady { done_percent } => {
                SdkError::SubtitleNotReady { done_percent }
            }
            ServerError::SubtitleChanged => SdkError::SubtitleChanged,
            ServerError::PreviewsNotReady { done_percent } => {
                SdkError::PreviewsNotReady { done_percent }
            }
            ServerError::NoPreviews => SdkError::NoPreviews,
            ServerError::DeviceSignInDenied => SdkError::SignInDenied,
            ServerError::DeviceSignInExpired => SdkError::SignInExpired,
            ServerError::NotSetUp => SdkError::NotSetUp,
            ServerError::RecentSignInRequired => SdkError::RecentSignInRequired,
            ServerError::LastSignInMethod => SdkError::LastSignInMethod,
            ServerError::InviteInvalid => SdkError::InviteInvalid,
            ServerError::InviteExpired => SdkError::InviteExpired,
            ServerError::InviteExhausted => SdkError::InviteExhausted,
            ServerError::UsernameTaken => SdkError::UsernameTaken,
            ServerError::TieChallengeExpired => SdkError::TieChallengeExpired,
            ServerError::PortableIdTaken => SdkError::PortableIdTaken,
            ServerError::WrongInviteKind => SdkError::WrongInviteKind,
            ServerError::NotForPortableUsers => SdkError::NotForPortableUsers,
            ServerError::AdminRequired => SdkError::AdminRequired,
            ServerError::PasswordChangeRequired => SdkError::PasswordChangeRequired,
            ServerError::LastAdmin => SdkError::LastAdmin,
            ServerError::PlaylistReadOnly => SdkError::PlaylistReadOnly,
            ServerError::KindNotAllowed { kind } => SdkError::KindNotAllowed { kind },
            ServerError::PlaylistConflict { revision } => SdkError::PlaylistConflict { revision },
            ServerError::NotAllowed => SdkError::NotAllowed,
            ServerError::ImageTooLarge {
                max_bytes,
                max_pixels,
            } => SdkError::ImageTooLarge {
                max_bytes,
                max_pixels,
            },
            ServerError::ImageUnreadable => SdkError::ImageUnreadable,
            ServerError::QuotaExhausted {
                whose,
                resets_at,
                site,
            } => SdkError::QuotaExhausted {
                whose,
                site,
                resets_at,
            },
            ServerError::NoSubtitleSource => SdkError::NoSubtitleSource,
            ServerError::DownloadNotReady => SdkError::DownloadNotReady,
            ServerError::PairingRefused { reason } => SdkError::PairingRefused { reason },
            ServerError::OriginOffline => SdkError::OriginOffline,
            other => SdkError::Server(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PX-02's refusals reach an app with their fields, not as a
    /// server message to match on.
    #[test]
    fn a_used_up_subtitle_quota_keeps_whose_where_and_when() {
        use crate::types::QuotaOf;
        assert_eq!(
            SdkError::from_server(ServerError::QuotaExhausted {
                whose: QuotaOf::Account,
                resets_at: Some(1_760_000_000),
                site: Some("subdl".into()),
            }),
            SdkError::QuotaExhausted {
                whose: QuotaOf::Account,
                site: Some("subdl".into()),
                resets_at: Some(1_760_000_000),
            }
        );
        assert_eq!(
            SdkError::from_server(ServerError::QuotaExhausted {
                whose: QuotaOf::User,
                resets_at: None,
                site: None,
            }),
            SdkError::QuotaExhausted {
                whose: QuotaOf::User,
                site: None,
                resets_at: None,
            }
        );
    }

    #[test]
    fn a_server_without_a_subtitle_site_is_told_apart() {
        assert_eq!(
            SdkError::from_server(ServerError::NoSubtitleSource),
            SdkError::NoSubtitleSource
        );
    }

    #[test]
    fn a_session_signed_in_too_long_ago_is_told_apart() {
        assert_eq!(
            SdkError::from_server(ServerError::RecentSignInRequired),
            SdkError::RecentSignInRequired
        );
    }

    #[test]
    fn a_playback_stopped_on_the_server_is_told_apart_from_an_expired_one() {
        assert_eq!(
            SdkError::from_server(ServerError::PlaybackStopped),
            SdkError::PlaybackStopped
        );
    }

    #[test]
    fn the_last_way_to_sign_in_is_told_apart() {
        assert_eq!(
            SdkError::from_server(ServerError::LastSignInMethod),
            SdkError::LastSignInMethod
        );
    }

    #[test]
    fn the_tie_errors_are_told_apart() {
        assert_eq!(
            SdkError::from_server(ServerError::TieChallengeExpired),
            SdkError::TieChallengeExpired
        );
        assert_eq!(
            SdkError::from_server(ServerError::PortableIdTaken),
            SdkError::PortableIdTaken
        );
    }

    #[test]
    fn the_last_admin_is_told_why_they_cannot_leave() {
        assert_eq!(
            SdkError::from_server(ServerError::LastAdmin),
            SdkError::LastAdmin
        );
    }

    #[test]
    fn a_one_time_password_session_is_told_to_change_it() {
        assert_eq!(
            SdkError::from_server(ServerError::PasswordChangeRequired),
            SdkError::PasswordChangeRequired
        );
    }

    #[test]
    fn portable_sign_in_errors_are_told_apart() {
        assert_eq!(
            SdkError::from_server(ServerError::WrongInviteKind),
            SdkError::WrongInviteKind
        );
        assert_eq!(
            SdkError::from_server(ServerError::NotForPortableUsers),
            SdkError::NotForPortableUsers
        );
    }

    #[test]
    fn an_admin_request_by_someone_else_is_told_apart() {
        assert_eq!(
            SdkError::from_server(ServerError::AdminRequired),
            SdkError::AdminRequired
        );
    }
}
