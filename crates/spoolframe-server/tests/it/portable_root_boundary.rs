//! A portable user's root stays on their phone (P7-01, ARCHITECTURE §43):
//! the server never generates, recovers or accepts one. The recovery
//! phrase is the root's only source, so the server, and the protocol it
//! speaks, must not even build the BIP-39 code that reads one.
//!
//! Reads the real graph with `cargo metadata`, following normal and build
//! dependencies only: the server's tests link the client SDK, and with it
//! `bip39`, which is fine.

use std::collections::{BTreeSet, HashMap};

use cargo_metadata::{CargoOpt, DependencyKind, Metadata, MetadataCommand, PackageId};

/// What reads a recovery phrase into a root.
const PHRASE_CRATE: &str = "bip39";

fn metadata() -> Metadata {
    MetadataCommand::new()
        .manifest_path(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml"))
        .features(CargoOpt::AllFeatures)
        .exec()
        .expect("cargo metadata runs on the workspace")
}

/// Every package `root` builds with, itself included: normal and build
/// dependencies, transitively, on any target.
fn shipped_graph(metadata: &Metadata, root: &str) -> BTreeSet<String> {
    let resolve = metadata.resolve.as_ref().expect("metadata has a resolve");
    let nodes: HashMap<&PackageId, _> = resolve.nodes.iter().map(|n| (&n.id, n)).collect();
    let names: HashMap<&PackageId, &str> = metadata
        .packages
        .iter()
        .map(|p| (&p.id, p.name.as_str()))
        .collect();
    let start = metadata
        .workspace_packages()
        .into_iter()
        .find(|p| p.name.as_str() == root)
        .unwrap_or_else(|| panic!("{root} is a workspace member"));
    let mut stack = vec![&start.id];
    let mut seen = BTreeSet::new();
    let mut found = BTreeSet::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        found.insert(names[id].to_owned());
        for dep in &nodes[id].deps {
            let shipped = dep
                .dep_kinds
                .iter()
                .any(|k| matches!(k.kind, DependencyKind::Normal | DependencyKind::Build));
            if shipped {
                stack.push(&dep.pkg);
            }
        }
    }
    found
}

#[test]
fn neither_the_server_nor_the_protocol_builds_the_recovery_phrase_code() {
    let metadata = metadata();
    // The check means something only if the phrase code is in the
    // workspace at all: the SDK builds with it.
    assert!(
        shipped_graph(&metadata, "spoolframe-client-sdk").contains(PHRASE_CRATE),
        "the client SDK no longer builds with {PHRASE_CRATE}; update this test"
    );
    for root in ["spoolframe-server", "spoolframe-protocol"] {
        let graph = shipped_graph(&metadata, root);
        assert!(
            graph.contains("spoolframe-protocol"),
            "{root}'s graph looks wrong: {graph:?}"
        );
        assert!(
            !graph.contains(PHRASE_CRATE),
            "{root} builds with {PHRASE_CRATE}: a portable root could be made or read there"
        );
    }
}
