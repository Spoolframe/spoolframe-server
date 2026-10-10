{
  description = "spoolframe — Self-hosted media server: Plex metadata, own playback, Iroh transport.";

  inputs = {
    molds.url = "git+ssh://forgejo@git.coldsteel.casa/Coldsteel/molds.git";
    nixpkgs.follows = "molds/nixpkgs-unstable";
    flake-parts.follows = "molds/flake-parts";
    crane.follows = "molds/crane";
    # dx and wasm-bindgen-cli for the web UI (P3-01), at named versions.
    ports = {
      url = "git+ssh://forgejo@git.coldsteel.casa/Coldsteel/ports.git";
      inputs.molds.follows = "molds";
    };
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
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      perSystem = {
        pkgs,
        lib,
        system,
        ...
      }: let
        craneLibBase = inputs.crane.mkLib pkgs;
        # Teach crane how to download crates from the coldsteel sparse registry
        # (where first-party deps like uclicious live). Reads are anonymous.
        craneLib = craneLibBase.appendCrateRegistries [
          (craneLibBase.registryFromSparse {
            indexUrl = "https://cargo.coldsteel.casa/api/v1/crates/";
            configSha256 = "sha256-HqSvh8XOBRcuVdl0VYapaNbQfPXNgjiZOG0oe17zziA=";
          })
        ];
        # Named versions, not ports' floating `dioxus-cli`: dx, the dioxus
        # crate and wasm-bindgen move together, and dx refuses to bundle on a
        # mismatch (scribe's FD-451 found the floating one moving under it).
        # nixpkgs' ffmpeg-headless with libplacebo (PGPU-04).
        ffmpeg = pkgs.callPackage ./nix/ffmpeg.nix {};
        spoolframe = pkgs.callPackage ./nix/package.nix {
          inherit lib craneLib ffmpeg;
          # A dirty tree says so; a tree without git has no revision.
          rev = inputs.self.shortRev or inputs.self.dirtyShortRev or null;
          dioxus-cli = inputs.ports.packages.${system}.dioxus-cli_0_7_10;
          wasm-bindgen-cli = inputs.ports.packages.${system}.wasm-bindgen-cli_0_2_126;
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
