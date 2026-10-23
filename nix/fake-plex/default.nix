# A Plex Media Server stand-in for the NixOS federation test
# (nix/federation-e2e.nix): `server` answers the PMS endpoints the server
# calls with fixed data (fake_plex.py), and `media` is the files it lists,
# made here with ffmpeg: two films and two episodes, ten seconds each of
# H.264 and AAC in Matroska, each its own picture and tone so that one
# cannot pass for another. Plex reports them under /data; a server maps
# /data to `media` (plex.pathMappings).
{
  lib,
  runCommand,
  writeShellScriptBin,
  python3,
  ffmpeg,
}: let
  media =
    runCommand "fake-plex-media" {
      nativeBuildInputs = [ffmpeg];
    } ''
      # make <path under $out> <lavfi video> <tone, Hz> <video bitrate>
      make() {
        mkdir -p "$(dirname "$out/$1")"
        ffmpeg -nostdin -hide_banner -loglevel error \
          -f lavfi -i "$2" \
          -f lavfi -i "sine=frequency=$3:duration=10:sample_rate=48000" \
          -map 0:v -map 1:a \
          -c:v libx264 -preset veryfast -pix_fmt yuv420p -b:v "$4" -threads 1 \
          -c:a aac -b:a 96k -ac 1 \
          -map_metadata -1 -fflags +bitexact -flags:v +bitexact -flags:a +bitexact \
          "$out/$1"
      }
      # The film played: noise keeps it from compressing, so it is some
      # 20 MB, more than one read (MAX_READ, 16 MiB).
      make "movies/Federation Test Film (2001)/Federation Test Film (2001).mkv" \
        "testsrc2=duration=10:size=1280x720:rate=25,noise=alls=60:allf=t" 440 16M
      make "movies/Second Test Film (2002)/Second Test Film (2002).mkv" \
        "testsrc=duration=10:size=640x360:rate=25" 550 1M
      make "tv/Federation Test Show/Season 01/Federation Test Show - S01E01.mkv" \
        "smptebars=duration=10:size=640x360:rate=25" 660 1M
      make "tv/Federation Test Show/Season 01/Federation Test Show - S01E02.mkv" \
        "rgbtestsrc=duration=10:size=640x360:rate=25" 770 1M
      # Every item's poster.
      ffmpeg -nostdin -hide_banner -loglevel error \
        -f lavfi -i "testsrc=size=200x300:rate=1" -frames:v 1 -fflags +bitexact \
        "$out/poster.png"
    '';

  server = writeShellScriptBin "fake-plex" ''
    exec ${lib.getExe python3} -I ${./fake_plex.py} --media-dir ${media} "$@"
  '';
in {
  inherit media server;
  # The film the test plays, as the server reads it.
  film = "${media}/movies/Federation Test Film (2001)/Federation Test Film (2001).mkv";
  # The film the test downloads (P8-07).
  secondFilm = "${media}/movies/Second Test Film (2002)/Second Test Film (2002).mkv";
}
