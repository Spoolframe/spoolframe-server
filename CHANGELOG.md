# Changelog

## Unreleased

- HLS of a copied video is cut into segments of at least
  `transcode.copy_segment_secs` (2 s if left out, 1 to 10; review
  2026-10-11 M2), each at the first keyframe that far past the last,
  instead of at every keyframe: an all-intra film was a segment per
  frame, 172,627 over two hours, now 3,597. A conversion keeps the
  length it started with. Its runs are cut by ffmpeg's segment muxer at
  that list, the same from a resume or a quality switch, and a resumed
  run starts exactly at its segment's keyframe. Where ffmpeg cannot write
  the header before the first packet (a codec record without its
  parameter sets, an `avc3`/`hev1` track, or AC-3 or E-AC-3 sound), the
  video is cut at every keyframe as before. The segments, their names and
  numbers are as before; they no longer start with a `styp` box.

- SDK review fixes (2026-10-12, C4-C8, P5-P8, B2-B5). Forgetting a
  server clears its sessions before its pin. `Session::password_change_required()`
  turns false, on every clone, once `change_password` succeeds. Closing
  a server's link lets a storage write that is under way finish.
  Relay pairing refuses with `PairingError::NotAllowed` once guests were
  turned off after the offer. `add_root` checks the root count before
  the key registers, and refuses a credential no root can carry
  instead of panicking. A removed phrase root issues no credentials, and
  a tie proof then uses the device credential. Recovery phrase words no
  longer stay in freed memory. A managed playback's share task ends with
  it. A late, older segment open no longer hides a newer one. A re-plan
  for the link restarts an ended playback instead of failing, and the
  direct reader asks again after a failed ask. A resume from a thread
  outside Tokio starts on the playback's own runtime.

- SDK: `Portable::guest_pairings()` lists each guest with the server it
  was paired for and when (`GuestPairing { id, server, paired_at }`,
  `GuestServer { endpoint_id, name }`), for a "Shared computer" row's
  "server name · date". A pairing through a server keeps that server; a
  QR pairing keeps the first server the phone handed. Guests paired
  before this have `None` for both. `Portable::guests()` is unchanged.

- SDK: `Session::leave_server` removes only the leaving user's profile
  on this device, and forgets the server (pin and all) only when no
  other profile is kept for it. Before, on a shared device such as a TV
  with Alice and Bob, Alice leaving also took Bob's saved session and
  the pin. An app that shows "It's no longer saved here" checks
  `Client::servers()` after the leave.
- `WhoAmI` answers `can_leave` (PX-47): `false` for the last admin, whom
  `LeaveServer` would refuse with `LastAdmin`, so an app greys out
  "Leave this server" before it is asked. Additive at revision 14: left
  out by an older server, it reads as `true`. The SDK has it as
  `Session::can_leave()`, as of sign-in (or `Client::profiles`), and
  `Session::can_leave_now()`, asked again.

- PX-47: a user leaves a server from the app (`LeaveServer`, after a
  recent sign-in). A portable user's grant is revoked and its data here
  deleted; a local account is disabled and marked left, with its data
  kept, for an admin to restore. The last admin cannot leave
  (`LastAdmin`).

- On VA-API with GPU filters, an HDR film made SDR has its burned-in
  pictures laid over it after the tone map, not before: SDR subtitles
  are no longer mapped with the video, dimmed or shifted, and the GPU's
  overlay gets 8-bit frames as it expects.

- `transcode.scratch_dir` (or, unset, the state directory it defaults
  under) with one of `' : \ , ; [ ]` in its path is refused at load, the
  key named: ffmpeg's subtitle filter reads the film through a link in
  there and cannot take those characters, so every burned-in text
  subtitle failed, blamed on a subtitle file. The refusal of a path a
  burn-in cannot take now names which path it was.

- The web UI's sign-in endpoints answer a refusal with its own HTTP
  status instead of 500: a sign-in too old for a change is 403, a
  missing user, session, code or invite 404, the last admin 409, and
  Plex or a sign-in provider that does not answer 503. The JSON body is
  as before.

- Picture subtitles read better and fail less (review 2026-10-11 Lows).
  PGS goes through the same text and outline split as DVB and VobSub, so
  a black outline is no longer read as text; one PGS composition placed
  off the screen is skipped instead of failing OCR for the whole film; and
  DVB and VobSub data that cannot be read is counted as broken instead of
  read with made-up defaults.

- Previews that failed because a tile timed out (a slow network mount)
  or ffmpeg would not start are tried again at the next play, instead of
  staying failed until a restart; two items of the same file make their
  previews once.

- Review 2026-10-11, runtime and network Lows:
  `plex.watch_sync.interval_secs` under 10 is refused with its key, not
  quietly raised to 10 (L33). A second SIGINT or SIGTERM during shutdown
  stops the server at once (exit 130), and each shutdown step is bounded
  (L34); telemetry's flush on the way out takes at most 5 s (L44). A
  reload that changes `network.max_stranger_connections`,
  `first_frame_timeout_secs` or `stalled_read_secs` now says they apply
  after a restart (L37). A token file other users can read is warned of
  (L40). The start-up check of each Plex server runs in the background,
  so a stalled PMS no longer holds the web UI back (L41). Connections
  may now idle 30 s, server and apps alike, so a phone app suspended a
  little while keeps its connection (L43). A relay serving its own
  certificate files must be restarted when they are renewed; the docs
  now say so (L48).

- A DVB subtitle track is read for its own page only. A PID that carries
  a page per language (common in European broadcasts) was read as one
  track, the languages' cues interleaved and cutting each other short.

- Previews of played films are made two at a time: one slow film, such
  as one on a NAS that answers each tile in minutes, no longer holds up
  the previews of every film played after it.

- The subtitle text cache is bounded at 512 MiB: past it the least
  recently used texts are removed, and a text served from the cache
  counts as used. It had no bound, and a re-ripped film's old text stayed.

- A device holds at most 16 direct playbacks: a new one past that ends
  its least recently used one that is not being read. A client starting
  playbacks in a loop no longer piles up handles for 30 minutes each.

- A conversion that keeps HDR is refused as busy when no 10-bit encoder
  of its codec has room, instead of spilling to a software encode (or an
  8-bit card) that wrote 8-bit video under an HDR transfer. Either case is
  logged, and so is every spill to a software encode, with the encoder it
  runs on and the GPUs that were full.

- A file with a stream of a codec ffprobe does not know is described
  with that stream's codec as "unknown", instead of the whole file being
  unreadable: ffprobe leaves `codec_name` out for such a stream, and the
  probe failed to parse.

- `network.wan.mapped_port.public_ip = "lookup"` looks the address up
  again every hour, and after a failed lookup. It was looked up once at
  start: a server started before its network was up advertised no
  address, and one whose ISP changed its address kept the old one, until
  a restart.

- Measuring the upload works on slow uploads. Each upload was a fixed
  10 MiB with 25 s to finish, so under about 3.4 Mbit/s, the homes the
  budget matters most to, the measurement failed with no suggestion. A
  1 MiB upload now sizes the measured ones to about 8 s each.

- Plex watch sync follows `watch.started_after_secs` as it changes. It
  took the threshold once, at start, so after a change in the web UI
  the sync still judged what is in progress by the old one until a
  restart.

- A relay config (`spoolframe.relay.v1`) with no `server.addrs` is
  refused at start, with the key named. The relay dials the main server
  only there, so it loaded, asked nobody, and turned every device away,
  the owner's included. The NixOS module's `server.addrs` needs one too.

- A user an admin adds has their display name checked as one they set
  themselves is: at most 64 characters, no line breaks or other control
  characters. An admin could add a multi-line or kilobytes-long name
  that "Who's watching" then showed.

- An admin resets a user's password to a one-time one, shown once (the
  web UI's Users page, Reset password › One-time password; the admin
  API's `userResetPassword`; or `spoolframe-server user reset-password
  <user>`). It works for 7 days.
  Signing in with it gives a session that may only choose a new
  password (or sign out), in the apps (`change_password`, a new request;
  `logged_in.password_change_required`) and on the web, which opens
  "Choose a new password" after such a sign-in. The reset ends
  only what rested on the old password: sessions now record how they
  were signed in (a password, a passkey, a sign-on provider or a
  portable identity's credential), and a session a device approval or a
  trusted device's resume made carries its parent's. The reset ends the
  password ones and the devices trusted from them. Sessions made before
  this release have no record of it, and a reset treats them as a
  password's: they end too. A user can now change their own password
  from an app as well (`change_password`). `user list` and the Users
  page show a pending one-time password.

- Two library or Plex server edits at once no longer lose one: an edit
  made from settings that changed meanwhile is refused with "changed
  meanwhile; look again and repeat the change". Saving a setting no
  longer holds up everything that reads the settings while the file is
  written.

- Posters and other artwork already in the cache are served without a
  Plex request each: whether the server serves one is remembered for
  five minutes, and forgotten at once when a library is added, removed
  or changed.

- A subtitle still being read as text no longer holds up its playback:
  seeks, subtitle changes, stops and progress reports on it were
  answered only after each subtitle read's wait, up to 10 s each poll.

- A converted film whose file ends before its playlist does (a duration
  overstated, a file cut short) ends playback there with "the film ends
  before this segment", instead of the app asking for the missing
  segment again and again.

- A Plex setting the server cannot use (a token file it cannot read,
  say) is refused with the reason when it is changed, as it is at start,
  instead of being saved while the catalog keeps the old one. A config
  reload that names one is refused the same way.

- `network.wan_prefixes` (Settings › Remote access, "Always outside")
  lists address ranges whose devices are off the LAN for the uplink
  budget and Auto quality even at a private address: a WireGuard peer
  known to be always away, as `10.10.0.7/32`. A device at a private,
  carrier-grade NAT or unique local address (a WireGuard or Tailscale
  peer too) is still on the LAN otherwise. Between a range there and
  one in `network.lan_prefixes` that both hold an address, the longer
  prefix decides, `wan_prefixes` on a tie.

- Music and other files with no picture are held to the bitrate cap
  (the app's own, a chosen quality, the link's share and the uplink
  share off the LAN): one over it has its audio converted to fit
  instead of being sent at its full rate.

- A subtitle track whose text could not be made is remembered as failed
  until a restart: asking again answers at once with the reason instead
  of running the extract, decode and OCR again on the one OCR turn.

- A subtitle or quality change stops the conversion it replaces at
  once, instead of when a read still waiting on it gives up: the old
  ffmpeg no longer holds an encode slot beside the new one.

- A converted film with picture subtitles burned in plays when nothing
  else changes its picture (a 1080p Blu-ray to HEVC or AV1): its filter
  graph ended in `overlaynull`, which is no filter, and ffmpeg refused
  it, in software too.

- P7-08: a fresh install with only a security key recovers its identity
  from a server that knows it, without knowing the identity's id. The key
  gives back the genesis handle its credential carries; the new pre-auth
  request `identity_record_by_handle` finds the record by it (a key added
  to a phrase identity carries the identity's id, which finds it too),
  and `Client::recover_with_key` asks the key, fetches, checks the key is
  one of the identity's roots now, and imports. `SdkError::NotARoot` now
  means only "this key (or root) is not one of the identity's": a key
  holding none of the identity's credentials in a root action is
  `NotARoot`, not `SecurityKey(NoCredential)`, and a record, state or
  credential from elsewhere signed by no root of its identity is
  `IdentityState`/`Credential` `BadSignature` (or `InvalidInput` for a
  genesis of another identity), not `NotARoot`. Wire revision 14,
  additive; migration 0025 adds the handle column.

- Dead sessions (revoked or expired) are purged after 30 days, every
  6 hours; each device's newest one stays, so a user's devices and when
  they were last seen read as before. Sessions are looked up by device
  through an index (migration 0024), where every relay admission and
  every minute's check of each device read the whole table.

- Every database transaction that writes begins IMMEDIATE. One that
  read first, while another connection committed, failed its write with
  "database is locked" (SQLite's stale-snapshot error, which waiting
  does not retry): an admin's password change, disable or trust change
  racing a progress report or a session renewal.

- SDK: security key roots (P7-08). A `SecurityKey` (libfido2 on Linux,
  Core NFC on the iPhone, `SoftSecurityKey` in software) is the root of
  an identity made with `Portable::create_with_keys` (one key in hand,
  others as backups), or added to a phrase identity (`add_root`, the id
  kept). With a key (`RootSigner::Key`): `pair_self` (also renewal),
  `issue_credential_by`, `revoke_by`, `add_root`, `remove_root`;
  `renewal_due` says when a credential is in its last 14 days. An
  identity's record (genesis and newest state) exports and imports, or
  comes from a server that knows it (`Client::fetch_identity_record`),
  and any one key acts for it there. A device pairs through a server by
  a 10-character code (`offer_relay_pairing`, `collect_relay_pairing`,
  `accept_relay_pairing`; the root terminal `fetch_relay_pairing`,
  `answer_relay_pairing`), for a root terminal with no camera.

- The server checks security key roots (P7-08): a credential, a state
  or a tie counts only from a root the identity has now, worked out from
  its genesis (carried, or kept once the identity is known here) and
  every root-changing state kept (at most 32 per identity). A server
  answers `identity_record` with what it keeps of an identity it knows,
  for recovery, and relays a pairing for a root terminal with no camera:
  a device holds its request under a 10-character code for 5 minutes,
  the terminal fetches it (5 tries) and answers once for the offered key,
  and the device collects the answer. Codes are tried on the strangers'
  budget.

- Portable identities can have security key roots (P7-08, protocol): a
  FIDO2 key's P-256 credential signs CTAP2 assertions under
  `portable.spoolframe.app`, with touch and PIN, over the same bytes a
  phrase root signs. An identity begins with a genesis of 1 to 8 roots,
  each signing it; one of a single phrase root keeps its id (its public
  key), any other's id is derived from the genesis. States list the
  roots added (each proving it is held, endorsed by its adder with the
  time) and removed, cumulatively, and a verifier works out the roots
  from all it holds: a removed root never comes back, and what it adds
  after its removal does not count. Credentials, states and tie proofs name
  the root that signed them. Additive: revision 14, and a phrase
  identity's credential and state read as before.

- SDK: a playback's share of its server's uplink reaches the app with
  its bitrate (PX-46). `Client::playback_shares()` gives each
  `PlaybackShare { handle, max_bitrate }` as the server tells it, and
  `ManagedPlayback::share()` the last one for that playback; the SDK
  used to drop the bitrate and keep the event to itself.

- Where a run of a remux ladder's encoded rung stops cutting at the
  copy's keyframes is worked out once, when the run starts. Every
  segment read rebuilt the run's list of keyframes to force, up to
  120 KiB, to find it.

- An HLS playlist states each variant at the size its encode writes. AV1
  on VA-API writes 64×16 sizes only, and its rungs are scaled to one
  (1296×540 written 1280×528), but the playlist still said the size
  asked for. The playlist and the admin playback detail now take it from
  the same function the filter graph does.

## 0.6.3 (2026-10-11)

- SDK: an HLS segment or a converted chunk can be read as it arrives.
  `ManagedPlayback::segment_stream` and `chunk_stream` (and
  `Session::read_segment_stream`, `read_chunk_stream`) answer first with
  the read's length, the segment's size and whether it is whole, then
  hand on the body piece by piece, so a bridge gives its player the
  first bytes of a segment while the rest is on the link instead of
  after the whole read. The link meter and the step-down count it as
  they count a read; a resume while a segment is arriving ends it with
  `PlaybackResumed`, as reading on from part-way does.

- SDK: the link meter has a reading on a slow link. It needed 4 MiB
  inside its 10 s window, which a 1 Mbit/s link never carries (1 MiB
  segments, or the apps' 512 KiB reads, one after another), so the Auto
  step-down of a single-rung conversion never fired there. Two seconds
  of the link busy are enough now. A converted read is metered whatever
  its length, timed from its body's first chunk (which is not counted,
  as it came with the response), where a body under 512 KiB was not
  metered at all.

- A remux ladder's encodes reach the 1 Mbit/s floor whatever the
  copy's rate. Three encodes were cut from a ladder of four, so a fast
  copy lost its lowest: 32 Mbit/s gave 16, 6.35 and 2.52 Mbit/s, and a
  slow link had nothing under 2.5. They are now spread from half the
  copy's rate down to the floor (16, 4 and 1 Mbit/s).

- A remux ladder's encoded rung keeps to the playlist to the film's
  end. A run's list of keyframes to force stops at what one argument
  holds (120 KiB). Past its last, the run cut where its encoder keyed,
  and segments there held other times than their numbers said: from
  about 1:55:50 in a two-hour film keyed every 500 ms. A segment past a
  run's last forced keyframe now starts a new run there, with a list
  of its own, as a switch does.

- A software HEVC encode (libx265) adds no keyframes of its own and
  writes IDR at the forced ones. It ignored ffmpeg's `-sc_threshold`
  and keyed every scene change, each one an extra HLS segment off the
  playlist. Its forced keyframes were open-GOP CRA pictures, which a
  player starting there after a switch or a seek could not decode.
  libx265 now gets `scenecut=0:open-gop=0`, and libsvtav1 `scd=0`, in
  their own parameters.

- The server log has a line for each rung switch of an HLS ladder, and
  for each run restarted for a segment it does not have, under
  `spoolframe_server::playback`. It gives the playback, the rung from
  and to, what the player asked for, and where the new run starts.
  It says whether the init came from the kept copy, the run that left
  that rung, or a new run, and for a new run how long its init and
  first segment took to be written (`ready_ms`).

- An AV1 encode on VA-API is written at a size its encoder codes as it
  is: a width of 64s and a height of 16s (1296x540 becomes 1280x528).
  AMD's encoder coded any other size padded (1344x544, and 1920x1082
  for 1080p) without saying so in the track. AVPlayer then met a
  sequence header in every key frame that its decoder setup did not
  have, and a switch to such a rung played the sound over a black or
  frozen picture for seconds.

- A player switching to another rung of a ladder gets its init segment
  from one run, not two. The run for a rung's init started past what
  the old rung had written. The player's next segment was behind that,
  so a second run started for it: two ffmpeg starts before a picture.
  For a 4K HDR film on nimble that was 8.5 to 11.4 s tone-mapped to
  H.264 with libplacebo, where an iPhone gives up on an init after 6 s.
  That run now starts at the segment after the last one the player
  read: 4.6 to 6.2 s, and 0.7 to 0.9 s for AV1 keeping HDR. A rung's init is also kept from the first
  run that wrote it, so a player coming back to that rung has it at
  once.

- A converted film resumed partway through is cut where its playlist
  says. Each segment a resumed encode wrote was about a second off the
  playlist's grid (American Sniper on VA-API), or a whole segment off
  for a film keyed every 10 s, and an iPhone switching quality there
  played the sound over a black picture or jumped back. The HLS muxer
  measures its cuts from the first packet it gets: the copied audio,
  which began at the source's keyframe before the seek, up to a GOP
  early. So it cut early, at whatever keyframe came next, including
  the encoder's own (VA-API adds one every 120 frames). Now an HLS
  encode keys only where it is forced (a long GOP, no scene cuts), and
  a resumed encode's copied audio starts at the seek.

- SDK: `Client::reachability(endpoint_id)`, how this device stands with a
  server, for an app's "can't be reached" details: the addresses, relays
  and lookup it keeps, whether relays are on for it as the dialler
  applies them, what each address lookup (mDNS, each pkarr relay) last
  answered about it, its last dial in this run (when, how long, the
  stage it stopped at: no address, connect, timeout, handshake, closed,
  Hello, refused; the error, the addresses dialled, and every path iroh
  knew of then), and the paths of the connection open now with their
  round trips. Plain serde structs. Iroh does not say how each path of a
  failed dial fared, so a failed dial lists the paths known, not a
  result for each.

- SDK: a saved server this device reaches through relays, yet knows no
  relay or lookup for, is looked up at n0 (dns.iroh.link) too. A server
  saved while its relays and discovery were off, which turned them on
  since, was unreachable off the LAN until the device next reached it
  on the LAN and heard its new relays (a tester's iPhone on 5G).

- Off the LAN, an encode is an HLS ladder again. The remux ladder left
  an encode off the LAN as one rung with nowhere lower to go: American
  Sniper on 5G played AV1 800p alone at a 10 Mbit/s share while the
  link fell to 2.5 Mbit/s, and stalled.

- Single sign-on settings keep an issuer's trailing slash. The web UI
  dropped it on save, and a provider whose issuer ends in `/`
  (Authentik's) then failed every sign-in with an unexpected issuer: an
  ID token's `iss` has to match the configured issuer exactly.

- Off the LAN in Auto, a remux is the top of an HLS ladder (PX-21): the
  copied original, then encodes under it from half its rate down, so
  AVPlayer starts low and climbs to the original while the link carries
  it, where one copied variant had nowhere lower to go (the 9.4 Mbit/s
  remux that never started on cellular). Every variant has the copy's
  segments: an encoded rung forces a keyframe at each of the source's
  keyframes and adds none of its own (no scene cuts, a 1000-frame GOP).
  Only the rung being read is encoded, as for any ladder, and a rung
  counts against the encode budget when it runs. The copy's rung counts
  at its peak when choosing where to start. On the LAN, and for Original
  or a chosen bitrate, a copy plays alone as before. `Rung` gains
  `video`, an encoded rung's own (lenient, `None` from an older server).

- Off the LAN in Auto, a copy (direct play or a remux) has to fit its
  peak under the cap, not only its average (PX-46): half again over the
  average, the rule an HLS variant's `BANDWIDTH` already used. A 9.4
  Mbit/s H.264 rip under a 10 Mbit/s share now converts instead of
  remuxing at peaks near 14 Mbit/s that the link could not carry. On the
  LAN, and for a quality the viewer chose, the average still decides.

- `network.congestion_control`: `"cubic"` (the default, as before) or
  `"bbr"`, how the server's QUIC paths pace what they send. Cubic halves
  its window on every lost packet, which on a lossy cellular link held a
  window of 20 to 30 KB at 80 ms, a few Mbit/s; BBR (noq's BBRv3) paces
  by the rate and round trip it measures. One choice for every
  connection, home ones too: noq builds each path's controller knowing
  neither its address nor whether it crosses the WAN. Applies after a
  restart; logged at start; in the web UI under Settings › Remote
  access with advanced settings shown, and the NixOS module's
  `network.congestionControl`.

- The SDK measures its link through HLS too (PX-46): HLS segments and
  progressive chunks are metered from their first byte, so the encode's
  wait is not counted, and a managed HLS playback in Auto steps down for
  a link slower than its segments need, as direct play did. A managed
  playback follows `Event::PlaybackShare` by itself.
  `SdkError::WanStreamsFull { max }` is new.

- Settings › Remote access has an upload card (PX-46): the budget, the
  most playbacks outside at once and the admins' share, and *Measure the
  upload*, which times three 10 MB uploads to Cloudflare's public speed
  test (`admin.uplinkMeasure`) and offers 80% of their median as the
  budget. With advanced settings shown, *Also home* edits
  `network.lan_prefixes`. The NixOS module has `lanPrefixes` and
  `uplink.{budgetMbps,maxStreams,ownerSharePercent}`.

- PX-46, the WAN uplink budget: a playback off the LAN with Auto
  quality is planned under its share of `network.uplink.budget_mbps`
  (0, the default, is no limit), split equally between the playbacks
  off the LAN, the admins' together holding
  `network.uplink.owner_share_percent` (50) while they play beside
  anyone else. `network.uplink.max_streams` refuses one more with
  `WanStreamsFull { max }`. As playbacks start and end, the others are
  told their new share (`Event::PlaybackShare`, for a move of 15% or
  more, after a few seconds' settling) and re-plan. The LAN is now told
  by the server's own subnets: a phone on the home's IPv6 /64 is `lan`,
  one on 5G `direct`, and `network.lan_prefixes` adds ranges (a
  WireGuard network). On the LAN, Auto plays the original as before.

- The link to a playing device, in the log and the web UI: every 10 s
  while a device has a playback open, a `link` line
  (`spoolframe_server::link`) gives its connection's path, round trip,
  congestion window, MTU, open paths, the rate sent and received, packets
  lost and the share of them, congestion events, and the path changes in
  those 10 s. When a playback ends, a `playback link` line sums it up
  beside "a playback ended": seconds, the average rate, the largest
  round trip, the loss, the congestion events and path changes, and the
  seconds on a LAN, direct and relayed path. Server › Playback's detail
  shows the same in a Link card (`PlaybackRow.link` in the admin API,
  left out by an older server). The "connection path" line logs the
  first change, then at most one every 10 s that counts the changes
  since and the paths they went between: a phone flipping between two of
  its cellular IPv6 addresses wrote several lines a second.

- Relays are a choice per server in the SDK. `ClientOptions::relays`
  (on by default) is the default; a saved server may choose its own
  (`ServerEntry::use_relays`, `Client::set_server_relays`): `Some(true)`
  reaches it through its relays even with the client's off,
  `Some(false)` never, `None` follows the client's. Its relays are an
  editable list (`ServerEntry::relays`, `relays_edited`,
  `Client::set_server_relay_urls`): what its `ServerAddresses` event and
  an invite's link name, until the user edits it; empty gives it back.
  Acting on an invite whose link names relays is consent to them: a
  device with relays off connects, redeems (P6-08's token included) and
  saves through them, and the saved server chooses them (`Some(true)`).
  The endpoint now always has relays as a way to dial, with an empty
  relay map (no home relay, nothing contacted at start, never probed);
  "off" is kept per server by dialling it with no relay URL and by
  dropping the relay URLs from address lookups' records of it.

- A playback is planned for the screen as it is now (PGPU-06):
  `playback_plan`, `start_playback`, `change_subtitle` and
  `change_quality` take an optional `display` (`DisplayCapabilities {
  bit_depth, hdr }`) over the HDR formats and bit depth the app gave at
  `Hello`, and the playback keeps it for its re-plans. An Apple TV
  whose display mode changes while the app runs (Match Dynamic Range)
  no longer has HDR sent to an SDR mode, or a tone map for an HDR one,
  until the app is opened again. SDK: `PlaybackOptions::display`, and
  `Session::playback_plan_on` (`PlaybackOptions` is no longer `Copy`).
  Wire revision 14 still: a request without it plans as before.
- Each wire field added since 0.6.0 that a peer may leave out is
  marked `compat(v1): required at the protocol reset`, for PX-16's
  reset to revision 1: `ClientCapabilities.hardware_video`, the
  `display` fields, `ConvertedVideo::Copy.dolby_vision`,
  `MediaTracks.bit_rate`, `VideoTrack.bit_rate`, `Episode.year`,
  `Episode.air_date`, and the pairing `Credential` frame's `servers`.

- A relay on the server's own side of the NAT is no longer asked what
  address it sees the server at (QUIC address discovery): one whose name
  resolves to a private, loopback, link-local or CGNAT address, to one of
  the host's own, or to the server's public address (the router's
  hairpin). From inside the NAT it saw a private address where n0 saw the
  public one, and iroh warned every report that "IPv4 address detected by
  QAD varies by destination". The relays are placed again as the
  server's addresses change, and every 10 minutes; one at a public
  address not yet placed is left unasked beside n0 and asked alone. It
  still carries connections. A relay outside (a VPS) is asked as before.

## 0.6.2 (2026-10-10)

- PGPU-06, Dolby Vision: the probe reads the DOVI record's profile,
  level and base-layer compatibility. A Dolby Vision film for a device
  that shows its base layer's HDR (HDR10, HLG) but not Dolby Vision is
  copied as that base layer, no longer converted and tone mapped; for
  one that shows Dolby Vision, profiles 5 and 8 are copied labelled so
  (`ConvertedVideo::Copy.dolby_vision`, optional): muxed with
  `-strict unofficial` so the init segment carries `dvvC` (checked on
  profile 8.1), profile 8 tagged `hvc1` with
  `SUPPLEMENTAL-CODECS="dvh1.08.LL/db1p"` (`db4h` HLG), profile 5 tagged
  and named `dvh1.05.LL`. Profile 7 is always its HDR10 base layer, its
  enhancement layer left out of an HLS copy. Profile 5, which has no base
  layer, is converted through libplacebo applying its RPU: to HDR10 in
  10 bits for a device that shows it, or tone mapped, on a card that
  runs libplacebo whatever `transcode.tone_map` says.
- PGPU-06 in the web UI: Settings › Playback has Output codecs, a list
  of AV1, HEVC and H.264 to drag into order (by a pointer, a finger, the
  arrow keys on a row's grip, or its up and down buttons), each ticked
  to be used, saying which GPU here encodes it. The Server page's GPUs
  card says what a 4K HDR10 film becomes for an HDR screen that decodes
  AV1, one that does not (Apple TV 4K) and an SDR screen, and on which
  card (admin API `TranscodingStatus.picks`); its routes table has a
  row with HDR kept (`RouteJob::KeepHdr`), and a route whose cards are
  all full ends in "next codec" where a playback takes the next codec,
  software encoding H.264 alone.
- PGPU-06, what a kept-HDR encode writes: 10 bits on every path (VA-API
  scales its decoded frames in `p010` and uploads `p010` from memory;
  CUDA keeps the depth) on an encoder that took a 10-bit test frame, 8
  bits on any other; HEVC as Main 10; BT.2020 and the source's transfer
  said in the stream. HDR10's mastering display and light level metadata
  come through VA-API and NVENC into the fMP4 init segment (`colr`,
  `mdcv`, `clli`), checked on an RX 7900 XTX and a GTX 1060; QSV is not
  checked. The HLS playlist names what is written: Main 10 or AV1 10-bit
  (`av01.0.<level>M.10`, the level by height and frame rate) and PQ or
  HLG for kept HDR, Main or 8-bit with SDR for a tone map. Pictures
  burned into a 10-bit output on VA-API are laid over on the CPU, as on
  NVIDIA.
- PGPU-06, the GPU's output codecs: `transcode.video_codecs` orders the
  codecs a GPU encodes to, `["av1", "hevc", "h264"]` if left out. A
  conversion takes the first the client plays (and decodes in
  hardware, where it says: `ClientCapabilities.hardware_video`, new and
  optional) that a card with room encodes; with none, it is encoded in
  software, which is now H.264 only: libx265 and SVT-AV1 are no longer
  tried or used, a spill to software included. An HDR film for a
  screen that shows its HDR, with 10 bits allowed, keeps it in HEVC or
  AV1 on a card whose encoder took a 10-bit test frame (detection tries
  one per GPU HEVC and AV1 encoder) rather than going to H.264 tone
  mapped: on an RX 7900 XTX 16× real time and 0.4 s of CPU for 30 s of
  4K HDR10 against 2.0× and 9.2 s with libplacebo's tone map. Dolby
  Vision keeps its base layer's HDR10 or HLG; profile 5 has none, and is
  tone mapped. The NixOS module has `transcode.videoCodecs`.
- A conversion whose GPUs are all full is encoded in software instead
  of refused as busy: it decodes (and filters, where it can) on the
  first card that decodes the source and draws on the CPU's budget,
  `transcode.max_jobs`; only with that full too is it busy. The GPUs
  card's routes end in "CPU" accordingly, and in "busy" only where
  software cannot encode the codec either.
- `transcode.tone_map = "auto"` (the default) tone maps HDR with
  libplacebo on the job's GPU whatever `transcode.gpu_filters` says; it
  used to stay on the CPU while GPU filters were off. With GPU filters
  on, the GPU driver's own tone map (Intel's `tonemap_vaapi`) still
  comes first. A 4K HDR film to 1080p SDR costs about 2 s of CPU per
  20 s of film on a GTX 1060, against about 120 s on the CPU. `"cpu"`
  keeps the old picture where it matters.
- A session's renewal is written once when requests arrive together:
  each found it due and wrote it, one after another, so a slow disk held
  every one of them (four web UI requests waited 2.3 s). Now the first
  writes and the rest find it written, for web, device and portable
  sessions alike.
- The server's own relay serves TLS 1.3 only (glm L-3), from its PEM
  files and from ACME alike; rustls' defaults had it offer 1.2 as well.
  Every iroh client speaks 1.3; a client of 1.2 alone is now refused.
- The server's own relay (P6-04) cuts a device it carries once that
  device is no longer let through (glm L-1). It asks the main server
  again about each endpoint it carries every 5 s, so a device signed out,
  a session ended, or an invite or grant revoked stops being relayed
  within about 10 s; before, the gate ran only when a connection opened,
  and iroh keeps one relay connection per endpoint, so such a device
  kept relaying until it happened to reconnect. The 60 s answer cache now
  bounds only whether a connection is let through. The wire is
  unchanged.
- The GPUs card moves from the Overview to the Server page, under "This
  server", and says more plainly where conversions go. Each cell of its
  table is the order the server's pick tries the cards in: the first
  choice, then each next while the ones before are full (the pick asked
  again with them counted full), ending in "busy" where a GPU encode is
  then refused, since it never falls back to the CPU; a next card that
  does less of the job says so. A tone map or burn-in on the CPU says
  why, from the server: GPU filters being off (with a pointer to
  Settings › Playback), a software encode, a CPU decode, or the notes a
  job logs. What a card lacks names a filter as its API's own ("VA-API
  tone map (tonemap_vaapi)"), so it no longer reads as contradicting
  "Tone maps: libplacebo (own)". In the admin API, a route's card and
  share move into `steps`, beside `busyAfter`; each step has `cpuWhy`.

## 0.6.1 (2026-10-10)

- The web UI's Overview shows the GPUs (PGPU-05): a card for each GPU
  encodes run on, as the server's start lines say it (its API, render
  node or CUDA index, discrete or integrated and how that was known, the
  `transcode.gpus` entry that took it, its GPU jobs now and its limit,
  what it encodes, decodes, filters and tone maps with, and what it
  lacks); the host's other GPUs and why they do nothing (no entry takes
  them, or none of their tests passed); and the entries that took no
  card. Under them, where typical conversions run now: H.264, HEVC and
  AV1 sources to each of H.264, HEVC and AV1, an HDR10 film tone mapped
  to SDR, and a film with picture subtitles burned in, each cell the
  card the jobs' own pick chooses and how much of the job it does
  there. In the admin API, `serverStatus`'s `transcoding` gains `gpus`,
  `otherGpus`, `gpuNotes` and `routes`.
- P7-03, guests on a shared device: a device-local "guest sign-ins"
  setting in the SDK, off by default, and pairing by QR code. A TV
  with it on shows a code; the guest's phone scans it, shows "Authorize
  <TV>?" with the TV's key fingerprint, and its root signs a device
  credential that goes back over a direct Iroh connection on the new
  ALPN `spoolframe/pair/1`. No server takes part, and the client
  protocol is unchanged. With the setting off the device shows no code
  and takes no pairing; each refusal is logged as the server logs its
  own. Guests are listed and signed out one by one. The TV learns
  which identity is pairing, and the fingerprint to show beside the
  phone's, as soon as the phone connects (`Pairing::connected`), before
  the person authorizes.
- P7-06, signing in with a portable identity (wire revision 14): a
  device with a credential signs in to a server by signing the server's
  nonce with its key (`begin_portable_sign_in`, then
  `finish_portable_sign_in` with the credential, the proof and the
  identity states the device has). Local-first: an identity tied to an
  account here signs in as that account; an untied one with a grant
  here as itself, a portable session that browses, plays and keeps
  watch state of its own, and gets `not_for_portable_users` for what
  only an account here has (password, PIN, passkeys, devices,
  sessions, ties, approving sign-ins). Every refusal is
  `invalid_credentials`, counted as a wrong guess. Grants are not
  users: the users list never shows them; the admin API lists and
  revokes them (`grantList`, `grantRevoke`), as do
  `spoolframe-server grant list|revoke` and the web UI's Invites page
  ("Guests with their own identity"). Revoking a grant ends its
  sessions at once. A device signed in only as a portable user is a
  friend's on the server's relay, not the owner's.
- P7-05, inviting a portable identity: `inviteCreate` (and `invite
  create --kind portable-account`, and the web UI's "For: A guest with
  their own identity") makes a `portable-account` invite. Its link says
  `kind=portable-account`. The guest's phone redeems it with its own
  device credential (`redeem_portable_invite`): the server grants the
  identity access, makes no account, and signs it in. A code redeemed
  the other way is `wrong_invite_kind`, and keeps its use. `federation`
  invites stay refused until P8.
- The SDK side of P7-05 and P7-06: `Client::redeem_invite_portable`
  joins a server with a guest invite as a portable identity (the root's
  phone signs itself a credential, `Portable::own_credential`);
  `Client::sign_in_portable` signs in with this device's credential,
  kept as a profile (`SignInMethod::Portable`, `SavedProfile::portable`)
  that signs in again with no password. Pairing hands the TV the
  servers the phone signs in to as the identity (`Paired::servers`),
  pinned on the TV before `Pairing::wait` returns. `InviteInfo::kind`
  says which redemption a link wants; `SdkError::WrongInviteKind` and
  `SdkError::NotForPortableUsers` are new.
- P7-05/P7-06 after review, the server:
  - A session a credential opened (a guest's, or a tied account's)
    ends when the credential does, and at once when an identity state
    revoking its key arrives; it is never renewed past the credential's
    expiry. Its key is checked on every use too.
  - A tied account's session opened with a credential counts as signed
    in long ago: adding a passkey, approving a device, trusting one,
    removing a passkey and tying or untying ask to confirm first.
  - A revoked guest is kept as a tombstone: the invite that brought
    them, or any made before the revocation, cannot bring them back
    (`invite_invalid`, its use kept). The grants list shows live ones.
  - A sign-in carries at most 2 identity states, each the credential's
    own identity's; the credential is checked before them, and they are
    kept only for an identity tied or granted here.
  - A tied identity redeeming a guest invite signs in as its account;
    no grant is made and no use taken. A redemption whose credential
    holds is not counted as a guess whatever the code. Racing
    redemptions of one invite no longer fail as internal errors.
  - A guest's session gives its device the normal connection limits,
    not a local user's standing with the strangers' sign-in budget.
- P7-06 after review, the SDK: a phone hands a paired TV only the
  servers it has verified; the TV pins them unverified (a password
  sign-in there shows "Not verified"), never changes a pin it had, and
  `sign_in_portable` works on such a pin.
- An episode says when it first aired, for the apps' "2019": `Episode`
  has `year` and `air_date` (`"YYYY-MM-DD"`, `spoolframe_types::
  CalendarDate`), from Plex's `year` and `originallyAvailableAt`; with
  only the date, the year is the date's. Both are `null` where Plex has
  neither. Both are optional on the wire: an episode from an older
  server, without the keys, reads as `null`, and `PROTOCOL_VERSION`
  stays. Items come from Plex on each request, so existing libraries
  have them at once, without a rescan. The web UI's item page shows
  "Aired", and its Year column covers episodes.
- A file's bit rate, for the apps' "4K · Dolby Vision · 42 Mbit/s":
  `MediaTracks` has `bit_rate`, the whole file's average in bits per
  second, and `VideoTrack` has `bit_rate`, the picture's alone. The
  file's is what the container says, else its size over its length; the
  picture's is what an MP4 says of the stream, else a Matroska muxer's
  `BPS` tag, else `null`. Both are optional on the wire as above, and
  `PROTOCOL_VERSION` stays. Files are probed when asked about, so
  existing libraries have them at once, without a rescan. The
  playback planner's bit rate check now also covers a file whose
  container states no rate.
- PX-44, invites made in the apps: an admin's session makes, lists
  and revokes invites over the client protocol as the web UI's Invites
  page does, with the same kinds, limits and store (`create_invite`,
  `list_invites`, `revoke_invite`; `Session::create_invite`,
  `Session::invites`, `Session::revoke_invite` in the SDK). Making and
  revoking need a recent confirmation (`recent_sign_in_required`, then
  `confirm_password` or a passkey); anyone but an admin is the new
  `admin_required`. Each invite now says where it was made, the web UI,
  the CLI or an app's device (`created_from`), and who by, with their
  display name; those made before are the web UI's, or the CLI's.
  Additions to wire revision 14, not a new one.

## 0.6.0 (2026-10-09)

- An invite's holder on your relay (P6-08): a device redeeming an
  invite relays as the invite's kind says before it has an account. An
  invite that makes an account here, admin or consumer, is an owner's;
  a portable user's (P7) or a server's (P8) a friend's. The SDK reaches
  the link's relays with a token made from the code and its own key,
  until the invite is redeemed. A token that matches no live invite
  counts as a wrong invite code. The relay-access question gains an
  optional `invite`; its ALPN stays `spoolframe/relay-access/1`, and an
  older server ignores it.
- PX-22, the server side:
  - Any string setting may name an environment variable as
    `${SF_ENV_NAME}`. Only `SF_ENV_` variables are read; an unset one
    stops the server (or refuses the change) naming it. `config show`,
    the settings API and runtime.ucl keep the reference.
  - `auth.oidc[].client_secret`, read after `client_secret_file` and
    before the secret pasted in the web UI. The runtime layer takes only
    a `${SF_ENV_…}` reference; a provided file's literal one shows as
    `(hidden)`.
  - The settings API's restart-pending entries carry `running`, what
    the server runs with until it restarts.
- PX-22, the web UI:
  - Every network setting: the app and Plex discovery ports on the
    Server tab; the relays, relay keys, pkarr record and the addresses
    devices are told on Remote access. Lists are edited one entry a
    row.
  - Settings › Sign-in providers: SSO providers added, changed and
    removed. The client secret from an environment variable (saved as
    `${SF_ENV_<NAME>}`, recommended), pasted (kept unencrypted on the
    server's disk, as the form says before the field), or from a file.
  - Playback: when frame previews are made, how often, their cache and
    the artwork cache (in GiB), and when an item counts as started and
    as watched. Security: the media tools and their sandbox.
  - A restart setting changed since the start shows what the server
    runs with now and after the restart.
  - "Show advanced settings", off by default and kept per browser, hides
    the settings few servers need; a tab with hidden ones set says how
    many.
- At least one enabled admin keeps a password: the last one with a
  password cannot be demoted, disabled or have it removed, setup cannot
  finish without one, and a server set up before says so at start.
- `passwordRemove` (a user's own) and `userRemovePassword` (an admin's),
  each after a recent sign-in, and never a user's last way to sign in.
  Their other sessions end and trusted devices are forgotten, as with a
  new password.
- The web UI's setup asks for the first admin's password, the passkey
  optional; My profile and a user's page remove a password, after
  saying what that ends.
- The web UI says when it is open over plain HTTP across the network:
  above the sign-in form, in a banner on every admin page, and at the
  setup page's insecure cookies switch. The password and the session
  can be read on the way; HTTPS or an SSH tunnel to localhost is the
  fix. The wizard's Access step names those first, insecure cookies
  last.
- Portable identities begin (P7-01): the client SDK makes a root
  signing key with a 24-word BIP-39 recovery phrase, recovers the same
  key and `PortableUserId` from the phrase on any device, and keeps
  several identities apart in the app's storage. The root stays on the
  device; the server and the protocol do not build the phrase code, and
  nothing on the wire changes yet.
- Portable device credentials (P7-02): the root signs a 90-day credential
  for each device's own key, and revokes keys with a signed, versioned
  identity state that stamps each revocation with its time. The
  protocol crate defines both, and a device's proof of its key, as fixed
  signed byte layouts (PROTOCOL.md "Portable identities"); the SDK
  issues, installs, renews and revokes them, and gives a device a fresh
  key once its old one is revoked. Revocation only grows: the server
  refuses a credential whose key any valid state it has seen revokes,
  older revisions included, and drops a revocation once no credential
  it could stop can still hold (90 days and 5 minutes). No request
  carries them yet (P7-06). The root's phone keeps the devices it
  issued to, under a label the user gives (`Portable::issued`: the
  newest credential per key, and whether it is revoked), and
  `Portable::list` says whether each identity's root is still there,
  which a restored backup that leaves out device-only secrets is not.
- Ties (P7-04, wire revision 13): a signed-in local account ties itself
  to a portable identity. The server gives the session a nonce
  (`TieBegin`, 5 minutes, used once); the phone signs it with the root,
  or a TV with its device key and credential, over the server's id, the
  nonce, the identity and the username (`TieFinish`), so the proof
  works nowhere else. One identity per account, one account per
  identity on a server; an account already tied severs first
  (`Untie`). Each needs a recent sign-in. The tie is a row beside the
  account: severing it leaves sessions, passkeys and watch state as
  they were. `WhoAmI` and the admin API's `UserInfo` carry it, and the
  web UI shows it as a badge on a user's page and in My profile, with
  Sever (`admin.userUntie`, `me.portableUntie`); tying is done from the
  phone app only. The SDK ties with `Session::tie` and severs with
  `Session::untie`. Signing in with a tied credential is P7-06.
- An encoded HLS playback's first segment is 2 s (PX-26): the grid is
  0, 2, 4, 8, 14 s… and the player has a segment two seconds into the
  encode, not six. A copied video is still cut at its keyframes, audio
  alone every 6 s. A run for one of the first three segments starts
  from 0. The forced keyframes count from the run's first frame, where
  ffmpeg's HLS muxer counts its cuts from, so they no longer drift off
  the grid (at 23.976 fps they fell 7 s behind over two hours). A
  capped encode's BANDWIDTH is now twice its cap: what a 2 s segment
  can hold.
- A remux over HLS starts without reading the whole file first (PX-25):
  a copied video's keyframes come from the file's own index, Matroska's
  Cues or MP4's sync sample table, read in a fraction of a second where
  every packet took a minute or two on a 4K remux. The index is used
  only after ffprobe finds exactly its keyframes in four intervals
  between them; a file without one, or with one that misses keyframes,
  is read whole as before. The reader is bounded (48 MiB read, 32 MiB of
  times) and has a fuzz target, `keyframe_index`.
- Frame previews made ahead also wait while a remux runs and while a
  converted playback is being prepared (its keyframes read), not only
  while an encode runs.
- Server › Playback says how each playback is made (PX-24), from the
  plan and the filter graph the server built: delivery (direct, remux,
  transcode), decoder (software or the GPU's API), video encoder with
  its codec, HDR tone mapping and subtitle burn-in (GPU, CPU or none),
  and the output's bitrate measured from the segments written beside
  the encoder's target, with its size. A playback's page also shows its
  start: request to plan, the keyframes' read (from the index or a full
  scan), and the first segment ready; the server logs the same once per
  start as `playback start timed`. The "playback started" line's
  `plan=` says `remux` or `transcode` where it said `converted`.
- An NVENC encode decodes on NVDEC (PGPU-01): `-hwaccel cuda`, the frames
  back in memory, so tone mapping, burn-in and scaling run on the CPU as
  before. Detection tests NVDEC per codec and depth (H.264, 8- and
  10-bit HEVC and AV1, VP9) where NVENC works, and a source it did not
  decode in a test, or one whose GPU decode fails, decodes in software.
  The transcoding line, the admin API and the Playback step's encoder
  list name what each GPU decodes (`hevc 10-bit (nvdec)`), and a
  playback's page says `GPU · NVDEC (CUDA)`.
- NVENC and NVDEC start under the media sandbox: CUDA names a thread
  through `/proc/self/task/<tid>/comm` and reads `/proc/self/maps`, and
  failed with `CUDA_ERROR_OPERATING_SYSTEM` when Landlock refused them.
  A run on the NVIDIA GPU may now write its own threads' names and read
  its own `maps`, and nothing else of `/proc`; other runs still have no
  `/proc` at all. A run on a GPU opens only that GPU's device nodes: a
  VA-API run no NVIDIA node, an NVIDIA run no render node.
- A failed test encode or decode at start logs what ffmpeg said last
  (`stderr=`): as a warning for a family `transcode.hardware` names, at
  debug for one only `auto` tried. Before, a GPU left out said nothing
  of why.
- A video encode runs on the one GPU that does the most of it (PGPU-02).
  With `transcode.hardware = ["nvenc", "vaapi"]` the encoder was the
  first family that encodes the output codec, and the source decoded on
  it or on the CPU. Now each family that encodes the codec is ranked:
  the whole job on it (decode, then tone map, picture burn-in and scale
  on the GPU), decode and encode with the filters on the CPU, or encode
  only; the best wins, the list's order between equals. A GTX 1060
  beside an AMD GPU sends an AV1 film to the AMD GPU, which decodes it,
  and a film both decode to NVIDIA unless the AMD GPU also runs its
  filters (VA-API scales on the GPU, NVDEC's frames scale on the CPU, so
  a downscale goes to VA-API). A job never decodes on one GPU and
  encodes on another. A software encode decodes on the first GPU that
  decodes the source, NVDEC too now. A run whose GPU decode fails runs
  again in software on the same encoder. Server › Playback's plan and
  "How it is made" name the encoder the run picked.
- A family `transcode.hardware` names that lacks a codec logs one line
  at info, `nvenc lacks: av1 encode, av1 decode, av1 10-bit decode`,
  rather than a warning per failed test at every start; each test's
  stderr is at debug. A named family with no working encoder still
  warns, now with its first failed test's stderr.
- With `transcode.gpu_filters` on, an NVIDIA job filters on the GPU too
  (PGPU-03): NVDEC's frames stay there (`-hwaccel_output_format cuda`),
  `scale_cuda` scales them (and makes a 10-bit source 8-bit for H.264),
  and picture subtitles are uploaded and burned in with `overlay_cuda`.
  NVIDIA tone maps on the CPU, so an HDR film, or one with text burned
  in, filters all on the CPU as before. Detection tries both filters
  with a short run where NVENC and NVDEC work; one that fails stays on
  the CPU and shows in the `nvenc lacks: …` line, and the transcoding
  line says GPU filters per family (`vaapi: tone map, overlay; nvenc:
  scale, overlay`). Such a job weighs as all on the GPU, the playback
  page says its burn-in ran there, and one whose CUDA filters fail runs
  again decoded in software with the CPU's filters. With GPU filters on,
  a 4K film made 1080p that both GPUs decode now goes to NVIDIA when it
  is first in `transcode.hardware`, and a picture burn-in to NVIDIA over
  an AMD GPU (Mesa has no `overlay_vaapi`).
- The server hardens its own process as it starts, for every
  subcommand (PX-31): it is not dumpable, so another process as the same
  user can no longer read its `/proc/<pid>/environ` (the `SF_ENV_`
  secrets) or memory, nor attach a debugger (root still can); it gains
  no privileges from a setuid program it runs; it dumps no core; and a
  seccomp deny-list refuses, with `EPERM`, the calls it never makes:
  `ptrace`, `process_vm_*`, `bpf`, `perf_event_open`, `userfaultfd`,
  `io_uring_*`, the mount calls, `unshare`, `setns`, a `clone` into a
  new namespace, keyrings, modules, `kexec`, `reboot`, swap, `acct`,
  `quotactl`, `open_by_handle_at`, a persona other than Linux's, and on
  x86_64 `iopl`/`ioperm`. Processes, threads, every socket family and
  the media tools' own confinement are unchanged; their filter stacks on
  top. A restart in place keeps the filter rather than load it twice.
- An HDR film is tone mapped on the GPU with libplacebo (PGPU-04).
  `transcode.tone_map` (live; Settings › Playback): `"auto"`, the
  default, takes the job's GPU's own tone map (`tonemap_vaapi`, Intel),
  else libplacebo on the job's GPU through Vulkan, else the CPU, and the
  CPU while `transcode.gpu_filters` is off, so a setup that tone mapped
  on the CPU still does after the upgrade; `"cpu"`, `"gpu"` and
  `"libplacebo"` force one, a job whose GPU lacks it tone mapping on the
  CPU (noted once), and one no GPU here runs is a warning at start.
  NVIDIA's libplacebo shares ffmpeg's Vulkan device: the film decoded on
  it (`-hwaccel vulkan`), tone mapped and scaled there, and downloaded in
  8 bits to NVENC (fulcrum's GTX 1060: 4.0× real time and 2 s of CPU
  against 2.0× and 120 s on the CPU). On AMD, libplacebo makes its own
  Vulkan device, the frames going through system memory: VA-API decodes
  into memory, libplacebo tone maps and scales, the result is uploaded
  to `h264_vaapi` (an RX 7900 XTX here, 20 s of 4K HDR10 confined:
  1.7× real time and 8.4 s of CPU against 1.4× and 181 s on the CPU).
  The run's jail opens only the job's render node, so RADV lists only
  that GPU and libplacebo's own device lands on it; unconfined, Mesa's
  device-select layer is told its PCI ids. A burn-in goes over
  libplacebo's output on the CPU. Detection tries libplacebo per family
  with a one-second confined run; the transcoding line says `GPU tone
  maps: vaapi: libplacebo (own); nvenc: libplacebo (shared)`, a failure
  shows in `… lacks: libplacebo tone map`, and `spoolframe status` and
  Settings › Playback list them. Placement counts a GPU tone map as on
  the GPU; a libplacebo job with nothing burned in weighs as all on the
  GPU; a libplacebo failure before the first output runs again decoded
  in software with the CPU's tone map; Server › Playback says which tone
  map ran (`libplacebo (shared)`, `libplacebo (own)`, `VA-API`, `CPU`).
- An `experimental { }` block, for switches that may change or go away
  without notice: the server warns of each one on at start, and the web
  UI shows them only with Show advanced settings on (Settings ›
  Playback › Experimental). The first is `libplacebo_shared_on_amd`
  (off): libplacebo on an AMD GPU uses ffmpeg's Vulkan device, derived
  from the VA-API one, falling back to its own at detection where that
  fails, as it does on RADV today.
- The package, the Docker images and the dev shell ship nixpkgs'
  ffmpeg-headless with libplacebo and a shader compiler (`nix build
  .#ffmpeg`): 330.5 MiB against 305.4 MiB.
- Docker images by GPU, sharing every layer but the drivers' (the
  server's closure is layered the same way in each): `:<version>`
  (`.#docker`, the default, 1.5 GiB) for any GPU or when unsure, with
  nixpkgs' whole Mesa (radeonsi, r600, iris, crocus, nouveau, virgl, d3d12
  for Docker Desktop on WSL2, zink, llvmpipe; RADV, ANV, hasvk, dzn,
  lavapipe) and Intel's VA-API drivers iHD and i965; `:<version>-slim`
  (`.#docker-slim`, 587 MiB) for a recent AMD or Intel GPU, with a Mesa
  trimmed to radeonsi, RADV and ANV (172 MiB, no LLVM) and iHD;
  `:<version>-nvidia` (`.#docker-nvidia`, 449 MiB) for NVIDIA, no Mesa;
  and `:<version>-cpu`, the `-nvidia` image under a second tag, for a
  machine with no GPU (a 4K HDR film to 1080p in software takes roughly 6
  cores per stream at real time). Before, the image had no AMD or Intel
  VA-API driver. NVIDIA's Vulkan driver works in every one (checked on
  fulcrum's GTX 1060 under CDI): the images carry libX11, libXext and
  glvnd's `libEGL.so.1`, which it loads and the toolkit does not mount,
  set `NVIDIA_DRIVER_CAPABILITIES=compute,video,utility,graphics`, find
  the toolkit's Vulkan manifest in `/usr/share/vulkan/icd.d`,
  `/etc/vulkan/icd.d` or a NixOS host's `/run/opengl-driver`, and its
  libraries in `/usr/lib64`, `/usr/lib/x86_64-linux-gnu` or `/usr/lib`.
- SIGHUP reloads the server's config (PX-33). The config file and
  `runtime.ucl` are read and checked again as at start; one that does
  not load changes nothing, and an ERROR says what is wrong and in which
  file. Live settings apply through the same path as a change in the web
  UI (the Plex catalog, watch rules, job limits, previews, sign-in,
  metrics, logs, telemetry); restart settings are pending, Settings
  showing "Now / After restart". A runtime value still wins over the
  file's unless the file is locked. Encoders are detected again, as
  SIGHUP did before, and the config's warnings are logged again. One
  line says what happened: `SIGHUP: config reloaded: applied name;
  after a restart: network.mdns; unchanged otherwise`, or `nothing
  changed`. A reload and a settings change never interleave, and
  reloads run one at a time. The admin API's `configReload` (a recent
  sign-in) and `spoolframe-server config reload` do the same; the NixOS
  module's unit has `ExecReload`, so `systemctl reload spoolframe` does
  too, and in Docker `docker kill -s HUP <container>`.
- A server uses every GPU it has, several of one API too (PGPU-05):
  `transcode.gpus` lists entries in order of preference, each an `api`
  (`nvenc`, `vaapi`, `qsv`) and either `pick` (`all`, the default, every
  card best first; `best`; `dgpu`; `igpu`) or one `device` (a CUDA index,
  or a render node, by-path links accepted), and `max_jobs` for its
  cards. Left out, every card of every API, NVIDIA first. Detection runs
  on each card, confined to it; a card's rank is how much detection found
  it does (encodes, decodes by depth, GPU filters), discrete before
  integrated among equals. Discrete or integrated comes from the kernel:
  NVIDIA is discrete, amdgpu says (its APU flag), Intel's iGPU is PCI
  00:02.0. Each conversion runs on the card that does the most of it,
  across entries, then in the entries' order and rank, skipping a card
  that runs its most jobs; `transcode.max_gpu_jobs` is now each card's
  limit, and the status sums them. A job's ffmpeg may open its own
  card's nodes only, and an NVIDIA job is shown its one card by CUDA
  (`CUDA_VISIBLE_DEVICES`, PCI order). The start's log has a line per
  card: name, node, discrete or integrated and how that was known, its
  entry, what it does and lacks. A host with one GPU runs as before.
  libplacebo's tone map is tested on each card, confined to it, and a
  card that runs it counts the tone map as on the GPU; a reload (SIGHUP,
  `configReload`) detects again on the cards found at start.
  Server › Settings › Playback edits `gpus` among the advanced settings;
  the NixOS module has `transcode.gpus`, and allows every DRM node for a
  VA-API pick (its cards are only known at start), a device's node alone
  otherwise.
- `transcode.hardware` and `transcode.vaapi_device` are deprecated and
  will be removed in a later release. They are still read, as the `gpus`
  they mean (each family every card of it; VA-API on `vaapi_device` alone
  when that is set); setting `gpus` too is refused. A warning at start,
  `config set`, `config show`, the settings API and the web UI each name
  the exact `transcode.gpus` to write instead; the web UI shows them only
  while they are set, with a Convert button for runtime settings. The
  NixOS options `transcode.hardware` and `transcode.vaapiDevice` are null
  by default now, deprecated, and warn with the `gpus` equivalent.

## 0.5.0 (2026-10-09)

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
- A copied HEVC stream in HLS has its parameter sets in `init.mp4`. Some
  Matroska files (4K WEB-DLs; about one HEVC file in twenty on one Plex
  library) keep VPS, SPS and PPS only in the stream, and their
  `CodecPrivate` is an `hvcC` record with none. ffmpeg copied that record
  into an 8-byte `hvcC` box, and an Apple TV refused the stream as an
  unsupported format (CoreMediaErrorDomain -19601), with no server error.
  The copy now goes through `hevc_mp4toannexb`, so the MP4 muxer reads
  the parameter sets from the first keyframe into a whole record (P2-05).
- A failed HLS segment read is a warning in the server's log. A client
  that hands HLS to a local player (the tvOS app and AVPlayer) showed a
  failed `ReadSegment` or `ReadSubtitleSegment` only to that player, so a
  playback could fail with nothing in the log. Each read answered with an
  error is now logged at WARN with the item's id, the endpoint, the
  segment number and the error; a WebVTT segment that is not ready yet is
  not (P2-05, P2-07).
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
- VobSub and DVB subtitles read as text (P3-08), as PGS is (P2-07):
  `spoolframe-media::vobsub` and `::dvb` parse a track's packets, which
  `ocr::read_track` takes from ffprobe (an embedded track, or a `.idx`
  with its `.sub`), into cues with their times and pictures. The pictures
  keep the text and drop its outline, whichever of them is the brighter.
  `text` now plans OCR for both formats instead of `Unsupported`, and
  `auto` prefers it to burning in; a track without recognition data for
  its language still burns in, or is `Unsupported` with `text`, with the
  reason. Two-letter language codes (a file's `Movie.en.idx`) find their
  tesseract data, and a bar read alone becomes the capital I it is. No
  protocol change.
- Layered configuration (P3-10): a server's settings are the built-in
  defaults (`spoolframe-server config defaults` prints them, every key with
  what it does), then the provided file (`--config` or
  `SPOOLFRAME_CONFIG`, now optional), then `runtime.ucl` in the state
  directory, which only the server writes (0600, rewritten whole). They
  merge key by key, later layers winning, except that a provided file with
  `locked = true;` locks every key it sets. `config show` prints every
  setting with the layer it comes from; secrets (a Plex token set at run
  time, kept in `secrets/plex-token`) only as set or not set. The state
  directory comes from `--state-dir`, `SPOOLFRAME_STATE_DIR`, systemd's
  `STATE_DIRECTORY`, `$XDG_STATE_HOME/spoolframe`, `/var/lib/spoolframe`
  as root, or `~/.local/state/spoolframe`. **Config break**: `state_dir`
  is no longer a key; a file that has it is refused with what to pass
  instead. The NixOS module passes `--state-dir` and locks its file.
- Not set up (P3-10): without `plex` the server runs, its web UI and
  admin API work, and every catalog and playback request is answered with
  the new protocol error `NotSetUp` (`SdkError::NotSetUp`), until Plex is
  configured. **Protocol change**: clients built on an older SDK see it as
  an unknown error. The admin listener listens on every IPv4 interface
  (`0.0.0.0:7421`) by default; `admin.allow_remote` is gone (refused with
  a message), and `admin.listen` still takes loopback. At start the log
  names every address the web UI answers on, and beside them the setup
  code while there are no users. `admin.insecure_cookies` (off by
  default) sends the session cookie without `Secure` and `HttpOnly`, so
  signing in over plain HTTP on the LAN works; the server warns at start
  while it is on, and `/auth/site` says so. The NixOS module's
  `plex.url` may be null, its `admin.listen` defaults to the server's,
  and `admin.allowRemote` is removed.
- Runtime settings (P3-10): the admin API's `admin.settings` lists every
  setting with its value, layer, lock, the file value a runtime one
  overrides, and whether it applies live or after a restart;
  `admin.settingSet` and `admin.settingUnset` change the runtime layer,
  checked by the parse a file goes through (a refused value gets the
  file's message; a locked key is `SETTING_LOCKED`). Live: `name` (Hello,
  the mDNS announcement and the status), `plex.url`, the token and
  `plex.path_mapping` (the catalog is rebuilt and swapped),
  `watch.*`, `transcode.max_jobs`, `scratch_limit_mb` and
  `read_ahead_secs` (for the next encode), `admin.insecure_cookies`.
  Everything else is saved and flagged `restartPending`; `admin.restart`
  exits with 75 for systemd to start the server again, where it runs
  under one. `plex.token` is kept in `secrets/plex-token`, never in a
  layer, a reply or the log. Each change is logged with the admin who
  made it. `spoolframe-server config set <key> <value>` (`--stdin` for a
  token) and `config unset <key>` do the same through the running
  server; `status` says "Plex: not set up" instead of failing.
- The setup wizard's server side (P3-10): `POST /auth/setup/check` checks
  the setup code before the first admin exists and, asked to, turns
  `admin.insecure_cookies` on with it as the authority; `admin.setupState`,
  `setupStepDone` and `setupFinish` keep the wizard's progress in the
  runtime layer (`setup { done; finished; }`; a server whose file already
  had Plex counts as set up). `admin.plexDiscover` finds Plex Media
  Servers with Plex's GDM; `admin.plexProbe` asks a PMS without saving it;
  `admin.plexPinBegin` and `plexPinCheck` sign in with Plex (plex.tv's PIN
  flow; the token goes straight to `plex.token`, never to the browser);
  `admin.plexPathCheck` checks five files per library with unsaved
  mappings and suggests one where they fail. `ServerStatus` gains the OCR
  languages and the web UI's addresses, `PlexStatus` the PMS version.
  `admin.restart` is offered where `SPOOLFRAME_SUPERVISED` is set, which
  the NixOS module does. `SPOOLFRAME_PLEX_TV_URL`, `SPOOLFRAME_PLEX_APP_URL`
  and `SPOOLFRAME_GDM_TARGETS` point the wizard at stand-ins.
- The setup wizard in the web UI (P3-10), as sf-design's WebWizard boards
  draw it: the setup code's page (over plain HTTP on a LAN address, with
  the insecure-cookies offer), the first admin (P3-01's page, now step 1
  of 7), the server's name and fingerprint, Plex (servers found on the
  LAN or typed, Sign in with Plex or a pasted token, the libraries, OCR),
  the paths (each library's folder mapped, suggestions, checked again as
  typed), playback (encoders found, hardware choice, limits, OCR
  languages), access (public address, trusted proxies, listen address)
  and a summary. Each step saves what changed as it is confirmed; a
  signed-in admin with the wizard unfinished is welcomed back at the step
  where it stopped. Afterwards every step is a tab of Server › Settings
  (with Security for insecure cookies), each field saying when the locked
  config file sets it, which file value it overrides (and a way back to
  it), or that it applies after a restart, with "Restart now" where the
  server is supervised. Every admin page shows a banner while insecure
  cookies are on. The login card works over plain HTTP when they are.
- Plex libraries are chosen (P3-11): `libraries [ { name; kind; plex {
  server; section; } } ]` lists the libraries clients see, in order, each
  one section of a Plex server under a Spoolframe name; left out, every
  library of the first Plex server is served as before. More Plex servers
  go in `plex.servers` (`id`, `name`, `url`, `token_file`,
  `path_mapping`; a token set at run time is `secrets/plex-token-<id>`).
  The first server's IDs are unchanged (`plex:movie:12`); another's carry
  its id (`plex:movie:cabin:12`, `plex:cabin:2`), which clients treat as
  opaque. One catalog routes each call to its server and merges search
  (round robin) and recently added (newest first). A Plex server that is
  down leaves its libraries listed and answering `Unavailable`, while
  search and Home go on with the others. An item in a library that is not
  served is not found for clients, and its watch state is kept and never
  re-linked elsewhere, so adding the library back restores it.
- The admin API manages libraries and Plex servers (P3-11): `libraries`
  (each library with its server, Plex's name for it, its item count, a
  sample path check and why it is unavailable; each server reachable or
  not, with every section marked with the library it is, and those
  Spoolframe cannot serve apart), `libraryAdd`, `libraryRename`,
  `libraryMove`, `libraryRemove`, `plexServerAdd` (probed first, named as
  Plex names itself, with a pasted token or the first server's),
  `plexServerUpdate`, `plexServerSetToken`, `plexServerSetPaths` and
  `plexServerRemove` (with its libraries and token; not the first server).
  The first change to libraries chooses all that were served, then makes
  itself. `plexPathCheck` takes a server and sections, to check a library
  before it is added; `plexStatus` and `plexProbe` give each section's key
  and the library it is.
- Server › Libraries in the web UI (P3-11), as sf-design's WebLibraries,
  WebLibraryAdd and WebLibrariesStates boards draw it: the libraries in the
  order apps show them, each with where it comes from, its size and a
  sample path check, moved up and down, renamed, re-mapped and removed
  from its menu; a card for each Plex server that is down, checked again
  every 30 seconds and on "Check now"; and "Add a library": a Plex server
  already added, found on the LAN or typed, the first server's token or a
  pasted one, then its libraries with their Spoolframe names and paths,
  each checked on a few files before it is added. The wizard's Plex step
  (and Settings › Plex) chooses its libraries with checkboxes, and "Add
  another Plex server" opens the add flow. The boards' watch sync column
  and panel are P3-12's and not here.
- Sign in with Plex per user (P3-12): every user links their own Plex
  account with plex.tv's PIN flow (`me.plexLinkBegin`, `me.plexLinkCheck`),
  and unlinks it (`me.plexUnlink`). The token is kept as the user's secret
  (`secrets/plex-account-<id>`), never in a reply, a log line or a config
  layer. `me.plexLink` and `admin.plexSync` show who is linked to which
  Plex account; an admin can unlink (`admin.userPlexUnlink`), not link.
  PX-04's `plex.watch_sync.user` is that user's link, made by the config
  with each server's own token: nothing to change in an existing config,
  and only the config unlinks it. `watch_sync.user` is now optional, so
  the interval can be set alone. The linked account lists its Plex servers
  (`admin.plexAccountServers`), and `plexServerAdd` without a token uses
  the one the caller's account has for that server. Each library says
  which way its watch state syncs (`plex { sync = "off" | "from_plex" |
  "to_plex" | "both"; }`, shown in `libraries`, set with
  `librarySetSync`); left out, both for the first server's libraries
  while `watch_sync.user` is set, as PX-04 synced them, else off.
- Watch sync per library and per user (P3-12): every linked user's state
  syncs with their own Plex account, on each Plex server their libraries
  come from, with the token plex.tv gives that account for that server
  (or, for the config's owner, the server's own). `from_plex` only takes
  Plex's state, `to_plex` only sends Spoolframe's, `off` does neither, and
  `both` keeps PX-04's rules. An item only Spoolframe knows is sent only
  once Plex says which library it is in. Each linked user runs at start,
  every `watch_sync.interval_secs`, soon after their own state changes
  and at most once a minute during playback; a new link runs at once,
  and unlinking stops the runs. Each run's counts (pulled, pushed,
  skipped, failed) and notes ("Cabin movies skipped: Cabin can't be
  reached") are logged and kept: `admin.plexSync` and `me.plexLink` show
  each user's last run. `admin.plexWatchSync` and `plex sync-watch` take a
  user (`--user`), or run every linked user in turn.
- Plex links and watch sync in the web UI (P3-12), as sf-design's
  WebPlexAccountStates, WebUserSessions, WebLibraries and
  WebLibrariesStates boards draw them. My profile has a Plex account card:
  what linking grants, "Link with Plex" (Plex's sign-in in a new window,
  with a countdown, "Open the Plex window again" and Cancel), the linked
  account with its last run, Unlink after a question, and plex.tv out of
  reach or a code that expired, each said as such; the config's owner can
  link their own account in its place. A user's page has a Plex account
  section for the admin: who the user is linked as, the last sync, "Sync
  now" and Unlink. Server › Libraries has a watch sync column (off, from
  Plex, to Plex, both) and a sync panel: how often it runs, each user's
  last run with its counts and notes, "Sync now" and "Sync everyone",
  "Not linked to Plex · not synced" for the others, and what each
  direction does. The add-library flow lists the servers of the admin's
  linked Plex account ("From your Plex account") and adds one with that
  account's token for it; `plexProbe` tries that token too.
- Scoped, grouped search (P3-09). `Search` takes a `scope` (`all`,
  `movies`, `shows`, `episodes`, `music`), an optional `library`, and an
  optional `kind` for "See all" (`shows`, `movies`, `people`, `artists`,
  `albums`, `tracks`, `episodes`). The reply is `{ top, groups, counts,
  year, without_year }`: the best match (an exact title, then one that
  starts with the query, a leading article aside; a show before its own
  episodes), one group per
  kind with `more` and, for episodes left out of `all` because their
  show is listed, `hidden`; per-scope counts; a year taken from the end of
  the query ("dune 2021") as a filter; and up to 3 items from other
  years. Plex is asked each kind in the sections that hold it
  (`/library/sections/{id}/search?type=`), with the year, so the scope and
  the counts are Plex's; people (actors and directors) come from
  `/hubs/search`, once each across sections. Several Plex servers'
  results are merged per kind and their counts summed; a library's search
  asks only its server. Wire-incompatible: clients rebuild on this SDK,
  whose `Session::search` takes a `SearchRequest` and returns
  `SearchResults`.
- Choosing a person lists their items (P3-09): `PersonItems { person,
  offset, limit }` answers `Items` with what an actor or director a
  search found is in, in the served libraries, from
  `/library/all?actor=` (or `director=`) on that person's Plex server.
  SDK: `Session::person_items`.
- `ServerInfo`, the answer to `Hello`, has `web_url`: the server's
  `admin.public_url`, or none. A client links to its web UI (linking a
  Plex account, P3-12) without beginning a device sign-in, whose web
  link was the only other place it appeared. Wire-incompatible.
- SDK: `Session::web_url`, the `web_url` of the Hello of the connection
  the session is on (the latest, after a reconnect), so an app links into
  the web UI from a signed-in screen.
- Frame previews for scrubbing are made on the server (P3-13): for a film
  or episode, a picture every 10 s, 320 px wide, packed as JPEG sheets of
  10 × 10. Each tile is one ffmpeg seek and one keyframe
  (`-skip_frame nokey`), with a hardware decoder where P2-02 found one for
  the codec, tone-mapped for HDR. On a 4K remux that reads well under
  1 GB, where one pass over the file reads all of it. One file at a time,
  by one worker, at nice 19 and the idle I/O class, outside the
  conversion slots. Sets are kept in `state_dir/cache/previews`, named by
  the file's path, size and modification time and the interval, so a
  replaced file gets new ones, under a size limit, least recently used
  first.
- Clients get frame previews (P3-13). `GetPreviews { id }` answers
  `Previews { interval_ms, tile_width, tile_height, columns, rows, count,
  sheets }`, `PreviewsNotReady { done_percent }` while they are made
  (asking puts the item first in line), or `NoPreviews` (music, a file
  they cannot be made of, or `when = never`). Each sheet is an `ArtworkId`
  in the new `preview:` namespace, read on the artwork stream like any
  image (Original, or scaled whole). `Previews::tile(position_ms)` names
  the sheet and the rectangle for a time. Starting a playback makes its
  item's previews next. SDK: `Session::previews`,
  `SdkError::PreviewsNotReady` and `NoPreviews`, and `Previews` and
  `PreviewTile` re-exported. Config, all live: `previews { when =
  "ahead" | "on_play" | "never"; interval_secs = 10; cache_mb = 4096; }`.
  A stopping server gives up the file being made. Wire-incompatible.
- Previews come from Plex's own index where it has one (P3-13): a part
  whose `indexes` says `sd` has its "video preview thumbnails" at
  `/library/parts/{id}/indexes/sd`, a BIF file (a 320 px JPEG every 2 s;
  23.8 MB for a 110-minute film), read from the film's own Plex server
  and packed into sheets as if made here, with nothing decoded. A part
  without one, or an index that cannot be read, is decoded as before.
  `Catalog::preview_frames`, `None` by default.
- `previews.when = "ahead"` (the default) makes previews before anyone
  plays (P3-13): what any user has in progress first, then the newest
  items, then every film and episode of every library, while the cache
  has room for another set; a play or an ask still goes first and may
  evict. The newest and in-progress items are queued again every 10
  minutes, ahead of the rest, so an item added to Plex gets previews
  soon; every library is walked at start, when `ahead` is turned on, and
  every 6 hours.
- My profile's account, PIN, devices and playback on the server (P3-03).
  `me.setDisplayName` (64 characters on one line; blank is the
  username), which clients show at their next `WhoAmI`;
  `me.revokeOtherSessions` ("sign out everywhere else": every other
  session of the caller's, web, CLI and devices); `me.trackPreferences`
  and `me.setTrackPreferences` (the P1-20 preferences a device's next
  playback uses); `me.trackOverrides` (each series' or movie's pick,
  with its title when the catalog answers within 3 s) and
  `me.clearTrackOverride`. `UserInfo.passwordChangedAt`. `me.pinSet`,
  `me.pinClear`, and `me.setPinRequired` turning the PIN off on a device,
  now need a recent sign-in (`RECENT_SIGN_IN_REQUIRED`), as P3-03 says
  of the PIN; asking for it on a device still needs only the session.
- My profile in full in the web UI (P3-03), as the WebProfile,
  WebProfilePhone and WebAdminProfile boards draw it. A consumer gets no
  sidebar: a slim bar (the mark, the server's name, the avatar and Sign
  out) over one 720 px column, avatar and "Name · username" at its head,
  then Account, Sign-in methods, Plex account, Devices and sessions,
  Profile PIN and Playback. An admin keeps My profile in the admin shell,
  in two columns. Account edits the display name (the bar follows) and
  says when the password last changed. Profile PIN sets, changes and
  removes it and switches it on or off per device. Devices and sessions
  lists every live session with "Sign out", and "Sign out everywhere
  else" after asking. Playback sets the audio language (one language, or
  one then the original; a list a device set is kept as its own choice),
  avoiding audio description and commentary, when subtitles show, their
  language and SDH, saved as each is chosen, and lists each series' or
  movie's pick with "Reset". A change that needs a recent sign-in asks
  "Confirm it's you" first, with the password or a passkey, then goes on.
- Single sign-on with OpenID Connect on the server (P3-04). Config, live:
  `auth { oidc [ { name; issuer; client_id; client_secret_file; } ] }`
  (setting `auth.oidc`); the issuer is HTTPS (HTTP on loopback only),
  names and issuers are unique, and any provider needs
  `admin.public_url`, whose `/auth/oidc/callback` is the redirect URI.
  Without `client_secret_file` the client secret is the one set with
  `admin.ssoSetClientSecret`, kept as `secrets/oidc-<name>`; never shown
  or logged. The server is the provider's confidential client with
  `openidconnect`: authorization code with PKCE (S256), `state` and
  `nonce`, the discovery document (kept a minute), and the ID token
  checked for its signature against the provider's own keys (no HMAC,
  never unsigned), issuer, audience, expiry and nonce. A flow is begun
  with `POST /auth/oidc/begin {provider, purpose}` (`signIn`; `link`,
  which needs a recent sign-in; `confirm`, which renews one), lives 10
  minutes in memory, is good once, and is bound to the browser that
  began it by a `SameSite=Lax` cookie of its own. `GET
  /auth/oidc/callback` always redirects: signed in to `/` with the
  session cookie, or back to `/login` or `/profile` with `?sso=` saying
  what happened (`linked`, `unlinked`, `taken`, `already`, `confirmed`,
  `notyours`, `failed`, `unavailable`, `expired`, `cancelled`) and
  `&provider=`. SSO never makes or finds an account: an identity
  (issuer and subject, never email) signs in only the user who linked
  it, one per provider per user, and an identity linked to one user is
  refused to another. New table `external_identities`. `me.ssoLinks`,
  `me.ssoUnlink` (recent sign-in), `admin.userSsoLinks`,
  `admin.userSsoUnlink`; no way to link for someone else. Unlinking, and
  removing a passkey, keep the user's last way to sign in
  (`LAST_SIGN_IN_METHOD`). `UserInfo.hasSso`, `/auth/site`'s `sso` (the
  providers' names; none without a public URL), error codes
  `NO_SUCH_LINK` and `LINK_REFUSED`. A provider that cannot be reached
  fails SSO with "<name> can't be reached right now"; passwords,
  passkeys and sessions are untouched. Tested against a stand-in
  provider with its own keys.
- The web UI's marks are the full logo (the reel in its four frame
  corners) at every size: the admin sidebar, a consumer's top bar and the
  phone bars too, not only the signed-out cards. The three-hole reel is
  the favicon's alone.
- Sign-in providers in the web UI (P3-04), as the WebLogin,
  WebLoginPhone, WebProfile and WebUserSessions boards draw them. The
  login page has "Continue with <name>" for each provider and says "A
  sign-in provider only works once it is linked in My profile"; an
  account that is not linked gets the board's alert. My profile's
  Sign-in methods card lists the providers ("Link <name>", or the linked
  account and "Unlink"; a link to a provider no longer set up is shown,
  to unlink), and says what the provider's callback did. "Confirm it's
  you" offers "Continue with <name>" for a provider the user has linked,
  so a user whose only way in is SSO can still make sensitive changes.
  An admin's user page lists the user's linked accounts with Unlink, and
  the users list counts a provider as a way in. The page reads the
  callback's `?sso=` before the router starts, and takes it out of the
  address.
- A sign-in provider is what its discovery document says (P3-04): the
  token endpoint is called with `client_secret_basic` where offered, else
  `client_secret_post` (one offering neither is refused); PKCE is S256,
  and a provider offering only `plain` is refused; a document whose
  `issuer` is not exactly the configured one is refused, naming both.
  `auth.oidc[].scopes`, `["openid", "profile", "email"]` if left out
  (`openid` always). A link shows the account's `preferred_username`,
  else its email; it is matched by issuer and subject only. The flow's
  cookie follows `admin.insecure_cookies` as the session's does, so SSO
  works over a plain-HTTP `public_url` with insecure cookies on.
- NixOS module: `services.spoolframe.auth.oidc = [ { name; issuer;
  clientId; clientSecretFile; scopes; } ]` (P3-04), written to the locked
  config's `auth.oidc`, each secret passed as a systemd credential
  (`oidc-<n>`) so its file can stay root-only. It asserts
  `admin.publicUrl` is set.
- Previews made ahead run beside each other and give way to encodes
  (P3-13). `previews.workers` (live) is how many files are made ahead at
  once; left out, it is the physical cores the server may use (read from
  sysfs' `core_cpus_list`, no more than `available_parallelism`, the
  logical CPUs where the cores are not known), which the log says at
  start and Settings › Playback shows ("Default 16 (physical cores)";
  `ServerStatus.previewWorkersDefault` in the admin API). They start only
  while no encode runs (a conversion that encodes a stream; a remux does
  not count), and one being made when an encode starts waits before its
  next tile, keeping those it has, and goes on from there when the last
  encode ends. A played or asked-for item is made at once, one at a
  time, encodes or not; one waiting in the ahead queue moves to it, and
  one being made ahead goes on at once where it is. Every tile is still
  nice 19 and the idle I/O class.
- A sign-in with a provider begun on the device approval page returns
  to it (P3-04, P3-06). `POST /auth/oidc/begin` takes an optional
  `return_to` for `signIn`: a path on this server with its query, starting
  with one `/`, of visible ASCII without a backslash, and still a path
  here once resolved (`/../..//x` is refused); anything else, or a
  `return_to` for a link or a confirmation, is refused as invalid input.
  It is kept with the flow, never read from the callback. Signed in, the
  callback sends the browser there; not signed in, there with `sso=` and
  `provider=` after the page's own query. The device page now offers
  "Continue with <provider>" (it did not), passes `/auth/device?code=…`,
  and says what went wrong there; the web UI takes `sso=` and `provider=`
  out of the address and keeps the rest of its query. The login page's
  sign-in still lands on the start page.
- Sign in with Plex finishes at app.plex.tv (P3-10, P3-12). The PIN is
  now a strong one (`POST /api/v2/pins?strong=true`): Plex's sign-in page
  refused the short one with "Security Alert: another device is attempting
  to sign in". Its address now also carries `forwardUrl`, the web UI's
  new `/auth/plex` page at the browser's own origin (the socket's checked
  `Origin`; the CLI has none and gets no `forwardUrl`), which closes its
  window: without it Plex ended on "We were unable to complete this
  request" although the PIN was approved. Polling the PIN still links.
  Every plex.tv request and the sign-in address name the same device:
  product Spoolframe, its version, the server's platform, device
  `Server`. A strong PIN's code is long and not typeable at plex.tv/link,
  so `PlexPin` no longer has `code`, and the wizard's Plex step no longer
  shows a code or plex.tv/link: like My profile's card it says to sign in
  in the Plex window, with "Open the Plex window again" and the hint that
  a browser may have blocked it.
- An admin's web UI starts on the Server page (P3-02): signing in,
  opening `/` or `/admin`, and finishing the setup wizard land on
  `/admin/server` rather than Users, and the mark and wordmark in the
  admin sidebar and the mark in the phone bar link there. A consumer's
  top-bar mark and wordmark link to My profile, where they land as
  before. A page to come back to still wins (the device approval page's
  sign-in returns to the code).
- Marking a season or a show watched (or unwatched) marks each of its
  episodes, specials included for a show, as marking them one by one
  would; before, it was refused as "no watch state". Seasons and shows
  still keep no state of their own.
- `WhoAmI` says whether the user has a PIN and how many digits it has
  (P2-08): `UserSummary.pin: Option<PinState>`, `PinState { digits:
  Option<u8> }`, for "Set · 4 digits" in a client's preferences and for
  sizing a PIN pad (a locked session gets it too). The server now keeps
  each PIN's length (migration 0010, `pin_credentials.digits`) when it is
  set over the client protocol or `me.pinSet`; a PIN set before has
  `digits: None` until it is set again, since its hash cannot tell.
- Changing or removing a PIN over the client protocol takes the current
  one (P2-08): `SetPin { pin, current: Option<String> }` (`current`
  required once the user has a PIN, else `InvalidInput`) and `ClearPin {
  current }`. It is checked as `UnlockProfile` checks a PIN, on the same
  failure limit and the same count of wrong PINs in a row (so neither is
  a way round the lock's limit, and 10 wrong revoke the session on that
  device), and unlocks nothing. SDK: `Session::set_pin(pin, current)`,
  `Session::clear_pin(current)`. The server's `Auth::unlock` is now
  `Auth::check_pin`.
- Each device's kind of client is kept and shown (P2-08, P3-03): `Hello`
  has a required `platform: Platform` (`linux`, `tvos`, `ios`, `macos`,
  `cli`; an unknown one decodes as `Unknown`), and the SDK's `Device` has
  `platform`. The server keeps it per device (migration 0011,
  `devices.platform`) at sign-in, device approval and every `Hello`, so a
  device signed in before gets one when it next connects; until then it
  is `None`. `DeviceInfo.platform` and `SessionInfo.platform` in the
  client protocol and `DeviceEntry.platform` and `SessionEntry.platform`
  in the admin API carry it. The web UI's session and device rows say
  "Linux app", "Apple TV app" and so on in place of "App" ("App" for a
  device that has not said yet; "Web or CLI" for an admin API session,
  which the admin's user page called "Browser or CLI").
  `spoolframe_types::Platform::label` is the shared wording.
- A client reads the signed-in user's Plex link (P3-12): `GetPlexLink`
  answers `PlexLink { link: Option<PlexLink> }`, with the account's
  username and email as My profile shows them (never the token), whether
  the config file makes the link, when it was linked, the last sync run
  since the server started (`PlexSyncSummary`: when, libraries, pulled,
  pushed, skipped, failed) and whether one is running. `None` when not
  linked. Read only: linking stays in the web UI, `<web_url>/profile`.
  SDK: `Session::plex_link()`.
- The SDK's saved servers are dated (P1-13, P1-21): `ServerEntry` has
  `pinned_at`, `verified_at` (when the user last confirmed the
  fingerprint: at pinning, or a later "Verify now"; `None` while not
  verified) and `last_connected_at` (the last connect it answered as
  itself), as `Option<SystemTime>`. They are kept in the servers file with
  each server; a file from before keeps every server, with the dates
  `None` until they happen again. `trust_new_identity` dates the new key
  as pinned and verified now.
- Changing or removing a PIN over the client protocol takes the current
  PIN or the account password (P2-08), so a forgotten PIN is replaced or
  removed without the web UI: `SetPin { pin, proof: Option<PinProof> }`
  and `ClearPin { proof: PinProof }`, with `PinProof::Pin { pin }` or
  `PinProof::Password { password }` (tagged `type`: `pin`, `password`;
  its `Debug` redacts both). The first PIN needs no proof; changing one
  without a proof is `InvalidInput`. A PIN proof is checked as before, as
  `UnlockProfile` checks a PIN (same limit, same count of wrong PINs in a
  row; 10 wrong revoke the session on that device). A password proof is
  checked as `LoginPassword` checks it: a wrong one is a failed sign-in
  for that username from that device (`InvalidCredentials`, then
  `RateLimited`, one limit with `LoginPassword`), never counts as a wrong
  PIN and never revokes the session. A user with no password (passkeys or
  SSO only) who sends one gets the new `Error::NoPassword` (SDK:
  `SdkError::NoPassword`), with nothing counted; the PIN proof works for
  them. A locked session still gets `ProfileLocked`. SDK:
  `Session::set_pin(pin, Option<PinProof>)`, `Session::clear_pin(PinProof)`,
  and `PinProof` is re-exported. The web UI's `me.pinSet` and
  `me.pinClear` are unchanged.
- Users have UUIDv7 ids (P3-20), in place of SQLite row numbers: unique
  beyond one server, saying nothing about how many users a server has,
  and still sorting by creation. `WhoAmI`'s `user_id` is
  `local:<uuid>` (lowercase, hyphenated), still opaque. Wire change: a
  client that kept a `user_id` reads it again from `WhoAmI`; sessions
  keep working, tokens are not keyed on it. Passkey user handles stay
  the random UUIDs they were. The admin API and CLI show no ids and take
  usernames, as before.
- Migration 0012 moves an existing database to the new ids in one
  transaction: each user gets a UUIDv7 made from their creation time,
  in creation order; `password_credentials`, `sessions`,
  `pin_credentials`, `pin_required`, `passkey_users`, `passkeys` and
  `external_identities` move to it; `watch_state`, `continue_hidden`
  (and their `_ids`), `track_preferences` and `track_overrides` move
  from `local:<n>` to `local:<uuid>`, and a `local:<n>` of no user is
  dropped. A row of a user that does not exist fails the migration and
  leaves the database as it was. Before it runs, the server copies the
  database to `state_dir/spoolframe.db.pre-0012` (`.1`, `.2`, ... when
  one is there already) and logs the path; to undo it, stop the server
  and put that copy in place of `spoolframe.db`. After it,
  `secrets/plex-account-<n>` is renamed to `plex-account-<uuid>`, from
  the old-to-new table `legacy_user_ids` the migration keeps, at every
  start until done. An older server refuses the migrated database
  (migration 0012 unknown to it).
- Passkeys in the apps (P3-19), through one relying party every server
  shares: RP ID `spoolframe.app`, origin `https://spoolframe.app`
  (`spoolframe_protocol::{APP_RP_ID, APP_ORIGIN}`). It needs nothing
  configured and does not depend on `admin.public_url`, so a LAN-only
  server has app passkeys though its web UI has none; the server never
  contacts `spoolframe.app`. New in `spoolframe/client/1` (wire change):
  pre-auth `BeginPasskeySignIn { username: Option<String> }` ->
  `PasskeyRequest { challenge, allow_credential_ids }` (empty: any
  passkey the platform finds; with a username, that user's app passkeys)
  and `FinishPasskeySignIn { credential_id, authenticator_data,
  client_data_json, signature, user_handle }` -> `LoggedIn { session }`,
  a session bound to the device as `LoginPassword`'s, with its PIN asked
  as usual; in a session, `BeginPasskeyRegistration` ->
  `PasskeyCreation { challenge, user_id, user_name, display_name,
  exclude_credential_ids }` and `FinishPasskeyRegistration {
  credential_id, attestation_object, client_data_json, name }` -> `Done`.
  All bytes are CBOR byte strings. Registration needs a session signed in
  within 10 minutes, or the new `Error::RecentSignInRequired`. A
  challenge lives 5 minutes, is used once, and is the device's that asked
  for it (a registration's is its session's). Anything that does not
  verify is `InvalidCredentials`, counted by the failed-login limit as a
  web passkey is (then `RateLimited`); a name that is not 1 to 64
  characters is `InvalidInput`. The account's name in the platform's
  passkey list is `<username> on <server name>`; the user handle is the
  random one each user already has per server (P3-05).
- App passkeys are kept with the web UI's, under the relying-party ID
  `spoolframe.app`: listed in `me.passkeys` and `userPasskeys` with the
  new `PasskeyEntry::app` (and `usable`), removable as a web passkey is,
  and counted by the last-sign-in-method rule. The log no longer calls
  them passkeys of another host at start.
- SDK: app passkeys (P3-19). `Client::begin_passkey_sign_in(server,
  username: Option<&str>) -> PasskeyRequest` and
  `Client::finish_passkey_sign_in(server, FinishPasskeySignIn) -> Session`
  (kept as `Client::sign_in` keeps one); `Session::begin_passkey_registration()
  -> PasskeyCreation` and `Session::finish_passkey_registration(
  FinishPasskeyRegistration)`. Raw WebAuthn bytes as `Vec<u8>`, for the
  app to map to and from its platform API; `APP_RP_ID`, `APP_ORIGIN` and
  the four types are re-exported. New `SdkError::RecentSignInRequired`.
- Web UI: My profile and an admin's user page show an app passkey
  (P3-19) as "Spoolframe app · <device>", in the same row as a web
  passkey (`spoolframe.app · added ...`), removable there as one is.
- Passkey management in the apps (P3-19), as the web UI's `me.passkeys`,
  `me.passkeyRename` and `me.passkeyRemove` and through the same store
  calls. New in `spoolframe/client/1` (wire change), in a session:
  `ListPasskeys` -> `Passkeys { passkeys: Vec<PasskeyInfo> }`, the user's
  web and app passkeys oldest first, each `{ id, name, created_at,
  last_used_at, rp_id, app, usable_here }` (`id` the credential id in
  base64url, as the web UI's; `usable_here` only for an app passkey);
  `RenamePasskey { id, name }` -> `Done` (1 to 64 characters trimmed, or
  `InvalidInput`); `RemovePasskey { id }` -> `Done`, needing a sign-in
  within 10 minutes (`RecentSignInRequired`) and refusing the user's last
  way to sign in with the new `Error::LastSignInMethod`. Another user's or
  an unknown `id` is `NotFound`; a locked profile gets `ProfileLocked`.
  SDK: `Session::passkeys() -> Vec<PasskeyInfo>`,
  `Session::rename_passkey(id, name)`, `Session::remove_passkey(id)`,
  `PasskeyInfo` re-exported, new `SdkError::LastSignInMethod`.
- `spoolframe-catalog`: the `Inspect` trait (P3-16), an admin's look at
  a catalog beside `Catalog`: a library's rows a page at a time
  (`browse`: kinds one after another, a title search, the Unmatched and
  No artwork filters), `count` per kind, `child_rows`, `inspect` (the
  item as clients get it, its library, show and season, Plex GUID,
  versions and parts with the mapping used and whether the file is
  found, its children as rows), `check_library` (every file, as
  `check-paths` checks it, each missing one with its item) and
  `backend_link`.
- `spoolframe-plex`: `PlexCatalog` implements it. Rows come from Plex's
  section listings and P3-09's section search with `includeGuids=1`;
  only the rows of the page have their files looked for. Unmatched is a
  `local://` or "none" agent GUID, the same thing Plex's own filter
  shows. "Open in Plex" is the Plex web app's page for the item on the
  server's machine identifier
  (`https://app.plex.tv/desktop/#!/server/<id>/details?key=...`). The
  path check now keeps every failing file with its item
  (`LibraryPaths::missing`, `section`); its counts and output are as
  they were.
- Admin API, catalog browsing (P3-16), admins only (`FORBIDDEN`
  otherwise), read through the catalog of every Plex server (P3-11):
  `admin.catalogLibrary(id)` (counts per kind, unmatched and
  no-artwork counts, Plex's name for the section, and the library's
  path check, started if it has none), `admin.catalogRows(CatalogQuery
  { library, kind, text, filter: Missing | Unmatched | NoArtwork,
  offset, limit <= 200 })`, `admin.catalogChildren(library, id)`,
  `admin.catalogItem(id)` (the client protocol's `Item` reply as JSON,
  with the caller's own playback state; external IDs; versions and
  parts with the mapping used and whether each file is found, looked
  for when asked; children; "Open in Plex"), `admin.catalogArtwork(id)`
  (each image's size as Plex has it) and `admin.catalogCheckPaths(id)`.
  A Plex server that is down is `UNAVAILABLE`: the server keeps no copy
  of the catalog to show instead.
- Each library's last path check is kept while the server runs, until
  its Plex servers, libraries or mappings change. Opening a library
  starts one if there is none, and Server › Media paths' check
  (`plexCheckPaths`) fills it in for the libraries it covers. "Missing
  files" lists that check's own failures, one row per movie, episode or
  track, so it is exactly what `check-paths` reports; a show's, season's
  or album's missing count comes from it. Only the rows of a page look
  for their files as they are listed.
- `GET /catalog/artwork?id=<ArtworkId>&size=thumb|small|medium|large`
  on the admin listener: artwork for the catalog pages, resized through
  the artwork cache, to an admin's session only.
- Web UI: catalog browsing (P3-16). In Server › Libraries a library's
  name opens its items (`/admin/libraries/<library>`): movies, shows or
  albums in a paged table (50, 100 or 200 a page; the page, filter,
  kind and search are in the address). Shows open in place into seasons
  and seasons into episodes, and albums into tracks. Each row has its
  poster, title, year, kind, match (the agents' ids, or Unmatched), its
  mapped path and whether its file is found; a show, season or album
  shows how many of its files are missing. The Columns menu adds
  MediaId, Plex path, Added and Updated. Those columns are kept per
  admin in this browser (`localStorage`). Missing files, Unmatched and
  No artwork filters, a Kind select and P3-09's title search; Missing
  files lists the path check's own list, by episode, with MediaId and
  Plex path shown. The header names the source, the counts and when the
  paths were checked.
- An item (`/admin/libraries/<library>/<MediaId>`) has a header with
  its artwork, MediaId (copy), Matched, files and markers, and "Open in
  Plex". Its tabs are Overview (the item as clients get it, its
  external IDs linked out, the Plex GUID, and Show JSON of the exact
  client reply), Files & paths (each version and part with size, Plex
  path, the mapping used, the mapped path and Found or Not found,
  checked as the page opens), Artwork (each image's id, size and
  source, or None), Markers (a timeline with each range), Children,
  and Debug. Debug is shown greyed out: playback debug (P3-15) is not
  built yet. A consumer who opens a catalog link sees "Only admins can
  see the catalog". On a phone, the rows are cards with back and next,
  and the tabs scroll sideways.
- A Plex server's files must be on this machine (P3-11). Spoolframe
  plays the files itself, so Add a library and the wizard's Plex step
  say so under the server choices. A server away from this network (an
  account server plex.tv has no LAN address for, a public IP, or a
  plex.direct name for one) is marked "Remote · its files must be
  mounted here", and the wizard never picks one for you. A library none
  of whose 5 sample files is found here shows "Not on this machine" and
  holds the add up, with a reason that names its folders. `libraryAdd`
  refuses such a library too (`INVALID_INPUT`), in the same words.
- Movies, shows and episodes carry their age rating, `content_rating`
  ("R", "TV-MA"), as the catalog has it; `None` when unrated. From
  Plex's `contentRating`, in listings and single items alike.
- Shows carry `season_count` (specials count as a season) and `genres`,
  in lists, search and on the show itself, from Plex's `childCount` and
  `Genre` tags. Plex lists only the first two genres in listings and
  search; the show's own item has all of them.
- Music format and position: a track carries `codec` (the shared
  `AudioCodec`), `bit_depth` (bits per sample) and `sample_rate` (Hz) as
  numbers, and `album_track_count` for "track 3 of 9"; an album carries
  `track_count`. The codec is in every listing; bit depth and sample
  rate come with an album's own tracks and a track's own item, which
  ask Plex for the tracks' streams (one request per album, and the
  tracks still load without them if it fails). `album_track_count` is
  set on an album's own tracks; `track_count` on an album's own item,
  where Plex counts them (`leafCount`).
- `MediaTracks` (the `Tracks` reply) says what the file is: `container`
  (the shared `Container`) and `video`, a `VideoTrack` with `codec`
  (`VideoCodec`), `width` and `height` in pixels, `bit_depth` and `hdr`
  (`HdrFormat`; `None` is SDR). From the server's own probe of the file,
  which `Tracks` already runs and caches. HDR10+ shows as HDR10: its
  metadata is per frame, and the probe reads streams, not frames.
- Search: each `SearchGroup` has `total`, how many a "See all" search
  for that group finds (its top match and the episodes under their show
  included): the catalog's exact count for items; for people only when
  all of them are shown, since Plex lists people only up to the limit.
- Search: the top match can be a person. `SearchResults.top` is now a
  `TopResult`, `Item(Item)` or `Person(Person)` (`{"item": …}` or
  `{"person": …}` on the wire). A person is the top match only when
  their name matches better than every title (an exact name over titles
  that start with it), or when only people are found; on a tie the item
  wins. Like an item, the top person is not repeated in their group.
  Their items are listed with `PersonItems`, as before.
- SDK: `Client::saved_profiles(server)` lists everyone signed in on a
  server from this device, each a `SavedProfile` (`username`,
  `display_name`, `sign_in`), read from the device's store without the
  server. `ProfileSignIn::Automatic` has a kept session;
  `ProfileSignIn::Password` is someone whose session the server ended,
  who signs in again. A session the server ends is no longer forgotten
  outright: its token goes and its user stays, as `Password`.
  `Client::forget_profile(server, username)` takes one off; signing out
  still removes the profile too. `Client::profiles` is unchanged: the
  sessions that still work.
- SDK: `Session::connection_path()` says how the device reaches the
  server now, for a player's diagnostics: `ConnectionPath::Lan { addr }`
  (straight to a private, link-local or loopback address),
  `Direct { addr }` (straight to a public address; iroh does not say
  whether a hole was punched), `Relay`, or `Unknown` without a
  connection. It never dials.
- SDK: `Client::connection_path(endpoint_id)` says how the device
  reaches a server now, without dialling and before any profile is
  chosen ("Who's watching"); `Session::connection_path()` is the same
  for its server. Unknown when there is no connection.
- SDK: a connection that dies without closing no longer hangs requests
  (P1-15). An app suspended by iOS came back to a connection the server
  had dropped, and a request on it waited forever. Now every request has
  a time limit (`Timeouts`, default 15 s; `Client::open_with` sets it),
  plus the server's own wait for requests built to wait:
  `AwaitDeviceSignIn` +25 s, subtitle and converted-media reads +10 s,
  requests that may probe the file +30 s, starting a playback or
  changing its subtitle +2 min. A media read or image fails only on a
  pause of 15 s, not on a slow link. Past the limit a request is the new
  `SdkError::TimedOut`; the connection is then asked for a `Ping`
  (3 s), and dropped if that goes unanswered too, so the next request
  dials. A request that only reads (lists, item, search, `WhoAmI`,
  `LookupDeviceSignIn`, media reads, images…) is asked once more on the
  new connection; one that changes something (approve or deny, PINs,
  playback start, marking watched…) never is, since it may have been
  done. A connection that answers the `Ping` is kept: the server is
  slow, not gone. A Hello not answered in time is `Unreachable`.
  `Client::resume()` pings every open connection and drops those that
  do not answer, without dialling servers that have none: apps call it
  on returning to the foreground or waking from sleep. The endpoint's
  QUIC idle timeout is 15 s (keep-alive every 5 s), down from 30 s.
- SDK: a saved profile remembers how it last signed in on this device
  (PX-07), so "Who's watching" can say "Passkey" for a profile whose
  session ended, not only "Password". `SavedProfile.method` is
  `Option<SignInMethod>`: `Password` (`Client::sign_in`), `Passkey`
  (`Client::finish_passkey_sign_in`) or `Device` (approved on another
  device, `Client::await_device_sign_in`, P3-07). It is kept with the
  profile in the sessions store and outlives its session; signing in
  again records the new method. Profiles stored by an older SDK are
  kept, with `None`. `SavedProfile.sign_in` is unchanged.
- A track says whose it is (PX-07). A search result showed only "Can I
  Get It · Track 6" on a blank tile, with nothing to say it is Adele's.
  `Track` now has `artist_name` (the track's own artist from its tags,
  Plex's `originalTitle`, such as "Adele with Chris Stapleton", else the
  album artist, `grandparentTitle`), `album_title` (`parentTitle`) and
  `artwork`: the album's cover as the poster (`parentThumb`, else the
  track's own `thumb`, which Plex sets to the album's) and the artist's
  background (`art`). Every track carries them: listings, an album's
  tracks, an item, search and its top match. Another Plex server's
  track covers name that server, as other artwork does. Wire change:
  the fields are required, as the wire may break for now.
- Metrics (P3-14): `GET /metrics` on the admin listener answers with
  Prometheus text (format 0.0.4). Off by default (`admin.metrics =
  false`), and then not found; when on, only for the addresses and CIDR
  ranges in `admin.metrics_from` (default `127.0.0.0/8, ::1`), and 403
  for anyone else. Behind `admin.trusted_proxies` the address checked is
  the one the proxy forwards for. Both are live settings. The process
  series: `spoolframe_process_uptime_seconds`,
  `spoolframe_process_resident_memory_bytes` and
  `spoolframe_process_open_files` (the last two on Linux).
- Metrics (P3-14), the server's parts, read when scraped:
  `spoolframe_connections_open{kind="app"|"web"}` (Iroh connections,
  admin API sockets); `spoolframe_sessions{user="local:<uuid>",kind}`
  (live sessions, read from the database every 10 s);
  `spoolframe_playbacks_active{plan="direct"|"remux"|"transcode"}`,
  `spoolframe_playbacks_started_total`, `spoolframe_playbacks_ended_total`,
  `spoolframe_playback_errors_total{error="expired"|"failed"}`;
  `spoolframe_conversions_running`, `spoolframe_conversions_paused`,
  `spoolframe_conversions_refused_total` (over `transcode.max_jobs`: jobs
  have no queue), `spoolframe_conversions_stalled_total`,
  `spoolframe_conversion_output_seconds_total`;
  `spoolframe_media_reads_total`, `spoolframe_media_read_bytes_total`,
  `spoolframe_artwork_requests_total`, `spoolframe_artwork_cache_hits_total`,
  `spoolframe_subtitle_reads_total`; per Plex server (its id)
  `spoolframe_plex_requests_total`, `spoolframe_plex_errors_total` (a 404
  is not one) and the histogram `spoolframe_plex_request_duration_seconds`;
  `spoolframe_plex_sync_runs_total{outcome="ok"|"failed"}`;
  `spoolframe_previews_queued{state="waiting"|"making"}`,
  `spoolframe_previews_total{outcome="done"|"failed"}`,
  `spoolframe_ocr_queued`, `spoolframe_ocr_total{outcome}`.
- `spoolframe-plex`: `PlexCatalog::request_counts()`, every request to
  one PMS counted and timed, across the views made of its catalog.
- Tracing spans `client request` (the request's type), `plex request`
  (the path) and `plex sync run` (the user as `local:<uuid>`): log lines
  inside them name them.
- Admin API `admin.activity` (P3-14), admins only (`FORBIDDEN`
  otherwise), for the web UI's Server › Activity page: `now` and the
  last hour's `history` of `ActivityPoint`s (connections, playbacks by
  plan, encodes running and paused, output seconds per second, MEDIA
  reads and bytes per second, artwork requests per second and hit rate,
  subtitle reads, Plex requests and errors per second, previews and OCR
  queued, memory, open files), sampled every 10 s and kept in memory
  only; `totals` since start; each Plex server's requests, errors and
  mean latency; and each user's live sessions. A page polls it.
- The last log lines in memory (P3-14), for the web UI's Server › Logs
  page: `logs.buffer_lines` (default 5000, 1 to 100000, live) lines at
  the level the server logs at, as stderr has them (spans, message,
  fields). Admin API `admin.logs(query)`, admins only: the last lines,
  or with `after` the ones since a page's cursor (`next`), at least as
  severe as `level` and containing `text` in any case; `missed` says
  lines were dropped before they were asked for. A page follows new
  lines by polling with the cursor.
- OpenTelemetry export (P3-14), opt-in: `telemetry { otlp { endpoint;
  protocol = "grpc" | "http"; headers; traces; metrics; logs; } }`, off
  without an endpoint, a live setting. The server's spans (a client
  request, a Plex request, a sync run), its log records at the level it
  logs at, and every metric of `GET /metrics` (counters without `_total`,
  the Plex latency histogram as its `_sum` and `_count`) go to the
  collector, each with the resource `service.name = spoolframe-server`,
  `service.version`, `spoolframe.server.name` and
  `spoolframe.server.endpoint_id`. `headers` (`name=value`,
  comma-separated) is write-only: never logged, shown or returned, only
  whether it is set. Spans and log records are queued (2048 each) and
  exported in batches by tasks of their own; a full queue drops and counts
  (`spoolframe_telemetry_dropped_total{signal}`), a failed export is
  counted (`spoolframe_telemetry_export_failures_total{signal}`) and
  logged at WARN at most once a minute. What is queued is exported when
  the server stops. Built on opentelemetry 0.31, opentelemetry-otlp 0.31,
  tracing-opentelemetry 0.32 and opentelemetry-appender-tracing 0.31.
- Admin API `admin.telemetryStatus`, admins only, for Settings ›
  Telemetry: the export's endpoint, signals and header names, when an
  export last went through, what was dropped since when, the last error;
  the resource attributes; whether `GET /metrics` is on, and the
  addresses refused it in the last hour with their counts.
- Web UI: Server › Activity (`/admin/activity`, P3-14). A card each for
  playbacks (direct, remux, transcode; started, ended, expired, failed),
  conversions (paused by the throttle, output speed, refused at the job
  limit, stalls ended), media reads (bytes, range, subtitle and artwork
  reads, artwork cache hits), connections (apps, web, sessions per
  user), Plex (requests a minute; each server's requests, errors and
  mean latency; watch sync runs), background work (previews and OCR
  queued, done, failed), the process (memory, open files, uptime,
  version) and the telemetry export (on or off). Each has its number now
  and a graph of the last hour, drawn against the hour, so after a
  restart it starts part-way. The page asks `admin.activity` every 5 s,
  says "Live · updated N s ago", and Pause stops it asking. On a phone,
  two small cards a row with the number and the graph only. The sidebar
  has Activity under Server, before Libraries and Settings.
- Web UI: Server › Logs (`/admin/logs`, P3-14): the server's last log
  lines from `admin.logs`, oldest first, in mono with WARN and ERROR
  tinted, each with its local time, level, target (its last part; the
  whole on hover) and text. DEBUG+, INFO+ (the default), WARN+ and ERROR
  pick the least severe level shown, and the text filter (applied as
  typing stops) any text or target; the server filters both. The page
  follows new lines by asking every 2 s after the cursor it was given,
  keeping the newest in sight; Pause holds new lines back and says how
  many ("Paused · 4 new lines"), and Follow shows them. Lines dropped
  before they were asked for are said. Download saves the lines shown as
  a text file made in the browser, times in UTC. The page keeps as many
  lines as the server does (`logs.buffer_lines`). The sidebar has Logs
  under Server, after Activity.
- Web UI: Settings › Telemetry (P3-14), two cards. Prometheus: "Serve
  /metrics" (`admin.metrics`) with its address on the admin listener,
  and Allowed from (`admin.metrics_from`) as chips: loopback is always
  there and cannot be removed, "Add an address" takes an address or a
  CIDR range and refuses anything else, and an address refused in the
  last hour is named with its count and "Allow <address>". OpenTelemetry:
  "Export with OTLP" (off is no `telemetry.otlp.endpoint`), the
  endpoint, gRPC or HTTP, the headers by name only with their values
  masked (Replace the headers, typing every value again, or Remove
  them; a value is never shown or read back), Traces, Metrics and Logs,
  and the resource attributes every export carries. Under it, how the
  export is going, asked every 5 s: "Exporting · last batch N s ago · N
  dropped"; the collector unreachable or failing ("N dropped since
  HH:MM. Trying again every N s." and Try now, which asks again); or
  refused, named with the collector's answer and the headers to check.
  Discard and Save, as the other tabs; it all applies when saved. The
  Activity page links there, and its Telemetry card says how the export
  is going.
- Plex discovery is answered through a host firewall (P3-10). The
  wizard's search for Plex servers (GDM) went out from a random port, and
  each Plex server's unicast answer to it was dropped by the host's
  firewall, which cannot match an answer to a search sent to a multicast
  group or the broadcast address: no server was ever found on a host
  with a firewall. The search is now sent from a fixed UDP port,
  `network.plex_discovery_port` (default 32415: past Plex's own
  32410-32414, which a Plex server on the same host holds), which the
  firewall can open. The setting is live: the next search uses it. A
  port another program holds is given up for any free one, with a
  warning that answers to it may be dropped; 0 is any free port.
  Searches take turns, since two on one port would each get only some
  of the answers.
- NixOS module: `services.spoolframe.plexDiscovery.enable` (default
  true) and `.port` (default 32415) (P3-10). Enabled, the provided file
  sets `network.plex_discovery_port` to the port and, when the host's
  firewall is on, `networking.firewall.allowedUDPPorts` gets it, so
  the wizard's search for Plex servers is answered through the
  firewall. A host that runs the server without the module opens the
  port itself: `networking.firewall.allowedUDPPorts = [ 32415 ];`.
- Sign in with Plex in the setup wizard offers the account's servers
  (P3-10, P3-11). The wizard's Plex step listed only what discovery
  found and a typed address, so after signing in a user's own servers
  (or ones shared with them) were not offered. Now, once the PIN is
  approved, the step asks plex.tv for the account's servers and lists
  them as "From your Plex account", after the ones found on this
  network: each server once, one both found and listed being matched by
  Plex's machine identifier (GDM's `Resource-Identifier`, now
  `DiscoveredPlex.machineId`) or its address, and read at the address
  discovery found. A server reached only through Plex's relay is left
  out; a remote one carries "Remote · its files must be mounted here"
  and is never chosen for the admin. `admin.plexAccountServers` falls
  back to the account the wizard signed in with when the caller has
  linked none (kept in memory, so a restart asks for a new sign-in).
  `admin.plexProbe` and `plexServerAdd` use that account's token for a
  server too, and the new `admin.plexAccountTokenUse(url)` keeps, as
  `plex.token`, the token plex.tv gives the account for the server at
  `url`, which a server shared with the account needs: the wizard
  calls it when one of the account's servers is chosen. A server is
  matched to its plex.tv entry by a connection's URL or by the address
  and port plex.tv says a plex.direct name stands for. Wire change:
  `DiscoveredPlex.machineId` is always sent (null when a server's
  answer has none).
- Protocol: `Error::PlaybackStopped` (P3-15), "the playback was stopped
  on the server": an admin stopped it. A client tells the viewer it
  ended and does not start it again, as it would after
  `PlaybackExpired`. Every later read, progress report, seek or
  subtitle change on its handle gets it, for as long as an idle handle
  would have lived. The SDK's `SdkError::PlaybackStopped`;
  `ManagedPlayback` does not recover from it. A client built before it
  sees `Unknown`.
- Playbacks are kept for playback debug (P3-15): each has a random id
  of its own (never its handle), who plays it on which device, what it
  plays, its plan, when it started, the position its client last
  reported, and its reads over the last 10 minutes (one point a second
  at most: where in the film, and how far its conversion had got). A
  playback that ends is kept for an hour (200 at most) with how it
  ended: its client stopped it, an admin did, it idled out, its device
  started another conversion or was gone too long, its session ended,
  or its conversion failed. An encode keeps its command line and
  ffmpeg's last 50 stderr lines; End conversion stops ffmpeg and frees
  its slot while what it wrote stays readable, and nothing starts it
  again. The command line shown hides the playback handle in its paths
  and any token-like value. All in memory.
- Admin API, playback debug (P3-15), admins only (`FORBIDDEN`
  otherwise); nothing in it changes a playback but Stop and End
  conversion. `admin.playbacks`: every open playback, newest first, and
  those that ended in the last hour with how: user, device, item, plan
  (`direct`, `remux`, `transcode`, `unsupported`) and the plan's own
  reasons, the "playback started" log line's summary, epoch, revision,
  the position the client last reported, where the last read was in the
  film, the furthest byte or segment read, when it was, and the
  conversion's progress, whether the throttle holds it back, whether it
  runs, ended or failed. `admin.playback(id)`: that, the plan laid out by
  stream (what each is in the file, copied, encoded with which encoder,
  read as text, converted, burned in, and the reasons naming it, word for
  word), the ffmpeg command line (paths shown; the playback handle and
  any token hidden), ffmpeg's last stderr lines, and the reads of the
  last 10 minutes; it answers for an ended playback too.
  `admin.playbackStop(id)` and `admin.playbackEndConversion(id)`
  (`NO_SUCH_PLAYBACK`, `NO_CONVERSION` for direct play).
  `admin.catalogDebug(id)`: the item's file as ffprobe sees it (path,
  container, duration, bitrate, size, each stream with its index, codec,
  details, language, title, role, default and forced), the devices "Plan
  for" offers, each subtitle track's text as the cache has it (made,
  being made, not made, or why it cannot be), and the frame previews'
  state; looking queues nothing. `admin.catalogPlanFor(id, request)`:
  the plan `PlaybackPlan` would give a device for those tracks and
  picture-subtitle mode, laid out as above, with the plan itself as the
  client gets it. A device is one connected now (`device:<endpoint>`,
  with what its Hello said it plays) or a stored profile: `linux`
  (the Linux app with GStreamer's usual plugins), `apple-tv` (AVPlayer:
  H.264 and HEVC, AAC, AC-3, E-AC-3, MP4 and TS, WebVTT, HLS), `iphone`
  (as Apple TV, stereo), `mac` (as Apple TV) and `cli` (no player).
- Web UI: Server › Playback (`/admin/playback`, P3-15), in the sidebar
  under Server after Activity. Each playback: what plays, who and on
  which device; its plan's pill (Direct, Remux, Transcode, Cannot play)
  and the plan's own reasons; Range, HLS or Progressive and the epoch;
  the position the client reported and where the reads have got to; the
  conversion's share done with a bar, "Paused by the throttle" when it
  is; and how long since the last read. "Nothing is playing" when
  nothing is, and "Ended in the last hour" with how each ended. Asked
  every 2 s. On a phone, a card each. One playback's page
  (`/admin/playback/<id>`): position, converted to, the throttle and the
  last read with the furthest byte or segment; the plan a row per
  stream (in the file, out, and the reasons naming it) with the log
  line's summary under it; the reads of the last 10 minutes drawn as
  read position against converted-to; the ffmpeg command line with Copy
  ("Paths are shown; tokens never are"), a failure's reason, and the
  last stderr lines. End conversion and Stop playback each ask first;
  an ended playback's page says when and how it ended.
- Web UI: an item's Debug tab (P3-15), now open on films, episodes and
  tracks. Probe: the file's path, container, length, size and bitrate,
  and a row per stream (its index in the file, kind, track number,
  codec, details, language, role), with "Look again". "Plan for…":
  Device (each one connected now, then the stored profiles Linux app,
  Apple TV, iPhone, Mac and CLI, with what each stands for), Audio,
  Subtitles and Picture subtitles (Auto, Text, Burn in); the plan comes
  back as the Playback page lays it out, the plan `PlaybackPlan` would
  give, and nothing plays. Subtitles and previews: each picture track's
  text (and a text track's, once something of it is made), whether it
  is made, being read, failed or cannot be; and the frame previews'
  state.
- Device sign-in (P3-18, P3-07): one code has one outcome. A pending
  sign-in moves Waiting → Claimed → Approved/Failed, or Waiting →
  Denied, each step under one lock and only from the step before, so
  two approvals, an approval and a denial, or an approval and the code
  running out cannot both pass. An approval that the code outruns while
  its session is made revokes that session and tells the approver the
  code is gone (`NotFound` / `NO_SUCH_SIGN_IN_CODE`, not counted as a
  wrong code), as the device was told it expired; a sign-in decided a
  moment before its code ran out is still handed to the device for 25 s.
- Device sign-in (P3-18, P3-07): the approver sees what kind of device
  asks. `DeviceSignInRequest` (the `LookupDeviceSignIn` reply) and the
  admin API's `DeviceSignIn` (`me.deviceSignInLookup`) gain a required
  `platform`, from the waiting device's `Hello`; the web UI's
  `/auth/device` page shows it under the device's name ("Apple TV app",
  `Platform::label`). Wire change: approver apps (phone, Linux) should
  show it beside the device name.
- Web UI (P3-18, P3-01): every passkey prompt opens from the click
  itself, before anything is awaited, as Safari requires. Setup fetches
  the passkey request for the username once typing pauses; sign-in and
  "Confirm it's you" fetch theirs when the card opens; "Add a passkey"
  already had its own. A click before the request is in hand fetches it
  first, as before, and a refused prompt fetches a new one for the next
  click. Checked in WebKit 26.4 (Playwright) with a stand-in
  authenticator that refuses a prompt opened outside the click: a
  passkey-only setup and a passkey sign-in, over HTTPS.
- NixOS VM test (P3-18, P3-04, P3-10): `checks.<system>.nixos-module`
  now boots two machines. `server` sets P3-10's admin options (listener
  on loopback, `publicUrl`, `trustedProxies`) and a P3-04 sign-in
  provider with its secret as a credential, and checks `/auth/site` and
  `spoolframe config show` show them, and that `openFirewall` opens UDP
  7420, 5353 and 32415. `fresh` has only `enable`: not set up, the
  service active, `/auth/site` answering, `config show` working, its
  key and database in `/var/lib/spoolframe` (the service's, mode 700),
  UDP 32415 open and nothing else of ours, and the web UI not reachable
  from another machine. Run, with KVM: it passes, and found no module
  bug; the server package (OpenSSL, P3-05) builds.
- Web UI: Server › Libraries' drag handle works (P3-18): a library
  dragged by it to another row takes that row's place, with a line
  showing where it lands, and the order is saved through `libraryMove`
  as the up and down arrows save it. Only the handle starts a drag, so
  the row's link and watch sync menu work as before; the arrows stay
  for the keyboard.
- The first Plex server can be removed (P3-18), once it is the only one:
  `admin.plexServerRemove("plex")` takes out its libraries and every
  `plex.*` setting of the runtime layer, forgets its token, and leaves the
  server not set up until a Plex server is added again. Its watch state is
  kept under its items' ids (`plex:movie:12`), as a removed library's is,
  and the same server added back finds it. While another server remains it
  is refused (`INVALID_INPUT`), naming them: the next server could take its
  place only by its items taking those ids, which would show the old
  server's state on other films and change every id clients hold. A first
  server the config file names is removed there. The add-library flow's
  Cancel now takes out a first server it added.
- A Plex server that is down keeps its libraries' Plex titles (P3-18):
  the server keeps each section's title as every server last gave it, in
  `state_dir/plex-section-titles.json` (not a setting), and
  `LibraryEntry.plexTitle` is that title while its server is down, so
  Server › Libraries says "Plex · Cabin · Movies" rather than "section 1",
  after a restart too. A server removed is forgotten; a missing or
  unreadable file is filled again by the next answers.
- Search inside a library finds people (P3-18): the actors and directors
  Plex's hubs found in that library's section, each with their count
  there. The hubs already say where each person was found, so it costs
  no call per person; inside a library the hubs are asked for four times
  the limit (at most 100), so people found elsewhere first leave room.
  The request and reply are unchanged; `PersonItems` still lists all of a
  person's items.
- Admin pages (P3-18): a user whose password sign-ins are refused for
  too many wrong ones under their username shows "Rate limited" on the
  Users list and their page, with the wait in its tooltip;
  `UserInfo.rateLimitedSecs` (and `whoAmI`'s) is the wait, from the P1-11
  limiter. A source that is limited is not shown on anyone. The CLI's
  `user list` says "rate limited for N min".
- An admin API session says what signed it in (P3-18): `SessionEntry
  .platform` is `web` for the web UI's cookie sessions (setup, password,
  passkey, SSO) and `cli` for the rest, kept in the session's new
  `client` column (migration 0013); `null` for one from before. The web
  UI shows "Web" or "Command-line client" in place of "Web or CLI", and
  the CLI's `user sessions` likewise. `Platform` gains `web`, which no
  client sends; an older peer reads it as `unknown`.
- Web UI: setup's first-admin step has a display name (P3-18), beside the
  username as the WebSetup board draws it, on the passkey and the
  password-only setup alike. It is optional, checked as `setDisplayName`
  checks it (at most 64 characters, one line) before the account is
  made, and given through `setDisplayName` when the step is finished, so
  the Users list and My profile show it. The setup API is unchanged.
- An app confirms it is its user (P3-07): a session signed in more than
  10 minutes ago, which gets `RecentSignInRequired` for approving a
  device, adding or removing a passkey, or no longer asking for the PIN,
  sends `ConfirmPassword { password }` or `BeginConfirmPasskey` then
  `FinishConfirmPasskey(FinishPasskeySignIn)`, both answered `Confirmed`,
  and asks again; it no longer has to sign out and in. Only that session
  is refreshed, with the store call the web UI's `me.confirmPassword` and
  `me.confirmPasskey` use, and failures count under the same keys as a
  login (`InvalidCredentials`, then `RateLimited`). The passkey challenge
  lists only the user's app passkeys and is the session's on its device;
  a user without one gets `NotFound`, without a password `NoPassword`. A
  PIN is not a confirmation, and a locked profile gets `ProfileLocked`.
  The SDK has `Session::confirm_password`, `begin_confirm_passkey` and
  `finish_confirm_passkey`.
- Web UI: the setup wizard's summary warns about missing files (P3-18,
  P3-10), as the WebWizardDone board draws it: the Paths line names each
  library some of whose sample files this server cannot read ("Music: 2
  of 5 missing"), beside the Change link that opens the Paths step. The
  summary runs the Paths step's sample check again on the saved
  mappings, so a mapping changed since then is what it shows. No API
  change.
- Sign in with Plex works for every Plex server (P3-18, P3-11): Add a
  library offers "Sign in with Plex" when no Plex account lists servers
  (none linked in My profile, none signed in with since the server
  started), then lists that account's servers; the one chosen is probed
  and added with the token plex.tv has for it, as the wizard does for the
  first, and the other servers keep theirs. Admin API:
  `plexPinCheck(id, keepToken)` gains `keepToken` (required): the Plex
  step passes `true` and the token is kept as `plex.token` as before;
  with `false` the account is only remembered, for `plexAccountServers`,
  `plexProbe` and `plexServerAdd`. A wire change for callers of
  `plexPinCheck`.
- Admin pages show the Plex and Spoolframe versions (P3-18, P3-02), where
  the WebServer board places them: This server's Version is "0.5.0 ·
  9f3c2a1" and the Plex card says "Plex Media Server 1.41 · <host:port> ·
  N libraries", from `PlexStatus.version` (to its minor, as the wizard's
  Plex step says it). Admin API: `ServerStatus.revision` (required,
  `null` when unknown) is the source revision the binary was built from,
  read at compile time from `SPOOLFRAME_GIT_REV`; the nix package sets it
  to the flake's short revision (`-dirty` for a dirty tree), and a plain
  `cargo build` or a flake without git leaves it `null`, so only the
  version shows. The CLI's `status` prints `version: 0.5.0 (9f3c2a1)`.
- Server › Media paths says when the files were last checked (P3-18,
  P3-02): "1,204 files · checked 2 minutes ago" on the first row, as the
  WebServer board has it, moving on while the page is open. The server
  keeps the last check of every library with when it finished, so the
  page shows it again when opened, until the server restarts or its Plex
  servers, libraries or path mappings change. Admin API: `plexCheckPaths`
  answers a `PathCheckRun { at, reports }` (Unix seconds) in place of
  the bare list, a wire change; `plexLastPathCheck` gives the kept one,
  or `null`. A check of one library is answered the same way but not
  kept.
- A media file or a subtitle file beside it that is swapped for a symlink
  during its playback is not read through the link (PX-10, PX-09
  finding 1). Direct play reads only the file it started with (its device
  and inode), so a read after the swap is `Unavailable`; a subtitle file
  is opened without following a symlink and must be a regular file, and
  may still be replaced by another file as P2-11 allows. Subtitle files
  found beside a film are regular files, or symlinks to one in the same
  directory; a FIFO or a link out of the directory is skipped.
- Plex paths are confined (PX-10, PX-09 finding 12): a path Plex reports
  with a `..` component is never served or probed, and once
  `plex.path_mapping` (or a library's mappings) has any rule, a path that
  no rule maps is refused too; the path check counts it under "no
  matching mapping". A library at the same path on both hosts needs a rule
  from that path to itself. With no rules at all, Plex's paths are used as
  they are, as before.
- An admin's changes that could take over an account or the server need a
  recent sign-in, as the user's own do (PX-10, PX-09 finding 3): setting
  a password, removing a passkey, clearing a PIN or no longer asking for
  it, unlinking a sign-in provider account, adding an admin or making
  one, setting or unsetting an `admin.*` or `auth.*` setting, a sign-in
  provider's client secret, and a Plex server's token. From a session
  signed in more than 10 minutes ago they are `RECENT_SIGN_IN_REQUIRED`;
  the web UI then shows "Confirm it's you" and tries the change again.
  Admin API: those methods may now answer `RECENT_SIGN_IN_REQUIRED`.
- A new password signs out whoever knew the old one (PX-10, PX-09
  finding 4): `me.setPassword` revokes every other session of the user's,
  in browsers and on devices, and keeps the one it came from;
  `admin.userSetPassword` revokes every session of that user's, except
  the admin's own when they set their own. Devices are asked to sign in
  again (`Unauthenticated`). The web UI's password forms say so. Admin
  API: no new fields; the revocation is a change of behaviour.
- The Plex token reaches only the server it is for (PX-10, PX-09
  finding 5). Spoolframe follows no redirect from a Plex server, so
  `X-Plex-Token` is never sent on to another host. `plexProbe` and
  `plexServerAdd` no longer fall back to the first server's token for
  another URL, such as one a LAN discovery answer named: they use a
  pasted token, else the one plex.tv lists for that server under the
  linked Plex account (or the wizard's Sign in with Plex), and a probe of
  a server already added uses its own. Add a library no longer offers the
  first server's token for a new server; it asks for the account or a
  pasted token. Admin API: a change of behaviour for callers that relied
  on the fallback.
- Bitmap subtitles read for OCR are bounded (PX-10, PX-09 finding 8): a
  PGS object or picture larger than 4096 × 4096 stops the track's
  reading with an error, and a DVB region or page past that size is left
  out, instead of allocating what the file's header says (up to 4 GiB).
  ffprobe's packets of a track are read to 512 MiB, a `.sup` to 256 MiB
  and tesseract's text to 16 MiB. Each tesseract run may take 5 minutes
  and each ffmpeg or ffprobe pass over a film (copying a track out,
  reading its packets, converting a text track) 15, then it is killed and
  the subtitle fails to read.
- A file that says it is longer than 48 hours gets no frame previews and
  no HLS (PX-10, PX-09 finding 11): its length is ffprobe's, from the
  file, and it sized the previews' tiles and the HLS grid and playlist.
  Previews of it fail with that reason, and an HLS playback of it is
  `PlaybackFailed` saying so. Direct play and a progressive conversion
  are unchanged.
- Connections from devices that are not signed in get small limits
  (PX-10, PX-09 findings 7 and C-3). A device with no live session (a
  *stranger*) may open 4 bidirectional streams at once and send at most
  512 KiB unread, with request frames of at most 64 KiB; a device with a
  live session gets 100 streams and 16 MiB, on the connection it signs
  in on (`LoginPassword`, `FinishPasskeySignIn`, an approved
  `AwaitDeviceSignIn`) and on every later one. No client may open a
  unidirectional stream. A stream whose kind byte and first frame do not
  arrive within `network.first_frame_timeout_secs` (10) is reset with
  `reset::FIRST_FRAME_TIMEOUT`. At most `network.max_stranger_connections`
  (64) stranger connections are held; one more is closed with
  `close::BUSY`, and a signed-in device is always let in. When a device's
  last live session ends (sign-out, revoke, "sign out everywhere else",
  too many wrong PINs, its user disabled, or expiry, checked every
  minute), its connections are closed with `close::SIGNED_OUT` once its
  replies are delivered (at most 2 s), and it reconnects as a stranger.
  Connections that send nothing for 15 s are dropped (iroh keeps alive
  every 5 s). Protocol: `close::BUSY`, `close::SIGNED_OUT`,
  `reset::FIRST_FRAME_TIMEOUT` and `codec::read_frame_bytes_within` are
  new.
- One user or device can no longer fill the mutation replay pools (PX-10,
  PX-09 findings 6 and C-2). A pending mutation reserves what its kind of
  reply can take (1 MiB for `StartPlayback`, `ChangeSubtitle` and
  `SeekPlayback`, 64 KiB for passkey ceremony options, 16 KiB for the
  rest) in place of 1 MiB for every one, and a finished one its
  outcome's size. Each endpoint and each user may hold an eighth of a
  pool's entries and bytes: 512 and 8 MiB of the signed-in pool, 64 and
  1 MiB of the anonymous one. Anonymous outcomes are replayed for 30 s
  once known, not for the operation's whole expiry. Over its share, a
  mutation gets `RateLimited` (retry after 1 s).
- A full sign-in limiter no longer refuses everyone (PX-10, PX-09 findings
  9 and A-4). When a new username or source finds its 100,000 keys held,
  the keys out of the window go first, then those whose last failure is
  oldest. Sign-in attempts (`LoginPassword`, `FinishPasskeySignIn`) from
  devices that are not signed in now share one budget: a burst of 20,
  then 5 a second. Past it they get `RateLimited`. A signed-in device
  adding a user does not draw on it.
- Password hashing and checking run at most once a core at a time
  (PX-10, PX-09 findings 10 and A-5): logins, PIN checks and password
  changes wait for a free core instead of each starting Argon2 (about
  19 MiB) on the blocking pool. The admin API's JSON-RPC refuses batch
  requests (C-5): one message could carry any number of calls. No
  Spoolframe client sends them; a caller that did gets an error for the
  batch.
- Anonymous callers can no longer fill the sign-in tables (PX-10, PX-09
  findings 16, 17, A-6 and C-6). `Hello`'s `client_name`,
  `client_version` and `device_name` are cut to 128 characters
  (`MAX_HELLO_TEXT`), not refused: what the server keeps, lists as
  connected and shows to an approver is the cut text. At most 256
  device sign-ins are pending on the server (`MAX_PENDING`; a device
  still has at most 3), past which `BeginDeviceSignIn` is `RateLimited`.
  One remote address may hold at most 16 web passkey sign-ins
  (`/auth/passkey/begin`, 429 past it) and 16 SSO flows
  (`/auth/oidc/begin`); a caller with no source address (loopback, or an
  untrusted proxy) is not counted apart. A stranger's
  `BeginPasskeySignIn` draws on the stranger sign-in budget, as its
  `FinishPasskeySignIn` does.
- A password change or an admin's reset closes the user's other devices'
  connections at once with `close::SIGNED_OUT` (PX-10, findings 4 and
  7), as a sign-out or a revoke does, where they waited for the
  minute's recheck.
- An app passkey sign-in can no longer be relayed from one server to
  another (PX-10, PX-09 finding 2). Every server shares the relying party
  `spoolframe.app`, so a malicious server could hand an app the challenge
  of another server and sign in there with what the app signed. Now a
  server sends a nonce, and the challenge is SHA-256 of a tag, the
  server's endpoint id and the nonce: the server uses its own id, the SDK
  the id it pinned (`spoolframe_protocol::app_passkey_challenge`). This
  applies to sign-in, confirmation and registration. Wire change:
  `Reply::PasskeyRequest` and `Reply::PasskeyCreation` are now
  `PasskeySignInBegun` and `PasskeyRegistrationBegun`, with `nonce` in
  place of `challenge`. The SDK still gives apps a `PasskeyRequest` and a
  `PasskeyCreation` with the challenge in them, so apps change nothing.
  `BeginPasskeySignIn` with a `username` no longer lists that user's
  credential ids to anyone who asks; only that user's passkeys finish it.
- The SDK reads the server's close codes (PX-10). A connection closed
  with `close::SIGNED_OUT` ends every session the SDK keeps for that
  server, and a call in one of them is `SdkError::SignInRequired`: a call
  it cut off, and each call after it, at once and with no dial. Before,
  it was `Unreachable` or `TimedOut`, and only the retry learned the
  session was gone. The SDK no longer reconnects as a stranger after a
  sign-out; the state goes to `Disconnected` and the next call dials.
  `Client::signed_out` returns a `SignOutWatch` that fires on each such
  close, after the sessions are ended, so an app can show "Who's
  watching" with no request in flight. A connection refused with
  `close::BUSY` is the new `SdkError::ServerFull` (try again later), not
  `Unreachable`; a reconnect refused so stops. Session-less calls cut
  off by a sign-out are retried on a new connection as before.
- Trusted devices (P4-01): a device a user chose to remember opens their
  profile again without a password once their session has ended. The
  device's Iroh key is the proof, so it works with the Internet down.
  Wire revision 4 (`PROTOCOL_VERSION` 4):
  - `LoginPassword`, `FinishPasskeySignIn` and `BeginDeviceSignIn` take
    a required `remember`. It is never true unless the user ticked
    "Remember this device".
  - `ApproveDeviceSignIn` takes the approver's `remember`. A device is
    trusted only if it asked and the approver agreed.
    `DeviceSignInRequest.remember` says that the device asked.
  - `LoggedIn` says whether the device is `remembered`.
  - The new pre-auth `TrustedProfiles` lists this device's own trusted
    users, and `ResumeProfile` opens a session for one of them. Every
    refusal is `Unauthenticated`. A device that requires the PIN gets a
    locked session. A resumed session is not a recent sign-in: its user
    confirms before a sensitive change.
  - The new `SetDeviceTrust` turns trust and auto-login on for this
    device, after a recent sign-in. It turns trust off for any of the
    user's devices, and that ends the user's sessions there.
  - `DeviceInfo` has `trusted` and `auto_login`.

  At most one user per device has auto-login. Auto-login turns the PIN
  off there, and the PIN turns auto-login off. These end trust: signing
  out, a session ended by its user or an admin, "sign out everywhere
  else", ten wrong PINs, any password change or reset, and disabling the
  user. Trust unused for `auth.trust_idle_days` (default 180) lapses.
  Migration 0014 adds `trusted_endpoints`. SDK: `remember` on `sign_in`,
  `finish_passkey_sign_in`, `begin_device_sign_in` and
  `approve_device_sign_in`. New calls: `Client::trusted_profiles`,
  `Client::resume_profile`, `Session::set_device_trust` and
  `Session::remembered`.
- Trusted devices in the admin API and the CLI (P4-01, for the web UI's
  P4-03). `DeviceEntry` (`me.devices`, `admin.userDevices`) has
  `trusted` and `autoLogin`. `DeviceSignIn` (`me.deviceSignInLookup`)
  has `remember`: the device asked to be remembered.
  `me.deviceSignInApprove` takes the approver's `remember`. The web UI
  sends `false` until P4-03 shows the box. `me.setDeviceTrust(endpointId,
  trusted, autoLogin)` works for any device the caller has signed in on.
  `admin.userSetDeviceTrust(username, endpointId, trusted, autoLogin)`
  works for any user's device. Both need a recent sign-in in either
  direction (`RECENT_SIGN_IN_REQUIRED`). A device the user never used is
  `NO_SUCH_DEVICE`, and auto-login without trust is `INVALID_INPUT`.
  Stopping trust ends the user's sessions on that device. `user devices`
  shows "trusted" and "opens at start". `user trust <name> --device
  <name|prefix> [--off | --auto-login]` sets trust.
- Trusted devices on the web (P4-03):
  - The web approval page (`/auth/device`) shows "Remember this device",
    ticked, when the device asked to be remembered. Approved as is, the
    device is trusted; unticked, it signs in just this once. A device
    that did not ask shows no box and is never trusted. Approving after
    the recent sign-in has lapsed asks "Confirm it's you", then approves.
  - My profile's "Devices and sessions" lists each device the user has
    signed in on, with or without a live session, beside the browsers'
    and the CLI's sessions. Each device has three switches: PIN, Trusted
    (with a Trusted pill) and Auto-login. A browser's row shows "—" for
    each. The Profile PIN card keeps only setting, changing and removing
    the PIN.
  - An admin's user page shows the same three switches per device, made
    through `admin.userSetDeviceTrust` and `admin.userSetPinRequired`.
  - On both pages, "Stop trusting" asks first. It says that the user is
    signed out on that device at once and needs a full sign-in there.
    Turning auto-login on says first that the PIN is no longer asked for
    there and that the device no longer opens anyone else at start.
    Asking for the PIN where auto-login is on says first that auto-login
    turns off. Each change that needs a recent sign-in asks "Confirm it's
    you", then is made. A toast says what was done.
  - Signing out a trusted device from My profile says that it is no
    longer trusted, since P4-01 ends trust on any revocation of the
    device's session.
- Saved servers remember every direct address (PX-12). The SDK kept only
  the addresses of the last connection's paths, and each connect replaced
  them: a phone that last reached the server over its public IPv6 forgot
  the address it had reached it at over a VPN. Now each saved server keeps
  a set of addresses, each with when it was last seen. A connect adds
  every address it reached and never removes any. The server announces its
  addresses inside the connection, and iroh opens a path to each one it
  can reach, then closes all but the best within a millisecond. The SDK
  follows those path events for as long as the connection is open, so an
  address reached only for that moment is saved too. An address the server
  announces that this device cannot reach then (a VPN address while the
  device is off the VPN) is not saved: iroh does not show it. An address
  not seen for 30 days is dropped at the next sighting, and only the
  newest 32 are kept. Every one is dialled, with the addresses given by
  the app and heard on the LAN, and iroh takes whichever answers. A
  servers file from an older SDK keeps its addresses, as seen at the first
  load. `ServerEntry.addrs` lists them all, the address in use first. No
  wire or API change.
- `Client::profiles` lists the sessions left when the server signs the
  device out while it runs (PX-10). It read the kept sessions, then asked
  the server after each one; a `close::SIGNED_OUT` that landed between the
  two made that ask `SdkError::SignInRequired`, and the whole list failed
  with it. Now that session is left out and its profile needs a password,
  as for one the server answers `Unauthenticated`. No API change.
- A signed-in device is told the server's addresses (PX-12). Iroh shows a
  client only the paths that opened, so a phone that never reached the
  server's VPN address could not learn it, though iroh announces it inside
  the connection. The new `Request::ServerAddresses` answers
  `Reply::ServerAddresses { addrs }`: every address on the server
  endpoint's interfaces, with its port, but loopback, link-local and
  unspecified ones. The addresses go as text on the wire. Only a live
  session is told (`Unauthenticated` otherwise), never `Hello`; a locked
  profile is told too. No new revision: a server without it answers
  `Unsupported`. The SDK asks after each sign-in or resume, and on each
  new connection when it keeps a session there, and saves what it is told
  with the server's addresses, marked as told. The same 30-day age-out and
  32-address cap apply, but a told address never pushes out a reached one:
  the newest reached addresses are kept first, and told ones fill the room
  left. A told address that a connection reaches becomes reached. A
  servers file from before keeps its addresses as reached. No API change.
- `network.announce` chooses which addresses a signed-in device is told
  (PX-12). The server told every address on every interface, and on a
  host with Docker that includes its bridges (172.17.0.1, 172.18.0.1),
  which no device reaches. Told now are the addresses on
  `announce.interfaces` (each interface when empty), less those on
  `announce.ignore_interfaces`, less those in `announce.ignore`
  (addresses and CIDR ranges), plus `announce.add` (`ip` with the
  server's port, or `ip:port`), which is told even when an ignore matches
  it. Interface names take `*` and `?`. `ignore_interfaces` is
  `["docker*", "br-*", "veth*", "virbr*"]` by default, the others empty.
  A bad address, range or empty name is a config error naming the key.
  Restart settings; the interfaces are read each time a device asks.
  `spoolframe-server status`, the web UI's server page and the start-up
  log show the addresses told now (`ServerStatus.told_addresses`, a new
  required field).
- A path mapping for every library saves (P3-18). The wizard's Paths
  step failed on Continue with "value: null is not a value" for a
  mapping added by hand: the admin API's `PathMappingEntry.library` is
  `None` there, which serde writes as `"library": null`. A setting's
  value now leaves out a `null` member of an object, as a missing key; a
  `null` setting, or one in a list, is still refused.
- The wizard's Paths step keeps saved mappings that no library folder
  is under (P3-18). Its rows show only the folders of the libraries
  served, and Continue saved only the rows and the mappings added there,
  so a mapping for a library not served yet, or for a prefix no folder is
  under, was erased. Those now load as extra rows, under their library
  or "Every library"; with the check failed, every saved mapping does.
- Server › Media paths keeps its last check through a library reorder
  and a restart (P3-18). The check was kept against the catalog object
  it ran on, and any library change (even a reorder) or a restart
  replaced that, so the page lost it. It is now kept while what it read
  is the same: the path mappings, and each served Plex section's key,
  title and folders (`PlexCatalog::paths_basis`, asked of Plex when the
  page loads). A changed mapping or folder still drops it. It is saved
  in the state directory as `last-path-check.json` (mode 0600, written
  whole through a temporary file), holding a hash of what it read rather
  than the paths.
- The NixOS module and the server are hardened (PX-11 findings 32-36,
  38):
  - The module locks `admin.insecure_cookies` off. The new
    `admin.allowInsecureCookies` option leaves the setting to the web UI.
    The server refuses a change that turns insecure cookies on while
    `admin.public_url` is `https`, or that sets an `https` public URL while
    they are on. This applies to `settingSet` and to the setup code's page
    (`/auth/setup/check`), which answers 400. A config file that already
    has both still only gets the start-up warning.
  - The module's `admin.listen` default is now `127.0.0.1:7421`, not every
    IPv4 interface. Set it to another address only with a TLS proxy in
    front. `null` still gives the server's own default.
  - The unit gets `UMask=0077`, `MemoryHigh`/`MemoryMax`/`TasksMax` (the
    new `resources.*` options, 6G/8G/4096 by default), a system call
    filter (`@system-service`, without `@privileged` and `@resources`, but
    with `setpriority` and `ioprio_set`, which the previews use; refused
    calls fail with EPERM), `SystemCallArchitectures=native`,
    `ProtectProc=invisible`, `ProcSubset=pid`, `ProtectClock`,
    `ProtectHostname`, `ProtectKernelLogs`, `RestrictSUIDSGID`,
    `RemoveIPC`, and `PrivateUsers` when no GPU is used. `ProtectHome` is
    now `true`; the new `mediaUnderHome` option makes it read-only again.
    VA-API gets only its render node (`transcode.vaapiDevice`), not every
    DRM node. `MemoryDenyWriteExecute` is not set: GPU drivers make code
    at run time. The VM test scores the unit with `systemd-analyze
    security`.
  - The server sets its umask to 077 at start. The database is created
    0600, and an existing database, WAL and WAL index are made 0600 at
    start. The backup before migration 0012 is 0600. The cache
    directories are made 0700, ones an older server left open included.
  - The module opens the Plex discovery port (UDP 32415) only with
    `openFirewall`, or on the interfaces in the new
    `plexDiscovery.interfaces` option. Before, it opened it on every
    interface whenever the firewall was on. The server now takes a GDM
    answer only from a network of the interface it came in on, or from
    one of the host's own addresses.
  - The server warns at start, and the module warns at evaluation, when
    the admin listener is on loopback, `public_url` is set and
    `trusted_proxies` is empty: a proxy on the same host that the server
    is not told about.
- The admin listener's answers are hardened (PX-11 findings 21, 37 and
  13):
  - Insecure cookies (`admin.insecure_cookies`) drop only `Secure` now.
    The session and sign-in provider cookies stay `HttpOnly`, so page
    scripts cannot read them over plain HTTP either. The web UI's texts
    about insecure cookies say so.
  - Every answer carries `X-Content-Type-Options: nosniff`,
    `X-Frame-Options: DENY` and a content security policy fitted to the
    web UI: `default-src 'self'; script-src 'self' 'wasm-unsafe-eval';
    style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:;
    font-src 'self'; connect-src 'self'; object-src 'none';
    base-uri 'none'; form-action 'self'; frame-ancestors 'none'`. The
    wasm needs `wasm-unsafe-eval`. The app sets `style` attributes and
    puts its font faces in a `<style>` element, which needs
    `unsafe-inline` for styles. Pages that a debug build takes from
    `dx serve` get no policy, so hot reload keeps working. `/auth/*`,
    `/jsonrpc` and every JSON answer get `Cache-Control: no-store`.
  - `/catalog/artwork` serves only JPEG, PNG and WebP. Any other type
    from Plex, SVG included, gets 502 and a warning in the log.
- The SDK passes on a server's `web_url` only when it is an `http` or
  `https` URL with a host (PX-11, PX-09 finding 18). Any other one in
  `ServerInfo` (`Session::web_url`) or `DeviceSignInStarted`
  (`DeviceSignIn::web_url`), such as `file:` or an app's own scheme, is
  dropped with a warning, as if the server had no web UI.
- `deny.toml` and `deny-sdk.toml` list the two accepted advisories with
  their reasons (PX-11 finding 39, PX-09 C-15): RUSTSEC-2023-0071 (`rsa`,
  used only to verify OIDC ID token signatures) and RUSTSEC-2024-0436
  (`paste`, an unmaintained build-time proc-macro). `cargo deny --locked
  check advisories` now passes on both, so a new advisory fails it.
- The web UI's approval page (`/auth/device`) follows the WebApproveStates
  board in every state (P4-03):
  - Signed in, it asks `Sign in "<device>"?` with Deny and Approve. It
    shows the code big, and the server's name, host and key fingerprint
    in a box, to be matched against the device's screen.
    `deviceSignInLookup` now answers with a required
    `serverFingerprint` for this.
  - "This device will be remembered" says whose profile the device can
    open, and that unticking signs it in just this once.
  - Opened without a code, it takes the code in a big centred field, with
    "8 letters. Dashes and lowercase are fine."
  - A wrong code says "That code doesn't work" and keeps the code in a red
    field, with Try again. Too many wrong codes counts the wait down
    ("Try again in 11:20") with the field and button disabled.
  - Signed out, it shows the code around the sign-in form and says that
    signing in comes straight back to it.
  - Approved and denied end on a big check or circled x, with no button
    to the web UI.
- The web UI's trust dialogs and toasts use the WebTrustStates board's
  words (P4-03). "Open your profile automatically on <device>?" with Turn
  on, "Ask for your PIN on <device>?" with Turn PIN on, and the shorter
  "Stop trusting" text. On an admin's user page, the dialogs name the user
  by display name ("Stop trusting Alice's Living room Apple TV?") and say
  the user is not told by email. The toasts read "… · PIN off there" and
  "Stopped trusting <device> · signed out there". "Confirm it's you" says
  it is about "changing how your devices sign in". Each dialog has the
  board's icon: play, lock, or a yellow shield.
- My profile's Devices and sessions card follows WebProfile and
  WebProfilePhone (P3-03):
  - The column headers are small caps.
  - A browser's row is named for the browser where it is this one
    ("Firefox on Linux", from its User-Agent), and "Web browser"
    otherwise. Its line reads "Web browser · 3 days ago".
  - The card ends with the board's footnote about trust, auto-login and
    confirming it's you. The extra "Sign out everywhere else" button is
    gone, as the board has none. Each row still signs its own sessions
    out.
  - At phone width, "This device" or Sign out sits at a row's top right,
    the switches are labelled under them, and a browser's row says "A
    browser can't be trusted or open by itself."
  - The "Trusted" pill beside a device's name is gone: the switch says
    it.
- An admin's user page lays out the user's devices as WebUserSessions
  and WebAdminPhone do (P4-03). It has the board's description, and a
  Clear PIN button on each row (the PIN is one per user, so it clears it
  everywhere, as its dialog says). The "Trusted" pill is gone. At phone
  width each device is a card with its switches labelled under them,
  and the bar's title is the user's display name. A session is named as
  My profile names it: "Web browser" and "Command line" instead of
  "Browser" and "web / CLI".
- The device switches explain themselves (P4-03). On My profile's
  Devices and sessions and on an admin's user Devices, a legend under the
  table or cards says what PIN, Trusted and Auto-login do, each name in
  bold. It is where the footnote was. The admin's legend says "this
  user's profile". The 180 days a trusted device may go unused is the
  server's default: the admin API does not expose
  `auth.trust_idle_days`. Each column head, phone label and switch has
  its legend line as its description (`aria-describedby`) and as its
  `title`. Each switch that cannot be pressed has a visible reason. With
  no PIN, the PIN line says how to get one (My profile) or that no device
  can ask for one (admin). Auto-login's line says it needs
  Trusted. On My profile, a last line says a browser can't be
  trusted or open by itself, which a wide row shows only as dashes.
- A user's device can be deleted (P4-03). Signing a device out kept it
  in the user's devices, since a revoked session still lists it. Deleting
  it deletes the user's sessions there, live or revoked, with its trust
  and PIN requirement for them, in one transaction. It leaves the list,
  and if it signs in again it is new. Other users' sessions, trust and
  PIN requirement on that device are kept. The device's connection
  closes with `SIGNED_OUT` once no user's session is left on it.
  - Admin API: `me.forgetDevice(endpointId)` and
    `admin.userForgetDevice(username, endpointId)`. Both always need a
    recent sign-in (`RECENT_SIGN_IN_REQUIRED`); a device the user never
    used is `NO_SUCH_DEVICE`.
  - Client protocol: the new `ForgetDevice { device }`, answered `Done`.
    It needs a session and a recent sign-in (`RecentSignInRequired`), is
    a mutation, and a locked profile cannot send it. This device is
    `InvalidInput` (sign out with `Logout`); a device that is not the
    user's is `NotFound`. An added request: `PROTOCOL_VERSION` stays 5.
  - SDK: `Session::forget_device(device)`.
- Delete on the web (P4-03, canvas v75). My profile's Devices and
  sessions has Delete beside Sign out on each app's device, signed in or
  not; a browser's row and this device have none. An admin's user
  Devices has it beside Clear PIN on each device, and at phone width in
  each device card's header. Delete is red words with a trash can and no
  border, quieter than Sign out. It asks "Delete <device>?" ("Delete
  Alice's <device>?" on an admin's page): "It is signed out and
  forgotten. If it signs in again it shows up as new." with Cancel and a
  red Delete. Then a toast says "Deleted <device>" and the list is read
  again. A session past its recent sign-in asks "Confirm it's you"
  first, then deletes. My profile's rows switch to the wide layout at
  600px instead of 520px, so that Sign out and Delete fit side by side.
- The Server page follows WebServer and WebAdminPhone (P3-02):
  - LAN discovery is a switch that sets `network.mdns`. That setting
    applies at the next start, so the page says "Applies after a
    restart". Once it is changed, the page offers Restart now where the
    server runs under a supervisor. A value the config file sets cannot
    be changed here.
  - An IPv6 address wraps only between its groups, and its last group
    stays on the line with the port.
  - "Told to devices" lists its addresses as Addresses does, with a line
    saying what they are.
  - At phone width the card shows only the name, the version, the full
    fingerprint under its label, and a Copy key button.
- The users list follows WebUsers (P3-02):
  - Admins come first.
  - An admin's avatar is teal. Anyone else's tone follows the letter the
    avatar shows, so Alice is mauve and Steve is peach, as on the
    boards, on every page.
  - Under the table, "<user>'s devices" shows a tile for each device
    the picked user signed in on. Each tile has the key's start, the
    kind, when it was last seen, that user's sessions on it with
    Revoke, and "also: …" for other users on the same device. The admin's
    own devices show first. Clicking a row, or its device count, picks
    another user.
- The admin sidebar and phone drawer have the boards' Overview and Plex
  items (P3-02, P3-12). Overview (`/admin`) is now an admin's start page.
  It shows four tiles (playing now, users, uptime, and whether Plex
  answers), what plays now, the server's name, fingerprint and version
  with Copy server key, and each library with its state. It reads again
  every 15 seconds. Plex opens Settings › Plex, where the Plex Media
  Server and the admin's Plex account are. That tab now marks Plex in
  the sidebar, not Settings.
- Server › Libraries follows WebLibraries (P3-11). Library names are
  white, turning teal only when pointed at. The column headers are 12px
  subtext with no row styles mixed in. The watch sync choice is 96px
  wide, and rows are as tall as the board's. The note about the first
  change saving the full list of libraries now says plainly what
  happens.
- The setup wizard's Done card follows WebWizardDone (P3-10). The
  fingerprint is in monospace. The Playback line names the encoder
  Automatic picks ("Automatic (VA-API) · 2 encodes at once"). Missing
  files are one warning beside Paths: the library, or how many
  libraries. At phone width the labels narrow and the warning goes
  under the value, so no line breaks inside a word.
- Smaller web UI fixes from the browser review (P3-18):
  - The reset-password dialog says, as WebUsersStates does, "Alice will
    be signed out on every device, and needs the new password to sign in
    again", naming the user by display name.
  - My profile's Plex account card has a teal outline icon, as every
    other card does.
  - The wizard's button that removes a path mapping has an accessible
    name.
  - Password fields are in forms: setup's password, Plex token fields,
    and OpenTelemetry header values.
  - `GET /auth/me?or=null` answers `null` when no one is signed in. The
    web UI asks that way, so a signed-out page no longer logs a failed
    401 request in the browser's console. Without the query it is 401,
    as before.
- An app passkey signs in, or confirms (P3-19, P3-07), with an empty
  `user_handle`. The Linux client gets no user handle from a phone over
  caBLE, so it sends an empty one, and the server refused every such
  sign-in. Now an empty handle is absent: the passkey's credential id
  says whose it is, and the signature, challenge, counter and failed-login
  limit are checked as before. A confirmation still takes only the
  session's user's passkey. A handle that is sent must still be the
  passkey's user's. The web UI's passkeys still need one.
- `FinishPasskeySignIn.user_handle` is now `Option<Vec<u8>>` (P3-19).
  No handle is CBOR null, and the key is still required; a handle is a
  CBOR byte string, as before. The server took an empty handle as no
  handle; now it is `InvalidInput`, which uses no challenge and counts as
  no failed sign-in. The client SDK's `finish_passkey_sign_in`
  and `finish_confirm_passkey` send an app's empty handle as `None`.
  `PROTOCOL_VERSION` stays 4: see "The user handle" in PROTOCOL.md.
- Each user chooses how picture subtitles (PGS, VobSub, DVB) are shown:
  read as text (OCR), the default, or shown as pictures. The choice is
  `subtitles.bitmap` in the track preferences (`"text"` or
  `"pictures"`), kept per user, so it follows the user to every device.
  When an app asks for `bitmap: auto`, the server reads the track as text
  if it can, else the app draws the pictures, else they are burned in. A
  user who wants pictures has the app draw them, or has them burned in.
  An app's explicit `text` or `burn_in` still wins. With the server's
  `subtitles.ocr` off, pictures are shown whatever the preference. Before
  this, `auto` let an app that draws PGS draw it, and read the track as
  text only for apps that cannot. The admin debug plan's `auto` plans as
  for a user who never chose.
- The track preferences say whether the server offers picture subtitles
  as text. `GetTrackPreferences` and the admin API's
  `me.trackPreferences` now answer the preferences with `ocr_offered`
  (`ocrOffered`), true when `subtitles.ocr` is on and tesseract has a
  language. Apps show the picture subtitle choice disabled when it is
  false. The SDK's `track_preferences` returns `TrackPreferencesInfo`.
  The reply's encoding changed: update the server and SDK together.
- The web UI's OCR switch moved from the Plex form (the wizard's Plex
  step and Settings › Plex) to the Playback settings and the wizard's
  Playback step, beside the OCR languages. It reads "Offer picture
  subtitles as text (OCR)": "Picture subtitles (PGS, VobSub, DVB) can be
  read into text, so any device can show them styled and sized. Each user
  picks what they see." The note under it says what users get when OCR
  is off or tesseract has no language. My profile › Playback has a new
  row, "Picture subtitles": Read as text (OCR) or Show as pictures. When
  the server does not offer OCR, the row is disabled, shows pictures, and
  says why.
- Each server has a server id: 80 random bits, written as 16 lowercase
  base32 characters, a DNS label. The server makes it at its first start
  and keeps it in `server.id` in its state directory (mode `0600`),
  beside its key. A new key keeps it. `spoolframe-server id` prints it
  under the endpoint id, each line labelled, and the server logs it with
  its fingerprint. A `server.id` that is not one stops the server.
- `PROTOCOL_VERSION` is 5 (PX-14): update the server and SDK together.
  `ServerInfo` has a new required field, `server_id`. Revision 5 also
  covers PX-13's change to the `GetTrackPreferences` reply (now a
  `TrackPreferencesInfo` with `ocr_offered`), which shipped without a
  bump. A mismatched
  build is now refused at `Hello`, not on one request later.
- Admins see the server id (PX-14). The admin API's `serverStatus` has
  a required `serverId`. `spoolframe-server status` prints it and the
  passkey domain, `<server id>.spoolframe.app`. The web UI's Server page
  has a "Passkey domain" row, under the fingerprint, with a copy button:
  the domain a password manager lists the app passkeys under.
- The SDK's pin is the server's key and its server id (PX-14). Both are
  pinned at the first connect; a server saved before takes its id at the
  next connect. A new name is saved without asking. A new key with the
  pinned id is `IdentityChanged`, and `trust_new_identity` keeps the id.
  The pinned key with a new id is the new `SdkError::ServerIdChanged`,
  refused every time. A new key and a new id are another server: a name
  no longer marks a server with an id pinned as replaced.
  `ServerEntry` has `server_id`.
- App passkeys have a relying party per server (PX-14):
  `<server id>.spoolframe.app`, at `https://<server id>.spoolframe.app`,
  for registration, sign-in and confirmation. A security key with
  passkeys for two servers, or two accounts, offered all of them to a
  sign-in that listed none, and a server refused the one the client
  took; now it offers only that server's. The server refuses another
  relying party's hash and any other origin, `https://spoolframe.app`
  too. The challenge stays bound to the pinned key (PX-10), as a server
  can name another's id.
- `PasskeyCreation` and `PasskeyRequest` carry `rp_id` and `origin`,
  which the SDK makes from the pinned server id; `APP_RP_ID` and
  `APP_ORIGIN` are gone. A passkey's account name is
  `<username> @ <server name>`; its display name is the user's own.
- App passkeys made for `spoolframe.app` are kept. They sign in nowhere,
  are listed as app passkeys that are not usable, and My profile says to
  remove them and add them again in the app. The server logs them at
  start. `LEGACY_APP_RP_ID` and `is_app_rp_id` name them.
- Home has one row per show (P1-14). While an episode of a show is in
  progress, the show is in Continue watching and has nothing in Next up;
  finishing that episode puts the one after it in Next up. Every episode
  in progress counts, also one past the length of Continue watching.
  Before, the next episode could be in both rows (it was the one in
  progress), and a show with a later episode in progress was in Next up
  at the episode after an older one watched.
- "Remove" removes only its card (P1-22). `HideFromContinueWatching`
  with an episode hides that episode, from Continue watching or Next up,
  not its show. The show's other episodes stay. A removed Next up episode
  stays out until it is watched, and then the one after it is next up. A
  removed episode in progress does not keep its show out of Next up.
  Playing or marking a removed item brings it back, as for a movie; a
  write for another episode of the show no longer does. The wire is
  unchanged: apps already send the card's own id. Migration 0015 drops
  the hides made per show before, so those shows come back to Home once.
- Every field of the client protocol, the admin API and the models they
  carry is required (PX-15). Before, about 70 fields had serde defaults,
  among them `SessionEntry.device_name`, the `ServerStatus` fields and
  all of `TrackPreferences`, so a peer that left a field out was read as
  if it sent none. Now it is refused, with the missing field named. A
  field with no value is sent as `null`. `PROTOCOL_VERSION` stays 5:
  the server, the SDK and the Linux app already send every field, so
  their bytes do not change. The web UI now sends `return_to: null` to
  `/auth/oidc/begin` when there is none. Stored track preferences from
  before a field existed still load: the server fills in the defaults
  when it reads a row. Device-local files (the SDK's saved servers and
  sessions) keep their defaults.
- ffmpeg, ffprobe and tesseract run in a sandbox of their own (PX-11
  finding 34). The new `media.sandbox` setting is `required`, `auto` (the
  server's default) or `off`; `media.bwrap` names bubblewrap (`bwrap` on
  `PATH` by default).
  - Confined, each run is in a bubblewrap sandbox: new user, mount, PID,
    network, IPC and UTS namespaces, so no network at all; a read-only
    root with only the system's programs and libraries in it; no state
    directory, `/proc`, `/tmp` or `/home`. Each input file (the film, a
    subtitle file beside it) is opened by the server and bound in by its
    file descriptor, symlinks followed, and refused if it leads into the
    state directory or systemd's credentials. The job's own directory is
    the one writable path. A seccomp filter refuses new processes (only
    threads), sockets other than Unix ones, and `ptrace`, mounts,
    namespaces, keyrings, `bpf`, `io_uring` and the like. A run that may
    use the GPU also gets its device nodes, `/sys` and
    `/run/opengl-driver`.
  - `required` stops the server at start when bubblewrap cannot confine a
    program; `auto` then runs the tools unconfined and logs an error
    saying so; `off` never confines them, with a warning.
  - Confined or not, a run's environment is cleared to a short list
    (`PATH`, the GPU drivers' and tesseract's variables), with `HOME` and
    `TMPDIR` in its own directory. It gets limits it cannot raise:
    memory (`RLIMIT_DATA`, which grows with the core count: an encode
    2 GiB and 512 MiB a core), CPU time, the largest file it may write,
    and no core dumps. It runs in a process group of its own, and an
    encode is paused, resumed and stopped as a group.
  - `media.*` settings now need a recent sign-in, as `admin.*` and
    `auth.*` do.
  - The nix package puts bubblewrap on the server's `PATH`. The NixOS
    module has a new `media.sandbox` option, `required` by default; with
    it on, the unit allows the user, mount, PID, network, IPC and UTS
    namespaces and the `@mount` and `capset` calls, and sees all of
    `/proc` (`ProcSubset=all`), which bubblewrap needs. The dev shell has
    bubblewrap, and the tests' real ffmpeg, ffprobe and tesseract run
    confined.
  - An encode that ended at once could lose its last progress lines, and
    so its output time; they are read after it ends now.
- A client no longer sees ffmpeg's own words when a conversion fails
  (PX-09 finding 23): they named the server's scratch directory, the
  playback's handle and where ffmpeg is installed. Its reads get
  `PlaybackFailed` with "the conversion failed on the server", and one
  that cannot run ffmpeg at all "the conversion could not be started on
  the server". The server's own reasons (stalled, scratch space full,
  ended by an admin) are sent as before. ffmpeg's last lines stay in the
  log and on the admin's playback page.
- The setup code is no longer sent to the OTLP collector (PX-09 finding
  20): whoever read the logs there could set up a server that has no
  users yet. It is still logged at warn to stderr, the journal and the
  Logs page, under the target `spoolframe::local_only`, which the export
  never takes.
- Without `admin.public_url`, the web UI and the admin API take a browser
  only at the listener's address or `localhost` (PX-09 finding 22). A page
  at a name made to resolve to the listener (DNS rebinding) sent that name
  as both `Host` and `Origin`, which passed. Reaching the web UI by a name
  now needs `admin.public_url`; the refusal's log line says so.
- Wrong passwords from elsewhere no longer lock a user out where they
  signed in before (PX-09 finding 24). Five failures for a name still
  refuse it for 15 minutes from any new place, and a guessing source is
  still refused for every name; but the last 16 places (endpoints,
  addresses) a right password, passkey or PIN came from are not refused
  for the name's own failures. The setup code is now limited per caller:
  five wrong codes from one place no longer keep the owner from setting
  the server up.
- A preview sheet is served only while its film is (PX-09 finding 25).
  Its id was enough to fetch it for as long as the cache kept it, after
  the film's library stopped being served too. The server now checks the
  catalog still has the item the previews were last asked for, and that
  its file is the one they were made of. After a restart a client asks
  for the previews again before their sheets, as players do.
- A PMS's answer is read only up to its limit (PX-09 finding 26): 64 MiB
  for JSON, which had none, 256 MiB for a preview index and 32 MiB for an
  image, refused as soon as it passes it. Before, the whole answer was read
  first, so a malicious PMS could make the server hold whatever it sent
  in 15 seconds. A preview sheet's first tile must be at most 320 pixels
  wide and twice as tall as wide: its height is the film's shape, which a
  crafted file sets, and a sheet is ten by ten of it.
- What one consumer can start is bounded (PX-09 finding 15, with the
  timeouts of PX-10): at most 4 text subtitle conversions run at once,
  and at most 64 subtitle tracks are being made or waiting (past that,
  one more is "try again later", not queued); the queue of previews asked
  for keeps the newest 16; a user can begin 5 Plex sign-ins in 15
  minutes (`plexLinkBegin` then answers `RATE_LIMITED` without asking
  plex.tv), and keeps at most 5 PINs.
- A media or artwork stream whose reader takes nothing for
  `network.stalled_read_secs` (new, 120 by default) is reset (PX-09
  finding 14). It held its file open, and what waited to be sent, for as
  long as the connection lived, so one client could pin as many as it
  could open streams.
- Without `admin.public_url`, web passkeys take only the listener's own
  port as their origin, `http://localhost:<port>` (PX-09 finding 28). Any
  localhost port was taken, so a page on another one (a dev server,
  another app) could have the user's passkey sign and relay the sign-in
  here. An SSH tunnel must keep the listener's port.
- The web UI's device approval says where the device asked from (PX-09
  finding 27): "The TV asked from 192.168.86.20." For a public address
  it warns instead: the device is on the internet, not on the
  approver's network, so approve only one you can see; a code someone
  sent you is theirs. `deviceSignInLookup` has the new `address` and
  `fromInternet`.
- ffmpeg, ffprobe and tesseract are confined with Landlock and seccomp,
  not bubblewrap (PX-11 finding 34). Each run confines itself between
  fork and exec, with no namespace, so the NixOS unit keeps its full
  hardening again: `RestrictNamespaces=true`, `ProcSubset=pid`, and no
  `capset` or `@mount` in `SystemCallFilter`; `systemd-analyze security` scores
  it 1.6 again (2.5 with bubblewrap), and the VM test's threshold is
  that 1.6.
  - A confined run may read and run the system's programs and libraries,
    read each input file (the inode the server opened and checked) and
    the directories it was given, and write its own directory, whose
    files it cannot run. Nothing else: not the state directory,
    systemd's credentials, `/home` or `/proc`. Landlock also refuses TCP
    (ABI 4), and signals and abstract Unix sockets out of the run (ABI
    6). The seccomp filter also refuses `connect(2)` and `sendto(2)` to
    an address now: without a mount namespace the host's Unix sockets
    are in sight.
  - `media.sandbox = "required"` needs Landlock ABI 3 (Linux 6.2) or
    later; `auto` runs the tools with the seccomp filter and the limits
    only on a kernel without it, with an error in the log; `off` is as
    before. The server logs the ABI at start, and shows the tools
    confined: a confined ffprobe must be refused the state directory and
    a listener on loopback, or `required` does not start.
  - `media.bwrap` is gone, and so is bubblewrap from the package's
    `PATH` and the dev shell. The unit allows the three `landlock_*`
    calls, which `@system-service` does not have.
  - Not kept from bubblewrap: a PID, mount, network, IPC and UTS
    namespace of the run's own. A run sees the unit's processes and
    every path's name and metadata, but can open, signal or connect to
    none of them (security/report.md 34).
- `spoolframe-probe` (PX-11 coverage gap 3): checks over live QUIC that
  a server keeps the limits it documents for a device with no session.
  `spoolframe-probe --endpoint <hex> --addr <ip:port> [--budget N]
  [--check-cap] [--check-limits] [--json]`. Each check is PASS, FAIL or
  SKIP with its evidence, and each ends with a `Ping`:
  - a `Hello` from another protocol revision is refused, and not kept;
  - `Libraries`, `Item`, `StartPlayback` and `ServerAddresses` without a
    session are `Unauthenticated`;
  - a stranger gets no unidirectional stream and at most 4 bidirectional
    ones, a well-formed request over 64 KiB is `Malformed`, and a stream
    silent after its kind byte is reset after
    `network.first_frame_timeout_secs`;
  - with `--check-cap`, the 65th stranger connection is closed `BUSY`;
  - with `--check-limits`, the 6th wrong password for a username made up
    for the run is `RateLimited`.
  - Every connection and stream counts against `--budget` (200), and 3
    exchanges in a row unanswered or `Internal` stop the run. Every
    frame it sends is a real request; malformed frames are the fuzz
    targets' job. It is `MIT OR Apache-2.0`, and links no server crate.
- `spoolframe-probe --username <name> --password-file <path> [--web
  <url>] [--web-origin <origin>]` (PX-11 coverage gap 1): signed-in
  checks as a consumer account. It signs in once over QUIC, from a fresh
  key of the account's own, and once on the web listener; a refused
  sign-in is never tried again. The password is read from the file only,
  and is in no line of the report.
  - `consumer-admin-refused`: the account is a consumer on both sides,
    and nine admin reads (`admin_userList`, `admin_settings`,
    `admin_logs`, …) are `FORBIDDEN`. No admin mutation is sent.
  - `session-bound`: the session from another endpoint key is
    `Unauthenticated`, and still works from its own.
  - `visibility`: what `Libraries` lists; a library and an item on a
    Plex server no server has are `NotFound`.
  - `recent-sign-in`: on a fresh session, `RemovePasskey` of a passkey
    id no account has gets past the recent sign-in check (`NotFound`).
  - `signed-in-streams`: the signed-in connection opens 8 streams at
    once, past a stranger's 4.
  - `session-reread`: `Ping` and `WhoAmI` work; `Logout` closes the
    connection `SIGNED_OUT`, and the session is `Unauthenticated` after.
  - The run signs out every session it made, past the budget if need be,
    and lists what the account still has (`me_sessions`, `me_devices`).
    Web requests count against the budget, and a 5xx counts towards the
    stop rule.
- Music on iOS starts (P2-03): a device keeps "now + next", the playback
  it plays and the one it starts after it. Starting a third ends the
  device's oldest converted playback, and its encode slot. Before, every
  start ended the device's earlier conversions, so the iOS app's next
  track, prepared 140 ms after the first for gapless audio, ended the
  first's HLS remux, and its segment reads failed as `PlaybackExpired`.
  With no slot free (`transcode.max_jobs = 1`), the device's own earlier
  conversion still gives way to the new start, as before; another
  device's slot is never taken.
- HLS declares each variant's peak rate as `BANDWIDTH` and its mean as
  `AVERAGE-BANDWIDTH` (P2-05). AVPlayer refuses a segment over
  `BANDWIDTH` ("Segment exceeds specified bandwidth for variant",
  CoreMediaErrorDomain -12318), and copied FLAC was declared at
  640 kbit/s whatever its rate. A copied stream, or an encode with no
  cap, now declares the source's rate (for music, the file's own) and a
  peak half again over it; a capped video encode declares its cap, and a
  peak of the cap plus its two-cap buffer over one 6 s segment; an audio
  encode declares its bitrate and a tenth more.
- A panic while a lock is held no longer breaks that feature until a
  restart (PX-10). The server, the SDK, `spoolframe-media` and
  `spoolframe-probe` use `parking_lot` locks, which do not poison; with
  `std::sync` locks, every later `lock()` after one panic panicked too.
- A first PIN set in an app needs a recent sign-in, as the web UI's
  `pinSet` does (PX-09 A-14): `SetPin` with no `proof` from a session
  that signed in over 10 minutes ago is `RecentSignInRequired`, and the
  app asks "Confirm it's you". An unlocked session left on a TV could put
  a PIN on the account, which its owner then had to prove to change.
  Changing a PIN with a `PinProof` is unchanged: the proof confirms it.
- `PROTOCOL_VERSION` is 6 (PX-09 finding 27): update the server and SDK
  together. The apps' device approvals can say where the device asked
  from, as the web UI's does: `DeviceSignInRequest`, the reply to
  `LookupDeviceSignIn`, has a new required `requested_from`, `null` or a
  `DeviceAddress { address, network }`, where `network` is `loopback`,
  `local` or `internet`, judged as the web page's `fromInternet`. The
  SDK re-exports `DeviceAddress` and `AddressNetwork`. An app shows "The
  TV asked from 192.168.86.20.", and for an `internet` (or unknown)
  address warns instead: approve only a device you can see.
- Every way to change the first Plex server's token needs a recent
  sign-in (finding 44), as `plexServerSetToken` already did for every
  server's: `admin.plexAccountTokenUse`, `plexPinCheck` with `keepToken`,
  and `settingSet`/`settingUnset` on `plex.token`. From a session that
  signed in over 10 minutes ago they are `RECENT_SIGN_IN_REQUIRED`, and
  the web UI's Plex step (the wizard and Settings) asks "Confirm it's
  you", then goes on: a PIN approved meanwhile is kept, not used up.
  Signing in with Plex only to add servers (`keepToken = false`) is
  unchanged.
- `admin.plexServerUpdate` is gone: nothing called it, not the web UI,
  the CLI or a test. A Plex server's address and name change with
  `settingSet` on `plex.url`, `plex.name` or `plex.servers`, as before.
- Two admin methods are renamed, so a name says which of a similar pair
  it is (a wire change; the web UI moves with them):
  `admin.plexPathCheck` is `plexPathTry` (it tries unsaved mappings on a
  sample, and keeps nothing; `plexCheckPaths` checks every file under the
  saved ones), and `admin.plexSync` is `plexSyncOverview` (who is linked
  and how their sync is going; `plexWatchSync` runs one).
- Picture subtitles (PGS, DVB and VobSub, read as text with OCR) are
  decoded in a child process, never in the server (finding 45). A few
  hundred bytes of a crafted track could ask for gigabytes of pictures,
  and a `.sup` beside a film took the whole server down when anyone
  played it with text subtitles.
  - The server runs its own binary again, as the hidden
    `spoolframe-server decode-subtitle`, confined as ffmpeg is
    (`media.sandbox`): it reads the track on its standard input, writes
    its cues' pictures into the job's directory and nothing else, and
    prints their times. It may use 4 GiB, 10 minutes of CPU and 10
    minutes in all.
  - A track decodes within a budget: 20,000 cues, 1 Gi pixels of
    pictures, and 64 Mi pixels held at once between cues. A film's
    track is 1 to 3 thousand cues of about 100 thousand pixels each.
    Past the budget the track is not read, and the reason says why
    ("the track's pictures come to more than 1073741824 pixels"); it is
    never cut short.
  - A decoder that panics, is refused by its budget, or is killed past a
    limit fails that track only.
  - At start, with OCR on, the server decodes a one-cue track with it and
    shows it may not write the state directory; `required` does not
    start otherwise, and `auto` logs an error ("SUBTITLE DECODER MAY NOT
    WORK OR NOT BE CONFINED").
  - `spoolframe-media`: `pgs::cues_within`, `dvb::cues_within` and
    `vobsub::cues_within` take a `bitmap::Budget`; `cues` keeps
    `Budget::DEFAULT`, and `dvb::cues` now returns a `Result`.
    `Ocr::recognise` reads pictures the decoder wrote (`decode::picture`),
    and `ocr::text_cues` takes their times.
- Refused requests are logged (finding 46): one line each, at WARN, on
  the target `spoolframe::refused`, with `kind`, `channel` (`admin_api`,
  `web` or `quic`), `request` (the method, path or request type),
  `source` (the caller's address, or the device's short id), `user` when
  known (the session's user, or the username a sign-in gave) and
  `reason`. The kinds are `unauthenticated` (a token that is not live,
  or not this device's: "session bound to another device"; a live
  session refused another session's playback: "playback is another
  session's"), `forbidden` (not an admin), `recent_sign_in_required`,
  `profile_locked`, `sign_in_failed` ("unknown user", "wrong password",
  "user disabled", "wrong PIN", "passkey refused", "wrong setup code")
  and `rate_limited`. A request with no session at all is `no_session`,
  at INFO only: every signed-out caller makes one. No line has a
  password, PIN, code or token. Per source and kind, 10 lines a minute
  are written, then one line, "N more refusals suppressed"; at most
  10,000 sources are counted apart. The lines reach the OTLP export like
  other warnings.
- Changing where a Plex token is sent needs a recent sign-in (finding
  47), as changing the token does (44): `settingSet` and `settingUnset`
  on `plex.url` and `plex.servers` (one key with every other server's
  address, name and paths), and `admin.plexServerAdd` while no Plex
  server is configured, since the first takes the token kept before it.
  From a session that signed in over 10 minutes ago they are
  `RECENT_SIGN_IN_REQUIRED`; the web UI's Plex Save and the Libraries
  page's add ask "Confirm it's you" and go on. `plex.name`,
  `plexServerSetPaths` and `plexServerRemove` are unchanged.
- A device approval no longer warns that a device in the same home is on
  the internet (finding 27). With IPv6, a home's devices have public
  addresses in the server's own /64, and every one of them was called
  `internet`. An address in the network of one of the server's own
  interface addresses, by that interface's prefix length, is now
  `local`, for the web page's `fromInternet` and the apps'
  `DeviceAddress` alike. Only the server's own networks count: another
  /64 of the same ISP block is still `internet`.
- `PROTOCOL_VERSION` is 7 (PX-12): update the server and SDK together.
  The events stream has a new event, `ServerAddresses { addrs, relays }`:
  the addresses a signed-in device is told, as text, and the URLs of the
  iroh relays the server uses (empty while `network.relay` is off). Each
  event is the whole list, and replaces the one told before. Both fields
  are required.
- The server pushes `ServerAddresses` on the events stream of each
  signed-in connection (PX-12): right after a sign-in on it (password,
  approved device sign-in, app passkey, trusted-device resume), right
  after `Hello` from a device that has a live session, and to every
  signed-in connection when the endpoint's addresses change. A
  connection with no session is pushed nothing. `relays` lists the URLs
  of the relays `network.relay` names: none while relays are off, which
  is the only setting accepted until remote access.
- The SDK applies the pushed `ServerAddresses` (PX-12) and no longer asks
  for the addresses on each connection. Each list replaces the told
  addresses saved before, so an address the server stopped telling (a
  Docker bridge now ignored) goes at once, not after 30 days. Addresses
  this device reached stay, and age out as before. The relays are saved
  with the server; a servers file written before has none. The SDK does
  not dial through them yet: its endpoint has relays off.
- The `ServerAddresses` request and reply are removed (revision 7): the
  pushed event does their work, and nothing else asked. A server answers
  the request `Unsupported`. `spoolframe-probe`'s check of requests sent
  without a session no longer sends it.
- `network.announce` leaves out virtual interfaces (`docker*`, `br-*`,
  `veth*`, `virbr*`) unless `announce.interfaces` names them: a device
  told 172.17.0.1 would dial its own docker0. Before, the same list was
  the default of `ignore_interfaces`, and an ignored interface stayed
  ignored even when `interfaces` named it. Now `ignore_interfaces` is
  empty by default and is for the user's own list, which still wins over
  `interfaces`. To tell a bridge, name it: `interfaces = ["enp*",
  "docker0"]`, or `["*"]` for every interface. A config that set
  `ignore_interfaces = []` to tell the bridges now needs that instead.
- `ServerEntry` shows what the server advertises apart from every dial
  hint (`addrs`, unchanged): `advertised_addrs`, the addresses of the
  latest `ServerAddresses` event in the server's order; `relays`, the
  relay URLs it listed; and `advertised_at`, when it came (to the
  second). They are empty and `None` for a server that never told this
  device, or one not saved. An address this device only reached is not
  in `advertised_addrs`. Older servers files load with none.
- Strangers can no longer fill the anonymous operation pool and refuse
  every other device's sign-in (finding 48, independent review M-1). An
  anonymous outcome that changed nothing (an error, or
  `DeviceSignInPending`) is not kept for replay; a retry runs again.
  Password and passkey sign-ins, which the strangers' bucket meters, have
  a pool apart from `BeginDeviceSignIn`, `AwaitDeviceSignIn` and
  `ResumeProfile`. A device sign-in is waited on by one
  `AwaitDeviceSignIn` at a time: a newer one ends the older as
  `DeviceSignInPending`.
- Eight signed-in users at their shares can no longer fill the
  operation pool (finding 49, independent review M-3): a user already
  holding operations may not take the pool's last eighth, which is kept
  for users holding none.
- Setup-code and device sign-in code guesses are counted before they are
  checked (A-13), as passwords already were: guesses sent at once can no
  longer pass the limit of 5. A right code is not counted.
- A proxy on the same host that the server is not told about is refused,
  not only warned of (findings 29 and 38, the second review's M-4): with
  `admin.listen` on loopback, `admin.public_url` set and
  `admin.trusted_proxies` empty, the config does not load. The server
  does not start, with an error naming `admin.trusted_proxies`, and a
  `settingSet` or `settingUnset` that would make that shape is refused
  as `INVALID_INPUT`. The new `admin.not_proxied = true` (a restart
  setting; NixOS `admin.notProxied`) says nothing proxies to the
  listener, and lets the shape load: its loopback callers are taken as
  local and share one failed-login limit, as before. The NixOS module's
  warning is now an assertion with the same opt-out, checked by the new
  flake check `nixos-module-assertions`.
- Changing the OTLP export needs a recent sign-in (finding 56, third
  review G-7): `settingSet` and `settingUnset` on every `telemetry.*`
  key, as on `admin.*`, `auth.*` and `media.*`. The endpoint receives
  every span, metric and log record, and `telemetry.otlp.headers` is a
  secret sent with each export. The web UI's Telemetry Save already
  asked "Confirm it's you" for the metrics settings, and now does for
  the export too.
- Changing `plex.token_file` needs a recent sign-in (finding 57, third
  review G-8), as `plex.token` does: the file it names is read and sent
  as the first Plex server's token.
- A listener on every interface with `admin.public_url` set and
  `admin.trusted_proxies` empty is warned of at start (finding 58, third
  review G-9): a proxy on this host may be in front, and every caller
  through it would look like loopback. It is not refused, since browsers
  may as well reach the listener directly or through an SSH tunnel;
  `admin.not_proxied = true` says so and silences it. The NixOS module
  warns the same way. The `admin` recipe in `defaults.ucl` now puts a
  proxy on the same host in front of a loopback listener with
  `trusted_proxies = "127.0.0.1"`.
- Confirming it's you with a sign-in provider asks it to sign the account
  in again (A-15): the flow sends `prompt=login` and `max_age=0`, and an
  ID token whose `auth_time` is from before the confirmation began
  (60 s allowed for clock skew) confirms nothing (`sso=failed`). Signing
  in and linking ask neither.
- The web UI names a sign-in provider in what its callback said
  (`?sso=…&provider=…` on the login, device and My profile pages) only
  if the server has a provider by that name (A-16); any other name is
  "the sign-in provider". The query is anyone's to write in a link, so
  it could put words of their choosing on the login page.
- A web UI (admin API) session ends after an hour unused (A-18). Each
  use renews it, written at most once a minute as a client's is, and it
  still ends 12 hours after its sign-in however much it is used. Until
  now it lasted the full 12 hours whether used or not.
- The NixOS module reads `admin.listen` as the server does (third review
  G-12): every spelling of `::1` (`[0:0:0:0:0:0:0:1]:7421`) is loopback
  for the unnamed-proxy assertion, and every spelling of `::` is every
  interface for its warning. A name (`localhost:7421`) or a bracketed
  IPv4 address (`[127.0.0.1]:7421`), which the server refuses to parse,
  is now refused by the module too, naming `admin.listen`.
- The NixOS unit's `ExecStart` quotes the state directory (third review
  G-13), as the CLI wrapper does: a `stateDir` with a space was split
  into two arguments.
- `spoolframe-probe --check-limits --check-pool` in one run no longer
  fails `anonymous-pool` (third review G-16): the limits check's wrong
  passwords come from a key of its own, so the source they lock is not
  the run's key, from which the pool check then logs in.
- A device's playing track is no longer ended when it prepares the next
  (P2-03). When a start needs room on its device, a conversion used in
  the last 30 s (a segment, playlist or media read, or any request
  naming it) is playing and never ends. Of the others, the first to end
  is a killed app's, whose connection closed and that no new connection
  has used; then the least recently used, never-used first. With none to
  end, the device may hold a third conversion, and a fourth start is
  `Busy`. With no slot free, only the device's conversions not in use
  give way; with none, the start is `Busy` and the client tries again.
  Before, the iOS app's playing track failed as `PlaybackExpired` when
  the next was prepared. Earlier connections of the same app, killed
  without `EndPlayback`, had left conversions behind. With no slot free,
  every conversion of the device ended, the playing one with them.
- An app killed mid-track and opened again at once can play again
  (P2-03). Before, every start from the new connection was `Busy`: the
  killed app's conversions had been read in the last 30 s, so they
  counted as playing and held every slot. A killed app sends no close,
  and the server holds its connection until it idles out. A conversion
  is now a killed app's if its connection closed, or if a newer
  connection of its device is starting and has not used it since that
  connection opened. Such a conversion is never protected. All of them
  end at the new connection's first start that needs room or a slot. A
  reconnect that reads a playback on (a resume) keeps it.
- An ended playback's ffmpeg stops, and its slot is free, at once. Before,
  a read still waiting on it (a killed app's) kept both until the read
  gave up. A read in progress no longer counts as the client using the
  playback while it waits.
- A session that ends ends its playbacks at once (B-10): revoked, logged
  out, its user disabled, its device forgotten, its user's password
  changed, or too many wrong PINs. Their ffmpeg stops and their slot is
  free; Server › Playback lists them as "Signed out". A client still
  using one is told `Unauthenticated`. Before, closing the device's
  connection stopped its reads, but the playback and its encode lived on
  for the 30 s reconnect grace. That grace is now only for a device that
  drops off the network with its session still live.
- Removing a library ends its open playbacks at once (B-10), as does
  removing a Plex server or any other change that stops a library being
  served. Their ffmpeg stops; a client still using one is told
  `PlaybackExpired`, and starting it again is `NotFound`. Server ›
  Playback lists them as "Its library was removed". Each read's 30 s
  recheck now also asks whether the item is still served, beside the
  read, so a film Plex moves to a section not served here ends there; a
  Plex that cannot be reached ends nothing. Before, such a playback went
  on until it idled out, 5 minutes after its last read.
- TLS is aws-lc-rs, not ring: iroh (`tls-aws-lc-rs`) and the OTLP gRPC
  exporter (`tls-aws-lc`). The server and the probe install aws-lc-rs as
  the process's provider at start, so reqwest 0.12 (Plex, SSO, OTLP over
  HTTP) uses it too. ring stays linked in those two binaries as reqwest
  0.12's fallback. The client SDK and the Linux client link no ring.
  aws-lc-sys builds with the C compiler alone (its cc builder); no cmake.
- A paused playback lives 30 minutes without use, not 5. An Apple TV
  paused an HLS remux for more than 5 minutes; on unpause, every segment
  read was `PlaybackExpired`. A paused conversion keeps its ffmpeg for
  that time. The read-ahead throttle has stopped ffmpeg (SIGSTOP), so it
  uses no CPU, but it keeps its slot, its memory and its scratch files.
  The device's next start that needs the slot still ends it: a playback
  not read in the last 30 s is not protected.
- A paused client keeps its playback alive with `UpdatePlaybackProgress`
  at the paused position, once a minute. The report resets the idle
  limit. It does not count as playing, so it does not hold the slot
  against the device's next start. A report at the same position does
  not change the watch state. See PROTOCOL.md, "Paused playback and the
  idle limit".
- Each playback that the server ends is logged once at INFO, as
  `a playback ended`. The fields are `playback` (the debug id), `item`,
  `user`, `endpoint`, `plan`, `why` and `idle_secs`, plus `error` for a
  failed conversion. Before, an idle end was not logged, and the other
  ends were logged as counts without the playback. A client's own
  `StopPlayback` is not logged.
- The client SDK's `ManagedPlayback` resumes a playback the server ended, for
  every app. A pause longer than the server's idle limit, a server restart
  or the device being let go made the next read or control
  `PlaybackExpired`, and each app had to notice and start again: the Apple
  TV asked the dead handle 9 times and froze. On `PlaybackExpired` the SDK
  now starts the same item again with the same tracks and options, at the
  last known position (the reported progress, a seek, or the HLS segment
  asked for). It swaps the new handle in behind the same object and tries
  the operation once more. Direct reads, HLS playlists and segments,
  progress (also in the background), seeks, subtitle changes and `finish`
  go on with no error; the new playback is told the viewer's position. HLS
  is read through the new `ManagedPlayback::manifest`, `read_segment` and
  `read_subtitle_segment`. A progressive reader, or a segment read
  part-way, gets the new `SdkError::PlaybackResumed { at }`: build a new
  reader from `snapshot()` at `at`, or read the segment again from 0.
  Nothing is resumed after the app closed it, after a start refused for
  good (`SignInRequired`, `NotFound`, `CannotPlay`, `PlaybackStopped`,
  given again on every later call), for a plan that changed, or more than
  3 times in 5 minutes. Before, only a direct read was started again, at
  most once a call and with no bound.
- The server's public addresses (P6-02): a `network.wan` table, absent
  by default. Absent, nothing changes. Present, its parts add to the
  addresses a signed-in device is told (`ServerAddresses`), so clients
  save them as `advertised_addrs`:
  - `static`: public addresses with their port, told as written;
  - `mapped_port`: one address from a port forwarded on the router.
    `mode = "auto"` (the default) asks the router over UPnP, NAT-PMP or
    PCP, through iroh's port mapper, and renews the mapping; `"manual"`
    means the operator forwarded `port` (default `network.port`).
    `public_ip` is an address or `"lookup"` (the default), one HTTPS GET
    at start to `lookup_via` (`"wtfismyip"`, or `false` to never ask).
    `mapped_port = false` turns it off;
  - `host_ipv6 = true`: the host's stable global IPv6 addresses, never a
    temporary one.

  A private, loopback, link-local, CGNAT or unique local address is
  refused at load, the range named; so is `public_ip = "lookup"` with
  `lookup_via = false`. A refused mapping or a failed lookup is one log
  line and a status note, retried with backoff; startup never waits for
  either. Addresses found later are pushed to signed-in devices. The
  admin status (`serverStatus`, `spoolframe-server status`, the Server
  page) lists each public address with its source: static, mapped port,
  lookup or host IPv6. The NixOS module has `services.spoolframe.wan`.
- The web UI has Settings › Remote access (P6-02): what the server
  advertises now, each address with its source's badge, the router's
  answer, the IP lookup, then the `network.wan` form (the master switch,
  the mapped port, static addresses, host IPv6). A typed address in a
  private, loopback, link-local, CGNAT or ULA range is refused as typed,
  in the server's words. The setup wizard's Access step ends with
  "Reach this server from outside your home?": Not now (the default,
  no `network.wan`), Automatic (asks the router at once and shows its
  answer), Forwarded or Static. New admin calls `wanTryMapping`,
  `wanLookup` and `wanHostIpv6`, and the status's `wanMapping` and
  `wanLookup`, feed them. `network.wan` is one setting, set whole.
- `spoolframe-probe --check-wan <addr> --endpoint <id>` dials the server
  at one public address only and says whether it answered. Run it from
  another network: a server cannot see its own port forward.
- Changing any `network.*` setting needs a recent sign-in (finding 61),
  as `admin.*`, `auth.*`, `media.*` and `telemetry.*` do. `network.wan`,
  the announced addresses, the port and the relay say where signed-in
  devices connect, so an old or stolen admin session could send them
  elsewhere and learn their IP addresses. Settings › Remote access and
  the LAN discovery switch now ask "Confirm it's you" and save again.
- `admin.restart` works on a server started by hand (no supervisor): it
  stops as for SIGTERM (connections closed, playbacks and encodes ended,
  the database closed), then runs its own binary again in the same
  process, with the same PID, arguments, environment and standard
  streams. A pipe the log goes to, such as `| tee`, keeps receiving it.
  The binary is the one at the path it was started from, so a rebuild is
  picked up. A failed exec is logged, and the process exits non-zero.
  Under a supervisor (`SPOOLFRAME_SUPERVISED`) it still exits with 75.
  `Settings.canRestart` is always true now, and the new
  `Settings.restartMode` says which way: `supervisor` or `inPlace`. The
  "listening" log line has the PID. The admin listener waits up to five
  seconds for its port if it is still taken as it starts.
  `admin.restart` now needs a recent sign-in: every device connected
  drops, and a server started by hand runs whatever binary is at its path.
- The web UI restarts the server. While restart settings changed since
  the server started (`restartPending`), every Settings tab and the
  Server page show a banner: "N changes take effect after a restart",
  the settings by key, and "Restart now". The Server page also has a
  "Restart server" button for admins, shown whether or not anything is
  pending. Every restart button, the setting notes' "Restart now"
  included, first asks "Restart the server?" and says that connected
  devices drop for a few seconds. Then it asks "Confirm it's you" if the
  server wants a recent sign-in, and shows "Restarting…". It waits for
  the old server's socket to close and for the server to answer again,
  then loads the page again, which also loads a new web bundle. After a
  minute without an answer it says to check the log. The setting notes'
  "Restart now" no longer depends on `canRestart`.
- The SDK stops reconnecting on its own when dialling again cannot help.
  A server that refuses the SDK's `Hello`, or answers with another
  protocol revision, is the new `SdkError::UpdateRequired
  { server_protocol }`, where it was `Server`/`Protocol`; a server that
  answers with another server id (`ServerIdChanged`), as another key
  (`IdentityChanged`, `WrongServer`), signs the device out
  (`SignInRequired`) or is full (`ServerFull`) stops it too. The link
  goes `Disconnected`, and the new `StateWatch::stopped()` says why, so
  an app can show it in place of "Reconnecting…". `ConnectionState` is
  unchanged. Every other failure is dialled again after 250 ms, doubling
  up to 5 minutes (was 10 s), each wait moved by up to 20% either way.
  Before, an app kept a server that refused it dialled every 10 s for as
  long as it ran.
- A refused `Hello` is logged: one INFO line on `spoolframe::refused`,
  `kind=hello_refused`, message "hello refused", with the device's short
  endpoint id (`source`), its name if someone signed in from it
  (`device`, from the devices table), the protocol revision it sent
  (`client_protocol`), its `client` name and version, the server's
  `server_protocol` and `server_version`, and the `reason`
  ("unsupported protocol revision", or "malformed: …" for a `Hello`
  that does not decode or a first frame that cannot be read). At most
  one line per device an hour, then a count of those left out; past
  10,000 devices, new ones share one line. Before, an app of another
  revision left only its connects and disconnects in the log.
- The server's connection log is quieter. "client connected" is DEBUG:
  every connection starts anonymous. "client disconnected" is INFO only
  when the connection was signed in at some point (a sign-in on its
  device while it was open, or a session used on it; the new
  `signed_in` field says which), or when it did not end with the app
  closing it with code 0: an error code, a timeout, a reset, or the
  server closing it (signed out, shutdown). An app that dials, says
  `Hello` and hangs up without signing in is DEBUG at both ends, even
  when its device has a session (`known=true`). Refusing a connection
  as the server is full stays INFO.
- Fixed: once the disk filled, the server's database stayed "locked"
  after space came back, and every write failed until a restart. A
  COMMIT that cannot write to a full disk makes SQLite roll the
  transaction back by itself; sqlx's own ROLLBACK then failed, it kept
  counting that connection inside a transaction, and its next
  transactions on it became savepoints inside a real transaction that
  never ended and kept the write lock (and whose writes were never
  saved). A connection given back to the pool while sqlx still counts it
  inside a transaction is now closed, and the pool opens a new one. A
  full disk is also logged as one ERROR, "the disk is full: database
  writes fail until space is freed", at most once a minute.
- Changed: text subtitles are never burned in. One the client shows in
  no text format the output can carry is left out of the plan, with a
  reason such as "subtitle 1: the client shows no text subtitles; not
  shown", and the video is not encoded for it. Picture subtitles are
  burned in only when they are not read as text and the client cannot
  draw them. `bitmap: "burn_in"` (and "Burn in" in "Plan for…") now asks
  for pictures: a client that can draw them draws them. The wire is
  unchanged (protocol 7).
- Transcodes decode on the GPU. A VA-API or software video encode decodes
  its source with VA-API where detection found a decoder for its codec
  (not 10-bit H.264 or anything but 4:2:0): the frames come back to
  memory for tone mapping, burn-in and libx264, or, for a VA-API encode
  that only scales or changes the codec, stay on the GPU and are scaled
  with `scale_vaapi`. A run whose GPU decode fails before its first
  output runs again in the same job decoding in software, and the file
  decodes in software until the server restarts; one that fails later
  fails, and the playback's next segment or seek goes on in software.
  Measured for 60 s of output, a VA-API encode of a 1080p source went
  from 10.4 to 0.5 s of CPU time (10 Mbit/s) and from 43.5 to 0.9 s
  (96 Mbit/s); a 4K one scaled to 1080p from 36 to 1.9 s and from 176 to
  3.9 s; libx264 saved 3 to 57%.
- A conversion all on the GPU (decoded, scaled and encoded there, nothing
  on the CPU) weighs 0.25 in the encode budget, as an audio-only encode
  does, and counts against `transcode.max_gpu_jobs`. A GPU encode that
  decodes or filters on the CPU still weighs 0.5. The metrics' `kind`
  label gains `gpu_only`, and the playback page shows "video all on the
  GPU".
- `transcode.gpu_filters` (live, off by default; Settings › Playback,
  "GPU filters"): a VA-API encode that decodes on the GPU also tone maps
  HDR (`tonemap_vaapi`) and burns in picture subtitles (`overlay_vaapi`,
  the pictures uploaded) there, staying on the GPU throughout. Detection
  tries each filter with a one-second run, as it does the encoders, and
  `spoolframe status`, the web UI and the encoders' log line say which
  this GPU has; Mesa's VA-API (AMD) has neither. A filter the driver
  lacks, and text burn-in (libass), run on the CPU for that conversion,
  logged once. A run whose GPU filters fail before its first output runs
  again on the CPU, and the file stays on the CPU until a restart, as a
  failed GPU decode does.
- Invites (P6-01): an admin makes one in the web UI (Users and devices ›
  Invites) or with `spoolframe-server invite create`, and whoever has its
  link finds the server from anywhere and makes their own account. The
  link is one string, `spoolframe://invite?server=&name=&addr=…&relay=…
  &exp=&role=&cap=`, shown as a QR code: the server's key, its told and
  public addresses, its relays, and the code. The code is 12 characters
  (about 59 bits), shown once: the server keeps its BLAKE3 hash. An
  invite makes 1 to 100 accounts (1 by default) with the role it names
  (consumer by default) for an hour to 30 days (7 by default), and is
  revoked from its list. `portable-account` and `federation` invites are
  refused until P7 and P8. The server's link without a code (Copy link,
  Show QR) only finds and saves the server.
- Wire revision 8 (`PROTOCOL_VERSION` 8): the pre-auth `RedeemInvite {
  code, username, display_name, password }`, answered `LoggedIn` with a
  session bound to the device, and the errors `InviteInvalid`,
  `InviteExpired`, `InviteExhausted` and `UsernameTaken`. It draws on the
  strangers' sign-in budget, a wrong or revoked code counts as a guess
  before it is checked, refusals are not kept as operations and are
  logged as refused sign-ins, and the last use goes to exactly one of two
  redemptions. Admin calls `inviteCreate` and `inviteRevoke` (recent
  sign-in), `inviteList` and `inviteLink`; the error code
  `NO_SUCH_INVITE`.
- The SDK takes invite links: `read_invite` (what it says, nothing
  sent), `connect_invite` (who answers), `save_invite_server` (pin the
  key it names, verified, with the server id it answers with) and
  `redeem_invite` (the account, the server saved, a session). Its hints
  join the known addresses as told ones; `SdkError` gains `NotAnInvite`,
  `InviteInvalid`, `InviteExpired`, `InviteExhausted` and
  `UsernameTaken`.
- Relays (P6-03): `network.relays` lists the relays the server may take
  its home relay from, `default` (n0's public relays) and relay URLs,
  comma-separated; empty, as by default, is relays off. The old
  `network.relay` is refused at load, pointing to it. The log names the
  home relay, devices are told the list with it first (in
  `ServerAddresses` and invite links), and
  `spoolframe_connection_paths{path="direct"|"relay"}` counts the open
  app connections by the way they go.
- The SDK reaches a server through the relays it told, when it cannot be
  dialled directly: `ClientOptions::relays`, on by default. The device
  has no relay of its own and contacts none until it dials through one;
  off, it has no relay transport at all.
- Media over the Internet (P6-06): the SDK measures the link to each
  server from its direct reads, all of them together, as they arrive
  (`Session::link_rate`), and a playback is planned for 75% of it as
  well as for the device's own cap. Wire revision 9: `StartPlayback`
  carries `link_bitrate`, and `LimitPlayback` plans a live playback
  again for a link found slow. A direct play that stalls for 3 s on a
  link too slow for the file is switched to a conversion where the
  viewer is; the player makes a new reader on `PlaybackRestarted`.
- The SDK lets the server send 8 MiB on a stream before reading it, a
  whole direct read, so a long round trip no longer caps a read at about
  33 Mbit/s; `reader::Tuning` sets the reads in flight and the
  read-ahead.
- A quality choice per playback (P6-06, wire revision 10): Auto (for
  the link), a bitrate, or Original (the file, whatever the link), at
  the start (`PlaybackOptions::quality`) and while playing
  (`change_quality`). Only Auto is switched on a stall. A capped encode
  is also made smaller: 1080p from 4.5 Mbit/s of H.264, 720p from 2.2,
  and so on, rather than a starved 2160p.
- Adaptive HLS for Auto quality (PX-21, wire revision 11): a playback
  over HLS that encodes its video is a ladder of up to four encodes,
  from the app's own cap down to 1 Mbit/s, each at the height its rate
  carries, and the player moves between them as its link changes. One
  encode runs at a time: a read of another rung moves it there, and the
  rung it left keeps what it wrote. The player starts on the rung the
  measured link chose. `MediaRead` and `GetManifest` name the rung.
- pkarr (P6-05, wire revision 12): `network.pkarr = "n0"` or a pkarr
  relay's URL publishes where the server is, signed with its key: its
  relays and public addresses, never a LAN one. Devices are told where
  (in `ServerAddresses` and invite links) and look the server up there
  by its key when nothing they know reaches it, as after an IP change.
- A relay of your own (P6-04): `spoolframe-server relay run --config`
  runs an iroh relay that lets through only whom you allow, by tier:
  owner (the server and its users' devices, which it asks the main server
  about), friends (none until federation) and everyone else, each allowed
  or denied. TLS from files, from Let's Encrypt (TLS-ALPN-01, or its
  staging, or another ACME directory), or at a proxy in front. The main
  server answers the relays `network.own_relays` names. NixOS:
  `services.spoolframe-relay`, and the server module's `relays`, `pkarr`
  and `ownRelays`.
- The settings that were comma-separated strings are UCL lists and
  tables now: `network.relays`, `network.own_relays`,
  `admin.trusted_proxies` and `admin.metrics_from` are lists, as
  `relays = ["default"];`; `transcode.hardware` is `"auto"`, `"none"`
  or a list of families in order, as `["nvenc", "vaapi"]`; and
  `telemetry.otlp.headers` is a table of name to value, as
  `headers { authorization = "Bearer …"; }`. A later layer replaces the
  table whole. The old comma string does not load: the error names the
  setting and shows the new form, and never echoes the headers' string.
- `network.ipv6`, on by default: off, the server listens on IPv4 only,
  so no device reaches it over IPv6, directly or by hole punching. It
  applies at the next start. Settings › Remote access has a "Listen on
  IPv6" switch for it, and the NixOS module `ipv6`.
- `network.wan.host_ipv6` is gone, with the admin API's `wanHostIpv6`,
  the "Host IPv6" address source and the NixOS module's
  `wan.hostIpv6`. It added nothing: iroh tells devices every global
  IPv6 address on the host's interfaces itself, and offers them when it
  punches holes. Turning it off did not keep the server off IPv6
  either. Set in the config file it is refused at load, pointing to
  `network.ipv6`; saved by the web UI in the runtime layer it is
  dropped, with a warning in the log.
- P6-07, the review of remote access, and its fixes:
  - A relay of your own holds at most 2 connections on the main server's
    relay-access ALPN, and keeps at most 10,000 answers.
  - The relay warns at start, as does its NixOS module, when `proxy`
    mode's plain HTTP is not on loopback and when `access.everyone` is
    `"allow"`. The module gains `resources.memoryMax` (1G) and
    `resources.tasksMax` (512).
  - The server warns at start when `network.own_relays` is set and
    `network.relays` is empty.
  - The SDK keeps from a server's `server_addresses` only `http(s)`
    relay and lookup URLs: at most 32 relays, and at most 8 pkarr relays
    looked up at in all. Invite hints are kept newest first, 32 at most.
  - An invite link longer than 4 KiB is not read, and its name is cut
    to 128 characters.
  - A redemption refused for a taken username or an unacceptable
    password is logged as a refused sign-in.
  - "client disconnected" names the device's key and the users signed in
    on the connection, in place of an `auth=Anonymous` that was always
    so.
  - `spoolframe-probe --check-limits` also sends wrong invite codes until
    they are `RateLimited`.
- `nix build .#docker`: the server as an OCI image
  (`ghcr.io/spoolframe/spoolframe-server`), ffmpeg and tesseract included,
  running as uid 1000 with its state in a volume.
