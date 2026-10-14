# spoolframe

Self-hosted media server: Plex metadata, own playback, Iroh transport.

## Development

```bash
devenv shell
cargo run -p spoolframe-server -- --config dev.ucl --state-dir .dev-state
devenv --no-tui tasks run --profile ci ci:static ci:style ci:test
```

Everything comes from public sources: the Nix inputs from GitHub (pinned in
`flake.lock` and `devenv.lock`; keep both on one nixpkgs revision), crates
from crates.io. `ci:static` runs the license check
(`scripts/check-licenses.sh`) and clippy for the host and for wasm32;
`ci:style` checks rustfmt and that the committed Tailwind stylesheet is
fresh; `ci:test` runs nextest under llvm-cov. The LAN tests need
unprivileged user namespaces (`unshare`).

### CI and publishing

`.forgejo/workflows/ci.yaml` runs the three `ci:*` tasks and
`om ci run` (every flake output, NixOS VM test included) on pull requests
to `master` and on pushes to it. `.forgejo/workflows/publish.yaml` runs
`scripts/publish-changed.sh` on every push to `master`: each crate with
`publish = ["coldsteel"]` (types, protocol, policy, client-sdk, test-dir,
hls-bridge) whose version is not on the Coldsteel registry yet is
published, in dependency order. To see what it would do:

```bash
devenv shell -- scripts/publish-changed.sh .
```

A crate published there names its workspace dependencies with
`registry = "coldsteel"` (Cargo.toml), which is why `.cargo/config.toml`
declares the registry; building never reads it.

`dev.ucl` is your own config and is not tracked: write one for your Plex
server, paths and listeners (`cargo run -p spoolframe-server -- config
defaults` prints every key). Without one, or without its `plex`, the server
starts "not set up": clients are told so until Plex is configured, and the
log names the web UI's addresses and the setup code, which opens the setup
wizard there. Every step is a tab of Server › Settings afterwards. The state directory
(`--state-dir`) holds the key, the database, the caches and the settings
changed at run time (`config show` lists every setting and where it comes
from; `config set` and `config unset` change one on the running server).

### Container

`nix build .#docker` makes a script that streams an OCI image of the server,
with ffmpeg and tesseract, tagged `ghcr.io/spoolframe/spoolframe-server`:

```bash
nix build .#docker && ./result | docker load
docker run -d --name spoolframe \
  -v spoolframe:/var/lib/spoolframe -v /srv/media:/srv/media:ro \
  -p 7420:7420/udp -p 7421:7421 \
  ghcr.io/spoolframe/spoolframe-server:0.9.0
docker logs spoolframe   # the web UI's address and the setup code
```

It runs as uid 1000 with its state in `/var/lib/spoolframe`, and reads
`/etc/spoolframe/server.ucl` (UDP 7420, admin on 7421); mount your own
config there to replace it. Media tools run confined as on any host: Docker
29's default seccomp profile allows Landlock. With `--network host`, mDNS
and Plex discovery reach the LAN too. After editing the mounted config,
`docker kill -s HUP spoolframe` has the server read it again: live
settings apply at once, the others at the next `docker restart`.

Pick a tag by the GPU it runs on. They share every layer but the GPU
drivers':

| tag | for | size |
|---|---|---|
| `:0.9.0` (`.#docker`), the default | any GPU, or not sure: AMD (old and new), Intel (old and new), NVIDIA, VMs (virgl), Docker Desktop on Windows (WSL2, d3d12) | about 1.5 GiB |
| `:0.9.0-slim` (`.#docker-slim`) | a recent AMD or Intel GPU (radeonsi, RADV, ANV, iHD) | about 590 MiB |
| `:0.9.0-nvidia` (`.#docker-nvidia`) | NVIDIA | about 450 MiB |
| `:0.9.0-cpu` | no GPU: the `-nvidia` image under a second tag | about 450 MiB |

Pass an AMD or Intel GPU's render node in with `--device
/dev/dri/renderD128` and `--group-add` its group (often `render`). With no
GPU everything is encoded on the CPU, which for a 4K HDR film to 1080p
takes roughly 6 cores per stream at real time.

An NVIDIA GPU needs the NVIDIA Container Toolkit (`--gpus all`, or CDI's
`--device nvidia.com/gpu=all`); every tag runs it. `NVIDIA_DRIVER_CAPABILITIES`
must include `graphics` as well as `compute,video,utility` for libplacebo's
tone map, which runs on NVIDIA's Vulkan driver: the images set
`compute,video,utility,graphics`, so set it again only if you override it.
The toolkit's Vulkan manifest is found in `/usr/share/vulkan/icd.d`,
`/etc/vulkan/icd.d` or a NixOS host's `/run/opengl-driver`, and its
libraries in `/usr/lib64`, `/usr/lib/x86_64-linux-gnu` or `/usr/lib`
(`LD_LIBRARY_PATH`); the images carry the libX11, libXext and glvnd
`libEGL` that NVIDIA's Vulkan driver loads.

### Web UI

`crates/spoolframe-web` is a Dioxus app, bundled to wasm and embedded in the
server by the `embed-web` feature (`nix build` does both).

To work on it, run `dx serve` and a debug server without `embed-web`:

```bash
devenv up                                            # dx serve on 127.0.0.1:8080, Tailwind --watch
devenv shell -- cargo run -p spoolframe-server -- --config dev.ucl --state-dir .dev-state
```

Then open the admin listener (`admin.listen`) in a browser, not port 8080.
The debug server sends every page and asset to `dx serve`, and its
hot-reload WebSocket (`/_dioxus`) too, so the browser talks only to the
server. Sign-in, the cookie and the origin check are then the real ones.
When `dx serve` is not running, each page says so.

To serve a bundle instead (what a release has):

```bash
devenv --no-tui tasks run spoolframe:web   # Tailwind + dx bundle
SPOOLFRAME_WEB_DIST=$PWD/target/dx/spoolframe-web/release/web/public \
  cargo run -p spoolframe-server --features embed-web -- --config dev.ucl --state-dir .dev-state
```

A debug server with `embed-web` reads the bundle directory as it is, so
after a change: bundle, reload.
`crates/spoolframe-web/assets/tailwind.css` is generated but committed;
`ci:style` fails when it is stale.
