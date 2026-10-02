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
- `spoolframe/client/1` (`spoolframe-protocol`): CBOR messages in
  length-prefixed frames, one QUIC stream per RPC, a server-opened events
  stream, typed errors, and room for newer peers (unknown requests are
  `Unsupported`, unknown errors and events decode as `Unknown`).
- The server speaks it: `Hello` first, answered with `ServerInfo`;
  `AuthMethods`, `LoginPassword` and `Ping` without a session; every other
  request is `Unauthenticated` until sessions exist; `ShuttingDown` on the
  events stream when the server stops. Optional top-level `name` config.
- A server database, `state_dir/spoolframe.db` (SQLite through sqlx, with
  migrations): local users, their password credentials, devices and
  sessions.
- Password login: Argon2id, one error for unknown user / wrong password /
  disabled user, five failures per 15 minutes per username and per source
  before logins are refused. Sessions are stored by token hash. Over Iroh,
  `LoginPassword` now opens a session bound to the client's endpoint;
  `Error::RateLimited` is new in the protocol.
- An admin API: JSON-RPC over WebSocket (`spoolframe-admin-api`), on
  `127.0.0.1:7421` by default (`admin { listen; allow_remote; }`).
  `setup` registers the first admin on an empty server, with the one-time
  setup code the server prints in its log at start; `login`,
  `logout` and user management for admins.
- `spoolframe-server setup` and `spoolframe-server user
  {add,passwd,list,disable,enable}`, which talk to the running server and
  log in like any admin (`SPOOLFRAME_ADMIN_USER`/`_PASSWORD` for
  scripts).
- Client sessions are checked on every request: a session works only from
  the endpoint that logged in, and one connection can carry several
  users' sessions. Sessions last 90 days from last use. New requests
  `WhoAmI`, `Logout` and `ListSessions`; `Hello` takes an optional
  `device_name`. Users have an optional display name (`user add
  --display-name`).
- In a session, clients now get the catalog: `Libraries`, `Items`,
  `Item`, `Search` and `Tracks` (the probed audio and subtitle tracks).
- Playback planning: a shared codec vocabulary (`VideoCodec`,
  `AudioCodec`, `Container`, `SubtitleFormat`) that ffprobe's names map to;
  clients send `ClientCapabilities` in `Hello`; `PlaybackPlan { id,
  selection }` answers `Direct` or `Unsupported` with a reason per
  stream, planning only the container, video and the selected tracks.
  `Remux` and `Transcode` exist on the wire for Phase 2.
- Artwork: clients get posters and backgrounds from the server on their
  own stream (`ArtworkRequest` → `ArtworkResponse` and the bytes), at
  fixed sizes (thumb 160, small 320, medium 640, large 1280, original),
  in a session. The server fetches each image once from the catalog,
  resizes it itself and keeps it in an on-disk cache under
  `artwork { cache_mb; }` (1024 MiB by default). Only real Plex image
  paths are ever fetched.
- Direct play over Iroh: `StartPlayback { id, selection }` plans the item
  and, if it plays directly, opens a playback handle; the client reads the
  file in ranges on `MEDIA` streams (`MediaRead` → `MediaReadReply { sent,
  size }` and the bytes, at most 16 MiB a read, many in flight). Handles
  are bound to their session and endpoint, expire after 5 minutes idle
  (`PlaybackExpired`), and re-check the session every 30 s. A selected
  external subtitle comes whole through `ReadSubtitle`.
- Watch state per user (movies and episodes): `UpdatePlaybackProgress`,
  `MarkWatched`, `MarkUnwatched`; started after 60 s, watched at 90% or
  with under 5 minutes left (`watch { … }` config), progress written at
  most every 10 s per item. Every `Items`, `Item` and `Search` reply
  carries the user's own `PlaybackState`.
- `Home`: continue watching, next up (the episode after the latest
  watched one of recently watched shows) and recently added.
