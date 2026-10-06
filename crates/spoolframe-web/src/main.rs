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
mod catalog;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod libraries;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod logic;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod logs;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod playback;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod plex_sync;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod preferences;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod setup;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod sso;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod telemetry;

#[cfg(target_arch = "wasm32")]
mod app;

fn main() {
    #[cfg(target_arch = "wasm32")]
    app::launch();
}
