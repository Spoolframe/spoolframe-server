//! What kind of client a device runs: said in its `Hello`, kept per
//! device by the server, and shown in device and session lists ("Linux
//! app", "Apple TV app") in place of a bare "App".

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    /// The Linux desktop app (`spoolframe-linux`).
    Linux,
    /// The Apple TV app.
    Tvos,
    /// The iPhone and iPad app.
    Ios,
    /// The Mac app.
    Macos,
    /// A command-line or scripted client of the SDK (its examples, tools),
    /// or, for an admin API session, the server's own CLI.
    Cli,
    /// The Android app, on phones, tablets and Android TV.
    Android,
    /// The Xbox app.
    Xbox,
    /// A client in a browser: a web player's `Hello`, or an admin API
    /// session the web UI signed in.
    Web,
    /// A platform this build does not know, from a newer peer.
    #[serde(other)]
    Unknown,
}

impl Platform {
    /// The name on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Linux => "linux",
            Platform::Tvos => "tvos",
            Platform::Ios => "ios",
            Platform::Macos => "macos",
            Platform::Cli => "cli",
            Platform::Android => "android",
            Platform::Xbox => "xbox",
            Platform::Web => "web",
            Platform::Unknown => "unknown",
        }
    }

    /// The platform named `name` ([`Platform::as_str`]); a name this build
    /// does not know is [`Platform::Unknown`], as on the wire.
    pub fn from_name(name: &str) -> Self {
        match name {
            "linux" => Platform::Linux,
            "tvos" => Platform::Tvos,
            "ios" => Platform::Ios,
            "macos" => Platform::Macos,
            "cli" => Platform::Cli,
            "android" => Platform::Android,
            "xbox" => Platform::Xbox,
            "web" => Platform::Web,
            _ => Platform::Unknown,
        }
    }

    /// What a device or session list calls a device running it.
    pub fn label(self) -> &'static str {
        match self {
            Platform::Linux => "Linux app",
            Platform::Tvos => "Apple TV app",
            Platform::Ios => "iPhone or iPad app",
            Platform::Macos => "Mac app",
            Platform::Cli => "Command-line client",
            Platform::Android => "Android app",
            Platform::Xbox => "Xbox app",
            Platform::Web => "Web",
            Platform::Unknown => "App",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Platform; 9] = [
        Platform::Linux,
        Platform::Tvos,
        Platform::Ios,
        Platform::Macos,
        Platform::Cli,
        Platform::Android,
        Platform::Xbox,
        Platform::Web,
        Platform::Unknown,
    ];

    #[test]
    fn the_stored_name_is_the_wire_name_and_reads_back() {
        for platform in ALL {
            let wire = serde_json::to_value(platform).unwrap();
            assert_eq!(wire, platform.as_str(), "{platform:?}");
            assert_eq!(Platform::from_name(platform.as_str()), platform);
        }
    }

    #[test]
    fn a_platform_from_a_newer_peer_is_unknown() {
        assert_eq!(Platform::from_name("vision_os"), Platform::Unknown);
        let read: Platform = serde_json::from_str("\"vision_os\"").unwrap();
        assert_eq!(read, Platform::Unknown);
    }
}
