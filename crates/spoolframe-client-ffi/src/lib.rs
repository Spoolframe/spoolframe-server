//! Spoolframe's client side for Swift (iOS, macOS, tvOS) and Kotlin
//! (Android), through UniFFI: `spoolframe-client-sdk` and the loopback
//! `spoolframe-hls-bridge` as exports. Kept thin; the SDK does the work.
//! The loopback bridge a URL player reads from is the `hls-bridge`
//! feature, on by default; without it (Android's Media3) a playback is
//! opened and its bytes read through the SDK.
//!
//! What crosses is typed: the SDK's, protocol's and types' own records
//! and enums (their `uniffi` feature), a few records of this crate's own
//! where UniFFI can't take the SDK's as they are ([`values`]), and one
//! error, [`SpoolError`], case for case `SdkError`'s, with its
//! [`SpoolError::kind`]. Nothing is passed as JSON.
//!
//! Events come as objects with an async `next()`, one per stream: the
//! foreign side loops over it in a task and stops when it returns `None`.
//!
//! Licensed as the SDK and the bridge are (MIT OR Apache-2.0), so that
//! proprietary apps may link it; it never depends on an AGPL crate.

uniffi::setup_scaffolding!();

#[cfg(all(feature = "android", target_os = "android"))]
mod android;
#[cfg(feature = "hls-bridge")]
mod bridge;
mod client;
mod describe;
mod downloads;
mod error;
mod events;
mod extras;
mod features;
mod invites;
mod paired;
mod pairing;
mod playback;
mod playlists;
mod portable;
mod reader;
mod resources;
mod root_sync;
mod runtime;
mod security_key;
mod session;
mod settings;
mod sign_in;
mod storage;
#[cfg(test)]
mod test_fixtures;
pub mod values;

pub use client::{
    DeviceSpec, PendingSignIn, PortableProfile, SavedProfileName, ServerWatch, SignOutWatch,
    SpoolClient, StateWatch,
};
pub use downloads::{DownloadNotice, DownloadRun, DownloadWatch};
pub use error::{PairingFailure, PhraseFailure, SpoolError};
pub use events::{
    LibrariesNotice, LibrariesWatch, PlaybackShareWatch, PlaylistNotice, PlaylistWatch,
    ProfileNotice, ProfileWatch, SubtitleTimingNotice, SubtitleTimingWatch, UplinkShare,
};
pub use features::{InvitePasskeysApi, NextEpisodesApi};
pub use pairing::{Connected, Paired, PairedServer, SpoolPairing, SpoolPairingOffer};
pub use playback::{PlaybackStart, PlaybackSubtitle, SpoolPlayback, SubtitleEntry};
pub use portable::{
    Guest, IdentityState, IssuedDevice, NewIdentity, PortableIdentity, RecoveryPhrase,
    RevokedDevice, SpoolPortable,
};
pub use resources::PlaybackResource;
pub use security_key::{AppSecurityKey, IdentityRoot, KeyAnswer, KeyFailure, KeyRegistration};
pub use session::{AccountTie, ApprovalLink, PictureSubtitles, SpoolSession, User};
pub use settings::{CodecPlayer, PlayerDecoders};
pub use storage::{CoreStorage, FileStorage, SecretCipher, StorageFailure};

use spoolframe_client_sdk as sdk;

/// This library's version, so the app can show which core it was built
/// with.
#[uniffi::export]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

/// `typed` as an invite's code, normalized (`QW7K2MFPXR4T`), whatever
/// case, dashes or spaces it was typed with. Nothing is sent.
/// `NotAnInviteCode` for anything that can't be one, a link included.
#[uniffi::export]
pub fn parse_invite_code(typed: String) -> Result<String, SpoolError> {
    Ok(sdk::parse_code(&typed)?)
}

/// The fingerprint of a server key (hex), as `spoolframe-server
/// fingerprint` prints it; for the "Is this your server?" and "identity
/// changed" sheets.
#[uniffi::export]
pub fn fingerprint(endpoint_id: String) -> Option<String> {
    sdk::fingerprint::of_hex(&endpoint_id)
}

/// A server's passkey relying party, `<server id>.spoolframe.app`, to
/// show under its fingerprint; `None` for an id that isn't one.
#[uniffi::export]
pub fn passkey_domain(server_id: String) -> Option<String> {
    server_id.parse::<sdk::ServerId>().ok().map(|id| id.rp_id())
}

/// What kind of client a platform is, as device and session lists show
/// it ("Apple TV app").
#[uniffi::export]
pub fn platform_label(platform: sdk::types::Platform) -> String {
    platform.label().to_owned()
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_the_crates() {
        assert_eq!(super::core_version(), env!("CARGO_PKG_VERSION"));
    }
}
