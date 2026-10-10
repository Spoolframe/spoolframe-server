# The server as an OCI image, for hosts that are not NixOS.
#
# A script that streams the image to stdout (dockerTools.streamLayeredImage):
# nothing lands in the store but the script, and the layers split along store
# paths, so a new server revision pushes only its own layer and ffmpeg's stay
# cached.
#
#   nix build .#docker && ./result | docker load
{
  lib,
  dockerTools,
  cacert,
  tzdata,
  writeTextDir,
  # Mesa: RADV, AMD's Vulkan driver, for libplacebo's tone map (PGPU-04),
  # and radeonsi's VA-API driver beside it.
  mesa,
  spoolframe,
  rev ? null,
}: let
  # What the NixOS module's defaults would set, so the published ports are
  # known. The file layer is not locked: the web UI can change either, and
  # a config mounted over this file replaces it.
  config = writeTextDir "etc/spoolframe/server.ucl" ''
    network { port = 7420; }
    admin { listen = "0.0.0.0:7421"; }
  '';
  uid = "1000";
in
  dockerTools.streamLayeredImage {
    name = "ghcr.io/spoolframe/spoolframe-server";
    tag = spoolframe.version + lib.optionalString (rev != null) "-${rev}";
    contents = [
      spoolframe
      config
      cacert
      tzdata
      # /etc/passwd and /etc/group with root and nobody, for libc's lookups.
      dockerTools.fakeNss
    ];
    # The state directory is owned by the user the server runs as, and a
    # named volume mounted there starts as a copy of it, ownership included.
    fakeRootCommands = ''
      mkdir -p var/lib/spoolframe tmp run/opengl-driver
      chown ${uid}:${uid} var/lib/spoolframe
      chmod 0700 var/lib/spoolframe
      chmod 1777 tmp
      # nixpkgs' ffmpeg dlopens the NVIDIA driver (libcuda, libnvidia-encode)
      # from /run/opengl-driver/lib alone: its glibc reads no /etc/ld.so.cache,
      # and the media sandbox clears LD_LIBRARY_PATH. The NVIDIA Container
      # Toolkit (`--gpus`) puts the host's driver in /usr/lib64 in an image
      # with no /etc/debian_version, as this one.
      ln -s /usr/lib64 run/opengl-driver/lib
      # Mesa's Vulkan drivers and layers where nixpkgs' Vulkan loader looks
      # for them, as on NixOS (PGPU-04): RADV for an AMD GPU passed in with
      # `--device /dev/dri`, and the device-select layer. NVIDIA's comes
      # from the toolkit, in /etc/vulkan or /usr/share/vulkan.
      mkdir -p run/opengl-driver/share
      ln -s ${mesa}/share/vulkan run/opengl-driver/share/vulkan
    '';
    enableFakechroot = false;
    config = {
      Entrypoint = [(lib.getExe spoolframe)];
      User = "${uid}:${uid}";
      Env = [
        "SPOOLFRAME_STATE_DIR=/var/lib/spoolframe"
        "SPOOLFRAME_CONFIG=/etc/spoolframe/server.ucl"
        "SSL_CERT_FILE=${cacert}/etc/ssl/certs/ca-bundle.crt"
        "TZDIR=${tzdata}/share/zoneinfo"
        # The toolkit's default is compute,utility: no NVENC or NVDEC, and
        # without graphics no Vulkan driver, which libplacebo's tone map
        # runs on (PGPU-04).
        "NVIDIA_DRIVER_CAPABILITIES=compute,video,utility,graphics"
        # radeonsi's VA-API driver: /run/opengl-driver/lib is NVIDIA's.
        "LIBVA_DRIVERS_PATH=${mesa}/lib/dri"
      ];
      ExposedPorts = {
        "7420/udp" = {};
        "7421/tcp" = {};
      };
      Volumes."/var/lib/spoolframe" = {};
      Labels = {
        "org.opencontainers.image.description" = spoolframe.meta.description;
        "org.opencontainers.image.licenses" = "AGPL-3.0-or-later";
        "org.opencontainers.image.version" = spoolframe.version;
      };
    };
  }
