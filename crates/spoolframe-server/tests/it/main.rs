//! Every integration test of spoolframe-server, as modules of one test binary: each
//! file directly under `tests/` would be a binary of its own, linking the
//! crate and all its dependencies again. A new test goes in as a module
//! here, never as a new file under `tests/`.

mod support;

mod admin;
mod admin_web;
mod admission;
mod app_passkeys;
mod artwork;
mod catalog;
mod cli;
mod confirm;
mod connection_log;
mod device_sign_in;
mod encode;
mod endpoint;
mod forget_device;
mod lan;
mod libraries;
mod license_boundary;
mod metrics;
mod not_set_up;
mod passkey_relay;
mod passkey_setup;
mod passkeys;
mod pin;
mod playback;
mod playback_authz;
mod playback_debug;
mod plex_link;
mod plex_sync;
mod previews;
mod probe;
mod profile;
mod protocol;
mod recent_admin;
mod refusals;
mod sdk;
mod search;
mod server_addresses;
mod server_pin;
mod sessions;
mod settings;
mod sso;
mod subtitle_decoder;
mod subtitles;
mod telemetry;
mod throughput;
mod tracks;
mod trust;
mod wan;
mod watch;
mod wizard;
