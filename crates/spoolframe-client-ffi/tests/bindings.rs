//! The bindings generate: Swift and Kotlin for this crate and the SDK,
//! protocol and types crates whose records it passes, from the crate's
//! cdylib, as an app's build makes them, with each crate's `uniffi.toml`
//! and no config of the app's. And they compile: the Kotlin with kotlinc
//! against JNA and kotlinx-coroutines, the Swift (on Linux) typechecked
//! with swiftc, so a name UniFFI makes that clashes with Kotlin's, JNA's
//! or Swift's fails here rather than in an app's build. devenv's ci
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

fn generated(dir: &Utf8PathBuf) -> Vec<String> {
    let mut files = Vec::new();
    let mut stack = vec![dir.clone().into_std_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path.file_name().unwrap().to_string_lossy().into_owned());
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
    let mut files = Vec::new();
    let mut stack = vec![dir.clone().into_std_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == ext) {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// `name` from the environment, as devenv's ci profile sets it; `None`
/// (said on stderr) without it, outside CI, where it must be set.
fn from_ci_profile(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) => Some(value),
        Err(_) if std::env::var_os("CI").is_some() => {
            panic!("{name} is unset: run under `devenv shell --profile ci`")
        }
        Err(_) => {
            eprintln!("{name} is unset (devenv's ci profile sets it): not compiled");
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

/// Every crate's Swift as one module, as the Apple apps build it, each
/// crate's C header through its module map.
#[test]
fn swift_bindings_typecheck() {
    let Some(include) = from_ci_profile("SPOOLFRAME_SWIFT_INCLUDE") else {
        return;
    };
    let dir = out_dir("swift-typechecked");
    generate_in(&dir, TargetLanguage::Swift);
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
