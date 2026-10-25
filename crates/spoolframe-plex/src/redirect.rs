//! Where an image's redirect may go. A PMS answers the thumb of an item
//! whose art an agent gave with a 302 to Plex's image proxy
//! (`https://images.plex.tv/photo?...`); Plex's own apps follow it. The
//! server follows it too, but it fetches a URL the PMS gave it, so the
//! URL is held to these rules:
//!
//! - at most [`MAX_REDIRECTS`] of them, within the one request's time
//!   and the one image's size;
//! - one to the PMS's own origin is followed as any PMS request is, with
//!   the token;
//! - one off it only over HTTPS, to a host that is public: an address
//!   [`not_public`] passes, a name that is not a local one, and that
//!   resolves only to public addresses ([`PublicOnly`], so the address
//!   checked is the one dialled);
//! - off the PMS the token is never sent: not as `X-Plex-Token`, and a
//!   URL that names it is refused. Once off the PMS, nothing after is
//!   sent it.

use std::net::{IpAddr, SocketAddr};

use spoolframe_admin_api::wan::not_public;
use url::{Host, Url};

/// The most redirects an image's request follows.
pub(crate) const MAX_REDIRECTS: usize = 3;

/// The names only a local network resolves: never a host off the PMS.
const LOCAL_SUFFIXES: [&str; 5] = [".local", ".localhost", ".internal", ".lan", ".home.arpa"];

/// Which hosts off the PMS an image's redirect may go to: public ones, or
/// any for a test whose image proxy is on loopback.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Outside {
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) inner: bool,
}

impl Outside {
    fn inner(self) -> bool {
        #[cfg(any(test, feature = "test-support"))]
        if self.inner {
            return true;
        }
        false
    }

    /// Why a redirect off the PMS to `url` is not followed, if it is not.
    /// `token` is the PMS's.
    pub(crate) fn refused(self, url: &Url, token: Option<&str>) -> Option<String> {
        if let Some(token) = token.filter(|t| !t.is_empty())
            && names_token(url, token)
        {
            return Some("its URL carries the PMS's token".into());
        }
        if self.inner() {
            return None;
        }
        if url.scheme() != "https" {
            return Some(format!("it is {}, not HTTPS", url.scheme()));
        }
        match url.host() {
            None => Some("it names no host".into()),
            Some(Host::Ipv4(ip)) => not_public(ip.into()).map(str::to_owned),
            Some(Host::Ipv6(ip)) => not_public(ip.into()).map(str::to_owned),
            Some(Host::Domain(name)) => {
                let name = name.trim_end_matches('.').to_ascii_lowercase();
                let local = !name.contains('.')
                    || name == "localhost"
                    || LOCAL_SUFFIXES.iter().any(|suffix| name.ends_with(suffix));
                local.then(|| format!("{name} is a local name"))
            }
        }
    }
}

/// Whether `url` names `token` anywhere: as is, or once its query is
/// decoded (a URL in a parameter, `?url=%2Fx%3FX-Plex-Token%3D...`).
fn names_token(url: &Url, token: &str) -> bool {
    url.as_str().contains(token)
        || url
            .query_pairs()
            .any(|(key, value)| key.contains(token) || value.contains(token))
}

/// Whether `a` and `b` are the same origin: scheme, host and port.
pub(crate) fn same_origin(a: &Url, b: &Url) -> bool {
    a.origin() == b.origin()
}

/// The resolver of the client that fetches off the PMS: a name resolves
/// to its public addresses only, and is unreachable with none (a public
/// name pointed at an inner address). The address checked is the one
/// dialled, so a name cannot pass the check and then resolve elsewhere.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PublicOnly(pub(crate) Outside);

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let outside = self.0;
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let found: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let public: Vec<SocketAddr> = found
                .iter()
                .copied()
                .filter(|addr| outside.inner() || public(addr.ip()))
                .collect();
            if public.is_empty() {
                return Err(format!("{host} resolves to no public address").into());
            }
            Ok(Box::new(public.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

fn public(ip: IpAddr) -> bool {
    not_public(ip).is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(url: &str, token: Option<&str>) -> Option<String> {
        Outside::default().refused(&url.parse().unwrap(), token)
    }

    #[test]
    fn plex_s_image_proxy_is_followed() {
        let proxy = "https://images.plex.tv/photo?height=480&url=https%3A%2F%2Fmetadata-static.plex.tv%2Fa.jpg";
        assert_eq!(refused(proxy, Some("s3cr3t")), None);
    }

    #[test]
    fn a_url_with_the_token_in_a_parameter_is_refused() {
        let url = "https://images.plex.tv/photo?url=http%3A%2F%2Fpms%2Fx%3FX-Plex-Token%3Ds3cr3t";
        assert!(refused(url, Some("s3cr3t")).is_some_and(|why| why.contains("token")));
    }

    #[test]
    fn the_same_origin_is_scheme_host_and_port() {
        let pms: Url = "http://192.168.1.2:32400/".parse().unwrap();
        assert!(same_origin(
            &pms,
            &"http://192.168.1.2:32400/x?y".parse().unwrap()
        ));
        for other in [
            "https://192.168.1.2:32400/",
            "http://192.168.1.2:32401/",
            "http://192.168.1.3:32400/",
        ] {
            assert!(!same_origin(&pms, &other.parse().unwrap()), "{other}");
        }
    }
}
