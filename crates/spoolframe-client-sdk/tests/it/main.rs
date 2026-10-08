//! Every integration test of spoolframe-client-sdk, as modules of one test
//! binary: each file directly under `tests/` would be a binary of its own,
//! linking the crate and all its dependencies again. A new test goes in as
//! a module here, never as a new file under `tests/`.

mod addresses;
mod reconnect;
mod stalls;
mod stub;
mod web_url;
