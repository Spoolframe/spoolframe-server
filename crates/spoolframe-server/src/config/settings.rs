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
}

const fn live(key: &'static str) -> Setting {
    Setting {
        key,
        apply: Apply::Live,
        secret: false,
        write_only: false,
    }
}

const fn restart(key: &'static str) -> Setting {
    Setting {
        key,
        apply: Apply::Restart,
        secret: false,
        write_only: false,
    }
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
        key,
        apply: Apply::Live,
        secret: false,
        write_only: true,
    }
}

/// Every setting, in the order `config show` lists them.
pub const SETTINGS: &[Setting] = &[
    live("name"),
    live("plex.url"),
    Setting {
        key: PLEX_TOKEN,
        apply: Apply::Live,
        secret: true,
        write_only: true,
    },
    live("plex.name"),
    live("plex.token_file"),
    live("plex.path_mapping"),
    restart("plex.watch_sync.user"),
    restart("plex.watch_sync.interval_secs"),
    live("plex.servers"),
    live("libraries"),
    restart("network.port"),
    restart("network.ipv6"),
    restart("network.relays"),
    restart("network.pkarr"),
    restart("network.own_relays"),
    restart("network.mdns"),
    live("network.plex_discovery_port"),
    restart("network.announce.add"),
    restart("network.announce.ignore"),
    restart("network.announce.ignore_interfaces"),
    restart("network.announce.interfaces"),
    // One setting, the whole table: present or absent is what it means,
    // and its parts are checked together (`public_ip` against
    // `lookup_via`), so the web UI sets it whole.
    restart("network.wan"),
    live("network.lan_prefixes"),
    live("network.wan_prefixes"),
    live("network.uplink.budget_mbps"),
    live("network.uplink.max_streams"),
    live("network.uplink.owner_share_percent"),
    restart("network.congestion_control"),
    // Read once, by the endpoint's options at start (`endpoint_options`):
    // a reload that changes only these waits for a restart, and says so.
    restart("network.max_stranger_connections"),
    restart("network.first_frame_timeout_secs"),
    restart("network.stalled_read_secs"),
    restart("admin.listen"),
    restart("admin.public_url"),
    restart("admin.trusted_proxies"),
    restart("admin.not_proxied"),
    live("admin.insecure_cookies"),
    live("admin.metrics"),
    live("admin.metrics_from"),
    live("mcp.enabled"),
    // The whole list, as the web UI saves it.
    live("mcp.allowed_redirects"),
    live("auth.oidc"),
    restart("auth.trust_idle_days"),
    restart("artwork.cache_mb"),
    live("artwork.memory_mb"),
    live("catalog.memory_mb"),
    live("watch.started_after_secs"),
    live("watch.watched_percent"),
    restart("media.ffmpeg"),
    restart("media.ffprobe"),
    restart("media.tesseract"),
    restart("media.sandbox"),
    restart("transcode.gpus"),
    restart("transcode.hardware"),
    restart("transcode.vaapi_device"),
    live("transcode.max_jobs"),
    live("transcode.max_remux"),
    live("transcode.max_gpu_jobs"),
    restart("transcode.scratch_dir"),
    live("transcode.scratch_limit_mb"),
    live("transcode.read_ahead_secs"),
    live("transcode.copy_segment_secs"),
    live("transcode.gpu_filters"),
    live("transcode.tone_map"),
    live("transcode.video_codecs"),
    restart("subtitles.ocr"),
    restart("subtitles.ocr_threads"),
    restart("subtitles.cache_dir"),
    restart("subtitles.sync_at_once"),
    live("previews.when"),
    live("previews.interval_secs"),
    // The disk tier is laid out at start (crate::hybrid).
    restart("previews.cache_mb"),
    live("previews.memory_mb"),
    live("previews.workers"),
    live("logs.buffer_lines"),
    live("telemetry.otlp.endpoint"),
    live("telemetry.otlp.protocol"),
    Setting {
        key: "telemetry.otlp.headers",
        apply: Apply::Live,
        secret: false,
        write_only: true,
    },
    live("telemetry.otlp.traces"),
    live("telemetry.otlp.metrics"),
    live("telemetry.otlp.logs"),
    // Detection reads it, at start.
    restart("experimental.libplacebo_shared_on_amd"),
    live("experimental.index_ahead"),
    // The port is bound at start (PX-49).
    restart("http_playback.listen"),
    restart("http_playback.address"),
    // PX-02. OpenSubtitles' API key is built in: not a setting. The
    // password and SubDL's key are `${SF_ENV_…}` references, never shown:
    // the runtime layer refuses the secret itself.
    live("opensubtitles.username"),
    write_only(OPENSUBTITLES_PASSWORD),
    live("opensubtitles.languages"),
    // Before SubDL, the limit was OpenSubtitles' own; read when
    // `subtitles.daily_limit_per_user` is left out.
    live("opensubtitles.daily_limit_per_user"),
    write_only(SUBDL_API_KEY),
    live("subdl.languages"),
    live("subtitles.sources"),
    live("subtitles.daily_limit_per_user"),
    // Not `opensubtitles.api_url` nor `subdl.api_url` or
    // `subdl.download_url`: in the config file only (tests). Set at run
    // time, they would send the key, which may name any `SF_ENV_`
    // variable, wherever an admin pointed it.
];

/// The setting called `key`, if there is one.
pub fn setting(key: &str) -> Option<&'static Setting> {
    SETTINGS.iter().find(|s| s.key == key)
}
