{
  description = "spoolframe — Self-hosted media server: Plex metadata, own playback, Iroh transport.";

  # Public inputs only: a fresh clone builds with no SSH key and no private
  # registry. nixpkgs is the same input devenv.yaml names; keep the two locks
  # on one revision so the shell and the package share a store.
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
  };

  outputs = inputs @ {flake-parts, ...}:
    flake-parts.lib.mkFlake {inherit inputs;} {
      flake.nixosModules = {
        default = ./nix/nixos-module.nix;
        spoolframe = ./nix/nixos-module.nix;
        spoolframe-relay = ./nix/relay-module.nix;
      };

      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];

      perSystem = {
        pkgs,
        lib,
        ...
      }: let
        # Every crate comes from crates.io, so crane needs no extra registry.
        craneLib = inputs.crane.mkLib pkgs;
        # Named versions (nix/dioxus-cli): dx, the dioxus crate and
        # wasm-bindgen move together, and dx refuses to bundle on a mismatch.
        dioxusPackages = pkgs.callPackage ./nix/dioxus-cli/package.nix {};
        # nixpkgs' ffmpeg-headless with libplacebo (PGPU-04).
        ffmpeg = pkgs.callPackage ./nix/ffmpeg.nix {};
        spoolframe = pkgs.callPackage ./nix/package.nix {
          inherit lib craneLib ffmpeg;
          # A dirty tree says so; a tree without git has no revision.
          rev = inputs.self.shortRev or inputs.self.dirtyShortRev or null;
          dioxus-cli = dioxusPackages."0.7.10".override {wasm-bindgen-version = "0.2.126";};
          wasm-bindgen-cli = dioxusPackages.wasm-bindgen-versions."0.2.126";
        };
      in {
        packages =
          {default = spoolframe;}
          // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
            inherit ffmpeg;
            # Any GPU, anywhere: nixpkgs' whole Mesa (PGPU-04).
            docker = pkgs.callPackage ./nix/docker.nix {
              inherit spoolframe;
              rev = inputs.self.shortRev or null;
            };
            # AMD and recent Intel: a trimmed Mesa, no LLVM.
            docker-slim = pkgs.callPackage ./nix/docker.nix {
              inherit spoolframe;
              mesa = pkgs.callPackage ./nix/mesa.nix {};
              variant = "slim";
              rev = inputs.self.shortRev or null;
            };
            # NVIDIA and the CPU only: no Mesa.
            docker-nvidia = pkgs.callPackage ./nix/docker.nix {
              inherit spoolframe;
              mesa = null;
              variant = "nvidia";
              rev = inputs.self.shortRev or null;
            };
          };
        checks = lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          nixos-module = import ./nix/tests.nix {
            inherit pkgs;
            spoolframePackage = spoolframe;
            spoolframeModule = ./nix/nixos-module.nix;
          };
          nixos-module-assertions = import ./nix/assertions.nix {
            inherit pkgs;
            spoolframeModule = ./nix/nixos-module.nix;
            relayModule = ./nix/relay-module.nix;
          };
        };
        formatter = pkgs.alejandra;
      };
    };
}
