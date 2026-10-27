//! The Spoolframe web UI (P3): a Dioxus app the server embeds and serves on
//! its admin listener, beside the JSON-RPC socket it talks to.
//!
//! It never holds a token: signing in is `POST /auth/login`, whose answer
//! sets an `HttpOnly` cookie, and the browser sends that cookie on the
//! socket by itself.

// Used by the app, which only the wasm build has; the host has its tests.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod activity;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod advanced;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod approve;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod browse;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod catalog;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod connect;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod experimental;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod federation;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod gpus;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod invites;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod libraries;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod licenses;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod logic;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod logs;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod network;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod optimized;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod overview;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod playback;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod playlists;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod plex_sync;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod portable;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod preferences;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod providers;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod remote;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod restart;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod setup;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod sso;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod subtitle_search;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod subtitle_timing;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod telemetry;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod trust;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod uplink;

#[cfg(target_arch = "wasm32")]
mod app;

fn main() {
    #[cfg(target_arch = "wasm32")]
    app::launch();
}

#[cfg(test)]
mod tests {
    /// The page dx builds the bundle's `index.html` from (the crate's
    /// `index.html`): dx's own template, with what keeps a browser's
    /// night mode off the already-dark pages, in the page itself, where
    /// Dark Reader (Firefox for iOS's Night Mode) reads it as it loads.
    #[test]
    fn the_page_tells_night_modes_it_is_dark_already() {
        let page = include_str!("../index.html");
        let head = &page[..page.find("</head>").expect("a head")];
        assert!(head.contains(r#"<meta name="color-scheme" content="light dark">"#));
        assert!(head.contains(r#"<meta name="darkreader-lock">"#));
        // dx's template otherwise: its title, and the app's root.
        assert!(head.contains("<title>{app_title}</title>"));
        assert!(page.contains(r#"<div id="main"></div>"#));
    }
}
