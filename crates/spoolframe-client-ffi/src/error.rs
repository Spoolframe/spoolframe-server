//! [`SdkError`] as Swift and Kotlin see it: the same cases, variant by
//! variant, each with its fields typed. Nothing is said as a string that
//! the SDK says as a value; `message()` is the SDK's own text, to show.
//!
//! [`SpoolError::kind`] is [`SdkError::kind`]: what the app does about it
//! (try again, ask for the PIN, sign in), whichever case it is.

use std::time::Duration;

use spoolframe_client_sdk::types::QuotaOf;
use spoolframe_client_sdk::{
    self as sdk, CredentialError, ErrorKind, ItemKind, PairingRefused, RootSyncRefused, SdkError,
    SecurityKeyError, StateError,
};

/// Why a call failed. The cases are [`SdkError`]'s, with the same names,
/// and this layer's own: [`SpoolError::Cancelled`],
/// [`SpoolError::Replanned`], and `Bridge` with the `hls-bridge` feature.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, uniffi::Error)]
#[uniffi::export(Display)]
pub enum SpoolError {
    #[error("the operation belongs to an earlier server instance")]
    ServerRestarted,
    #[error("the operation expired; its outcome is no longer recoverable")]
    OperationExpired,
    #[error("the operation ID was already used with different arguments")]
    OperationConflict,
    /// The playback was ended here already (after `end_playback`).
    #[error("the playback is closed")]
    PlaybackClosed,
    #[error("the media changed while recovering playback")]
    PlaybackChanged,
    /// The server answered, but something it needs (its media source, a
    /// mount) is down: not the connection's fault.
    #[error("{message}")]
    Unavailable { message: String },
    #[error("an older progress report was rejected")]
    StaleProgress,
    #[error("playback revision changed to {revision}")]
    RevisionConflict { revision: u64 },
    /// The app's storage failed; shown, never worked around.
    #[error("storage: {message}")]
    Storage { message: String },
    /// `server` cannot be reached right now.
    #[error("{server} cannot be reached")]
    Unreachable { server: String },
    /// No answer in time. For a request that changes something the
    /// outcome is unknown: show the state again before offering to repeat
    /// it.
    #[error("no answer in time: {message}")]
    TimedOut { message: String },
    #[error("no server {server} is known")]
    UnknownServer { server: String },
    /// Pin the server (after its fingerprint) before using it.
    #[error("server {server} is not saved yet; pin it first")]
    NotPinned { server: String },
    /// A saved server answered with another key: show both fingerprints,
    /// and `trust_new_identity(saved, now)` if the user says so.
    /// `server_id` is the saved server's pinned server id, to say which
    /// server it is.
    #[error("server {saved} answered with another key, {now}")]
    IdentityChanged {
        saved: String,
        now: String,
        server_id: Option<String>,
    },
    /// A saved server's key answered with another server id: not the same
    /// server. Refused, with no accepting it: remove the server, and add
    /// it again if the user says it's theirs.
    #[error("server {endpoint_id} answered with server id {now}, not its own, {saved}")]
    ServerIdChanged {
        endpoint_id: String,
        saved: String,
        now: String,
    },
    /// The server and this app speak different protocol revisions.
    /// `server_protocol` is the server's, when it said it: lower than the
    /// app's is [`ErrorKind::ServerTooOld`].
    #[error("the server speaks another protocol revision; update the app or the server")]
    UpdateRequired { server_protocol: Option<u32> },
    #[error("expected server {expected}, but {actual} answered")]
    WrongServer { expected: String, actual: String },
    /// The session is gone and forgotten: sign in again.
    #[error("sign in again")]
    SignInRequired,
    #[error("the username or password is wrong")]
    InvalidCredentials,
    /// What was pasted or opened isn't an invite link.
    #[error("this isn't a Spoolframe invite")]
    NotAnInvite,
    /// What was typed isn't an invite's code (12 letters and digits);
    /// nothing was sent.
    #[error("this isn't an invite code")]
    NotAnInviteCode,
    #[error("this invite doesn't work: its code is wrong or was revoked")]
    InviteInvalid,
    #[error("this invite has expired")]
    InviteExpired,
    #[error("this invite has been used")]
    InviteExhausted,
    #[error("that username is taken")]
    UsernameTaken,
    #[error("too many failed sign-ins; try again in {} s", retry_after.as_secs())]
    RateLimited { retry_after: Duration },
    #[error("cannot play: {}", reasons.join("; "))]
    CannotPlay { reasons: Vec<String> },
    #[error("the playback has expired")]
    PlaybackExpired,
    /// The server's admin stopped it: it's over, not to be started again.
    #[error("the playback was stopped on the server")]
    PlaybackStopped,
    #[error("{what} not found")]
    NotFound { what: String },
    #[error("the server is busy converting other files")]
    Busy,
    /// The server plays at most `max` playbacks outside its home at once,
    /// and that many are playing: try again when one ends.
    #[error("the server plays at most {max} playbacks outside its home at once")]
    WanStreamsFull { max: u32 },
    /// The server holds too many devices that aren't signed in: try
    /// again later, by hand.
    #[error("the server is busy; try again later")]
    ServerFull,
    #[error("the conversion failed: {reason}")]
    PlaybackFailed { reason: String },
    #[error("the playback restarted (epoch {epoch})")]
    PlaybackRestarted { epoch: u32 },
    /// The SDK started the playback again at `at`; the bridge reads the
    /// segment again.
    #[error("the playback was started again at {} s", at.as_secs())]
    PlaybackResumed { at: Duration },
    /// Ask for the profile's PIN and `unlock`.
    #[error("this profile is locked; enter its PIN")]
    ProfileLocked,
    /// Offline, the profile's PIN can't be checked: no PIN has unlocked
    /// it online on this device yet. Offer the owner's unlock, or unlock
    /// online.
    #[error("this profile's PIN cannot be checked offline here yet")]
    NoOfflinePin,
    #[error("{message}")]
    InvalidInput { message: String },
    /// The account has no password (passkeys or a sign-in provider
    /// only): offer another way, or the server's web page.
    #[error("this account has no password; use the PIN or the web UI")]
    NoPassword,
    #[error("the subtitle is still being read ({done_percent}% done)")]
    SubtitleNotReady { done_percent: u8 },
    /// The item's frame previews are being made: ask again in a few
    /// seconds.
    #[error("the previews are being made ({done_percent}% done)")]
    PreviewsNotReady { done_percent: u8 },
    /// The item has no frame previews and won't have: don't ask again.
    #[error("this item has no previews")]
    NoPreviews,
    /// Rewritten while read, twice: read it again.
    #[error("the subtitle changed while it was read")]
    SubtitleChanged,
    /// The server has no library yet: it's set up in its web UI. Signing
    /// in works.
    #[error("this server is not set up yet")]
    NotSetUp,
    /// The sign-in is too old for this: confirm, then ask again.
    #[error("sign in again first")]
    RecentSignInRequired,
    /// Removing it would leave the account no way to sign in.
    #[error("that is the last way to sign in to this account")]
    LastSignInMethod,
    /// The device's sign-in was refused on the phone.
    #[error("the sign-in was denied")]
    SignInDenied,
    /// The device's sign-in code ran out or was used: start again.
    #[error("the sign-in code has expired")]
    SignInExpired,
    /// The recovery phrase isn't one: the reason goes under the entry.
    #[error("{error}")]
    Phrase { error: PhraseFailure },
    #[error("no portable identity {id} on this device")]
    UnknownIdentity { id: String },
    /// A device credential that doesn't hold.
    #[error("{error}")]
    Credential { error: CredentialError },
    /// An identity state that isn't one.
    #[error("{error}")]
    IdentityState { error: StateError },
    #[error("the credential is for another device")]
    NotThisDevice,
    #[error("this device's key for the identity is revoked")]
    DeviceKeyRevoked,
    /// That key or the phrase isn't one of the identity's roots now.
    #[error("that key is not one of the identity's roots")]
    NotARoot,
    /// Root sync refused, and which way.
    #[error("{refused}")]
    RootSync { refused: RootSyncRefused },
    /// A security key failed or said no, and which way. A cancel is
    /// `SecurityKeyError::Cancelled`.
    #[error("{error}")]
    SecurityKey { error: SecurityKeyError },
    /// The tie's challenge ran out: try again.
    #[error("the tie took too long; try again")]
    TieChallengeExpired,
    #[error("that identity is tied to another account on this server")]
    PortableIdTaken,
    /// An account invite redeemed as an identity, or the other way round.
    #[error("this invite is of another kind")]
    WrongInviteKind,
    /// An account-only thing asked of a guest (portable) session.
    #[error("not available to a guest from another identity")]
    NotForPortableUsers,
    #[error("only an admin can do that")]
    AdminRequired,
    /// Signed in with a one-time password: until a new one is chosen
    /// (`change_password`), every other request is refused.
    #[error("choose a new password first")]
    PasswordChangeRequired,
    /// The user is the server's last enabled admin: it won't let them
    /// leave until someone else is an admin.
    #[error("you are the only admin; make someone else an admin first")]
    LastAdmin,
    /// An experimental feature isn't on for the session: hide what needs
    /// it.
    #[error("{feature} is not on for this session")]
    Unsupported { feature: String },
    /// The server doesn't know `request`: it's older than this app.
    #[error("this server does not know the request {request:?}")]
    NotOnServer { request: String },
    /// `feature` isn't on for this connection: the server doesn't have it.
    /// Nothing was sent; hide what needs it.
    #[error("{feature} is not on for this connection")]
    FeatureOff { feature: sdk::Feature },
    #[error("you cannot change this playlist")]
    PlaylistReadOnly,
    /// The playlist doesn't take items of `kind`.
    #[error("this playlist does not take a {kind:?}")]
    KindNotAllowed { kind: ItemKind },
    /// Someone changed the playlist since `revision` was read: reload.
    #[error("someone changed this playlist; reload")]
    PlaylistConflict { revision: u64 },
    #[error("you are not allowed to do that")]
    NotAllowed,
    #[error("the picture is too large: at most {max_bytes} bytes and {max_pixels} pixels")]
    ImageTooLarge { max_bytes: u64, max_pixels: u64 },
    #[error("the picture cannot be read")]
    ImageUnreadable,
    /// No subtitle downloads left today: `whose` (the user's own limit,
    /// or the server's account at `site`). `resets_at`, Unix seconds.
    #[error("no subtitle downloads left today")]
    QuotaExhausted {
        whose: QuotaOf,
        site: Option<String>,
        resets_at: Option<i64>,
    },
    /// The server has no subtitle site set up: no Find Subtitles.
    #[error("this server has no subtitle source set up")]
    NoSubtitleSource,
    /// Pairing didn't happen, and which way.
    #[error("{error}")]
    Pairing { error: PairingFailure },
    #[error("the download is not ready")]
    DownloadNotReady,
    /// The downloads directory failed (full, unreadable), or there is
    /// none.
    #[error("downloads: {message}")]
    Downloads { message: String },
    /// The other server didn't pair, for `reason`.
    #[error("the other server did not pair: {reason}")]
    PairingRefused { reason: PairingRefused },
    /// The paired server a `fed:` item is from is offline to the user's.
    #[error("the paired server the item is from is offline")]
    OriginOffline,
    #[error("server: {message}")]
    Server { message: String },
    #[error("protocol: {message}")]
    Protocol { message: String },
    /// The app stopped the call (`PendingSignIn::cancel`,
    /// `DownloadRun::cancel`).
    #[error("cancelled")]
    Cancelled,
    /// A playback's resource asked after it was planned again: follow
    /// the new plan (`follow_replan`) rather than read the old one's.
    #[error("the playback was planned again: follow the new plan")]
    Replanned,
    /// The loopback bridge the player reads from couldn't start.
    #[cfg(feature = "hls-bridge")]
    #[error("the player bridge can't start: {message}")]
    Bridge { message: String },
}

/// Why a recovery phrase isn't one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, uniffi::Enum)]
pub enum PhraseFailure {
    /// A phrase is 24 words; this is `found`.
    #[error("a recovery phrase is 24 words; this is {found}")]
    WordCount { found: u32 },
    /// Word `position` (from 1) isn't a recovery word.
    #[error("word {position} is not a recovery phrase word")]
    UnknownWord { position: u32 },
    /// Every word is one, but they don't check out.
    #[error("the words do not check out; one is wrong or out of place")]
    Checksum,
}

/// Why pairing a device didn't happen.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, uniffi::Enum)]
pub enum PairingFailure {
    /// The device doesn't take guests: turned off in its settings.
    #[error("this device does not allow sign-ins from other people's accounts")]
    NotAllowed,
    /// What was scanned isn't a pairing code.
    #[error("{message}")]
    NotALink { message: String },
    #[error("this device is already showing a pairing code")]
    AlreadyPairing,
    /// This phone doesn't hold the identity's root: recover it first.
    #[error("this device does not hold that identity's root")]
    NoRoot,
    #[error("the pairing code does not match")]
    WrongCode,
    #[error("the pairing code has expired")]
    Expired,
    #[error("the pairing was declined on the phone")]
    Declined,
    #[error("the device did not accept the credential")]
    CredentialRefused,
    #[error("the device could not finish pairing")]
    Failed,
    /// Cut off part way.
    #[error("the pairing was cut off: {message}")]
    Aborted { message: String },
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

impl From<sdk::PhraseError> for PhraseFailure {
    fn from(error: sdk::PhraseError) -> Self {
        match error {
            sdk::PhraseError::WordCount { found } => Self::WordCount {
                found: count(found),
            },
            sdk::PhraseError::UnknownWord { position } => Self::UnknownWord {
                position: count(position),
            },
            sdk::PhraseError::Checksum => Self::Checksum,
        }
    }
}

impl From<sdk::PairingError> for PairingFailure {
    fn from(error: sdk::PairingError) -> Self {
        match error {
            sdk::PairingError::NotAllowed => Self::NotAllowed,
            sdk::PairingError::NotALink(e) => Self::NotALink {
                message: e.to_string(),
            },
            sdk::PairingError::AlreadyPairing => Self::AlreadyPairing,
            sdk::PairingError::NoRoot => Self::NoRoot,
            sdk::PairingError::WrongCode => Self::WrongCode,
            sdk::PairingError::Expired => Self::Expired,
            sdk::PairingError::Declined => Self::Declined,
            sdk::PairingError::CredentialRefused => Self::CredentialRefused,
            sdk::PairingError::Failed => Self::Failed,
            sdk::PairingError::Aborted(message) => Self::Aborted { message },
        }
    }
}

impl From<SdkError> for SpoolError {
    fn from(error: SdkError) -> Self {
        match error {
            SdkError::ServerRestarted => Self::ServerRestarted,
            SdkError::OperationExpired => Self::OperationExpired,
            SdkError::OperationConflict => Self::OperationConflict,
            SdkError::PlaybackClosed => Self::PlaybackClosed,
            SdkError::PlaybackChanged => Self::PlaybackChanged,
            SdkError::Unavailable(message) => Self::Unavailable { message },
            SdkError::StaleProgress => Self::StaleProgress,
            SdkError::RevisionConflict { revision } => Self::RevisionConflict { revision },
            SdkError::Storage(e) => Self::Storage { message: e.0 },
            SdkError::Unreachable(server) => Self::Unreachable { server },
            SdkError::TimedOut(message) => Self::TimedOut { message },
            SdkError::UnknownServer(server) => Self::UnknownServer { server },
            SdkError::NotPinned(server) => Self::NotPinned { server },
            SdkError::IdentityChanged {
                saved,
                now,
                server_id,
            } => Self::IdentityChanged {
                saved,
                now,
                server_id,
            },
            SdkError::ServerIdChanged {
                endpoint_id,
                saved,
                now,
            } => Self::ServerIdChanged {
                endpoint_id,
                saved,
                now,
            },
            SdkError::UpdateRequired { server_protocol } => {
                Self::UpdateRequired { server_protocol }
            }
            SdkError::WrongServer { expected, actual } => Self::WrongServer { expected, actual },
            SdkError::SignInRequired => Self::SignInRequired,
            SdkError::InvalidCredentials => Self::InvalidCredentials,
            SdkError::NotAnInvite => Self::NotAnInvite,
            SdkError::NotAnInviteCode => Self::NotAnInviteCode,
            SdkError::InviteInvalid => Self::InviteInvalid,
            SdkError::InviteExpired => Self::InviteExpired,
            SdkError::InviteExhausted => Self::InviteExhausted,
            SdkError::UsernameTaken => Self::UsernameTaken,
            SdkError::RateLimited { retry_after } => Self::RateLimited { retry_after },
            SdkError::CannotPlay { reasons } => Self::CannotPlay { reasons },
            SdkError::PlaybackExpired => Self::PlaybackExpired,
            SdkError::PlaybackStopped => Self::PlaybackStopped,
            SdkError::NotFound(what) => Self::NotFound { what },
            SdkError::Busy => Self::Busy,
            SdkError::WanStreamsFull { max } => Self::WanStreamsFull { max },
            SdkError::ServerFull => Self::ServerFull,
            SdkError::PlaybackFailed(reason) => Self::PlaybackFailed { reason },
            SdkError::PlaybackRestarted { epoch } => Self::PlaybackRestarted { epoch },
            SdkError::PlaybackResumed { at } => Self::PlaybackResumed { at },
            SdkError::ProfileLocked => Self::ProfileLocked,
            SdkError::NoOfflinePin => Self::NoOfflinePin,
            SdkError::InvalidInput(message) => Self::InvalidInput { message },
            SdkError::NoPassword => Self::NoPassword,
            SdkError::SubtitleNotReady { done_percent } => Self::SubtitleNotReady { done_percent },
            SdkError::PreviewsNotReady { done_percent } => Self::PreviewsNotReady { done_percent },
            SdkError::NoPreviews => Self::NoPreviews,
            SdkError::SubtitleChanged => Self::SubtitleChanged,
            SdkError::NotSetUp => Self::NotSetUp,
            SdkError::RecentSignInRequired => Self::RecentSignInRequired,
            SdkError::LastSignInMethod => Self::LastSignInMethod,
            SdkError::SignInDenied => Self::SignInDenied,
            SdkError::SignInExpired => Self::SignInExpired,
            SdkError::Phrase(e) => Self::Phrase { error: e.into() },
            SdkError::UnknownIdentity(id) => Self::UnknownIdentity { id },
            SdkError::Credential(error) => Self::Credential { error },
            SdkError::IdentityState(error) => Self::IdentityState { error },
            SdkError::NotThisDevice => Self::NotThisDevice,
            SdkError::DeviceKeyRevoked => Self::DeviceKeyRevoked,
            SdkError::NotARoot => Self::NotARoot,
            SdkError::RootSync(refused) => Self::RootSync { refused },
            SdkError::SecurityKey(error) => Self::SecurityKey { error },
            SdkError::TieChallengeExpired => Self::TieChallengeExpired,
            SdkError::PortableIdTaken => Self::PortableIdTaken,
            SdkError::WrongInviteKind => Self::WrongInviteKind,
            SdkError::NotForPortableUsers => Self::NotForPortableUsers,
            SdkError::AdminRequired => Self::AdminRequired,
            SdkError::PasswordChangeRequired => Self::PasswordChangeRequired,
            SdkError::LastAdmin => Self::LastAdmin,
            SdkError::Unsupported { feature } => Self::Unsupported { feature },
            SdkError::NotOnServer { request } => Self::NotOnServer { request },
            SdkError::FeatureOff { feature } => Self::FeatureOff { feature },
            SdkError::PlaylistReadOnly => Self::PlaylistReadOnly,
            SdkError::KindNotAllowed { kind } => Self::KindNotAllowed { kind },
            SdkError::PlaylistConflict { revision } => Self::PlaylistConflict { revision },
            SdkError::NotAllowed => Self::NotAllowed,
            SdkError::ImageTooLarge {
                max_bytes,
                max_pixels,
            } => Self::ImageTooLarge {
                max_bytes,
                max_pixels,
            },
            SdkError::ImageUnreadable => Self::ImageUnreadable,
            SdkError::QuotaExhausted {
                whose,
                site,
                resets_at,
            } => Self::QuotaExhausted {
                whose,
                site,
                resets_at,
            },
            SdkError::NoSubtitleSource => Self::NoSubtitleSource,
            SdkError::Pairing(e) => Self::Pairing { error: e.into() },
            SdkError::DownloadNotReady => Self::DownloadNotReady,
            SdkError::Downloads(message) => Self::Downloads { message },
            SdkError::PairingRefused { reason } => Self::PairingRefused { reason },
            SdkError::OriginOffline => Self::OriginOffline,
            SdkError::Server(message) => Self::Server { message },
            SdkError::Protocol(message) => Self::Protocol { message },
        }
    }
}

#[uniffi::export]
impl SpoolError {
    /// What the app does about it: [`SdkError::kind`]. This layer's own
    /// cases are [`ErrorKind::Refused`].
    pub fn kind(&self) -> ErrorKind {
        self.classified().map_or(ErrorKind::Refused, |e| e.kind())
    }

    /// The connection failed under the request: show the state again;
    /// the next call dials.
    pub fn is_transport(&self) -> bool {
        self.kind() == ErrorKind::Transport
    }

    /// What to show: the same text as the SDK's.
    pub fn message(&self) -> String {
        self.to_string()
    }
}

impl SpoolError {
    /// The SDK's error for a case the SDK classifies by (its `kind`,
    /// `who_updates`, [`sdk::PinFailure::of`],
    /// [`sdk::InviteRefusal::of_error`]), so that every rule is the SDK's
    /// own. A case can't carry its SDK error across to Swift unseen, so
    /// this is the one way back; `None` for every other case, which they
    /// all leave unclassified.
    pub(crate) fn classified(&self) -> Option<SdkError> {
        Some(match self {
            Self::Unreachable { server } => SdkError::Unreachable(server.clone()),
            Self::TimedOut { message } => SdkError::TimedOut(message.clone()),
            Self::SignInRequired => SdkError::SignInRequired,
            Self::ProfileLocked => SdkError::ProfileLocked,
            Self::PasswordChangeRequired => SdkError::PasswordChangeRequired,
            Self::RecentSignInRequired => SdkError::RecentSignInRequired,
            Self::RateLimited { retry_after } => SdkError::RateLimited {
                retry_after: *retry_after,
            },
            Self::NotOnServer { request } => SdkError::NotOnServer {
                request: request.clone(),
            },
            Self::FeatureOff { feature } => SdkError::FeatureOff { feature: *feature },
            Self::UpdateRequired { server_protocol } => SdkError::UpdateRequired {
                server_protocol: *server_protocol,
            },
            Self::InvalidCredentials => SdkError::InvalidCredentials,
            Self::NoOfflinePin => SdkError::NoOfflinePin,
            Self::InviteInvalid => SdkError::InviteInvalid,
            Self::InviteExpired => SdkError::InviteExpired,
            Self::InviteExhausted => SdkError::InviteExhausted,
            Self::WrongInviteKind => SdkError::WrongInviteKind,
            _ => return None,
        })
    }
}

/// A value the app passed that isn't one (a malformed ID).
pub(crate) fn invalid(what: &str, e: impl std::fmt::Display) -> SpoolError {
    SpoolError::InvalidInput {
        message: format!("{what}: {e}"),
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
