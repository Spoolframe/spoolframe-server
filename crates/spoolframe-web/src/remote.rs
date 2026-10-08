//! Settings › Remote access and the wizard's remote choice (P6-02): the
//! form as the `network.wan` setting, and back, and what the status card
//! says. The setting is one table, saved whole; the server checks it as
//! it checks the config file, and a typed address is refused here with
//! the same words (`spoolframe_admin_api::wan`).

use serde_json::{Value, json};
use spoolframe_admin_api::{AddressSource, LookupStatus, MappingStatus, wan};

/// How the mapped port is opened on the router.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opened {
    /// The server asks the router (`mode = "auto"`).
    Automatic,
    /// The operator forwarded it (`mode = "manual"`).
    Forwarded,
}

/// Where the mapped port's public IP comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ip {
    LookUp,
    Typed,
}

/// The Remote access form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WanForm {
    /// "Advertise a public address": `network.wan` present.
    pub on: bool,
    /// "Mapped port"; off writes `mapped_port = false`.
    pub mapped: bool,
    /// The public port; empty takes the server's own.
    pub port: String,
    pub opened: Opened,
    pub ip: Ip,
    pub typed_ip: String,
    /// The lookup service: wtfismyip, or none (`lookup_via = false`).
    pub lookup_service: bool,
    pub statics: Vec<String>,
    pub host_ipv6: bool,
}

impl Default for WanForm {
    /// `network.wan` absent: off, with the server's defaults ready for
    /// when it is turned on.
    fn default() -> Self {
        Self {
            on: false,
            mapped: true,
            port: String::new(),
            opened: Opened::Automatic,
            ip: Ip::LookUp,
            typed_ip: String::new(),
            lookup_service: true,
            statics: Vec::new(),
            host_ipv6: false,
        }
    }
}

/// Why the form cannot be saved as it is, in the words the page shows.
pub const LOOKUP_WITHOUT_SERVICE: &str =
    "“Look it up” needs a lookup service. Pick wtfismyip.com, or type the address.";

impl WanForm {
    /// The form for the `network.wan` setting's value: `None`, absent.
    pub fn from_setting(value: Option<&Value>) -> Self {
        let Some(Value::Object(wan)) = value else {
            return Self::default();
        };
        let mut form = Self {
            on: true,
            ..Self::default()
        };
        if let Some(Value::Array(list)) = wan.get("static") {
            form.statics = list
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
        }
        form.host_ipv6 = wan.get("host_ipv6").and_then(Value::as_bool) == Some(true);
        match wan.get("mapped_port") {
            Some(Value::Bool(false)) => form.mapped = false,
            Some(Value::Object(mapped)) => {
                if let Some(port) = mapped.get("port").and_then(Value::as_u64) {
                    form.port = port.to_string();
                }
                if mapped.get("mode").and_then(Value::as_str) == Some("manual") {
                    form.opened = Opened::Forwarded;
                }
                match mapped.get("public_ip").and_then(Value::as_str) {
                    None | Some("lookup") => {}
                    Some(ip) => {
                        form.ip = Ip::Typed;
                        form.typed_ip = ip.to_owned();
                    }
                }
                if mapped.get("lookup_via") == Some(&Value::Bool(false)) {
                    form.lookup_service = false;
                }
            }
            _ => {}
        }
        form
    }

    /// Why Save is held back, if it is: a lookup with no service, which
    /// the server refuses at load too.
    pub fn blocked(&self) -> Option<&'static str> {
        (self.on && self.mapped && self.ip == Ip::LookUp && !self.lookup_service)
            .then_some(LOOKUP_WITHOUT_SERVICE)
    }

    /// The setting the form makes: `None` takes `network.wan` out. `Err`
    /// says which field is wrong.
    pub fn to_setting(&self) -> Result<Option<Value>, String> {
        if !self.on {
            return Ok(None);
        }
        if let Some(why) = self.blocked() {
            return Err(why.to_owned());
        }
        let mut wan = serde_json::Map::new();
        wan.insert("static".into(), json!(self.statics));
        wan.insert("host_ipv6".into(), json!(self.host_ipv6));
        if !self.mapped {
            wan.insert("mapped_port".into(), json!(false));
            return Ok(Some(Value::Object(wan)));
        }
        let mut mapped = serde_json::Map::new();
        if let Some(port) = port(&self.port)? {
            mapped.insert("port".into(), json!(port));
        }
        let mode = match self.opened {
            Opened::Automatic => "auto",
            Opened::Forwarded => "manual",
        };
        mapped.insert("mode".into(), json!(mode));
        let public_ip = match self.ip {
            Ip::LookUp => "lookup".to_owned(),
            Ip::Typed => wan::check_public_ip(&self.typed_ip)?.to_string(),
        };
        mapped.insert("public_ip".into(), json!(public_ip));
        let via = if self.lookup_service {
            json!("wtfismyip")
        } else {
            json!(false)
        };
        mapped.insert("lookup_via".into(), via);
        wan.insert("mapped_port".into(), Value::Object(mapped));
        Ok(Some(Value::Object(wan)))
    }
}

/// A typed port; empty is the server's own.
fn port(text: &str) -> Result<Option<u16>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    match text.parse::<u16>() {
        Ok(0) | Err(_) => Err(format!("{text:?} is not a port: 1 to 65535.")),
        Ok(port) => Ok(Some(port)),
    }
}

/// A static address typed to add to `existing`: as the server will have
/// it, or why not, in the server's words.
pub fn add_static(text: &str, existing: &[String]) -> Result<String, String> {
    let addr = wan::check_static(text)?.to_string();
    if existing.contains(&addr) {
        return Err(format!("{addr} is listed already."));
    }
    Ok(addr)
}

/// The wizard's answer to "From outside your home?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Writes no `network.wan`.
    NotNow,
    /// The router maps the server's port; the public IP is looked up when
    /// it does not report one.
    Automatic,
    /// The operator forwarded a port.
    Forwarded,
    /// A fixed public address.
    Static,
}

/// The `network.wan` the wizard's choice makes: `None` for Not now.
/// `port` and the IP are the Forwarded fields; `statics`, the Static ones.
pub fn wizard_setting(
    choice: Choice,
    port: &str,
    ip: Ip,
    typed_ip: &str,
    statics: &[String],
) -> Result<Option<Value>, String> {
    let form = match choice {
        Choice::NotNow => return Ok(None),
        Choice::Automatic => WanForm {
            on: true,
            ..WanForm::default()
        },
        Choice::Forwarded => WanForm {
            on: true,
            port: port.to_owned(),
            opened: Opened::Forwarded,
            ip,
            typed_ip: typed_ip.to_owned(),
            ..WanForm::default()
        },
        Choice::Static => {
            if statics.is_empty() {
                return Err("Add your public address, with its port.".into());
            }
            WanForm {
                on: true,
                mapped: false,
                statics: statics.to_vec(),
                ..WanForm::default()
            }
        }
    };
    form.to_setting()
}

/// The choice an existing `network.wan` reads as, for a wizard opened
/// again.
pub fn choice_of(value: Option<&Value>) -> Choice {
    let form = WanForm::from_setting(value);
    match (form.on, form.mapped, form.opened) {
        (false, _, _) => Choice::NotNow,
        (true, false, _) => Choice::Static,
        (true, true, Opened::Automatic) => Choice::Automatic,
        (true, true, Opened::Forwarded) => Choice::Forwarded,
    }
}

/// A source's badge: its words and its class (Static blue, Mapped port
/// teal, Looked up mauve, Host IPv6 peach).
pub fn badge(source: AddressSource) -> (&'static str, &'static str) {
    match source {
        AddressSource::Static => ("Static", "src-badge src-static"),
        AddressSource::MappedPort => ("Mapped port", "src-badge src-mapped"),
        AddressSource::Lookup => ("Looked up", "src-badge src-lookup"),
        AddressSource::HostIpv6 => ("Host IPv6", "src-badge src-ipv6"),
    }
}

/// What an advertised address's row says of where it came from.
/// `time` turns Unix seconds into the browser's local time.
pub fn source_detail(
    source: AddressSource,
    address: &str,
    mapping: &MappingStatus,
    lookup: &LookupStatus,
    time: impl Fn(u64) -> String,
) -> String {
    let port = address.rsplit(':').next().unwrap_or_default();
    match source {
        AddressSource::Static => "Typed in Static addresses.".into(),
        AddressSource::HostIpv6 => {
            "This machine’s stable address on the router’s IPv6 prefix.".into()
        }
        AddressSource::MappedPort => match mapping {
            MappingStatus::Granted { external, .. } if external == address => {
                "Granted by the router; it reported this address.".into()
            }
            MappingStatus::Granted { .. } => {
                format!("Mapped by the router on port {port}, at the public IP typed in.")
            }
            _ => format!("Your forward on port {port}, at the public IP typed in."),
        },
        AddressSource::Lookup => match lookup {
            LookupStatus::Found { via, at, .. } => {
                let mapped = matches!(mapping, MappingStatus::Granted { .. });
                let what = if mapped {
                    "The router’s mapping"
                } else {
                    "Your forward"
                };
                format!(
                    "{what} on port {port}, at the IP {} reported at {}.",
                    service_name(via),
                    time(*at)
                )
            }
            _ => format!("Port {port}, at the IP looked up."),
        },
    }
}

/// How bad a status line is: its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Good,
    Bad,
    Quiet,
}

/// A status card row's value: its main line, and the line under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub tone: Tone,
    pub text: String,
    pub sub: Option<String>,
}

/// The "Router mapping" row.
pub fn mapping_line(mapping: &MappingStatus, opened: Opened) -> Line {
    match mapping {
        MappingStatus::NotUsed => Line {
            tone: Tone::Quiet,
            text: match opened {
                Opened::Forwarded => "Not used: you forwarded the port yourself.".into(),
                Opened::Automatic => "Not used.".into(),
            },
            sub: None,
        },
        MappingStatus::Asking => Line {
            tone: Tone::Quiet,
            text: "Asking the router…".into(),
            sub: None,
        },
        MappingStatus::Granted {
            local_port,
            external,
        } => {
            let outside = external.rsplit(':').next().unwrap_or_default();
            Line {
                tone: Tone::Good,
                text: format!("Granted · port {local_port} → {outside}"),
                sub: None,
            }
        }
        MappingStatus::Refused { reason } => {
            let unsupported = reason.contains("none of UPnP");
            Line {
                tone: Tone::Bad,
                text: if unsupported {
                    "No router support found".into()
                } else {
                    "The router refused the mapping".into()
                },
                sub: Some(format!(
                    "{}. The server started normally. Forward the port yourself and choose “I \
                     forwarded it myself”.",
                    capitalized(reason)
                )),
            }
        }
    }
}

/// The "Public IP lookup" row.
pub fn lookup_line(lookup: &LookupStatus, time: impl Fn(u64) -> String) -> Line {
    match lookup {
        LookupStatus::NotUsed => Line {
            tone: Tone::Quiet,
            text: "Not used.".into(),
            sub: None,
        },
        LookupStatus::NotNeeded => Line {
            tone: Tone::Quiet,
            text: "Not needed: the router reported the public address.".into(),
            sub: None,
        },
        LookupStatus::Asking => Line {
            tone: Tone::Quiet,
            text: "Looking it up…".into(),
            sub: None,
        },
        LookupStatus::Found { ip, via, at } => Line {
            tone: Tone::Good,
            text: format!("{ip} from {} · {}", service_name(via), time(*at)),
            sub: None,
        },
        LookupStatus::Failed { error, retrying } => Line {
            tone: Tone::Bad,
            text: format!("Lookup failed: {error}"),
            sub: Some(if *retrying {
                "Retrying with backoff.".into()
            } else {
                "Gave up until the next start. Type the public IP instead.".into()
            }),
        },
    }
}

/// The router's answer as the wizard says it, under Automatic.
pub fn tried_line(mapping: &MappingStatus) -> Line {
    match mapping {
        MappingStatus::Granted {
            local_port,
            external,
        } => Line {
            tone: Tone::Good,
            text: format!("Your router opened port {local_port}"),
            sub: Some(external.clone()),
        },
        MappingStatus::Refused { reason } => Line {
            tone: Tone::Bad,
            text: "Your router did not open the port".into(),
            sub: Some(format!(
                "{}. Choose “I forwarded a port” if you open it yourself.",
                capitalized(reason)
            )),
        },
        MappingStatus::Asking | MappingStatus::NotUsed => Line {
            tone: Tone::Quiet,
            text: "The router has not answered yet".into(),
            sub: Some("It may answer later; the server keeps asking once this is saved.".into()),
        },
    }
}

/// `wtfismyip` as a person reads it.
pub fn service_name(via: &str) -> &str {
    match via {
        "wtfismyip" => "wtfismyip.com",
        other => other,
    }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(_: u64) -> String {
        "18:02".into()
    }

    #[test]
    fn off_takes_network_wan_out() {
        assert_eq!(WanForm::default().to_setting(), Ok(None));
        assert_eq!(WanForm::from_setting(None), WanForm::default());
    }

    #[test]
    fn on_with_the_defaults_maps_the_port_and_looks_the_ip_up() {
        let form = WanForm {
            on: true,
            ..WanForm::default()
        };
        assert_eq!(
            form.to_setting(),
            Ok(Some(json!({
                "static": [],
                "host_ipv6": false,
                "mapped_port": {
                    "mode": "auto",
                    "public_ip": "lookup",
                    "lookup_via": "wtfismyip"
                }
            })))
        );
    }

    #[test]
    fn a_saved_setting_reads_back_as_the_same_form() {
        let form = WanForm {
            on: true,
            port: "443".into(),
            opened: Opened::Forwarded,
            ip: Ip::Typed,
            typed_ip: "203.0.113.24".into(),
            lookup_service: false,
            statics: vec!["198.51.100.7:7420".into()],
            host_ipv6: true,
            ..WanForm::default()
        };
        let saved = form.to_setting().unwrap().unwrap();
        assert_eq!(
            saved,
            json!({
                "static": ["198.51.100.7:7420"],
                "host_ipv6": true,
                "mapped_port": {
                    "port": 443,
                    "mode": "manual",
                    "public_ip": "203.0.113.24",
                    "lookup_via": false
                }
            })
        );
        assert_eq!(WanForm::from_setting(Some(&saved)), form);
    }

    #[test]
    fn the_mapped_port_off_is_written_false() {
        let form = WanForm {
            on: true,
            mapped: false,
            statics: vec!["198.51.100.7:7420".into()],
            ..WanForm::default()
        };
        let saved = form.to_setting().unwrap().unwrap();
        assert_eq!(saved["mapped_port"], json!(false));
        assert!(!WanForm::from_setting(Some(&saved)).mapped);
    }

    /// The server refuses it at load; the page holds Save back first.
    #[test]
    fn a_lookup_with_no_service_holds_save_back() {
        let form = WanForm {
            on: true,
            lookup_service: false,
            ..WanForm::default()
        };
        assert_eq!(form.blocked(), Some(LOOKUP_WITHOUT_SERVICE));
        assert_eq!(form.to_setting(), Err(LOOKUP_WITHOUT_SERVICE.to_owned()));
        let typed = WanForm {
            ip: Ip::Typed,
            typed_ip: "203.0.113.24".into(),
            ..form
        };
        assert_eq!(typed.blocked(), None);
    }

    #[test]
    fn a_typed_public_ip_or_port_that_cannot_work_is_refused() {
        let form = WanForm {
            on: true,
            ip: Ip::Typed,
            typed_ip: "100.64.0.1".into(),
            ..WanForm::default()
        };
        let err = form.to_setting().unwrap_err();
        assert!(err.contains("(100.64.0.0/10)"), "{err}");
        let port = WanForm {
            on: true,
            port: "70000".into(),
            ..WanForm::default()
        };
        assert!(port.to_setting().unwrap_err().contains("not a port"));
    }

    /// The refusals are the server's words, the range named.
    #[test]
    fn a_static_address_must_be_public_and_new() {
        assert_eq!(
            add_static(" 198.51.100.7:7420 ", &[]),
            Ok("198.51.100.7:7420".into())
        );
        for (typed, says) in [
            ("192.168.1.20:7420", "(192.168.0.0/16)"),
            ("10.0.0.5:7420", "(10.0.0.0/8)"),
            ("172.20.0.1:7420", "(172.16.0.0/12)"),
            ("100.100.1.1:7420", "(100.64.0.0/10)"),
            ("127.0.0.1:7420", "loopback"),
            ("[::1]:7420", "loopback"),
            ("[fe80::1]:7420", "link-local"),
            ("[fd00::5]:7420", "(fc00::/7)"),
            ("198.51.100.7", "not an address and a port"),
        ] {
            let err = add_static(typed, &[]).unwrap_err();
            assert!(err.contains(says), "{typed}: {err}");
        }
        assert!(
            add_static("198.51.100.7:7420", &["198.51.100.7:7420".into()])
                .unwrap_err()
                .contains("listed already")
        );
    }

    #[test]
    fn the_wizard_s_choices_write_network_wan() {
        assert_eq!(
            wizard_setting(Choice::NotNow, "", Ip::LookUp, "", &[]),
            Ok(None)
        );
        let auto = wizard_setting(Choice::Automatic, "", Ip::LookUp, "", &[])
            .unwrap()
            .unwrap();
        assert_eq!(auto["mapped_port"]["mode"], json!("auto"));
        let forwarded = wizard_setting(Choice::Forwarded, "7420", Ip::LookUp, "", &[])
            .unwrap()
            .unwrap();
        assert_eq!(forwarded["mapped_port"]["mode"], json!("manual"));
        assert_eq!(forwarded["mapped_port"]["port"], json!(7420));
        assert_eq!(forwarded["mapped_port"]["public_ip"], json!("lookup"));
        let typed = wizard_setting(Choice::Forwarded, "", Ip::Typed, "203.0.113.24", &[])
            .unwrap()
            .unwrap();
        assert_eq!(typed["mapped_port"]["public_ip"], json!("203.0.113.24"));
        let fixed = wizard_setting(
            Choice::Static,
            "",
            Ip::LookUp,
            "",
            &["198.51.100.7:7420".into()],
        )
        .unwrap()
        .unwrap();
        assert_eq!(fixed["mapped_port"], json!(false));
        assert_eq!(fixed["static"], json!(["198.51.100.7:7420"]));
        assert!(wizard_setting(Choice::Static, "", Ip::LookUp, "", &[]).is_err());
        for setting in [&auto, &forwarded, &fixed] {
            assert_ne!(choice_of(Some(setting)), Choice::NotNow);
        }
        assert_eq!(choice_of(Some(&auto)), Choice::Automatic);
        assert_eq!(choice_of(Some(&forwarded)), Choice::Forwarded);
        assert_eq!(choice_of(Some(&fixed)), Choice::Static);
        assert_eq!(choice_of(None), Choice::NotNow);
    }

    #[test]
    fn each_source_has_its_badge() {
        assert_eq!(badge(AddressSource::Static).0, "Static");
        assert_eq!(badge(AddressSource::MappedPort).0, "Mapped port");
        assert_eq!(badge(AddressSource::Lookup).0, "Looked up");
        assert_eq!(badge(AddressSource::HostIpv6).0, "Host IPv6");
    }

    #[test]
    fn the_status_card_says_how_each_address_was_found() {
        let granted = MappingStatus::Granted {
            local_port: 7420,
            external: "203.0.113.24:7420".into(),
        };
        assert_eq!(
            source_detail(
                AddressSource::MappedPort,
                "203.0.113.24:7420",
                &granted,
                &LookupStatus::NotNeeded,
                at
            ),
            "Granted by the router; it reported this address."
        );
        let found = LookupStatus::Found {
            ip: "203.0.113.24".into(),
            via: "wtfismyip".into(),
            at: 0,
        };
        assert_eq!(
            source_detail(
                AddressSource::Lookup,
                "203.0.113.24:7420",
                &MappingStatus::NotUsed,
                &found,
                at
            ),
            "Your forward on port 7420, at the IP wtfismyip.com reported at 18:02."
        );
        assert_eq!(
            mapping_line(&granted, Opened::Automatic).text,
            "Granted · port 7420 → 7420"
        );
        let unsupported = mapping_line(
            &MappingStatus::Refused {
                reason: "the router answers none of UPnP, NAT-PMP and PCP".into(),
            },
            Opened::Automatic,
        );
        assert_eq!(unsupported.text, "No router support found");
        assert_eq!(unsupported.tone, Tone::Bad);
        assert_eq!(
            mapping_line(&MappingStatus::NotUsed, Opened::Forwarded).text,
            "Not used: you forwarded the port yourself."
        );
        assert_eq!(
            lookup_line(&found, at).text,
            "203.0.113.24 from wtfismyip.com · 18:02"
        );
        let failed = lookup_line(
            &LookupStatus::Failed {
                error: "wtfismyip did not answer within 10 s".into(),
                retrying: true,
            },
            at,
        );
        assert_eq!(failed.sub.as_deref(), Some("Retrying with backoff."));
        assert_eq!(
            tried_line(&granted).sub.as_deref(),
            Some("203.0.113.24:7420")
        );
    }
}
