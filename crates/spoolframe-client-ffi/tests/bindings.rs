//! The bindings generate: Swift and Kotlin for this crate and the SDK,
//! protocol and types crates whose records it passes, from the crate's
//! cdylib, as an app's build makes them. Nothing is compiled in Swift or
//! Kotlin; that is the apps' build.
//!
//! The cdylib is beside this test in `deps/`: cargo builds it with the
//! library, and the nextest archive carries it (devenv.nix). This test's
//! own binary will not do: the linker drops the metadata statics it never
//! reads.

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
    generate(GenerateOptions {
        languages: vec![language],
        source: cdylib(),
        out_dir: out_dir.clone(),
        ..GenerateOptions::default()
    })
    .expect("the bindings generate");
    generated(&out_dir)
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
