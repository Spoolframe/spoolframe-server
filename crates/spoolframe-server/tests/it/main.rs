//! Every integration test of spoolframe-server, as modules of one test binary: each
//! file directly under `tests/` would be a binary of its own, linking the
//! crate and all its dependencies again. A new test goes in as a module
//! here, never as a new file under `tests/`.

mod support;

mod admin;
mod artwork;
mod cli;
mod device_sign_in;
mod encode;
mod endpoint;
mod lan;
mod license_boundary;
mod pin;
mod playback;
mod protocol;
mod sdk;
mod sessions;
mod subtitles;
mod throughput;
mod tracks;
mod watch;
