//! The links an app is opened with, or has pasted or scanned: an invite
//! (`spoolframe://invite?…`, or the `https://spoolframe.app/i#…` form a
//! message carries), a device's sign-in to approve
//! (`spoolframe://approve?…`), a device's pairing code
//! (`spoolframe://pair?…`), and an item to open or play
//! (`spoolframe://item?…`, from a Top Shelf or a widget). Read here, by
//! the protocol's own parsers, instead of each app's looser checks.

use spoolframe_protocol::{device_sign_in, invite, pairing};
use spoolframe_types::MediaId;

/// What a link opens ([`parse_app_link`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum AppLink {
    /// An invite, as the `spoolframe://` link the SDK's invite calls take
    /// ([`crate::Client::read_invite`]), whichever form it came in.
    Invite { link: String },
    /// Approve the device showing `code` on `server` (a hex endpoint id)
    /// ([`crate::Session::lookup_device_sign_in`]), as one of its
    /// profiles.
    Approve { server: String, code: String },
    /// A device's pairing code, for the phone holding an identity to
    /// authorize it ([`crate::Portable`]): the link as it is.
    Pair { link: String },
    /// Open item `id`, or play it with `play`, on `server` (a hex
    /// endpoint id) or on the server open now with `None`.
    Item {
        server: Option<String>,
        id: MediaId,
        play: bool,
    },
}

const ITEM_PREFIX: &str = "spoolframe://item?";

/// What `text` opens; `None` when it is no link an app takes. Surrounding
/// spaces are ignored. A link of a known kind that is damaged is `None`,
/// not a guess.
pub fn parse_app_link(text: &str) -> Option<AppLink> {
    let text = text.trim();
    if let Some(found) = invite::parse_link(text) {
        return Some(AppLink::Invite { link: found.link() });
    }
    if let Some(found) = device_sign_in::parse_link(text) {
        return Some(AppLink::Approve {
            server: found.server,
            code: found.code,
        });
    }
    if pairing::parse_link(text).is_ok() {
        return Some(AppLink::Pair {
            link: text.to_owned(),
        });
    }
    item(text)
}

/// `text` is an invite link, in either form.
pub fn is_invite_link(text: &str) -> bool {
    invite::parse_link(text).is_some()
}

/// The https form of invite `link` to send as text: Messages and Mail
/// make it tappable, and it opens the app. `None` when `link` is no
/// invite.
pub fn invite_share_link(link: &str) -> Option<String> {
    invite::parse_link(link).map(|found| found.share_link())
}

fn item(text: &str) -> Option<AppLink> {
    let query = text.strip_prefix(ITEM_PREFIX)?;
    let (mut server, mut id, mut play) = (None, None, false);
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        match pair.split_once('=').unwrap_or((pair, "")) {
            ("server", value)
                if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                server = Some(value.to_ascii_lowercase());
            }
            ("server", _) => return None,
            ("id", value) => id = Some(value.parse::<MediaId>().ok()?),
            ("play", value) => play = value == "1",
            // A parameter a newer app added.
            _ => {}
        }
    }
    Some(AppLink::Item {
        server,
        id: id?,
        play,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn an_https_invite_is_the_link_it_carries() {
        let opened = parse_app_link(&format!(
            "https://spoolframe.app/i#server={SERVER}&cap=QW7K-2MFP-XR4T"
        ));
        assert_eq!(
            opened,
            Some(AppLink::Invite {
                link: format!("spoolframe://invite?server={SERVER}&cap=QW7K-2MFP-XR4T")
            })
        );
    }

    /// The Apple app also took `www.spoolframe.app`, which no invite is
    /// sent from; the protocol takes the site alone, and another site
    /// never.
    #[test]
    fn an_invite_on_another_host_is_not_one() {
        for host in ["www.spoolframe.app", "spoolframe.app.example.com"] {
            let text = format!("https://{host}/i#server={SERVER}");
            assert_eq!(
                (parse_app_link(&text), is_invite_link(&text)),
                (None, false),
                "{host}"
            );
        }
    }

    #[test]
    fn an_approval_reads_its_server_and_code() {
        let opened = parse_app_link(&format!(
            " spoolframe://approve?server={SERVER}&code=bcdf-ghjk "
        ));
        assert_eq!(
            opened,
            Some(AppLink::Approve {
                server: SERVER.into(),
                code: "BCDFGHJK".into()
            })
        );
    }

    #[test]
    fn an_item_is_opened_or_played() {
        let opened = parse_app_link(&format!(
            "spoolframe://item?server={SERVER}&id=plex:movie:1&play=1"
        ));
        assert_eq!(
            opened,
            Some(AppLink::Item {
                server: Some(SERVER.into()),
                id: "plex:movie:1".parse().unwrap(),
                play: true
            })
        );
    }

    #[test]
    fn an_item_without_an_id_is_no_link() {
        assert_eq!(parse_app_link("spoolframe://item?play=1"), None);
    }

    #[test]
    fn an_invite_is_shared_in_its_https_form() {
        let link = format!("spoolframe://invite?server={SERVER}");
        assert_eq!(
            invite_share_link(&link),
            Some(format!("https://spoolframe.app/i#server={SERVER}"))
        );
    }
}
