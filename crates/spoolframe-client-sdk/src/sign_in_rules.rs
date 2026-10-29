//! What a PIN pad, a password field and a device sign-in approval check
//! before asking the server, and how they read its answers: written once
//! here instead of in each app.

use std::net::IpAddr;
use std::time::Duration;

use spoolframe_protocol::{AddressNetwork, DeviceAddress};
use spoolframe_types::{password, pin};

use crate::SdkError;

/// The fewest digits a PIN has: a PIN pad's dots start here.
pub const PIN_MIN_DIGITS: u8 = pin::MIN_DIGITS as u8;
/// The most digits a PIN has: a PIN pad takes no more.
pub const PIN_MAX_DIGITS: u8 = pin::MAX_DIGITS as u8;
/// The fewest characters a new password has.
pub const PASSWORD_MIN: u32 = password::MIN_CHARS as u32;

/// A PIN as a user may set or type it: 4 to 8 digits ([`Pin::new`]).
#[derive(Clone, PartialEq, Eq)]
pub struct Pin(String);

impl std::fmt::Debug for Pin {
    // A PIN unlocks a profile: keep it out of logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pin(<redacted>)")
    }
}

/// Why typed text is not a PIN.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum PinRefusal {
    #[error("a PIN has at least {min} digits")]
    TooShort { min: u8 },
    #[error("a PIN has at most {max} digits")]
    TooLong { max: u8 },
    #[error("a PIN is digits only")]
    NotDigits,
}

impl Pin {
    /// `typed` as a PIN; why not, before anything is sent.
    pub fn new(typed: &str) -> Result<Self, PinRefusal> {
        if !typed.bytes().all(|b| b.is_ascii_digit()) {
            return Err(PinRefusal::NotDigits);
        }
        match pin::pin_digits(typed) {
            Some(_) => Ok(Self(typed.to_owned())),
            None if typed.len() < pin::MIN_DIGITS => Err(PinRefusal::TooShort {
                min: PIN_MIN_DIGITS,
            }),
            None => Err(PinRefusal::TooLong {
                max: PIN_MAX_DIGITS,
            }),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// How many digits it has: what a PIN pad of this length shows.
    pub fn digits(&self) -> u8 {
        self.0.len() as u8
    }
}

/// What a PIN pad says after a PIN was refused, online
/// ([`crate::Session::unlock`]) or offline
/// ([`crate::Downloads::unlock_offline`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum PinFailure {
    /// Not the PIN: clear the pad and ask again.
    Wrong,
    /// Too many wrong in a while: the pad waits `retry_after`.
    Wait { retry_after: Duration },
    /// Too many wrong in a row: the session here ended, or the downloads
    /// stay locked. Sign in again in full, or unlock online.
    SignInAgain,
    /// Offline, this profile's PIN was never used on this device: unlock
    /// online, or as the device's owner.
    NotKnownHere,
}

impl PinFailure {
    /// What `error`, from a PIN's check, means for the pad; `None` for an
    /// error that is not about the PIN (no answer, a session ended for
    /// another reason), shown as any other.
    pub fn of(error: &SdkError) -> Option<Self> {
        match error {
            SdkError::InvalidCredentials => Some(Self::Wrong),
            SdkError::RateLimited { retry_after } => Some(Self::Wait {
                retry_after: *retry_after,
            }),
            SdkError::SignInRequired => Some(Self::SignInAgain),
            SdkError::NoOfflinePin => Some(Self::NotKnownHere),
            _ => None,
        }
    }
}

/// Why a new password will not do, before anything is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum PasswordProblem {
    #[error("a password has at least {min} characters")]
    TooShort { min: u32 },
}

/// Why `typed` cannot be a new password, as the server counts it
/// (characters, not bytes); `None` when it can.
pub fn password_problem(typed: &str) -> Option<PasswordProblem> {
    (typed.chars().count() < password::MIN_CHARS)
        .then_some(PasswordProblem::TooShort { min: PASSWORD_MIN })
}

/// Where a device asking to be signed in is, as an approval shows it
/// ([`address_class`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum AddressClass {
    /// The server's own machine.
    Loopback,
    /// On the approver's network: "It asked from 192.168.86.20".
    Local,
    /// On the network, by a public IPv6 address in it: local, and
    /// reachable from anywhere too. Say both.
    LocalPublicV6,
    /// On the internet, not the approver's network: warn, approve only a
    /// device you can see. A code someone sent is theirs.
    Internet,
}

impl AddressClass {
    /// The approval warns before approving.
    pub fn warns(self) -> bool {
        self == Self::Internet
    }
}

/// How an approval shows `from`, the address a device's sign-in came from
/// ([`spoolframe_protocol::DeviceSignInRequest::requested_from`]). A kind
/// of network a newer server judges and this build does not know is
/// warned of as the internet is.
pub fn address_class(from: &DeviceAddress) -> AddressClass {
    match from.network {
        AddressNetwork::Loopback => AddressClass::Loopback,
        AddressNetwork::Local => match from.address.parse::<IpAddr>() {
            Ok(IpAddr::V6(v6)) if crate::home_network::global(v6) => AddressClass::LocalPublicV6,
            _ => AddressClass::Local,
        },
        AddressNetwork::Internet | AddressNetwork::Unknown => AddressClass::Internet,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pin_is_4_to_8_digits() {
        assert_eq!(Pin::new("1234").map(|p| p.digits()), Ok(4));
        assert_eq!(Pin::new("12345678").map(|p| p.digits()), Ok(8));
    }

    #[test]
    fn a_pin_too_short_or_too_long_says_so() {
        assert_eq!(Pin::new("123"), Err(PinRefusal::TooShort { min: 4 }));
        assert_eq!(Pin::new("123456789"), Err(PinRefusal::TooLong { max: 8 }));
    }

    #[test]
    fn a_pin_with_a_letter_is_not_one_whatever_its_length() {
        assert_eq!(Pin::new("12a"), Err(PinRefusal::NotDigits));
        assert_eq!(Pin::new("12 34"), Err(PinRefusal::NotDigits));
    }

    #[test]
    fn a_pin_is_kept_out_of_logs() {
        assert_eq!(
            format!("{:?}", Pin::new("1234").unwrap()),
            "Pin(<redacted>)"
        );
    }

    #[test]
    fn the_pad_reads_the_servers_answers() {
        let wait = Duration::from_secs(8);
        assert_eq!(
            [
                SdkError::InvalidCredentials,
                SdkError::RateLimited { retry_after: wait },
                SdkError::SignInRequired,
                SdkError::NoOfflinePin,
                SdkError::Busy,
            ]
            .iter()
            .map(PinFailure::of)
            .collect::<Vec<_>>(),
            [
                Some(PinFailure::Wrong),
                Some(PinFailure::Wait { retry_after: wait }),
                Some(PinFailure::SignInAgain),
                Some(PinFailure::NotKnownHere),
                None,
            ]
        );
    }

    #[test]
    fn a_password_is_counted_in_characters() {
        assert_eq!(
            password_problem("1234567"),
            Some(PasswordProblem::TooShort { min: 8 })
        );
        // Eight characters, sixteen bytes.
        assert_eq!(password_problem("éééééééé"), None);
    }

    fn from(address: &str, network: AddressNetwork) -> AddressClass {
        address_class(&DeviceAddress {
            address: address.into(),
            network,
        })
    }

    #[test]
    fn a_device_on_the_network_is_local() {
        assert_eq!(
            [
                from("192.168.86.20", AddressNetwork::Local),
                from("fd7a:115c:a1e0::1", AddressNetwork::Local),
                from("fe80::1", AddressNetwork::Local),
            ],
            [AddressClass::Local; 3]
        );
    }

    /// The server's own /64: local, yet reachable from anywhere.
    #[test]
    fn a_public_ipv6_address_on_the_network_says_both() {
        assert_eq!(
            from("2001:db8::20", AddressNetwork::Local),
            AddressClass::LocalPublicV6
        );
    }

    #[test]
    fn the_internet_and_a_kind_not_known_warn() {
        let classes = [
            from("203.0.113.9", AddressNetwork::Internet),
            from("203.0.113.9", AddressNetwork::Unknown),
        ];
        assert_eq!(classes, [AddressClass::Internet; 2]);
        assert!(classes.iter().all(|c| c.warns()));
    }
}
