//! The Spoolframe media server. The binary only parses arguments and wires
//! what lives here (`docs/agent_guide/crate_layering.md`).
//!
//! AGPL-3.0-or-later. Clients reach it only over the protocol; they never
//! link this crate.

pub mod admin;
mod admin_http;
pub mod artwork;
pub mod auth;
pub mod catalog;
mod client;
pub mod config;
pub mod device_sign_in;
pub mod encode;
pub mod identity;
pub mod network;
pub mod passkeys;
pub mod planner;
pub mod playback;
pub mod plex_sync;
pub mod principal;
pub mod runtime;
pub mod store;
pub mod subtitles;
pub mod tracks;
pub mod transcode;
pub mod watch;
mod web;
