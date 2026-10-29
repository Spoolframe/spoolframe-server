//! What this server speaks of the client protocol: its version, the
//! regular features it has and the experimental features on, as the
//! Server page (`ServerStatus`), `spoolframe-server --version` and MCP's
//! server info show them.

use spoolframe_protocol::{PROTOCOL_VERSION, experimental};

/// The protocol's version, the regular features, and the experimental
/// features on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolInfo {
    pub version: u32,
    /// The regular features this server has (`client::features`), by
    /// their `spoolframe:` names: each is on for a connection whose
    /// `Hello` names it too.
    pub features: Vec<&'static str>,
    /// The experimental features turned on here, each by its setting.
    pub experimental: Vec<&'static str>,
}

impl ProtocolInfo {
    /// This build's, with `experimental` the experimental features on.
    pub fn new(experimental: Vec<&'static str>) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            features: crate::client::features().iter().map(|f| f.name()).collect(),
            experimental,
        }
    }

    /// One line each, for `--version` and MCP: "client protocol: 1", the
    /// regular features, then the experimental features under the heading
    /// `experimental`, "none" for an empty list, which is shown, not left
    /// out.
    pub fn lines(&self, experimental: &str) -> Vec<String> {
        vec![
            format!("client protocol: {}", self.version),
            format!("features: {}", listed(&self.features)),
            format!("{experimental}: {}", listed(&self.experimental)),
        ]
    }
}

/// The experimental features on: `http-playback` while its port is
/// configured (`http_playback.listen`), `federation` while
/// `experimental.federation` is on (P8-01); the same answer the client
/// protocol's `Hello` gets.
pub fn experimental_on(http_playback: bool, federation: bool) -> Vec<&'static str> {
    let mut on = Vec::new();
    if http_playback {
        on.push(experimental::HTTP_PLAYBACK);
    }
    if federation {
        on.push(experimental::FEDERATION);
    }
    on
}

/// Every experimental feature this build knows, each on by its setting:
/// what `--version` names, as it reads no config.
pub fn experimental_known() -> Vec<&'static str> {
    experimental_on(true, true)
}

/// `names` joined, or "none".
pub fn listed(names: &[&str]) -> String {
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}

/// The `--version` text after the program's name: the crate's version on
/// the first line, as the image's smoke test reads it, then the
/// protocol's.
pub fn long_version() -> String {
    let info = ProtocolInfo::new(experimental_known());
    let mut lines = vec![env!("CARGO_PKG_VERSION").to_owned()];
    lines.extend(info.lines("experimental features (each on by its setting)"));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_list_is_said_as_none() {
        assert_eq!(listed(&[]), "none");
        assert_eq!(listed(&["a", "b"]), "a, b");
        let info = ProtocolInfo::new(Vec::new());
        let lines = info.lines("experimental features on");
        assert_eq!(
            lines,
            [
                "client protocol: 1",
                "features: spoolframe:profile-events, spoolframe:watch-events, \
                 spoolframe:next-episode, spoolframe:invite-passkeys",
                "experimental features on: none",
            ]
        );
    }

    #[test]
    fn the_experimental_features_are_those_on() {
        let info = ProtocolInfo::new(experimental_on(true, false));
        assert_eq!(info.version, PROTOCOL_VERSION);
        assert_eq!(info.experimental, [experimental::HTTP_PLAYBACK]);
        assert!(experimental_on(false, false).is_empty());
        assert_eq!(experimental_on(false, true), [experimental::FEDERATION]);
        assert_eq!(
            experimental_known(),
            [experimental::HTTP_PLAYBACK, experimental::FEDERATION]
        );
    }

    #[test]
    fn the_long_version_starts_with_the_crate_s() {
        let text = long_version();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some(env!("CARGO_PKG_VERSION")));
        assert_eq!(
            lines.next(),
            Some(format!("client protocol: {PROTOCOL_VERSION}").as_str())
        );
        assert_eq!(
            lines.next(),
            Some(
                "features: spoolframe:profile-events, spoolframe:watch-events, \
                 spoolframe:next-episode, spoolframe:invite-passkeys"
            )
        );
        assert_eq!(
            lines.next(),
            Some("experimental features (each on by its setting): http-playback, federation")
        );
        assert_eq!(lines.next(), None);
    }
}
