# dx (dioxus-cli) and the wasm-bindgen-cli it must match, built from
# crates.io at the versions in versions.json. Copied from Coldsteel/ports
# (626d99d6, dioxus-cli/) so the repo needs no private flake input; only the
# versions this repo uses are kept. dx, the dioxus crate and wasm-bindgen
# move together: bump all three at once.
{
  lib,
  callPackage,
  fetchCrate,
  rustPlatform,
  pkg-config,
  cacert,
  openssl,
  rustfmt,
  makeWrapper,
  esbuild,
  testers,
}: let
  versions = lib.importJSON ./versions.json;

  buildWasmBindgen = callPackage ./wasm-bindgen.nix {};

  wasm-bindgen-versions = lib.mapAttrs buildWasmBindgen versions.wasm-bindgen-cli;

  buildDioxusCli = version: versionInfo: {wasm-bindgen-version ? null}:
    rustPlatform.buildRustPackage (finalAttrs: {
      pname = "dioxus-cli";
      inherit version;

      src = fetchCrate {
        pname = "dioxus-cli";
        inherit version;
        hash = versionInfo.hash;
      };

      cargoHash = versionInfo.cargoHash;

      buildFeatures = [
        "no-downloads"
        "disable-telemetry"
      ];

      env = {
        OPENSSL_NO_VENDOR = 1;
      };

      nativeBuildInputs = [
        pkg-config
        cacert
        makeWrapper
      ];

      buildInputs = [openssl];

      nativeCheckInputs = [rustfmt];

      checkFlags = [
        "--skip=serve::proxy::test"
        "--skip=test_harnesses::run_harness"
      ];

      postInstall = let
        bindgenVersion =
          if wasm-bindgen-version != null
          then wasm-bindgen-version
          else builtins.head (lib.attrNames versions.wasm-bindgen-cli);
        wasm-bindgen = wasm-bindgen-versions.${bindgenVersion};
      in ''
        wrapProgram $out/bin/dx \
          --suffix PATH : ${lib.makeBinPath [esbuild wasm-bindgen]}
      '';

      meta = {
        description = "CLI for building fullstack web, desktop, and mobile apps with a single codebase";
        homepage = "https://dioxus.dev";
        changelog = "https://github.com/DioxusLabs/dioxus/releases";
        license = with lib.licenses; [mit asl20];
        mainProgram = "dx";
      };
    });
in
  lib.mapAttrs (version: info: lib.makeOverridable (buildDioxusCli version info) {}) versions.dioxus-cli
  // {inherit wasm-bindgen-versions;}
