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
    ClientCapabilities, DisplayCapabilities, Item, Library, LibraryId, Marker, MediaId,
    MediaTracks, PersonId, Platform, Role, SubtitleFormat, TrackPreferences,
};

use crate::invite_admin::{CreateInvite, InviteCreated, InviteSummary};
use crate::media::PlaybackHandle;
use crate::pairing::{PairingAnswer, PairingOffer, RelayCode};
use crate::passkeys::{
    FinishPasskeyRegistration, FinishPasskeySignIn, PasskeyInfo, PasskeyRegistrationBegun,
    PasskeySignInBegun,
};
use crate::playback::{BitmapSubtitles, ChosenTracks, PlaybackPlan, Quality, Selection};
use crate::playlists::{
    Kinds, PinNote, PlaylistEdit, PlaylistPage, PlaylistRef, PlaylistSummary, Visibility,
};
use crate::plex::PlexLink;
use crate::portable::{
    IdentityRecord, PortableIdentityState, PortableUserId, TieProof, UserDeviceCredential,
    signature_hex,
};
use crate::previews::Previews;
use crate::search::{SearchRequest, SearchResults};

/// The protocol revision inside the `spoolframe/client/1` ALPN. Raised
/// for coordinated development releases, including breaking wire changes.
pub const PROTOCOL_VERSION: u32 = 15;

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
    /// Begin signing in as a portable user with a device credential
    /// (P7-06): `PortableSignInBegun`, a nonce for the device key to sign
    /// over ([`crate::portable::device_proof_message`] of this server's
    /// endpoint id and it). The nonce is this device's, for
    /// `expires_in_secs` (5 minutes), and is used once, whatever the
    /// finish makes of it; beginning again replaces it. Draws on the
    /// sign-in budget all devices not signed in share.
    BeginPortableSignIn,
    /// Finish it (P7-06, §58): `LoggedIn`, `remembered: false`, a session
    /// bound to this device. Local-first: where the identity is tied to an
    /// account here (P7-04) the session is that account's, as any sign-in
    /// to it (its PIN is asked as usual); else, where it has a grant here,
    /// the portable user's own; else nothing. Every refusal (no grant, a
    /// credential or proof that does not hold, a revoked key, an expired
    /// or missing nonce, a disabled account) is `InvalidCredentials`,
    /// counted as a failed sign-in (then `RateLimited`).
    FinishPortableSignIn(PortableSignIn),
    /// Redeem a `portable-account` invite (P7-05) as the portable user
    /// `sign_in` proves, over the nonce `BeginPortableSignIn` gave: the
    /// server grants them access (no account is made), uses one of the
    /// invite's uses, and signs them in as `FinishPortableSignIn` does:
    /// `LoggedIn`. Someone granted here already keeps their grant, and
    /// the invite its use.
    ///
    /// The code is checked as `RedeemInvite` checks it (`InviteInvalid`,
    /// counted as a wrong guess; `InviteExpired`; `InviteExhausted`); a
    /// `create-user` invite's code is `WrongInviteKind`, and its use is
    /// kept. The credential is checked as `FinishPortableSignIn` checks
    /// it: `InvalidCredentials`, counted.
    RedeemPortableInvite(RedeemPortableInvite),
    /// What this server keeps of portable identity `user` (P7-08): its
    /// genesis and newest state, `IdentityRecord`, for a device recovering
    /// the identity with one of its roots. Only an identity known here
    /// (tied or granted) has one; any other is `NotFound`. Public data,
    /// served to anyone, on the strangers' budget.
    IdentityRecord {
        user: PortableUserId,
    },
    /// The record, as `IdentityRecord` answers it, of the identity whose
    /// genesis handle is `handle` (P7-08): what a security key gives back
    /// as its credential's user handle, for a fresh install that holds a
    /// key and nothing else. A key added to a phrase identity carries the
    /// identity's id as its handle, so a handle that is the id of a phrase
    /// identity known here answers that identity's record. Only an
    /// identity known here has one; any other is `NotFound`. Handles are
    /// 32 random bytes, so a miss says nothing. Public data, served to
    /// anyone, on the strangers' budget.
    IdentityRecordByHandle {
        #[serde(with = "crate::portable::handle_hex")]
        handle: [u8; crate::portable::HANDLE_LEN],
    },
    /// Hold a device's pairing request under its relay code (P7-08), for a
    /// root terminal with no camera to fetch: `Done`. For
    /// [`crate::pairing::RELAY_TTL_SECS`]; offering again from the same
    /// device replaces it. A code another device holds is
    /// `InvalidInput`. On the strangers' budget.
    OfferPairing(PairingOffer),
    /// The pairing request held under `code` (P7-08): `PairingOffered`, or
    /// `NotFound`. On the strangers' budget, so codes are not guessed.
    FetchPairing {
        code: RelayCode,
    },
    /// The root terminal's answer to the request under `code` (P7-08):
    /// `Done`. One answer only, for the key the request named; another,
    /// or no request, is `NotFound`; a credential for another key
    /// `InvalidInput`. The server carries it and checks nothing else: the
    /// device does. On the strangers' budget.
    AnswerPairing {
        code: RelayCode,
        answer: PairingAnswer,
    },
    /// The answer to this device's request under `code`, if one came
    /// (P7-08): `PairingAnswered`, taken (the request ends with it) or
    /// `None` while there is none yet. Only the device that offered it;
    /// any other, or no request, is `NotFound`. Not on the strangers'
    /// budget: the device asks every few seconds.
    CollectPairing {
        code: RelayCode,
    },

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
        /// What the screen shows now (PGPU-06): its HDR formats and bit
        /// depth, over Hello's, for this plan. Left out, Hello's.
        // compat(v1): required at the protocol reset
        #[serde(default)]
        display: Option<DisplayCapabilities>,
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
        /// The quality the viewer chose for it (P6-06): for the link, at
        /// most a bitrate, or the original.
        quality: Quality,
        /// What the screen shows now (PGPU-06): its HDR formats and bit
        /// depth, over Hello's, for this plan and the playback it starts;
        /// a re-plan without one keeps it. Left out, Hello's.
        // compat(v1): required at the protocol reset
        #[serde(default)]
        display: Option<DisplayCapabilities>,
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
    /// Change the session user's password: `Done`. `current_password` is
    /// checked as `LoginPassword` checks it (a wrong one is
    /// `InvalidCredentials`, counted towards the same limit). A
    /// `new_password` the server does not take, or the same as the current
    /// one, is `InvalidInput`. Every other session of the user's ends, on
    /// the web and on devices. The way out of a session a one-time
    /// password opened (`PasswordChangeRequired`): this session may then
    /// do anything again. A user with no password gets `NoPassword`: set
    /// one in the web UI. Not for a portable user (`NotForPortableUsers`).
    ChangePassword(ChangePassword),
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
        /// What the screen shows now (PGPU-06), over Hello's, for this
        /// plan and the playback from then on. Left out, what the playback
        /// started with.
        // compat(v1): required at the protocol reset
        #[serde(default)]
        display: Option<DisplayCapabilities>,
    },
    /// Plan a playing playback again for `quality` (P6-06): the viewer
    /// chose another, or, with `Auto`, the device found its link too slow
    /// for a direct play that stalls. The same tracks, and `bitmap` as
    /// `ChangeSubtitle` takes it. A plan that changes restarts at
    /// `position_ms` as a new epoch, from offset 0, as `ChangeSubtitle`
    /// does; one that does not is answered as it is, nothing restarted.
    /// `PlaybackChanged`.
    ChangeQuality {
        handle: PlaybackHandle,
        quality: Quality,
        bitmap: BitmapSubtitles,
        position_ms: u64,
        /// What the screen shows now (PGPU-06), over Hello's, for this
        /// plan and the playback from then on. Left out, what the playback
        /// started with.
        // compat(v1): required at the protocol reset
        #[serde(default)]
        display: Option<DisplayCapabilities>,
    },
    /// A converted playback's HLS playlist (P2-05).
    GetManifest {
        handle: PlaybackHandle,
        kind: ManifestKind,
        /// For a media playlist, the variant's (PX-21): a ladder's rung,
        /// as `v{n}/media.m3u8` in the multivariant playlist. 0 for one
        /// encode, and for the other kinds.
        variant: u32,
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
    /// Begin tying the session user's account to a portable identity
    /// (P7-04): `TieChallenge`, a nonce for the identity to sign over
    /// ([`crate::portable::tie_message`]). The nonce is this session's,
    /// for its user, for `expires_in_secs` (5 minutes), and is used once,
    /// whatever `TieFinish` makes of it. Needs a session signed in within
    /// the last 10 minutes, or `RecentSignInRequired`. An account may be
    /// tied already: it ties any number of identities (P7-09).
    TieBegin,
    /// Tie the session user's account to `user`, with `proof` that `user`
    /// signed the tie message over the nonce `TieBegin` gave: by the root,
    /// or by a device key with a credential for `user` that holds and is
    /// not revoked by any identity state this server has seen. `Done`.
    ///
    /// Needs a recent sign-in, as `TieBegin`. A nonce that is not this
    /// session's, is used or is over is `TieChallengeExpired`: begin
    /// again. A proof that does not hold is `InvalidCredentials`. An
    /// identity tied to another account here is `PortableIdTaken`; one
    /// tied to this account already is `Done`, changing nothing (its
    /// first tie date stands). The account's other ties stay (P7-09).
    /// Nothing about the account moves: the tie is a link beside it.
    TieFinish {
        user: PortableUserId,
        proof: TieProof,
    },
    /// Sever the session user's tie to `user` (P7-04, P7-09): `Done`.
    /// Only that link goes; the account's other ties, its sessions (those
    /// opened with `user`'s credential too), passkeys and watch state stay
    /// as they are. Needs a recent sign-in. No tie to `user` on this
    /// account is `NotFound`.
    Untie {
        user: PortableUserId,
    },

    /// Make an invite (PX-44, [`crate::invite_admin`]), as the web UI's
    /// `inviteCreate` does, with its limits: `InviteCreated`, the code
    /// shown this once. An admin's only (`AdminRequired`), and only after
    /// they confirmed it is them recently (`RecentSignInRequired`: confirm
    /// with `ConfirmPassword` or a passkey, then ask again). A
    /// `federation` invite is `InvalidInput` until P8 builds them, as are
    /// fields out of their limits.
    CreateInvite(CreateInvite),
    /// The invites that can still be redeemed, newest first, made here or
    /// in the web UI: `Invites`. An admin's only; no confirmation needed.
    ListInvites,
    /// Revoke invite `id`: its code stops working, and what it made stays.
    /// `Done`. An admin's only, after a recent confirmation, as
    /// `CreateInvite`. One that is not live (revoked, expired, used up, or
    /// none) is `NotFound`.
    RevokeInvite {
        id: String,
    },

    /// Experimental (`http-playback`, PX-49; see [`crate::experimental`]):
    /// an HTTP URL for one of this session's playbacks, for a player with
    /// no Iroh (AirPlay first): `HttpPlaybackUrl`, or
    /// `HttpPlaybackRefused` with why not. Refused as `Off` unless this
    /// connection's `Hello` named the feature and the server's answer
    /// named it back. The URL is
    /// `http://<address>:<port>/<token>/master.m3u8` for HLS, `.../file`
    /// for direct play, its token unguessable and bound to this playback:
    /// it works until the playback ends or is stopped, or nothing asks for
    /// it for a while. Asked again, it may be the same URL. A newer
    /// server may change or drop this.
    HttpPlaybackUrl {
        handle: PlaybackHandle,
    },

    /// Leave this server (PX-47): `Done`, and every session of the user
    /// here is ended, this one too. Needs a recent sign-in
    /// (`RecentSignInRequired`): a local user confirms with
    /// `ConfirmPassword` or a passkey; a portable user (P7-06) signs in
    /// with its credential again, which opens a new session that counts as
    /// recent. What leaving does depends on who:
    ///
    /// - A portable user's grant is taken away, and its data here (watch
    ///   state, hidden rows, track preferences) is deleted. Coming back
    ///   takes a new invite.
    /// - A local account is set aside, not deleted: disabled and marked
    ///   as left, its sessions ended and its devices no longer trusted;
    ///   its data stays, and an admin can restore it.
    ///
    /// The last enabled admin (or the last with a password) cannot leave:
    /// `LastAdmin`. Nothing was changed then.
    LeaveServer,

    /// Playlists (PX-40, feature `spoolframe:playlists`): every playlist
    /// the session user sees, theirs, shared with them and server-wide,
    /// each with whether it is pinned and where; and the notes of pinned
    /// ones that left their Pinned. `Playlists`.
    ListPlaylists,
    /// One playlist and a page of its entries, from `offset`, at most
    /// `limit` (200): `Playlist`. Items the user may not see are neither
    /// listed nor counted; an item whose file cannot be read now is
    /// listed as unavailable. One they do not see is `NotFound`.
    GetPlaylist {
        id: String,
        offset: u32,
        limit: u32,
    },
    /// Make a playlist the session user owns: `PlaylistInfo`. A name is 1
    /// to 100 characters; `kinds` ticks at least one. `ServerWide` is an
    /// admin's only (`AdminRequired`); a share naming someone who is not a
    /// user here is `InvalidInput`.
    CreatePlaylist {
        name: String,
        kinds: Kinds,
        visibility: Visibility,
    },
    /// Rename a playlist, widen its kinds (never narrow them:
    /// `InvalidInput`), or change who sees it: `PlaylistInfo`. `None`
    /// leaves each as it is. Its owner's, or any admin's for a
    /// server-wide one; anyone else who sees it gets `PlaylistReadOnly`.
    /// Whoever no longer sees it is sent `PlaylistGone`, and loses its pin
    /// with a note.
    UpdatePlaylist {
        id: String,
        #[serde(deserialize_with = "spoolframe_types::required::option")]
        name: Option<String>,
        #[serde(deserialize_with = "spoolframe_types::required::option")]
        kinds: Option<Kinds>,
        #[serde(deserialize_with = "spoolframe_types::required::option")]
        visibility: Option<Visibility>,
    },
    /// Change a playlist's entries, as seen at `revision`: `PlaylistEdited`
    /// with its new revision. 1 to 100 edits, applied in order, all or
    /// none. Made on an older revision, the edits are redone on the newer
    /// one unless one removes or moves an entry changed since:
    /// `PlaylistConflict` ("Someone changed this playlist; reload"). An
    /// item of a kind the playlist does not take is `KindNotAllowed`. A
    /// read-only share gets `PlaylistReadOnly`.
    EditPlaylist {
        id: String,
        revision: u64,
        edits: Vec<PlaylistEdit>,
    },
    /// Delete a playlist: `Done`. Its owner's, or any admin's for a
    /// server-wide one. Whoever had it pinned gets a note.
    DeletePlaylist {
        id: String,
    },
    /// Pin a playlist the session user sees to their Pinned at `position`
    /// (0 first; past the end, last), or move it there: `Done`. Every
    /// device of theirs is sent `PinsChanged`.
    PinPlaylist {
        id: String,
        position: u32,
    },
    /// Take a playlist out of their Pinned: `Done`, also when it was not
    /// there.
    UnpinPlaylist {
        id: String,
    },
    /// Start or go on playing a playlist: `PlaylistItem`, the entry to
    /// play, or `PlaylistFinished` when nothing is left to play. With
    /// `from`, that entry (or the next playable one after it); without,
    /// the user's place: the entry they were in, if they had not finished
    /// it, else the one after it; with no place, the top. The settings are
    /// kept for the user and playlist, for `NextInPlaylist` and the next
    /// time. Unavailable entries, and with `skip_watched` watched ones,
    /// are passed. The client plays the item itself (`StartPlayback`),
    /// skipping its credits, and records progress as any playback does.
    PlayPlaylist {
        id: String,
        #[serde(deserialize_with = "spoolframe_types::required::option")]
        from: Option<crate::playlists::EntryId>,
        shuffle: bool,
        repeat: crate::playlists::Repeat,
        skip_watched: bool,
    },
    /// What plays after entry `after` ended or was skipped:
    /// `PlaylistItem`, or `PlaylistFinished` at the end with repeat off,
    /// which clears the place so the next `PlayPlaylist` starts at the
    /// top. The server keeps the place.
    NextInPlaylist {
        id: String,
        after: crate::playlists::EntryId,
    },
    /// What plays next, without playing it (`spoolframe:playlist-upcoming`):
    /// `PlaylistUpcoming`, at most `limit` (100) entries after the session
    /// user's place, each the one `NextInPlaylist` would give after the
    /// one before. The round's order while shuffling, else the playlist's;
    /// from the start again with repeat all; with repeat one, the place's
    /// entry alone. With no place, from the top. Unavailable entries, and
    /// with `skip_watched` watched ones, are passed. Whoever sees the
    /// playlist. Only from a client that listed the feature; from any
    /// other, `Unsupported`.
    UpcomingInPlaylist {
        id: String,
        limit: u32,
    },
    /// Keep how the session user plays a playlist, playing nothing
    /// (`spoolframe:playlist-upcoming`): `Done`. The place and the shuffled
    /// round stay. Shuffle turned on shuffles anew, from the place, so the
    /// whole playlist is ahead; turned off, the playlist's order, from the
    /// place. Whoever sees the playlist. Only from a client that listed
    /// the feature; from any other, `Unsupported`.
    SetPlaylistSettings {
        id: String,
        shuffle: bool,
        repeat: crate::playlists::Repeat,
        skip_watched: bool,
    },
    /// Import the session user's Plex playlists, once (PX-40): each video
    /// or music playlist of their linked Plex account, on every Plex
    /// server here, becomes a personal playlist of theirs, its items
    /// mapped to this server's: `PlexImport`, what was made and what did
    /// not map. Nothing is synced afterwards. With `dry_run`, nothing is
    /// made. Without a linked Plex account, `NotFound`; a portable user
    /// has none (`NotForPortableUsers`). Imported twice, made twice.
    ImportPlexPlaylists {
        dry_run: bool,
    },
    /// Forget the notes `ListPlaylists` gave: a client showed them.
    /// `Done`. Notes are per user, not per device.
    DismissPinNotes,
    /// Like tracks or albums (PX-43): their tracks go to the top of the
    /// session user's Liked, in the order given, an album's in disc and
    /// track order; a track liked already stays where it is. An album
    /// likes the tracks it has now: one added to it later is not liked.
    /// `PlaylistInfo`, Liked as it is now. 1 to 100 items; anything but
    /// a track or an album is `KindNotAllowed`, an item not here
    /// `NotFound`. Liked again, nothing changes. Only from a client that
    /// listed `spoolframe:liked`; from any other, `Unsupported`.
    Like {
        items: Vec<MediaId>,
    },
    /// Take tracks, or every track of albums, out of the session user's
    /// Liked (PX-43): `PlaylistInfo`, Liked as it is now. As `Like`
    /// takes them; one not liked is no change.
    Unlike {
        items: Vec<MediaId>,
    },
    /// Whom playlist `id` can be shared with: `ShareCandidates`, a page
    /// from `offset`, at most `limit` (200), by display name. Every
    /// enabled account here and every portable user granted access, but
    /// the session user; with a `query`, only those whose display name or
    /// username holds it, in any case (empty: everyone). Only whoever
    /// manages the playlist asks: anyone else who sees it gets
    /// `PlaylistReadOnly`, one who does not `NotFound`. Only from a client
    /// that listed `spoolframe:playlist-sharing`; from any other,
    /// `Unsupported`.
    ListShareCandidates {
        id: String,
        query: String,
        offset: u32,
        limit: u32,
    },
    /// What the session user played lately (PX-52): `Items`, each item
    /// once, by its latest counted play, newest first; a page from
    /// `offset`, at most `limit` (500), and how many in all. `kinds` keeps
    /// films, episodes or tracks only; `None`, every kind. An item gone
    /// from the library is left out of the page. Each item's
    /// `PlaybackState` has its `play_count` and `last_played_at`. Only
    /// from a client that listed `spoolframe:play-history`; from any
    /// other, `Unsupported`.
    RecentlyPlayed {
        kinds: Option<crate::playlists::Kinds>,
        offset: u32,
        limit: u32,
    },
    /// Take away a playlist's uploaded cover (PX-53): `PlaylistInfo`, its
    /// `artwork` the mosaic again. Whoever may edit its entries may, as
    /// they may upload one (`SetPlaylistArt`, a resource request); anyone
    /// else who sees it, and anyone at all for Liked and All, gets
    /// `NotAllowed`. One without a cover is no change. Only from a client
    /// that listed `spoolframe:playlist-art`; from any other,
    /// `Unsupported`.
    RemovePlaylistArt {
        id: String,
    },
    /// Sync item `id`'s subtitle `track` (its place in `Tracks`'
    /// `subtitles`) to the file (PX-03), in the background: an external
    /// text subtitle (SRT, WebVTT, ASS) only. `SubtitleTiming`, its state
    /// `Syncing` (a track already syncing is left to it); the outcome comes
    /// as `SubtitleTimingChanged`, or in `Tracks` asked again. The track
    /// is served as it was until the sync is done, then synced; a sync
    /// that scores too low is not used (`Uncertain`). Anyone who can play
    /// the item may. Another track is `InvalidInput`, a track the item
    /// does not have `NotFound`. Only from a client that listed
    /// `spoolframe:subtitle-sync`; from any other, `Unsupported`.
    SyncSubtitle {
        id: MediaId,
        track: u32,
    },
    /// Serve item `id`'s subtitle `track` with its original timing again,
    /// for everyone (PX-03): the sync is set aside, the offset kept.
    /// `SubtitleTiming`. As `SyncSubtitle` for who may and which tracks.
    UseOriginalSubtitleTiming {
        id: MediaId,
        track: u32,
    },
    /// Move item `id`'s subtitle `track` by `offset_ms` (positive: later)
    /// on top of its timing, for everyone (PX-03); 0 resets it. At most
    /// ten minutes either way (`InvalidInput`). The track records who set
    /// it and when. `SubtitleTiming`. As `SyncSubtitle` for who may and
    /// which tracks.
    SetSubtitleOffset {
        id: MediaId,
        track: u32,
        offset_ms: i64,
    },
    /// Search the subtitle sites the admin turned on (OpenSubtitles,
    /// SubDL, or both) for item `id`'s subtitles in `language` (an ISO
    /// 639-1 code such as `en`, or `pt-BR`), or, with none, in the
    /// languages the server offers first (PX-02). Both sites are asked at
    /// once and their results are one list, `SubtitleResults`, best
    /// first: those found by the file's hash (made for this release), then
    /// the rest by rating and downloads, machine-translated and AI-made
    /// after people's, ones removed before as bad last; its `sources` say
    /// how each site's part went. Kept a day per site, item and language.
    /// Anyone who can play the item may. `NoSubtitleSource` when the
    /// server has none set up. When every site failed, the first one's
    /// error: `Unavailable` when it cannot be reached, `RateLimited` when
    /// it asks to wait, `QuotaExhausted` when it is out. Only from a client
    /// that listed `spoolframe:subtitle-search`; from any other,
    /// `Unsupported`.
    SearchSubtitles {
        id: MediaId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
    },
    /// Download result `result` (`SubtitleResult::id`) of a search for
    /// item `id` (PX-02), from the site that found it: the
    /// item gains an external subtitle track for everyone, usable at once
    /// (`SubtitleDownloaded`). One not found by the file's hash is synced
    /// in the background (`spoolframe:subtitle-sync`). The same result
    /// downloaded again gives the same track, at no cost. `NotFound` for a
    /// `result` no search of the item listed lately (search again);
    /// `NotAllowed` for a user the admin took downloads from;
    /// `QuotaExhausted` when the server's account or the user's own daily
    /// limit is used up; `InvalidInput` for a file that is not a subtitle
    /// the server reads. Otherwise as `SearchSubtitles`.
    DownloadSubtitle {
        id: MediaId,
        result: String,
    },
    /// Remove item `id`'s downloaded subtitle `download`
    /// (`SubtitleDownload::id`), for everyone (PX-02): its files go, and
    /// the result is listed last in the item's searches, marked rejected.
    /// The one who downloaded it, or an admin, may: `NotAllowed` for
    /// anyone else. `Done`; a download already removed is `Done` too.
    RemoveSubtitleDownload {
        id: MediaId,
        download: String,
    },

    /// What the device may download of film or episode `id` (PX-38,
    /// `spoolframe:downloads`): `DownloadOptions`, its tracks, the choice
    /// to show first, the profiles, what original quality would be, and
    /// the subtitles a download would leave out. Anyone who can play the
    /// item may ask. Only from an iPhone, iPad, Mac or Linux client
    /// (`Hello.platform`); from any other, and from a client that did not
    /// list the feature, `Unsupported`. With `spoolframe:music-downloads`
    /// too, `id` may be a track (PX-51): its one audio track, no
    /// profiles, and what `original` would be.
    DownloadOptions {
        id: MediaId,
    },
    /// Prepare a download of film or episode `id` from `source` with
    /// `tracks` (PX-38): `Download`, its id and state. The same user's
    /// same request is the same id, ready at once when its file is; one
    /// that failed is tried again. The file is made in the background;
    /// `DownloadChanged` tells its state as it moves. A track the item
    /// does not have, or a source or choice this build does not know, is
    /// `InvalidInput`; a profile not there `NotFound`. As
    /// `DownloadOptions` for who may and from where. A track (PX-51, with
    /// `spoolframe:music-downloads`) takes `original` or `aac256`, and
    /// `tracks` is not read; a film or an episode is refused `aac256`.
    PrepareDownload {
        id: MediaId,
        source: crate::downloads::DownloadSource,
        tracks: crate::downloads::DownloadTracks,
    },
    /// The session user's downloads `ids` (PX-38): `Downloads`, those
    /// they have in order, and in `gone` each id they have not (never
    /// theirs, cancelled, or no longer kept), so one such id never fails
    /// the rest; at most [`crate::downloads::MAX_ASKED`] ids.
    DownloadStatus {
        ids: Vec<String>,
    },
    /// Cancel download `id` (PX-38), as the device also does once it has
    /// the file whole and its SHA-256 checked: `Done`. Its file goes once
    /// no download names it and no one reads it; one being made stops. An
    /// id that is not the session user's, or was cancelled, is `NotFound`.
    CancelDownload {
        id: String,
    },
    /// Where the device got to in films and episodes while it had no
    /// server (PX-38, `spoolframe:downloads`): `OfflineWatchSynced`, an
    /// outcome per entry, in order. `device_now` is the device's clock
    /// now, in Unix seconds: each entry's `at` is moved by the difference
    /// to the server's clock, and kept no later than now. An entry is
    /// written only when it is newer than what the server has (newest
    /// wins, whichever device it came from); watched is judged by the
    /// server's rules from the position and the item's credits, or taken
    /// from `watched: true`. At most [`crate::downloads::MAX_ASKED`]
    /// entries; one with neither a position nor `watched` is
    /// `InvalidInput`. As `DownloadOptions` for from where.
    SyncOfflineWatch {
        device_now: i64,
        entries: Vec<crate::downloads::OfflineWatch>,
    },
    /// What the music downloads a device follows hold now (PX-51,
    /// `spoolframe:music-downloads` with `spoolframe:downloads`):
    /// `MusicDownloadChanges`, one change per artist, per playlist, then
    /// Liked, in that order, and a cursor to ask with next time. With
    /// `since`, the cursor of the last answer, each thing that holds the
    /// same tracks is `unchanged`; else, and with no cursor, its tracks
    /// now, in full, for the device to compare with what it keeps. A
    /// cursor this server cannot read is no cursor. Asked each time the
    /// device reaches the server. At most
    /// [`crate::downloads::MAX_FOLLOWED`] artists and playlists together;
    /// an id that is not an artist, or All, is `InvalidInput`; Liked
    /// needs `spoolframe:liked` too (`Unsupported` without). Only the
    /// tracks the user may see. As `DownloadOptions` for from where.
    MusicDownloadChanges {
        artists: Vec<MediaId>,
        playlists: Vec<String>,
        liked: bool,
        #[serde(deserialize_with = "spoolframe_types::required::option")]
        since: Option<String>,
    },
    /// What the device played and liked while it had no server (PX-51,
    /// `spoolframe:music-downloads`): `OfflinePlaysSynced`, an outcome per
    /// play and per like, in order. `device_now` is the device's clock
    /// now, in Unix seconds: each `at` is moved by the difference to the
    /// server's clock, and kept no later than now. Each play of a track is
    /// recorded once, counted as the server's rules say from
    /// `played_ms`: it is in play counts and last played. A like or an
    /// unlike is made only when newer than the track's last change on the
    /// server (newest wins, whichever device made it); likes need
    /// `spoolframe:liked` too (`Unsupported` without). At most
    /// [`crate::downloads::MAX_ASKED`] plays and likes together. As
    /// `DownloadOptions` for from where.
    SyncOfflinePlays {
        device_now: i64,
        plays: Vec<crate::downloads::OfflinePlay>,
        liked: Vec<crate::downloads::OfflineLike>,
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
                | Request::BeginPortableSignIn
                | Request::FinishPortableSignIn(_)
                | Request::RedeemPortableInvite(_)
                | Request::IdentityRecord { .. }
                | Request::IdentityRecordByHandle { .. }
                | Request::OfferPairing(_)
                | Request::FetchPairing { .. }
                | Request::AnswerPairing { .. }
                | Request::CollectPairing { .. }
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
    /// The experimental features this client wants (PX-49), by name
    /// ([`crate::experimental`]); the server's answer names those it
    /// turned on. Left out when empty, so a client that wants none sends
    /// what an older one did; an older server ignores it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub experimental: Vec<String>,
    /// Every regular feature this client is built with (PX-16), by name
    /// ([`crate::features`]). The server uses only what both list.
    // compat(v1): required at the protocol reset
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
}

/// `text` cut to [`MAX_HELLO_TEXT`] characters: what a server keeps of a
/// device's strings, and a device of an invite link's name.
pub(crate) fn cut(text: &mut String) {
    if let Some((at, _)) = text.char_indices().nth(MAX_HELLO_TEXT) {
        text.truncate(at);
    }
}

impl Hello {
    /// This `Hello` with each string cut to [`MAX_HELLO_TEXT`] characters:
    /// what a server keeps of it.
    pub fn within_bounds(mut self) -> Self {
        cut(&mut self.client_name);
        cut(&mut self.client_version);
        if let Some(name) = &mut self.device_name {
            cut(name);
        }
        self.experimental.truncate(crate::experimental::MAX_ASKED);
        for name in &mut self.experimental {
            cut(name);
        }
        self.features.truncate(crate::features::MAX_LISTED);
        for name in &mut self.features {
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

/// The most identity states a portable sign-in carries.
pub const MAX_SIGN_IN_STATES: usize = 2;

/// A portable user's device proving itself (`FinishPortableSignIn`,
/// P7-06): the credential their root gave the device's key, and that
/// key's signature over this server's nonce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableSignIn {
    pub credential: UserDeviceCredential,
    /// The device key's signature over
    /// [`crate::portable::device_proof_message`] of the server's endpoint
    /// id and the nonce `BeginPortableSignIn` gave.
    #[serde(with = "signature_hex")]
    pub proof: ed25519_dalek::Signature,
    /// The identity's states the device has, newest last, at most
    /// [`MAX_SIGN_IN_STATES`]: the server takes them in before it checks
    /// the credential (§48), so a device carries revocations to servers
    /// it signs in to.
    pub states: Vec<PortableIdentityState>,
    /// What they would be called here, for a grant made with this
    /// sign-in (P7-05's invite); a grant there already keeps its name.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub display_name: Option<String>,
}

/// A `portable-account` invite's code and the portable user redeeming it
/// (`RedeemPortableInvite`, P7-05).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedeemPortableInvite {
    pub code: String,
    pub sign_in: PortableSignIn,
}

impl fmt::Debug for RedeemPortableInvite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedeemPortableInvite")
            .field("code", &"<redacted>")
            .field("sign_in", &self.sign_in)
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

/// The session user's password and the one to replace it with
/// (`ChangePassword`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangePassword {
    pub current_password: String,
    pub new_password: String,
}

impl fmt::Debug for ChangePassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ChangePassword { <redacted> }")
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
        /// The password was a one-time one an admin reset to: show "Choose
        /// a new password" and send `ChangePassword` (its current password
        /// is the one-time one). Until then the session gets
        /// `PasswordChangeRequired` for anything else but `Logout` and
        /// `WhoAmI`, and the device is not trusted.
        // compat(v1): required at the protocol reset
        #[serde(default)]
        password_change_required: bool,
        /// Every regular feature this server has (PX-16,
        /// [`crate::features`]), pre-auth or not.
        // compat(v1): required at the protocol reset
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        features: Vec<String>,
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
        /// The playlists the user sees that hold it (PX-40): a movie,
        /// episode or track itself; any of a season's or a show's
        /// episodes; any of an album's or an artist's tracks. Only for a
        /// client that listed `spoolframe:playlists`; left out otherwise.
        // compat(v1): required at the protocol reset
        #[serde(default, skip_serializing_if = "Option::is_none")]
        playlists: Option<Vec<PlaylistRef>>,
        /// Its heart (PX-43): a track's `all` or `none`; an album's
        /// `all`, `some` or `none` by the tracks it has now. `None` for
        /// any other item, and for a client without `spoolframe:liked`.
        // compat(v1): required at the protocol reset
        #[serde(default, skip_serializing_if = "Option::is_none")]
        liked: Option<crate::playlists::Liked>,
        /// An album's tracks among `children` that are liked (PX-43),
        /// for their hearts. `None` as `liked` is.
        // compat(v1): required at the protocol reset
        #[serde(default, skip_serializing_if = "Option::is_none")]
        liked_children: Option<Vec<MediaId>>,
    },
    Search(SearchResults),
    Tracks(MediaTracks),
    WhoAmI(WhoAmIReply),
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
    /// The playback's new plan after `ChangeSubtitle` or `ChangeQuality`:
    /// read `epoch` (0 for direct play), whose size is `size` (0 while a
    /// conversion writes it).
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
    /// What a portable identity signs to tie the session user's account
    /// to it (`TieBegin`, P7-04).
    TieChallenge {
        /// Used once, for `expires_in_secs`, by this session.
        #[serde(with = "serde_bytes")]
        nonce: [u8; 32],
        /// This server's endpoint id, as the tie message names it. A
        /// client signs with the id it dialled, never this one: the field
        /// says which server meant to ask.
        #[serde(with = "serde_bytes")]
        server: [u8; 32],
        expires_in_secs: u64,
    },
    /// What a portable user's device signs to sign in
    /// (`BeginPortableSignIn`, P7-06).
    PortableSignInBegun {
        /// Used once, for `expires_in_secs`, by this device.
        #[serde(with = "serde_bytes")]
        nonce: [u8; 32],
        expires_in_secs: u64,
    },
    /// A new invite, with its code (`CreateInvite`, PX-44).
    InviteCreated(InviteCreated),
    /// What the server keeps of a portable identity (`IdentityRecord`,
    /// P7-08).
    IdentityRecord(IdentityRecord),
    /// The pairing request held under a code (`FetchPairing`, P7-08).
    PairingOffered(PairingOffer),
    /// The answer to a device's pairing request, or `None` while none
    /// came yet (`CollectPairing`, P7-08).
    PairingAnswered {
        answer: Option<PairingAnswer>,
    },
    /// The invites that can still be redeemed (`ListInvites`, PX-44).
    Invites {
        invites: Vec<InviteSummary>,
    },
    /// Experimental (`http-playback`, PX-49): where a player with no Iroh
    /// fetches the playback (`HttpPlaybackUrl`).
    HttpPlaybackUrl {
        url: String,
    },
    /// Experimental (`http-playback`, PX-49): why `HttpPlaybackUrl` gave
    /// no URL.
    HttpPlaybackRefused {
        reason: crate::experimental::HttpPlaybackRefusal,
    },
    /// The playlists the user sees (`ListPlaylists`, PX-40), oldest first,
    /// and the notes of pinned ones that left their Pinned.
    Playlists {
        playlists: Vec<PlaylistSummary>,
        pin_notes: Vec<PinNote>,
    },
    /// One playlist and a page of its entries (`GetPlaylist`).
    Playlist(PlaylistPage),
    /// A playlist as it is now (`CreatePlaylist`, `UpdatePlaylist`).
    PlaylistInfo(PlaylistSummary),
    /// A subtitle track's timing after `SyncSubtitle`,
    /// `UseOriginalSubtitleTiming` or `SetSubtitleOffset` (PX-03).
    SubtitleTiming(spoolframe_types::SubtitleTiming),
    /// What `SearchSubtitles` found (PX-02).
    SubtitleResults(spoolframe_types::SubtitleSearch),
    /// The track `DownloadSubtitle` added (PX-02).
    SubtitleDownloaded(spoolframe_types::SubtitleDownloaded),
    /// The playlist's revision after `EditPlaylist`.
    PlaylistEdited {
        revision: u64,
    },
    /// The entry to play now (`PlayPlaylist`, `NextInPlaylist`).
    PlaylistItem(crate::playlists::PlaylistNext),
    /// Nothing is left to play: the end, with repeat off, or no entry can
    /// be played. The user's place is cleared.
    PlaylistFinished,
    /// What plays next in a playlist (`UpcomingInPlaylist`), in turn.
    PlaylistUpcoming {
        entries: Vec<crate::playlists::PlaylistEntry>,
    },
    /// What importing Plex playlists made, or would (`ImportPlexPlaylists`).
    PlexImport(crate::playlists::PlexImport),
    /// Whom a playlist can be shared with (`ListShareCandidates`).
    ShareCandidates(crate::playlists::ShareCandidatePage),
    /// What the device may download of an item (`DownloadOptions`).
    DownloadOptions(crate::downloads::DownloadOptionsInfo),
    /// A download prepared (`PrepareDownload`).
    Download(crate::downloads::DownloadInfo),
    /// Downloads, in the order asked (`DownloadStatus`): each the user
    /// has, and the ids asked for that they have not. A device treats a
    /// gone download as cancelled: preparing it again brings it back.
    Downloads {
        downloads: Vec<crate::downloads::DownloadInfo>,
        /// Left out when empty.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        gone: Vec<String>,
    },
    /// What became of each offline entry, in order (`SyncOfflineWatch`).
    OfflineWatchSynced {
        outcomes: Vec<crate::downloads::OfflineWatchOutcome>,
    },
    /// What each followed thing holds (`MusicDownloadChanges`), and the
    /// cursor to ask with next.
    MusicDownloadChanges {
        changes: Vec<crate::downloads::FollowedChange>,
        cursor: String,
    },
    /// What became of each offline play and like, in order
    /// (`SyncOfflinePlays`).
    OfflinePlaysSynced {
        plays: Vec<crate::downloads::OfflineWatchOutcome>,
        liked: Vec<crate::downloads::OfflineWatchOutcome>,
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
    /// The portable identities the account is tied to (P7-04, P7-09),
    /// the oldest tie first: shown on the account, never as other
    /// accounts. Empty for an account with none, and for a portable
    /// user's own session (P7-06), whose identity is its `user_id`.
    pub portable: Vec<PortableTie>,
}

/// One of an account's ties to a portable identity (P7-09).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableTie {
    pub id: PortableUserId,
    /// Unix seconds.
    pub tied_at: i64,
    /// Unix seconds of the identity's last sign-in into the account with
    /// its credential; `None` until one.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub last_used_at: Option<i64>,
}

/// What `WhoAmI` answers: the session's user, and whether they may leave
/// the server. On the wire the user's fields and `can_leave` sit in one
/// map, as the user's alone did before, so an older client reads it as a
/// [`UserSummary`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhoAmIReply {
    #[serde(flatten)]
    pub user: UserSummary,
    /// `LeaveServer` would be done, not refused with `LastAdmin` (PX-47):
    /// false for the last enabled admin, or the last enabled admin with a
    /// password, so an app can grey out "Leave this server" before it is
    /// asked. As of this answer: an admin made or demoted later changes
    /// it. Left out by an older server, true; its refusal still stands.
    // compat(v1): required at the protocol reset
    #[serde(default = "WhoAmIReply::can_leave_by_default")]
    pub can_leave: bool,
    /// Every regular feature this server has (PX-16), as `LoggedIn` gives
    /// them: for a client holding a session from before, or from before
    /// the server was upgraded.
    // compat(v1): required at the protocol reset
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
}

impl WhoAmIReply {
    /// An older server says nothing about leaving: let the app offer it,
    /// and the server's `LastAdmin` refuse it if it must.
    fn can_leave_by_default() -> bool {
        true
    }
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
    /// The experimental features turned on for this connection (PX-49):
    /// those its `Hello` named that this server knows and has on. Left
    /// out when none, as an older server does.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub experimental: Vec<String>,
    /// The pre-auth regular features this server has (PX-16,
    /// [`crate::features`]); the rest come with a session (`LoggedIn`,
    /// `WhoAmI`). Left out when none.
    // compat(v1): required at the protocol reset
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
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
    /// The server plays at most `max` playbacks off its LAN at once
    /// (PX-46, `network.uplink.max_streams`), and that many are playing.
    /// Try again when one ends; a device on the server's LAN is not
    /// counted.
    #[error("the server plays at most {max} playbacks outside its home at once")]
    WanStreamsFull { max: u32 },
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
    /// The invite's code is right, and it is of another kind than the
    /// request redeems (P7-05): a `portable-account` invite's code to
    /// `RedeemInvite`, or a `create-user` one's to
    /// `RedeemPortableInvite`. Nothing was made and the invite keeps its
    /// use: redeem it the other way.
    #[error("that invite is of another kind")]
    WrongInviteKind,
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
    /// `TieFinish` with a nonce that is not outstanding for this session:
    /// it was used, it is over, or `TieBegin` never gave it here (P7-04).
    /// Begin again.
    #[error("the tie challenge is used or has expired; begin again")]
    TieChallengeExpired,
    /// Another account on this server is tied to that portable identity
    /// (P7-04): one identity ties to one account per server. Sever that
    /// tie first. Nothing was changed.
    #[error("that portable identity is tied to another account here")]
    PortableIdTaken,
    /// The session is a portable user's (P7-06), and this is for an
    /// account here only: its password, PIN, passkeys, devices, sessions,
    /// ties, approving sign-ins. Hide what asks for it while signed in as
    /// one. Nothing was changed.
    #[error("not for a guest from another identity")]
    NotForPortableUsers,
    /// The request is an admin's (PX-44: making and revoking invites),
    /// and the session user is not one. Nothing was changed.
    #[error("only an admin can do that")]
    AdminRequired,
    /// The session was opened with a one-time password an admin reset to:
    /// it may only `ChangePassword` (or `Logout`, `WhoAmI`) until the user
    /// chooses a new password. Nothing was done.
    #[error("choose a new password first")]
    PasswordChangeRequired,
    /// `LeaveServer` from the last enabled admin, or the last enabled
    /// admin with a password (PX-47): make someone else an admin, or give
    /// one a password, first. Nothing was changed.
    #[error("you are the only admin; make someone else an admin first")]
    LastAdmin,
    /// The user sees the playlist and may not make this change (PX-40): an
    /// edit on a read-only share, or a rename, sharing change or delete by
    /// anyone but its owner (or an admin, for a server-wide one). Nothing
    /// was changed.
    #[error("you cannot change this playlist")]
    PlaylistReadOnly,
    /// The playlist does not take items of this kind (PX-40): its kinds
    /// were ticked when it was made. Nothing was changed. A season or
    /// show is episodes, an album or artist tracks.
    #[error("this playlist does not take a {kind:?}")]
    KindNotAllowed { kind: crate::playlists::ItemKind },
    /// Someone changed the playlist since the revision the edit was made
    /// on, and touched an entry the edit touches (PX-40): "Someone changed
    /// this playlist; reload". `revision` is its revision now. Nothing was
    /// changed.
    #[error("someone changed this playlist; reload (revision {revision})")]
    PlaylistConflict { revision: u64 },
    /// The user may not do this to what they see (PX-53): a cover set or
    /// taken away by someone who may not edit the playlist, or on Liked
    /// or All, whose tiles are fixed. Nothing was changed.
    #[error("you are not allowed to do that")]
    NotAllowed,
    /// The picture is over `max_bytes` long, or over `max_pixels` by its
    /// header (PX-53). Refused before it was decoded; nothing was
    /// changed.
    #[error("the picture is too large: at most {max_bytes} bytes and {max_pixels} pixels")]
    ImageTooLarge { max_bytes: u64, max_pixels: u64 },
    /// Not a JPEG, PNG, WebP or HEIC picture the server can decode, or
    /// one that hit the decoder's limits (PX-53). Nothing was changed.
    #[error("the picture cannot be read: a JPEG, PNG, WebP or HEIC is taken")]
    ImageUnreadable,
    /// The downloads of `whose` are used up for today (PX-02): the
    /// server's account at subtitle site `site` (`opensubtitles`,
    /// `subdl`, as `SubtitleResult::source`), or the user's own daily
    /// limit on this server (no `site`). `resets_at` (Unix seconds) is when more may be
    /// downloaded, when known. Nothing was downloaded; searching still
    /// works.
    #[error("no subtitle downloads left today ({whose:?})")]
    QuotaExhausted {
        whose: spoolframe_types::QuotaOf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resets_at: Option<i64>,
        // compat(v1): required at the protocol reset
        #[serde(default, skip_serializing_if = "Option::is_none")]
        site: Option<String>,
    },
    /// The server has no subtitle site set up (PX-02): hide "Find
    /// subtitles". Nothing was asked of any site.
    #[error("this server has no subtitle source set up")]
    NoSubtitleSource,
    /// The download's file is not whole yet (PX-38): read it once its
    /// state is `ready`. A download that failed is prepared again.
    #[error("the download is not ready")]
    DownloadNotReady,
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
        /// The URLs of the iroh relays the server may use
        /// (`network.relays`), its home relay now first; empty while
        /// relays are off.
        relays: Vec<String>,
        /// The pkarr relay the server publishes where it is to
        /// (`network.pkarr`, P6-05), for the device to look it up by its
        /// key when nothing else reaches it; `None` while it publishes
        /// nothing.
        #[serde(deserialize_with = "spoolframe_types::required::option")]
        lookup: Option<String>,
    },
    /// Playback `handle`'s share of the server's WAN uplink changed
    /// (PX-46): another WAN playback started or ended, or the budget
    /// changed. A playback in `Quality::Auto` re-plans with
    /// `ChangeQuality` at its position (the SDK's managed playback does
    /// it by itself); `max_bitrate` is the new cap, `None` none. Sent only
    /// for a move of 15% or more, a few seconds after the last start or
    /// end. One of another quality may ignore it.
    PlaybackShare {
        handle: PlaybackHandle,
        #[serde(deserialize_with = "spoolframe_types::required::option")]
        max_bitrate: Option<u64>,
    },
    /// A playlist the user sees was made or changed (PX-40): its entries,
    /// name, kinds or sharing. Read it again if it is shown. Only to a
    /// client that listed `spoolframe:playlists`.
    PlaylistChanged { id: String, revision: u64 },
    /// A playlist the user saw is gone to them: deleted, or no longer
    /// shared with them. Close it if it is shown.
    PlaylistGone { id: String },
    /// The user's Pinned, or its notes, changed on another device or by
    /// another user's change: `ListPlaylists` again.
    PinsChanged,
    /// The user liked or unliked a track (PX-51), on any device: a device
    /// that downloaded Liked asks what it holds now
    /// (`MusicDownloadChanges`). Only to the user's own connections that
    /// listed `spoolframe:music-downloads`.
    LikedChanged,
    /// Item `id`'s subtitle `track` is timed anew (PX-03): synced, its
    /// sync done or set aside, or its offset changed, by anyone. What it
    /// is served with changed: a player showing it reads it again (a
    /// `ReadSubtitle` in progress answers `SubtitleChanged`), with no
    /// restart. Only to a client that listed `spoolframe:subtitle-sync`,
    /// on a connection that asked for the item's `Tracks` or plays it.
    SubtitleTimingChanged {
        id: MediaId,
        track: u32,
        timing: spoolframe_types::SubtitleTiming,
    },
    /// Download `id`'s state moved (PX-38): `waiting`, then `converting`
    /// (told once per percent or every 2 s), `writing`, and `ready` or
    /// `failed`. Only to the connections of the user whose it is, that
    /// listed `spoolframe:downloads`.
    DownloadChanged {
        id: String,
        state: crate::downloads::DownloadState,
    },
    /// An event type this build does not know; ignore it.
    #[serde(other)]
    Unknown,
}

/// Socket addresses on the wire as text (`192.168.1.10:4433`,
/// `[2001:db8::1]:4433`). CBOR is not a human-readable format to serde, so
/// a bare `SocketAddr` would go as serde's enum of byte tuples, which a
/// peer outside Rust would have to copy; text is also what JSON shows.
pub(crate) mod socket_addrs_as_text {
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
