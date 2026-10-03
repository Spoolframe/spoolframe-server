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
