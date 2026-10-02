{pkgs, ...}: {
  # The Rust toolchain, nextest and the ci:* tasks come from
  # molds/devenv/rust. uclicious -> uclicious-libucl-sys links libucl via
  # pkg-config, so the server crates need these at build/link time.
  packages = with pkgs; [
    pkg-config
    libucl
    # ffprobe, which the server runs on every media file it serves (P1-06).
    ffmpeg-headless
    # Reads bitmap subtitles as text (P2-07); English is what the tests
    # read.
    (tesseract.override {enableLanguages = ["eng"];})
    # The LAN tests (P1-13) run in a private network namespace: unshare
    # makes it, ip brings up loopback with multicast.
    util-linux
    iproute2
  ];

  # The OCR tests draw text with ffmpeg's drawtext, which needs a font file;
  # fontconfig finds none in the shell.
  env.SPOOLFRAME_TEST_FONT = "${pkgs.dejavu_fonts}/share/fonts/truetype/DejaVuSans.ttf";

  enterShell = ''
    echo "spoolframe dev environment"
  '';
}
