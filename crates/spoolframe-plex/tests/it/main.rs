//! Every integration test of spoolframe-plex, as modules of one test binary: each
//! file directly under `tests/` would be a binary of its own, linking the
//! crate and all its dependencies again. A new test goes in as a module
//! here, never as a new file under `tests/`.

mod check_paths;
mod http;
mod live_pms;
