# spoolframe-server — Self-hosted media server: Plex metadata, own playback, Iroh transport.
#
# Built with crane for incremental dependency caching.
# Takes craneLib as an argument (constructed in flake.nix from the crane input).
{
  craneLib,
  lib,
  pkg-config,
  libucl,
  openssl,
  makeWrapper,
  # nix/ffmpeg.nix: nixpkgs' ffmpeg-headless with libplacebo (PGPU-04).
  ffmpeg,
  tesseract,
  # The web UI (P3-01).
  dioxus-cli,
  wasm-bindgen-cli,
  binaryen,
  tailwindcss_4,
  # The web UI's fonts, carried in the bundle (nothing is fetched at run
  # time). devenv.nix names the same four.
  google-fonts,
  # rustc links wasm32 with `lld`, which nixpkgs' rustc does not carry.
  lld,
  # The source revision the admin pages show beside the version (P3-18):
  # the flake's short revision, or null where there is none.
  rev ? null,
}: let
  # English by default; the NixOS module points the server at a tesseract
  # with the languages the host asks for.
  ocr = tesseract.override {enableLanguages = ["eng"];};
  webFonts = google-fonts.override {
    fonts = ["Outfit" "Figtree" "JetBrains Mono" "Bricolage Grotesque"];
  };

  # lib.cleanSource (not a .rs/.toml filter): the web UI's assets, CSS and
  # migrations are build inputs too. Less what .gitignore leaves out: in a checkout with no .git (a jj
  # workspace) cleanSource kept every build's target/ and fuzz/'s corpus,
  # some 27 GB copied per build, and a server rebuilt whenever they changed.
  # fuzz/ is its own workspace, not a member.
  src = let
    root = toString ./..;
  in
    lib.cleanSourceWith {
      src = lib.cleanSource ./..;
      filter = path: type: let
        rel = lib.removePrefix "${root}/" (toString path);
        base = baseNameOf path;
      in
        !(
          (type == "directory" && base == "target")
          || lib.hasPrefix ".devenv" base
          || base == ".direnv"
          || lib.elem rel ["fuzz" ".dev-state" "dev.ucl" "dev-server.log" "crates/spoolframe-web/dist"]
        );
    };

  commonArgs = {
    inherit src;
    strictDeps = true;
    pname = "spoolframe-server";
    version = (lib.importTOML ../Cargo.toml).workspace.package.version;
    # uclicious-libucl-sys links libucl via pkg-config; so does
    # openssl-sys OpenSSL, for webauthn-rs (passkeys, P3-05).
    nativeBuildInputs = [pkg-config];
    buildInputs = [libucl openssl];
  };

  # Build dependencies separately for caching. noq-proto comes from git
  # (the root Cargo.toml's [patch.crates-io], PX-62): crane vendors it
  # with builtins.fetchGit at the rev Cargo.lock pins, so it needs no
  # hash here.
  cargoArtifacts = craneLib.buildDepsOnly commonArgs;
in
  craneLib.buildPackage (commonArgs
    // {
      inherit cargoArtifacts;
      # Read by the server at compile time (`option_env!`). Only here, not
      # in commonArgs, so a new revision does not rebuild the dependencies.
      SPOOLFRAME_GIT_REV = lib.optionalString (rev != null) rev;

      # The web UI first, then the server carrying it: Tailwind, `dx bundle`
      # (from the workspace root, where dx resolves members), then the server
      # with `embed-web` reading the bundle from SPOOLFRAME_WEB_DIST.
      WASM_BINDGEN = "${wasm-bindgen-cli}/bin/wasm-bindgen";
      buildPhaseCargoCommand = ''
        t="${webFonts}/share/fonts/truetype"
        d=crates/spoolframe-web/assets/fonts
        install -m 644 "$t/Outfit[wght].ttf" "$d/Outfit.ttf"
        install -m 644 "$t/Figtree[wght].ttf" "$d/Figtree.ttf"
        install -m 644 "$t/JetBrainsMono[wght].ttf" "$d/JetBrainsMono.ttf"
        install -m 644 "$t/BricolageGrotesque[opsz,wdth,wght].ttf" "$d/BricolageGrotesque.ttf"
        tailwindcss -i crates/spoolframe-web/input.css -o crates/spoolframe-web/assets/tailwind.css
        dx bundle --package spoolframe-web --platform web --release --profile wasm-release
        export SPOOLFRAME_WEB_DIST="$PWD/target/dx/spoolframe-web/release/web/public"
        cargoBuildLog=$(mktemp cargoBuildLogXXXX.json)
        cargoWithProfile build -p spoolframe-server --features embed-web --message-format json-render-diagnostics >"$cargoBuildLog"
      '';

      # The server runs ffprobe on every file it serves (P1-06), ffmpeg to
      # encode (P2-02) and tesseract to read bitmap subtitles (P2-07), each
      # confined by Landlock and seccomp (PX-11 finding 34); put them on the
      # binary's PATH rather than relying on the host having them.
      # nixpkgs' ffmpeg-headless has VA-API, NVENC (the CUDA driver is
      # loaded at run time) and Vulkan; nix/ffmpeg.nix adds libplacebo for
      # the HDR tone map on the GPU (PGPU-04). QSV is the one it lacks, and
      # Intel GPUs encode through VA-API.
      nativeBuildInputs =
        commonArgs.nativeBuildInputs
        ++ [
          makeWrapper
          dioxus-cli
          wasm-bindgen-cli
          binaryen
          tailwindcss_4
          lld
        ];
      postInstall = ''
        wrapProgram $out/bin/spoolframe-server \
          --prefix PATH : ${lib.makeBinPath [ffmpeg ocr]}
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
