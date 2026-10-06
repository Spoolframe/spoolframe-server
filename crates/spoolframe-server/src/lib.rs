//! The Spoolframe media server. The binary only parses arguments and wires
//! what lives here (`docs/agent_guide/crate_layering.md`).
//!
//! AGPL-3.0-or-later. Clients reach it only over the protocol; they never
//! link this crate.

pub mod admin;
mod admin_http;
pub mod app_passkeys;
pub mod artwork;
pub mod auth;
pub mod catalog;
mod client;
pub mod config;
pub mod connected;
pub mod device_sign_in;
pub mod encode;
pub mod identity;
mod keyed_locks;
pub mod libraries;
pub mod logs;
pub mod metrics;
pub mod network;
pub mod oidc;
mod operations;
pub mod passkeys;
pub mod planner;
pub mod playback;
pub mod playback_debug;
pub mod plex_accounts;
pub mod plex_sync;
pub mod previews;
pub mod principal;
pub mod redact;
pub mod runtime;
pub mod search;
pub mod store;
pub mod subtitles;
pub mod telemetry;
pub mod tracks;
pub mod transcode;
pub mod watch;
mod web;
