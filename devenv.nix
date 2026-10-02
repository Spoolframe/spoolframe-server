{pkgs, ...}: {
  # The Rust toolchain, nextest and the ci:* tasks come from
  # molds/devenv/rust. uclicious -> uclicious-libucl-sys links libucl via
  # pkg-config, so the server crates need these at build/link time.
  packages = with pkgs; [
    pkg-config
    libucl
    # ffprobe, which the server runs on every media file it serves (P1-06).
    ffmpeg-headless
  ];

  enterShell = ''
    echo "spoolframe dev environment"
  '';
}
