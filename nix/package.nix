# spoolframe-server — Self-hosted media server: Plex metadata, own playback, Iroh transport.
#
# Built with crane for incremental dependency caching.
# Takes craneLib as an argument (constructed in flake.nix from the crane input).
{
  craneLib,
  lib,
  pkg-config,
  libucl,
  makeWrapper,
  ffmpeg-headless,
}: let
  # lib.cleanSource (not a .rs/.toml filter) so `.cargo/config.toml` — which
  # declares the coldsteel registry for vendoring — is kept in the build source.
  src = lib.cleanSource ./..;

  commonArgs = {
    inherit src;
    strictDeps = true;
    pname = "spoolframe-server";
    version = (lib.importTOML ../Cargo.toml).workspace.package.version;
    # uclicious-libucl-sys links libucl via pkg-config.
    nativeBuildInputs = [pkg-config];
    buildInputs = [libucl];
  };

  # Build dependencies separately for caching.
  cargoArtifacts = craneLib.buildDepsOnly commonArgs;
in
  craneLib.buildPackage (commonArgs
    // {
      inherit cargoArtifacts;

      cargoExtraArgs = "-p spoolframe-server";

      # The server runs ffprobe on every file it serves (P1-06); put it on
      # the binary's PATH rather than relying on the host having one.
      nativeBuildInputs = commonArgs.nativeBuildInputs ++ [makeWrapper];
      postInstall = ''
        wrapProgram $out/bin/spoolframe-server \
          --prefix PATH : ${lib.makeBinPath [ffmpeg-headless]}
      '';
      # Tests run in devenv/CI; the license-boundary test shells out to
      # `cargo metadata`, which the package build has no reason to do.
      doCheck = false;

      meta = {
        description = "Self-hosted media server: Plex metadata, own playback, Iroh transport.";
        license = lib.licenses.agpl3Plus;
        mainProgram = "spoolframe-server";
      };
    })
