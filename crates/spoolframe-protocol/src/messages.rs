//! The messages of `spoolframe/client/1`.
//!
//! Every enum is internally tagged: `{"type": "ping"}`,
//! `{"type": "item", "id": "plex:movie:1"}`. Peers ignore fields they do
//! not know, and an error or event type they do not know decodes as
//! `Unknown`, so a newer peer can add both without breaking an older one.
//! A request type the server does not know is answered with
//! [`Error::Unsupported`].

use std::fmt;

use serde::{Deserialize, Serialize};
use spoolframe_types::{
    ClientCapabilities, Item, Library, LibraryId, Marker, MediaId, MediaTracks, PersonId, Platform,
    Role, SubtitleFormat, TrackPreferences,
};

use crate::media::PlaybackHandle;
use crate::passkeys::{
    FinishPasskeyRegistration, FinishPasskeySignIn, PasskeyInfo, PasskeyRegistrationBegun,
    PasskeySignInBegun,
};
use crate::playback::{BitmapSubtitles, ChosenTracks, PlaybackPlan, Selection};
use crate::plex::PlexLink;
use crate::previews::Previews;
use crate::search::{SearchRequest, SearchResults};

/// The protocol revision inside the `spoolframe/client/1` ALPN. Raised
/// for coordinated development releases, including breaking wire changes.
pub const PROTOCOL_VERSION: u32 = 8;

/// The maximum raw body of one subtitle chunk on a resource stream.
pub const SUBTITLE_CHUNK: u64 = 512 << 10;

/// An opaque session token from a login (P1-12). It is only valid from
/// the endpoint that logged in.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

impl fmt::Debug for SessionId {
    // A session is a credential (bound to one endpoint, but still): keep it
    // out of logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionId(<redacted>)")
    }
}

/// One request, and the session it is made in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    /// `None` for the pre-auth requests. Several sessions may be used on
    /// one connection.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub session: Option<SessionId>,
    pub request: Request,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    // Pre-auth: the only requests served without a session (§20).
    /// The first request on every connection.
    Hello(Hello),
    AuthMethods,
    LoginPassword(LoginPassword),
    Ping,
    /// Explicit foreground profile selection. Background requests never select a profile.
    ActivateProfile,
    /// Last accepted progress sequence for a restored playback handle.
    PlaybackProgressSequence {
        handle: PlaybackHandle,
    },
    /// Ask to be signed in by someone approving this device on another
    /// one they are signed in on (P3-07). Show the code (and its
    /// [`crate::device_sign_in::link`] as a QR code), then
    /// `AwaitDeviceSignIn`. `DeviceSignInStarted`.
    ///
    /// `remember` asks for this device to be trusted (P4-01); the
    /// approver sees the request and decides. Never true unless the user
    /// ticked "Remember this device".
    BeginDeviceSignIn {
        remember: bool,
    },
    /// Wait, up to about 25 s, for `request` to be approved: `LoggedIn`
    /// with a session for this device, `DeviceSignInPending` (ask again),
    /// or `DeviceSignInDenied` / `DeviceSignInExpired`. Only the device
    /// that began the request may ask, and one wait at a time: a newer
    /// one ends the one before it with `DeviceSignInPending`.
    AwaitDeviceSignIn {
        request: String,
    },
    /// Begin signing in with an app passkey (P3-19, [`crate::passkeys`]):
    /// `PasskeySignInBegun`, a nonce, listing no passkeys (any the
    /// platform finds): anyone can ask. With `username`, only that user's
    /// passkeys finish it. The nonce is this device's, for 5 minutes;
    /// beginning again replaces it. The challenge to sign is
    /// [`crate::app_passkey_challenge`] of this server's id and the nonce.
    BeginPasskeySignIn {
        username: Option<String>,
    },
    /// Finish it with what the passkey signed: `LoggedIn`, a session for
    /// this device, as `LoginPassword` gives (its PIN is asked as usual),
    /// and with `remember` this device trusted, as `LoginPassword` does.
    /// Anything that does not sign in is `InvalidCredentials`, counted as
    /// a failed sign-in (then `RateLimited`).
    FinishPasskeySignIn {
        signed: FinishPasskeySignIn,
        remember: bool,
    },
    /// The users this device is trusted for (P4-01): `TrustedProfiles`,
    /// for "Who's watching" once their sessions have ended. Only this
    /// device's own; nothing about other devices. Trust that lapsed is
    /// left out.
    TrustedProfiles,
    /// Start a session on this device for `user_id` (as `TrustedProfiles`
    /// gives it), without a password: `LoggedIn`, a normal session bound
    /// to this device, `remembered: true`. A device that asks for the
    /// user's PIN gets it locked, as after any sign-in. `Unauthenticated`,
    /// the same for every reason, when this device is not trusted for
    /// them, the trust lapsed or was removed, or the user is disabled.
    /// The server never resumes a profile by itself: auto-login is the
    /// client resuming the profile marked `auto_login` at start.
    ResumeProfile {
        user_id: String,
    },
    /// Make an account with an invite's code (P6-01) and sign in to it:
    /// `LoggedIn`, a session bound to this device as `LoginPassword`
    /// gives, `remembered: false`. The account has the invite's role.
    ///
    /// A code that is not one, or whose invite was revoked, is
    /// `InviteInvalid`, counted as a wrong guess (then `RateLimited`); one
    /// past its expiry `InviteExpired`; one with no uses left
    /// `InviteExhausted`. A username someone has is `UsernameTaken`, and a
    /// username, display name or password that is not acceptable
    /// `InvalidInput`; the invite is not used up by either. A device that
    /// is not signed in draws on the sign-in budget all such devices share,
    /// as `LoginPassword` does.
    RedeemInvite(RedeemInvite),

    // Need a session.
    Libraries,
    Items {
        library: LibraryId,
        offset: u32,
        limit: u32,
    },
    Item {
        id: MediaId,
    },
    /// Scoped, grouped search (P3-09). `Search`.
    Search(SearchRequest),
    /// The items a person a search found is in (P3-09), one page at a
    /// time. `Items`.
    PersonItems {
        person: PersonId,
        offset: u32,
        limit: u32,
    },
    Tracks {
        id: MediaId,
    },
    /// Who this request's session belongs to.
    WhoAmI,
    /// End this request's session (and only it). On a trusted device it
    /// forgets the device for this user too (P4-01): the next time is a
    /// full sign-in.
    Logout,
    /// The session user's live sessions on every device.
    ListSessions,
    /// How the server would play `id` for this client with these tracks.
    /// Without a selection the server chooses them from the user's
    /// preferences (P1-20) and says which in the reply.
    PlaybackPlan {
        id: MediaId,
        #[serde(deserialize_with = "spoolframe_types::required::option")]
        selection: Option<Selection>,
        /// How a picked bitmap subtitle is shown (P2-07).
        bitmap: BitmapSubtitles,
    },
    /// Plan `id` and, if it plays directly, open a playback session to
    /// read it through. Without a selection, as for `PlaybackPlan`.
    StartPlayback {
        id: MediaId,
        #[serde(deserialize_with = "spoolframe_types::required::option")]
        selection: Option<Selection>,
        /// Where to start, for a converted playback (P2-03): the encode
        /// begins there. Direct play reads from wherever the client likes.
        #[serde(deserialize_with = "spoolframe_types::required::option")]
        start_ms: Option<u64>,
        /// How a picked bitmap subtitle is shown (P2-07).
        bitmap: BitmapSubtitles,
    },
    /// End a playback now, and any encode for it.
    StopPlayback {
        handle: PlaybackHandle,
    },
    /// Recover a handle's configuration revision after reconnecting or
    /// reopening an SDK. This does not change the encoder's epoch.
    PlaybackRevision {
        handle: PlaybackHandle,
    },
    /// Restart a converted playback's encode at `position_ms` (P2-04):
    /// reads then name the new epoch and start at offset 0.
    SeekPlayback {
        handle: PlaybackHandle,
        position_ms: u64,
    },
    /// Set or change the session user's PIN (P2-08): 4 to 8 digits. `Done`.
    ///
    /// A user with no PIN sets their first with no `proof`, from a session
    /// that signed in within the last 10 minutes; an older one gets
    /// `RecentSignInRequired`. A user who has one changes it only with a
    /// [`PinProof`]: the current PIN, or the account password when the PIN
    /// is forgotten. Without one it is `InvalidInput`. A locked session
    /// gets `ProfileLocked`.
    SetPin {
        pin: String,
        proof: Option<PinProof>,
    },
    /// Remove the session user's PIN, and its requirement on every device,
    /// with a [`PinProof`], checked as for `SetPin`. `Done`; also for a
    /// user with no PIN (any PIN passes then).
    ClearPin {
        proof: PinProof,
    },
    /// Unlock this session on this connection with the user's PIN (P2-08).
    /// Wrong PINs are rate-limited; after 10 in a row the session on this
    /// device is revoked (`Unauthenticated`): sign in with the password.
    /// `Done`.
    UnlockProfile {
        pin: String,
    },
    /// The devices the session user has signed in from. `Devices`.
    ListDevices,
    /// Begin making an app passkey for the session user (P3-19):
    /// `PasskeyRegistrationBegun`, a nonce, as for `BeginPasskeySignIn`.
    /// Needs a session signed in within the last 10
    /// minutes, or `RecentSignInRequired`: confirm it is the user
    /// (`ConfirmPassword`, `BeginConfirmPasskey`) first.
    BeginPasskeyRegistration,
    /// Keep the passkey made for it, in the session that began it: `Done`.
    /// One that does not verify is `InvalidCredentials`; a name that is
    /// not 1 to 64 characters `InvalidInput` (the challenge is not used
    /// up then).
    FinishPasskeyRegistration(FinishPasskeyRegistration),
    /// The session user's passkeys, the web UI's and the apps', oldest
    /// first: `Passkeys`.
    ListPasskeys,
    /// Rename one of them (`id` from `Passkeys`): `Done`. A name that is
    /// not 1 to 64 characters (trimmed) is `InvalidInput`; an `id` that is
    /// not one of the session user's passkeys `NotFound`.
    RenamePasskey {
        id: String,
        name: String,
    },
    /// Remove one of them: `Done`. Needs a session signed in within the
    /// last 10 minutes, or `RecentSignInRequired`; the user's last way to
    /// sign in is `LastSignInMethod`; an `id` that is not one of their
    /// passkeys `NotFound`.
    RemovePasskey {
        id: String,
    },
    /// Confirm it is the session user, with their account password, so
    /// that a session signed in long ago may make a change that needs a
    /// recent sign-in (`ApproveDeviceSignIn`, `BeginPasskeyRegistration`,
    /// `RemovePasskey`, `SetPinRequired` with `required: false`):
    /// `Confirmed`, and this session counts as signed in now, for 10
    /// minutes. Only this session; the user's others are as they were.
    ///
    /// Checked as `LoginPassword` checks it: a wrong one is a failed
    /// sign-in for the user's username from this device
    /// (`InvalidCredentials`, then `RateLimited`, one limit with
    /// `LoginPassword` and the web UI's confirmation). A user with no
    /// password gets `NoPassword`, with nothing counted: confirm with an
    /// app passkey. A locked session gets `ProfileLocked`. The PIN is
    /// never a confirmation: it guards switching profiles on a shared
    /// device, and is weaker than a sign-in.
    ConfirmPassword(ConfirmPassword),
    /// Begin confirming it is the session user with one of their app
    /// passkeys (P3-19): `PasskeySignInBegun`, a nonce as for
    /// `BeginPasskeySignIn`, listing their app passkeys only. The nonce is
    /// this session's on this device, for 5
    /// minutes; beginning again replaces it. A user with no app passkey
    /// gets `NotFound`; one whose passkey sign-ins are limited
    /// `RateLimited`. A locked session gets `ProfileLocked`.
    BeginConfirmPasskey,
    /// Finish it with what the passkey signed: `Confirmed`, as for
    /// `ConfirmPassword`. A passkey that is not the session user's, a
    /// challenge that is not this session's, or anything else that does
    /// not verify is `InvalidCredentials`, counted as a failed passkey
    /// sign-in (then `RateLimited`).
    FinishConfirmPasskey(FinishPasskeySignIn),
    /// The session user's Plex account link (P3-12): `PlexLink`. Read
    /// only; a client links or unlinks by opening `<web_url>/profile`.
    GetPlexLink,
    /// Ask for the session user's PIN on `device` (an endpoint id from
    /// `Devices`; this device if left out), or stop asking. A locked
    /// session cannot ask this (nor anything but `WhoAmI` and
    /// `UnlockProfile`). Stopping the requirement needs recent authentication.
    /// `Done`.
    SetPinRequired {
        #[serde(deserialize_with = "spoolframe_types::required::option")]
        device: Option<String>,
        required: bool,
    },
    /// Which device shows sign-in code `code` (P3-07), to ask the user
    /// before approving it. `DeviceSignInRequest`; `NotFound` for a code
    /// that is not pending (guesses are rate-limited).
    LookupDeviceSignIn {
        code: String,
    },
    /// Sign the device showing `code` in as the session user. `Done`.
    /// Requires recent authentication; an old session cannot mint a fresh one.
    ///
    /// `remember`: the approver's answer to the device's request to be
    /// trusted (`DeviceSignInRequest::remember`, P4-01). The device is
    /// trusted only if it asked and this is true; this cannot trust a
    /// device that did not ask.
    ApproveDeviceSignIn {
        code: String,
        remember: bool,
    },
    /// Trust `device` (an endpoint id from `Devices`; this device if left
    /// out) for the session user, or stop (P4-01). `Done`.
    ///
    /// - Turning trust on, or auto-login on, works for this device only
    ///   (`InvalidInput` for another: trust another device from the web
    ///   UI), and needs a recent sign-in (`RecentSignInRequired`).
    /// - Auto-login needs trust (`InvalidInput` without). It turns
    ///   auto-login off for the device's other users, and the PIN
    ///   requirement off for this user there.
    /// - Turning trust off works for any of the user's devices, and ends
    ///   the user's sessions there, this one too if it is this device.
    ///   For another device it needs a recent sign-in; for this one it
    ///   needs only the session.
    /// - A device that is not the user's is `NotFound`.
    SetDeviceTrust {
        device: Option<String>,
        trusted: bool,
        auto_login: bool,
    },
    /// Delete one of the session user's other devices (P4-03): `device`
    /// is an endpoint id from `Devices`. `Done`. The user's sessions
    /// there are deleted, and with them its trust and PIN requirement; it
    /// leaves `Devices`, and if it signs in again it is listed as new.
    /// Other users' sessions on that device are kept.
    ///
    /// - Always needs a recent sign-in (`RecentSignInRequired`).
    /// - This device is `InvalidInput`: sign out with `Logout`.
    /// - A device that is not the user's is `NotFound`.
    ForgetDevice {
        device: String,
    },
    /// Refuse it: the device stops waiting. `Done`.
    DenyDeviceSignIn {
        code: String,
    },
    /// Pick another subtitle (or none) for a playing playback (P2-07): it
    /// is planned again with the same audio. A plan that plays directly
    /// only changes what `ReadSubtitle` gives; a converted one restarts its
    /// encode at `position_ms` as a new epoch, from offset 0 (a burned-in
    /// subtitle is in the picture). `PlaybackChanged`; `CannotPlay` leaves
    /// the playback as it was.
    ChangeSubtitle {
        handle: PlaybackHandle,
        subtitle: Option<u32>,
        bitmap: BitmapSubtitles,
        position_ms: u64,
    },
    /// A converted playback's HLS playlist (P2-05).
    GetManifest {
        handle: PlaybackHandle,
        kind: ManifestKind,
    },
    /// The selected external subtitle of a playback, one chunk of it
    /// (P2-11). One the server reads as text (P2-07) answers
    /// `SubtitleNotReady` until it is done: ask again.
    ///
    /// The reply is `SubtitleChunk`: at most [`SUBTITLE_CHUNK`] bytes of
    /// the file from `offset`, `total`, the file's length, and `version`,
    /// which names the file as it was when the chunk was read. Ask from
    /// the end of each chunk, with the first chunk's `version`, until
    /// `total` bytes are read; an `offset` at or past the end answers no
    /// bytes. When the file is no longer the one `version` names, the
    /// answer is `SubtitleChanged`: the chunks read so far are of a file
    /// that is gone, so start again from 0. The first read has no file
    /// to hold the server to, and sends no `version`.
    ReadSubtitle {
        handle: PlaybackHandle,
        offset: u64,
        version: Option<u64>,
    },
    /// Segment `n` (from 1) of an HLS playback's subtitle rendition (P2-07):
    /// `Subtitle { format: WebVtt, .. }`, the cues over that segment's time,
    /// timed with `X-TIMESTAMP-MAP` to the video. `SubtitleNotReady` while
    /// the subtitle is made.
    ReadSubtitleSegment {
        handle: PlaybackHandle,
        n: u32,
    },
    /// The frame previews of a movie or episode, for scrubbing (P3-13):
    /// `Previews`, `PreviewsNotReady` while the server makes them (asking
    /// puts the item first in line; ask again in a few seconds, and show
    /// the time alone meanwhile), or `NoPreviews`.
    GetPreviews {
        id: MediaId,
    },
    /// Where playback of a movie or episode has got to. Send it now and
    /// then while playing and on pause or stop; the server decides what is
    /// written, and when.
    UpdatePlaybackProgress {
        handle: PlaybackHandle,
        sequence: u64,
        position_ms: u64,
    },
    MarkWatched {
        id: MediaId,
    },
    MarkUnwatched {
        id: MediaId,
    },
    /// Remove the card `id`, a movie or an episode, from the session
    /// user's Continue watching or Next up (P1-22), leaving its progress
    /// and watched state. Only that card: never the episode's whole show.
    /// `id` is the card's own, not its show's. Playing or marking it again
    /// brings it back. `Done`.
    HideFromContinueWatching {
        id: MediaId,
    },
    /// Undo a hide (an "Undo" toast). `Done`.
    UnhideFromContinueWatching {
        id: MediaId,
    },
    /// The session user's home screen.
    Home,
    /// How the session user wants tracks chosen (P1-20).
    GetTrackPreferences,
    /// Replace them, for every device of the user.
    SetTrackPreferences {
        preferences: TrackPreferences,
    },
    /// Use the tracks of `selection` in `id` for the rest of its series
    /// (an episode) or for it (anything else), by language and role. No
    /// subtitle in `selection` means subtitles off.
    SetTrackOverride {
        id: MediaId,
        selection: Selection,
    },
    /// Back to the preferences for `id`'s series or for it.
    ClearTrackOverride {
        id: MediaId,
    },

    /// A request type this build does not know; answered with
    /// [`Error::Unsupported`].
    #[serde(other)]
    Unknown,
}

impl Request {
    /// Served without a session.
    pub fn is_pre_auth(&self) -> bool {
        matches!(
            self,
            Request::Hello(_)
                | Request::AuthMethods
                | Request::LoginPassword(_)
                | Request::Ping
                | Request::BeginDeviceSignIn { .. }
                | Request::AwaitDeviceSignIn { .. }
                | Request::BeginPasskeySignIn { .. }
                | Request::FinishPasskeySignIn { .. }
                | Request::TrustedProfiles
                | Request::ResumeProfile { .. }
                | Request::RedeemInvite(_)
        )
    }
}

/// The most characters the server keeps of each of `Hello`'s strings
/// (PX-10): `client_name`, `client_version` and `device_name`. It cuts a
/// longer one to this many, and keeps and shows only that.
pub const MAX_HELLO_TEXT: usize = 128;

/// The client introduces itself. Every field is required (PX-15). Each
/// string is cut to [`MAX_HELLO_TEXT`] characters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u32,
    /// e.g. `spoolframe-linux`.
    pub client_name: String,
    pub client_version: String,
    /// What kind of client this is: the server keeps it for the device, for
    /// its device and session lists. Not taken from `client_name`, which
    /// one app may share across platforms.
    pub platform: Platform,
    /// What the device calls itself (a hostname, "Living room TV"); shown
    /// in session lists. `None` when it has no name to give.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub device_name: Option<String>,
    /// What this device can play (P1-07). `None`: every playback plan is
    /// `Unsupported`.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub capabilities: Option<ClientCapabilities>,
}

impl Hello {
    /// This `Hello` with each string cut to [`MAX_HELLO_TEXT`] characters:
    /// what a server keeps of it.
    pub fn within_bounds(mut self) -> Self {
        fn cut(text: &mut String) {
            if let Some((at, _)) = text.char_indices().nth(MAX_HELLO_TEXT) {
                text.truncate(at);
            }
        }
        cut(&mut self.client_name);
        cut(&mut self.client_version);
        if let Some(name) = &mut self.device_name {
            cut(name);
        }
        self
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginPassword {
    pub username: String,
    pub password: String,
    /// Trust this device for the user (P4-01): it may resume them later
    /// without a password (`ResumeProfile`). Never true unless the user
    /// ticked "Remember this device".
    pub remember: bool,
}

impl fmt::Debug for LoginPassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginPassword")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("remember", &self.remember)
            .finish()
    }
}

/// An invite's code and the account to make with it (`RedeemInvite`,
/// P6-01). `code` as the link carries it ([`crate::invite::Invite::code`]);
/// any case, dashes or spaces are taken.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedeemInvite {
    pub code: String,
    pub username: String,
    /// What to show for them; the username when `None`.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub display_name: Option<String>,
    pub password: String,
}

impl fmt::Debug for RedeemInvite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedeemInvite")
            .field("code", &"<redacted>")
            .field("username", &self.username)
            .field("display_name", &self.display_name)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// The account password, to confirm it is the session user
/// (`ConfirmPassword`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmPassword {
    pub password: String,
}

impl fmt::Debug for ConfirmPassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfirmPassword")
            .field("password", &"<redacted>")
            .finish()
    }
}

/// What proves a PIN change or removal is the user's (P2-08): the PIN
/// they have, or, when it is forgotten, their account password.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PinProof {
    /// The current PIN, checked as `UnlockProfile` checks it: a wrong one
    /// counts towards the same limit (`InvalidCredentials`, then
    /// `RateLimited`) and the same count of wrong PINs in a row; the 10th
    /// revokes the session on this device (`Unauthenticated`). Checking it
    /// unlocks nothing. A user with no PIN passes any.
    Pin { pin: String },
    /// The account password, checked as `LoginPassword` checks it: a wrong
    /// one is a failed sign-in for this username from this device
    /// (`InvalidCredentials`, then `RateLimited`, one limit with
    /// `LoginPassword`). It never revokes the session and never touches
    /// the PIN's limit. A user with no password (passkeys or SSO only) gets
    /// `NoPassword`: the PIN proof, or the web UI, is theirs.
    Password { password: String },
}

impl fmt::Debug for PinProof {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            PinProof::Pin { .. } => "Pin",
            PinProof::Password { .. } => "Password",
        };
        write!(f, "PinProof::{kind}(<redacted>)")
    }
}

/// The session user's track preferences (`GetTrackPreferences`), and
/// whether this server reads picture subtitles as text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackPreferencesInfo {
    pub preferences: TrackPreferences,
    /// The server reads picture subtitles as text: `subtitles.ocr` is on
    /// and tesseract has at least one language. When false,
    /// `preferences.subtitles.bitmap` has no effect, and apps show that
    /// choice disabled.
    pub ocr_offered: bool,
}

/// How a user can log in to this server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    Password,
    /// Approval on another signed-in device (P3-07): `BeginDeviceSignIn`.
    DeviceApproval,
    /// A method this build does not know.
    #[serde(other)]
    Unknown,
}

/// The answer to one request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[expect(
    clippy::large_enum_variant,
    reason = "built once per request and encoded at once; boxing would only add noise"
)]
pub enum Response {
    Ok(Reply),
    Err(Error),
}

impl From<Result<Reply, Error>> for Response {
    fn from(result: Result<Reply, Error>) -> Self {
        match result {
            Ok(reply) => Response::Ok(reply),
            Err(error) => Response::Err(error),
        }
    }
}

impl From<Response> for Result<Reply, Error> {
    fn from(response: Response) -> Self {
        match response {
            Response::Ok(reply) => Ok(reply),
            Response::Err(error) => Err(error),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    PlaybackProgressSequence {
        sequence: u64,
    },
    ServerInfo(ServerInfo),
    AuthMethods {
        methods: Vec<AuthMethod>,
    },
    LoggedIn {
        session: SessionId,
        /// This device is trusted for the session's user after it
        /// (P4-01): the sign-in asked to be remembered and it was (by the
        /// approver too, for a device sign-in), or the device already was.
        remembered: bool,
    },
    /// The users this device is trusted for (`TrustedProfiles`), by
    /// username.
    TrustedProfiles {
        profiles: Vec<TrustedProfile>,
    },
    Pong,
    /// A device sign-in is pending (P3-07). Show `code` (see
    /// [`crate::device_sign_in::display`]); keep `request` to await it.
    DeviceSignInStarted {
        request: String,
        code: String,
        expires_in_ms: u64,
        /// Where a browser approves it
        /// ([`crate::device_sign_in::web_link`]), for a QR code a phone's
        /// camera opens; `None` if the server's web UI has no public URL.
        web_url: Option<String>,
    },
    /// Not approved yet: `AwaitDeviceSignIn` again.
    DeviceSignInPending,
    /// The device a sign-in code belongs to.
    DeviceSignInRequest(DeviceSignInRequest),
    Libraries {
        libraries: Vec<Library>,
    },
    Items {
        items: Vec<Item>,
        total: u32,
    },
    Item {
        item: Item,
        children: Vec<Item>,
        /// The item's intro and credits (P2-10), in order of start; empty
        /// for an item that has none.
        markers: Vec<Marker>,
    },
    Search(SearchResults),
    Tracks(MediaTracks),
    WhoAmI(UserSummary),
    LoggedOut,
    Sessions {
        sessions: Vec<SessionInfo>,
    },
    PlaybackPlan {
        plan: PlaybackPlan,
        /// The tracks the server chose, when the request had no selection.
        #[serde(deserialize_with = "spoolframe_types::required::option")]
        chosen: Option<ChosenTracks>,
    },
    PlaybackStarted {
        handle: PlaybackHandle,
        /// Always `Direct` in Phase 1.
        plan: PlaybackPlan,
        /// The size of the file the handle reads.
        size: u64,
        /// The tracks the server chose, when the request had no selection.
        #[serde(deserialize_with = "spoolframe_types::required::option")]
        chosen: Option<ChosenTracks>,
        /// The encode reads name; 0 for direct play.
        epoch: u32,
        /// The item's intro and credits, as in `Item`, so a player needs
        /// no second request for them. Their times are the item's own,
        /// wherever the playback starts.
        markers: Vec<Marker>,
    },
    /// A seek restarted the encode: reads name this epoch from offset 0.
    PlaybackSeeked {
        epoch: u32,
    },
    PlaybackRevision {
        revision: u64,
    },
    /// The session user's devices (P2-08), most recently seen first.
    Devices {
        devices: Vec<DeviceInfo>,
    },
    /// The session user's Plex link; `None`: not linked (or the server
    /// links no Plex accounts).
    PlexLink {
        link: Option<PlexLink>,
    },
    /// The playback's new plan after `ChangeSubtitle`: read `epoch` (0 for
    /// direct play), whose size is `size` (0 while a conversion writes it).
    PlaybackChanged {
        plan: PlaybackPlan,
        size: u64,
        epoch: u32,
    },
    /// An HLS playlist, as text.
    Manifest {
        text: String,
    },
    /// A segment of a subtitle rendition, whole (`ReadSubtitleSegment`).
    Subtitle {
        format: SubtitleFormat,
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
    },
    /// The chunk of a subtitle file that `ReadSubtitle` asked for (P2-11).
    SubtitleChunk {
        format: SubtitleFormat,
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
        /// The whole file's length.
        total: u64,
        /// The file these bytes are of. It is another value once the file
        /// is written again or replaced, and means nothing else to a
        /// client: send it back with the next `ReadSubtitle`.
        version: u64,
    },
    /// The request was carried out; there is nothing to return.
    Done,
    /// The session user confirmed it is them (`ConfirmPassword`,
    /// `FinishConfirmPasskey`): this session counts as signed in now.
    Confirmed,
    Home(HomeScreen),
    TrackPreferences(TrackPreferencesInfo),
    /// An item's frame previews (`GetPreviews`, P3-13).
    Previews(Previews),
    /// What the platform makes an app passkey with, but the challenge:
    /// the nonce it is made from (`BeginPasskeyRegistration`, P3-19).
    PasskeyRegistrationBegun(PasskeyRegistrationBegun),
    /// What the platform signs in with an app passkey with, but the
    /// challenge: the nonce it is made from (`BeginPasskeySignIn`,
    /// `BeginConfirmPasskey`, P3-19).
    PasskeySignInBegun(PasskeySignInBegun),
    /// The session user's passkeys (`ListPasskeys`).
    Passkeys {
        passkeys: Vec<PasskeyInfo>,
    },
}

/// Which HLS playlist (P2-05).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestKind {
    /// The top playlist, naming the variant.
    Multivariant,
    /// The variant's segments.
    Media,
    /// The subtitle rendition's WebVTT segments (P2-07), read with
    /// `ReadSubtitleSegment`; there when the plan has a subtitle.
    Subtitles,
    #[serde(other)]
    Unknown,
}

/// Rows for a home screen. Every item carries the user's playback state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomeScreen {
    /// Started and not finished, most recent first.
    pub continue_watching: Vec<Item>,
    /// For each recently watched show, its next unwatched episode.
    pub next_up: Vec<Item>,
    pub recently_added: Vec<Item>,
}

/// A session's user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserSummary {
    /// Opaque; stable for the user on this server. Since P3-20 a
    /// server's ids are UUIDs (`local:<uuid>` today), and one that served
    /// `local:<number>` before changes it once: a client that kept an id
    /// reads it again from `WhoAmI`.
    pub user_id: String,
    pub username: String,
    /// What to show; the username when the user has no display name.
    pub display_name: String,
    /// What the user may do (P2-09).
    pub role: Role,
    /// The session needs the user's PIN on this connection before anything
    /// but `WhoAmI` and `UnlockProfile` (P2-08).
    pub locked: bool,
    /// The user's PIN, if they have one (P2-08): for "Set · 4 digits", and
    /// for how many boxes a PIN pad shows. Never the PIN itself.
    pub pin: Option<PinState>,
}

/// A user's PIN as clients may know it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinState {
    /// How many digits it has (4 to 8); `None` for a PIN set before the
    /// server kept its length, until it is set again.
    pub digits: Option<u8>,
}

/// A user a device is trusted for (P4-01), as that device sees them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedProfile {
    /// What `ResumeProfile` takes: `UserSummary::user_id`.
    pub user_id: String,
    /// To match the profile with one the device keeps by username.
    pub username: String,
    /// What to show; the username when the user has no display name.
    pub display_name: String,
    /// The user's PIN is asked for on this device: a resumed session
    /// starts locked.
    pub pin_required: bool,
    /// Open this profile at start, past "Who's watching". At most one per
    /// device.
    pub auto_login: bool,
}

/// One of a user's devices (P2-08).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// Hex `EndpointId`: what `SetPinRequired { device }` takes.
    pub endpoint_id: String,
    /// What the device calls itself.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub name: Option<String>,
    /// What kind of client it runs, as its `Hello` said; `None` for a
    /// device not seen since before the server kept it.
    pub platform: Option<Platform>,
    /// Unix seconds.
    pub last_seen_at: i64,
    /// The user's PIN is asked for on it.
    pub pin_required: bool,
    /// It may resume the user without a password (P4-01).
    pub trusted: bool,
    /// It opens the user's profile at start (P4-01).
    pub auto_login: bool,
    /// The device this request came from.
    pub this_device: bool,
}

/// A device waiting to be signed in (P3-07), as the approver sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceSignInRequest {
    /// What the device calls itself (from its `Hello`).
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub device_name: Option<String>,
    /// Its app, e.g. `spoolframe-tvos`.
    pub client_name: String,
    /// What kind of client it runs, as its `Hello` said: shown beside its
    /// name ([`Platform::label`], "Apple TV app"), so the approver knows
    /// which device asks.
    pub platform: Platform,
    /// Unix seconds.
    pub requested_at: i64,
    /// The device asked to be remembered (P4-01): the approval says "This
    /// device will be remembered", ticked, and the approver may untick it.
    pub remember: bool,
    /// Where the device's `BeginDeviceSignIn` came from, as the server saw
    /// it (PX-09 finding 27): the one thing on this screen an attacker who
    /// sent the approver a code cannot choose. `None` if the server could
    /// not tell. The approval shows it, "The TV asked from 192.168.86.20.",
    /// and for [`AddressNetwork::Internet`] warns instead: the device is
    /// not on the approver's network, so approve only one you can see.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub requested_from: Option<DeviceAddress>,
}

/// An address a device reached the server from (PX-09 finding 27).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceAddress {
    /// Its IP address as text: `192.168.86.20`, `fd7a:115c:a1e0::1`. An
    /// IPv4 address is never in its IPv6-mapped form.
    pub address: String,
    /// What kind of network that address is on, as the server judges it.
    pub network: AddressNetwork,
}

/// What kind of network an address is on, as the server's web approval
/// page judges it too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AddressNetwork {
    /// The server's own machine: `127.0.0.0/8`, `::1`.
    Loopback,
    /// A home, office or VPN network: private IPv4 ranges, link-local,
    /// CGNAT and VPNs' `100.64.0.0/10`, IPv6 unique-local and link-local.
    Local,
    /// A public address: a device far from the approver, which is what a
    /// sign-in code sent to someone else (device-code phishing) looks
    /// like. The approval warns.
    Internet,
    /// A kind this build does not know, from a newer server. Show the
    /// address and warn as for [`AddressNetwork::Internet`].
    #[serde(other)]
    Unknown,
}

/// One live session, without its token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// The device's name; `None` for an admin API session or an unnamed
    /// device.
    pub device_name: Option<String>,
    /// What kind of client the device runs; `None` for an admin API
    /// session (the web UI or the CLI) or a device not seen since before
    /// the server kept it.
    pub platform: Option<Platform>,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds.
    pub last_used_at: i64,
    /// The session this request was made in.
    pub current: bool,
}

/// Who the server is. `endpoint_id` is what the client pins (P1-13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    /// Hex, as `spoolframe-server id` prints it.
    pub endpoint_id: String,
    /// The server's id ([`crate::ServerId`]): made once, kept when its key
    /// is replaced. A client pins it beside `endpoint_id`, the first time
    /// it sees it.
    pub server_id: String,
    pub protocol_version: u32,
    pub server_version: String,
    pub auth_methods: Vec<AuthMethod>,
    /// Where a browser reaches the server's web UI (`admin.public_url`),
    /// if it has one: for "open in the browser" links, such as linking a
    /// Plex account (P3-12). Absent without a public URL, since the
    /// listener's own address may be loopback or one no browser trusts.
    pub web_url: Option<String>,
}

/// Why a request failed. Typed, so clients act on the kind, not on text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Error {
    #[error("progress belongs to an older playback or sequence")]
    StaleProgress,
    #[error("the operation belongs to an earlier server instance")]
    ServerRestarted,
    #[error("the operation expired; its outcome is no longer recoverable")]
    OperationExpired,
    #[error("the operation ID was already used with different arguments")]
    OperationConflict,
    #[error("the playback configuration changed (revision {revision})")]
    RevisionConflict { revision: u64 },
    /// The connection's first request was not `Hello`.
    #[error("send Hello first")]
    HandshakeRequired,
    /// The request needs a session, and none valid for this endpoint was
    /// given.
    #[error("not authenticated")]
    Unauthenticated,
    /// The server does not know this request type.
    #[error("unsupported request {request:?}")]
    Unsupported { request: String },
    #[error("malformed request: {message}")]
    Malformed { message: String },
    #[error("{what} not found")]
    NotFound { what: String },
    /// Wrong username or password; deliberately the same for both.
    #[error("invalid credentials")]
    InvalidCredentials,
    /// Too many failed logins for this username or from this endpoint.
    #[error("too many failed logins; retry in {retry_after_ms} ms")]
    RateLimited { retry_after_ms: u64 },
    /// The client cannot play this selection; one reason per stream, as
    /// in [`PlaybackPlan::Unsupported`].
    #[error("cannot play: {}", reasons.join("; "))]
    CannotPlay { reasons: Vec<String> },
    /// The playback handle is unknown or has been idle too long. Call
    /// `StartPlayback` again and go on from where playback was.
    #[error("the playback has expired; start it again")]
    PlaybackExpired,
    /// An admin stopped the playback on the server (P3-15). It is over:
    /// tell the viewer it ended, and do not start it again on your own
    /// as after `PlaybackExpired`. Its progress is kept.
    #[error("the playback was stopped on the server")]
    PlaybackStopped,
    /// The server is converting as many files as it may at once.
    #[error("the server is busy converting; try again later")]
    Busy,
    /// The encode for this playback failed; `reason` is ffmpeg's.
    #[error("the conversion failed: {reason}")]
    PlaybackFailed { reason: String },
    /// The read names an encode that a seek replaced; read `epoch` from
    /// offset 0.
    #[error("the playback restarted at a seek (epoch {epoch})")]
    PlaybackRestarted { epoch: u32 },
    /// The subtitle is still being read as text (P2-07); `done_percent` of
    /// its cues are read. Ask again in a few seconds.
    #[error("the subtitle is still being read ({done_percent}% done)")]
    SubtitleNotReady { done_percent: u8 },
    /// The item's frame previews are being made (P3-13); `done_percent` of
    /// them are. Ask again in a few seconds; show the time alone till then.
    #[error("the previews are being made ({done_percent}% done)")]
    PreviewsNotReady { done_percent: u8 },
    /// The item has no frame previews and will not have: music, a file
    /// they could not be made of, or a server that makes none. Do not ask
    /// again for this item.
    #[error("this item has no previews")]
    NoPreviews,
    /// The subtitle is not the file that `ReadSubtitle`'s `version` names:
    /// it was written again or replaced since that chunk (P2-11). Read it
    /// again from offset 0.
    #[error("the subtitle changed while it was read")]
    SubtitleChanged,
    /// The session's profile is locked on this connection: `UnlockProfile`
    /// with the user's PIN first (P2-08).
    #[error("this profile is locked; enter its PIN")]
    ProfileLocked,
    /// A value the request carries is not acceptable (a PIN that is not 4
    /// to 8 digits, say); the message says why.
    #[error("{message}")]
    InvalidInput { message: String },
    /// The request carried the user's account password, and they have none:
    /// they sign in with a passkey or SSO only (a `PinProof::Password`).
    /// Not a wrong password, and nothing was counted. Use another proof, or
    /// send them to the server's web UI (`ServerInfo::web_url`, when the
    /// server has one).
    #[error("this account has no password; use the PIN or the web UI")]
    NoPassword,
    /// The session was signed in too long ago for this (an app passkey,
    /// P3-19; approving a device, P3-07): confirm it is the user
    /// (`ConfirmPassword`, or `BeginConfirmPasskey`), or sign in again,
    /// then ask again. Nothing was changed.
    #[error("sign in again first")]
    RecentSignInRequired,
    /// Removing this would leave the user no way to sign in: it is their
    /// only passkey, and they have no password or linked sign-in provider
    /// (`RemovePasskey`). Nothing was changed.
    #[error("that is the last way to sign in to this account")]
    LastSignInMethod,
    /// The device sign-in was refused on the approving device (P3-07).
    #[error("the sign-in was denied")]
    DeviceSignInDenied,
    /// The device sign-in is over: it timed out, or is not this device's.
    /// Begin a new one.
    #[error("the sign-in code has expired")]
    DeviceSignInExpired,
    /// The reply would be over the frame limit
    /// ([`crate::codec::MAX_FRAME`]); `bytes` is its length. A page of
    /// items is to ask for in smaller pages.
    #[error("the reply is too large to send ({bytes} bytes)")]
    ReplyTooLarge { bytes: u64 },
    /// Something the server depends on (Plex, a media mount) is down.
    #[error("unavailable: {message}")]
    Unavailable { message: String },
    /// The server has no library yet: it is waiting to be set up in its
    /// web UI (P3-10). Every catalog and playback request gets this until
    /// it is; show "This server is not set up yet", not an error. Signing
    /// in and the rest work.
    #[error("this server is not set up yet")]
    NotSetUp,
    /// `RedeemInvite` with a code that is no invite's, or whose invite
    /// was revoked (P6-01). Counted as a wrong guess. Offer "Save server
    /// only", which pins the server as a link without a code does.
    #[error("that invite code is not valid")]
    InviteInvalid,
    /// The invite's code is right, and it has expired: ask for another.
    #[error("the invite has expired")]
    InviteExpired,
    /// The invite's code is right, and every account it could make has
    /// been made: ask for another.
    #[error("the invite has been used up")]
    InviteExhausted,
    /// Someone on this server has that username: pick another. Nothing
    /// was made and the invite was not used up.
    #[error("that username is taken")]
    UsernameTaken,
    #[error("internal server error")]
    Internal,
    /// An error type this build does not know.
    #[serde(other)]
    #[error("unknown error")]
    Unknown,
}

/// Something the server tells the client without being asked, on the
/// events stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// The server is stopping; the connection will close.
    ShuttingDown,
    /// Where this server can be reached (PX-12), pushed to a connection
    /// as soon as it is signed in, and again whenever the list changes.
    /// Each one is the whole list: it replaces what was told before.
    /// Never sent to a connection that is not signed in.
    ServerAddresses {
        /// The addresses `network.announce` tells, with their port, as
        /// text: `192.168.1.10:4433` or `[2001:db8::1]:4433`.
        #[serde(with = "socket_addrs_as_text")]
        addrs: Vec<std::net::SocketAddr>,
        /// The URLs of the iroh relays the server uses (`network.relay`);
        /// empty while relays are off.
        relays: Vec<String>,
    },
    /// An event type this build does not know; ignore it.
    #[serde(other)]
    Unknown,
}

/// Socket addresses on the wire as text (`192.168.1.10:4433`,
/// `[2001:db8::1]:4433`). CBOR is not a human-readable format to serde, so
/// a bare `SocketAddr` would go as serde's enum of byte tuples, which a
/// peer outside Rust would have to copy; text is also what JSON shows.
mod socket_addrs_as_text {
    use std::net::SocketAddr;

    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(addrs: &[SocketAddr], s: S) -> Result<S::Ok, S::Error> {
        let text: Vec<String> = addrs.iter().map(ToString::to_string).collect();
        text.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<SocketAddr>, D::Error> {
        Vec::<String>::deserialize(d)?
            .iter()
            .map(|text| {
                text.parse()
                    .map_err(|_| D::Error::custom(format!("not a socket address: {text:?}")))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
