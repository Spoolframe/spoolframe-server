# The ffmpeg the server runs (PGPU-04): nixpkgs' ffmpeg-headless with
# libplacebo, for the HDR to SDR tone map on a Vulkan GPU, and a GLSL
# compiler at build time, which ffmpeg 9's own Vulkan filters
# (scale_vulkan, overlay_vulkan, …) are compiled with.
#
# ffmpeg-headless already has VA-API, NVENC/NVDEC (the CUDA driver is
# loaded at run time) and Vulkan; it lacks libplacebo (ffmpeg-full's) and,
# from ffmpeg 9, a shader compiler (nixpkgs passes libshaderc only to older
# versions). ffmpeg-full has both, but its closure is 1.4 GiB; this one's
# is 330.5 MiB, against ffmpeg-headless' 305.4 MiB (nixpkgs' 9.0.1,
# 2026-10-09).
#
# And libvpl, Intel's oneVPL dispatcher, for Quick Sync (PGPU-07): the
# `*_qsv` encoders and decoders. It is x86 only, as Intel's GPUs and their
# runtime (vpl-gpu-rt) are, so an arm64 build is unchanged. The dispatcher
# finds the runtime at run time: nixpkgs' looks in /run/opengl-driver/lib
# (NixOS's `hardware.graphics.extraPackages`), and the image names it in
# ONEVPL_SEARCH_PATH (nix/docker.nix). Not withMfx, the old Media SDK:
# nixpkgs refuses both, and libvpl's dispatcher loads the old runtime too.
# The closure went from 328.0 to 329.3 MiB with it (9.0.2, 2026-11-02).
{
  lib,
  stdenv,
  ffmpeg-headless,
  libvpl,
  shaderc,
}:
(ffmpeg-headless.override {
  withPlacebo = true;
  withVpl = stdenv.hostPlatform.isx86_64;
  # Without its examples (sample videos and their sources): 19 MiB of
  # nixpkgs' libvpl's 19.4, in every image.
  libvpl = libvpl.overrideAttrs (old: {
    cmakeFlags = (old.cmakeFlags or []) ++ [(lib.cmakeBool "INSTALL_EXAMPLES" false)];
  });
})
.overrideAttrs (old: {
  # configure probes for `glslc` (shaderc's) on PATH at build time.
  nativeBuildInputs = (old.nativeBuildInputs or []) ++ [shaderc];
  patches =
    (old.patches or [])
    ++ [
      # hevc_vaapi's extradata is not what Mesa's encoder codes to: since
      # 24.3 (c43f8cb0, MR !30927) radeonsi writes its own VPS/SPS/PPS, so
      # an `hvc1` record built from ffmpeg's plays green, and no flag or
      # filter avoids it. mesa#15529. This builds the record from the
      # stream, whatever Mesa writes; live Matroska can't rewrite its
      # header, which the next patch makes true instead.
      ./ffmpeg-hevc-record-from-stream.patch
      # The rest are Jellyfin's (each file says which, and what is cut).
      # AMD: extradata with the values radeonsi writes, on Mesa as it is
      # today.
      ./ffmpeg-jellyfin-0086-amd-hevc-extradata.patch
      # A picture subtitle's canvas sent again only when the graph asks.
      ./ffmpeg-jellyfin-0058-sub2video-canvases.patch
      # No HDR10 metadata on libplacebo's SDR output.
      ./ffmpeg-jellyfin-0085-libplacebo-side-data.patch
      # tonemap_vaapi without HDR10 metadata. Untested: no Intel GPU here.
      ./ffmpeg-jellyfin-0070-tonemap-vaapi-no-metadata.patch
    ];
})
