//! A portable user's root stays on their phone (P7-01, ARCHITECTURE §43):
//! the server never generates, recovers or accepts one. The recovery
//! phrase is the root's only source, so the server, and the protocol it
//! speaks, must not even build the BIP-39 code that reads one.
//!
//! Reads the real graph with `cargo tree`, one package at a time,
//! following normal and build dependencies only: the server's tests link
//! the client SDK, and with it `bip39`, which is fine. Each package is
//! resolved as it is built alone, with its own features, as a release
//! builds it: `cargo metadata` resolves the whole workspace at once, and
//! there spoolframe-client-ffi's `spoolframe-hls-bridge/sdk` would put the
//! SDK under the server's bridge, which the server takes without it
//! (PX-49).

use std::collections::BTreeSet;
use std::process::Command;

/// What reads a recovery phrase into a root.
const PHRASE_CRATE: &str = "bip39";

/// Every package `root` builds with, itself included: normal and build
/// dependencies, transitively, on any target, with `features` of its own
/// on.
fn shipped_graph(root: &str, features: &[&str]) -> BTreeSet<String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let output = Command::new(cargo)
        .args(["tree", "--manifest-path"])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml"))
        .args(["-p", root, "-e", "normal,build", "--target", "all"])
        .args(["--prefix", "none", "--format", "{p}"])
        .args(["--features", &features.join(",")])
        .output()
        .expect("cargo tree runs on the workspace");
    assert!(
        output.status.success(),
        "cargo tree -p {root} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("cargo tree prints UTF-8")
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_owned)
        .collect()
}

#[test]
fn neither_the_server_nor_the_protocol_builds_the_recovery_phrase_code() {
    // The check means something only if the phrase code is in the
    // workspace at all: the SDK builds with it.
    assert!(
        shipped_graph("spoolframe-client-sdk", &[]).contains(PHRASE_CRATE),
        "the client SDK no longer builds with {PHRASE_CRATE}; update this test"
    );
    for (root, features) in [
        ("spoolframe-server", &["embed-web", "test-support"][..]),
        ("spoolframe-protocol", &["soft-key"][..]),
    ] {
        let graph = shipped_graph(root, features);
        assert!(
            graph.contains("spoolframe-protocol"),
            "{root}'s graph looks wrong: {graph:?}"
        );
        assert!(
            !graph.contains(PHRASE_CRATE),
            "{root} builds with {PHRASE_CRATE}: a portable root could be made or read there"
        );
        assert!(
            !graph.contains("spoolframe-client-sdk"),
            "{root} builds the client SDK"
        );
    }
}
