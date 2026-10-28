<p align="center">
  <img src="docs/spoolframe.svg" alt="Spoolframe" width="128" height="128">
</p>

<h1 align="center">Spoolframe</h1>

<p align="center">
  A self-hosted media server. It reads your library from Plex, plays and
  converts the files itself, and reaches devices over Iroh.
</p>

## Run it in Docker

```bash
nix build .#docker && ./result | docker load
docker run -d --name spoolframe \
  -v spoolframe:/var/lib/spoolframe -v /srv/media:/srv/media:ro \
  -p 7420:7420/udp -p 7421:7421 \
  ghcr.io/spoolframe/spoolframe-server:0.16.2
docker logs spoolframe   # the web UI's address and the setup code
```

Open the address from the log and enter the setup code to start the setup
wizard.

- The server runs as uid 1000 and keeps its state in `/var/lib/spoolframe`.
- Clients connect on UDP 7420. The web UI and admin API are on 7421.
- The config is `/etc/spoolframe/server.ucl`. Mount your own file there to
  replace it, then `docker kill -s HUP spoolframe` to reload it. Live
  settings apply at once; the rest apply at the next restart.
- With `--network host`, mDNS and Plex discovery also reach the LAN.

### Pick a tag for your GPU

The tags share every layer except the GPU drivers. `:0.16.2` and
`:0.16.2-slim` are for x86-64 and arm64 (Docker picks the host's);
`-nvidia` and `-cpu` are x86-64 only.

| Tag | For | Size |
|---|---|---|
| `:0.16.2` (`.#docker`) | Any GPU, or if you are not sure: AMD, Intel, NVIDIA, VMs (virgl), Docker Desktop on Windows (WSL2) | ~1.5 GiB |
| `:0.16.2-slim` (`.#docker-slim`) | A recent AMD or Intel GPU (radeonsi, RADV, ANV, iHD) | ~640 MiB |
| `:0.16.2-nvidia` (`.#docker-nvidia`) | NVIDIA | ~500 MiB |
| `:0.16.2-cpu` | No GPU. The same image as `-nvidia`. | ~500 MiB |

**AMD or Intel:** pass the render node with `--device /dev/dri/renderD128`
and add its group with `--group-add` (often `render`).

**NVIDIA:** install the NVIDIA Container Toolkit and pass `--gpus all` (or
`--device nvidia.com/gpu=all` with CDI). Every tag works.
`NVIDIA_DRIVER_CAPABILITIES` must include `graphics`, because the HDR tone
map runs on NVIDIA's Vulkan driver. The images already set
`compute,video,utility,graphics`.

**No GPU:** the CPU encodes everything. A 4K HDR film to 1080p at real time
takes about 6 cores per stream.

## Configuration

The server reads one UCL file. To print every key with its default:

```bash
docker run --rm ghcr.io/spoolframe/spoolframe-server:0.16.2 config defaults
```

Without a config, or without a `plex` section, the server starts "not set
up". Its log shows the web UI's addresses and a setup code that opens the
setup wizard. After setup, every step is a tab under Server › Settings.

The state directory (`--state-dir`) holds the server's key, its database,
its caches and the settings changed at run time. `config show` lists each
setting and where it comes from. `config set` and `config unset` change a
setting on the running server.

## AI agents (MCP)

The admin listener serves MCP at `/mcp`. It is off until you turn on
`mcp.enabled` (Settings › MCP).

To connect Claude, add a custom connector with the web UI's address plus
`/mcp`, for example `https://media.example.org/mcp`. Claude registers
itself, the user signs in and approves the scopes, and Claude gets a token.

- Over the Internet this needs `admin.public_url` with TLS. Without it, only
  a client that reaches the server at its own address (an IP or
  `localhost`) can connect.
- Only apps whose redirect URI is in `mcp.allowed_redirects` can sign in.
  The default list has Claude Code, Claude (claude.ai and desktop) and
  Codex. Add other agents in Settings › MCP › Allowed redirects.
- Users see and disconnect their apps in My profile › Connected apps.
  Admins see what the apps did in Server › Audit.

## Building apps

The client SDK is on crates.io as
[`spoolframe-client-sdk`](https://crates.io/crates/spoolframe-client-sdk).
Read its [README](crates/spoolframe-client-sdk/README.md) first: it
explains one `[patch]` line an app should add.

## License

The server crates are AGPL-3.0-or-later. The protocol, types, client SDK and
their helper crates are MIT OR Apache-2.0, so any client can link them.
spoolframe-policy is GPL-3.0-or-later. Each crate's `Cargo.toml` states its
license. The texts: [LICENSE](LICENSE) (AGPL-3.0),
[LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE), and
[crates/spoolframe-policy/LICENSE](crates/spoolframe-policy/LICENSE) (GPL-3.0).

Building or changing the server: [MAINTAINER.md](MAINTAINER.md).
