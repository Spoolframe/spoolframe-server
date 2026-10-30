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
{
  ffmpeg-headless,
  shaderc,
}:
(ffmpeg-headless.override {withPlacebo = true;}).overrideAttrs (old: {
  # configure probes for `glslc` (shaderc's) on PATH at build time.
  nativeBuildInputs = (old.nativeBuildInputs or []) ++ [shaderc];
  patches =
    (old.patches or [])
    ++ [
      # hevc_vaapi's extradata is not what Mesa's encoder codes to: since
      # 24.3 (c43f8cb0, MR !30927) radeonsi writes its own VPS/SPS/PPS, so
      # an `hvc1` record built from ffmpeg's plays green, and no flag or
      # filter avoids it. mesa#15529. Live Matroska can't rewrite its
      # header: CodecPrivate stays false beside true in-band sets.
      ./ffmpeg-hevc-record-from-stream.patch
    ];
})
