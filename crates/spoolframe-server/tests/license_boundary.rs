//! The license boundary from ARCHITECTURE §56: the permissive crates
//! (types, protocol, client SDK) are what proprietary clients link, so none
//! of them may reach an AGPL crate through a normal or build dependency.
//!
//! Reads the real workspace with `cargo metadata --no-deps`, so a new crate
//! or a new dependency edge is checked without editing this file.

use std::collections::{BTreeMap, BTreeSet};

use cargo_metadata::{DependencyKind, MetadataCommand};

const PERMISSIVE: &str = "MIT OR Apache-2.0";
const AGPL: &str = "AGPL-3.0-or-later";

/// One workspace member: its license and the members it depends on.
struct Member {
    license: Option<String>,
    deps: BTreeSet<String>,
}

/// Every way the workspace breaks the boundary, one line each.
fn violations(members: &BTreeMap<String, Member>) -> Vec<String> {
    let mut found = Vec::new();
    for (name, member) in members {
        match member.license.as_deref() {
            Some(PERMISSIVE) => {
                if let Some(path) = path_to_agpl(members, name) {
                    found.push(format!(
                        "{name} is {PERMISSIVE} but reaches an AGPL crate: {}",
                        path.join(" -> ")
                    ));
                }
            }
            Some(AGPL) => {}
            other => found.push(format!(
                "{name} has license {other:?}; expected {PERMISSIVE:?} or {AGPL:?}"
            )),
        }
    }
    found
}

/// The dependency path from `start` to the first AGPL member, if there is one.
fn path_to_agpl(members: &BTreeMap<String, Member>, start: &str) -> Option<Vec<String>> {
    let mut stack = vec![vec![start.to_owned()]];
    let mut seen = BTreeSet::new();
    while let Some(path) = stack.pop() {
        let last = path.last().expect("paths are never empty");
        if !seen.insert(last.clone()) {
            continue;
        }
        for dep in &members[last].deps {
            let mut next = path.clone();
            next.push(dep.clone());
            if members[dep].license.as_deref() == Some(AGPL) {
                return Some(next);
            }
            stack.push(next);
        }
    }
    None
}

fn workspace_members() -> BTreeMap<String, Member> {
    let metadata = MetadataCommand::new()
        .manifest_path(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml"))
        .no_deps()
        .exec()
        .expect("cargo metadata runs on the workspace");
    let names: BTreeSet<String> = metadata
        .workspace_packages()
        .iter()
        .map(|p| p.name.to_string())
        .collect();
    metadata
        .workspace_packages()
        .into_iter()
        .map(|p| {
            let deps = p
                .dependencies
                .iter()
                .filter(|d| matches!(d.kind, DependencyKind::Normal | DependencyKind::Build))
                .filter(|d| names.contains(&d.name))
                .map(|d| d.name.clone())
                .collect();
            let member = Member {
                license: p.license.clone(),
                deps,
            };
            (p.name.to_string(), member)
        })
        .collect()
}

#[test]
fn the_workspace_keeps_permissive_crates_free_of_agpl() {
    let members = workspace_members();
    assert!(
        members.contains_key("spoolframe-types") && members.contains_key("spoolframe-server"),
        "cargo metadata did not return the expected members: {:?}",
        members.keys().collect::<Vec<_>>()
    );
    let found = violations(&members);
    assert!(
        found.is_empty(),
        "license boundary broken:\n{}",
        found.join("\n")
    );
}

#[test]
fn a_permissive_crate_reaching_agpl_through_another_crate_is_caught() {
    let member = |license: &str, deps: &[&str]| Member {
        license: Some(license.to_owned()),
        deps: deps.iter().map(|d| (*d).to_owned()).collect(),
    };
    let members = BTreeMap::from([
        ("sdk".to_owned(), member(PERMISSIVE, &["protocol"])),
        ("protocol".to_owned(), member(PERMISSIVE, &["plex"])),
        ("plex".to_owned(), member(AGPL, &[])),
    ]);
    let found = violations(&members);
    assert!(
        found.iter().any(|v| v.contains("sdk -> protocol -> plex")),
        "{found:?}"
    );
}

#[test]
fn a_crate_with_an_unexpected_license_is_caught() {
    let members = BTreeMap::from([(
        "stray".to_owned(),
        Member {
            license: Some("MIT".to_owned()),
            deps: BTreeSet::new(),
        },
    )]);
    assert_eq!(violations(&members).len(), 1);
}
