//! Settings' network fields (PX-22): the relays and the pkarr record as
//! the forms show them, and back. The server checks what is saved as it
//! checks the config file; these only split and join.

use serde_json::Value;

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

    use serde_json::json;

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
