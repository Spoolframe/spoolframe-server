//! Server › Settings › Experimental, apart from the page: HTTP playback's
//! `http_playback.listen` (PX-49) as the tab shows it, a switch and a
//! port. Host-compiled, so `cargo test` covers it.

/// The port HTTP playback listens on unless another is typed.
pub const DEFAULT_PORT: u16 = 7422;

/// `http_playback.listen` as the tab shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listen {
    /// Not set: nothing listens.
    Off,
    /// Every address, IPv4 (`0.0.0.0`) or IPv6 (`[::]`), on `port`: what
    /// the switch and the port field make.
    Any { v6: bool, port: u16 },
    /// Anything else (one address, or a value the server would refuse),
    /// as it is: the switch is on, and saving the tab replaces it only if
    /// the switch or the port is changed.
    Other(String),
}

impl Listen {
    /// `listen`, the setting's value; empty is off.
    pub fn of(listen: &str) -> Self {
        let listen = listen.trim();
        if listen.is_empty() {
            return Listen::Off;
        }
        match listen.parse::<std::net::SocketAddr>() {
            Ok(addr) if addr.ip().is_unspecified() && addr.port() != 0 => Listen::Any {
                v6: addr.is_ipv6(),
                port: addr.port(),
            },
            _ => Listen::Other(listen.to_owned()),
        }
    }

    pub fn on(&self) -> bool {
        !matches!(self, Listen::Off)
    }

    /// The port field's text: the port listened on, else the default.
    pub fn port_text(&self) -> String {
        let port = match self {
            Listen::Any { port, .. } => Some(*port),
            Listen::Other(text) => text.parse::<std::net::SocketAddr>().ok().map(|a| a.port()),
            Listen::Off => None,
        };
        port.unwrap_or(DEFAULT_PORT).to_string()
    }

    /// The setting the switch and the typed port make: `None` takes it
    /// out (off). A value that was [`Listen::Other`] is kept as it is
    /// while neither was changed. `Err` says what is wrong with the port.
    pub fn setting(&self, on: bool, port: &str) -> Result<Option<String>, String> {
        if !on {
            return Ok(None);
        }
        let port = port.trim();
        if let Listen::Other(text) = self
            && port == self.port_text()
        {
            return Ok(Some(text.clone()));
        }
        let port: u16 = match port.parse() {
            Ok(0) | Err(_) => return Err("The port is a number from 1 to 65535.".into()),
            Ok(port) => port,
        };
        let v6 = matches!(self, Listen::Any { v6: true, .. });
        Ok(Some(if v6 {
            format!("[::]:{port}")
        } else {
            format!("0.0.0.0:{port}")
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_is_no_setting_and_the_default_port() {
        let off = Listen::of("");
        assert_eq!(off, Listen::Off);
        assert!(!off.on());
        assert_eq!(off.port_text(), "7422");
        // Turned on with the port as shown: every IPv4 address.
        assert_eq!(off.setting(true, "7422"), Ok(Some("0.0.0.0:7422".into())));
        assert_eq!(off.setting(false, "7422"), Ok(None));
    }

    #[test]
    fn every_address_round_trips_ipv4_and_ipv6() {
        for listen in ["0.0.0.0:7422", "[::]:7422", "0.0.0.0:8080", "[::]:9000"] {
            let shown = Listen::of(listen);
            assert!(shown.on(), "{listen}");
            assert_eq!(
                shown.setting(true, &shown.port_text()),
                Ok(Some(listen.to_owned())),
                "{listen}"
            );
        }
    }

    #[test]
    fn a_custom_port_is_kept_on_its_family() {
        let v6 = Listen::of("[::]:7422");
        assert_eq!(
            v6,
            Listen::Any {
                v6: true,
                port: 7422
            }
        );
        assert_eq!(v6.setting(true, " 8443 "), Ok(Some("[::]:8443".into())));
        assert_eq!(
            Listen::of("0.0.0.0:7422").setting(true, "8443"),
            Ok(Some("0.0.0.0:8443".into()))
        );
    }

    #[test]
    fn a_port_that_is_not_one_is_refused() {
        let off = Listen::Off;
        for port in ["", "0", "65536", "seven"] {
            assert!(off.setting(true, port).is_err(), "{port}");
        }
    }

    /// One address, or what the server would refuse, is shown as it is
    /// and saved back as it is, until the switch or the port changes.
    #[test]
    fn another_value_is_shown_as_it_is() {
        let one = Listen::of("192.168.1.5:7500");
        assert_eq!(one, Listen::Other("192.168.1.5:7500".into()));
        assert!(one.on());
        assert_eq!(one.port_text(), "7500");
        assert_eq!(
            one.setting(true, "7500"),
            Ok(Some("192.168.1.5:7500".into()))
        );
        assert_eq!(one.setting(true, "7600"), Ok(Some("0.0.0.0:7600".into())));
        let odd = Listen::of("media:7422");
        assert_eq!(odd, Listen::Other("media:7422".into()));
        assert_eq!(odd.port_text(), "7422");
        assert_eq!(odd.setting(true, "7422"), Ok(Some("media:7422".into())));
        assert_eq!(odd.setting(false, "7422"), Ok(None));
    }
}
