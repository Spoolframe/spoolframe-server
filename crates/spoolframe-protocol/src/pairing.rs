//! Pairing a device with a portable identity:
//! the link a device shows as a QR code, and the frames the identity's
//! phone and the device then exchange over a direct Iroh connection on
//! [`PAIR_ALPN`]. No Spoolframe server takes part.
//!
//! A TV has no camera, so the QR code goes one way, device to phone:
//!
//! `spoolframe://pair?e=<endpoint id>&c=<code>&n=<name>&a=<ip:port>&r=<relay url>`
//!
//! - `e`, the device's `EndpointId` as 64 hex digits, and `c`, the
//!   one-time [`PairingCode`] as 43 base64url characters (no padding),
//!   are required. The phone dials `e` and nothing else, and Iroh's TLS
//!   proves the key, so the phone pairs with the device that showed the
//!   code or with nothing.
//! - `n` is the device's name, for "Pairing with …" while the phone
//!   dials; cut to [`MAX_NAME`] characters.
//! - `a` (repeated, at most [`MAX_ADDRS`]) and `r` (repeated, at most
//!   [`MAX_RELAYS`]) are where the device may be reached. Hints: the key
//!   is still proved whatever path is taken.
//!
//! Values are percent-encoded as in an invite link ([`crate::invite`]).
//! A parameter this build does not know is ignored; a second `e` or `c`
//! is not (the link is refused), nor is one that cannot be read.
//!
//! Then, on one bidirectional stream, each frame a [`crate::codec`] frame
//! (big-endian `u32` length, CBOR) of at most [`MAX_FRAME`] bytes:
//!
//! 1. phone → device: [`PhoneFrame::Hello`], the code and the identity
//!    the phone wants this device to act as;
//! 2. device → phone: [`DeviceFrame::Key`], the device's key for that
//!    identity and its name, or [`DeviceFrame::Refused`];
//! 3. phone → device: [`PhoneFrame::Credential`], the root's credential
//!    for that key, the identity's newest state and the servers it signs
//!    in to, or [`PhoneFrame::Declined`];
//! 4. device → phone: [`DeviceFrame::Done`], or [`DeviceFrame::Refused`].

use std::fmt;
use std::net::SocketAddr;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

use crate::invite::{decode, encode, is_relay_url};
use crate::portable::{
    DevicePublicKey, PortableIdentityState, PortableUserId, UserDeviceCredential,
};

/// The ALPN of pairing, version 1. A device accepts it only while it
/// shows a pairing code; at any other time a connection on it fails its
/// handshake.
pub const PAIR_ALPN: &[u8] = b"spoolframe/pair/1";

/// Bytes in a [`PairingCode`]: 256 bits, out of reach of guessing within
/// its five minutes whatever the rate.
pub const CODE_LEN: usize = 32;

/// How long a code is good for, from when the device shows it.
pub const CODE_TTL_SECS: u64 = 5 * 60;

/// The longest link read, in bytes. A link this build writes fits with
/// every hint at its limit, the name in characters of three bytes each;
/// a usual one (a LAN address or two, a short name) is under 250, a QR
/// code of version 10 or so.
pub const MAX_LINK: usize = 1024;

/// The most `a` hints a link is written or read with.
pub const MAX_ADDRS: usize = 8;

/// The most `r` hints a link is written or read with.
pub const MAX_RELAYS: usize = 2;

/// The most characters of the device's name a link carries.
pub const MAX_NAME: usize = 32;

/// The largest pairing frame either side reads: room for an identity
/// state at its limit of revoked keys, 1024 of about 90 bytes each.
pub const MAX_FRAME: u32 = 256 << 10;

const LINK_PREFIX: &str = "spoolframe://pair?";

/// A one-time pairing code: 32 random bytes the device made, shown only in
/// its QR code. Compared in constant time; [`fmt::Debug`] does not show
/// it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PairingCode(#[serde(with = "serde_bytes")] [u8; CODE_LEN]);

impl PairingCode {
    pub fn from_bytes(bytes: [u8; CODE_LEN]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; CODE_LEN] {
        &self.0
    }

    /// Whether `other` is this code, in time that does not depend on
    /// where they differ: every byte is looked at, whatever the first.
    pub fn matches(&self, other: &PairingCode) -> bool {
        let diff = self
            .0
            .iter()
            .zip(other.0.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b));
        std::hint::black_box(diff) == 0
    }

    /// The code as the link writes it: base64url, no padding.
    pub fn to_text(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }

    /// The code `text` writes, as [`Self::to_text`] gives it; `None` if
    /// it is not one.
    pub fn from_text(text: &str) -> Option<Self> {
        if text.len() != 43 {
            return None;
        }
        let bytes = URL_SAFE_NO_PAD.decode(text).ok()?;
        Some(Self(bytes.try_into().ok()?))
    }
}

impl fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairingCode(<redacted>)")
    }
}

/// What a pairing link holds.
#[derive(Clone, PartialEq, Eq)]
pub struct PairingLink {
    /// The device's `EndpointId`.
    pub endpoint: [u8; 32],
    pub code: PairingCode,
    /// What the device calls itself.
    pub name: String,
    /// Addresses it may be reached at.
    pub addrs: Vec<SocketAddr>,
    /// The URLs of relays it may be reached through.
    pub relays: Vec<String>,
}

impl fmt::Debug for PairingLink {
    // The code pairs: keep it out of logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingLink")
            .field("endpoint", &hex(&self.endpoint))
            .field("name", &self.name)
            .field("addrs", &self.addrs)
            .field("relays", &self.relays)
            .finish_non_exhaustive()
    }
}

impl PairingLink {
    /// The link: what the QR code carries. The name is cut to
    /// [`MAX_NAME`] characters, the hints to their limits, so what is
    /// written reads back the same.
    pub fn link(&self) -> String {
        let mut link = format!(
            "{LINK_PREFIX}e={}&c={}",
            hex(&self.endpoint),
            self.code.to_text()
        );
        let mut add = |key: &str, value: &str| {
            link.push('&');
            link.push_str(key);
            link.push('=');
            link.push_str(&encode(value));
        };
        add("n", &cut(&self.name));
        for addr in self.addrs.iter().take(MAX_ADDRS) {
            add("a", &addr.to_string());
        }
        for relay in self.relays.iter().take(MAX_RELAYS) {
            add("r", relay);
        }
        link
    }
}

/// Why a text is not a pairing link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    #[error("not a Spoolframe pairing code")]
    NotALink,
    #[error("the pairing code is longer than any device shows")]
    TooLong,
    #[error("the pairing code is damaged: {0}")]
    Damaged(&'static str),
}

/// The pairing link `text` holds.
pub fn parse_link(text: &str) -> Result<PairingLink, LinkError> {
    let text = text.trim();
    if text.len() > MAX_LINK {
        return Err(LinkError::TooLong);
    }
    let query = text.strip_prefix(LINK_PREFIX).ok_or(LinkError::NotALink)?;
    let mut endpoint = None;
    let mut code = None;
    let mut name = String::new();
    let mut addrs = Vec::new();
    let mut relays = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, raw) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "e" => {
                if endpoint.is_some() {
                    return Err(LinkError::Damaged("two endpoints"));
                }
                endpoint = Some(unhex(raw).ok_or(LinkError::Damaged("not an endpoint id"))?);
            }
            "c" => {
                if code.is_some() {
                    return Err(LinkError::Damaged("two codes"));
                }
                code = Some(PairingCode::from_text(raw).ok_or(LinkError::Damaged("not a code"))?);
            }
            "n" => name = cut(&decode(raw).ok_or(LinkError::Damaged("the name"))?),
            "a" if addrs.len() < MAX_ADDRS => {
                if let Some(addr) = decode(raw).and_then(|a| a.parse().ok()) {
                    addrs.push(addr);
                }
            }
            "r" if relays.len() < MAX_RELAYS => {
                if let Some(relay) = decode(raw).filter(|r| is_relay_url(r)) {
                    relays.push(relay);
                }
            }
            // A parameter a newer device added, or a hint past the limit.
            _ => {}
        }
    }
    Ok(PairingLink {
        endpoint: endpoint.ok_or(LinkError::Damaged("no endpoint id"))?,
        code: code.ok_or(LinkError::Damaged("no code"))?,
        name,
        addrs,
        relays,
    })
}

/// What the phone sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PhoneFrame {
    /// The code from the QR code, and the identity the device is to act
    /// as. First, and only once.
    Hello {
        code: PairingCode,
        identity: PortableUserId,
    },
    /// The root's credential for the key [`DeviceFrame::Key`] gave, the
    /// identity's newest state if the phone has one, and the servers the
    /// identity signs in to, for the device to save and sign in
    /// to.
    Credential {
        credential: Box<UserDeviceCredential>,
        state: Option<Box<PortableIdentityState>>,
        servers: Vec<PairedServer>,
    },
    /// The person on the phone said no.
    Declined,
}

/// The most servers a [`PhoneFrame::Credential`] carries; a device reads
/// no more.
pub const MAX_PAIRED_SERVERS: usize = 16;

/// A server the paired identity signs in to, as the phone has it saved:
/// enough for the device to pin and reach it. The phone vouches
/// for it as the invite that brought it there did; the device dials the
/// key and nothing else, and Iroh proves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedServer {
    /// Its `EndpointId`, 64 hex digits.
    pub endpoint_id: String,
    /// The server id pinned with the key.
    pub server_id: String,
    pub name: String,
    /// Where it may be reached, at most [`MAX_ADDRS`]: hints. As text,
    /// as `ServerAddresses` sends them.
    #[serde(with = "crate::messages::socket_addrs_as_text")]
    pub addrs: Vec<SocketAddr>,
    /// Its relays' URLs, at most [`MAX_RELAYS`]: hints.
    pub relays: Vec<String>,
}

impl PairedServer {
    /// This server held to what a device keeps of one: `None` if its key
    /// is not one; its name cut, and its hints to their limits, a relay
    /// that is not a relay's URL left out.
    pub fn within_bounds(mut self) -> Option<Self> {
        let key = &self.endpoint_id;
        if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        self.endpoint_id = key.to_ascii_lowercase();
        if self.server_id.is_empty() || self.server_id.len() > 128 {
            return None;
        }
        crate::messages::cut(&mut self.name);
        self.addrs.truncate(MAX_ADDRS);
        self.relays.retain(|r| is_relay_url(r));
        self.relays.truncate(MAX_RELAYS);
        Some(self)
    }
}

/// What the device sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeviceFrame {
    /// The device's key for the identity [`PhoneFrame::Hello`] named, and
    /// what the device calls itself.
    Key {
        device_key: DevicePublicKey,
        name: String,
    },
    /// The credential is installed.
    Done,
    /// The pairing is over, refused for `reason`.
    Refused { reason: Refusal },
}

/// Why a device refused a pairing. The device's pairing ends with each
/// of them: it shows a new code to try again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    /// The device does not take portable sign-ins (its policy is off).
    #[error("this device does not allow sign-ins from other people's accounts")]
    NotAllowed,
    /// The code is not the one shown.
    #[error("the pairing code does not match")]
    WrongCode,
    /// The code was shown more than [`CODE_TTL_SECS`] ago.
    #[error("the pairing code has expired")]
    Expired,
    /// The credential does not hold, is for another identity or key, or
    /// the state revokes the key.
    #[error("the device did not accept the credential")]
    CredentialRefused,
    /// Something else went wrong on the device: its storage, most likely.
    #[error("the device could not finish pairing")]
    Failed,
}

fn cut(name: &str) -> String {
    name.chars().take(MAX_NAME).collect()
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.is_ascii() {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(bytes)
}

/// Characters in a [`RelayCode`]: 10 of Crockford's base32, 50 bits.
/// Typed by a person, so short; guessed only at the rate the server lets
/// a stranger try, against a code that lives [`RELAY_TTL_SECS`].
pub const RELAY_CODE_LEN: usize = 10;

/// How long a server holds a relayed pairing, from its offer.
pub const RELAY_TTL_SECS: u64 = 5 * 60;

/// Crockford's base32 alphabet: no I, L, O or U.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The short code a device shows beside its QR code, for a root terminal
/// with no camera to pair it through a server both reach: 10
/// characters of Crockford's base32, written in two groups of five.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RelayCode(String);

impl RelayCode {
    /// The code `bytes` make: each of the first 10 taken mod 32.
    pub fn from_random(bytes: [u8; RELAY_CODE_LEN]) -> Self {
        Self(
            bytes
                .iter()
                .map(|b| CROCKFORD[usize::from(b % 32)] as char)
                .collect(),
        )
    }

    /// The code a person typed: case, spaces and dashes ignored; I and L
    /// read as 1, O as 0, as Crockford's base32 reads them. `None` if it
    /// is not 10 such characters.
    pub fn parse(text: &str) -> Option<Self> {
        let code: String = text
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .map(|c| match c.to_ascii_uppercase() {
                'I' | 'L' => '1',
                'O' => '0',
                c => c,
            })
            .collect();
        (code.len() == RELAY_CODE_LEN && code.bytes().all(|b| CROCKFORD.contains(&b)))
            .then_some(Self(code))
    }

    /// As a person reads it: `ABCDE-FGHJK`.
    pub fn display(&self) -> String {
        format!("{}-{}", &self.0[..5], &self.0[5..])
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RelayCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RelayCode(<redacted>)")
    }
}

impl TryFrom<String> for RelayCode {
    type Error = &'static str;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text).ok_or("a relay code is 10 characters of Crockford's base32")
    }
}

impl From<RelayCode> for String {
    fn from(code: RelayCode) -> Self {
        code.0
    }
}

/// A device's pairing request, held on a server under a [`RelayCode`]
/// for a root terminal to fetch: the key the root is to vouch for
/// and the device's name. Public data: whoever has the code may read it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingOffer {
    pub code: RelayCode,
    /// The device's key for the identity it is to act as.
    pub device_key: DevicePublicKey,
    /// Cut to [`MAX_NAME`] characters.
    pub name: String,
}

/// The root terminal's answer to a [`PairingOffer`]: what
/// [`PhoneFrame::Credential`] carries on a direct pairing. The server only
/// carries it: the device checks the credential names its own key and
/// shows whose identity it is before it keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingAnswer {
    pub credential: Box<UserDeviceCredential>,
    pub state: Option<Box<PortableIdentityState>>,
    /// At most [`MAX_PAIRED_SERVERS`].
    pub servers: Vec<PairedServer>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PairingLink {
        PairingLink {
            endpoint: [0xab; 32],
            code: PairingCode::from_bytes([7; 32]),
            name: "Living room TV & co".into(),
            addrs: vec![
                "192.168.1.20:51234".parse().unwrap(),
                "[fe80::1]:51234".parse().unwrap(),
            ],
            relays: vec!["https://relay.example.org/".into()],
        }
    }

    #[test]
    fn a_link_reads_back_as_written() {
        let link = sample();
        let text = link.link();
        assert!(text.starts_with("spoolframe://pair?e=abab"), "{text}");
        assert_eq!(parse_link(&text).unwrap(), link);
    }

    #[test]
    fn a_link_at_every_limit_fits() {
        let link = PairingLink {
            name: "名".repeat(200),
            addrs: (0..20)
                .map(|n| {
                    format!("[fd12:3456:789a:1:2:3:4:{n:x}]:65535")
                        .parse()
                        .unwrap()
                })
                .collect(),
            relays: (0..5)
                .map(|n| format!("https://relay-{n}.some-long-domain-name.example.org/"))
                .collect(),
            ..sample()
        };
        let text = link.link();
        assert!(text.len() <= MAX_LINK, "{} bytes", text.len());
        let read = parse_link(&text).unwrap();
        assert_eq!(read.name.chars().count(), MAX_NAME);
        assert_eq!(
            (read.addrs.len(), read.relays.len()),
            (MAX_ADDRS, MAX_RELAYS)
        );
    }

    #[test]
    fn what_is_not_a_link_is_refused() {
        let good = sample().link();
        assert_eq!(parse_link("https://example.org"), Err(LinkError::NotALink));
        assert_eq!(
            parse_link(&good.replace("spoolframe://pair?", "spoolframe://invite?")),
            Err(LinkError::NotALink)
        );
        assert_eq!(
            parse_link(&format!("{good}&x={}", "y".repeat(MAX_LINK))),
            Err(LinkError::TooLong)
        );
        let code = PairingCode::from_bytes([7; 32]).to_text();
        for (damaged, why) in [
            (good.replace(&code, ""), "not a code"),
            (good.replace(&code, &code[..42]), "not a code"),
            (good.replace(&format!("c={code}"), "x=1"), "no code"),
            (good.replace("e=abab", "e=zzab"), "not an endpoint id"),
            (
                good.replace(&format!("e={}", "ab".repeat(32)), ""),
                "no endpoint id",
            ),
            (format!("{good}&c={code}"), "two codes"),
            (format!("{good}&e={}", "cd".repeat(32)), "two endpoints"),
            (format!("{good}&n=%ZZ"), "the name"),
        ] {
            assert_eq!(
                parse_link(&damaged),
                Err(LinkError::Damaged(why)),
                "{damaged}"
            );
        }
    }

    #[test]
    fn unknown_parameters_and_unreadable_hints_are_ignored() {
        let text = format!(
            "{}&v=2&a=not-an-address&r=ftp%3A%2F%2Fnope",
            sample().link()
        );
        assert_eq!(parse_link(&text).unwrap(), sample());
    }

    #[test]
    fn the_code_is_kept_out_of_debug() {
        let link = sample();
        let shown = format!("{link:?} {:?}", link.code);
        assert!(!shown.contains(&link.code.to_text()), "{shown}");
        assert!(!shown.contains("7, 7"), "{shown}");
    }

    #[test]
    fn codes_match_only_themselves() {
        let code = PairingCode::from_bytes([7; 32]);
        assert!(code.matches(&PairingCode::from_bytes([7; 32])));
        let mut last = [7; 32];
        last[31] = 8;
        assert!(!code.matches(&PairingCode::from_bytes(last)));
        let mut first = [7; 32];
        first[0] = 0;
        assert!(!code.matches(&PairingCode::from_bytes(first)));
    }

    /// Damaged links, from a fixed seed: none panics, and what reads as a
    /// link writes back to one that reads the same.
    #[test]
    fn mangled_links_never_panic_and_what_reads_round_trips() {
        let base = sample().link().into_bytes();
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let mut bytes = base.clone();
            for _ in 0..(next() % 6 + 1) {
                let at = (next() as usize) % bytes.len();
                match next() % 3 {
                    0 => bytes[at] = next() as u8,
                    1 => {
                        bytes.remove(at);
                    }
                    _ => bytes.insert(at, b"&=%:[]"[(next() % 6) as usize]),
                }
                if bytes.is_empty() {
                    break;
                }
            }
            let Ok(text) = std::str::from_utf8(&bytes) else {
                continue;
            };
            if let Ok(link) = parse_link(text) {
                assert_eq!(parse_link(&link.link()), Ok(link));
            }
        }
    }

    #[test]
    fn frames_are_tagged_by_type() {
        let declined = crate::codec::encode(&PhoneFrame::Declined).unwrap();
        let read: PhoneFrame = crate::codec::decode(&declined).unwrap();
        assert_eq!(read, PhoneFrame::Declined);
        let refused = DeviceFrame::Refused {
            reason: Refusal::NotAllowed,
        };
        let json = serde_json::to_string(&refused).unwrap();
        assert_eq!(json, r#"{"type":"refused","reason":"not_allowed"}"#);
    }

    /// A credential frame for user 7's device 9, with `servers`.
    fn credential_frame(servers: Vec<PairedServer>) -> PhoneFrame {
        use ed25519_dalek::{Signer, SigningKey};
        let root = SigningKey::from_bytes(&[7; 32]);
        let device = SigningKey::from_bytes(&[9; 32]);
        let credential = UserDeviceCredential::new(
            PortableUserId::from_bytes(root.verifying_key().to_bytes()).unwrap(),
            DevicePublicKey::from_bytes(device.verifying_key().to_bytes()).unwrap(),
            1,
            1_791_504_000,
            1_791_504_000 + 3600,
            |m| root.sign(m),
        );
        PhoneFrame::Credential {
            credential: Box::new(credential),
            state: None,
            servers,
        }
    }

    fn home() -> PairedServer {
        PairedServer {
            endpoint_id: "ab".repeat(32),
            server_id: "q7k3mpa9trdwf2h".into(),
            name: "Home Media".into(),
            addrs: vec![
                "192.168.1.10:7420".parse().unwrap(),
                "[2001:db8::1]:7420".parse().unwrap(),
            ],
            relays: vec!["https://relay.example.org/".into()],
        }
    }

    /// P7-06: the servers ride the credential frame, their addresses as
    /// text (a tagged frame cannot carry serde's own form through CBOR).
    #[test]
    fn a_credential_frame_carries_its_servers() {
        let frame = credential_frame(vec![home()]);
        let read: PhoneFrame =
            crate::codec::decode(&crate::codec::encode(&frame).unwrap()).unwrap();
        assert_eq!(read, frame);
        let json = serde_json::to_value(&frame).unwrap();
        assert_eq!(json["servers"][0]["addrs"][1], "[2001:db8::1]:7420");
    }

    /// A credential frame without its servers, as a phone before P7-06
    /// sent it, is refused: an empty list is sent as one.
    #[test]
    fn a_credential_frame_without_servers_is_refused() {
        let mut json = serde_json::to_value(credential_frame(vec![home()])).unwrap();
        json.as_object_mut().unwrap().remove("servers");
        let error = serde_json::from_value::<PhoneFrame>(json).unwrap_err();
        assert!(
            error.to_string().contains("missing field `servers`"),
            "{error}"
        );
    }

    #[test]
    fn a_paired_server_is_held_to_bounds() {
        assert_eq!(home().within_bounds(), Some(home()));
        let loud = PairedServer {
            endpoint_id: "AB".repeat(32),
            name: "x".repeat(500),
            addrs: (0..20)
                .map(|n| format!("10.0.0.{n}:7420").parse().unwrap())
                .collect(),
            relays: vec![
                "ftp://nope".into(),
                "https://a.example/".into(),
                "https://b.example/".into(),
                "https://c.example/".into(),
            ],
            ..home()
        };
        let held = loud.within_bounds().unwrap();
        assert_eq!(held.endpoint_id, "ab".repeat(32));
        assert_eq!(held.name.chars().count(), crate::MAX_HELLO_TEXT);
        assert_eq!(held.addrs.len(), MAX_ADDRS);
        assert_eq!(held.relays, ["https://a.example/", "https://b.example/"]);
        for bad in [
            PairedServer {
                endpoint_id: "not a key".into(),
                ..home()
            },
            PairedServer {
                server_id: String::new(),
                ..home()
            },
        ] {
            assert_eq!(bad.within_bounds(), None);
        }
    }
}
