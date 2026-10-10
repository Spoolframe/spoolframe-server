# Mesa for the full Docker image (PGPU-04): only what an AMD or Intel GPU
# passed into the container encodes and tone maps with. radeonsi for AMD's
# VA-API, RADV for AMD's Vulkan and ANV for Intel's (libplacebo), and the
# device-select layer, which picks the job's GPU for an unconfined run.
# nixpkgs' mesa builds every driver it can, for every machine: 1 GiB with
# LLVM, most of it nothing a media server's GPU uses.
{
  lib,
  mesa,
}:
(mesa.override {
  galliumDrivers = ["radeonsi"];
  vulkanDrivers = ["amd" "intel"];
  vulkanLayers = ["device-select"];
  # eglPlatforms stay x11 and wayland: Mesa's meson refuses Vulkan's X11
  # lease support without them, and they cost a few small X libraries.
}).overrideAttrs (old: {
  mesonFlags =
    old.mesonFlags
    ++ [
      # radeonsi and RADV compile shaders with ACO; LLVM is only a build
      # tool here (the OpenCL C that ANV's and Rusticl's kernels are
      # compiled from), not linked into a driver.
      (lib.mesonBool "amd-use-llvm" false)
      (lib.mesonBool "draw-use-llvm" false)
    ];
  # spirv2dxil is Dozen's (d3d12), which is not built: an empty output.
  postInstall =
    old.postInstall
    + ''
      mkdir -p $spirv2dxil
    '';
})
