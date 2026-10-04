{
  pkgs,
  inputs,
  ...
}: let
  # dx, the dioxus crate and wasm-bindgen move together or not at all: dx
  # refuses to bundle on a mismatch. Named versions, so a ports bump is a
  # change here rather than a surprise (as scribe learnt, FD-451).
  dioxusPackages = pkgs.callPackage "${inputs.ports}/dioxus-cli/package.nix" {};
  # The web UI's fonts (the design's: Outfit, Figtree, JetBrains Mono,
  # Bricolage Grotesque), from nixpkgs so the bundle carries them and a
  # LAN without the Internet renders the pages as designed.
  webFonts = pkgs.google-fonts.override {
    fonts = ["Outfit" "Figtree" "JetBrains Mono" "Bricolage Grotesque"];
  };
  # The upright variable files only, under the names
  # crates/spoolframe-web/src/app/mod.rs' @font-face rules name.
  webFontsCopy = fonts: ''
    t="${fonts}/share/fonts/truetype"
    d=crates/spoolframe-web/assets/fonts
    install -m 644 "$t/Outfit[wght].ttf" "$d/Outfit.ttf"
    install -m 644 "$t/Figtree[wght].ttf" "$d/Figtree.ttf"
    install -m 644 "$t/JetBrainsMono[wght].ttf" "$d/JetBrainsMono.ttf"
    install -m 644 "$t/BricolageGrotesque[opsz,wdth,wght].ttf" "$d/BricolageGrotesque.ttf"
  '';
in {
  # The Rust toolchain, nextest and the ci:* tasks come from
  # molds/devenv/rust. uclicious -> uclicious-libucl-sys links libucl via
  # pkg-config, so the server crates need these at build/link time.
  packages = with pkgs; [
    pkg-config
    libucl
    # webauthn-rs (passkeys, P3-05) verifies signatures with OpenSSL.
    openssl
    # ffprobe, which the server runs on every media file it serves (P1-06).
    ffmpeg-headless
    # Reads bitmap subtitles as text (P2-07); English is what the tests
    # read.
    (tesseract.override {enableLanguages = ["eng"];})
    # The LAN tests (P1-13) run in a private network namespace: unshare
    # makes it, ip brings up loopback with multicast.
    util-linux
    iproute2
    # The web UI (P3-01): `dx bundle`, its wasm-bindgen, and Tailwind.
    dioxusPackages."0.7.10"
    dioxusPackages.wasm-bindgen-versions."0.2.126"
    tailwindcss_4
  ];

  # The OCR tests draw text with ffmpeg's drawtext, which needs a font file;
  # fontconfig finds none in the shell.
  env.SPOOLFRAME_TEST_FONT = "${pkgs.dejavu_fonts}/share/fonts/truetype/DejaVuSans.ttf";

  # spoolframe-web/assets/tailwind.css is generated and committed (dx
  # bundles it as an asset); a stale one renders the page with class names
  # and no rules behind them, which looks like a styling bug, not a stale
  # file (scribe's FD-488).
  tasks = {
    "spoolframe:tailwind-fresh" = {
      description = "The committed Tailwind stylesheet matches a fresh build";
      exec = ''
        set -euo pipefail
        built="$(mktemp -t tailwind-fresh.XXXXXX.css)"
        trap 'rm -f "$built"' EXIT
        tailwindcss -i crates/spoolframe-web/input.css -o "$built" >/dev/null 2>&1
        if ! diff -q "$built" crates/spoolframe-web/assets/tailwind.css >/dev/null; then
          echo "crates/spoolframe-web/assets/tailwind.css is stale. Regenerate it:" >&2
          echo "  tailwindcss -i crates/spoolframe-web/input.css -o crates/spoolframe-web/assets/tailwind.css" >&2
          diff -u crates/spoolframe-web/assets/tailwind.css "$built" | head -40 >&2
          exit 1
        fi
      '';
    };
    # Additive: molds keeps owning the rustfmt check.
    "ci:style".after = ["spoolframe:tailwind-fresh"];

    # The fonts the bundle carries, into the asset folder the app embeds
    # (ignored by git: they come from nixpkgs).
    "spoolframe:fonts" = {
      description = "Copy the web UI's fonts into crates/spoolframe-web/assets/fonts";
      exec = ''
        set -euo pipefail
        ${webFontsCopy webFonts}
      '';
    };

    # The web UI's bundle, for `cargo run --features embed-web` with
    # SPOOLFRAME_WEB_DIST=target/dx/spoolframe-web/release/web/public. A
    # debug server reads that directory as it is, so bundle and reload.
    "spoolframe:web" = {
      description = "Build the web UI bundle (Tailwind, then dx bundle)";
      exec = ''
        set -euo pipefail
        ${webFontsCopy webFonts}
        tailwindcss -i crates/spoolframe-web/input.css -o crates/spoolframe-web/assets/tailwind.css
        rm -rf target/dx/spoolframe-web/release/web/public
        WASM_BINDGEN="$(command -v wasm-bindgen)" \
          dx bundle --package spoolframe-web --platform web --release --profile wasm-release
      '';
    };
  };

  # The web UI's dev loop (`devenv up`): a debug server without
  # `embed-web` takes every page from dx serve on 127.0.0.1:8080 (the port
  # crates/spoolframe-server/src/web/dev_server.rs names) and tunnels its
  # hot-reload socket, so the browser only ever opens the admin listener.
  # Tailwind runs beside it: dx does not build the stylesheet.
  processes = {
    web.exec = ''
      set -euo pipefail
      ${webFontsCopy webFonts}
      export WASM_BINDGEN="$(command -v wasm-bindgen)"
      exec dx serve --package spoolframe-web --platform web --port 8080 \
        --hot-reload true --watch true --open false --interactive false
    '';
    tailwind.exec = "exec tailwindcss -i crates/spoolframe-web/input.css -o crates/spoolframe-web/assets/tailwind.css --watch=always";
  };

  # molds' host clippy never builds the app: it is wasm only. A wasm pass
  # keeps it (and its tests) compiling, as scribe's ci:static does after it
  # once collected errors nobody saw.
  profiles.ci.module.tasks."ci:static".exec = ''
    set -euo pipefail
    cargo clippy -- -D warnings
    cargo clippy --locked --target wasm32-unknown-unknown -p spoolframe-web --all-targets -- -D warnings
  '';

  enterShell = ''
    echo "spoolframe dev environment"
  '';
}
