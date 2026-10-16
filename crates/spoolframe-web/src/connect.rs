//! What the pages about connected apps say (PX-42), apart from the
//! pages: the consent page's words for each scope, which of them are
//! granted, the address an MCP client is given in Settings › MCP and the
//! commands that add it to Claude Code and Codex, the list of apps allowed
//! to sign in there, and Connected apps' and the audit list's lines.

use spoolframe_admin_api::mcp::{AllowedRedirect, default_allowed_redirects};

/// What a scope lets an app do, as consent words it: a title and a line.
/// `None` for one this page does not know, which it never offers.
pub fn scope_words(scope: &str) -> Option<(&'static str, &'static str)> {
    match scope {
        "library:read" => Some((
            "See your library and what you watch",
            "Search, item details, what's new, what you're watching and what you watched.",
        )),
        "playlists:read" => Some((
            "See your playlists",
            "Your playlists, those shared with you and the server's, and what is in them. It changes nothing.",
        )),
        "playlists:write" => Some((
            "Change your playlists and watched marks",
            "Make, rename, share and delete playlists, add, move and remove what is in them, pin them, and mark items watched or unwatched.",
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

/// The name the commands below give this server in the agent's list.
pub const MCP_NAME: &str = "spoolframe";

/// The command that adds the MCP server at `url` to Claude Code. `/mcp`
/// in Claude Code then signs in (OAuth, in the browser).
pub fn claude_code_add(url: &str) -> String {
    format!("claude mcp add --transport http {MCP_NAME} {url}")
}

/// The commands that add the MCP server at `url` to Codex and sign in to
/// it, in that order.
pub fn codex_add(url: &str) -> [String; 2] {
    [
        format!("codex mcp add {MCP_NAME} --url {url}"),
        format!("codex mcp login {MCP_NAME}"),
    ]
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
        "playlists:read" => "Playlists (read)",
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

/// `mcp.allowed_redirects` as Settings › MCP lists it: the entries saved,
/// or the defaults when no layer has it. A value that is not the list
/// shows as empty, not as the defaults: what the editor saves is then
/// what it showed.
pub fn allowed_redirects(value: Option<&serde_json::Value>) -> Vec<AllowedRedirect> {
    match value {
        None | Some(serde_json::Value::Null) => default_allowed_redirects(),
        Some(value) => serde_json::from_value(value.clone()).unwrap_or_default(),
    }
}

/// The default apps (Claude Code, Claude, Codex) no longer on `list`,
/// by pattern: while there is one, "Restore Claude and Codex" is offered.
pub fn missing_defaults(list: &[AllowedRedirect]) -> Vec<AllowedRedirect> {
    default_allowed_redirects()
        .into_iter()
        .filter(|d| !list.iter().any(|e| e.pattern == d.pattern))
        .collect()
}

/// `list` with each default app it lacks put back at the end, the
/// admin's own entries kept.
pub fn restore_defaults(list: &[AllowedRedirect]) -> Vec<AllowedRedirect> {
    [list.to_vec(), missing_defaults(list)].concat()
}

/// `list` with the typed `name` and `pattern` added, both trimmed; or why
/// not. Only what the page can tell: the server checks the pattern. The
/// page offers Add only once both are typed.
pub fn add_redirect(
    list: &[AllowedRedirect],
    name: &str,
    pattern: &str,
) -> Result<Vec<AllowedRedirect>, String> {
    let (name, pattern) = (name.trim(), pattern.trim());
    if list.iter().any(|e| e.pattern == pattern) {
        return Err(format!("{pattern} is listed already."));
    }
    let mut added = list.to_vec();
    added.push(AllowedRedirect {
        name: name.to_owned(),
        pattern: pattern.to_owned(),
    });
    Ok(added)
}

/// `list` as the setting's value.
pub fn redirects_value(list: &[AllowedRedirect]) -> serde_json::Value {
    serde_json::to_value(list).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, pattern: &str) -> AllowedRedirect {
        AllowedRedirect {
            name: name.into(),
            pattern: pattern.into(),
        }
    }

    #[test]
    fn the_allowed_list_is_the_defaults_until_saved() {
        assert_eq!(allowed_redirects(None), default_allowed_redirects());
        let saved = serde_json::json!([{ "name": "Mine", "pattern": "https://a.example/cb" }]);
        assert_eq!(
            allowed_redirects(Some(&saved)),
            [entry("Mine", "https://a.example/cb")]
        );
        assert!(allowed_redirects(Some(&serde_json::json!([]))).is_empty());
        assert_eq!(
            redirects_value(&allowed_redirects(Some(&saved))),
            saved,
            "saved as it was read"
        );
    }

    #[test]
    fn restoring_puts_back_only_the_defaults_missing() {
        let mine = entry("Mine", "https://a.example/cb");
        let defaults = default_allowed_redirects();
        assert!(missing_defaults(&defaults).is_empty());
        let without_codex: Vec<_> = defaults
            .iter()
            .filter(|e| e.name != "Codex")
            .cloned()
            .chain([mine.clone()])
            .collect();
        assert_eq!(
            missing_defaults(&without_codex),
            [entry("Codex", "http://127.0.0.1:*/callback/*")]
        );
        let restored = restore_defaults(&without_codex);
        assert!(missing_defaults(&restored).is_empty());
        assert_eq!(restored.len(), defaults.len() + 1);
        assert!(restored.contains(&mine));
        assert_eq!(restore_defaults(&restored), restored);
    }

    #[test]
    fn an_entry_added_needs_a_new_pattern() {
        let list = add_redirect(&[], " Mine ", " https://a.example/cb ").unwrap();
        assert_eq!(list, [entry("Mine", "https://a.example/cb")]);
        assert_eq!(
            add_redirect(&list, "Again", " https://a.example/cb"),
            Err("https://a.example/cb is listed already.".to_owned())
        );
    }

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
    fn the_agents_are_given_the_mcp_address() {
        let url = mcp_url(Some("https://media.example.org"), "http://10.0.0.5:7421");
        assert_eq!(
            claude_code_add(&url),
            "claude mcp add --transport http spoolframe https://media.example.org/mcp"
        );
        assert_eq!(
            codex_add(&url),
            [
                "codex mcp add spoolframe --url https://media.example.org/mcp".to_owned(),
                "codex mcp login spoolframe".to_owned(),
            ]
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
