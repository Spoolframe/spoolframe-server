//! Every `SdkError` crosses as the case of the same name, with its text
//! and its kind: nothing flattened to a string, nothing classified twice.

use std::time::Duration;

use spoolframe_client_sdk::types::QuotaOf;
use spoolframe_client_sdk::{
    CredentialError, ErrorKind, ItemKind, PairingError, PairingRefused, PhraseError,
    RootSyncRefused, SdkError, SecurityKeyError, StateError, StorageError,
};

use super::*;

/// One of every `SdkError` variant. The `From` match has no wildcard, so
/// a new variant does not compile until it is mapped; add it here too.
fn every_variant() -> Vec<SdkError> {
    vec![
        SdkError::ServerRestarted,
        SdkError::OperationExpired,
        SdkError::OperationConflict,
        SdkError::PlaybackClosed,
        SdkError::PlaybackChanged,
        SdkError::Unavailable("the mount is down".into()),
        SdkError::StaleProgress,
        SdkError::RevisionConflict { revision: 7 },
        SdkError::Storage(StorageError("keychain locked".into())),
        SdkError::Unreachable("server".into()),
        SdkError::TimedOut("Hello".into()),
        SdkError::UnknownServer("server".into()),
        SdkError::NotPinned("server".into()),
        SdkError::IdentityChanged {
            saved: "a".into(),
            now: "b".into(),
            server_id: Some("abcd".into()),
        },
        SdkError::ServerIdChanged {
            endpoint_id: "a".into(),
            saved: "x".into(),
            now: "y".into(),
        },
        SdkError::UpdateRequired {
            server_protocol: Some(0),
        },
        SdkError::WrongServer {
            expected: "a".into(),
            actual: "b".into(),
        },
        SdkError::SignInRequired,
        SdkError::InvalidCredentials,
        SdkError::NotAnInvite,
        SdkError::NotAnInviteCode,
        SdkError::InviteInvalid,
        SdkError::InviteExpired,
        SdkError::InviteExhausted,
        SdkError::UsernameTaken,
        SdkError::RateLimited {
            retry_after: Duration::from_secs(30),
        },
        SdkError::CannotPlay {
            reasons: vec!["audio 0: dts".into()],
        },
        SdkError::PlaybackExpired,
        SdkError::PlaybackStopped,
        SdkError::NotFound("item".into()),
        SdkError::Busy,
        SdkError::WanStreamsFull { max: 2 },
        SdkError::ServerFull,
        SdkError::PlaybackFailed("ffmpeg".into()),
        SdkError::PlaybackRestarted { epoch: 3 },
        SdkError::PlaybackResumed {
            at: Duration::from_secs(90),
        },
        SdkError::Replanned,
        SdkError::ProfileLocked,
        SdkError::NoOfflinePin,
        SdkError::InvalidInput("a PIN is 4 to 8 digits".into()),
        SdkError::NoPassword,
        SdkError::SubtitleNotReady { done_percent: 40 },
        SdkError::PreviewsNotReady { done_percent: 10 },
        SdkError::NoPreviews,
        SdkError::SubtitleChanged,
        SdkError::NotSetUp,
        SdkError::RecentSignInRequired,
        SdkError::LastSignInMethod,
        SdkError::SignInDenied,
        SdkError::SignInExpired,
        SdkError::Phrase(PhraseError::UnknownWord { position: 23 }),
        SdkError::UnknownIdentity("ab".into()),
        SdkError::Credential(CredentialError::Expired),
        SdkError::IdentityState(StateError::BadSignature),
        SdkError::NotThisDevice,
        SdkError::DeviceKeyRevoked,
        SdkError::NotARoot,
        SdkError::RootSync(RootSyncRefused::NoSecurityKey),
        SdkError::SecurityKey(SecurityKeyError::WrongPin),
        SdkError::TieChallengeExpired,
        SdkError::PortableIdTaken,
        SdkError::WrongInviteKind,
        SdkError::NotForPortableUsers,
        SdkError::AdminRequired,
        SdkError::PasswordChangeRequired,
        SdkError::LastAdmin,
        SdkError::Unsupported {
            feature: "federation".into(),
        },
        SdkError::NotOnServer {
            request: "DeleteUser".into(),
        },
        SdkError::FeatureOff {
            feature: sdk::Feature::NextEpisodes,
        },
        SdkError::PlaylistReadOnly,
        SdkError::KindNotAllowed {
            kind: ItemKind::Movie,
        },
        SdkError::PlaylistConflict { revision: 9 },
        SdkError::NotAllowed,
        SdkError::ImageTooLarge {
            max_bytes: 1,
            max_pixels: 2,
        },
        SdkError::ImageUnreadable,
        SdkError::QuotaExhausted {
            whose: QuotaOf::Account,
            site: Some("subdl".into()),
            resets_at: Some(1_760_000_000),
        },
        SdkError::NoSubtitleSource,
        SdkError::Pairing(PairingError::Aborted("the phone went away".into())),
        SdkError::DownloadNotReady,
        SdkError::Downloads("the disk is full".into()),
        SdkError::PairingRefused {
            reason: PairingRefused::KeyHeld,
        },
        SdkError::OriginOffline,
        SdkError::Server("boom".into()),
        SdkError::Protocol("garbage".into()),
    ]
}

/// The variant's name, as `Debug` begins.
fn name(debug: String) -> String {
    debug
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect()
}

#[test]
fn every_variant_crosses_as_the_case_of_its_name() {
    for error in every_variant() {
        let crossed = SpoolError::from(error.clone());
        assert_eq!(
            name(format!("{crossed:?}")),
            name(format!("{error:?}")),
            "{error:?}"
        );
    }
}

#[test]
fn every_variant_keeps_the_sdk_s_text() {
    for error in every_variant() {
        assert_eq!(
            SpoolError::from(error.clone()).message(),
            error.to_string(),
            "{error:?}"
        );
    }
}

#[test]
fn every_variant_is_the_kind_the_sdk_says() {
    for error in every_variant() {
        assert_eq!(
            SpoolError::from(error.clone()).kind(),
            error.kind(),
            "{error:?}"
        );
    }
}

/// The PIN pad's reading, an invite's refusal and who updates are the
/// SDK's, case for case.
#[test]
fn every_variant_reads_as_the_sdk_reads_it() {
    for error in every_variant() {
        let crossed = SpoolError::from(error.clone());
        assert_eq!(
            (
                crossed.pin_failure(),
                crossed.invite_refusal(),
                crossed.who_updates()
            ),
            (
                sdk::PinFailure::of(&error),
                sdk::InviteRefusal::of_error(&error),
                error.who_updates()
            ),
            "{error:?}"
        );
    }
}

#[test]
fn fields_cross_as_values() {
    assert_eq!(
        SpoolError::from(SdkError::IdentityChanged {
            saved: "a".into(),
            now: "b".into(),
            server_id: Some("abcd".into()),
        }),
        SpoolError::IdentityChanged {
            saved: "a".into(),
            now: "b".into(),
            server_id: Some("abcd".into()),
        }
    );
    assert_eq!(
        SpoolError::from(SdkError::KindNotAllowed {
            kind: ItemKind::Track
        }),
        SpoolError::KindNotAllowed {
            kind: ItemKind::Track
        }
    );
    assert_eq!(
        SpoolError::from(SdkError::Phrase(PhraseError::WordCount { found: 12 })),
        SpoolError::Phrase {
            error: PhraseFailure::WordCount { found: 12 }
        }
    );
}

#[test]
fn a_security_key_cancel_is_its_own_case() {
    assert_eq!(
        SpoolError::from(SdkError::SecurityKey(SecurityKeyError::Cancelled)),
        SpoolError::SecurityKey {
            error: SecurityKeyError::Cancelled
        }
    );
}

#[test]
fn the_layer_s_own_cases_are_refused() {
    assert_eq!(SpoolError::Cancelled.kind(), ErrorKind::Refused);
}

#[cfg(feature = "hls-bridge")]
#[test]
fn a_bridge_that_cannot_start_is_refused() {
    let bridge = SpoolError::Bridge {
        message: "no socket".into(),
    };
    assert_eq!(bridge.kind(), ErrorKind::Refused);
}

#[test]
fn a_rate_limit_says_when_to_try_again() {
    let error = SpoolError::from(SdkError::RateLimited {
        retry_after: Duration::from_secs(30),
    });
    assert_eq!(
        error.kind(),
        ErrorKind::RateLimited {
            retry_after: Duration::from_secs(30)
        }
    );
}

/// The errors nested in a `SpoolError` say what to show, as it does.
#[test]
fn a_nested_error_s_message_is_its_text() {
    let phrase = PhraseFailure::WordCount { found: 12 };
    let pairing = PairingFailure::Aborted {
        message: "cut off".into(),
    };
    let key = SecurityKeyError::Locked;
    let refused = PairingRefused::InviteExpired;
    assert_eq!(
        [
            phrase.message(),
            pairing.message(),
            key.message(),
            refused.message()
        ],
        [
            phrase.to_string(),
            pairing.to_string(),
            key.to_string(),
            refused.to_string()
        ]
    );
}
