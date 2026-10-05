//! Every integration test of spoolframe-server, as modules of one test binary: each
//! file directly under `tests/` would be a binary of its own, linking the
//! crate and all its dependencies again. A new test goes in as a module
//! here, never as a new file under `tests/`.

mod support;

mod admin;
mod admin_web;
mod app_passkeys;
mod artwork;
mod catalog;
mod cli;
mod device_sign_in;
mod encode;
mod endpoint;
mod lan;
mod libraries;
mod license_boundary;
mod not_set_up;
mod passkey_setup;
mod passkeys;
mod pin;
mod playback;
mod plex_link;
mod plex_sync;
mod previews;
mod profile;
mod protocol;
mod sdk;
mod search;
mod sessions;
mod settings;
mod sso;
mod subtitles;
mod throughput;
mod tracks;
mod watch;
mod wizard;
