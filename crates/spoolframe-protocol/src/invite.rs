//! Invites: the link that finds a server from anywhere, and the
//! code in it that lets someone make their own account there.
//!
//! An invite is one string, shown as a QR code and pasted as it is:
//!
//! `spoolframe://invite?server=<endpoint id>&name=<name>&addr=<ip:port>&relay=<url>&exp=<unix seconds>&role=<consumer|admin>&cap=<code>`
//!
//! Sent as text it has an https form, which Messages and Mail make
//! tappable and the apps open as a universal link, with the same query
//! after `#`: `https://spoolframe.app/i#server=…&cap=…`. A browser never
//! sends a fragment, so the site never sees a code, an address or a
//! server's key. A QR code stays `spoolframe://`: it is scanned in the
//! same room and never needs the site. [`parse_link`] reads both.
//!
//! - `server`, the hex `EndpointId`, is the only parameter a link must
//!   have. A client pins it as it is: the link came from the server's
//!   admin, so there is nothing to trust on first use.
//! - `name`, `addr` (repeated, one per address) and `relay` (repeated)
//!   are hints for finding it. Iroh still proves the key whatever path is
//!   taken.
//! - `cap` is the invite's code, the capability that makes an account
//!   (`RedeemInvite`). Without it the link only finds and saves the server.
//! - `exp` and `role` are hints for the join form ("Joins as a consumer ·
//!   expires Oct 14"). The server enforces its own when the code is
//!   redeemed, and a link without them works the same.
//! - `kind=portable-account`: the code grants a portable identity
//!   access (`RedeemPortableInvite`) rather than making an account. Absent,
//!   it makes an account (`RedeemInvite`). A hint for which form to show:
//!   the server answers the other request with `WrongInviteKind`.
//! - `kind=federation` (experimental): the code pairs another
//!   server with this one; that server's admin redeems it
//!   (`PairServer`, or the admin API's `federation_redeem`).
//!
//! Values are percent-encoded where they would break the link (`&`, `=`,
//! `#`, `%`, `+`, spaces, anything outside ASCII); `:` and `/` stay as
//! they are, so addresses and URLs read as themselves. A parameter this
//! build does not know is ignored, as in the device sign-in link
//! ([`crate::device_sign_in::parse_link`]), and so is a hint that cannot
//! be read: a newer server may add both.
//!
//! The code is of the setup code's class: 12 characters from an alphabet
//! with nothing to misread, about 59 bits, shown in groups of four
//! (`QW7K-2MFP-XR4T`). People may type it with any case, dash or spacing;
//! [`normalize_code`] takes all of them.

use std::fmt;
use std::net::SocketAddr;

use spoolframe_types::Role;

/// The characters an invite code is made of: no 0/O, 1/I/L or U.
pub const CODE_ALPHABET: &[u8; 30] = b"ABCDEFGHJKMNPQRSTVWXYZ23456789";

/// Characters in a code: 30^12, about 59 bits.
pub const CODE_LEN: usize = 12;

/// The most `addr` or `relay` hints a link is read with; any after them
/// are left out, so a link cannot make a client keep or dial without
/// end.
pub const MAX_HINTS: usize = 32;
/// The longest link read, in bytes. A QR code holds a
/// few hundred; a pasted link past this is not one a server wrote.
pub const MAX_LINK: usize = 4096;

const LINK_PREFIX: &str = "spoolframe://invite?";
/// The https form's site, and the page whose fragment holds the invite.
/// Permanent: every shipped app's associated domains name it, and every
/// invite sent.
const SHARE_HOST: &str = "spoolframe.app";
const SHARE_PREFIX: &str = "https://spoolframe.app/i#";

/// `input` as a code (`QW7K2MFPXR4T`), whatever case, dashes or spaces it
/// was typed with; `None` if it cannot be one.
pub fn normalize_code(input: &str) -> Option<String> {
    let code: String = input
        .chars()
        .filter(|c| !matches!(c, '-' | ' '))
        .map(|c| c.to_ascii_uppercase())
        .collect();
    (code.len() == CODE_LEN && code.bytes().all(|b| CODE_ALPHABET.contains(&b))).then_some(code)
}

/// `code` as shown: `QW7K-2MFP-XR4T`.
pub fn display_code(code: &str) -> String {
    let groups: Vec<&str> = (0..code.len())
        .step_by(4)
        .filter_map(|i| code.get(i..(i + 4).min(code.len())))
        .collect();
    groups.join("-")
}

/// What an invite link holds.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Invite {
    /// The server's `EndpointId`, hex, lowercase.
    pub server: String,
    /// What the server calls itself, for the join form until it answers.
    pub name: Option<String>,
    /// Addresses it may be reached at.
    pub addrs: Vec<SocketAddr>,
    /// The URLs of relays it may be reached through.
    pub relays: Vec<String>,
    /// The pkarr relay it publishes where it is to: a device
    /// looks it up there by its key when no hint reaches it.
    pub lookup: Option<String>,
    /// The invite's code, normalized; `None` for a link that only finds
    /// the server.
    pub code: Option<String>,
    /// When the code stops working, in Unix seconds: a hint.
    pub expires_at: Option<u64>,
    /// The role the account is made with: a hint.
    pub role: Option<Role>,
    /// What the code does: a hint.
    pub kind: InviteLinkKind,
}

/// What an invite link's code does, as the link says.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum InviteLinkKind {
    /// Makes an account on the server (`RedeemInvite`): a link without
    /// `kind`.
    #[default]
    CreateUser,
    /// Grants a portable identity access (`RedeemPortableInvite`).
    PortableAccount,
    /// Pairs another server with this one: redeemed by that
    /// server, not a device.
    Federation,
}

impl fmt::Debug for Invite {
    // The code makes an account: keep it out of logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Invite")
            .field("server", &self.server)
            .field("name", &self.name)
            .field("addrs", &self.addrs)
            .field("relays", &self.relays)
            .field("lookup", &self.lookup)
            .field("code", &self.code.as_ref().map(|_| "<redacted>"))
            .field("expires_at", &self.expires_at)
            .field("role", &self.role)
            .field("kind", &self.kind)
            .finish()
    }
}

impl Invite {
    /// The `spoolframe://` link: what the QR code carries.
    pub fn link(&self) -> String {
        format!("{LINK_PREFIX}{}", self.query())
    }

    /// The https link: what is sent as text and copied. The invite
    /// is its fragment, never its path or query, so a browser that opens
    /// it sends none of it to the site.
    pub fn share_link(&self) -> String {
        format!("{SHARE_PREFIX}{}", self.query())
    }

    /// The parameters both links carry.
    fn query(&self) -> String {
        let mut link = format!("server={}", self.server);
        let mut add = |key: &str, value: &str| {
            link.push('&');
            link.push_str(key);
            link.push('=');
            link.push_str(&encode(value));
        };
        if let Some(name) = &self.name {
            add("name", name);
        }
        for addr in &self.addrs {
            add("addr", &addr.to_string());
        }
        for relay in &self.relays {
            add("relay", relay);
        }
        if let Some(lookup) = &self.lookup {
            add("lookup", lookup);
        }
        if let Some(at) = self.expires_at {
            add("exp", &at.to_string());
        }
        if let Some(role) = self.role {
            add("role", role.as_str());
        }
        match self.kind {
            InviteLinkKind::CreateUser => {}
            InviteLinkKind::PortableAccount => add("kind", "portable-account"),
            InviteLinkKind::Federation => add("kind", "federation"),
        }
        if let Some(code) = &self.code {
            add("cap", &display_code(code));
        }
        link
    }
}

/// The invite `text` holds, in either form; `None` if it is not an
/// invite link: another scheme or site, no `server`, a `server` that is
/// not a key, or a `cap` that is not a code (a damaged link, not one
/// without a code).
pub fn parse_link(text: &str) -> Option<Invite> {
    let text = text.trim();
    if text.len() > MAX_LINK {
        return None;
    }
    let query = text
        .strip_prefix(LINK_PREFIX)
        .or_else(|| share_query(text))?;
    let mut server = None;
    let mut invite = Invite::default();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, raw) = pair.split_once('=').unwrap_or((pair, ""));
        // A value that does not decode is a hint that cannot be read.
        let value = decode(raw);
        match (key, value) {
            ("server", Some(value)) if server.is_none() => server = Some(value),
            ("server", None) => return None,
            ("cap", Some(value)) => invite.code = Some(normalize_code(&value)?),
            ("cap", None) => return None,
            // Shown in the join form: cut as a device's name is.
            ("name", Some(mut value)) => {
                crate::messages::cut(&mut value);
                invite.name = Some(value);
            }
            ("addr", Some(value)) if invite.addrs.len() < MAX_HINTS => {
                if let Ok(addr) = value.parse() {
                    invite.addrs.push(addr);
                }
            }
            ("relay", Some(value)) if invite.relays.len() < MAX_HINTS => {
                if is_relay_url(&value) {
                    invite.relays.push(value);
                }
            }
            // An http(s) URL, as a relay's is.
            ("lookup", Some(value)) if is_relay_url(&value) => invite.lookup = Some(value),
            ("exp", Some(value)) => invite.expires_at = value.parse().ok(),
            ("role", Some(value)) => {
                invite.role = match value.as_str() {
                    "consumer" => Some(Role::Consumer),
                    "admin" => Some(Role::Admin),
                    _ => None,
                }
            }
            ("kind", Some(value)) if value == "portable-account" => {
                invite.kind = InviteLinkKind::PortableAccount;
            }
            ("kind", Some(value)) if value == "federation" => {
                invite.kind = InviteLinkKind::Federation;
            }
            // A parameter a newer server added, or a hint past the limit.
            _ => {}
        }
    }
    let server = server?;
    if server.len() != 64 || !server.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    invite.server = server.to_ascii_lowercase();
    Some(invite)
}

/// The fragment of `text` if it is the https form: `https`, the host
/// `spoolframe.app` and nothing else in the authority (no user, port or
/// trailing dot), the path `/i` or `/i/`, no query, then `#`. Scheme and
/// host in any case, as URLs allow; the path as it is. Anything else is
/// not an invite, so a link to another site never reads as one.
fn share_query(text: &str) -> Option<&str> {
    let scheme = text.get(..8)?;
    if !scheme.eq_ignore_ascii_case("https://") {
        return None;
    }
    let rest = &text[8..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    if !rest[..end].eq_ignore_ascii_case(SHARE_HOST) {
        return None;
    }
    let after = &rest[end..];
    after
        .strip_prefix("/i#")
        .or_else(|| after.strip_prefix("/i/#"))
}

/// Whether `value` may be a relay's URL: `https://` or `http://` and a
/// host, and nothing an opener could read another way. The SDK holds a
/// server's pushed relays and lookup to it as well.
pub fn is_relay_url(value: &str) -> bool {
    let rest = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"));
    rest.is_some_and(|rest| {
        !rest.is_empty()
            && !rest.starts_with('/')
            && !value.chars().any(|c| c.is_whitespace() || c.is_control())
    })
}

/// Whether `b` goes into a link as it is: letters, digits and what an
/// address or a URL is written with, but none of what a query string
/// gives a meaning to (`&`, `=`, `#`, `+`, `%`).
fn plain(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"-._~:/@[]!$'()*,;?".contains(&b)
}

/// `value` percent-encoded where it is not [`plain`], byte by byte of its
/// UTF-8.
pub(crate) fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if plain(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `raw` with its `%XX` escapes decoded; `None` if an escape is broken or
/// the bytes are not UTF-8. `+` is itself, as [`encode`] never writes one
/// for a space.
pub(crate) fn decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P6-07, glm E-4: a pasted link past MAX_LINK is not read, and the
    /// name shown from one is cut as a device's name is.
    #[test]
    fn a_link_too_long_is_not_read_and_a_long_name_is_cut() {
        let key = "ab".repeat(32);
        let name = "x".repeat(500);
        let link = format!("{LINK_PREFIX}server={key}&name={name}");
        let invite = parse_link(&link).unwrap();
        assert_eq!(invite.name.unwrap().chars().count(), crate::MAX_HELLO_TEXT);
        let padded = format!("{link}&pad={}", "y".repeat(MAX_LINK));
        assert_eq!(parse_link(&padded), None);
    }

    const SERVER: &str = "3f9a1c7e5b2d8a04e6c1f7b39d52a8e0c4b6f1a2d3e5c7b9a1f0e2d4c6b8a0f1";

    /// The design's invite (WebInviteResult).
    fn invite() -> Invite {
        Invite {
            server: SERVER.into(),
            name: Some("Home Media".into()),
            addrs: vec!["203.0.113.24:41641".parse().unwrap()],
            relays: vec!["https://relay.home-media.example".into()],
            lookup: None,
            code: Some("QW7K2MFPXR4T".into()),
            expires_at: Some(1_792_003_200),
            role: Some(Role::Consumer),
            kind: InviteLinkKind::CreateUser,
        }
    }

    #[test]
    fn a_link_is_the_one_the_design_shows() {
        assert_eq!(
            invite().link(),
            format!(
                "spoolframe://invite?server={SERVER}&name=Home%20Media\
                 &addr=203.0.113.24:41641&relay=https://relay.home-media.example\
                 &exp=1792003200&role=consumer&cap=QW7K-2MFP-XR4T"
            )
        );
    }

    #[test]
    fn a_link_reads_back() {
        assert_eq!(parse_link(&invite().link()), Some(invite()));
    }

    #[test]
    fn a_share_link_is_the_query_after_the_site_s_fragment() {
        assert_eq!(
            invite().share_link(),
            format!(
                "https://spoolframe.app/i#server={SERVER}&name=Home%20Media\
                 &addr=203.0.113.24:41641&relay=https://relay.home-media.example\
                 &exp=1792003200&role=consumer&cap=QW7K-2MFP-XR4T"
            )
        );
    }

    #[test]
    fn a_share_link_reads_back_as_the_same_invite() {
        assert_eq!(parse_link(&invite().share_link()), Some(invite()));
        let only = Invite {
            server: SERVER.into(),
            ..Invite::default()
        };
        assert_eq!(parse_link(&only.share_link()), Some(only));
    }

    /// The code, the key and the hints are all in the fragment: a browser
    /// asks the site for `/i` and nothing else.
    #[test]
    fn nothing_of_the_invite_is_in_the_share_link_s_path_or_query() {
        let link = invite().share_link();
        let (before, fragment) = link.split_once('#').unwrap();
        assert_eq!(before, "https://spoolframe.app/i");
        for part in ["QW7K", SERVER, "203.0.113.24", "Home"] {
            assert!(fragment.contains(part), "{part} in {fragment}");
        }
        // Not even an escaped `#` could end the fragment early.
        let odd = Invite {
            name: Some("#cap=AAAA?x/y".into()),
            ..invite()
        };
        let link = odd.share_link();
        assert_eq!(link.matches('#').count(), 1, "{link}");
        assert_eq!(parse_link(&link), Some(odd));
    }

    /// A link written before PX-45, as a QR code made then carries it.
    #[test]
    fn a_spoolframe_link_from_before_share_links_still_reads() {
        let old = format!(
            "spoolframe://invite?server={SERVER}&name=Home%20Media\
             &addr=203.0.113.24:41641&relay=https://relay.home-media.example\
             &exp=1792003200&role=consumer&cap=QW7K-2MFP-XR4T"
        );
        assert_eq!(parse_link(&old), Some(invite()));
    }

    #[test]
    fn a_share_link_is_read_with_a_trailing_slash_and_in_any_case_of_host() {
        let query = invite().link().replace(LINK_PREFIX, "");
        for text in [
            format!("https://spoolframe.app/i/#{query}"),
            format!("HTTPS://SpoolFrame.App/i#{query}"),
            format!("  https://spoolframe.app/i#{query}\n"),
        ] {
            assert_eq!(parse_link(&text), Some(invite()), "{text}");
        }
    }

    #[test]
    fn a_share_link_on_another_site_or_shape_is_not_an_invite() {
        let query = invite().link().replace(LINK_PREFIX, "");
        for text in [
            // Another host, or one made to look like it.
            format!("https://example.com/i#{query}"),
            format!("https://spoolframe.app.example.com/i#{query}"),
            format!("https://www.spoolframe.app/i#{query}"),
            format!("https://spoolframe.app./i#{query}"),
            format!("https://spoolframe.apps/i#{query}"),
            format!("https://spoo1frame.app/i#{query}"),
            format!("https://xn--spoolframe-xyz.app/i#{query}"),
            // The userinfo trick, both ways round, and a port.
            format!("https://spoolframe.app@example.com/i#{query}"),
            format!("https://user@spoolframe.app/i#{query}"),
            format!("https://spoolframe.app:443/i#{query}"),
            // Not https.
            format!("http://spoolframe.app/i#{query}"),
            format!("spoolframe.app/i#{query}"),
            format!("//spoolframe.app/i#{query}"),
            // In the query, which a browser would send the site.
            format!("https://spoolframe.app/i?{query}"),
            format!("https://spoolframe.app/i?x=1#{query}"),
            // Another path.
            format!("https://spoolframe.app/I#{query}"),
            format!("https://spoolframe.app/invite#{query}"),
            format!("https://spoolframe.app/i/x#{query}"),
            format!("https://spoolframe.app/#{query}"),
            format!("https://spoolframe.app#{query}"),
            // An empty fragment.
            "https://spoolframe.app/i#".to_owned(),
        ] {
            assert_eq!(parse_link(&text), None, "{text}");
        }
    }

    /// P7-05: a portable-account invite says so; one that does not is an
    /// account's, as every link before it was.
    #[test]
    fn a_portable_account_link_says_its_kind_and_reads_back() {
        let portable = Invite {
            role: None,
            kind: InviteLinkKind::PortableAccount,
            ..invite()
        };
        let link = portable.link();
        assert!(link.contains("&kind=portable-account&"), "{link}");
        assert_eq!(parse_link(&link), Some(portable));
        assert!(!invite().link().contains("kind="));
        // A kind this build does not know is a hint it cannot read.
        let other = invite().link().replace("&cap=", "&kind=household&cap=");
        assert_eq!(parse_link(&other).unwrap().kind, InviteLinkKind::CreateUser);
    }

    /// P8-01: a server's pairing invite says so, with no role.
    #[test]
    fn a_federation_link_says_its_kind_and_reads_back() {
        let federation = Invite {
            role: None,
            kind: InviteLinkKind::Federation,
            ..invite()
        };
        let link = federation.link();
        assert!(link.contains("&kind=federation&"), "{link}");
        assert!(!link.contains("role="), "{link}");
        assert_eq!(parse_link(&link), Some(federation.clone()));
        assert_eq!(parse_link(&federation.share_link()), Some(federation));
    }

    #[test]
    fn every_hint_reads_back_however_many_and_whatever_they_hold() {
        let invite = Invite {
            server: SERVER.into(),
            name: Some("Anna & Bob's = 100% \"home\" #1 + café ✓".into()),
            addrs: vec![
                "192.168.1.10:7420".parse().unwrap(),
                "[2001:db8::1]:7420".parse().unwrap(),
                "203.0.113.7:7420".parse().unwrap(),
            ],
            relays: vec![
                "https://relay.example.org/".into(),
                "https://other.example.org:8443/path?a=1&b=2#x".into(),
            ],
            lookup: Some("https://dns.example.org/pkarr".into()),
            code: None,
            expires_at: None,
            role: Some(Role::Admin),
            kind: InviteLinkKind::PortableAccount,
        };
        assert_eq!(parse_link(&invite.link()), Some(invite));
    }

    #[test]
    fn a_link_with_only_the_server_finds_it() {
        let only = Invite {
            server: SERVER.into(),
            ..Invite::default()
        };
        assert_eq!(only.link(), format!("spoolframe://invite?server={SERVER}"));
        assert_eq!(parse_link(&only.link()), Some(only));
    }

    #[test]
    fn parameters_come_in_any_order_and_new_ones_are_ignored() {
        let text = format!(
            "spoolframe://invite?v=2&cap=qw7k 2mfp-xr4t&role=consumer&flavour=mint\
             &exp=1792003200&relay=https://relay.home-media.example&addr=203.0.113.24:41641\
             &name=Home%20Media&server={}",
            SERVER.to_ascii_uppercase()
        );
        assert_eq!(parse_link(&text), Some(invite()));
    }

    #[test]
    fn a_hint_that_cannot_be_read_is_left_out() {
        let text = format!(
            "spoolframe://invite?server={SERVER}&addr=nowhere&addr=10.0.0.2:7420\
             &relay=ftp://relay.example.org&exp=soon&role=overlord&name=%zz"
        );
        assert_eq!(
            parse_link(&text),
            Some(Invite {
                server: SERVER.into(),
                addrs: vec!["10.0.0.2:7420".parse().unwrap()],
                ..Invite::default()
            })
        );
    }

    #[test]
    fn hints_past_the_limit_are_left_out() {
        let addrs: String = (0..MAX_HINTS + 5)
            .map(|i| format!("&addr=10.0.{}.{}:7420", i / 250, i % 250 + 1))
            .collect();
        let read = parse_link(&format!("spoolframe://invite?server={SERVER}{addrs}")).unwrap();
        assert_eq!(read.addrs.len(), MAX_HINTS);
    }

    #[test]
    fn anything_else_is_not_an_invite() {
        for text in [
            format!("https://example.com/invite?server={SERVER}"),
            format!("spoolframe://approve?server={SERVER}&code=BCDFGHJK"),
            "spoolframe://invite?name=Home".to_owned(),
            "spoolframe://invite?server=abc".to_owned(),
            format!("spoolframe://invite?server={SERVER}&cap=AAAA"),
            format!("spoolframe://invite?server={SERVER}&cap=QW7K-2MFP-XR40"),
        ] {
            assert_eq!(parse_link(&text), None, "{text}");
        }
    }

    #[test]
    fn a_code_is_read_however_it_is_typed() {
        for typed in [
            "QW7K-2MFP-XR4T",
            "qw7k2mfpxr4t",
            "qw7k 2mfp xr4t",
            " Qw7K-2mFp-xR4t ",
        ] {
            assert_eq!(
                normalize_code(typed).as_deref(),
                Some("QW7K2MFPXR4T"),
                "{typed:?}"
            );
        }
    }

    #[test]
    fn a_code_with_a_misreadable_character_or_the_wrong_length_is_not_one() {
        for typed in ["QW7K-2MFP-XR4O", "QW7K-2MFP-XR4", "QW7K-2MFP-XR4TT", ""] {
            assert_eq!(normalize_code(typed), None, "{typed:?}");
        }
    }

    #[test]
    fn a_code_shows_in_groups_of_four() {
        assert_eq!(display_code("QW7K2MFPXR4T"), "QW7K-2MFP-XR4T");
    }

    #[test]
    fn the_code_stays_out_of_debug_output() {
        assert!(!format!("{:?}", invite()).contains("QW7K"));
    }
}
