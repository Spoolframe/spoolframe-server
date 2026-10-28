# The server as an OCI image, for hosts that are not NixOS, in three
# flavours (PGPU-04), `variant`:
#
# - "full" (`.#docker`, tag `<version>`, the default): any GPU, anywhere.
#   nixpkgs' whole Mesa (radeonsi, r600, iris, crocus, nouveau, virgl,
#   d3d12 for Docker Desktop on WSL2, zink, llvmpipe; RADV, ANV, hasvk,
#   dzn, lavapipe, …) and Intel's two VA-API drivers, iHD and i965.
# - "slim" (`.#docker-slim`, tag `<version>-slim`): AMD and Intel GPUs of
#   the last decade: nix/mesa.nix (radeonsi, RADV, ANV; no LLVM) and iHD.
# - "nvidia" (`.#docker-nvidia`, tag `<version>-nvidia`): NVIDIA and the
#   CPU, no Mesa; the NVIDIA Container Toolkit brings the host's driver in.
#   Released under `<version>-cpu` too, for machines with no GPU.
#
# All three run NVIDIA's GPUs: the toolkit's driver, mounted in.
#
# A script that streams the image to stdout (dockerTools.streamLayeredImage):
# nothing lands in the store but the script, and the layers split along store
# paths, so a new server revision pushes only its own layer and ffmpeg's stay
# cached, and the two flavours share every layer of the server's closure
# (`layeringPipeline`).
#
#   nix build .#docker && ./result | docker load
{
  lib,
  stdenv,
  buildPackages,
  dockerTools,
  cacert,
  tzdata,
  writeTextDir,
  # nixpkgs' Mesa (full) or nix/mesa.nix (slim); null for NVIDIA's.
  mesa,
  # iHD, Intel's VA-API driver (Broadwell on).
  intel-media-driver,
  # i965, Intel's older one (up to Coffee Lake), in the full image.
  intel-vaapi-driver,
  # NVIDIA's Vulkan and GLX library (libGLX_nvidia) links libX11 and
  # libXext, and its Vulkan driver dlopens glvnd's libEGL.so.1, failing
  # vkCreateInstance without it (found on fulcrum with strace: the loader
  # then reports "Could not get 'vkCreateInstance'", VK_ERROR_INCOMPATIBLE_
  # DRIVER). The toolkit mounts none of them: a distribution's image has
  # them, this one carries them.
  libx11,
  libxext,
  libglvnd,
  spoolframe,
  variant ? "full",
  rev ? null,
}: let
  mesaDrivers = variant != "nvidia";
  # Intel's VA-API drivers are x86-only (nixpkgs marks them so): an arm64
  # image has Mesa's alone, which covers an AMD GPU in an arm64 host.
  intel = lib.optionals stdenv.hostPlatform.isx86_64;
  vaapiDrivers =
    {
      full = [mesa] ++ intel [intel-media-driver intel-vaapi-driver];
      slim = [mesa] ++ intel [intel-media-driver];
      nvidia = [];
    }
    .${
      variant
    };
  # What the NixOS module's defaults would set, so the published ports are
  # known. The file layer is not locked: the web UI can change either, and
  # a config mounted over this file replaces it.
  config = writeTextDir "etc/spoolframe/server.ucl" ''
    network { port = 7420; }
    admin { listen = "0.0.0.0:7421"; }
  '';
  uid = "1000";
  # The layers: the server's closure (it, ffmpeg, tesseract, glibc, …)
  # first, by popularity, in up to 96 layers (it has about 140 paths, so the
  # least popular share one); then the GPU drivers (Mesa, the VA-API drivers,
  # libXext) and what only they need, in up to 20; then the rest (the config,
  # certificates, time zones, passwd), one a layer. The server's closure is
  # the same graph in all three images, and so are the last ones, so their
  # layers are the same, digests and all: the images differ in the drivers'
  # layers only. Left to itself, streamLayeredImage ranks all of an image's
  # paths together and merges the least popular past maxLayers, which put
  # different paths together in different images. Docker takes up to 125
  # layers.
  layeringPipeline = [
    ["subcomponent_out" ["${spoolframe}"]]
    ["over" "main" ["pipe" [["popularity_contest"] ["limit_layers" 96]]]]
    [
      "over"
      "rest"
      [
        "pipe"
        [
          ["subcomponent_out" (map toString (vaapiDrivers ++ [libxext]))]
          ["over" "main" ["pipe" [["popularity_contest"] ["limit_layers" 20]]]]
          ["over" "rest" ["popularity_contest"]]
        ]
      ]
    ]
    ["flatten"]
  ];
  # The tool that splits the layers (flatten-references-graph) needs python's
  # igraph, whose tests pull in plotly, whose tests start Chromium through
  # Kaleido. Chromium does not start under qemu, so an arm64 image built on
  # an emulated builder failed there, and cache.nixos.org has no arm64
  # plotly for this nixpkgs. igraph's tests say nothing about our layers: on
  # aarch64 they are skipped, and plotly drops out of the build. x86's
  # image is untouched, the cached tool and all.
  layerTools =
    if stdenv.buildPlatform.isAarch64
    then
      dockerTools.override {
        buildPackages =
          buildPackages
          // {
            dockerMakeLayers = buildPackages.dockerMakeLayers.override {
              flatten-references-graph = buildPackages.flatten-references-graph.override {
                python3Packages = buildPackages.python3Packages.overrideScope (_: prev: {
                  igraph = prev.igraph.overridePythonAttrs {doCheck = false;};
                });
              };
            };
          };
      }
    else dockerTools;
in
  layerTools.streamLayeredImage {
    inherit layeringPipeline;
    name = "ghcr.io/spoolframe/spoolframe-server";
    tag =
      spoolframe.version
      + lib.optionalString (rev != null) "-${rev}"
      + lib.optionalString (variant != "full") "-${variant}";
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
    fakeRootCommands =
      ''
        mkdir -p var/lib/spoolframe tmp run/opengl-driver
        chown ${uid}:${uid} var/lib/spoolframe
        chmod 0700 var/lib/spoolframe
        chmod 1777 tmp
        # nixpkgs' ffmpeg dlopens the NVIDIA driver (libcuda, libnvcuvid,
        # libnvidia-encode) through its RUNPATH, /run/opengl-driver/lib
        # first: its glibc reads no /etc/ld.so.cache, so the toolkit's
        # ldcache hook does nothing for it. The NVIDIA Container Toolkit's
        # legacy runtime (`--gpus`) puts the host's driver in /usr/lib64 in
        # an image with no /etc/debian_version, as this one; under CDI it
        # mounts the libraries at the host's own paths (LD_LIBRARY_PATH
        # below covers those).
        ln -s /usr/lib64 run/opengl-driver/lib
        # nixpkgs' Vulkan loader reads manifests from
        # /run/opengl-driver/share/vulkan (its SYSCONFDIR), /etc/xdg/vulkan,
        # /usr/local/share/vulkan and /usr/share/vulkan, not /etc/vulkan.
        # The image's go in /usr/share/vulkan: a CDI spec from a NixOS host
        # mounts the host's /run/opengl-driver over the image's (fulcrum's
        # does), and the media sandbox reads /usr. NVIDIA's manifest: the
        # toolkit mounts it into /usr/share/vulkan/icd.d on most
        # distributions, which is read as it is; /etc/vulkan/icd.d, where
        # some put it, is linked from here, leading nowhere without
        # --gpus (a NixOS host's is also in its /run/opengl-driver).
        mkdir -p usr/share/vulkan/icd.d
        ln -s /etc/vulkan/icd.d/nvidia_icd.json usr/share/vulkan/icd.d/nvidia_etc_icd.json
      ''
      # Mesa's Vulkan drivers and layers beside it (PGPU-04), for a GPU
      # passed in with `--device /dev/dri`.
      + lib.optionalString mesaDrivers ''
        for icd in ${mesa}/share/vulkan/icd.d/*.json; do
          ln -s "$icd" usr/share/vulkan/icd.d/
        done
        ln -s ${mesa}/share/vulkan/implicit_layer.d usr/share/vulkan/implicit_layer.d
      '';
    enableFakechroot = false;
    config = {
      Entrypoint = [(lib.getExe spoolframe)];
      User = "${uid}:${uid}";
      Env =
        [
          "SPOOLFRAME_STATE_DIR=/var/lib/spoolframe"
          "SPOOLFRAME_CONFIG=/etc/spoolframe/server.ucl"
          "SSL_CERT_FILE=${cacert}/etc/ssl/certs/ca-bundle.crt"
          "TZDIR=${tzdata}/share/zoneinfo"
          # The toolkit's default is compute,utility: no NVENC or NVDEC, and
          # without graphics no Vulkan driver, which libplacebo's tone map
          # runs on (PGPU-04).
          "NVIDIA_DRIVER_CAPABILITIES=compute,video,utility,graphics"
          # The toolkit's Vulkan manifest names libGLX_nvidia.so.0 bare, and
          # the Vulkan loader's RUNPATH (unlike ffmpeg's) has no driver
          # directory, nor does nixpkgs' glibc look in any system one: where
          # the toolkit puts NVIDIA's libraries, legacy (/usr/lib64, through
          # the link above) or CDI at the host's paths (Debian and Ubuntu
          # /usr/lib/x86_64-linux-gnu or aarch64-linux-gnu, Fedora /usr/lib64,
          # Arch /usr/lib). The
          # image has nothing else in any of them, so nothing of its own is
          # shadowed: Mesa's drivers are found by absolute paths (the
          # manifests, LIBVA_DRIVERS_PATH) and their RUNPATHs. Kept in the
          # media sandbox's runs, which may read /usr and, on a GPU,
          # /run/opengl-driver.
          # libX11, libXext and glvnd last, for libGLX_nvidia (above):
          # after the toolkit's directories, so a host's own come first.
          "LD_LIBRARY_PATH=${lib.concatStringsSep ":" [
            "/run/opengl-driver/lib"
            "/usr/lib/x86_64-linux-gnu"
            "/usr/lib/aarch64-linux-gnu"
            "/usr/lib64"
            "/usr/lib"
            "${lib.getLib libx11}/lib"
            "${lib.getLib libxext}/lib"
            "${lib.getLib libglvnd}/lib"
          ]}"
        ]
        # The VA-API drivers: /run/opengl-driver/lib is NVIDIA's.
        ++ lib.optional (vaapiDrivers != [])
        "LIBVA_DRIVERS_PATH=${lib.concatMapStringsSep ":" (d: "${d}/lib/dri") vaapiDrivers}";
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
