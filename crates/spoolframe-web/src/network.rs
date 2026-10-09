//! Settings' network fields (PX-22): the relays and the pkarr record as
//! the forms show them, and back. The server checks what is saved as it
//! checks the config file; these only split and join.

use serde_json::Value;

/// n0's public relays, as `network.relays` names them.
pub const N0_RELAYS: &str = "default";

/// `network.relays` as the form shows it: whether n0's public relays are
/// on, and the relay URLs of the operator's own. A config file may name
/// one relay as a plain string, which the server reads as a list of one.
pub fn relays_form(value: Option<&Value>) -> (bool, Vec<String>) {
    let all: Vec<String> = match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        Some(Value::String(one)) => vec![one.clone()],
        _ => Vec::new(),
    };
    let n0 = all.iter().any(|r| r == N0_RELAYS);
    (n0, all.into_iter().filter(|r| r != N0_RELAYS).collect())
}

/// The form's relays as `network.relays`: n0's first, when on.
pub fn relays_list(n0: bool, urls: &[String]) -> Vec<String> {
    let mut list = Vec::with_capacity(urls.len() + 1);
    if n0 {
        list.push(N0_RELAYS.to_owned());
    }
    list.extend(urls.iter().filter(|u| *u != N0_RELAYS).cloned());
    list
}

/// Where the pkarr record goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pkarr {
    Off,
    /// n0's public service, dns.iroh.link.
    N0,
    /// A pkarr relay of the operator's own, at a URL.
    Own,
}

/// `network.pkarr` as the form shows it: the choice, and the URL typed
/// for one's own.
pub fn pkarr_form(value: Option<&Value>) -> (Pkarr, String) {
    match value.and_then(Value::as_str).map(str::trim) {
        None | Some("") | Some("off") => (Pkarr::Off, String::new()),
        Some("n0") => (Pkarr::N0, String::new()),
        Some(url) => (Pkarr::Own, url.to_owned()),
    }
}

/// The form's choice as `network.pkarr`; `Err` for one's own with no URL.
pub fn pkarr_setting(choice: Pkarr, url: &str) -> Result<Value, String> {
    match choice {
        Pkarr::Off => Ok(Value::String("off".into())),
        Pkarr::N0 => Ok(Value::String("n0".into())),
        Pkarr::Own => match url.trim() {
            "" => Err("Type your pkarr relay's URL, such as https://dns.example.org/pkarr.".into()),
            url => Ok(Value::String(url.to_owned())),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_relay_written_as_a_string_is_a_list_of_one() {
        let one = Value::String("https://relay.example.org".into());
        assert_eq!(
            relays_form(Some(&one)),
            (false, vec!["https://relay.example.org".to_owned()])
        );
        let n0 = Value::String(N0_RELAYS.into());
        assert_eq!(relays_form(Some(&n0)), (true, Vec::new()));
    }
    use serde_json::json;

    #[test]
    fn n0s_relays_are_a_switch_and_the_rest_a_list() {
        let value = json!(["default", "https://relay.example.org"]);
        assert_eq!(
            relays_form(Some(&value)),
            (true, vec!["https://relay.example.org".to_owned()])
        );
        assert_eq!(relays_form(None), (false, Vec::new()));
    }

    #[test]
    fn the_relays_saved_put_n0s_first() {
        let urls = vec!["https://relay.example.org".to_owned()];
        assert_eq!(
            relays_list(true, &urls),
            ["default", "https://relay.example.org"]
        );
        assert_eq!(relays_list(false, &urls), ["https://relay.example.org"]);
        assert!(relays_list(false, &[]).is_empty());
    }

    #[test]
    fn pkarr_reads_as_off_n0_or_a_url() {
        assert_eq!(pkarr_form(Some(&json!("off"))), (Pkarr::Off, String::new()));
        assert_eq!(pkarr_form(None), (Pkarr::Off, String::new()));
        assert_eq!(pkarr_form(Some(&json!("n0"))), (Pkarr::N0, String::new()));
        assert_eq!(
            pkarr_form(Some(&json!("https://dns.example.org/pkarr"))),
            (Pkarr::Own, "https://dns.example.org/pkarr".to_owned())
        );
    }

    #[test]
    fn pkarr_of_ones_own_needs_its_url() {
        assert!(pkarr_setting(Pkarr::Own, "  ").is_err());
        assert_eq!(
            pkarr_setting(Pkarr::Own, " https://dns.example.org/pkarr ").unwrap(),
            json!("https://dns.example.org/pkarr")
        );
        assert_eq!(pkarr_setting(Pkarr::N0, "").unwrap(), json!("n0"));
        assert_eq!(pkarr_setting(Pkarr::Off, "x").unwrap(), json!("off"));
    }
}
