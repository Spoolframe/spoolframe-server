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
- The server announces itself on the LAN over mDNS (service
  `_spoolframe._udp.local`, its name in the record); `network { mdns =
  false; }` keeps it off. Hosts need inbound UDP 5353 open to hear it.
- `spoolframe-client-sdk`: `Client::open(storage, device)` with the
  app's `Storage` (secrets and plain data). The device key is made once
  and never replaced; unreadable storage is an error. The client lists
  servers heard on the LAN and saved ones (`servers()`), takes typed-in
  addresses (`add_address`), and pins a server's key on first `connect`;
  a server with the same name and another key stays a separate server.
  The client never announces itself.
- SDK sessions: `sign_in`, `profiles` ("who's watching", named by
  `WhoAmI`), and a `Session` per user with typed calls: home, libraries,
  items, item, search, tracks, playback plan, start playback, range reads,
  subtitles, artwork, progress and watched marks, sessions and sign-out.
  Every session shares the device's one connection to a server. A session
  the server no longer knows is forgotten, and the call says
  `SignInRequired`.
- The SDK keeps each server connected: when the connection is lost or the
  server says it is shutting down, it dials the pinned key again (backoff
  from 250 ms to 10 s) and the sessions go on working. `Client::state`
  reports `Disconnected`, `Connecting`, `Connected` and `Reconnecting {
  attempt }`. A call made while reconnecting waits up to 10 s, then fails
  with `Unreachable`.
- Track roles: every audio track is `main`, `audio_description`,
  `commentary` or `other`, every subtitle `full`, `sdh`, `forced` or
  `commentary`, from the file's disposition flags and then its title
  (whole words: "Audio Description", "SDH", "Forced", …) or, for subtitle
  files, `.sdh`/`.cc`/`.hi` in the name. `Tracks` replies carry them.
- Track preferences per user (`GetTrackPreferences`,
  `SetTrackPreferences`): audio languages and roles to avoid; subtitle
  mode (`always`, `foreign_only`, `forced_only`, `off`), languages and
  SDH. `PlaybackPlan` and `StartPlayback` without a `selection` let the
  server choose the tracks; the reply's `chosen` says which and why. A
  pick can be kept for the rest of a series or for a movie
  (`SetTrackOverride`, `ClearTrackOverride`), by language and role.
- A NixOS module, `services.spoolframe` (`nixosModules.spoolframe`, also
  re-exported by foundry's root flake): a hardened service under its own
  user, the Plex token as a systemd credential, path mappings, the port
  and mDNS, and a `spoolframe` admin CLI that knows the host's config.
  `checks.nixos-module` runs it in a VM.
- Encoder detection (P2-02): at start and on SIGHUP the server
  test-encodes with every VA-API, NVENC, QSV and software video encoder
  ffmpeg lists and keeps the ones that work, hardware first; audio
  encoders come from ffmpeg's list. `transcode { hardware = "auto" |
  "vaapi" | "nvenc" | "qsv" | "none"; vaapi_device; }`, and `media {
  ffmpeg; ffprobe; }` for the tool paths. `spoolframe-server status` (the
  admin API's `serverStatus`) shows what was found. The NixOS module
  gives the service the GPU's device nodes when hardware may be used.
- Converted playback plans (P2-01). Capabilities gain `delivery` (range,
  hls, progressive), `max_video` (height, frame rate, bit depth, HDR
  formats shown), `max_audio_channels` and `max_bitrate`, all optional.
  `Remux` and `Transcode` become one `Converted` plan: per stream copy or
  encode, subtitles copied, converted (to WebVTT for HLS) or burned in,
  with reasons. The planner copies what fits and encodes the rest with
  the detected encoders; a Phase 1 client gets the plans it got before.
  `StartPlayback` refuses a converted plan until conversions are served
  (P2-03), naming why it would convert.
- Two-way watch state sync with the Plex account the server reads as
  (PX-04): `plex { watch_sync { user = "…"; interval_secs = 120; } }`.
  Watched marks and resume positions move both ways, the newer side
  winning; a run happens at start, every interval, and a few seconds
  after the user's state changes here. `spoolframe-server plex sync-watch
  [--dry-run]` runs one now and prints it.
- Wire and config for converted playback (P2-03, P2-04, P2-05):
  `StartPlayback.start_ms`, `StopPlayback`, `SeekPlayback`, `GetManifest`,
  `epoch` and `segment` on media reads, `complete` and `epoch` on their
  replies, and the errors `Busy`, `PlaybackFailed` and `PlaybackRestarted`.
  `transcode { max_jobs; scratch_dir; scratch_limit_mb; read_ahead_secs; }`.
  `StopPlayback` works now; seeking and manifests come with the encodes.
- Encode jobs (P2-03): `StartPlayback` on a converted plan starts ffmpeg
  for the handle, from `start_ms` in the film's own time, into the
  scratch directory. It is paused once `read_ahead_secs` ahead of its
  reader and ends with the handle (stop, idle expiry, revoked session),
  taking its files with it. Over `max_jobs` the answer is `Busy`; leftover
  files are removed at start.
- Progressive delivery (P2-04): a converted playback's Matroska output is
  read over the same MEDIA range reads as a file, while it is encoded.
  `size` is what is written so far, a read past it waits up to 10 s, and
  `complete` says the encode has ended. `SeekPlayback` restarts the
  encode at a position as a new `epoch`; reads of the old one get
  `PlaybackRestarted`. A failed encode fails reads with ffmpeg's reason.
- HLS (P2-05): a converted playback for an HLS client is encoded into fMP4
  segments; `GetManifest` returns the multivariant and the VOD media
  playlist covering the whole file, and `MediaRead { segment }` reads a
  segment (0 is init). An encoded video is a 6 s grid; a copied one is
  cut at the source's keyframes, which the server lists once per file. A
  segment far ahead of the encode restarts it there.
- `spoolframe-media::pgs` (P2-07): the cues of a PGS (`.sup`) subtitle
  stream, each with its times and its picture as dark text on white, for
  OCR. Objects in fragments, several objects on screen at once, cropping
  and fades (one cue, not several) are handled. The `fixtures` feature
  writes PGS streams for tests, since ffmpeg cannot encode PGS.
- `spoolframe-media::ocr` (P2-07): PGS cues read as text by tesseract
  (several processes side by side, a list of pictures each), written as
  SubRip or WebVTT. A track is read with its own language's data, from its
  tag (Matroska's 639-2/B codes mapped to tesseract's), or English if
  untagged; a language without data is not guessed at. The dev shell has
  tesseract with English.
- Bitmap subtitles read as text (P2-07). A PGS track a client cannot draw
  is read with tesseract and sent as an external SubRip (or WebVTT)
  subtitle, so the file itself still plays directly; a converted playback
  keeps its video copied (`SubtitleAction::Ocr`) instead of burning the
  subtitle in. The first playback of a track starts the reading (the
  playback starts at once), `ReadSubtitle` answers `SubtitleNotReady {
  done_percent }` until it is done, and the text is cached, so later
  playbacks get it at once. `PlaybackPlan` and `StartPlayback` take
  `bitmap: auto | text | burn_in`: `text` reads even a track the client
  could draw (or is `CannotPlay`, saying why), `burn_in` burns it in.
  Config: `media { tesseract }`, `subtitles { ocr; ocr_threads;
  cache_dir; }`; the NixOS module's `subtitles.ocrLanguages` (default
  English) picks tesseract's languages.
- Client SDK: converted playback (P2-03 to P2-05) and bitmap subtitles
  (P2-07). `start_playback_with(id, selection, PlaybackOptions { start,
  bitmap })` and `playback_plan_with(id, selection, bitmap)` beside the
  existing calls, which keep their signatures; `Playback.epoch`;
  `read_chunk(handle, epoch, offset, len)` and `read_segment(handle,
  epoch, n, offset, len)` give a `Chunk { bytes, size, complete, epoch }`;
  `seek_playback(handle, position) -> epoch`, `stop_playback(handle)`,
  `manifest(handle, kind)`. `SdkError` gains `Busy`,
  `PlaybackFailed(reason)`, `PlaybackRestarted { epoch }` and
  `SubtitleNotReady { done_percent }`.
- Server key fingerprints (P1-21): `spoolframe_protocol::fingerprint`,
  SHA-256 over a label and the `EndpointId`, 80 bits in Crockford base32
  (`K7QX-3MPA-9TRD-WF2H`), with test vectors computed apart from the code.
  The server logs it at start (`server identity`), `spoolframe-server
  fingerprint` prints it with the server stopped, and the admin API's
  `serverStatus` (and `spoolframe status`) include it.
- Client SDK pins only on the user's word (P1-21, P1-13). `connect` says
  who answered (`ServerEntry.fingerprint`) and saves nothing; `pin(id,
  verified)` saves the server, and pinning a saved one only sets
  `verified` ("Verify now", no reconnect). Nothing but `Hello` goes to an
  unpinned server (`SdkError::NotPinned`). A saved server that seems to
  answer with another key (heard under its name while its own key is not)
  is `SdkError::IdentityChanged { saved, now }`, listed once, and never
  replaced by a connect or a pin; `trust_new_identity(saved, now)`
  replaces the pin, verified, and forgets the old key's sessions. Saved
  servers from before are `verified: false`.
- Remove from Continue watching (P1-22): `HideFromContinueWatching { id }`
  and `UnhideFromContinueWatching { id }`, and the SDK's
  `hide_from_continue` / `unhide_from_continue`. Per user; position and
  watched state stay. A movie hides itself, an episode its show (from
  Continue watching and Next up). The next watch-state write for the item
  or any episode of the show clears it, in the same transaction, and so
  does progress the write throttle skips; Plex sync's writes leave it.
  Hides follow an item to a new key by its external ids, like watch state
  (migration 0005).
- User roles (P2-09): `Role { Admin, Consumer }` (with `Unknown` for a
  newer server's) replaces `is_admin` on users (migration 0006), in
  `UserSummary`/`WhoAmI` and the admin API. The admin API splits into
  `me.*`, the caller's own account for every role (`login`, `logout`,
  `whoAmI`, `setPassword` with the current password, `sessions`,
  `revokeSession`), and `admin.*`, other users and the server, admins only
  (`FORBIDDEN` otherwise; `admin.setup` still makes the first admin). The
  role is read on every call, so a demoted admin's next `admin.*` call is
  refused without signing them out. `admin.userSetRole`; the last enabled
  admin cannot be demoted or disabled (`LAST_ADMIN`). CLI: `user add
  --role admin|consumer` (`--admin` kept, hidden), `user role <name>
  <role>`, and `user list` shows each role.
- Text subtitles converted once and cached (P2-07). A text subtitle the
  client cannot show (ASS, `mov_text`, …) but whose text it can take is
  converted to SubRip or WebVTT by ffmpeg and sent beside the file, as
  read-as-text bitmap subtitles are, so the file still plays directly;
  ASS styling is lost. Conversions share the OCR cache, its key and its
  `SubtitleNotReady` answer.
- HLS subtitle rendition (P2-07): an HLS playback's subtitle (copied,
  converted or read from pictures) goes as WebVTT. The multivariant
  playlist names it (`#EXT-X-MEDIA:TYPE=SUBTITLES`, with the BCP 47
  language); `GetManifest { kind: Subtitles }` is its playlist, on the
  video's own cuts; `ReadSubtitleSegment { handle, n }` (SDK
  `read_subtitle_segment`) is segment n's cues, with `X-TIMESTAMP-MAP`
  tying them to the video's timestamps. An empty `CODECS` is no longer
  written.
- Changing subtitles mid-playback (P2-07): `ChangeSubtitle { handle,
  subtitle, bitmap, position_ms }` (SDK `change_subtitle`) plans the
  playback again with the same audio. Direct play changes only what
  `ReadSubtitle` gives; a converted playback restarts its encode at the
  position as the next epoch (`PlaybackChanged { plan, size, epoch }`),
  so reads of the old one get `PlaybackRestarted`; a choice that cannot
  play is `CannotPlay` and leaves the playback as it was.
- Burned-in PGS is scaled to the video (P2-07): the subtitle's canvas is
  scaled to the picture before it is laid over it. Blu-ray PGS is 1080p
  even over 4K video, and was burned in small at the top left (or off
  the picture). Tested on a decoded frame: the cue shows where it was
  placed, nothing else changes, and nothing shows after the cue.
- Profile PIN storage (P2-08): a PIN per user, hashed like a password
  (migration 0007), and on which of the user's devices it is asked for
  (`pin_required`, per user and endpoint, outliving sign-outs). Clearing
  the PIN clears its requirement everywhere; requiring needs a PIN; a
  device is one the user has signed in from. The session lookup reports
  whether this device requires the user's PIN.
- Profile PIN lock (P2-08): on a device where the user's PIN is
  required, their session starts locked on every connection. Locked, it
  answers only `WhoAmI` (`locked: true`) and `UnlockProfile { pin }`;
  everything else is `ProfileLocked` on every stream (RPC, range reads,
  artwork). Using another profile on the connection locks it again.
  Wrong PINs are rate-limited apart from passwords, and 10 in a row
  revoke the session on that device. `SetPin`, `ClearPin`,
  `ListDevices` and `SetPinRequired { device, required }`. A PIN is never
  a way to sign in: it only locks and unlocks a session that a real
  sign-in (the password; later SSO or a passkey) made, so an unlocked
  session is all these take.
- Profile PINs in the admin API and CLI (P2-08): `me.pinSet`,
  `me.pinClear`, `me.devices` and `me.setPinRequired` for the caller's
  own account; `admin.userClearPin`, `admin.userDevices` and
  `admin.userSetPinRequired` for anyone's (an admin clears a PIN, never
  sets one). CLI: `user pin set <name>` (signed in as that user with
  their password), `user pin clear <name>`, `user devices <name>`, and `user
  pin require <name> --device <name> [--off]`.
- Client SDK: profile PINs (P2-08). `Session::unlock(pin)`, `set_pin`,
  `clear_pin`, `devices` and `set_pin_required(device, required)`; a
  locked profile's `user().locked` from `Client::profiles`, and
  `SdkError::ProfileLocked` and `InvalidInput` from its calls.
- Device sign-in by approval (P3-07): a device that cannot type a
  password (an Apple TV) sends the pre-auth `BeginDeviceSignIn` and shows
  the code (`BCDF-GHJK`, 8 letters without vowels) and its QR link
  (`spoolframe://approve?server=…&code=…`, in
  `spoolframe_protocol::device_sign_in`); a signed-in, unlocked phone
  `LookupDeviceSignIn`s and `ApproveDeviceSignIn`s (or `Deny…`) it, and
  the device's `AwaitDeviceSignIn` (up to 25 s, then `Pending`) gets a
  session of its own, bound to its endpoint, as the approver's user.
  Codes live 5 minutes, are used once, are never approvable from the
  device that shows them, and wrong ones are rate-limited per user and
  per device; only the device that began a request may await it, and one
  device has at most 3 pending. Pending sign-ins are only in memory.
  `AuthMethods` lists `device_approval`.
- Client SDK: device sign-in by approval (P3-07). On the TV,
  `Client::begin_device_sign_in(server)` gives a `DeviceSignIn` (`code`,
  `display_code`, the QR `link`, `expires_in`) and
  `Client::await_device_sign_in` waits and keeps the session as
  `sign_in` does (`SdkError::SignInDenied` / `SignInExpired`). On the
  phone, `device_sign_in::parse_link` reads a scanned code, and
  `Session::lookup_device_sign_in`, `approve_device_sign_in` and
  `deny_device_sign_in` answer it.
- The admin listener's browser side (P3-01): axum now fronts the admin
  JSON-RPC socket (at `/jsonrpc`, and at any path for the CLI as before)
  and serves `POST /auth/setup`, `/auth/login` and `/auth/logout` and
  `GET /auth/me`. Setup and login set the `spoolframe_session` cookie
  (`HttpOnly; Secure; SameSite=Strict`, an API session like the CLI's),
  which the socket accepts as it does the CLI's Bearer token (the header
  wins when both are sent); logout revokes it and clears the cookie. A
  POST or socket upgrade from another `Origin` is refused: the listener's
  own host by default, or exactly `admin.public_url` (config, and the
  NixOS module's `admin.publicUrl`) behind a TLS proxy.
- A killed app no longer keeps the server busy: a device starting a
  playback ends its own earlier converted playbacks (a device plays one
  thing at a time), and a device's playbacks end 30 s after its last
  connection closes, unless it reconnects. Before, an app killed
  mid-film held its encode slot until the playback idled out after 5
  minutes, and the next start was refused as `Busy`.
- Next up follows the episode watched most recently, not the furthest one
  watched: a rewatch (or a show picked up again from the middle) goes on
  from where the person is, as Plex's On Deck does. Before, a show whose
  finale had ever been watched never came back to Next up.
- The web UI (P3-01): `spoolframe-web`, a Dioxus 0.7.10 app built to wasm
  with `dx bundle` (its own `wasm-release` profile: wasm-opt aborts on a
  module with debug sections) and carried by the server behind the
  `embed-web` feature, which `nix build` turns on. The admin listener
  serves it on every path that is not `/auth`, the socket or a file; a
  server built without it says so. Pages: setup (with the code from the
  log) while the server has no users, login, then an admin's Server page
  or a consumer's My profile, by the role read at each load; sign out.
  Catppuccin Latte or Mocha by the browser's preference. `GET /auth/setup`
  tells a signed-out page whether setup is open. devenv gains dx,
  wasm-bindgen and Tailwind (`spoolframe:web` bundles), `ci:static` a
  wasm clippy pass, and `ci:style` a check that the committed stylesheet
  is fresh.
- Admin API for the admin pages (P3-02): users carry `lastSeenAt`;
  `admin.userSessions` and `admin.userRevokeSession` list and end anyone's
  sessions, each with its device's `endpointId` (none for web or CLI);
  `admin.plexStatus` says whether Plex answers, its name and each
  library's item count (Plex down is an answer, not an error);
  `admin.plexCheckPaths` runs the path check over the API; `serverStatus`
  adds `mdns` and the endpoint's `addresses`. The CLI matches: `user list`
  shows when each user was last seen, `user sessions <name>`, `user
  revoke-session <name> <id>`, and `status` prints Plex, its libraries,
  mDNS and the addresses. `admin::serve` takes its services as one
  `Admin`.
- The admin pages (P3-02), under `/admin`: Users lists everyone with
  their role, whether they are enabled and when they were last seen, and
  adds a user, resets a password, makes an admin or a consumer, and
  disables or enables; the last enabled admin's "Make consumer" and
  "Disable" are greyed with the reason, rather than refused after a
  click. A user's Sessions lists each (its device, or "Web or CLI"; this
  browser marked) with when it began and was last used, and ends one.
  Server shows the name, version, fingerprint and key (with a copy
  button), LAN discovery and addresses; Plex reachable or not, its name
  and each library's item count; and a path check of every file, with
  examples of the failures.
- Failed logins on the admin API count per username and per caller's
  address, not under one name shared by every caller: five wrong passwords
  for any name used to lock everyone out of login, `me.setPassword` and
  setup, for as long as someone kept guessing. A loopback address (the
  CLI, an SSH tunnel, a proxy on this machine) is shared by all who come
  through it, so it counts for no one, and only the username is limited
  there (P1-11).
- A login or a PIN counts against the failure limit from the moment its
  check begins, and is taken back if it was right. Before, the limit was
  read before the slow check and written after it, so many wrong
  passwords or PINs sent at once were all checked: five tries per 15
  minutes was as many as fit in one burst (P1-11, P2-08).
- The failure limit forgets what it no longer needs: failures older than
  15 minutes and names with none left are dropped, and a login under a
  name no user can have (longer than 64 characters, or with other
  characters) is refused at once and not counted. Before, every name
  ever tried, up to 1 MiB each, was kept until the server stopped, so
  anyone on the LAN could fill its memory (P1-11).
- HLS of a copied video whose first keyframe is not at 0 lists the
  segments ffmpeg writes. ffmpeg never cuts at the first keyframe, but the
  playlist had a cut there: it listed one segment too many, each segment
  held the time the playlist gave to the next, and the last was empty.
  The first segment now runs from 0 to the second keyframe (P2-05).
- A subtitle change in a converted playback needs no free conversion
  slot: the new conversion runs in the slot of the one it replaces.
  Before, it asked for a second slot while the first was still held, so
  with `transcode.max_jobs = 1`, or with every slot in use, each change
  was refused as busy (P2-03, P2-07).
- A seek whose new encode cannot start leaves the playback playing. The
  new encode starts in the slot of the one it replaces, and only then is
  the old one stopped. Before, the old one was stopped first: its slot
  could go to another viewer, and if the new encode did not start, the
  playback kept a dead encode whose reads returned nothing, at once and
  for ever (P2-03).
- A full conversion scratch directory (`transcode.scratch_limit`) fails
  one conversion, not all of them. Only a conversion that is writing can
  be the one that fills it: an HLS one first removes its own segments
  behind its reader, the oldest first (one asked for again is encoded
  again), and if that is not enough it fails with `PlaybackFailed` and
  its files are removed at once. Before, every conversion, paused ones
  too, failed within a moment of the directory going over, and the files
  stayed, so new conversions failed as well (P2-03).
- A PGS file beside the film (`Movie.en.sup`) can be burned into the
  video: its pictures are laid over the video, as an embedded PGS
  track's are. Before, it went to the filter that draws text, ffmpeg
  refused it, and a playback the server had offered failed (P2-07).
- A VobSub subtitle beside the film (`Movie.en.idx` with its
  `Movie.en.sub`) plays in a converted playback, sent on as a stream to
  a client that draws VobSub or burned into the video. Before, ffmpeg
  was given the `.idx` under a name it could not find the `.sub` by,
  and the playback started and then failed (P2-07).
- At start the server removes only its own leftover conversions: they
  are kept in a directory of their own, `spoolframe-jobs`, under
  `transcode.scratch_dir`, and only that is emptied. Before, the whole
  of `scratch_dir` was removed at every start, so a `scratch_dir` set to
  the state directory, to the parent of the subtitle cache or to a
  directory other programs use lost the server's key and database, the
  subtitle texts or the other programs' files. `scratch_limit` counts
  only what is in `spoolframe-jobs` (P2-03).
- An item is watched at 95% of its duration (`watch.watched_percent`,
  before 90), unless it has credits that run to its end (below). The
  "less than 5 minutes left" rule is gone, and its config key
  `watch.watched_remaining_secs` with it: a config that still has the
  key does not load. Before, a 4-minute clip was watched at its first
  progress report and a 7-minute episode at 2:00 (P2-10, P1-14).
- The end of an item is written as seldom as the rest of it: once at the
  crossing into watched, then at most once every 10 seconds. Before,
  every progress report after the crossing
  was a full write, however often the client sent them (P1-14).
- The same film in two libraries (two items with the same external ids)
  keeps a watch state for each copy. Before, to open one copy's page
  moved the other copy's progress, and its "removed from Continue
  watching", to it. State moves to a new key only when the item has
  left the old one: the catalog has nothing under it, or another item
  (after a rebuild Plex hands the same keys out again), or it is not
  this catalog's key. Before, an old key that another film had taken
  kept the state for good, and one that a show had taken made the
  item's page and every progress write for it fail. If the catalog
  cannot say what the old key holds, the state stays and the next look
  asks again; the item itself works. A state written for the new key
  in the meantime (by the Plex sync) is kept, and nothing moves onto
  it; before, the request failed. A look at a second copy asks the
  catalog about the first with one small request (the new
  `Catalog::external_ids`), at most once a minute (P1-14).
- A subtitle file beside the film that is edited in place is converted
  again at the next playback. Before, the text made of it the first time
  was served until the media file itself changed (P2-07).
- During a playback the Plex watch sync runs at most once a minute.
  A change in Spoolframe still starts a run 5 seconds later, but not
  less than 60 seconds after the run before. Before, each progress
  write (one every 10 seconds) started a run, and each run reads all
  of Plex's watch state (PX-04).
- A reply too large for one frame (1 MiB) is answered with the new
  `Error::ReplyTooLarge { bytes }`, and the connection carries on.
  Before, the stream ended with no response: a subtitle file beside the
  film larger than 1 MiB could not be read and the client was not told
  why. Such a subtitle still cannot be sent; now the client can say so.
  An older client decodes the new error as `Unknown` (P1-10).
- `spoolframe-types`: `Marker { kind, start_ms, end_ms, to_end }`, an
  intro or the credits of a movie or episode. `Catalog::item` returns an
  item's markers, in order of start. `PlexCatalog` reads them from Plex
  (`includeMarkers=1`, asked only with a movie's or episode's details);
  Plex's other marker types are left out, and an item Plex has no markers
  for (no Plex Pass, detection off) has an empty list. A marker that
  cannot be read (an offset missing or not a whole number, no type), of
  any type, or that does not end after it starts, is left out too; the
  item loads without it. Before, one such marker made the item's page,
  `StartPlayback` and every progress write for it fail (P2-10).
- The item reply and `StartPlayback`'s reply carry the item's `markers`,
  and the SDK passes them on as `ItemDetail::markers` and
  `Playback::markers`. The field is optional on the wire: an older client
  ignores it, a reply from an older server decodes with an empty list,
  and a marker kind this build does not know decodes as
  `MarkerKind::Unknown` (P2-10).
- An item with credits that run to its end (a `Credits` marker with
  `to_end`) is watched once playback reaches the start of those credits,
  and not before: the 95% rule does not apply to it (P2-10, P1-14).
- HLS of a copied video whose first keyframe is before time 0 (an MP4
  cut with stream copy starts at the keyframe before the cut) lists the
  segments ffmpeg writes. That keyframe was left out of the file's
  keyframes, the second was taken for the first, and the playlist had
  no cut where ffmpeg made its first: it listed one segment fewer, and
  every segment after the first held the time the playlist gave to the
  one before it (P2-05).
- A PGS file beside the film that is sent on as a stream (the client
  draws PGS, and the film is converted for another reason) keeps its
  cues' times when played from the start. Before, every cue showed
  early by the first cue's time; after a seek they were right (P2-07).
- An HLS playback that seeks back to a part still converted keeps it
  while the scratch space is full. The server took the furthest segment
  ever asked for as where the player was, so after a seek back it
  removed the segments the player was reading to make room, and each
  such read started the conversion again. It now goes by the segment
  asked for last. A conversion held back for its player is no longer
  failed for space it cannot free at that moment (P2-03).
- An HLS segment removed just as it is read (for room in the scratch
  space, or with a conversion that fails for a full one) is answered as
  not ready, and the client asks again: the conversion restarts there,
  or the read fails with the conversion's reason. Before, the read
  could be answered as the whole of an empty segment, which a player
  takes for the segment, or fail with "a segment of the conversion
  could not be read" (P2-05, P2-03).
- `admin.trusted_proxies` (`services.spoolframe.admin.trustedProxies`):
  the reverse proxies in front of the admin listener, as addresses and
  CIDR ranges, comma-separated. The limit on failed logins counts a
  connection from one of them under the caller its `X-Forwarded-For`
  names: the right-most address that is not itself a trusted proxy
  (`Forwarded` is not read). Before, with the proxy on another host or
  in a container, everyone behind it was one source, and five wrong
  passwords from anyone locked all of them out. A trusted proxy that
  sends no usable header counts under no address, as a loopback
  connection does, and the server warns. Without the setting nothing
  changes: no header is read, a connection counts under its own
  address, and a loopback one under none (P1-11).
- The limit on failed logins costs the same however many names and
  sources have failed. It looks through all of them once a minute, not
  at every attempt, and judges the name and source of an attempt as of
  that moment. It holds at most 100 000 of them: while it is full, an
  attempt under a name or from a source it does not hold is refused as
  `RateLimited`, until old failures leave the window. Before, a LAN
  peer sending logins with a new name and a new Iroh key each time made
  every login wait behind a scan of all of its earlier attempts
  (P1-11).
- `spoolframe-server` allocates with snmalloc, with its hardening checks
  on, as mainframe's daemons do.
- A subtitle is read in chunks, so one over the 1 MiB frame limit
  arrives. `ReadSubtitle { handle, offset, version }` answers
  `SubtitleChunk { format, bytes, total, version }`: at most 512 KiB of
  the file from `offset` (`SUBTITLE_CHUNK`), the file's length, and a
  `version` that names the file as it was when the chunk was read. The
  server reads only that chunk from the disk; an `offset` at or past the
  end answers no bytes. Before, the server read a file over the limit
  whole and encoded it only to find that it could not be sent, at every
  request (P2-11).
- A subtitle that changes between two chunks is never joined from two
  files. A client sends the first chunk's `version` with each later
  read, and the server answers the new `Error::SubtitleChanged` when the
  file it opens is not that one: written again in place (another length
  or modification time), replaced by another file (another inode), or
  another subtitle after `ChangeSubtitle`. The version is a hash of the
  open file's device, inode, length and modification time (P2-11).
- The SDK's `read_subtitle` reads the subtitle in chunks and returns the
  whole file as before; an app changes nothing. When the server says the
  file changed, the SDK reads it again from the start, once, and returns
  the new file; if it changes during that read too, the call fails with
  the new `SdkError::SubtitleChanged` (P2-11).
- **Breaking on the wire:** `ReadSubtitle` needs `offset`, and answers
  `SubtitleChunk`, not `Subtitle`, which is now only a segment of an HLS
  subtitle rendition and has no `total`. A request without `offset` is
  `Malformed`, and there is no whole-file read any more: an app built on
  an older SDK cannot read external subtitles from this server until it
  is built on this SDK (P2-11).
- HLS segments store the film's own time. A run that started at a
  position (a resume, a start offset, a far seek) stored its fMP4
  fragments from 0 and put where it started in `init.mp4` as an empty
  edit. AVPlayer places WebVTT cues on the stored times and does not
  apply that edit to them, so a playback started past 0 showed no
  subtitles, or showed them late. ffmpeg now writes each fragment's own
  time (`-hls_segment_options movflags=+frag_discont`): a segment the
  playlist puts at 400 s is stored at 400 s, in every run, and
  `init.mp4` has no empty edit (P2-05).
- An encode is never held back while its reader has nothing to read. A
  progressive encode was paused once ffmpeg reported `read_ahead_secs`
  of encoded time, though ffmpeg had not yet written its first 2 s
  cluster: with a read-ahead under 2 s and an encode no faster than a
  few times real time, the reader waited for bytes that a paused encode
  never wrote, for ever. An encode is now paused only while its reader
  has written bytes still to read, or, for HLS, while the segment it
  asked for last is finished. `read_ahead_secs = 0` also works now: it
  once paused the encode and never let it go on (P2-04).
- An encode that stalls is ended. ffmpeg can wait for ever without
  failing: ffmpeg 9.0 (and 8.1) does when it burns in a picture subtitle
  file that holds one cue and the file's end reaches its filters before
  they are set up. Such an encode kept its slot, and its reads waited for
  as long as the client asked. An encode that is not held back for its
  reader and has neither written anything nor reported more encoded time
  for 120 s is now killed; it gives up its slot, and its reads fail with
  `PlaybackFailed` ("the conversion stalled: ffmpeg made no progress for
  120s"). The time an encode is held back does not count (P2-03, P2-07).
- HEVC in HLS is `hvc1`. ffmpeg wrote the video of `init.mp4` under the
  sample entry `hev1`, for a copied stream and for every HEVC encoder,
  while the playlist said `hvc1`. Apple's players play HEVC in fMP4 only
  as `hvc1`: an iPhone given a 4K HEVC film showed nothing, and the
  server logged no error. ffmpeg is now told the name (`-tag:v hvc1`)
  for HEVC in HLS; H.264 and progressive Matroska are as they were
  (P2-05).
- The HLS multivariant playlist says the video range and the frame rate.
  `EXT-X-STREAM-INF` now has `VIDEO-RANGE` (`PQ` for SMPTE 2084, `HLG`
  for ARIB STD-B67, else `SDR`), which Apple's players want for HDR, and
  `FRAME-RATE` when the file says it. Both describe the output: a copied
  stream and an encode that is not tone-mapped keep the source's
  transfer, a tone-mapped encode is `SDR`. A copied HEVC stream is named
  in `CODECS` with its own level (`hvc1.2.4.L123.B0` for Main 10 at 4.1)
  where it was guessed from the height. The media descriptor has the
  video's `level` (P2-05).
- Passkeys on existing accounts, the server side of adding them. The
  relying party is the host of `admin.public_url` (HTTPS), or `localhost`
  without one; a plain-HTTP `public_url` on another host, or one that
  names an address, has no passkeys, and the API says why
  (`NOT_CONFIGURED`). `me.passkeyAddBegin` and `me.passkeyAddFinish` add
  one, named by its user: stored on the authenticator (`residentKey:
  required`), with user verification, and with no say on where the
  authenticator is or how it is reached, so the device's own store, a
  password manager, a security key and a phone over the browser's QR
  code all work. `me.passkeys`, `me.passkeyRename`, `me.passkeyRemove`
  and `me.passkeyHost` manage them; admins have `admin.userPasskeys` and
  `admin.userRemovePasskey`, and cannot add one for someone else. Each
  passkey keeps the host it was made for; at start the log names those
  made for another host than the server has now. The build links
  OpenSSL (webauthn-rs) (P3-05).
- Adding or removing a passkey needs a recent sign-in: a session whose
  user signed in, or confirmed it with `me.confirmPassword`, more than 10
  minutes ago is refused with `RECENT_SIGN_IN_REQUIRED`. A user's last
  way to sign in cannot be removed (`LAST_SIGN_IN_METHOD`) (P3-03,
  P3-05).
- Signing in with a passkey, with no username. `POST /auth/passkey/begin`
  answers a request (`ceremony`, and the options for the browser's
  `navigator.credentials.get`: no credentials listed, user verification
  required), `POST /auth/passkey/finish` takes what the passkey signed and
  answers as `/auth/login` does, with the same cookie; `me.passkeyLoginBegin`
  and `me.passkeyLoginFinish` are the same on the socket. A request is
  good once, for 5 minutes. Every failure is `PASSKEY_REFUSED` and the
  log says why: an unknown or removed passkey, another relying party or
  origin, no user verification, a user handle that is not its user's, a
  disabled user, or a signature counter that went backwards (a copied
  passkey). Failures count in the limit on failed logins, under the
  passkey's user and the caller's address (P3-05).
- A device's sign-in can be approved in a browser. `BeginDeviceSignIn`
  answers `web_url` too, the web UI's page for the code
  (`<admin.public_url>/auth/device?code=<code>`; absent without a public
  URL), for a QR code any phone's camera opens; the SDK's `DeviceSignIn`
  has it as `web_url`. A web session, made by a password or a passkey,
  completes the pending request with `me.deviceSignInLookup`,
  `me.deviceSignInApprove` and `me.deviceSignInDeny`: the same codes, one
  use, 5 minutes, and wrong codes limited under the count the user's
  phone shares (`NO_SUCH_SIGN_IN_CODE`, `RATE_LIMITED`). Every role may
  approve for themselves. Wire change: `DeviceSignInStarted` has a new
  field, so clients rebuild on this SDK (P3-07, P3-05).
- The web UI has passkeys. The login page has "Sign in with a passkey"
  (the browser's own prompt, no username; hidden where the page has no
  secure context or the browser no WebAuthn). "My profile", every role's
  own page and a menu entry for an admin, has the sign-in methods: the
  password, changed with the current one, and the passkeys, listed with
  their name, when they were added and last used, renamed and removed;
  "Add a passkey" names the new one, says which host it is bound to and
  warns when that is `localhost`. A session older than 10 minutes is
  asked for the password first. An admin's page for a user lists the
  user's passkeys and removes them. `/auth/device?code=…`, what a
  device's QR code opens on a phone, signs in there if need be (password
  or passkey), shows "Sign in on '<device>'?" with the code, and
  approves or denies; without a code it asks for one (P3-05, P3-03,
  P3-07).
- `GET /auth/site` tells a signed-out page the server's name and its
  `admin.public_url`, if any. The server logs every password and passkey
  sign-in on the admin API with the user and the caller's address, and
  warns at start when the admin listener is not loopback and no
  `admin.public_url` is set: browsers keep the session cookie only over
  HTTPS or at `localhost`, so signing in at such an address looks like
  nothing happened. `me.confirmPasskey` confirms a session with one of
  its user's own passkeys, as `me.confirmPassword` does with the password
  (P3-05, P3-03).
- The web UI's login, My profile, "Add a passkey", "Confirm it's you",
  device approval and a user's passkeys follow the "Spoolframe UI"
  design boards: the signed-out card over the colour blobs with the
  mark and "Sign in to <server>", the sidebar (a bar on a phone), the
  cards and rows, the dialogs, the design's icons, and its fonts
  (Outfit, Figtree, JetBrains Mono, Bricolage Grotesque), which the build
  copies into the bundle from nixpkgs, so nothing is fetched at run
  time. In an insecure context (plain HTTP on a real host) the sign-in
  card says the browser would drop the session, names
  `admin.public_url` or `localhost` instead, and takes nothing (P3-05,
  P3-03, P3-07).
- A debug server built without `embed-web` takes the web UI's pages from
  `dx serve` on `127.0.0.1:8080` and tunnels its hot-reload WebSocket
  (`/_dioxus`), so the browser opens only the admin listener; without
  `dx serve` each page says how to start it. `devenv up` runs `dx serve`
  (fonts copied first) and Tailwind in watch mode. Builds with
  `embed-web`, and release builds without it, are unchanged.
- A sign-in on the admin listener's HTTP side (password, passkey or
  setup) from a plain-HTTP page on anything but loopback, as its
  `Origin` names it, logs a warning. The browser will drop the Secure
  session cookie, so the sign-in appears to do nothing, and the warning
  names HTTPS (`admin.public_url`) or `http://localhost` instead.
