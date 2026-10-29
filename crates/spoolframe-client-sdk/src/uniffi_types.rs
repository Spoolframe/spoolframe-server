//! The std types UniFFI passes as another type (feature `uniffi`): a
//! socket address as `host:port` and a path as a string, as an app shows
//! and opens them.

use std::net::SocketAddr;
use std::path::PathBuf;

uniffi::custom_type!(SocketAddr, String, {
    remote,
    lower: |addr| addr.to_string(),
    try_lift: |text| Ok(text.parse()?),
});

uniffi::custom_type!(PathBuf, String, {
    remote,
    lower: |path| path.to_string_lossy().into_owned(),
    try_lift: |text| Ok(PathBuf::from(text)),
});
