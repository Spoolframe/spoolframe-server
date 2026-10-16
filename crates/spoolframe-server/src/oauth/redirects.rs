//! The apps allowed to sign in (`mcp.allowed_redirects`): each entry a
//! name and a pattern of the redirect URIs it returns to. Registration
//! takes a client only if one of its redirect URIs matches an entry, and
//! `/authorize` checks again, so an entry removed stops new sign-ins.
//!
//! A pattern is a redirect URI with two wildcards and nothing else:
//!
//! - `*` as the port matches any port, or none (`http://localhost:*/callback`):
//!   a native app picks its port when it listens (RFC 8252 §7.3);
//! - a path ending in `*` matches every path that starts with what is before
//!   it (`/callback/*` matches `/callback/abc`, not `/callback`).
//!
//! The scheme and the host match exactly, the host in any case. A pattern
//! has no query; a redirect URI with a query matches only a pattern with
//! neither wildcard, so a wildcard never lets an app choose its own query.

use std::fmt;

/// The most entries `mcp.allowed_redirects` holds.
pub const MAX_ALLOWED_REDIRECTS: usize = 50;
/// The longest name an entry has, in characters.
pub const MAX_REDIRECT_NAME: usize = 64;

/// The port a pattern takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Port {
    /// `*`: any port, or none.
    Any,
    /// This port; the scheme's own when the pattern names none.
    Exactly(u16),
}

/// One entry of `mcp.allowed_redirects`, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedirectPattern {
    name: String,
    /// As the admin wrote it.
    text: String,
    scheme: String,
    /// As `url` writes a host: lower case, an IPv6 address in brackets.
    host: String,
    port: Port,
    /// The path, without the trailing `*` when `prefix`.
    path: String,
    prefix: bool,
}

impl fmt::Display for RedirectPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.name, self.text)
    }
}

impl RedirectPattern {
    /// The entry `name` allowing `pattern`, or why it cannot be one: a
    /// name of 1 to [`MAX_REDIRECT_NAME`] characters, and a pattern that is
    /// https, or plain http on loopback (the rule registration checks too,
    /// `super::redirect_scheme_refused`), with no credentials, query or
    /// fragment.
    pub fn parse(name: &str, pattern: &str) -> Result<Self, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err(format!("{pattern:?} has no name"));
        }
        if name.chars().count() > MAX_REDIRECT_NAME || name.chars().any(char::is_control) {
            return Err(format!(
                "{name:?}: a name is 1 to {MAX_REDIRECT_NAME} characters, none of them control \
                 characters"
            ));
        }
        let text = pattern.trim();
        let wrong = |why: &str| Err(format!("{name}: {text:?} {why}"));
        let Some((scheme, rest)) = text.split_once("://") else {
            return wrong("is not a URL (scheme://host/path)");
        };
        let scheme = scheme.to_ascii_lowercase();
        if rest.contains('#') {
            return wrong("has a fragment");
        }
        if rest.contains('?') {
            return wrong("has a query, which a pattern cannot");
        }
        let (authority, path) = match rest.find('/') {
            Some(at) => rest.split_at(at),
            None => (rest, "/"),
        };
        if authority.contains('@') {
            return wrong("carries credentials");
        }
        // The port is what follows the last colon outside an IPv6
        // address's brackets.
        let (host, port) = match authority.rfind(':') {
            Some(at) if !authority[at..].contains(']') => {
                (&authority[..at], Some(&authority[at + 1..]))
            }
            _ => (authority, None),
        };
        if host.contains('*') {
            return wrong("has * in the host; only the port and the end of the path take it");
        }
        // The host as `url` writes it, through the URL without the port.
        let Ok(bare) = url::Url::parse(&format!("{scheme}://{host}/")) else {
            return wrong("is not a URL (scheme://host/path)");
        };
        let Some(host) = bare.host_str().map(str::to_owned) else {
            return wrong("has no host");
        };
        if let Some(why) = super::redirect_scheme_refused(&scheme, bare.host()) {
            return wrong(&why);
        }
        let port = match port {
            Some("*") => Port::Any,
            Some(digits) => match digits.parse::<u16>() {
                Ok(n) if n > 0 && digits.bytes().all(|b| b.is_ascii_digit()) => Port::Exactly(n),
                _ => return wrong("has a port that is neither a number nor *"),
            },
            None => Port::Exactly(
                bare.port_or_known_default()
                    .expect("http and https have a port"),
            ),
        };
        let (path, prefix) = match path.strip_suffix('*') {
            Some(start) => (start, true),
            None => (path, false),
        };
        if path.contains('*') {
            return wrong("has * inside the path; only its end takes it");
        }
        // The path as `url` writes it, so `/a%20b` and `/a b` are one.
        let Ok(written) = url::Url::parse(&format!("{scheme}://{host}{path}")) else {
            return wrong("has a path that is not a URL's");
        };
        Ok(Self {
            name: name.to_owned(),
            text: text.to_owned(),
            scheme,
            host,
            port,
            path: written.path().to_owned(),
            prefix,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The pattern as the admin wrote it.
    pub fn pattern(&self) -> &str {
        &self.text
    }

    /// Whether the redirect URI `uri` is one this entry allows.
    pub fn matches(&self, uri: &str) -> bool {
        let Ok(url) = url::Url::parse(uri) else {
            return false;
        };
        if url.fragment().is_some() || !url.username().is_empty() || url.password().is_some() {
            return false;
        }
        if url.scheme() != self.scheme || url.host_str() != Some(self.host.as_str()) {
            return false;
        }
        let port = match self.port {
            Port::Any => true,
            Port::Exactly(n) => url.port_or_known_default() == Some(n),
        };
        let path = if self.prefix {
            url.path().starts_with(&self.path)
        } else {
            url.path() == self.path
        };
        let query = url.query().is_none() || (!self.prefix && self.port != Port::Any);
        port && path && query
    }
}

/// Whether one of `allowed` matches `uri`.
pub fn allowed(allowed: &[RedirectPattern], uri: &str) -> bool {
    allowed.iter().any(|p| p.matches(uri))
}

/// `entries` (name, pattern) checked as `mcp.allowed_redirects`: each one
/// a pattern, and at most [`MAX_ALLOWED_REDIRECTS`]. None is allowed, and
/// means no app signs in.
pub fn parse_all<'a>(
    entries: impl ExactSizeIterator<Item = (&'a str, &'a str)>,
) -> Result<Vec<RedirectPattern>, String> {
    if entries.len() > MAX_ALLOWED_REDIRECTS {
        return Err(format!(
            "{} entries; at most {MAX_ALLOWED_REDIRECTS}",
            entries.len()
        ));
    }
    entries
        .map(|(name, pattern)| RedirectPattern::parse(name, pattern))
        .collect()
}

/// The entries a server has until an admin changes them: Claude Code,
/// Claude on the web and its desktop app, and Codex.
pub fn defaults() -> Vec<RedirectPattern> {
    // The list is in spoolframe-admin-api, for the web UI's "Restore".
    spoolframe_admin_api::mcp::DEFAULT_ALLOWED_REDIRECTS
        .iter()
        .map(|(name, pattern)| RedirectPattern::parse(name, pattern).expect("a default parses"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(text: &str) -> RedirectPattern {
        RedirectPattern::parse("App", text).unwrap()
    }

    #[test]
    fn a_star_port_matches_any_port_and_none() {
        let p = pattern("http://localhost:*/callback");
        assert!(p.matches("http://localhost:53123/callback"));
        assert!(p.matches("http://localhost:1/callback"));
        assert!(p.matches("http://localhost/callback"));
        assert!(!p.matches("http://localhost:53123/callback/x"));
        assert!(!p.matches("http://127.0.0.1:53123/callback"));
    }

    #[test]
    fn a_port_written_out_matches_only_itself() {
        let p = pattern("http://127.0.0.1:8080/cb");
        assert!(p.matches("http://127.0.0.1:8080/cb"));
        assert!(!p.matches("http://127.0.0.1:8081/cb"));
        assert!(!p.matches("http://127.0.0.1/cb"));
    }

    #[test]
    fn no_port_is_the_scheme_s_own() {
        let p = pattern("https://claude.ai/api/mcp/auth_callback");
        assert!(p.matches("https://claude.ai/api/mcp/auth_callback"));
        assert!(p.matches("https://claude.ai:443/api/mcp/auth_callback"));
        assert!(!p.matches("https://claude.ai:8443/api/mcp/auth_callback"));
    }

    #[test]
    fn a_path_ending_in_a_star_matches_what_starts_with_it() {
        let p = pattern("http://127.0.0.1:*/callback/*");
        assert!(p.matches("http://127.0.0.1:40000/callback/abc123"));
        assert!(p.matches("http://127.0.0.1:40000/callback/"));
        assert!(!p.matches("http://127.0.0.1:40000/callback"));
        assert!(!p.matches("http://127.0.0.1:40000/callbacks/abc"));
    }

    #[test]
    fn a_path_without_a_star_matches_only_itself() {
        let p = pattern("https://claude.ai/api/mcp/auth_callback");
        assert!(!p.matches("https://claude.ai/api/mcp/auth_callback/x"));
        assert!(!p.matches("https://claude.ai/api/mcp/auth_callbac"));
        assert!(!p.matches("https://claude.ai/"));
    }

    #[test]
    fn the_host_matches_in_any_case_and_nothing_else() {
        let p = pattern("https://Claude.AI/cb");
        assert!(p.matches("https://claude.ai/cb"));
        assert!(p.matches("https://CLAUDE.ai/cb"));
        assert!(!p.matches("https://evil.claude.ai/cb"));
        assert!(!p.matches("https://claude.ai.evil.example/cb"));
    }

    #[test]
    fn another_scheme_does_not_match() {
        let p = pattern("https://claude.ai/cb");
        assert!(!p.matches("http://claude.ai/cb"));
        let p = pattern("http://localhost:*/cb");
        assert!(!p.matches("https://localhost:4000/cb"));
    }

    #[test]
    fn a_query_matches_only_an_exact_pattern() {
        assert!(pattern("https://claude.ai/cb").matches("https://claude.ai/cb?x=1"));
        assert!(!pattern("http://localhost:*/cb").matches("http://localhost:4000/cb?x=1"));
        assert!(!pattern("https://claude.ai/cb/*").matches("https://claude.ai/cb/a?x=1"));
    }

    #[test]
    fn a_fragment_or_credentials_never_match() {
        let p = pattern("https://claude.ai/cb");
        assert!(!p.matches("https://claude.ai/cb#x"));
        assert!(!p.matches("https://me:pw@claude.ai/cb"));
    }

    #[test]
    fn ipv6_loopback_takes_a_star_port() {
        let p = pattern("http://[::1]:*/cb");
        assert!(p.matches("http://[::1]:5000/cb"));
        assert!(!p.matches("http://[::2]:5000/cb"));
    }

    #[test]
    fn a_pattern_is_https_or_http_on_loopback() {
        for ok in [
            "https://example.com/cb",
            "http://localhost:*/cb",
            "http://127.0.0.1:*/cb/*",
            "http://[::1]/cb",
        ] {
            RedirectPattern::parse("App", ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in [
            "http://example.com/cb",
            "http://192.168.1.2:*/cb",
            "ftp://example.com/cb",
            "myapp://cb",
            "https://example.com/cb#x",
            "https://example.com/cb?x=1",
            "https://u:p@example.com/cb",
            "https://*.example.com/cb",
            "https://example.com/*/cb",
            "https://example.com:http/cb",
            "https://example.com:0/cb",
            "example.com/cb",
            "https:///cb",
        ] {
            assert!(RedirectPattern::parse("App", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn an_entry_needs_a_name() {
        assert!(RedirectPattern::parse("  ", "https://example.com/cb").is_err());
        assert!(RedirectPattern::parse(&"x".repeat(65), "https://example.com/cb").is_err());
    }

    #[test]
    fn a_list_is_at_most_fifty_and_may_be_empty() {
        assert!(parse_all(std::iter::empty()).unwrap().is_empty());
        let many = vec![("App", "https://example.com/cb"); MAX_ALLOWED_REDIRECTS + 1];
        assert!(parse_all(many.iter().copied()).is_err());
        assert_eq!(
            parse_all(many[..MAX_ALLOWED_REDIRECTS].iter().copied())
                .unwrap()
                .len(),
            MAX_ALLOWED_REDIRECTS
        );
    }

    /// What Claude Code, claude.ai and `codex mcp login` send match the
    /// defaults.
    #[test]
    fn the_defaults_let_claude_and_codex_in() {
        let defaults = defaults();
        for uri in [
            "http://localhost:61234/callback",
            "https://claude.ai/api/mcp/auth_callback",
            "http://127.0.0.1:43210/callback/q2Xr8mVb0kQx",
        ] {
            assert!(allowed(&defaults, uri), "{uri}");
        }
        assert!(!allowed(&defaults, "https://evil.example/cb"));
        assert!(!allowed(&defaults, "http://localhost:61234/other"));
    }
}
