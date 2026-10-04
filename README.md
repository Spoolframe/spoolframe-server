# spoolframe

Self-hosted media server: Plex metadata, own playback, Iroh transport.

## Development

```bash
devenv shell
cargo run -p spoolframe-server -- --config dev.ucl --state-dir .dev-state
devenv --no-tui tasks run --profile ci ci:static ci:style ci:test
```

`dev.ucl` is your own config and is not tracked: write one for your Plex
server, paths and listeners (`cargo run -p spoolframe-server -- config
defaults` prints every key). Without one, or without its `plex`, the server
starts "not set up": clients are told so until Plex is configured, and the
log names the web UI's addresses and the setup code, which opens the setup
wizard there. Every step is a tab of Server › Settings afterwards. The state directory
(`--state-dir`) holds the key, the database, the caches and the settings
changed at run time (`config show` lists every setting and where it comes
from; `config set` and `config unset` change one on the running server).

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
