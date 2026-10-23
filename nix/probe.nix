# spoolframe-probe (PX-11, P8): the server's package's sources and its
# built dependencies, so only the workspace crates the probe uses are
# compiled again. For the NixOS federation test's phone
# (nix/federation-e2e.nix); not shipped.
{
  craneLib,
  spoolframe,
  pkg-config,
  libucl,
  openssl,
}:
craneLib.buildPackage {
  inherit (spoolframe) src version cargoArtifacts;
  pname = "spoolframe-probe";
  strictDeps = true;
  nativeBuildInputs = [pkg-config];
  buildInputs = [libucl openssl];
  cargoExtraArgs = "--locked -p spoolframe-probe";
  doCheck = false;
  meta.mainProgram = "spoolframe-probe";
}
