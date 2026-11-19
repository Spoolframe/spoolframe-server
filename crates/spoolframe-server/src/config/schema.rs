//! Where the settings pages show each setting, and how its field reads
//! and writes it (stage 4 of the settings redesign): a page per area,
//! each with sections, an Advanced section collapsed, and a Sensitive
//! block. `settingsSchema` sends it, with each setting's label, help and
//! how it applies from [`super::settings::SETTINGS`], and the bounds a
//! number is checked against. The web UI draws its pages from it.

use super::settings::Area;

/// How a field shows and takes its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Widget {
    /// One line of text; empty takes the default.
    Text,
    /// A secret: never shown, only replaced.
    Secret,
    /// A secret named by the server's environment variable that holds
    /// it, as `${SF_ENV_…}` (the example): the settings are plain text.
    EnvRef(&'static str),
    /// A whole number, with its unit after the box.
    Number(&'static str),
    /// MiB kept, GiB shown and typed.
    Gib,
    Switch,
    /// One of a few values, each a card with what it does.
    Choice(&'static [(&'static str, &'static str, &'static str)]),
    /// Any of a few values, in a fixed order.
    Many(&'static [(&'static str, &'static str)]),
    /// A list of text, an item a row; the button's words to add one.
    List(&'static str),
    /// A field of its own ([`Custom`]).
    Custom(Custom),
}

/// The fields too particular for the generic widgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Custom {
    /// `network.wan`'s mapped port and public addresses.
    Wan,
    /// `network.wan` present or not: "Advertise a public address".
    WanSwitch,
    Pkarr,
    /// The upload budget, with Measure.
    Budget,
    Gpus,
    VideoCodecs,
    Oidc,
    Redirects,
    Headers,
    /// `plex.token`, with signing in to Plex.
    PlexToken,
    /// `admin.metrics_from`: loopback kept, and refused scrapers offered.
    MetricsFrom,
    /// What the Libraries page shows: its sources and Plex servers.
    Libraries,
    PathMapping,
}

/// What a section is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Plain,
    /// Collapsed until opened: settings few servers need.
    Advanced,
    /// A coloured edge, its consequence, and "Confirm it's you" on save.
    Sensitive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Section {
    pub name: &'static str,
    pub help: &'static str,
    pub kind: Kind,
    pub fields: &'static [(&'static str, Widget)],
}

/// Where an area's page is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// Settings › the area, at `/admin/settings/<slug>`.
    Settings,
    /// The Federation page's Settings tab.
    Federation,
    /// The Libraries page.
    Libraries,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    pub slug: &'static str,
    pub area: Area,
    pub name: &'static str,
    /// The line under its title.
    pub line: &'static str,
    pub place: Place,
    pub sections: &'static [Section],
}

const fn plain(
    name: &'static str,
    help: &'static str,
    fields: &'static [(&'static str, Widget)],
) -> Section {
    Section {
        name,
        help,
        kind: Kind::Plain,
        fields,
    }
}

const fn advanced(name: &'static str, fields: &'static [(&'static str, Widget)]) -> Section {
    Section {
        name,
        help: "Settings few servers need",
        kind: Kind::Advanced,
        fields,
    }
}

const fn sensitive(help: &'static str, fields: &'static [(&'static str, Widget)]) -> Section {
    Section {
        name: "Sensitive",
        help,
        kind: Kind::Sensitive,
        fields,
    }
}

use Widget::{Gib, List, Number, Switch, Text};

const SECS: Widget = Number("s");
const MIB: Widget = Number("MiB");
const COUNT: Widget = Number("");

pub const CONGESTION: &[(&str, &str, &str)] = &[
    (
        "bbr",
        "BBR (the default)",
        "Keeps the queue short: steadier over the internet.",
    ),
    (
        "cubic",
        "Cubic",
        "The usual TCP way: backs off at the first loss.",
    ),
];

pub const PREVIEWS_WHEN: &[(&str, &str, &str)] = &[
    (
        "ahead",
        "Ahead",
        "For the whole library, a little at a time.",
    ),
    (
        "on_play",
        "When played",
        "Only for what is played, as it is played.",
    ),
    (
        "never",
        "Never",
        "None: apps show the time alone when scrubbing.",
    ),
];

pub const OTLP_PROTOCOLS: &[(&str, &str, &str)] = &[
    ("grpc", "gRPC", "Port 4317, usually."),
    ("http", "HTTP", "Port 4318, usually."),
];

pub const SANDBOX: &[(&str, &str, &str)] = &[
    (
        "required",
        "Required",
        "Each run is confined with Landlock and a seccomp filter: no network, no state directory, only its own files. A kernel without Landlock (before Linux 6.2) stops the server from starting.",
    ),
    (
        "auto",
        "Automatic",
        "Default. Confined as Required where the kernel has Landlock; elsewhere with the seccomp filter alone, and an error in the log.",
    ),
    (
        "off",
        "Off",
        "Not confined: the tools read files anyone who can add to a library made.",
    ),
];

pub const SUBTITLE_SITES: &[(&str, &str)] =
    &[("opensubtitles", "OpenSubtitles"), ("subdl", "SubDL")];

/// Every area's page, in the sub-nav's order, then those kept elsewhere.
pub const PAGES: &[Page] = &[
    Page {
        slug: "general",
        area: Area::General,
        name: "General",
        line: "This server's name and how apps at home find it.",
        place: Place::Settings,
        sections: &[
            plain("This server", "", &[("name", Text)]),
            plain(
                "On this network",
                "How apps at home find this server.",
                &[("network.mdns", Switch)],
            ),
        ],
    },
    Page {
        slug: "network",
        area: Area::Network,
        name: "Network",
        line: "The web UI’s address, apps from outside, ports and upload.",
        place: Place::Settings,
        sections: &[
            plain(
                "Web UI",
                "How people reach this page. The public address and proxies are under Sensitive below.",
                &[("admin.listen", Text)],
            ),
            plain(
                "Apps from outside",
                "Tell apps outside your home where to find this server. Signed-in devices get these addresses on their own.",
                &[
                    ("network.wan", Widget::Custom(Custom::Wan)),
                    ("network.relays", List("Add a relay URL")),
                    ("network.own_relays", List("Add a relay key")),
                    ("network.pkarr", Widget::Custom(Custom::Pkarr)),
                    ("network.publish_lookup", Switch),
                    ("network.publish_lookup_private", Switch),
                ],
            ),
            plain(
                "Ports",
                "",
                &[
                    ("network.port", COUNT),
                    ("network.plex_discovery_port", COUNT),
                    ("http_playback.listen", Text),
                ],
            ),
            plain(
                "Bandwidth",
                "Upload for playback outside your home.",
                &[
                    ("network.uplink.budget_mbps", Widget::Custom(Custom::Budget)),
                    ("network.uplink.max_streams", COUNT),
                    ("network.uplink.owner_share_percent", Number("%")),
                    ("network.uplink.per_stream.mbps", Number("Mbit/s")),
                    ("network.uplink.per_stream.applies_to_local", Switch),
                    ("network.lan_prefixes", List("Add a range")),
                    ("network.wan_prefixes", List("Add a range")),
                    ("network.congestion_control", Widget::Choice(CONGESTION)),
                ],
            ),
            advanced(
                "Advanced network",
                &[
                    ("network.ipv6", Switch),
                    ("network.announce.interfaces", List("Add an interface")),
                    (
                        "network.announce.ignore_interfaces",
                        List("Add an interface"),
                    ),
                    ("network.announce.ignore", List("Add an address or range")),
                    ("network.announce.add", List("Add an address")),
                    ("network.first_frame_timeout_secs", SECS),
                    ("network.stalled_read_secs", SECS),
                    ("network.max_stranger_connections", COUNT),
                ],
            ),
            sensitive(
                "These decide who can reach this server from outside your home, and which client addresses it believes.",
                &[
                    ("network.wan", Widget::Custom(Custom::WanSwitch)),
                    ("admin.public_url", Text),
                    ("admin.trusted_proxies", List("Add an address")),
                ],
            ),
        ],
    },
    Page {
        slug: "sign-in",
        area: Area::SignIn,
        name: "Sign-in & apps",
        line: "Sign-in providers, trusted devices, and the apps that act for your users.",
        place: Place::Settings,
        sections: &[
            plain(
                "Sign-in providers",
                "",
                &[("auth.oidc", Widget::Custom(Custom::Oidc))],
            ),
            plain("Devices", "", &[("auth.trust_idle_days", Number("days"))]),
            plain(
                "AI agents (MCP)",
                "An MCP server for AI agents such as Claude Code and Codex, which act for the user who connects them. The switch is under Sensitive.",
                &[("mcp.allowed_redirects", Widget::Custom(Custom::Redirects))],
            ),
            advanced("Advanced sign-in", &[("admin.not_proxied", Switch)]),
            sensitive(
                "Turning these on lets software act as your users, or lets sessions travel readable on the network.",
                &[("mcp.enabled", Switch), ("admin.insecure_cookies", Switch)],
            ),
        ],
    },
    Page {
        slug: "plex",
        area: Area::Plex,
        name: "Plex",
        line: "The first Plex server and its account. Other Plex servers are sources in Libraries.",
        place: Place::Settings,
        sections: &[
            plain(
                "Plex Media Server",
                "Spoolframe reads your libraries from Plex and plays the files directly.",
                &[
                    ("plex.url", Text),
                    ("plex.token", Widget::Custom(Custom::PlexToken)),
                    ("plex.name", Text),
                ],
            ),
            plain(
                "Watch sync",
                "Watched marks and progress, kept in step with Plex.",
                &[
                    ("plex.watch_sync.user", Text),
                    ("plex.watch_sync.interval_secs", SECS),
                ],
            ),
            advanced("Advanced Plex", &[("plex.token_file", Text)]),
        ],
    },
    Page {
        slug: "transcoding",
        area: Area::Transcoding,
        name: "Transcoding",
        line: "GPUs, the encode budget, output, and the media tools.",
        place: Place::Settings,
        sections: &[
            plain(
                "Hardware",
                "Most playback is direct, with no work for the server. These settle what happens when a file has to be converted.",
                &[("transcode.gpus", Widget::Custom(Custom::Gpus))],
            ),
            plain(
                "Limits",
                "",
                &[
                    ("transcode.max_jobs", COUNT),
                    ("transcode.max_gpu_jobs", COUNT),
                    ("transcode.read_ahead_secs", SECS),
                    ("transcode.scratch_limit_mb", Gib),
                ],
            ),
            plain(
                "Output",
                "",
                &[
                    (
                        "transcode.video_codecs",
                        Widget::Custom(Custom::VideoCodecs),
                    ),
                    ("transcode.tone_map", Widget::Choice(TONE_MAPS)),
                    ("transcode.gpu_filters", Switch),
                ],
            ),
            advanced(
                "Advanced transcoding",
                &[
                    ("transcode.max_remux", COUNT),
                    ("transcode.scratch_dir", Text),
                    ("transcode.copy_segment_secs", SECS),
                    ("media.ffmpeg", Text),
                    ("media.ffprobe", Text),
                    ("media.tesseract", Text),
                    ("experimental.libplacebo_shared_on_amd", Switch),
                ],
            ),
            sensitive(
                "Off leaves the media tools unconfined: they read files anyone who can add to a library made.",
                &[("media.sandbox", Widget::Choice(SANDBOX))],
            ),
        ],
    },
    Page {
        slug: "media",
        area: Area::Media,
        name: "Media & storage",
        line: "Previews, optimized versions, downloads, caches and watch progress.",
        place: Place::Settings,
        sections: &[
            plain(
                "Frame previews",
                "",
                &[
                    ("previews.when", Widget::Choice(PREVIEWS_WHEN)),
                    ("previews.workers", COUNT),
                ],
            ),
            plain(
                "Optimized versions",
                "",
                &[
                    ("optimized.binge_ahead", Switch),
                    ("optimized.dir", Text),
                    ("optimized.max_size_mb", Gib),
                ],
            ),
            plain(
                "Downloads",
                "",
                &[("downloads.dir", Text), ("downloads.max_size_mb", Gib)],
            ),
            advanced(
                "Advanced media",
                &[
                    ("previews.interval_secs", SECS),
                    ("previews.cache_mb", Gib),
                    ("previews.memory_mb", MIB),
                    ("artwork.cache_mb", Gib),
                    ("artwork.memory_mb", MIB),
                    ("catalog.memory_mb", MIB),
                    ("watch.started_after_secs", SECS),
                    ("watch.watched_percent", Number("%")),
                ],
            ),
        ],
    },
    Page {
        slug: "subtitles",
        area: Area::Subtitles,
        name: "Subtitles",
        line: "Picture subtitles read as text, and subtitle search for everyone.",
        place: Place::Settings,
        sections: &[
            plain("Picture subtitles", "", &[("subtitles.ocr", Switch)]),
            plain(
                "Subtitle search",
                "From OpenSubtitles or SubDL, for everyone.",
                &[
                    ("subtitles.sources", Widget::Many(SUBTITLE_SITES)),
                    ("opensubtitles.username", Text),
                    (
                        "opensubtitles.password",
                        Widget::EnvRef("SF_ENV_OPENSUBTITLES_PASSWORD"),
                    ),
                    ("opensubtitles.languages", List("Add a language")),
                    ("subdl.api_key", Widget::EnvRef("SF_ENV_SUBDL_KEY")),
                    ("subdl.languages", List("Add a language")),
                    ("subtitles.daily_limit_per_user", COUNT),
                ],
            ),
            advanced(
                "Advanced subtitles",
                &[
                    ("subtitles.ocr_threads", COUNT),
                    ("subtitles.cache_dir", Text),
                    ("subtitles.sync_at_once", COUNT),
                    ("subtitles.daily_limit_per_peer", COUNT),
                ],
            ),
        ],
    },
    Page {
        slug: "monitoring",
        area: Area::Monitoring,
        name: "Monitoring",
        line: "Prometheus scraping, OpenTelemetry export, and the log kept for Logs.",
        place: Place::Settings,
        sections: &[
            plain(
                "Prometheus",
                "",
                &[
                    ("admin.metrics", Switch),
                    ("admin.metrics_from", Widget::Custom(Custom::MetricsFrom)),
                ],
            ),
            plain(
                "OpenTelemetry",
                "Off: with no endpoint, nothing leaves the server.",
                &[
                    ("telemetry.otlp.endpoint", Text),
                    ("telemetry.otlp.protocol", Widget::Choice(OTLP_PROTOCOLS)),
                    ("telemetry.otlp.headers", Widget::Custom(Custom::Headers)),
                    ("telemetry.otlp.traces", Switch),
                    ("telemetry.otlp.metrics", Switch),
                    ("telemetry.otlp.logs", Switch),
                ],
            ),
            advanced("Advanced monitoring", &[("logs.buffer_lines", COUNT)]),
        ],
    },
    Page {
        slug: "experimental",
        area: Area::Experimental,
        name: "Experimental",
        line: "These may change or go away in any release, without notice. The server logs the ones on.",
        place: Place::Settings,
        sections: &[plain(
            "Flags",
            "",
            &[
                ("experimental.index_ahead", Switch),
                ("experimental.smart_crop", Switch),
                ("http_playback.address", Text),
            ],
        )],
    },
    Page {
        slug: "federation",
        area: Area::Federation,
        name: "Federation",
        line: "Pairing with other servers, and the limits on what they ask of this one.",
        place: Place::Federation,
        sections: &[
            plain("Sync", "", &[("federation.sync_interval_secs", SECS)]),
            advanced(
                "Advanced federation",
                &[
                    ("federation.cache_max_mb", Gib),
                    ("federation.cache_total_mb", Gib),
                    ("federation.sync_max_pages", COUNT),
                    ("federation.sync_max_items", COUNT),
                    ("federation.max_cached_items", COUNT),
                    ("federation.sync_max_metadata_mb", MIB),
                    ("federation.max_cached_metadata_mb", MIB),
                    ("federation.walk_reuse_secs", SECS),
                    ("federation.max_streams", COUNT),
                    ("federation.artwork_burst", COUNT),
                    ("federation.assertion_burst", COUNT),
                    ("federation.tokens_per_user", COUNT),
                    ("federation.tokens_per_server", COUNT),
                    ("federation.download_tokens_per_user", COUNT),
                    ("federation.download_tokens_per_server", COUNT),
                    ("federation.play_sessions_per_server", COUNT),
                    ("federation.subtitle_burst", COUNT),
                ],
            ),
            sensitive(
                "On, other servers can pair with this one and play what you share with them.",
                &[("experimental.federation", Switch)],
            ),
        ],
    },
    Page {
        slug: "libraries",
        area: Area::Libraries,
        name: "Libraries and paths",
        line: "Plex libraries served, and where their files are.",
        place: Place::Libraries,
        sections: &[
            plain(
                "Plex libraries served",
                "Which Plex libraries feed Movies, Shows and Music. Other Plex servers are sources above.",
                &[
                    ("libraries", Widget::Custom(Custom::Libraries)),
                    ("plex.servers", Widget::Custom(Custom::Libraries)),
                ],
            ),
            plain(
                "Path mapping",
                "Where each Plex library’s files are on this server.",
                &[("plex.path_mapping", Widget::Custom(Custom::PathMapping))],
            ),
        ],
    },
];

/// The Sensitive block's settings, each with what turning it the risky
/// way does, for "Confirm it's you before …".
pub const SENSITIVE: &[(&str, &str)] = &[
    (
        "network.wan",
        "advertising a public address: apps outside your home are told where to reach this server",
    ),
    (
        "admin.public_url",
        "changing the public address: passkeys and sign-in providers are tied to it",
    ),
    (
        "admin.trusted_proxies",
        "changing the trusted proxies: the server believes the client address they forward",
    ),
    (
        "mcp.enabled",
        "turning on AI agents (MCP): agents will act for the users who connect them",
    ),
    (
        "admin.insecure_cookies",
        "allowing insecure cookies: sessions travel readable on the network",
    ),
    (
        "media.sandbox",
        "changing the media sandbox: off, the media tools run unconfined",
    ),
    (
        "experimental.federation",
        "turning on federation: other servers can pair with this one",
    ),
];

/// `transcode.tone_map`'s choices (PGPU-04): its word, its name, and what
/// it does.
pub const TONE_MAPS: &[(&str, &str, &str)] = &[
    (
        "auto",
        "Automatic",
        "libplacebo on the GPU where it runs, else the CPU. With GPU filters \
         on, the GPU driver's own tone map comes first (Intel).",
    ),
    (
        "cpu",
        "CPU",
        "Always on the CPU: the most CPU, the same picture on any server.",
    ),
    (
        "gpu",
        "The GPU driver's own",
        "tonemap_vaapi, which Intel's VA-API driver has. On the CPU for a GPU without it.",
    ),
    (
        "libplacebo",
        "libplacebo",
        "On the GPU through Vulkan, AMD and NVIDIA alike: fast, and little CPU. \
         On the CPU for a GPU without it.",
    ),
];

/// The whole numbers each numeric setting takes: the least, and the most
/// if there is a most. In the units it is kept in (MiB for one shown in
/// GiB). They are what the server's own check takes: a test sets each
/// edge, and one past it, through that check.
pub const BOUNDS: &[(&str, i64, Option<i64>)] = &[
    ("artwork.cache_mb", 0, None),
    ("artwork.memory_mb", 0, None),
    ("auth.trust_idle_days", 1, Some(3650)),
    ("catalog.memory_mb", 0, None),
    ("downloads.max_size_mb", 1, None),
    ("federation.artwork_burst", 0, None),
    ("federation.assertion_burst", 0, None),
    ("federation.cache_max_mb", 0, None),
    ("federation.cache_total_mb", 0, None),
    ("federation.download_tokens_per_server", 0, None),
    ("federation.download_tokens_per_user", 0, None),
    ("federation.max_cached_items", 0, None),
    ("federation.max_cached_metadata_mb", 0, None),
    ("federation.max_streams", 0, None),
    ("federation.play_sessions_per_server", 0, None),
    ("federation.subtitle_burst", 0, None),
    ("federation.sync_interval_secs", 0, None),
    ("federation.sync_max_items", 0, None),
    ("federation.sync_max_metadata_mb", 0, None),
    ("federation.sync_max_pages", 0, None),
    ("federation.tokens_per_server", 0, None),
    ("federation.tokens_per_user", 0, None),
    ("federation.walk_reuse_secs", 0, None),
    ("logs.buffer_lines", 1, Some(100000)),
    ("network.first_frame_timeout_secs", 1, None),
    ("network.max_stranger_connections", 1, Some(4294967295)),
    ("network.plex_discovery_port", 0, Some(65535)),
    ("network.port", 0, Some(65535)),
    ("network.stalled_read_secs", 1, None),
    ("network.uplink.budget_mbps", 0, None),
    ("network.uplink.max_streams", 0, Some(4294967295)),
    ("network.uplink.owner_share_percent", 0, Some(100)),
    ("network.uplink.per_stream.mbps", 0, None),
    ("optimized.max_size_mb", 1, None),
    ("plex.watch_sync.interval_secs", 10, None),
    ("previews.cache_mb", 0, None),
    ("previews.interval_secs", 1, None),
    ("previews.memory_mb", 0, None),
    ("previews.workers", 1, Some(4294967295)),
    ("subtitles.daily_limit_per_peer", 0, Some(4294967295)),
    ("subtitles.daily_limit_per_user", 0, Some(4294967295)),
    ("subtitles.ocr_threads", 1, Some(4294967295)),
    ("subtitles.sync_at_once", 1, Some(4294967295)),
    ("transcode.copy_segment_secs", 1, Some(10)),
    ("transcode.max_gpu_jobs", 1, Some(4294967295)),
    ("transcode.max_jobs", 1, Some(4294967295)),
    ("transcode.max_remux", 1, Some(4294967295)),
    ("transcode.read_ahead_secs", 0, None),
    ("transcode.scratch_limit_mb", 0, None),
    ("watch.started_after_secs", 0, None),
    ("watch.watched_percent", 1, Some(100)),
];

/// The bounds of `key`, a number.
pub fn bounds(key: &str) -> Option<(i64, Option<i64>)> {
    BOUNDS
        .iter()
        .find(|(k, _, _)| *k == key)
        .map(|(_, min, max)| (*min, *max))
}

/// The consequence a Sensitive setting's "Confirm it's you" names, if
/// `key` is one.
pub fn risk(key: &str) -> Option<&'static str> {
    SENSITIVE
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, about)| *about)
}

/// Each place a setting is: its page, its section, and its widget, in
/// page order. `network.wan`'s switch is its second place.
pub fn places() -> impl Iterator<Item = (&'static Page, &'static Section, &'static str, Widget)> {
    PAGES.iter().flat_map(|page| {
        page.sections.iter().flat_map(move |section| {
            section
                .fields
                .iter()
                .map(move |(key, widget)| (page, section, *key, *widget))
        })
    })
}

/// Whether `widget` takes a whole number, with bounds.
pub fn is_number(widget: Widget) -> bool {
    matches!(
        widget,
        Widget::Number(_) | Widget::Gib | Widget::Custom(Custom::Budget)
    )
}

#[cfg(test)]
mod tests {
    use super::super::settings::{SETTINGS, setting};
    use super::*;

    /// Every setting has a field, once; `network.wan`'s Sensitive switch
    /// is a second place, not a second field.
    #[test]
    fn every_setting_has_one_place() {
        let mut placed: Vec<&str> = places()
            .filter(|(_, _, _, w)| *w != Widget::Custom(Custom::WanSwitch))
            .map(|(_, _, key, _)| key)
            .collect();
        placed.sort();
        let mut keys: Vec<&str> = SETTINGS.iter().map(|s| s.key).collect();
        keys.sort();
        assert_eq!(placed, keys);
    }

    #[test]
    fn each_setting_is_on_its_own_area_s_page() {
        for (page, _, key, _) in places() {
            assert_eq!(setting(key).unwrap().area, page.area, "{key}");
        }
    }

    #[test]
    fn advanced_settings_are_in_advanced_sections_and_only_they() {
        for (_, section, key, widget) in places() {
            if widget == Widget::Custom(Custom::WanSwitch) {
                continue;
            }
            assert_eq!(
                setting(key).unwrap().advanced,
                section.kind == Kind::Advanced,
                "{key} in {}",
                section.name
            );
        }
    }

    /// The Sensitive block holds the seven settings that change who can
    /// reach the server or act as a user, each with what saving it does.
    #[test]
    fn the_sensitive_block_holds_the_seven_risky_settings() {
        let mut held: Vec<&str> = places()
            .filter(|(_, s, _, _)| s.kind == Kind::Sensitive)
            .map(|(_, _, key, _)| key)
            .collect();
        held.sort();
        let mut named: Vec<&str> = SENSITIVE.iter().map(|(k, _)| *k).collect();
        named.sort();
        assert_eq!(held, named);
        assert_eq!(named.len(), 7);
        assert!(risk("admin.public_url").unwrap().contains("passkeys"));
        assert_eq!(risk("name"), None);
    }

    /// A number field has bounds, and only a number field.
    #[test]
    fn every_number_has_bounds_and_only_numbers() {
        let mut numbers: Vec<&str> = places()
            .filter(|(_, _, _, w)| is_number(*w))
            .map(|(_, _, key, _)| key)
            .collect();
        numbers.sort();
        let mut bounded: Vec<&str> = BOUNDS.iter().map(|(k, _, _)| *k).collect();
        bounded.sort();
        assert_eq!(numbers, bounded);
    }
}
