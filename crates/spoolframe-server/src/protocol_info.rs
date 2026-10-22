//! What this server speaks of the client protocol: its version and its
//! features, as the Server page (`ServerStatus`), `spoolframe-server
//! --version` and MCP's server info show them. New wire capability comes
//! as a regular feature rather than a new version (PX-43 on), so the
//! version alone no longer says what a server can do.

use spoolframe_protocol::{PROTOCOL_VERSION, experimental, features};

/// The protocol's version, and the features this server has, each list
/// named for what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolInfo {
    pub version: u32,
    /// Every regular feature (`features::ALL`): always on in this build.
    pub features: Vec<&'static str>,
    /// Those of them a client sees before it signs in
    /// (`features::PRE_AUTH`).
    pub pre_auth: Vec<&'static str>,
    /// The experimental features turned on here, each by its setting.
    pub experimental: Vec<&'static str>,
}

impl ProtocolInfo {
    /// This build's, with `experimental` the experimental features on.
    pub fn new(experimental: Vec<&'static str>) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            features: features::ALL.to_vec(),
            pre_auth: features::PRE_AUTH.to_vec(),
            experimental,
        }
    }

    /// One line each, for `--version` and MCP: "features: a, b", and
    /// "none" for an empty list, which is shown, not left out.
    /// `experimental` names the experimental lines' heading.
    pub fn lines(&self, experimental: &str) -> Vec<String> {
        vec![
            format!("client protocol: {}", self.version),
            format!("features: {}", listed(&self.features)),
            format!("pre-auth features: {}", listed(&self.pre_auth)),
            format!("{experimental}: {}", listed(&self.experimental)),
        ]
    }
}

/// The experimental features on: `http-playback` while its port is
/// configured (`http_playback.listen`), the same answer the client
/// protocol's `Hello` gets.
pub fn experimental_on(http_playback: bool) -> Vec<&'static str> {
    let mut on = Vec::new();
    if http_playback {
        on.push(experimental::HTTP_PLAYBACK);
    }
    on
}

/// Every experimental feature this build knows, each on by its setting:
/// what `--version` names, as it reads no config.
pub fn experimental_known() -> Vec<&'static str> {
    experimental_on(true)
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
        assert_eq!(lines[2], "pre-auth features: spoolframe:invite-lookup");
        assert_eq!(lines[3], "experimental features on: none");
    }

    #[test]
    fn the_features_are_this_build_s() {
        let info = ProtocolInfo::new(experimental_on(true));
        assert_eq!(info.version, PROTOCOL_VERSION);
        assert!(info.features.contains(&features::LIKED));
        assert!(info.features.contains(&features::PLAYLISTS));
        assert_eq!(info.experimental, [experimental::HTTP_PLAYBACK]);
        assert!(experimental_on(false).is_empty());
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
        assert!(text.contains("spoolframe:liked"), "{text}");
    }
}
