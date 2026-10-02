# spoolframe

Self-hosted media server: Plex metadata, own playback, Iroh transport.

## Development

```bash
devenv shell
cargo run -p spoolframe-cli -- --version
devenv --no-tui tasks run --profile ci ci:static ci:style ci:test
```
