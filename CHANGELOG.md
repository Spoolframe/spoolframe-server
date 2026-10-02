# Changelog

## Unreleased

- The project skeleton, made with `foundry-make` from `__templates/rust`.
- The crate layout: permissive `spoolframe-types`, `-protocol` and
  `-client-sdk`; AGPL `spoolframe-plex` and `spoolframe-server`. A workspace
  test keeps the permissive crates from reaching an AGPL crate.
- `spoolframe-server --config <file>` loads a UCL config (schema
  `spoolframe.server.v1`, `state_dir`); unknown and repeated keys fail.
- `spoolframe-types`: the public media model (libraries, movies, shows,
  seasons, episodes, artists, albums, tracks, artwork, playback state) and
  namespaced string IDs such as `plex:movie:12345`.
- `spoolframe-catalog`: the `Catalog` trait (libraries, paged listings, an
  item with its children, search, media file path).
- `spoolframe-plex`: `PlexCatalog`, reading movies, shows and music from a
  Plex Media Server on the LAN, with an optional token.
- The server config has a required `plex { url; token_file; }` section
  (`token_file` optional). At startup the server lists the Plex libraries,
  and only warns if Plex is unreachable.
- Plex path mapping: `plex.path_mapping` rewrites the paths Plex reports
  to local ones (component-wise, longest prefix, library-scoped rules
  first). `media_path` returns only readable local files, and its error
  names the Plex path, the rule and the mapped path.
- `spoolframe-server plex check-paths [--library <title>]` reports, per
  library, files that are OK, have no matching mapping, or are missing; it
  exits non-zero on any failure. The server samples a few files per
  library at startup and warns for each library that fails.
- `spoolframe-media`: Spoolframe's own ffprobe of each media file into a
  `MediaDescriptor` (container, duration, video, audio and subtitle
  streams, external subtitle files), run with a timeout and cached until
  the file changes. Clients get the `MediaTracks` part (`spoolframe-types`).
- `spoolframe-server media probe <media-id>` prints an item's descriptor.
- The nix package puts ffprobe (`ffmpeg-headless`) on the server's PATH.
- The server has a permanent Iroh identity: a key made on first start in
  `state_dir/endpoint.key` (mode 0600) and only loaded after that; an
  unusable key file stops the server. `spoolframe-server id` prints the
  `EndpointId`.
- The server listens on an Iroh endpoint (LAN only, relays disabled) for
  `spoolframe/client/1`, tracks each connection as anonymous until a
  session says otherwise, and refuses `spoolframe/federation/1` with a
  reason. Optional `network { port; relay; }` config.
- The server runs until SIGINT/SIGTERM, and logs to stderr.
