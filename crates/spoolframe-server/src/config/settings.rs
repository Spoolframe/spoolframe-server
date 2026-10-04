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
}

const fn live(key: &'static str) -> Setting {
    Setting {
        key,
        apply: Apply::Live,
        secret: false,
    }
}

const fn restart(key: &'static str) -> Setting {
    Setting {
        key,
        apply: Apply::Restart,
        secret: false,
    }
}

/// The Plex token set in the web UI or with `config set`: a secret, kept
/// in `secrets/plex-token` under the state directory.
pub const PLEX_TOKEN: &str = "plex.token";

/// Every setting, in the order `config show` lists them.
pub const SETTINGS: &[Setting] = &[
    live("name"),
    live("plex.url"),
    Setting {
        key: PLEX_TOKEN,
        apply: Apply::Live,
        secret: true,
    },
    live("plex.name"),
    live("plex.token_file"),
    live("plex.path_mapping"),
    restart("plex.watch_sync.user"),
    restart("plex.watch_sync.interval_secs"),
    live("plex.servers"),
    live("libraries"),
    restart("network.port"),
    restart("network.relay"),
    restart("network.mdns"),
    restart("admin.listen"),
    restart("admin.public_url"),
    restart("admin.trusted_proxies"),
    live("admin.insecure_cookies"),
    live("auth.oidc"),
    restart("artwork.cache_mb"),
    live("watch.started_after_secs"),
    live("watch.watched_percent"),
    restart("media.ffmpeg"),
    restart("media.ffprobe"),
    restart("media.tesseract"),
    restart("transcode.hardware"),
    restart("transcode.vaapi_device"),
    live("transcode.max_jobs"),
    restart("transcode.scratch_dir"),
    live("transcode.scratch_limit_mb"),
    live("transcode.read_ahead_secs"),
    restart("subtitles.ocr"),
    restart("subtitles.ocr_threads"),
    restart("subtitles.cache_dir"),
    live("previews.when"),
    live("previews.interval_secs"),
    live("previews.cache_mb"),
    live("previews.workers"),
];

/// The setting called `key`, if there is one.
pub fn setting(key: &str) -> Option<&'static Setting> {
    SETTINGS.iter().find(|s| s.key == key)
}
