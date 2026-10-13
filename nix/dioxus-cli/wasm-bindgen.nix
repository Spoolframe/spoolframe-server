{
  lib,
  rustPlatform,
  fetchCrate,
  nodejs_latest,
  pkg-config,
  openssl,
  stdenv,
  curl,
}: version: versionInfo:
rustPlatform.buildRustPackage {
  pname = "wasm-bindgen-cli";
  inherit version;

  src = fetchCrate {
    pname = "wasm-bindgen-cli";
    inherit version;
    hash = versionInfo.hash;
  };

  cargoHash = versionInfo.cargoHash;

  nativeBuildInputs = [pkg-config];

  buildInputs =
    [openssl]
    ++ lib.optionals stdenv.hostPlatform.isDarwin [curl];

  nativeCheckInputs = [nodejs_latest];

  # tests require the wasm-bindgen monorepo
  doCheck = false;

  meta = {
    homepage = "https://wasm-bindgen.github.io/wasm-bindgen/";
    license = with lib.licenses; [asl20 mit];
    description = "Facilitating high-level interactions between wasm modules and JavaScript";
    mainProgram = "wasm-bindgen";
  };
}
