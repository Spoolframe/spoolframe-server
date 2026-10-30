//! The bindings generate: Swift and Kotlin for this crate and the SDK,
//! protocol and types crates whose records it passes, from the crate's
//! cdylib, as an app's build makes them, with each crate's `uniffi.toml`
//! and no config of the app's. And they compile: the Kotlin with kotlinc
//! against JNA and kotlinx-coroutines, the Swift (on Linux) typechecked
//! with swiftc, so a name UniFFI makes that clashes with Kotlin's, JNA's
//! or Swift's fails here rather than in an app's build. devenv's ci-test
//! profile gives the compilers and `SPOOLFRAME_KOTLIN_CLASSPATH` and
//! `SPOOLFRAME_SWIFT_INCLUDE`; without them the compiling tests say so
//! and pass, except under CI, where they fail.
//!
//! The cdylib is beside this test in `deps/`: cargo builds it with the
//! library, and the nextest archive carries it (devenv.nix). This test's
//! own binary will not do: the linker drops the metadata statics it never
//! reads.

use std::path::PathBuf;
use std::process::Command;

use camino::Utf8PathBuf;
use uniffi_bindgen::bindings::{GenerateOptions, TargetLanguage, generate};

/// A directory of the test's own under the target directory.
fn out_dir(name: &str) -> Utf8PathBuf {
    let dir = Utf8PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn cdylib() -> Utf8PathBuf {
    use std::env::consts::{DLL_PREFIX, DLL_SUFFIX};
    let exe = Utf8PathBuf::from_path_buf(std::env::current_exe().unwrap()).unwrap();
    let path = exe
        .parent()
        .expect("the test is in deps/")
        .join(format!("{DLL_PREFIX}spoolframe_client_ffi{DLL_SUFFIX}"));
    assert!(path.exists(), "no cdylib at {path}");
    path
}

/// Every file under `dir`, in path order.
fn generated(dir: &Utf8PathBuf) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.clone().into_std_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn generate_into(name: &str, language: TargetLanguage) -> Vec<String> {
    let out_dir = out_dir(name);
    generate_in(&out_dir, language);
    generated(&out_dir)
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

fn generate_in(out_dir: &Utf8PathBuf, language: TargetLanguage) {
    generate(GenerateOptions {
        languages: vec![language],
        source: cdylib(),
        out_dir: out_dir.clone(),
        // Each crate's uniffi.toml, by its root: the four are this
        // workspace's, so no registry is read (a CI shard has none).
        metadata_no_deps: true,
        ..GenerateOptions::default()
    })
    .expect("the bindings generate");
}

/// The files under `dir` ending in `ext`, in path order.
fn sources(dir: &Utf8PathBuf, ext: &str) -> Vec<PathBuf> {
    generated(dir)
        .into_iter()
        .filter(|path| path.extension().is_some_and(|e| e == ext))
        .collect()
}

/// `name` from the environment, as devenv's ci-test profile sets it; `None`
/// (said on stderr) without it, outside CI, where it must be set.
fn from_ci_profile(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) => Some(value),
        Err(_) if std::env::var_os("CI").is_some() => {
            panic!("{name} is unset: run under `devenv shell --profile ci-test`")
        }
        Err(_) => {
            eprintln!("{name} is unset (devenv's ci-test profile sets it): not compiled");
            None
        }
    }
}

/// `command`'s run, which must succeed; its output on failure.
fn succeeds(mut command: Command) {
    let output = command
        .output()
        .unwrap_or_else(|e| panic!("{command:?} doesn't run: {e}"));
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn swift_bindings_are_generated_for_every_crate() {
    let files = generate_into("swift", TargetLanguage::Swift);
    for module in [
        "spoolframe_client_ffi",
        "spoolframe_client_sdk",
        "spoolframe_protocol",
        "spoolframe_types",
    ] {
        for file in [
            format!("{module}.swift"),
            format!("{module}FFI.h"),
            format!("{module}FFI.modulemap"),
        ] {
            assert!(files.contains(&file), "no {file} in {files:?}");
        }
    }
}

#[test]
fn kotlin_bindings_are_generated_for_every_crate() {
    let files = generate_into("kotlin", TargetLanguage::Kotlin);
    for module in [
        "spoolframe_client_ffi",
        "spoolframe_client_sdk",
        "spoolframe_protocol",
        "spoolframe_types",
    ] {
        let file = format!("{module}.kt");
        assert!(files.contains(&file), "no {file} in {files:?}");
    }
}

/// Every crate's Kotlin together, as an app compiles it.
#[test]
fn kotlin_bindings_compile() {
    let Some(classpath) = from_ci_profile("SPOOLFRAME_KOTLIN_CLASSPATH") else {
        return;
    };
    let dir = out_dir("kotlin-compiled");
    generate_in(&dir, TargetLanguage::Kotlin);
    let mut kotlinc = Command::new("kotlinc");
    kotlinc
        .arg("-cp")
        .arg(classpath)
        .arg("-d")
        .arg(dir.join("classes"))
        .args(sources(&dir, "kt"));
    succeeds(kotlinc);
}

/// What the Apple app adds to `Item`: SwiftUI lists need it Identifiable,
/// and Swift refuses a property beside a method of the same name.
const ITEM_IS_IDENTIFIABLE: &str = "
import Foundation

extension Item: Identifiable {
    public var id: MediaId { itemId() }
    public var title: String { itemTitle() }
    public var artwork: [Artwork] { itemArtwork() }
    public var playback: PlaybackState? { itemPlayback() }
    public var duration: TimeInterval? { itemDuration() }
}
";

/// Every crate's Swift as one module, as the Apple apps build it, each
/// crate's C header through its module map.
#[test]
fn swift_bindings_typecheck() {
    // The ci-test profile gives swiftc its Foundation on Linux only; the
    // Apple apps' own builds compile it on macOS.
    if !cfg!(target_os = "linux") {
        eprintln!("the Swift is typechecked on Linux only: not typechecked");
        return;
    }
    let Some(include) = from_ci_profile("SPOOLFRAME_SWIFT_INCLUDE") else {
        return;
    };
    let dir = out_dir("swift-typechecked");
    generate_in(&dir, TargetLanguage::Swift);
    std::fs::write(dir.join("Identifiable.swift"), ITEM_IS_IDENTIFIABLE).unwrap();
    let mut swiftc = Command::new("swiftc");
    swiftc.args([
        "-typecheck",
        "-parse-as-library",
        "-module-name",
        "SpoolframeCore",
    ]);
    for path in include.split(':') {
        swiftc.arg("-I").arg(path);
    }
    for map in sources(&dir, "modulemap") {
        swiftc
            .arg("-Xcc")
            .arg(format!("-fmodule-map-file={}", map.display()));
    }
    swiftc.args(sources(&dir, "swift"));
    succeeds(swiftc);
}

/// The calls the apps asked for, by their Swift and Kotlin names: the
/// enums' wire names and lists, a sort's and filter's settings string,
/// an item's and a plan's accessors, the derived flags and the nested
/// errors' text. Names only; the Rust tests check what they answer.
const APP_CALLS: &[&str] = &[
    // Wire names.
    "wireName",
    "sortKeyFromWireName",
    "allSortKeys",
    "openFiles",
    "allAudioCodecs",
    "subtitleModeFromWireName",
    "allBitmapPreferences",
    "settingsString",
    "itemSortFromSettings",
    "itemFilterFromSettings",
    // Item and plan accessors.
    "itemId",
    "itemTitle",
    "itemArtwork",
    "itemDuration",
    "itemPlayback",
    "kind",
    "ladder",
    "sourceCodec",
    "convertedVideo",
    "audioAction",
    "subtitleAction",
    "burnsInSubtitle",
    "encodedHeight",
    // Derived flags.
    "isAdmin",
    "hasPin",
    "pinDigits",
    "isPortable",
    "isFederation",
    "isLocal",
    "isUser",
    // This device's own addresses, read here (Apple and Linux).
    "atHomeHere",
    "defaultQualityHere",
    // The catalog cache.
    "catalogCacheCapacity",
    "watchCatalog",
    "forgetCatalog",
    // One kind of search results, narrowed to a server.
    "onlyFrom",
    "seeAllCount",
    // Nested errors.
    "message",
    "pairingRefusedMessage",
    // Profiles, servers and the connection banner (Android).
    "openProfile",
    "forgetProfile",
    "protocolVersion",
    "watchServer",
    "server",
    "serversOnNetwork",
    "serverEntry",
    "details",
    "watchOwnProfile",
    "poster",
    "background",
    "thumb",
    "isForced",
    "mediaOrigin",
    "setSubtitleSearch",
];

#[test]
fn the_apps_calls_are_in_both_languages() {
    let read = |name: &str, language: TargetLanguage| {
        let dir = out_dir(name);
        generate_in(&dir, language);
        let ext = if name.starts_with("kotlin") {
            "kt"
        } else {
            "swift"
        };
        sources(&dir, ext)
            .iter()
            .map(|p| std::fs::read_to_string(p).unwrap())
            .collect::<String>()
    };
    let kotlin = read("kotlin-calls", TargetLanguage::Kotlin);
    let swift = read("swift-calls", TargetLanguage::Swift);
    let missing: Vec<String> = APP_CALLS
        .iter()
        .flat_map(|call| {
            [
                (!kotlin.contains(&format!("fun `{call}`("))).then(|| format!("Kotlin {call}")),
                (!swift.contains(&format!("func {call}("))).then(|| format!("Swift {call}")),
            ]
        })
        .flatten()
        .collect();
    assert!(missing.is_empty(), "missing: {missing:?}");
}

/// A session compares and hashes as its server and user in both
/// languages: a profile opened again equals the one held.
#[test]
fn a_session_is_equatable_and_hashable() {
    let read = |name: &str, language: TargetLanguage, ext: &str| {
        let dir = out_dir(name);
        generate_in(&dir, language);
        sources(&dir, ext)
            .iter()
            .map(|p| std::fs::read_to_string(p).unwrap())
            .collect::<String>()
    };
    let kotlin = read("kotlin-session-eq", TargetLanguage::Kotlin, "kt");
    let swift = read("swift-session-eq", TargetLanguage::Swift, "swift");
    let session = kotlin
        .split("open class SpoolSession:")
        .nth(1)
        .and_then(|rest| rest.split("\nopen class ").next())
        .unwrap_or_default();
    assert!(
        session.contains("override fun equals(") && session.contains("override fun hashCode("),
        "Kotlin SpoolSession lacks equals and hashCode"
    );
    assert!(
        swift.contains(
            "class SpoolSession: SpoolSessionProtocol, @unchecked Sendable, Equatable, Hashable"
        ),
        "Swift SpoolSession is not Equatable and Hashable"
    );
}

/// Spans and times reach both languages as their own types, not as
/// integers in some unit: a marker's start is a `Duration`
/// (`TimeInterval`), when an item was viewed an `Instant` (`Date`).
#[test]
fn times_are_instants_and_spans_durations() {
    let read = |name: &str, language: TargetLanguage, ext: &str| {
        let dir = out_dir(name);
        generate_in(&dir, language);
        sources(&dir, ext)
            .iter()
            .map(|p| std::fs::read_to_string(p).unwrap())
            .collect::<String>()
    };
    let kotlin = read("kotlin-times", TargetLanguage::Kotlin, "kt");
    let swift = read("swift-times", TargetLanguage::Swift, "swift");
    let expected = [
        (&kotlin, "var `start`: java.time.Duration"),
        (&kotlin, "var `lastViewedAt`: java.time.Instant?"),
        (
            &kotlin,
            "fun `ago`(`at`: java.time.Instant, `now`: java.time.Instant)",
        ),
        (&swift, "public var start: TimeInterval"),
        (&swift, "public var lastViewedAt: Date?"),
        (&swift, "public func ago(at: Date, now: Date)"),
    ];
    let missing: Vec<&str> = expected
        .iter()
        .filter(|(source, line)| !source.contains(line))
        .map(|(_, line)| *line)
        .collect();
    assert!(missing.is_empty(), "missing: {missing:?}");
}

/// The records an app builds take their defaults in both languages:
/// false for a playlist's kinds and play settings, nil and empty for the
/// optional parts of a selection, filter, invite and device.
#[test]
fn the_records_apps_build_have_defaults() {
    let read = |name: &str, language: TargetLanguage, ext: &str| {
        let dir = out_dir(name);
        generate_in(&dir, language);
        sources(&dir, ext)
            .iter()
            .map(|p| std::fs::read_to_string(p).unwrap())
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let kotlin = read("kotlin-defaults", TargetLanguage::Kotlin, "kt");
    let swift = read("swift-defaults", TargetLanguage::Swift, "swift");
    let expected = [
        (&kotlin, "var `movies`: kotlin.Boolean = false"),
        (&kotlin, "var `skipWatched`: kotlin.Boolean = false"),
        (&kotlin, "var `subtitle`: kotlin.UInt? = null"),
        (&kotlin, "var `yearFrom`: kotlin.UShort? = null"),
        (&kotlin, "var `genres`: List<kotlin.String> = listOf()"),
        (&kotlin, "var `expiresIn`: java.time.Duration? = null"),
        (&kotlin, "var `downloadsDir`: kotlin.String? = null"),
        (&swift, "movies: Bool = false"),
        (&swift, "skipWatched: Bool = false"),
        (&swift, "subtitle: UInt32? = nil"),
        (&swift, "yearFrom: UInt16? = nil"),
        (&swift, "genres: [String] = []"),
        (&swift, "expiresIn: TimeInterval? = nil"),
        (&swift, "downloadsDir: String? = nil"),
    ];
    let missing: Vec<&str> = expected
        .iter()
        .filter(|(source, line)| !source.contains(line))
        .map(|(_, line)| *line)
        .collect();
    assert!(missing.is_empty(), "missing: {missing:?}");
}

/// Swift alone takes the names the Apple app asked for; Kotlin keeps the
/// Rust ones.
#[test]
fn swift_alone_renames_quality_and_plan() {
    let read = |name: &str, language: TargetLanguage, ext: &str| {
        let dir = out_dir(name);
        generate_in(&dir, language);
        sources(&dir, ext)
            .iter()
            .map(|p| std::fs::read_to_string(p).unwrap())
            .collect::<String>()
    };
    let swift = read("swift-renames", TargetLanguage::Swift, "swift");
    let kotlin = read("kotlin-renames", TargetLanguage::Kotlin, "kt");
    assert_eq!(
        [
            swift.contains("public enum PlanQuality"),
            swift.contains("public struct PlaybackPlanKind"),
            kotlin.contains("sealed class Quality"),
            kotlin.contains("data class Plan ("),
        ],
        [true; 4]
    );
}
