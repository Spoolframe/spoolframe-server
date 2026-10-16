//! The Spoolframe media server. The binary only parses arguments and wires
//! what lives here, so tests reach everything the binary does.
//!
//! AGPL-3.0-or-later. Clients reach it only over the protocol; they never
//! link this crate.

pub mod admin;
mod admin_http;
pub mod admission;
pub mod announce;
pub mod app_passkeys;
pub mod artwork;
pub mod auth;
pub mod catalog;
mod client;
pub mod clock;
pub mod config;
pub mod connected;
pub mod covers;
pub mod device_sign_in;
pub mod encode;
mod events;
pub mod files;
pub mod gpu_status;
pub mod http_playback;
pub mod hybrid;
pub mod identity;
mod ifaddrs;
pub mod index_ahead;
pub mod invites;
mod keyed_locks;
pub mod libraries;
pub mod link_stats;
pub mod logs;
pub mod mcp;
pub mod metrics;
pub mod network;
pub mod oauth;
pub mod oidc;
mod operations;
pub mod own_relay;
pub mod pairing_relay;
pub mod passkeys;
pub mod planner;
pub mod playback;
pub mod playback_debug;
pub mod playlists;
pub mod plex_accounts;
pub mod plex_sync;
pub mod portable;
pub mod previews;
pub mod principal;
pub mod protocol_info;
pub mod redact;
pub mod refusals;
pub mod relay_access;
pub mod relay_nat;
pub mod runtime;
pub mod search;
pub mod store;
pub mod subtitles;
pub mod telemetry;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod ties;
pub mod tls;
pub mod tracks;
pub mod transcode;
pub mod uplink;
pub mod wan;
pub mod watch;
mod web;
