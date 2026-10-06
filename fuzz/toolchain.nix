# What scripts/fuzz.sh runs in: a nightly Rust (cargo-fuzz builds with
# sanitizer flags only nightly takes), cargo-fuzz, and a C++ compiler for
# libFuzzer itself. The rest of Spoolframe stays on devenv's stable Rust.
#
# Pinned to the nixpkgs and rust-overlay revisions devenv.lock had when
# this was written, so a campaign builds with the same nightly each time.
# Move them with devenv.lock when the nightly needs to move.
let
  nixpkgs = builtins.getFlake "github:NixOS/nixpkgs/b6c8664de9b6cc07fe5666a29f91884ba81197c4";
  rustOverlay = builtins.getFlake "github:oxalica/rust-overlay/b2ccad3b67335d368eef49ec48b9c8166858e73d";
  pkgs = import nixpkgs {
    system = builtins.currentSystem;
    overlays = [rustOverlay.overlays.default];
  };
in
  pkgs.buildEnv {
    name = "spoolframe-fuzz";
    paths = [
      (pkgs.rust-bin.selectLatestNightlyWith (toolchain: toolchain.minimal))
      pkgs.cargo-fuzz
      pkgs.stdenv.cc
    ];
  }
