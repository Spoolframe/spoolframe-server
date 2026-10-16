//! What the pages about connected apps say (PX-42), apart from the
//! pages: the consent page's words for each scope, which of them are
//! granted, the address an MCP client is given in Settings › Remote
//! access, and Connected apps' and the audit list's lines.

/// What a scope lets an app do, as consent words it: a title and a line.
/// `None` for one this page does not know, which it never offers.
pub fn scope_words(scope: &str) -> Option<(&'static str, &'static str)> {
    match scope {
        "library:read" => Some((
            "See your library and what you watch",
            "Search, item details, what's new, what you're watching and what you watched.",
        )),
        "playlists:write" => Some((
            "Change your playlists and watched marks",
            "Make playlists, add, move and remove what is in them, pin them, and mark items watched or unwatched.",
        )),
        "admin" => Some((
            "Manage the server as an admin",
            "List users and disable them, end their sign-ins and playbacks, and read the server's log. It asks you before each change that cannot be undone.",
        )),
        _ => None,
    }
}

/// The scopes to grant: those `offered` this page knows, less the ones
/// the user `unticked`, in the order offered.
pub fn granted(offered: &[String], unticked: &[String]) -> Vec<String> {
    offered
        .iter()
        .filter(|s| scope_words(s).is_some() && !unticked.contains(s))
        .cloned()
        .collect()
}

/// The address an MCP client connects to: the web UI's public URL if it
/// has one, else the page's own origin, and `/mcp`.
pub fn mcp_url(public_url: Option<&str>, page_origin: &str) -> String {
    let base = public_url.unwrap_or(page_origin).trim_end_matches('/');
    format!("{base}/mcp")
}

/// Where a sign-in on the consent page comes back to: the same request.
pub fn return_to(request: &str) -> String {
    let id: String = request
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    format!("/connect?request={id}")
}

/// A scope as Connected apps names it, short.
pub fn scope_name(scope: &str) -> &str {
    match scope {
        "library:read" => "Library",
        "playlists:write" => "Playlists",
        "admin" => "Admin",
        other => other,
    }
}

/// An app's calls today, saying the day is UTC's: the count starts over
/// at midnight UTC, not at the reader's.
pub fn calls_line(calls_today: u32) -> String {
    match calls_today {
        0 => "No calls today (UTC)".into(),
        1 => "1 call today (UTC)".into(),
        n => format!("{n} calls today (UTC)"),
    }
}

/// What an audit list entry did, in words: an app connected or
/// disconnected, or an MCP tool called by its name.
pub fn audit_action(action: &str) -> String {
    match action {
        "app.connect" => "Connected an app".into(),
        "app.revoke" => "Disconnected an app".into(),
        tool => format!("Called {tool}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_scope_the_server_offers_has_words() {
        for scope in ["library:read", "playlists:write", "admin"] {
            assert!(scope_words(scope).is_some(), "{scope}");
        }
        assert_eq!(scope_words("playback"), None);
    }

    #[test]
    fn what_is_granted_is_what_stays_ticked_of_what_was_offered() {
        let offered: Vec<String> = ["library:read", "playlists:write", "mystery"]
            .map(str::to_owned)
            .into();
        assert_eq!(granted(&offered, &[]), ["library:read", "playlists:write"]);
        assert_eq!(
            granted(&offered, &["library:read".to_owned()]),
            ["playlists:write"]
        );
        let all: Vec<String> = offered.clone();
        assert!(granted(&offered, &all).is_empty());
    }

    #[test]
    fn the_mcp_address_is_the_public_one_when_there_is_one() {
        assert_eq!(
            mcp_url(Some("https://media.example.org/"), "http://10.0.0.5:7421"),
            "https://media.example.org/mcp"
        );
        assert_eq!(
            mcp_url(None, "http://10.0.0.5:7421"),
            "http://10.0.0.5:7421/mcp"
        );
    }

    #[test]
    fn calls_today_says_the_day_is_utc_s() {
        assert_eq!(calls_line(0), "No calls today (UTC)");
        assert_eq!(calls_line(1), "1 call today (UTC)");
        assert_eq!(calls_line(12), "12 calls today (UTC)");
    }

    #[test]
    fn an_audit_entry_says_what_was_done() {
        assert_eq!(audit_action("app.revoke"), "Disconnected an app");
        assert_eq!(audit_action("disable_user"), "Called disable_user");
        assert_eq!(scope_name("playlists:write"), "Playlists");
    }

    #[test]
    fn a_sign_in_comes_back_to_the_same_request_and_nowhere_else() {
        assert_eq!(return_to("ab12"), "/connect?request=ab12");
        assert_eq!(
            return_to("ab12&next=//evil.example"),
            "/connect?request=ab12nextevilexample"
        );
    }
}
