//! Sign-in providers in the pages (P3-04): what the server's callback
//! says happened (`?sso=…&provider=…` on /login or /profile), in words.

/// How a message is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Good,
    Bad,
}

/// What `?sso=` and `?provider=` in `search` (`location.search`) say, if
/// anything.
pub fn outcome(search: &str) -> Option<(String, Option<String>)> {
    let mut what = None;
    let mut provider = None;
    for pair in search.trim_start_matches('?').split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = decode(value);
        match key {
            "sso" => what = Some(value),
            "provider" => provider = Some(value).filter(|p| !p.is_empty()),
            _ => {}
        }
    }
    what.filter(|w| !w.is_empty()).map(|w| (w, provider))
}

/// `search` without what the callback said (`sso`, `provider`), as
/// `?…`, or empty: the page's own query, which a page the sign-in began
/// on (`/auth/device?code=…`) still needs.
pub fn without_outcome(search: &str) -> String {
    let kept: Vec<&str> = search
        .trim_start_matches('?')
        .split('&')
        .filter(|pair| {
            let key = pair.split_once('=').map_or(*pair, |(key, _)| key);
            !pair.is_empty() && key != "sso" && key != "provider"
        })
        .collect();
    if kept.is_empty() {
        String::new()
    } else {
        format!("?{}", kept.join("&"))
    }
}

/// `application/x-www-form-urlencoded` text decoded: `+` is a space, and
/// `%XX` a byte.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                (Some(high), Some(low)) => {
                    out.push(high << 4 | low);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

/// The login page's message for what happened (the WebLoginPhone board's
/// for an account that is not linked).
pub fn login_message(what: &str, provider: Option<&str>) -> Option<(Tone, String)> {
    let name = provider.unwrap_or("The sign-in provider");
    let text = match what {
        "unlinked" => "This account is not linked to a Spoolframe user. Sign in another way and link it in My profile, or ask your admin.".to_owned(),
        "unavailable" => format!("{name} can't be reached right now. Sign in another way, or try again later."),
        "failed" => format!("{name}'s answer could not be trusted, so nobody was signed in. Try again, or sign in another way."),
        "expired" => "That sign-in took too long, or was begun in another browser. Start again.".into(),
        "cancelled" => "Signing in with the provider was cancelled.".into(),
        _ => return None,
    };
    Some((Tone::Bad, text))
}

/// My profile's message for what happened.
pub fn profile_message(what: &str, provider: Option<&str>) -> Option<(Tone, String)> {
    let name = provider.unwrap_or("the sign-in provider");
    let (tone, text) = match what {
        "linked" => (
            Tone::Good,
            format!("{name} is linked. You can sign in with it now."),
        ),
        "confirmed" => (Tone::Good, "Confirmed. Go on with your change.".into()),
        "taken" => (
            Tone::Bad,
            format!(
                "That {name} account is linked to another Spoolframe user, so it was not linked to yours."
            ),
        ),
        "already" => (
            Tone::Bad,
            format!("You have a {name} account linked already. Unlink it first to link another."),
        ),
        "notyours" => (
            Tone::Bad,
            format!("That {name} account is not linked to you, so it confirms nothing."),
        ),
        "unavailable" => (
            Tone::Bad,
            format!("{name} can't be reached right now. Try again later."),
        ),
        "failed" => (
            Tone::Bad,
            format!("{name}'s answer could not be trusted, so nothing was changed."),
        ),
        "expired" => (
            Tone::Bad,
            "That took too long, or was begun in another browser. Start again.".into(),
        ),
        "cancelled" => (
            Tone::Bad,
            "Cancelled at the provider. Nothing was changed.".into(),
        ),
        _ => return None,
    };
    Some((tone, text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_callback_s_query_is_read() {
        assert_eq!(
            outcome("?sso=linked&provider=Pocket+ID"),
            Some(("linked".into(), Some("Pocket ID".into())))
        );
        assert_eq!(
            outcome("?provider=A%C3%A9&sso=unlinked"),
            Some(("unlinked".into(), Some("Aé".into())))
        );
        assert_eq!(outcome("?sso=expired"), Some(("expired".into(), None)));
        assert_eq!(outcome(""), None);
        assert_eq!(outcome("?code=1"), None);
        assert_eq!(
            outcome("?sso=x&provider=100%"),
            Some(("x".into(), Some("100%".into())))
        );
    }

    #[test]
    fn the_page_s_own_query_stays_when_the_callback_s_is_taken_out() {
        assert_eq!(
            without_outcome("?code=BCDF-GHJK&sso=unlinked&provider=Pocket+ID"),
            "?code=BCDF-GHJK"
        );
        assert_eq!(without_outcome("?sso=linked&provider=Pocket+ID"), "");
        assert_eq!(without_outcome(""), "");
        assert_eq!(without_outcome("?a=1&sso=x&b=2"), "?a=1&b=2");
    }

    #[test]
    fn an_unlinked_account_gets_the_board_s_words() {
        let (tone, text) = login_message("unlinked", Some("Pocket ID")).unwrap();
        assert_eq!(tone, Tone::Bad);
        assert_eq!(
            text,
            "This account is not linked to a Spoolframe user. Sign in another way and link it in My profile, or ask your admin."
        );
    }

    #[test]
    fn messages_name_the_provider_and_say_good_from_bad() {
        assert_eq!(
            profile_message("linked", Some("Pocket ID")),
            Some((
                Tone::Good,
                "Pocket ID is linked. You can sign in with it now.".into()
            ))
        );
        assert_eq!(
            profile_message("taken", Some("Pocket ID")).unwrap().0,
            Tone::Bad
        );
        assert!(
            login_message("unavailable", Some("Pocket ID"))
                .unwrap()
                .1
                .starts_with("Pocket ID can't be reached")
        );
        assert_eq!(login_message("linked", None), None);
        assert_eq!(profile_message("nonsense", None), None);
    }
}
