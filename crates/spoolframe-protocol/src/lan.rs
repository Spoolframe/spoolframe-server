//! How a server announces itself on the LAN (P1-13, ARCHITECTURE §18).
//!
//! Servers publish an mDNS record under their own service name, so the
//! records of other Iroh apps on the network (`irohv1`) are never heard at
//! all. The record's user data carries [`CLIENT_ALPN`] and the server's name.
//! A record without that prefix is not a Spoolframe server and is ignored.
//!
//! All of it is untrusted: it says where to dial and which `EndpointId` to
//! expect, and Iroh proves the identity on connect.

use crate::CLIENT_ALPN;

/// The mDNS service name: records are `<id>._spoolframe._udp.local`.
pub const MDNS_SERVICE: &str = "spoolframe";

/// The most user data an Iroh mDNS record holds (`UserData::MAX_LENGTH`).
pub const MAX_ADVERT: usize = 245;

const SEPARATOR: char = ';';

/// What a server's record says about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advert {
    /// The server's name, possibly cut to fit the record.
    pub name: String,
}

/// The user data a server called `name` publishes: the client ALPN and the
/// name, the name cut at a character boundary to fit [`MAX_ADVERT`].
pub fn advert(name: &str) -> String {
    let alpn = std::str::from_utf8(CLIENT_ALPN).expect("the ALPN is ASCII");
    let mut advert = format!("{alpn}{SEPARATOR}");
    for c in name.chars() {
        if advert.len() + c.len_utf8() > MAX_ADVERT {
            break;
        }
        advert.push(c);
    }
    advert
}

/// Read a record's user data; `None` unless it is a Spoolframe server
/// speaking this client protocol.
pub fn parse_advert(user_data: &str) -> Option<Advert> {
    let (alpn, name) = user_data.split_once(SEPARATOR)?;
    (alpn.as_bytes() == CLIENT_ALPN).then(|| Advert {
        name: name.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_advert_reads_back_as_the_name() {
        assert_eq!(
            parse_advert(&advert("Home Media; upstairs")),
            Some(Advert {
                name: "Home Media; upstairs".into()
            })
        );
    }

    #[test]
    fn a_long_name_is_cut_at_a_character_boundary() {
        // Three bytes each: the limit falls inside one.
        let name = "€".repeat(100);
        let advert = advert(&name);
        assert!(advert.len() <= MAX_ADVERT);
        assert!(advert.len() > MAX_ADVERT - 3);
        let read = parse_advert(&advert).unwrap();
        assert!(name.starts_with(&read.name));
    }

    #[test]
    fn records_of_other_apps_and_protocols_are_not_servers() {
        for other in [
            "",
            "foobar",
            "spoolframe/client/2;Home Media",
            "spoolframe/client/1",
        ] {
            assert_eq!(parse_advert(other), None, "{other:?}");
        }
    }
}
