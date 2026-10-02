//! The Spoolframe media server. The binary only parses arguments and wires
//! what lives here (`docs/agent_guide/crate_layering.md`).
//!
//! AGPL-3.0-or-later. Clients reach it only over the protocol; they never
//! link this crate.

pub mod config;
pub mod identity;
pub mod network;
