# Maintaining Spoolframe

## Development

```bash
devenv shell
cargo run -p spoolframe-server -- --config dev.ucl --state-dir .dev-state
devenv --no-tui tasks run --profile ci ci:static ci:style ci:test
```

`dev.ucl` is your own config and is not tracked.

All inputs are public: Nix inputs from GitHub, pinned in `flake.lock` and
`devenv.lock` (keep both on the same nixpkgs revision), and crates from
crates.io, except noq-proto, which comes from noq's git until a release
has noq#866 (the `[patch.crates-io]` in `Cargo.toml`).

| Task | Runs |
|---|---|
| `ci:static` | The license check (`scripts/check-licenses.sh`), and clippy for the host and for wasm32 |
| `ci:style` | rustfmt, and a check that the committed Tailwind stylesheet is fresh |
| `ci:test` | nextest under llvm-cov. The LAN tests need unprivileged user namespaces (`unshare`). |

## Web UI

`crates/spoolframe-web` is a Dioxus app. It is bundled to wasm and embedded
in the server with the `embed-web` feature. `nix build` does both.

To work on it, run `dx serve` and a debug server without `embed-web`:

```bash
devenv up                                            # dx serve on 127.0.0.1:8080, Tailwind --watch
devenv shell -- cargo run -p spoolframe-server -- --config dev.ucl --state-dir .dev-state
```

Open the admin listener (`admin.listen`) in the browser, not port 8080. The
debug server forwards pages, assets and the hot-reload WebSocket to
`dx serve`, so sign-in, cookies and the origin check are the real ones. If
`dx serve` is not running, each page says so.

To serve a release bundle instead:

```bash
devenv --no-tui tasks run spoolframe:web   # Tailwind + dx bundle
SPOOLFRAME_WEB_DIST=$PWD/target/dx/spoolframe-web/release/web/public \
  cargo run -p spoolframe-server --features embed-web -- --config dev.ucl --state-dir .dev-state
```

A debug server reads the bundle directory as it is: after a change, bundle
again and reload. `crates/spoolframe-web/assets/tailwind.css` is generated
but committed, and `ci:style` fails when it is stale.

## CI

`.forgejo/workflows/ci.yaml` runs the three `ci:*` tasks and `om ci run`
(every flake output, including the NixOS VM test) on pull requests to
`master` and on pushes to it.

## Publishing the crates

`.forgejo/workflows/publish.yaml` runs `scripts/publish-changed.sh` on every
push to `master`, once for each registry: the Coldsteel registry (kellnr,
`https://cargo.coldsteel.casa`) first, then crates.io. Each run publishes,
in dependency order, each crate with `publish = ["crates-io", "coldsteel"]`
whose version is not on that registry yet: spoolframe-types, -protocol,
-policy, -client-sdk, -client-ffi, -test-dir and -hls-bridge. test-dir is
only a dev-dependency of the SDK, but cargo needs it on the registry to
publish the SDK. A crate without exactly that `publish` pair (or `publish =
false`) stops the script.

The apps take the crates from Coldsteel during development, by patching each
spoolframe crate to it (the SDK's README). The crates depend on each other
as crates.io crates, so the Coldsteel run patches the siblings already on
Coldsteel to it for the publish: a new version is not on crates.io yet when
it runs.

To see what each would publish:

```bash
devenv shell -- scripts/publish-changed.sh --registry coldsteel .
devenv shell -- scripts/publish-changed.sh --registry crates-io .
```

- The Coldsteel run reads the index from `.cargo/config.toml`
  (`[registries.coldsteel]`) and the token from the
  `CARGO_REGISTRIES_COLDSTEEL_TOKEN` secret: an API token from the kellnr
  web UI. A failed Coldsteel publish does not stop crates.io's; the job
  goes red after it.
- The crates.io token is the `CARGO_REGISTRY_TOKEN` secret: a crates.io API
  token with the publish-new and publish-update scopes, for `spoolframe-*`.
- crates.io rate-limits new crates (a few, then about one every 10
  minutes). The script waits and retries, and logs each wait.
- A missed or failed publish heals on the next push to `master`.
- `scripts/tests/publish-changed.sh` tests the script (CI's lint job).

## Images

`nix build .#docker`, `.#docker-slim` and `.#docker-nvidia` each make a
script that streams an OCI image. `scripts/push-images.sh` pushes them to
`ghcr.io/spoolframe/spoolframe-server`, each architecture under its own tag,
with `<ver>` and `<ver>-slim` as manifest lists over amd64 and arm64.

On the GitHub mirror, Actions › Images › Run workflow
(`.github/workflows/images.yaml`) does the same on native amd64 and arm64
runners, and skips an architecture whose `<ver>-<arch>` is already
published. Its Nix builds go to the `spoolframe` Cachix cache
(`CACHIX_AUTH_TOKEN`, an organization secret). It needs the repository to
be public (GitHub's arm64 runners). The ghcr package must let the
repository write to it (Package settings › Manage Actions access, role
Write), or the push is denied.

The image reads `/etc/spoolframe/server.ucl`. Media tools run confined as
on any host: Docker 29's default seccomp profile allows Landlock. In the
`-nvidia` image, the toolkit's Vulkan manifest is found in
`/usr/share/vulkan/icd.d`, `/etc/vulkan/icd.d` or a NixOS host's
`/run/opengl-driver`, and its libraries in `/usr/lib64`,
`/usr/lib/x86_64-linux-gnu`, `/usr/lib/aarch64-linux-gnu` or `/usr/lib`
(`LD_LIBRARY_PATH`). The images carry the libX11, libXext and glvnd
`libEGL` that NVIDIA's Vulkan driver loads.
