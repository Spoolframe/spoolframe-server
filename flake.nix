{
  description = "spoolframe — Self-hosted media server: Plex metadata, own playback, Iroh transport.";

  inputs = {
    molds.url = "git+ssh://forgejo@git.coldsteel.casa/Coldsteel/molds.git";
    nixpkgs.follows = "molds/nixpkgs-unstable";
    flake-parts.follows = "molds/flake-parts";
    crane.follows = "molds/crane";
  };

  outputs = inputs @ {flake-parts, ...}:
    flake-parts.lib.mkFlake {inherit inputs;} {
      flake.nixosModules = {
        default = ./nix/nixos-module.nix;
        spoolframe = ./nix/nixos-module.nix;
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
      in {
        packages.default = pkgs.callPackage ./nix/package.nix {
          inherit lib craneLib;
        };
        checks = lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          nixos-module = import ./nix/tests.nix {
            inherit pkgs;
            spoolframePackage = pkgs.callPackage ./nix/package.nix {inherit lib craneLib;};
            spoolframeModule = ./nix/nixos-module.nix;
          };
        };
        formatter = pkgs.alejandra;
      };
    };
}
