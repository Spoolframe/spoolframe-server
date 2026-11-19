//! Every setting the layers may set, and how a change to it is applied
//! (P3-10): at once, or at the next start.

/// When a changed setting takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Apply {
    /// At once: the running server picks it up.
    Live,
    /// At the next start: it is saved, and the UI says so.
    Restart,
}

/// One setting: a leaf of the config, by its dotted key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setting {
    pub key: &'static str,
    pub apply: Apply,
    /// Kept in a file of its own under the state directory, never in a
    /// layer, and never shown: only whether it is set.
    pub secret: bool,
    /// Kept in the layers, but its value never logged, shown or returned:
    /// only whether it is set. It may carry a key (P3-14's OTLP headers).
    pub write_only: bool,
    /// Where the settings pages show it.
    pub area: Area,
    /// Its name on the settings pages: "Server name".
    pub label: &'static str,
    /// One or two sentences under the label, saying what it does. Never
    /// its default, which `admin.settings` gives with the value.
    pub help: &'static str,
    /// Shown with "Show advanced settings" only.
    pub advanced: bool,
}

/// Where a setting is shown: a settings area, or a page of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Area {
    General,
    Network,
    SignIn,
    Plex,
    Transcoding,
    Media,
    Subtitles,
    Monitoring,
    Experimental,
    /// Federation's page, not Settings.
    Federation,
    /// Libraries' page, not Settings.
    Libraries,
}

impl Setting {
    const fn new(key: &'static str, apply: Apply) -> Self {
        Setting {
            key,
            apply,
            secret: false,
            write_only: false,
            area: Area::General,
            label: "",
            help: "",
            advanced: false,
        }
    }

    /// Shown in `area` as `label`, with `help` under it.
    const fn about(self, area: Area, label: &'static str, help: &'static str) -> Self {
        Setting {
            area,
            label,
            help,
            ..self
        }
    }

    /// Shown with "Show advanced settings" only.
    const fn advanced(self) -> Self {
        Setting {
            advanced: true,
            ..self
        }
    }

    /// The element id of its field on the settings pages: its key with
    /// `-` for `.`, after `setting-`.
    pub fn anchor(&self) -> String {
        format!("setting-{}", self.key.replace('.', "-"))
    }
}

const fn live(key: &'static str) -> Setting {
    Setting::new(key, Apply::Live)
}

const fn restart(key: &'static str) -> Setting {
    Setting::new(key, Apply::Restart)
}

/// The Plex token set in the web UI or with `config set`: a secret, kept
/// in `secrets/plex-token` under the state directory.
pub const PLEX_TOKEN: &str = "plex.token";

/// OpenSubtitles' account password (PX-02).
pub const OPENSUBTITLES_PASSWORD: &str = "opensubtitles.password";

/// SubDL's API key (PX-02).
pub const SUBDL_API_KEY: &str = "subdl.api_key";

/// A live setting whose value is never shown: only whether it is set.
const fn write_only(key: &'static str) -> Setting {
    Setting {
        write_only: true,
        ..Setting::new(key, Apply::Live)
    }
}

/// A live setting kept as a secret, never in a layer.
const fn secret(key: &'static str) -> Setting {
    Setting {
        secret: true,
        write_only: true,
        ..Setting::new(key, Apply::Live)
    }
}

/// Every setting, in the order `config show` lists them.
pub const SETTINGS: &[Setting] = &[
    live("name").about(Area::General, "Server name", "People see the name when they pick a server in the apps."),
    live("plex.url").about(Area::Plex, "Address", "The Plex server on your network. The port is usually 32400."),
    secret(PLEX_TOKEN).about(Area::Plex, "Plex account", "Your Plex account token. Kept in its own file on the server, not in the settings."),
    live("plex.name").about(Area::Plex, "Name", "What Spoolframe calls the Plex server; the address’s host if left out."),
    live("plex.token_file").about(Area::Plex, "Token file", "A file holding the Plex token, for a token kept outside the settings.").advanced(),
    live("plex.path_mapping").about(Area::Libraries, "Path mapping", "Rewrites the paths Plex reports to the paths this server reads."),
    restart("plex.watch_sync.user").about(Area::Plex, "Sync watch state for", "This server’s user whose watched state syncs with the Plex server owner’s account."),
    restart("plex.watch_sync.interval_secs").about(Area::Plex, "Sync every", "How often each linked user’s watch state is synced with Plex."),
    live("plex.servers").about(Area::Libraries, "More Plex servers", "Further Plex servers the catalog reads from."),
    live("libraries").about(Area::Libraries, "Libraries", "The libraries apps show, in this order, each one section of a Plex server."),
    restart("network.port").about(Area::Network, "App port", "The UDP port the apps connect to. Empty: a free one is picked at each start. Fix it when a router forwards it."),
    restart("network.ipv6").about(Area::Network, "Listen on IPv6", "The apps connect over IPv6 as well as IPv4.").advanced(),
    restart("network.relays").about(Area::Network, "Relays that let your devices through", "The relays your devices are let through. None listed: the default relays."),
    restart("network.pkarr").about(Area::Network, "Find the server by its key", "Publishes a record of where the server is, under its key, so devices find it away from home."),
    restart("network.own_relays").about(Area::Network, "Relays of your own", "Relays you run, by their key."),
    restart("network.mdns").about(Area::General, "Find on this network", "Apps on your home network find the server by themselves, with mDNS. Off: they add it from an invite or by its address."),
    restart("network.publish_lookup").about(Area::Network, "Find the server by its address", "Off: devices add the server from an invite or on your network only."),
    restart("network.publish_lookup_private").about(Area::Network, "List private addresses there too", "Off: public addresses only, so the document never shows your home or VPN network."),
    live("network.plex_discovery_port").about(Area::Network, "Plex discovery port", "Where Plex apps on this network look for the server. 0 is any free port."),
    restart("network.announce.add").about(Area::Network, "Also these addresses", "Addresses the server does not see itself, such as the public address of a NAT that forwards the port.").advanced(),
    restart("network.announce.ignore").about(Area::Network, "Never these addresses", "Addresses and ranges, such as 172.17.0.0/16 or 10.0.0.9.").advanced(),
    restart("network.announce.ignore_interfaces").about(Area::Network, "Never these interfaces", "Even when listed above. The virtual ones need not be listed.").advanced(),
    restart("network.announce.interfaces").about(Area::Network, "Only these interfaces", "Announce: addresses devices are told. None: every interface but the virtual ones. Naming a virtual one, such as docker0, tells it too.").advanced(),
    // One setting, the whole table: present or absent is what it means,
    // and its parts are checked together (`public_ip` against
    // `lookup_via`), so the web UI sets it whole.
    restart("network.wan").about(Area::Network, "Mapped port", "One public address made from a port on your router, plus public addresses you already know. Advertised while the switch under Sensitive is on."),
    live("network.lan_prefixes").about(Area::Network, "Also home", "Ranges that count as your home network."),
    live("network.wan_prefixes").about(Area::Network, "Always outside", "Ranges that never count as home."),
    live("network.uplink.budget_mbps").about(Area::Network, "Upload budget", "What playbacks outside your home may send together, in Mbit/s. 0: no budget."),
    live("network.uplink.max_streams").about(Area::Network, "Most playbacks outside at once", "0: no limit."),
    live("network.uplink.owner_share_percent").about(Area::Network, "Admins’ share while guests watch", "The share of the upload budget kept for admins while guests watch."),
    live("network.uplink.per_stream.mbps").about(Area::Network, "Cap per stream", "Each playback off the LAN is held to this, whatever quality was picked (Original too). 0: no cap."),
    live("network.uplink.per_stream.applies_to_local").about(Area::Network, "Applies to this server’s users", "On: the cap holds for this server’s accounts too."),
    restart("network.congestion_control").about(Area::Network, "Congestion control", "How the server paces what it sends: BBR keeps up on lossy links and Wi-Fi; CUBIC backs off on every lost packet."),
    // Read once, by the endpoint's options at start (`endpoint_options`):
    // a reload that changes only these waits for a restart, and says so.
    restart("network.max_stranger_connections").about(Area::Network, "Connections before sign-in", "The most connections held at once from devices that are not signed in. A signed-in device is always let in.").advanced(),
    restart("network.first_frame_timeout_secs").about(Area::Network, "Request timeout", "How long a request may take to arrive once a client opens a stream for it.").advanced(),
    restart("network.stalled_read_secs").about(Area::Network, "Stalled read timeout", "How long a media or artwork stream may wait for its reader before it is reset.").advanced(),
    restart("admin.listen").about(Area::Network, "Listen address", "Where the web UI answers, on this machine."),
    restart("admin.public_url").about(Area::Network, "Public address", "The address people type to open this page. It’s also the address passkeys and sign-in providers are tied to."),
    restart("admin.trusted_proxies").about(Area::Network, "Trusted proxies", "Addresses of your reverse proxy, so the server believes the client address it forwards."),
    restart("admin.not_proxied").about(Area::SignIn, "Not behind a proxy", "On: browsers reach the web UI directly, not through a reverse proxy.").advanced(),
    live("admin.insecure_cookies").about(Area::SignIn, "Allow insecure cookies", "Only for plain HTTP across the network. On, sessions can be read by anyone watching the network."),
    live("admin.metrics").about(Area::Monitoring, "Serve /metrics", "Answers GET /metrics on the web UI’s address with Prometheus text."),
    live("admin.metrics_from").about(Area::Monitoring, "Allowed from", "The addresses allowed to read /metrics. Anyone else is refused."),
    live("mcp.enabled").about(Area::SignIn, "AI agents (MCP)", "Agents act for the user who connects them, with that user’s rights."),
    // The whole list, as the web UI saves it.
    live("mcp.allowed_redirects").about(Area::SignIn, "Allowed redirects", "The apps allowed to connect agents, by their redirect address. None: no app can connect."),
    live("auth.oidc").about(Area::SignIn, "Providers", "Sign in with your own identity provider (OpenID Connect)."),
    restart("auth.trust_idle_days").about(Area::SignIn, "Forget an unused trusted device after", "Days a trusted device may go unused before it signs in again."),
    restart("artwork.cache_mb").about(Area::Media, "Artwork cache", "Posters and backdrops kept for the apps.").advanced(),
    live("artwork.memory_mb").about(Area::Media, "Artwork in memory", "The ones shown most, served without reading the disk.").advanced(),
    live("catalog.memory_mb").about(Area::Media, "Plex replies in memory", "Libraries and items kept a minute, so Home asks Plex less.").advanced(),
    live("watch.started_after_secs").about(Area::Media, "Started after", "Played this long, an item counts as started.").advanced(),
    live("watch.watched_percent").about(Area::Media, "Watched at", "Played this far, it counts as watched, unless it has credits markers.").advanced(),
    restart("media.ffmpeg").about(Area::Transcoding, "ffmpeg", "The ffmpeg the server runs: a name on the PATH or a full path.").advanced(),
    restart("media.ffprobe").about(Area::Transcoding, "ffprobe", "The ffprobe the server runs: a name on the PATH or a full path.").advanced(),
    restart("media.tesseract").about(Area::Transcoding, "tesseract", "The tesseract the server runs for OCR: a name on the PATH or a full path.").advanced(),
    restart("media.sandbox").about(Area::Transcoding, "Sandbox", "Confines ffmpeg, ffprobe and tesseract, so a bad file cannot reach the rest of the server."),
    restart("transcode.gpus").about(Area::Transcoding, "GPUs", "The GPUs encodes may use."),
    live("transcode.max_jobs").about(Area::Transcoding, "Encode budget", "Software video encodes at once. A GPU encode weighs half of one; a remux nothing."),
    live("transcode.max_remux").about(Area::Transcoding, "Remuxes at once", "They cost next to no CPU, but each holds the file open and writes to the scratch directory.").advanced(),
    live("transcode.max_gpu_jobs").about(Area::Transcoding, "GPU encodes per GPU", "GPU video encodes at once on each GPU, within the encode budget too."),
    restart("transcode.scratch_dir").about(Area::Transcoding, "Scratch directory", "Where encodes write their segments; the state directory’s cache if left out.").advanced(),
    live("transcode.scratch_limit_mb").about(Area::Transcoding, "Scratch space", "What all encodes may write together."),
    live("transcode.read_ahead_secs").about(Area::Transcoding, "Read ahead", "How far an encode may run ahead of the player."),
    live("transcode.copy_segment_secs").about(Area::Transcoding, "Copied segment length", "How long the HLS segments of a copied video are at least. Shorter switches quality and resumes sooner.").advanced(),
    live("transcode.gpu_filters").about(Area::Transcoding, "GPU filters", "Tone map and burn in subtitles on the GPU."),
    live("transcode.tone_map").about(Area::Transcoding, "HDR tone map", "How HDR video is made SDR for a screen that shows no HDR."),
    live("transcode.video_codecs").about(Area::Transcoding, "Output codecs", "Tried in this order for an encode."),
    restart("subtitles.ocr").about(Area::Subtitles, "Offer picture subtitles as text (OCR)", "Picture subtitles (PGS, VobSub, DVB) are read into text, so any device can show them."),
    restart("subtitles.ocr_threads").about(Area::Subtitles, "OCR threads", "tesseract processes at once; half the CPUs if left out.").advanced(),
    restart("subtitles.cache_dir").about(Area::Subtitles, "Cache directory", "Where subtitles read by OCR are kept; the state directory’s cache if left out.").advanced(),
    restart("subtitles.sync_at_once").about(Area::Subtitles, "Syncs at once", "Subtitle syncs that run at once; the rest wait their turn.").advanced(),
    live("previews.when").about(Area::Media, "When previews are made", "When frame previews for scrubbing are made."),
    live("previews.interval_secs").about(Area::Media, "Picture every", "The time between two pictures.").advanced(),
    // The disk tier is laid out at start (crate::hybrid).
    restart("previews.cache_mb").about(Area::Media, "Previews cache", "The oldest go first; a two-hour film’s take a few MiB.").advanced(),
    live("previews.memory_mb").about(Area::Media, "Previews in memory", "The films scrubbed last, kept in memory as well.").advanced(),
    live("previews.workers").about(Area::Media, "Made at once", "Previews made at the same time."),
    // The pool is built at start (PX-37).
    restart("optimized.binge_ahead").about(Area::Media, "Make the next episode ahead", "While someone watches."),
    restart("optimized.dir").about(Area::Media, "Directory", "A full path, starting with /."),
    restart("optimized.max_size_mb").about(Area::Media, "Most to keep", "The least recently used go first."),
    // The download area is laid out at start (PX-38).
    restart("downloads.dir").about(Area::Media, "Directory", "Where downloads are made and kept; inside the optimized versions directory if left out."),
    restart("downloads.max_size_mb").about(Area::Media, "Most to keep", "The most downloads may take on disk."),
    live("logs.buffer_lines").about(Area::Monitoring, "Log lines kept", "The server log lines kept in memory for the Logs page.").advanced(),
    live("telemetry.otlp.endpoint").about(Area::Monitoring, "Endpoint", "An OpenTelemetry Collector or Grafana Alloy, for example."),
    live("telemetry.otlp.protocol").about(Area::Monitoring, "Protocol", "gRPC, or HTTP with protobuf."),
    write_only("telemetry.otlp.headers").about(Area::Monitoring, "Headers", "Values are kept like passwords and never shown again."),
    live("telemetry.otlp.traces").about(Area::Monitoring, "Send traces", "Export traces to the endpoint."),
    live("telemetry.otlp.metrics").about(Area::Monitoring, "Send metrics", "Export metrics to the endpoint."),
    live("telemetry.otlp.logs").about(Area::Monitoring, "Send logs", "Export logs to the endpoint."),
    // Detection reads it, at start.
    restart("experimental.libplacebo_shared_on_amd").about(Area::Transcoding, "Tone mapping on AMD", "On an AMD GPU, tone map on ffmpeg’s Vulkan device, so the frames stay on the GPU.").advanced(),
    live("experimental.index_ahead").about(Area::Experimental, "Keyframes read ahead", "Read keyframes ahead of any play."),
    // The endpoint's ALPNs are fixed when it binds (P8-01).
    restart("experimental.federation").about(Area::Federation, "Pair with other servers", "Lets this server pair with other servers. Experimental: pair only with servers you run."),
    // The cache is built with the scanner, at start.
    restart("experimental.smart_crop").about(Area::Experimental, "Smart crop", "Find faces in backdrops, so apps place a wide crop around them."),
    live("federation.sync_interval_secs").about(Area::Federation, "Sync every", "How often each paired server's libraries are synced."),
    live("federation.cache_max_mb").about(Area::Federation, "Images per server", "A paired server's artwork kept here.").advanced(),
    live("federation.cache_total_mb").about(Area::Federation, "Images in all", "Every paired server's artwork together.").advanced(),
    // P8-04 (2026-10-23): a pair's limits. Streams apply to the
    // connections made after; the rest at once.
    live("federation.sync_max_pages").about(Area::Federation, "Pages per sync", "Of one library's sync.").advanced(),
    live("federation.sync_max_items").about(Area::Federation, "Items per sync", "Of one library's sync; raise it for a very large library.").advanced(),
    live("federation.max_cached_items").about(Area::Federation, "Items per server", "Cached of one paired server, every library together.").advanced(),
    live("federation.sync_max_metadata_mb").about(Area::Federation, "Metadata per sync", "Of one library's sync.").advanced(),
    live("federation.max_cached_metadata_mb").about(Area::Federation, "Metadata per server", "Cached of one paired server, every library together.").advanced(),
    live("federation.walk_reuse_secs").about(Area::Federation, "Walk reused for", "One walk of a library answers every pair's sync for this long.").advanced(),
    live("federation.max_streams").about(Area::Federation, "Streams per connection", "A pair's requests answered at once.").advanced(),
    live("federation.artwork_burst").about(Area::Federation, "Images in a burst", "A pair's image requests at once, then 100 a second.").advanced(),
    live("federation.assertion_burst").about(Area::Federation, "Plays in a burst", "Plays a pair's users start at once, then 1 a second.").advanced(),
    live("federation.tokens_per_user").about(Area::Federation, "Play tokens per user", "That work at once, for one user of a pair.").advanced(),
    live("federation.tokens_per_server").about(Area::Federation, "Play tokens per server", "That work at once, for a pair.").advanced(),
    live("federation.download_tokens_per_user").about(Area::Federation, "Download tokens per user", "Downloads being made or read at once, for one user of a pair.").advanced(),
    live("federation.download_tokens_per_server").about(Area::Federation, "Download tokens per server", "Downloads being made or read at once, for a pair.").advanced(),
    live("federation.play_sessions_per_server").about(Area::Federation, "Plays per server", "Plays a pair's users hold open at once.").advanced(),
    live("federation.subtitle_burst").about(Area::Federation, "Subtitle asks in a burst", "A pair's subtitle searches and downloads at once, then 1 every 10 s.").advanced(),
    // The port is bound at start (PX-49).
    restart("http_playback.listen").about(Area::Network, "HTTP playback for AirPlay", "The TCP port AirPlay plays from, on every address of the server. Plain HTTP: open it to your home network only."),
    restart("http_playback.address").about(Area::Experimental, "AirPlay: address for apps outside your home", "The address AirPlay apps outside your home use. The port is in Network › Ports."),
    // PX-02. OpenSubtitles' API key is built in: not a setting. The
    // password and SubDL's key are `${SF_ENV_…}` references, never shown:
    // the runtime layer refuses the secret itself.
    live("opensubtitles.username").about(Area::Subtitles, "OpenSubtitles account", "Your username at opensubtitles.com. Downloads count against it."),
    write_only(OPENSUBTITLES_PASSWORD).about(Area::Subtitles, "OpenSubtitles password", "Your password at opensubtitles.com. Never shown once set."),
    live("opensubtitles.languages").about(Area::Subtitles, "OpenSubtitles languages", "Languages searched first, in order."),
    write_only(SUBDL_API_KEY).about(Area::Subtitles, "SubDL’s API key", "Your API key at subdl.com. Never shown once set."),
    live("subdl.languages").about(Area::Subtitles, "SubDL languages", "Languages searched first on SubDL, in order."),
    live("subtitles.sources").about(Area::Subtitles, "Sites searched", "Both at once, as one list. Viewers do not choose."),
    live("subtitles.daily_limit_per_user").about(Area::Subtitles, "Downloads a day per user", "Subtitle downloads each user may make a day, from every site. 0: no limit."),
    live("subtitles.daily_limit_per_peer").about(Area::Subtitles, "Downloads a day per paired server", "Subtitle downloads each paired server’s users may make a day, together. 0: no limit.").advanced(),
    // Not `opensubtitles.api_url` nor `subdl.api_url` or
    // `subdl.download_url`: in the config file only (tests). Set at run
    // time, they would send the key, which may name any `SF_ENV_`
    // variable, wherever an admin pointed it.
];

/// The setting called `key`, if there is one.
pub fn setting(key: &str) -> Option<&'static Setting> {
    SETTINGS.iter().find(|s| s.key == key)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// The settings pages and their search show every setting by its
    /// label and help; none is left nameless.
    #[test]
    fn every_setting_has_a_label_and_help() {
        for s in SETTINGS {
            assert!(!s.label.is_empty(), "{} has no label", s.key);
            assert!(!s.help.is_empty(), "{} has no help", s.key);
        }
    }

    /// The default comes with the value (`SettingEntry::default`): help
    /// that typed it would go stale the day the default changes.
    #[test]
    fn no_help_types_a_default() {
        for s in SETTINGS {
            assert!(
                !s.help.starts_with("Default ") && !s.help.contains("Default: "),
                "{}: {}",
                s.key,
                s.help
            );
        }
    }

    #[test]
    fn every_key_and_anchor_is_one_setting_s() {
        let keys: HashSet<_> = SETTINGS.iter().map(|s| s.key).collect();
        let anchors: HashSet<_> = SETTINGS.iter().map(Setting::anchor).collect();
        assert_eq!(keys.len(), SETTINGS.len());
        assert_eq!(anchors.len(), SETTINGS.len());
        assert_eq!(
            setting("network.port").unwrap().anchor(),
            "setting-network-port"
        );
    }
}
