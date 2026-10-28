//! Signing a device in by approving it on another: the code a TV
//! shows, and the link its QR code carries.
//!
//! The code is 8 letters from an alphabet without vowels (so it never
//! spells a word), shown as two groups of four: `BCDF-GHJK`. 20^8 codes,
//! a 5-minute life and rate-limited guessing put a guessed code out of
//! reach. People type it with any case, dash or spacing; [`normalize`]
//! takes all of them. The server makes codes; everyone reads them here.

/// The letters a code is made of.
pub const ALPHABET: &[u8; 20] = b"BCDFGHJKLMNPQRSTVWXZ";

/// Letters in a code.
pub const CODE_LEN: usize = 8;

const LINK_PREFIX: &str = "spoolframe://approve?";

/// `input` as a code (`BCDFGHJK`), whatever case, dashes or spaces it was
/// typed with; `None` if it cannot be one.
pub fn normalize(input: &str) -> Option<String> {
    let code: String = input
        .chars()
        .filter(|c| !matches!(c, '-' | ' '))
        .map(|c| c.to_ascii_uppercase())
        .collect();
    (code.len() == CODE_LEN && code.bytes().all(|b| ALPHABET.contains(&b))).then_some(code)
}

/// `code` as shown: `BCDF-GHJK`.
pub fn display(code: &str) -> String {
    match code.split_at_checked(CODE_LEN / 2) {
        Some((a, b)) => format!("{a}-{b}"),
        None => code.to_owned(),
    }
}

/// What the device's QR code carries: the server to approve on (its hex
/// `EndpointId`, so a phone that has not seen it can connect and pin it)
/// and the code.
pub fn link(server: &str, code: &str) -> String {
    format!("{LINK_PREFIX}server={server}&code={code}")
}

/// The page of the web UI at `origin` (`https://media.example.org`) that
/// approves `code`: what the QR code carries for a phone with a browser
/// and no Spoolframe app.
pub fn web_link(origin: &str, code: &str) -> String {
    format!("{origin}/auth/device?code={code}")
}

/// A link a phone scanned or was opened with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// Hex `EndpointId`.
    pub server: String,
    /// Normalized.
    pub code: String,
}

/// The server and code of a [`link`]; `None` if `text` is not one.
pub fn parse_link(text: &str) -> Option<Link> {
    let query = text.trim().strip_prefix(LINK_PREFIX)?;
    let (mut server, mut code) = (None, None);
    for pair in query.split('&') {
        match pair.split_once('=')? {
            ("server", value) => server = Some(value),
            ("code", value) => code = Some(value),
            // A parameter a newer server added.
            _ => {}
        }
    }
    let server = server?;
    if server.len() != 64 || !server.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(Link {
        server: server.to_ascii_lowercase(),
        code: normalize(code?)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn a_code_is_read_however_it_is_typed() {
        for typed in ["BCDF-GHJK", "bcdfghjk", "bcdf ghjk", " BcDf-gHjK "] {
            assert_eq!(normalize(typed).as_deref(), Some("BCDFGHJK"), "{typed:?}");
        }
    }

    #[test]
    fn a_code_with_a_vowel_or_the_wrong_length_is_not_one() {
        for typed in ["BCDF-GHJA", "BCDFGHJ", "BCDFGHJKL", "", "BCDF-GHJ1"] {
            assert_eq!(normalize(typed), None, "{typed:?}");
        }
    }

    #[test]
    fn a_code_shows_in_two_groups() {
        assert_eq!(display("BCDFGHJK"), "BCDF-GHJK");
    }

    #[test]
    fn a_link_reads_back() {
        let text = link(SERVER, "BCDFGHJK");
        assert_eq!(
            text,
            format!("spoolframe://approve?server={SERVER}&code=BCDFGHJK")
        );
        assert_eq!(
            parse_link(&text),
            Some(Link {
                server: SERVER.into(),
                code: "BCDFGHJK".into()
            })
        );
    }

    #[test]
    fn a_web_link_is_the_approval_page_with_the_code() {
        assert_eq!(
            web_link("https://media.example.org", "BCDFGHJK"),
            "https://media.example.org/auth/device?code=BCDFGHJK"
        );
    }

    #[test]
    fn a_link_takes_its_parameters_in_any_order_and_ignores_new_ones() {
        let text = format!("spoolframe://approve?code=bcdf-ghjk&v=2&server={SERVER}");
        assert_eq!(parse_link(&text).unwrap().code, "BCDFGHJK");
    }

    #[test]
    fn anything_else_is_not_a_link() {
        for text in [
            "https://example.com/approve?server=x&code=BCDFGHJK".to_owned(),
            format!("spoolframe://approve?server={SERVER}"),
            "spoolframe://approve?server=abc&code=BCDFGHJK".to_owned(),
            format!("spoolframe://approve?server={SERVER}&code=AAAA"),
        ] {
            assert_eq!(parse_link(&text), None, "{text}");
        }
    }
}
