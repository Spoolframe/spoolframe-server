//! Every integration test of spoolframe-client-sdk, as modules of one test
//! binary: each file directly under `tests/` would be a binary of its own,
//! linking the crate and all its dependencies again. A new test goes in as
//! a module here, never as a new file under `tests/`.

mod addresses;
mod closing;
mod features;
mod invite_lookup;
mod lan_info;
mod leave;
mod one_time_password;
mod playlists;
mod reachability;
mod reconnect;
mod search;
mod stalls;
mod stream_window;
mod stub;
mod web_url;
