# Spoolframe client protocol

This is the wire protocol between Spoolframe apps and a Spoolframe
server, and the protocols beside it: pairing a portable user's device,
and a server's own relay asking the server who may relay. The design
behind it is in [ARCHITECTURE.md](ARCHITECTURE.md); `§N` below cites its
sections.

The types are in `spoolframe-protocol` (MIT OR Apache-2.0), with shared
models in `spoolframe-types`. `spoolframe-client-sdk` is the supported
client and handles most of what follows; the SDK notes in each section
say what it does for an app.

**Current revision: 15.** The client ALPN is `spoolframe/client/1`, and
`PROTOCOL_VERSION` (in `spoolframe_protocol::messages`) names the
revision inside it. Until the protocol is declared stable, a breaking
change raises the revision and keeps the ALPN, so the server and the SDK
must be updated together. The changes per revision are in
[CHANGELOG.md](CHANGELOG.md).

## Contents

- [Basics](#basics): ALPNs, encoding, required fields
- [Connections and streams](#connections-and-streams): stream kinds,
  RPC framing, limits, events
- [Mutation replay](#mutation-replay)
- [The server id](#the-server-id) and [server addresses](#server-addresses)
- Signing in: [pre-auth requests](#pre-auth-requests),
  [confirming it is you](#confirming-it-is-you),
  [a new password after a reset](#choosing-a-new-password-after-a-reset),
  [app passkeys](#app-passkeys-a-relying-party-per-server),
  [device sign-in](#where-a-device-asked-from),
  [trusted devices](#trusted-devices)
- [Invites](#invites) and [relay access](#relay-access)
- Playback: [profiles and progress](#profiles-and-progress),
  [playback revisions](#playback-revisions),
  [resource streams](#resource-streams), [quality](#quality-and-adaptive-hls),
  [output codecs and HDR](#output-codecs-and-hdr),
  [the WAN uplink budget](#the-wan-uplink-budget),
  [picture subtitles](#picture-subtitles),
  [idle playbacks](#paused-playback-and-the-idle-limit)
- [Home rows and "Remove"](#home-rows-and-remove)
- [Leaving a server](#leaving-a-server)
- [Portable identities](#portable-identities)

## Basics

### ALPNs

| ALPN | Between | Where |
|---|---|---|
| `spoolframe/client/1` | an app and a server | most of this document |
| `spoolframe/pair/1` | a phone and a device, no server | "Pairing" |
| `spoolframe/relay-access/1` | a server's own relay and that server | "Relay access" |
| `spoolframe/federation/1` | server and server | reserved; refused with `close::UNSUPPORTED_PROTOCOL` until federation exists |

### Encoding

Every message is CBOR. Enums are internally tagged by `type`, in
snake_case: `{"type": "ping"}`, `{"type": "item", "id": "plex:movie:1"}`.
Examples in this document are written as JSON for reading; byte strings
are CBOR byte strings unless a field says it is hex.

- A peer ignores fields it does not know.
- An error, event or enum value a peer does not know decodes as
  `Unknown` (or the value the type names for it), so a newer peer can
  add them.
- A request type the server does not know is answered
  `Error::Unsupported`.

### Every field is sent

A message carries every field its type has. A field with no value is
sent as `null`, never left out. The decoders refuse a message that
leaves out a field marked required (`spoolframe_types::required`):
`Malformed` from the server, a decode error in the SDK. Do not rely on
a decoder accepting a missing field: some older `Option` fields are
still read as `null` when absent, but senders must not leave them out.

The admin API (JSON-RPC) follows the same rule, except for an error
object's `data`, which JSON-RPC 2.0 lets an error leave out.

A few fields may be left out until the protocol is reset to revision 1
for a stable release. Each is marked `compat(v1): required at the
protocol reset` in the code:

| Field | Read as, when left out |
|---|---|
| `ClientCapabilities.hardware_video` | `null` |
| `display` on `PlaybackPlan`, `StartPlayback`, `ChangeSubtitle`, `ChangeQuality` | `null` |
| `LoggedIn.password_change_required` | `false` |
| `can_leave` in the `WhoAmI` reply | `true` |
| `Rung.video` | `null` |
| `ConvertedVideo::Copy.dolby_vision` | `false` |
| `servers` in pairing's `credential` frame | none |
| `root`, `genesis` on a device credential and a `root` tie proof; `signer`, `roots_added`, `roots_removed` on an identity state | absent (these are also left out when empty) |
| `year`, `air_date` on an item; `bit_rate` on a file's tracks and its video | `null` |
| `release_date` on a movie, `last_aired` on a show | `null` |
| `cast` on a movie, show and episode | `[]` |

## Connections and streams

### Stream kinds

The first byte of each QUIC stream says what it is
(`spoolframe_protocol::streams`):

| Direction | Byte | Stream |
|---|---|---|
| bidirectional, client opens | `1` | RPC: one request, one reply |
| bidirectional, client opens | `2` | resource: one request, a header, a raw body |
| unidirectional, server opens | `1` | events, for the life of the connection, opened after `Hello` |

A stream with an unknown first byte is reset with
`reset::UNKNOWN_STREAM_KIND` (1).

The first request on a connection is `Hello`. Its `protocol_version`
must equal the server's `PROTOCOL_VERSION`; otherwise the reply is
`Unsupported`, and the SDK reports a revision mismatch. The reply is
`ServerInfo`: the server's name, `endpoint_id`, `server_id`,
`protocol_version`, `server_version`, `auth_methods` and `web_url`.
The server keeps at most 128 characters (`MAX_HELLO_TEXT`) of each of
`Hello`'s strings.

### RPC framing

An RPC stream starts with byte `1`. The request is one length-prefixed
CBOR `RpcRequest` (a big-endian `u32` length, then the CBOR), at most
1 MiB. It wraps the `RequestEnvelope` (session and request), an optional
`Operation` (see "Mutation replay"), and an optional expected playback
revision.

The reply is a CBOR `RpcResponse`. The stream carries its total length
as a big-endian `u64`, then chunks, each with a big-endian `u32` length.
Each chunk is at most 1 MiB (`codec::MAX_FRAME`); the whole reply is at
most 64 MiB (`codec::MAX_REPLY`). Empty chunks, oversized lengths and
truncated replies fail. The server stops serializing at the limit,
before anything is sent. Each reply names the server process and its
clock (`ServerInstance { id, time_ms }`).

The SDK assembles the reply and returns its own result types. Catalog
collections come back whole; there is no paging cursor beyond the
`offset` and `limit` that `Items` and `PersonItems` take.

### Connection limits

A connection from an endpoint with no live session is a *stranger*. Its
limits come from the handshake: no unidirectional streams (the server
never accepts one), 4 concurrent bidirectional streams, 512 KiB of
unread data across them, and request frames of at most 64 KiB. A
connection from a device with a live session gets 100 streams and
16 MiB, at accept or as soon as a sign-in on it succeeds; frames may
then be 1 MiB.

Every stream must deliver its kind byte and first frame within 10 s
(`network.first_frame_timeout_secs`), or it is reset with
`reset::FIRST_FRAME_TIMEOUT` (2). The connection stays.

The server holds at most 64 stranger connections
(`network.max_stranger_connections`). One more is closed with
`close::BUSY` (2) and may try again later; a signed-in device is always
let in. When a device's last live session ends, the server delivers the
replies it is sending (for at most 2 s) and closes its connections with
`close::SIGNED_OUT` (3). Connections idle for 15 s are dropped; iroh
sends a keep-alive every 5 s.

The SDK, on `SIGNED_OUT`, ends every session it keeps for the server,
answers a call in one with `SignInRequired`, and dials again only when a
call needs it, as a stranger. It reports `BUSY` as `ServerFull`.

### Events

The server pushes `Event`s on the events stream, without being asked:

| Event | Meaning |
|---|---|
| `shutting_down` | The server is stopping; the connection will close. |
| `server_addresses { addrs, relays, lookup }` | Where the server can be reached. See "Server addresses". |
| `playback_share { handle, max_bitrate }` | A playback's share of the WAN uplink moved. See "The WAN uplink budget". |

An event a client does not know decodes as `Unknown`; ignore it.

## Mutation replay

A mutation carries an `Operation { id, instance, expires_at_ms }`: an
operation id, the server process id, and an expiry in the server's
clock, which only moves forward within a process. The SDK makes these
once per method call and reuses the exact request on its transport
retry. Reads need no operation and may be retried freely.

The server keys outcomes by the authenticated endpoint and the operation
id, and fingerprints the whole request, session and expected revision
included. Concurrent duplicates wait for the same execution.
Disconnecting, or cancelling the reply stream, does not cancel an
admitted mutation. Reusing an id with another request is
`OperationConflict`.

An operation lives at most 15 minutes (`rpc::OPERATION_WINDOW_MS`). The
cache holds 4096 entries and 64 MiB of serialized outcomes. A pending
operation reserves what its kind of reply can take: 1 MiB for
`StartPlayback`, `ChangeSubtitle` and `SeekPlayback`, 64 KiB for passkey
ceremony options, 16 KiB for the rest; a finished one counts its
outcome's size. One endpoint, and one user, may hold at most an eighth
of the entries and bytes, and a user who already holds some may not take
the last eighth of the pool, which is kept for users who hold none.

The anonymous sign-in requests have pools of their own, 512 entries and
8 MiB each, with the same shares: one for `LoginPassword`,
`BeginPasskeySignIn` and `FinishPasskeySignIn`, and one for
`BeginDeviceSignIn`, `AwaitDeviceSignIn` and `ResumeProfile`. Their
outcomes are replayed for 30 s after they are known, or until the
operation expires if that is sooner. An outcome that changed nothing (an
error, or `DeviceSignInPending`) is not kept, and a retry runs again.

When a pool is full, new operations get `RateLimited`; unexpired ids are
never evicted to make room. One outcome is at most 1 MiB. An expired id
is `OperationExpired`; an id from an earlier server process is
`ServerRestarted`. Neither runs the operation again.

This is a bounded replay guarantee, not a durable transaction log. After
a crash the client may need to look at state to learn whether an
operation happened. Calling an SDK method again is a new operation, not
a replay.

Profile unlocks belong to the connection. Replaying an earlier
successful PIN operation cannot unlock a new connection or undo a later
profile switch; a locked connection gets `ProfileLocked` and must unlock
again.

## The server id

`ServerInfo.server_id` is the server's id: 80 random bits, written as
lowercase RFC 4648 base32 with no padding, so 16 characters of `a`-`z`
and `2`-`7`. It is a DNS label. `spoolframe_protocol::ServerId::parse`
checks one.

The server makes it at its first start and keeps it in its state
directory, in `server.id` (mode `0600`), beside `endpoint.key`. It is
not the endpoint id. A new key keeps the server id, and a new server id
does not change the key. `spoolframe-server id` prints both:

```text
endpoint id: 5f0c…
server id:   mzxw6ytbojtg633c
```

A `server.id` that is not a server id stops the server. It is never made
again while the file is there.

### SDK: the pin is the key and the server id

The SDK checks that `server_id` is a server id, or the connect is a
`Protocol` error. It pins the server id with the key at the first
connect (§19); a server saved without one takes the id at its next
connect. `ServerEntry.server_id` is `Option<String>`.

- Same key and same id: the connect succeeds. The name is saved,
  whatever it is.
- Same id, another key: `SdkError::IdentityChanged { saved, now }`, from
  `connect` and from `pin`. `Client::trust_new_identity(saved, now)`
  pins the new key with the same id. The server's app passkeys keep
  working, because they belong to the id, and the SDK makes their
  challenges with the new key from then on.
- Same key, another id: `SdkError::ServerIdChanged { endpoint_id, saved,
  now }`, from every request to that server. Nothing is saved and the
  pin does not change. A server never changes its id, so this is not
  accepted in place: `Client::forget` it and add it again if the user
  says it is theirs.
- Another key and another id: another server. No pin applies, whatever
  its name.

A pin with no id yet is still matched by name: another key heard on the
LAN under its name is an `IdentityChanged`. A pin with an id is not.

## Server addresses

The server tells a signed-in device where it can be reached, on the
events stream:

```json
{"type": "server_addresses",
 "addrs": ["192.168.1.10:4433", "[2001:db8::1]:4433"],
 "relays": [],
 "lookup": null}
```

- `addrs`: the addresses on the server endpoint's interfaces that
  `network.announce` lets it tell, with their port (LAN, public and VPN
  addresses; never loopback, link-local or unspecified), then the
  server's public addresses (`network.wan`): written in the config, made
  of a port forwarded on the router and the network's public IP, or the
  host's own stable IPv6 addresses. Each is text, `192.168.1.10:4433`
  or `[2001:db8::1]:4433`, in CBOR as in JSON. The event does not say
  where each address came from; the admin status does.
- `relays`: the URL of each iroh relay the server may use
  (`network.relays`), its current home relay first. Empty while relays
  are off, as they are by default.
- `lookup`: the URL of the pkarr relay where the server publishes a
  record of where it is (`network.pkarr`), or `null`. The record, signed
  with the server's key, holds its relays and public addresses, never a
  LAN one. A device looks the server up there by its key when nothing it
  knows reaches it. A record is a hint only: the connection still proves
  the key.

Each event is the whole list and replaces what the server told before:
an address it no longer tells is gone.

The server pushes it to a connection:

- right after a sign-in on it (`LoginPassword`, an approved
  `AwaitDeviceSignIn`, `FinishPasskeySignIn`, `ResumeProfile`), and to
  every other connection of the same device that said `Hello`;
- right after `Hello`, when the device already has a live session;
- to every signed-in connection, when the addresses change: iroh sees
  an interface come up or go down, the router maps the port, the IP
  lookup answers, or the host's IPv6 addresses change.

A connection with no live session is never told, because a stranger
must not learn the server's internal and VPN addresses; the addresses
are never in `Hello` for the same reason. A locked profile is told: the
addresses say nothing about the user.

Iroh announces addresses inside each connection too, but a client sees
only the paths that opened. A phone that never reached the server's VPN
address cannot learn it from iroh.

### SDK

The SDK applies each `server_addresses` event to the saved server.
Nothing in the API needs calling.

- The list replaces the *told* addresses saved before. A told address
  not in the new list is removed at once.
- A *reached* address stays, whether the list has it or not. It ages out
  after 30 days without being reached.
- A new address is saved as told, with the time it was told, and becomes
  reached when a connection reaches it.
- At most 32 addresses are kept per server: the newest reached ones
  first, then told ones in the room left.
- The relays are saved with the server (`relays` in the servers file).
  The SDK's endpoint has relays off, so it only keeps them.

An event for a server that is not saved is ignored. Every saved address
is dialled, told or reached.

`ServerEntry` shows the last event as it came, so an app can show the
server's advertised addresses apart from every dial hint in `addrs`:
`advertised_addrs` (in the server's order), `relays` and
`advertised_at` (to the second). They are empty and `None` until the
server tells this device.

## Pre-auth requests

These are the only requests served without a session
(`Request::is_pre_auth`):

| Group | Requests |
|---|---|
| Handshake | `Hello`, `AuthMethods`, `Ping` |
| Password | `LoginPassword { username, password, remember }` |
| Device approval | `BeginDeviceSignIn { remember }`, `AwaitDeviceSignIn { request }` |
| App passkey | `BeginPasskeySignIn { username }`, `FinishPasskeySignIn { signed, remember }` |
| Trusted device | `TrustedProfiles`, `ResumeProfile { user_id }` |
| Invite | `RedeemInvite`, `RedeemPortableInvite` |
| Portable credential | `BeginPortableSignIn`, `FinishPortableSignIn` |
| Portable identity records | `IdentityRecord { user }`, `IdentityRecordByHandle { handle }` |
| Pairing through a server | `OfferPairing`, `FetchPairing`, `AnswerPairing`, `CollectPairing` |

Every sign-in answers `LoggedIn { session, remembered,
password_change_required }`. The session is bound to the endpoint that
signed in (§25). A device that is not signed in draws on a sign-in
budget all such devices share; past it the reply is `RateLimited`.

## Confirming it is you

Some requests need a session that proved its user within the last 10
minutes: `ApproveDeviceSignIn`, `BeginPasskeyRegistration`,
`RemovePasskey`, `SetPin` with no `proof` (a user's first PIN),
`SetPinRequired` with `required: false`, `SetDeviceTrust` except to stop
trusting this device, `ForgetDevice`, `TieBegin`, `TieFinish`, `Untie`,
`CreateInvite`, `RevokeInvite` and `LeaveServer`. An older session gets
`RecentSignInRequired`. The app asks its user to confirm, and sends the
request again. It does not sign out.

| Request | Reply |
|---|---|
| `{"type": "confirm_password", "password": "…"}` | `Confirmed` |
| `{"type": "begin_confirm_passkey"}` | `PasskeySignInBegun` |
| `{"type": "finish_confirm_passkey", …FinishPasskeySignIn fields}` | `Confirmed` |

Each needs a session. Each is a mutation, so it carries an operation id,
and the SDK never retries it on a new connection. `Confirmed` makes this
session count as signed in now; the user's other sessions do not change.
This is the same store change as the web UI's `me.confirmPassword` and
`me.confirmPasskey`.

The password is checked as `LoginPassword` checks it. A wrong password
is a failed sign-in for the user's username from this endpoint, counted
with logins and the web UI's confirmations. After five failures in 15
minutes the reply is `RateLimited`, also for the right password and for
a login. A user with no password gets `NoPassword`, and nothing is
counted.

`BeginConfirmPasskey` lists only the session user's app passkeys for
this server's relying party, and sends a nonce (see "App passkey
challenges"). The challenge belongs to this session on this endpoint,
for 5 minutes, and is used once. A sign-in challenge, or another
session's, does not finish it. Another user's passkey is
`InvalidCredentials`. A failure counts as a failed passkey sign-in of
the passkey's user from this endpoint. A user with no app passkey gets
`NotFound`. When the user's passkey sign-ins are rate-limited, `Begin`
gets `RateLimited` before a challenge is made.

A PIN is not proof. A PIN guards switching profiles on a shared device,
and it is weaker than a sign-in. A locked session gets `ProfileLocked`
for all three requests: unlock with the PIN first, then confirm. A
confirmation does not unlock a profile. The lock belongs to the
connection; the confirmation belongs to the session.

## Choosing a new password after a reset

An admin may reset a user's password to a one-time one (the admin API's
`userResetPassword`, the web UI's Users page, or `spoolframe-server user
reset-password`). The server shows it to the admin once. It works for 7
days; after that only another reset helps. The reset ends the user's
sessions that rest on the old password: every session signed in with a
password, every session a device approval or a trusted device's resume
made from one, and every session from before the server recorded how
each was signed in. It forgets the devices trusted on the same rule.
Sessions signed in with a passkey, a sign-on provider or a portable
identity's credential stay, as do the passkeys, provider links and ties.

`login_password` with the one-time password answers `logged_in` with
`password_change_required: true` and `remembered: false`: the device is
not trusted, whatever the sign-in asked. That session may send only
`change_password`, `logout` and `who_am_i`. Everything else, resource
streams included, is `password_change_required`. The app shows "Choose a
new password" and sends `change_password { current_password,
new_password }` with the one-time password as `current_password`.

`change_password` checks `current_password` as `login_password` does: a
wrong one is `invalid_credentials` and counts towards the same limit. A
`new_password` that is too short, or the same as the current one, is
`invalid_input`. A user with no password gets `no_password`, and a
portable user `not_for_portable_users`. It ends every other session of
the user's, on the web and on devices, and the session that sent it may
do anything again. Any user may use it to change their own password,
after a reset or not.

A passkey or sign-on sign-in while a one-time password is pending is not
limited: only the password was reset.

## App passkeys: a relying party per server

Each server has its own app relying party (§23):

| | Value |
|---|---|
| RP ID | `<server id>.spoolframe.app` (`ServerId::rp_id`) |
| Origin | `https://<server id>.spoolframe.app` (`ServerId::origin`) |

With an RP per server, a security key offers each server only its own
passkeys. The server accepts only its own RP ID hash and its own origin,
for registration, sign-in and confirmation. `https://spoolframe.app` is
not its origin: WebAuthn wants the origin's host at or under the RP ID,
and `spoolframe.app` is above `<server id>.spoolframe.app`.

`PasskeyCreation` and `PasskeyRequest`, which the SDK gives the app,
carry `rp_id` and `origin`. The SDK makes them from the server id it
pinned with the key. The app gives `rp_id` to the platform's passkey
API, and `origin` where the platform takes one (webauthn-authenticator-rs
on Linux). `PasskeyRegistrationBegun.user_name` is `<username> @ <server
name>`, because the RP ID says nothing a person can read; `display_name`
is the user's display name, or the username.

The web UI's own relying party, the host of `admin.public_url`, is
separate.

### Passkeys made for `spoolframe.app`

Early app builds made passkeys for the shared RP ID `spoolframe.app`
(`LEGACY_APP_RP_ID`). The server keeps them, but they sign no one in: a
sign-in or confirmation with one is `InvalidCredentials`, and the log
says the passkey "was made for another host". `ListPasskeys` lists each
with `app: true` and `usable_here: false`. The web UI lists it with
`app: true` and `usable: false`, and says it is an old app passkey to add
again. The user removes it as any other passkey. The server logs each
one at start. They still count as a way to sign in for the last-method
rule, as web passkeys of another host do. `is_app_rp_id` tells an app
passkey's RP ID from a web one.

### App passkey challenges

The RP ID comes from the server id a server names in `ServerInfo`, and a
server can name another server's id. The platform writes only the origin
and the challenge into `clientDataJSON`. So the challenge names the
server by its key: otherwise a server could claim another server's id,
begin a ceremony there, hand that challenge to an app as its own, and
finish there with what the app signed.

`BeginPasskeySignIn` and `BeginConfirmPasskey` reply `PasskeySignInBegun
{ nonce, allow_credential_ids }`. `BeginPasskeyRegistration` replies
`PasskeyRegistrationBegun { nonce, user_id, user_name, display_name,
exclude_credential_ids }`. Each `nonce` is 32 random bytes. There is no
`challenge` field. The challenge is:

```text
SHA-256("spoolframe/app-passkey-challenge/1" || server endpoint id (32 bytes) || nonce)
```

`spoolframe_protocol::app_passkey_challenge` computes it. The server uses
its own endpoint id. The client uses the endpoint id it pinned and
dialled, never an id the server sends. A signature over another server's
challenge is `InvalidCredentials`.

`BeginPasskeySignIn` lists no credential ids, with or without
`username`: anyone can send it. App passkeys are discoverable, so the
platform finds them. With `username`, only that user's passkeys finish
the sign-in.

### The user handle

`FinishPasskeySignIn` (and `FinishConfirmPasskey`, which carries the same
fields) has `user_handle: Option<bytes>`. The key is always there:

| Value | Meaning |
|---|---|
| CBOR byte string, 1 or more bytes | The handle the authenticator keeps with the passkey. It must be the passkey's user's, or the reply is `InvalidCredentials`. |
| CBOR null | The platform gave no handle (webauthn-authenticator-rs, for a phone over caBLE). The passkey's credential id says whose it is. |
| Empty byte string | `InvalidInput`. It uses no challenge and does not count as a failed sign-in. |
| Key not there | The message does not decode. |

The SDK sends an app's empty handle as null.

## Where a device asked from

A device sign-in (`BeginDeviceSignIn`, approved on another device with
`LookupDeviceSignIn` and `ApproveDeviceSignIn`) shows the approver the
device's own name, which an attacker chooses: one can begin a sign-in,
send someone the approval link or code, and get a session when they
approve it (device-code phishing). So the approval also shows where the
device's `BeginDeviceSignIn` came from, which the attacker cannot
choose.

`DeviceSignInRequest.requested_from` is `null` if the server could not
tell, or a `DeviceAddress`:

| Field | Value |
|---|---|
| `address` | The IP address as text, `"192.168.86.20"`; an IPv4 address never in its IPv6-mapped form. |
| `network` | `"loopback"` (the server's own machine), `"local"` (private IPv4, link-local, CGNAT and VPNs' `100.64.0.0/10`, IPv6 unique-local, or any address in the network of one of the server's own interface addresses, by that interface's prefix length) or `"internet"` (any other: a public address). An unknown value decodes as `Unknown`. |

The server judges `network` as the web approval page judges
`fromInternet`, reading its interfaces at each lookup. A home with IPv6
gives its devices public addresses in the server's own /64, so those are
`local`; an interface with no netmask counts as a /64 for IPv6 and as
its one address for IPv4. Only the server's own networks count, never a
wider ISP block: another /64 of the same /56 is `internet`.

An app's approval screen shows the address ("The TV asked from
192.168.86.20."), and for `internet` or `Unknown` warns instead: the
device is not on this network, so approve only a device you can see; a
code someone sent you is theirs. The SDK re-exports `DeviceAddress` and
`AddressNetwork`; `Session::lookup_device_sign_in` returns the field.

## Trusted devices

A user can make a device trusted (§24, §40). A trusted device starts a
new session for that user without a full sign-in. It is a row in the
server's `trusted_endpoints` table, for one user and one Iroh endpoint.
The device's key is the proof. A PIN unlock never makes trust or renews
it.

### Making trust

| Request | Field |
|---|---|
| `LoginPassword` | `remember: bool` |
| `FinishPasskeySignIn` | `{signed: FinishPasskeySignIn, remember: bool}` |
| `BeginDeviceSignIn` | `{remember: bool}`: the device asks to be trusted |
| `LookupDeviceSignIn` | its reply, `DeviceSignInRequest`, has `remember: bool`: the device asked |
| `ApproveDeviceSignIn` | `remember: bool`: the approver's answer |
| `SetDeviceTrust` | `{device: Option<String>, trusted, auto_login}` |

Clients send `remember: false` unless the user ticked "Remember this
device", and nothing ticks it by default.

After a device sign-in, the device is trusted only if it asked and the
approver's `remember` is `true`. An approver cannot trust a device that
did not ask.

`LoggedIn.remembered` is `true` when the device is trusted for the
session's user after the sign-in, so the device learns whether the
approver kept "Remember". The web UI's own sign-in never makes trust,
because a browser is not an Iroh endpoint.

`SetDeviceTrust` needs a session. `device` is an endpoint id from
`Devices`, or `null` for this device:

- Trust or auto-login can be turned on for this device only. For another
  device, the reply is `InvalidInput`; only the web UI trusts another
  device.
- Turning trust or auto-login on needs a recent sign-in.
- Auto-login needs trust, or the reply is `InvalidInput`.
- Trust can be turned off for any of the user's devices. This ends the
  user's sessions on that device, the caller's too if it is this device.
  For another device it needs a recent sign-in.
- A device that is not the user's gets `NotFound`.

### Resuming

These two requests are pre-auth:

| Request | Reply |
|---|---|
| `{"type": "trusted_profiles"}` | `TrustedProfiles { profiles }` |
| `{"type": "resume_profile", "user_id": "local:…"}` | `LoggedIn { session, remembered: true, … }` |

`TrustedProfiles` lists only the connecting endpoint's rows, each
`{user_id, username, display_name, pin_required, auto_login}`. It is a
read, and the SDK may retry it.

`ResumeProfile` is a mutation, so it carries an operation id. It opens a
normal session, bound to this endpoint as a sign-in's is. A refusal is
`Unauthenticated`, the same for every reason: the device is not trusted
for the user, the trust lapsed or was removed, or the user is disabled.
If the device requires the user's PIN, the new session starts locked. A
resumed session is not a recent sign-in, because nobody proved who they
are: until the user confirms, each request in "Confirming it is you"
gets `RecentSignInRequired`.

The server never opens a session by itself. For auto-login, the client
calls `TrustedProfiles` at start and resumes the profile that has
`auto_login`.

### Auto-login and the PIN

At most one user per device has `auto_login`. Turning it on for one user
turns it off for the others on that device, and turns off the PIN
requirement for that user on that device. Turning the PIN requirement on
(`SetPinRequired`) turns auto-login off.

### What ends trust

Trust never lasts longer than what it would bypass:

- `Logout` on the device removes the device's trust for that user.
- When the user or an admin ends a session (`me.revokeSession`,
  `admin.userRevokeSession`), that device's trust goes too.
- `me.revokeOtherSessions` removes the trust of every other device.
- Ten wrong PINs in a row revoke the device's sessions and its trust.
- A password change, an admin's password reset, and disabling the user
  each remove all of the user's trust. Enabling the user again does not
  bring it back.
- Turning trust off ends the user's sessions on that device.
- Trust lapses after `auth.trust_idle_days` without use (default 180).
  A resume uses the trust, and so does any use of the user's session on
  that device; these writes happen at most once a minute. Lapsed trust
  is refused and removed.

A session expiring does not end trust. That is the purpose of trust.

### Deleting a device

| Request | Reply |
|---|---|
| `{"type": "forget_device", "device": "<endpoint id>"}` | `Done` |

`device` is an endpoint id from `Devices`, and it is required. The
server deletes the user's sessions on that device, live or revoked, and
the device's trust and PIN requirement for the user. The device leaves
`Devices`; if it signs in again, it is a new device.

- It needs a session, and a locked profile gets `ProfileLocked`.
- It is a mutation, so it carries an operation id and is not retried.
- It always needs a recent sign-in (`RecentSignInRequired`).
- This device gets `InvalidInput`. To leave this device, send `Logout`.
- A device that is not the user's gets `NotFound`.
- Other users' sessions, trust and PIN requirement on that device stay.
  The server closes the device's connection with `SIGNED_OUT` only when
  no user's session is left on it.

The web UI sends `me.forgetDevice(endpointId)`, and an admin sends
`admin.userForgetDevice(username, endpointId)`. Both always need a recent
sign-in (`RECENT_SIGN_IN_REQUIRED`). A device the user never signed in
on gets `NO_SUCH_DEVICE`.

`DeviceInfo` (from `ListDevices`) has `trusted` and `auto_login`.

### SDK

- `Client::sign_in(server, username, password, remember)`.
- `Client::finish_passkey_sign_in(server, signed, remember)`.
- `Client::begin_device_sign_in(server, remember)`. The returned
  `DeviceSignIn` has `remember`.
- `Client::trusted_profiles(server)` returns `Vec<TrustedProfile>`.
- `Client::resume_profile(server, user_id)` returns a `Session`. If the
  device is no longer trusted, it returns `SdkError::SignInRequired`.
- `Session::remembered()` returns `Option<bool>`: `Some` for a session
  that a sign-in or a resume made in this process.
- `Session::set_device_trust(device, trusted, auto_login)`. When it turns
  this device's trust off, the SDK also forgets this session. The
  profile stays in `saved_profiles`.
- `Session::approve_device_sign_in(code, remember)`.
- `Session::forget_device(device)` deletes another of the user's
  devices. For this device it returns `SdkError::InvalidInput`: use
  `Session::sign_out`.

## Invites

An invite finds a server from anywhere and, with a code, makes an
account there. It is one string, the link, shown as a QR code and pasted
as it is (`spoolframe_protocol::invite`):

```text
spoolframe://invite?server=3f9a…a0f1&name=Home%20Media&addr=203.0.113.24:41641&relay=https://relay.home-media.example&exp=1792003200&role=consumer&cap=QW7K-2MFP-XR4T
```

| Parameter | |
|---|---|
| `server` | The server's `EndpointId`, 64 hex digits. Required. |
| `name` | What the server calls itself. |
| `addr` | An address it may be reached at, `ip:port` or `[ipv6]:port`. Repeated, one per address. |
| `relay` | An `http(s)` URL of a relay it may be reached through. Repeated. |
| `lookup` | An `http(s)` URL of the pkarr relay the server publishes its record at. |
| `exp` | When the code stops working, in Unix seconds. A hint. |
| `role` | `consumer` or `admin`: the role the account gets. A hint. |
| `kind` | `portable-account`: the code grants a portable identity access rather than making an account. Absent, it makes an account. A hint. |
| `cap` | The code, 12 characters from `ABCDEFGHJKMNPQRSTVWXYZ23456789`, shown in groups of four. |

Values are percent-encoded where they would break the link (`&`, `=`,
`#`, `%`, `+`, spaces, anything outside ASCII); `:` and `/` are written
as they are. A reader ignores a parameter it does not know, and a hint
it cannot read, as in the device sign-in link. A link without `server`,
with a `server` that is not a key, or with a `cap` that is not a code is
not an invite. At most 32 `addr` and 32 `relay` hints are read. A link
longer than 4096 bytes is not an invite either, and `name` is cut to 128
characters, as `Hello`'s strings are.

Without `cap`, the link only finds the server. A client pins the key
straight from it (it came from the server's admin, so there is nothing to
trust on first use), learns the server id at `Hello` and pins the pair,
and adds the hints to the addresses it knows as told ones. The user then
signs in as usual. `exp` and `role` only fill in the join form ("Joins
as a consumer · expires Oct 14"); the server decides both when the code
is redeemed.

With `cap`, the client sends, before any session:

```json
{"type": "redeem_invite", "code": "QW7K-2MFP-XR4T", "username": "mum",
 "display_name": "Mum", "password": "correct horse"}
```

`display_name` is `null` for none. The code may have any case, dashes or
spaces. The answer is `logged_in { session, remembered: false }`: the
account is made with the invite's role, and the session is bound to the
device that sent it, as a password sign-in's is. Refusals:

- `invite_invalid`: no invite has that code, or its invite was revoked.
  It is counted as a wrong guess before it is checked, as a wrong
  password is, so guessing ends in `rate_limited`.
- `invite_expired`, `invite_exhausted`: the code is right, and its
  invite is past its expiry, or has made every account it could.
- `username_taken`; `invalid_input` for a username, display name or
  password that is not acceptable. Neither uses the invite up.

A redemption draws on the sign-in budget of devices that are not signed
in (`rate_limited` once it is spent). A refused redemption is not kept
as an operation: sent again with the same operation id, it runs again.
Of two redemptions of an invite's last use, one makes its account and
the other is `invite_exhausted`. It is never retried on its own on a new
connection.

### Portable-account invites

An invite of kind `portable-account` (its link says
`kind=portable-account` and has no `role`) grants someone with their own
portable identity access, and makes no account: they watch as
`PortableUser(U)` (§51), appear in `admin.grantList`, and never in
`userList`. Their phone, which holds the identity, redeems it with its
own device credential, over a nonce from `begin_portable_sign_in` (see
"Signing in with a device credential"):

```json
{"type": "redeem_portable_invite", "code": "QW7K-2MFP-XR4T",
 "sign_in": {"credential": {…}, "proof": "<128 hex>", "states": [],
             "display_name": "Sam"}}
```

The answer is `logged_in`, the session `finish_portable_sign_in` would
give: the tied account here, if the identity has one, with no grant made
and no use taken; else the new portable session. The credential is
checked, and counted, as there; then the code, as `redeem_invite` checks
it (`invite_invalid` counted, `invite_expired`, `invite_exhausted`). An
identity granted here already keeps its grant, and the invite its use.
`display_name` is what the grant calls them (`invalid_input` if it is
not acceptable as a display name).

A code redeemed the wrong way is `wrong_invite_kind`: a
`portable-account` invite's code to `redeem_invite`, or a `create-user`
one's to `redeem_portable_invite`. The code was right, so it is not
counted, and the invite keeps its use.

`admin.inviteCreate` makes one with `kind: "portable-account"` and no
role (`INVALID_INPUT` for a role other than `consumer`), as does
`spoolframe-server invite create --kind portable-account`. `federation`
invites are refused until federation exists.

### Making invites in an app

An admin makes, lists and revokes invites from an app as the web UI's
Invites page does: the same kinds, fields, limits and store, so each
lists and revokes the other's.

```json
{"type": "create_invite", "kind": "create-user", "note": "Mum",
 "uses": 4, "expires_in_secs": 172800, "role": null}
```

Each `null` is the default (one use, 7 days, `consumer`). The answer is
`invite_created { invite, code, link }`: the code (`QW7K-2MFP-XR4T`) and
the link with it, shown this once, and `invite`, as `list_invites` lists
it:

```json
{"id": "0192…", "kind": "create-user", "note": "Mum", "role": "consumer",
 "uses": 4, "uses_left": 3, "expires_at": 1792003200,
 "created_at": 1791830400,
 "created_by": {"username": "andrey", "display_name": "Andrey"},
 "created_from": {"type": "device", "endpoint_id": "3f9a…",
                  "name": "Andrey's iPhone", "platform": "ios"}}
```

`created_from` is `web_admin`, `cli` or `device` (the device's
`EndpointId`, and the name and platform its `Hello` gave). A CLI
invite's `created_by` has no `username`, and `display_name` "server
CLI". `list_invites` lists only invites that can still be redeemed,
newest first. `revoke_invite { id }` stops one's code (`done`); one not
on the list is `not_found`.

All three are an admin's: anyone else gets `admin_required`, and a
portable guest `not_for_portable_users`. `create_invite` and
`revoke_invite` also need a recent confirmation, as the web UI's admin
changes do: `recent_sign_in_required` past it, and `confirm_password` or
a passkey confirmation lets them through. A field out of its limits, and
a `federation` invite, is `invalid_input`. Neither is retried on its own
on a new connection: a second `create_invite` makes a second code.

### SDK

`spoolframe_client_sdk::invite` is the link codec. A `Client` takes the
link as the user pasted or opened it:

- `read_invite(link) -> InviteInfo`: what the link says, with nothing
  sent: the key and its fingerprint, the name, the hints, whether it has
  a code, and the expiry and role hints as `expires_at` and `role`.
  `saved` says the key is a saved server already. `NotAnInvite` for
  anything else.
- `connect_invite(link) -> ServerEntry`: `Hello` at the link's hints,
  for "Is this the server you were invited to?": the name, fingerprint
  and server id that answered. Nothing is saved.
- `save_invite_server(link) -> ServerEntry`: pins the key as the link
  names it, verified, with the server id it answers with ("Save server
  only", and all a link without a code does). A saved server keeps its
  pin and gains the hints.
- `redeem_invite(link, username, display_name, password) -> Session`:
  `redeem_invite`, then the server saved as above and the session kept
  as a password sign-in's. A refused redemption saves nothing. Its
  refusals are `InviteInvalid`, `InviteExpired`, `InviteExhausted`,
  `UsernameTaken`, `InvalidInput` and `RateLimited`.

An admin's `Session` makes them: `create_invite(InviteSpec) ->
MintedInvite` (its `link` and `copy_string` are the same link, the one
the web UI copies), `invites() -> Vec<InviteSummary>` (`used` is
`uses - uses_left`; `created_from`'s `Device` says `this_device`) and
`revoke_invite(id)`. Their refusals are `AdminRequired`,
`NotForPortableUsers`, `RecentSignInRequired` (confirm, then ask again),
`InvalidInput` and `NotFound`.

The link's addresses join the server's known addresses as told ones,
replacing none; the server's next `server_addresses` event replaces them
as it replaces any told address. Its relays are kept beside the ones the
server told. What a device keeps from links and from `server_addresses`
is held to a link's limits: relays and lookups that are `http(s)` URLs
only, at most 32 relays and 32 addresses per server, newest first, and
at most 8 pkarr relays looked up in all. A key the device has not saved
is another server, whatever its name; one that answers with a saved
server's server id is that server with a new key, `IdentityChanged`,
before any account is made.

## Relay access

A relay of the server's own (`spoolframe-server relay run`) asks the main
server who may relay through it, on a second ALPN on the main server's
endpoint, `spoolframe/relay-access/1` (`spoolframe_protocol::relay_access`).
It is not the client protocol, and no app speaks it.

The main server accepts it only while `network.own_relays` names a relay,
and answers only those relays' keys: any other key is closed
`UNSUPPORTED_PROTOCOL` at once. A named relay holds at most 2 connections
on it; one more is closed `BUSY`.

One bidirectional stream per question, one frame each way:

```json
{"endpoint": "3f9a…a0f1", "invite": "sf-invite.1.Xq3…"}
```

answered

```json
{"tier": "owner"}
```

`tier` is `owner`, `friend` or `stranger`; a tier the relay does not
know is a stranger. The question is at most 1 KiB and must arrive within
10 s of the stream opening. `invite` is left out when the device has no
token.

- The server itself is `owner`.
- A device with a live session of a local account, or trusted for a
  user, is `owner`: a consumer's as much as an admin's.
- A device with a portable user's session is `friend`.
- A device that is no one's, with a live invite's token (below), is the
  tier of the invite's kind: `create-user` (an account here, of either
  role) is `owner`; `portable-account` and `federation` are `friend`.
  The kind alone decides it.
- Anyone else is a `stranger`. Servers paired with this one will be
  friends once federation exists.

### The invite holder's token

A device redeeming an invite is no one's until it has redeemed it. Off
the LAN the relay may be its only way to the server, so it proves to the
relay that it holds the code. It connects to the invite's relays with an
auth token, sent as `Authorization: Bearer` (iroh's
`RelayConfig::with_auth_token`):

```text
sf-invite.1.<base64url, no padding, of BLAKE3-keyed(key = BLAKE3(code), msg = endpoint key)>
```

`code` is the invite's code, normalized (`QW7K2MFPXR4T`); the endpoint
key is the device's 32 bytes, which the relay's handshake has already
proved. The token proves the code without carrying it, and admits no
other endpoint: a token copied from a log is no use to anyone else
(`relay_access::invite_token`).

The relay passes a well-formed token on in `invite`, and nothing for
anything else.

The server checks a token only for a device that is no one's, against
every live invite (not revoked, expired or used up), in constant time.
It is a sign-in a stranger makes, so it is guarded as redeeming is: it
draws on the strangers' budget, and counts as a guess at an invite code
from that endpoint, taken back only when it matches. A token that
matches no live invite stays counted, with the device's wrong codes in
`RedeemInvite`, and the device is what it would be without it.

The SDK holds the token on the link's relays from `connect_invite` to
`redeem_invite`, and drops it once the invite is redeemed or the server
is saved without it: from then on the session says who the device is.

The token is not a session. The relay only carries the holder's packets
to the server, where redeeming is guarded as usual.

**Offline guessing.** The token is a MAC under the code's hash: whoever
has a token and its endpoint key can test codes against it offline, 59
bits of them, with no limit. The token travels only in the
`Authorization` header, over TLS, to the operator's own relay. Proxies
do not log that header by default; do not make yours log it.

The relay keeps an answer for 60 s and a failure for 5 s, at most 10,000
of them, by endpoint and token: an endpoint's answer with a token is not
its answer without one. It lets through the tiers its `access` allows.
An answer it cannot get is a stranger's.

The kept answer decides only whether a new connection is let through.
iroh keeps one relay connection per endpoint for as long as it can, so
the relay asks again, every 5 s, about each endpoint (and token) it
carries, and keeps the new answer. A connection whose endpoint is now in
a tier `access` does not let through is cut: a device signed out, a
session ended, an invite or grant revoked stops being relayed within
5 s and one question (at most 5 s more). Its reconnects are refused on
the new answer. An endpoint the main server does not answer for in a
recheck is carried on until a recheck that does. With `access.everyone`,
and for the main server itself, nothing is asked and nothing rechecked.
At most 2 recheck questions are open at once.

## Profiles and progress

One connection may carry several users' sessions (§15).
`ActivateProfile`, sent in a session, makes it the connection's
foreground profile and answers `Done`. Selecting another profile relocks
earlier PIN unlocks on the connection. Background library and account
requests in other sessions still work, under their PIN checks, and never
select a profile. Activating the same profile again changes nothing, and
a PIN check that crosses a profile switch cannot restore its old unlock.

The SDK's `Client::activate_profile` does this and returns an
`ActiveProfile` with the session's methods:

```rust
let active = client.activate_profile(&profiles[chosen]).await?;
active.unlock(pin).await?;
let home = active.home().await?;
```

Progress is reported per playback:
`UpdatePlaybackProgress { handle, sequence, position_ms }`. The sequence
numbers increase. The server checks that the handle is the session's,
refuses older sequences, and refuses progress from a playback of the
same item in that session that a newer one replaced (`StaleProgress`). A
newer playback stays authoritative even after it stops. Separate
sessions report independently. Invalid or stopped handles cannot report
progress. `PlaybackProgressSequence { handle }` answers the last
accepted sequence, for a handle an app restored.

The SDK assigns the sequence numbers, keeps them across transport
retries, and fetches the last accepted one when a handle is restored.
Callers must finish their final progress report before stopping the
handle or switching profiles; the Linux player queues those calls in
event order.

## Playback revisions

A playback starts at revision 0. A change that plans it again
(`ChangeSubtitle`, `ChangeQuality`) and a `SeekPlayback` raise it.
`ChangeSubtitle`, `ChangeQuality`, `SeekPlayback` and `StopPlayback` must
name the expected revision in `RpcRequest.expected_revision`. Checking
and changing share the per-handle control lock; a stale command gets
`RevisionConflict { revision }` and changes nothing. `PlaybackRevision
{ handle }` reads a handle's revision.

The SDK keeps revisions itself, and serializes controls for the same
server, session and handle across cloned sessions. It can query the
revision of a handle the app restored. It does not silently reapply a
conflicting control. Idle cache entries may be dropped and fetched
again.

The playback revision is separate from the encoder epoch. The server may
regenerate HLS segments, changing the epoch, without changing the
playback's configuration.

### SDK: a playback the server ended

The server ends a playback that idles out, at a restart, and when it
lets a device's playbacks go; a later use of its handle is
`PlaybackExpired`. A new `StartPlayback` of the same item, selection and
`bitmap`, with `start_ms` at the viewer's position, carries on: HLS
segments are numbered on the item's own time, so the media playlist of
the same plan is the same, and segment `n` of the new playback is
segment `n` of the old. A progressive output, and the bytes of a
segment, are the new encode's, and do not continue the old one's.

`ManagedPlayback` in the SDK does this itself, at most 3 times in 5
minutes. It does not start again after `Unauthenticated`, or when the
start is `NotFound` (the item is no longer served to the user),
`CannotPlay` or `PlaybackStopped`. A client that holds raw handles does
the same.

## Resource streams

A resource stream starts with byte `2`. One CBOR `ResourceRequest` is
followed by one CBOR `ResourceResponse` and exactly the announced raw
body length. Error replies have no body. Each transfer uses its own QUIC
stream.

| Request | Carries | Header (`ResourceMetadata`) |
|---|---|---|
| `artwork { request, max_bytes }` | an `ArtworkRequest` | `artwork { content_type, etag }` |
| `media { request, revision }` | a `MediaRead { session, handle, offset, length, epoch, segment, variant }` | `media { size, complete, epoch, revision }` |
| `subtitle { session, handle, revision, offset, version }` | a chunk of a whole-file subtitle | `subtitle { format, total, version, revision }` |
| `subtitle_segment { session, handle, revision, segment }` | an HLS subtitle segment | `subtitle { … }` |

`ResourceResponse` is `ready { length, metadata }` or `error { error }`.

Media and subtitle requests carry the playback revision. The server
checks it before serving and refuses reads that a change made during
preparation invalidated. Media also has its encoder epoch and byte
range; a subtitle file keeps its `version` check between chunks. The
SDK checks the resource kind, revision, body length and allocation
limits before returning bytes.

For media, `segment` is `null` for the file itself (direct play) or the
progressive output, `0` for an HLS init segment, and `n` for segment `n`.
`variant` is the ladder rung (see "Quality and adaptive HLS"), 0 for one
encode and for anything that is not a segment. The end of the file is
`complete` and `offset + sent == size`.

Media reads and subtitle segments are at most 16 MiB (`MAX_READ`);
subtitle file chunks at most 512 KiB (`SUBTITLE_CHUNK`). Artwork
requests state a maximum length; the SDK uses 64 MiB. The server bounds
file copies by the announced length, even if a file grows while it is
sent.

## Quality and adaptive HLS

`StartPlayback` has `quality`, an object tagged by `type`:

| Quality | Planned for |
|---|---|
| `{"type": "auto", "link_bitrate": n \| null}` | the link: `LINK_SHARE` (75%) of what the device measured it carrying lately, in bits per second; `null` if it measured nothing |
| `{"type": "bitrate", "max_bitrate": n}` | at most that, whatever the link |
| `{"type": "original"}` | the file as it is, whatever the link: converted only for what the device cannot play |

The capabilities' `max_bitrate` stays a ceiling over all three. An
unknown type is `auto` with nothing measured. The playback keeps its
quality for later re-plans.

`ChangeQuality { handle, quality, bitmap, position_ms, display }`,
answered `PlaybackChanged`, plans a playing playback again: the viewer
chose another quality, or, with `auto`, the device found its link too
slow. It is a playback control, so it carries the expected revision. A
plan that changes restarts at `position_ms` as a new epoch, from offset
0; one that does not is answered as it is, nothing restarted. A plan
nothing plays under is `CannotPlay`, and the playback goes on as it was.

### The ladder

A converted plan has `ladder`: the encodes an HLS player switches
between, highest first, each a `Rung { height, max_bitrate, video }`.
It is empty for one encode, which is every plan except `auto` quality
over HLS.

The multivariant playlist of a ladder lists one variant per rung at
`v{n}/media.m3u8`, `n` being the rung's place in `ladder`, with the rung
the link chose first. Under `v{n}/` are that rung's `init.mp4` and
`seg{k}.m4s`, on the same segment grid as every other rung.
`GetManifest` and `MediaRead` carry `variant`: that `n` (0 for one
encode, and for anything that is not a segment or a media playlist). One
encode runs at a time: a read of another rung's segment moves it there,
and the rung it left keeps what it wrote, readable.

A rung's `video` is its own converted video where it is not the plan's
resized; `null` means the plan's. In a remux ladder (used off the LAN in
`auto`), the plan's `video` is a copy, the top rung has no `video` (the
copy), and the rungs under it each have an `encode`. All are cut at the
source's keyframes, on one grid.

### Display capabilities

`PlaybackPlan`, `StartPlayback`, `ChangeSubtitle` and `ChangeQuality`
carry `display`: `{ bit_depth, hdr }` (`DisplayCapabilities`, both fields
required inside it, as in `VideoLimits`), what the screen shows now. It
replaces `Hello`'s `max_video.bit_depth` and `max_video.hdr` for that
plan, and the playback keeps it: a re-plan without one plans for the
screen it started on. A TV whose display mode changes while the app runs
(Match Dynamic Range, another TV or receiver) sends it at each start.
With `null`, `Hello`'s apply.

## Output codecs and HDR

A conversion's video codec is the server's to choose, in the order its
admin set (`transcode.video_codecs`: AV1, HEVC, H.264 by default), among
the codecs the client plays and a GPU of the server's encodes. In
software the server encodes only H.264.

- `ClientCapabilities.hardware_video` (`[codec]` or `null`): the codecs
  of `video` the device decodes in hardware. A GPU codec is chosen only
  where the device decodes it in hardware; `null` counts every codec in
  `video`. An app sends what its platform decodes in hardware (Apple:
  `VTIsHardwareDecodeSupported`), so a device with no AV1 decoder is not
  sent AV1 to decode in software.
- An HDR film for a device whose `max_video.hdr` has its HDR format
  (`null`: every one), with `max_video.bit_depth` 10 or more (or
  `null`), keeps its HDR: HEVC Main 10 or AV1 10-bit, BT.2020, the
  source's transfer, its HDR10 metadata. `ConvertedVideo::Encode`'s
  `tone_map` is then `false`. Dolby Vision keeps its base layer's HDR
  (HDR10, or HLG for profile 8.4): the device must show that format.
  Profile 5 has no such base layer and is tone mapped.
- Dolby Vision copied: `ConvertedVideo::Copy.dolby_vision` says the copy
  is labelled Dolby Vision, for a device whose `max_video.hdr` has
  `dolby_vision` (or is `null`), the stream being profile 5 or 8.
  Otherwise a Dolby Vision stream whose base layer the device shows
  (HDR10, HLG or SDR; profile 7 always) is copied as that base layer,
  unlabelled, rather than converted. Labelled, profile 8 is named
  `hvc1.2.4.L…` with `SUPPLEMENTAL-CODECS="dvh1.08.LL/db1p"` (`db4h`
  HLG, `db2g` SDR) and muxed with its `dvvC`; profile 5 is
  `dvh1.05.LL`, tagged `dvh1`.
- The HLS playlist names what is written: `hvc1.2.4.L…` (Main 10) or
  `av01.0.<level>M.10` for kept HDR, with `VIDEO-RANGE=PQ` or `HLG`; a
  tone-mapped or 8-bit encode `hvc1.1.6.L…`, `av01.0.<level>M.08` or
  `avc1…`, with `VIDEO-RANGE=SDR`. The fMP4 init segment carries `colr`,
  `mdcv` and `clli`, and HEVC is tagged `hvc1`.

## The WAN uplink budget

The server splits its home's upload between the playbacks of devices off
its LAN (`network.uplink`). A device is on the LAN when the address its
connection uses is in one of the server's own interface subnets (a
global IPv6 address in the home's /64 too), private, carrier-grade NAT,
link-local or unique local (a WireGuard or Tailscale peer too), or in
`network.lan_prefixes`, and not in `network.wan_prefixes`. Between two
ranges that both hold the address, the longer prefix decides, and
`wan_prefixes` wins a tie. A relay path is never the LAN.

- `StartPlayback` off the LAN with `auto` quality is planned under the
  playback's share of `network.uplink.budget_mbps`, and under the
  client's measured `link_bitrate` if that is lower. On the LAN, and for
  `original` or a chosen `bitrate` anywhere, the budget is not applied.
- The share is equal, but while an admin plays off the LAN beside
  anyone else, the admins' playbacks share
  `network.uplink.owner_share_percent` of the budget and the others the
  rest.
- One playback off the LAN beyond `network.uplink.max_streams` is
  refused with `Error::WanStreamsFull { max }`.
- When a playback off the LAN starts or ends, the others' shares move.
  Each one whose share moved by 15% or more is told
  `Event::PlaybackShare { handle, max_bitrate }` on its connection's
  events stream, a few seconds after the last start or end. A client in
  `auto` re-plans with `ChangeQuality` (the same `auto`, at its
  position), which the server plans under the new share.

### SDK

- Converted reads (HLS segments, progressive chunks) of 512 KiB or more
  are metered from their first byte to their last, so the server's wait
  for its encode is not counted as the link's time. Direct reads are
  timed whole.
- A managed HLS playback in `auto` that is not a ladder notes each whole
  segment's bytes against its time. When the link carries less than
  `LINK_SHARE` of what the last segments needed, it asks to be planned
  for the link (`limit`), at most once every 30 s. A ladder is left to
  the player.
- On `Event::PlaybackShare` for its handle, a managed playback in `auto`
  re-plans with `change_quality(Auto, position)`.
- `SdkError::WanStreamsFull { max }`.

## Picture subtitles

Each user chooses how picture subtitles (PGS, VobSub, DVB) are shown, in
`TrackPreferences`: `subtitles.bitmap` is `"text"` (read as text by the
server, the default) or `"pictures"`. The choice is per user, so it is
the same on every device. An unknown value reads as `"text"`.

The server applies it when a `PlaybackPlan`, `StartPlayback` or
`ChangeSubtitle` request has `bitmap: "auto"`:

| Preference | The server reads the track | Else |
|---|---|---|
| `text` | sent as SubRip or WebVTT (`ocr`) | drawn by the client if it can, else burned in |
| `pictures` | not used | drawn by the client if it can, else burned in |

The server cannot read a track when `subtitles.ocr` is off, when
tesseract has no data for the track's language, or when the client takes
neither SubRip nor WebVTT. With `subtitles.ocr` off the preference has
no effect. A request with `bitmap: "text"` still gets text, whatever the
preference, or the plan is `Unsupported` if the server cannot read the
track. A request with `bitmap: "burn_in"` is planned as `pictures`: the
pictures are burned in only if the client cannot draw them.

The rules, for every plan:

- A text subtitle is never burned in. It is copied if the client shows
  it, else converted to a text format the client shows (WebVTT for HLS).
  If the client shows no text format the output can carry, the subtitle
  is not shown: the plan has no subtitle, and a reason says why, e.g.
  "subtitle 1: the client shows no text subtitles; not shown". The video
  is never encoded because of a text subtitle.
- A picture subtitle is burned in only when it is not read as text (OCR
  is off, the server cannot read the track, or pictures were asked for)
  and the client cannot draw it in this delivery.

The plan says which was used. A direct plan has the picked subtitle with
its format and delivery: `pgs` `embedded` is drawn by the client, and
`srt` or `web_vtt` `external` is read as text. A converted plan has a
`SubtitleAction` of `copy`, `convert`, `burn_in` or `ocr`.

`GetTrackPreferences` answers `TrackPreferences { preferences,
ocr_offered }` (`TrackPreferencesInfo`). `ocr_offered` is true when
`subtitles.ocr` is on and tesseract has at least one language. When it
is false, apps show the picture-subtitle choice disabled, because
pictures are shown whatever it says. The admin API's
`me.trackPreferences` answers the same, as `{ preferences, ocrOffered }`.
`SetTrackPreferences` and `me.setTrackPreferences` take the bare
preferences. The SDK's `Session::track_preferences` returns
`TrackPreferencesInfo`.

## Paused playback and the idle limit

A playback handle lives for 30 minutes without use. These count as use:

- a media read: a range of the file, an HLS playlist, or an HLS segment;
- a request that names the handle: `UpdatePlaybackProgress`,
  `SeekPlayback`, `ChangeSubtitle`, `ChangeQuality` or `StopPlayback`.

While the player is paused, send `UpdatePlaybackProgress { handle,
sequence, position_ms }` once a minute, with the paused position and the
next sequence number. This keeps the playback alive for as long as the
pause lasts. It does not change the watch state: the same position gives
the same result, and a paused item does not become watched.

A progress report keeps a playback alive, but does not count as playing.
If the device starts another playback and the server has no conversion
slot free, the server ends a converted playback of that device that was
not read in the last 30 s. A paused playback is one of these.

After the limit, every use of the handle gets `PlaybackExpired`. Do not
send the old handle again: call `StartPlayback` with `start_ms` at the
paused position, and play the new handle. `ManagedPlayback` does this
itself; see "SDK: a playback the server ended".

The server logs each playback it ends, at INFO, as `a playback ended`,
with the reason (`idle`, `replaced`, `device_gone`, `signed_out`,
`access_removed`, `stopped_by_admin` or `failed`) and how long the
playback was unused. A client's own `StopPlayback` is not logged.

## Home rows and "Remove"

`Home` shows a show in one row only. While an episode of a show is in
progress, that episode is in Continue watching and the show has nothing
in Next up. When the user finishes the episode, the next episode goes to
Next up.

`HideFromContinueWatching { id }` removes one card: the movie or the
episode named by `id`, from Continue watching or from Next up, wherever
it is. It does not remove the episode's show. Send the card's own `id`,
never its show's. `UnhideFromContinueWatching { id }` puts that card
back.

- The show's other episodes stay on Home.
- A removed Next up episode stays out until the user watches it. Then
  the episode after it goes to Next up.
- A removed episode in progress does not keep its show out of Next up.
- Playing or marking the removed item brings it back. Playing another
  episode of the show does not.

## Leaving a server

`LeaveServer` takes the session's user off this server: `Done`, and every
session of theirs here ends, the one that asked too. It is a mutation,
never sent again on a new connection, and needs a session that signed in
within the last 10 minutes (`RecentSignInRequired`):

- a local user confirms with `ConfirmPassword` or a passkey
  (`BeginConfirmPasskey`, `FinishConfirmPasskey`), then asks again;
- a portable user has no password: it signs in again with its device
  credential (`BeginPortableSignIn`, `FinishPortableSignIn`), and leaves
  from the new session.

What leaving does:

- **A portable user**: its grant is taken away (kept as a tombstone, as
  an admin's revoke keeps it, so an invite made before does not bring it
  back), its sessions here end, and its watch state, hidden rows and
  track preferences here are deleted. Coming back takes a new invite.
- **A local account**: set aside, not deleted. It is disabled and marked
  as left, its sessions end, and no device stays trusted for it; its
  data, passkeys and ties to portable identities stay. Signing in is
  refused as for a disabled account. An admin restores it as they enable
  one (`admin.userEnable`); the admin API shows `leftAt` on `UserInfo`.

The last enabled admin, or the last enabled admin with a password,
cannot leave: `LastAdmin`, and nothing changes.

The `WhoAmI` reply says ahead of time whether leaving would be refused,
so an app can grey out "Leave this server": `can_leave` is `false`
exactly when `LeaveServer` would answer `LastAdmin`, by the same check,
and `true` for everyone else (a portable user always). It is as of that
answer: an admin made or demoted later changes it, and `LeaveServer`
still refuses. It sits beside the user's fields in the same map.

## Portable identities

A portable user (§42–§49) is an Ed25519 root key the user holds on their
phone; its public key, 64 hex digits, is the `PortableUserId`. The root
never travels. What it signs does, and the formats are the protocol
crate's (`spoolframe_protocol::portable`), so any client can make and
check them.

Every signature is Ed25519 over a domain string (with its trailing NUL)
followed by fixed-width fields: keys and ids as their 32 bytes, integers
big-endian. There is one encoding of each value, so nothing needs
canonicalising. A verifier uses strict Ed25519 verification.

An identity may have more roots than the phrase root its id is: see
"Security key roots". Until then this section describes a phrase root.

### Device credential

The root's word that one device key may act as the user (§45). Each
device has its own Ed25519 key for this, apart from its Iroh endpoint
key.

```text
"spoolframe device credential v1\0"
user        32 bytes   the PortableUserId
device_key  32 bytes   the device's public key
serial      u64        larger for a later credential from one holder
issued_at   i64        Unix seconds
expires_at  i64        Unix seconds; required
```

signed by the root. In serde (JSON here):

```json
{"user": "ea4a…d22c", "device_key": "fd17…f618", "serial": 3,
 "issued_at": 1791504000, "expires_at": 1799280000,
 "signature": "9113…1308"}
```

Keys are 64 lowercase hex digits (either case is read), the signature
128. A credential is good at time `now` when the user's root signed it,
`issued_at <= now + 300` (five minutes of clock skew), `now <
expires_at`, and `expires_at - issued_at` is at most 90 days. Renewal is
a new credential for the same key with a larger serial. A verifier does
not read the serial beyond the signature: two credentials are told apart
by their device key. The SDK gives the issue time in milliseconds, or one
past the last serial it gave if that is larger, so two devices that hold
one root almost never give the same serial, and need no shared count.

### Identity state

The root's list of revoked device keys, each with when it was revoked
(§48):

```text
"spoolframe identity state v2\0"
user        32 bytes
revision    u64        higher is newer
issued_at   i64        Unix seconds; does not order states
count       u32        number of entries that follow
entries     sorted ascending by key, one per key, each:
  key         32 bytes
  revoked_at  i64      Unix seconds
```

```json
{"user": "ea4a…d22c", "revision": 2,
 "revoked": [{"key": "66be…473a", "revoked_at": 1791500400},
             {"key": "fd17…f618", "revoked_at": 1791417600}],
 "issued_at": 1791504000, "signature": "0185…da0d"}
```

The signature covers the set, so the order of `revoked` in the serde
form does not matter; a key listed twice counts once, at its earliest
`revoked_at`. There is no list of authorized keys: adding a device needs
no new state.

Revocation only grows. A verifier keeps the union of the entries of
every valid state it has seen for a user, older revisions included, at
each key's earliest `revoked_at`, and refuses any credential whose device
key is in that union. Separately it keeps the state of the highest
revision (at one revision, the lower digest; see "Rival states"), to
hand on to others. A later state from a second holder of the root that
did not know of a revocation cannot undo it.

A revocation lapses 90 days and 5 minutes (`REVOCATION_WINDOW`: the
longest credential plus the clock skew) after its `revoked_at`: no
credential issued before it can still hold, and a root issues none to a
key it has revoked. A verifier may drop lapsed entries from its union,
and a root leaves them out of the states it signs. A state may list at
most 1024 entries (`MAX_REVOKED`), so the limit bounds live revocations
only; a state with more is refused.

### Device proof

Possession of the device key, shown to one server:

```text
"spoolframe device proof v1\0"
server      32 bytes   the server's EndpointId, as the device dialled it
nonce       32 bytes   chosen by the server for this sign-in
```

signed by the device key the credential names. The server's id keeps a
proof from being replayed to another server, and the nonce from being
replayed to the same one.

### Signing in with a device credential

A device with a credential (from pairing, or the phone's own) signs in to
a server as the portable user (§58), by signing a device proof over the
server's nonce:

| Request | Reply |
|---|---|
| `{"type": "begin_portable_sign_in"}` | `{"type": "portable_sign_in_begun", "nonce": bytes(32), "expires_in_secs": 300}` |
| `{"type": "finish_portable_sign_in", "credential": {…}, "proof": "<128 hex>", "states": [{…}], "display_name": "Sam" or null}` | `LoggedIn`, `remembered: false` |

Both are pre-auth mutations, with an operation id, never sent again on a
new connection, and each draws on the sign-in budget of devices that are
not signed in, as `LoginPassword` does.

- The nonce is this device's (its endpoint), for 5 minutes, and the
  finish uses it up whatever it makes of it. Beginning again replaces
  it.
- The credential must hold now (its root's signature, its dates), its key
  must have signed the proof, and neither a state it carries nor one the
  server has seen may revoke the key.
- `states` are the identity's states the device has, at most 2
  (`MAX_SIGN_IN_STATES`), each the credential's own identity's and
  verified. The server keeps them only for an identity it knows (tied
  here, or with a live grant; for a redemption, once the grant is made):
  a stranger's are checked and dropped. A state it keeps that revokes a
  key ends every session that key opened, at once.
- Then, local first: an identity tied to an account here signs in as
  that account, a normal session (its PIN asked as usual) that counts as
  signed in long ago, so what needs a recent sign-in asks for a
  confirmation first. An untied identity with a grant here signs in as
  itself, a portable session. Any other gets nothing.

Every refusal is `InvalidCredentials`, counted as a wrong guess before
anything is checked, then `RateLimited`, and logged as a refused sign-in
(`portable credential refused`). Refusals keep nothing in the operation
pools.

A portable session lives as a device's does (renewed on use) while the
grant lasts, and never past the credential's expiry: an admin revoking
the grant (`admin.grantRevoke`) ends it at once, as does a state revoking
its key. The same holds for a tied account's session opened with a
credential. A revoked grant is kept as a tombstone: only an invite made
after the revocation brings the identity back.

For a portable session, `WhoAmI` answers `user_id: "portable:<id>"`,
`username` the id's 64 hex digits (what a device keeps the session by),
the name the grant has or the fingerprint as `display_name`,
`role: consumer`, no PIN, never locked, and an empty `portable`. It
browses, plays, and keeps watch state and track preferences of its own.
What only an account here has (password, PIN, passkeys, devices,
sessions, ties, approving sign-ins) is `NotForPortableUsers`;
`GetPlexLink` is `null`.

The admin API lists grants with `admin.grantList` (`{ id, fingerprint,
displayName, grantedAt, inviteId, liveSessions }`) and revokes one with
`admin.grantRevoke` (recent admin; `NO_SUCH_GRANT` for none). Grants are
not users: `userList` never shows them.

### Ties

A local account and a portable identity are one person once the identity
proves it to the server for that account. The tie is a link beside the
account: sessions, passkeys and watch state stay keyed on the local user,
and severing the tie removes only the link. An account ties to any
number of identities (two people sharing an account, or one moving to a
new identity), and one identity to at most one account on a server. A
server never holds the root, so a tie gives continued access, not
recovery.

The identity signs, with its root or with a device key:

```text
"spoolframe tie v1\0"
server      32 bytes   the server's EndpointId
nonce       32 bytes   from tie_challenge
user        32 bytes   the PortableUserId
length      u16        the local username's length in bytes
username    UTF-8      the local username
```

so a proof cannot be used on another server, again on this one, for
another identity, or for another account here. The SDK signs with the id
it dialled, never the one the challenge names.

| Request | Reply |
|---|---|
| `{"type": "tie_begin"}` | `{"type": "tie_challenge", "nonce": bytes(32), "server": bytes(32), "expires_in_secs": 300}` |
| `{"type": "tie_finish", "user": "<id>", "proof": {"type": "root", "signature": "<128 hex>"}}` | `Done` |
| `{"type": "tie_finish", "user": "<id>", "proof": {"type": "device", "credential": {…}, "signature": "<128 hex>"}}` | `Done` |
| `{"type": "untie", "user": "<id>"}` | `Done` |

Each needs a session that signed in within the last 10 minutes
(`RecentSignInRequired`). `TieBegin` is safe to retry; the other two are
mutations, with an operation id, never sent again on a new connection.

- The nonce belongs to the session that began it, for its user, for 5
  minutes. Beginning again replaces it; a user holds at most 4 across
  their sessions (the oldest goes). `TieFinish` uses it up whatever the
  proof: one that is not this session's, is used, or is over is
  `TieChallengeExpired`.
- A `root` proof is the root's signature. A `device` proof is a device
  key's, with a credential for `user` that holds now and whose key no
  identity state the server has seen revokes. Anything else is
  `InvalidCredentials`, logged as a refused sign-in (`tie proof
  refused`).
- A tied account ties another identity as it tied the first; its ties
  stay. An identity tied to this account already is `Done` and changes
  nothing: its first tie date and proof stand. An identity tied to
  another account here gets `PortableIdTaken`.
- `Untie { user }` severs the tie to `user` only. No tie to `user` on
  this account (none, or another account's) is `NotFound`. The account's
  other ties stay, and so do its sessions, those opened with `user`'s
  credential too.

`UserSummary.portable` (in the `WhoAmI` reply) lists the ties, oldest
first, each `{ "id": "<PortableUserId>", "tied_at": <unix secs>,
"last_used_at": <unix secs> | null }`. `last_used_at` is the last sign-in
with the identity's credential that landed in this account, `null` until
one. The list is empty for an account with none, and for a portable
user's own session, whose identity is its `user_id`.

The admin API shows the ties on `UserInfo.portable` (a list of `{ id,
fingerprint, tiedAt, lastUsedAt }`, oldest first) and severs one with
`admin.userUntie(username, id)` (recent admin) and `me.portableUntie(id)`
(recent sign-in); an identity the account is not tied to is
`NO_SUCH_LINK`. Neither ties: the web UI never holds the root.

### Pairing

A device acts as a portable user once that user's root has given it a
device credential (§45, §47). A TV has no camera, so it shows a QR code
and the user's phone, which holds the root, scans it. The credential
comes back over a direct Iroh connection between the two, on
`spoolframe/pair/1`. No Spoolframe server takes part.

Only a device whose owner allows it pairs: the device-local setting
"guest sign-ins" (`Portable::allow_guests`), off by default. Off, the
device shows no code, and its endpoint does not accept
`spoolframe/pair/1`, so a pairing connection fails its handshake: the
device made no key and contacted nothing. The same setting is meant to
gate every external principal, federated users included.

The QR code carries a link:

```text
spoolframe://pair?e=<endpoint id>&c=<code>&n=<name>&a=<ip:port>&r=<relay url>
```

- `e`: the device's `EndpointId`, 64 hex digits. Required.
- `c`: a one-time code, 32 random bytes as 43 base64url characters
  without padding. Required. Good for 5 minutes and one attempt.
- `n`: what the device calls itself, at most 32 characters.
- `a` (at most 8) and `r` (at most 2): where the device may be reached.
  Hints only: Iroh proves the key whatever the path.

Values are percent-encoded as in an invite link. A link longer than 1024
bytes, a second `e` or `c`, or one that cannot be read is refused; an
unknown parameter is ignored. A usual link is under 250 bytes.

The device accepts `spoolframe/pair/1` only while it shows a code. The
phone dials `e` with the addresses and relays in the link, and opens one
bidirectional stream. Each frame is a client-protocol frame (big-endian
`u32` length, then CBOR) of at most 256 KiB, tagged by `type`:

| # | From | Frame | |
|---|---|---|---|
| 1 | phone | `{"type": "hello", "code": bytes(32), "identity": "<PortableUserId>"}` | |
| 2 | device | `{"type": "key", "device_key": "<64 hex>", "name": "<name>"}` | or `refused` |
| 3 | phone | `{"type": "credential", "credential": {…}, "state": {…} or null, "servers": [{…}]}` | or `{"type": "declined"}` |
| 4 | device | `{"type": "done"}` | or `refused` |

`refused` is `{"type": "refused", "reason": r}`, with `r` one of
`not_allowed` (guest sign-ins were turned off after the code was shown),
`wrong_code`, `expired`, `credential_refused` and `failed`.

- At `hello` the device takes the code: compared in constant time, and
  used up whether it matches or not. A wrong code ends the pairing
  (`wrong_code`); so does a code presented after its 5 minutes
  (`expired`). The device then checks the setting again, and makes, or
  reuses, its key for `identity` (one key per identity).
- `key` is what the phone shows: "Authorize <name>?" with the key's
  fingerprint (in the server key format). The phone signs only when the
  person says yes, with the root it holds; without a root it refuses
  before it sends anything.
- `credential` must hold now, name `identity` and the key in `key`.
  `state`, if any, must be `identity`'s, verify, and not revoke that key
  (`credential_refused` otherwise). The device installs the credential,
  then takes the state in, by the rules of "Identity state".
- `servers` are the servers the phone signs in to as `identity` and has
  verified, at most 16, each `{"endpoint_id": "<64 hex>", "server_id":
  "…", "name": "…", "addrs": ["ip:port", …], "relays": ["https://…", …]}`
  (at most 8 addresses and 2 relays). After `done`, the device pins
  each, key and server id, but not as verified: the guest's phone vouches
  for it, not the device's owner. A server it has saved already is left
  exactly as it was, hints and all; one whose server id it has saved
  under another key is left out. It then signs in to them with
  `finish_portable_sign_in`.
- The device waits 2 minutes for frame 3 and 10 seconds for any other. A
  pairing that ends any way other than `done` leaves no credential on
  the device, and removes the key if this pairing made it. A key the
  device had before stays: it is harmless, and reused next time.

Each refusal on the device is one WARN line on `spoolframe::refused`
(`kind=external_sign_in_refused`, `channel=pair`, `request`, `source` as
the phone's short endpoint id, `user`, `reason`), limited per source and
reason to 10 a minute, as the server's are. A refused `start_pairing` is
one such line with `source=local`. No line holds the code.

**What the QR code gives away.** The phone dials only the endpoint id in
the link and Iroh proves it, so a phone never pairs with another device.
The code proves to the device that a phone saw the screen. Someone else
who sees the screen within the 5 minutes, and dials first, can do one of
two things. With a wrong code or none, they end the pairing: the TV shows
a new code (a nuisance, not a breach). With the code, they take the
pairing, and can give the TV a credential for their own identity only:
the phone that authorizes is the one whose root signs, and the
credential names the key the TV made for the identity the phone named.
The real owner's phone then fails with `wrong_code`. The TV ends up
acting as the intruder's portable identity: a guest on it, which is what
turning guest sign-ins on allows. It never gets anyone else's
credential, and nothing the device holds is sent. Turning the setting
off, or forgetting the guest (`Portable::forget_guest`), undoes it.

### Security key roots

An identity may have more roots than the phrase root its id is: a FIDO2
security key's P-256 credential, which signs only assertions under the
relying party `portable.spoolframe.app`, raw CTAP2 or WebAuthn. Any one
root is enough to act. A root on the wire:

```json
{"type": "phrase", "public_key": "<64 hex>"}
{"type": "security_key", "public_key": "<130 hex, SEC1 04‖x‖y>", "credential_id": "<hex, 1–128 bytes>"}
```

A root's `signature` takes one of three forms:

1. 128 hex digits: an Ed25519 signature, from a phrase root.
2. A CTAP2 assertion, `{"authenticator_data": "<hex, ≤512 bytes>",
   "signature": "<hex DER, ≤72 bytes>"}`, over `authenticator_data ‖
   SHA-256(message)`, the message being the same bytes a phrase root
   signs. It holds when `authenticator_data` starts with SHA-256 of the
   RP, its flags have UP (0x01) and UV (0x04) set (other bits are
   ignored), and the P-256 signature verifies. The counter is not
   checked.
3. A WebAuthn assertion, for a key reached through a platform's WebAuthn
   API (Apple's `ASAuthorizationSecurityKeyPublicKeyCredentialProvider`),
   which cannot be given a client data hash: the platform writes a
   `clientDataJSON` around the app's challenge, and the key signs
   `authenticatorData ‖ SHA-256(clientDataJSON)`.

```json
{"authenticator_data": "<hex, ≤512 bytes>", "client_data_json": "<hex, ≤4096 bytes>", "signature": "<hex DER, ≤72 bytes>"}
```

In the WebAuthn form, `client_data_json` is the exact bytes the platform
returned. **The challenge is SHA-256(message)**, the bytes the CTAP2 form
passes as its client data hash, so every message and its domain string
is the same in both forms. It holds, checked in this order, when:

1. no field is over its bound (`client_data_json` over 4 KiB is refused
   before it is parsed);
2. `client_data_json` is a strict JSON object with string `type` and
   `challenge`, each member once, and `type` is `"webauthn.get"` (a
   registration's `"webauthn.create"` is refused: a packed
   self-attestation is signed the same way);
3. `challenge` is SHA-256(message) as base64url without padding, in that
   one spelling;
4. `crossOrigin`, if present, is `false`;
5. `authenticator_data` starts with SHA-256 of the RP, with UP and UV
   set;
6. the P-256 signature verifies over `authenticator_data ‖
   SHA-256(client_data_json)`.

`origin` is not checked. A native app's platform writes its own (it need
not be `https://portable.spoolframe.app`); what binds the signature to
Spoolframe is the RP ID hash, which the platform sets from the RP ID the
app is entitled to, and which the verifier checks. The counter is not
checked.

A credential is bound only to its RP ID hash, so both assertion forms
verify against the same `security_key` root: a key registered on Linux
signs on an iPhone, and the reverse. The two object forms refuse unknown
members, so neither reads as the other.

A registration is not on the wire: a root proves itself by signing the
genesis or its addition. The SDK reads a new root from what the key
answered: over CTAP2, the public key and credential id as given (the
client data hash is the handle; the attestation is not checked); over
WebAuthn, an `attestationObject` (CBOR, ≤8 KiB) and a `clientDataJSON` of
type `"webauthn.create"` whose challenge is the handle, with the RP ID
hash, UP, UV and AT set, a 1–128 byte credential id and an ES256 P-256
COSE key. The attestation statement is not checked, whatever its `fmt`.
The SDK asks for a resident credential with `user.id` = the handle, but
a registration does not show that, so nothing checks it.

**Genesis.** `{"handle": "<64 hex>", "roots": [...], "signatures":
[...]}`: 1 to 8 roots, each signing `"spoolframe identity genesis v1\0" ‖
handle ‖ count (u8) ‖ each root's bytes` (a phrase root `01 ‖ key`, a
security key `02 ‖ public key ‖ id length (u8) ‖ id`). One phrase root
alone keeps its public key as the id; any other genesis's id is
`blake3::derive_key("spoolframe portable user id v1", that message)`. The
id is fixed at genesis. A credential or `root` tie proof carries
`genesis` where the server may not have it; a server keeps it once the
identity is known there.

**Who signed.** A credential's `root`, a state's `signer`, a tie proof's
`root`: absent, the phrase root the id is. Present, the server checks it
is one of the identity's roots now.

**Root changes.** A state lists every root added so far, each
self-certifying: `roots_added: [{"root", "added_by", "added_at",
"endorsement", "possession"}]`, `endorsement` being the adder's signature
over `"spoolframe root endorse v1\0" ‖ user ‖ root ‖ added_at (i64)` and
`possession` the added root's over `"spoolframe root add v1\0" ‖ user ‖
root`; and every one removed, `roots_removed: [{"root", "removed_at"}]`.
A state with a `signer` or any root changes signs `"spoolframe identity
state v3\0"`, the fields of the v2 layout, the signer, then the added
roots sorted (root, adder, added_at) and the removed ones sorted (root,
removed_at).

A verifier works the roots out from the genesis and every such state it
holds: a state counts if a root the identity ever had signed it; a root
counts if a member added it no later than that member's own removal; a
removed root never comes back. Times are the roots' own word, so a
server (and the SDK) refuses a state that brings an addition by a root
it already holds as removed: a stolen key cannot backdate its way past a
removal that reached the verifier first. Two roots that remove each
other are both gone: the identity is locked, not taken.

**Rival states.** Two holders of the identity's roots (a phone with the
phrase and a security key, or two devices a phrase root is synced to)
can each sign a state from one revision. Verifiers never refuse a rival:
its revocations and removals count, as above. A device keeps up to two
**heads**: the newest (highest revision, then the lower BLAKE3 of the
signed message and the signature's CBOR), and a rival the newest does
not cover. A state is covered when the other heads revoke each key it
revokes that has not lapsed, add each root it adds, and remove each root
it removes no later. A sign-in carries every head (`states`, at most 2).

The next state a holder signs is over the merge of all heads, at one
past the highest revision: every revocation not lapsed, at its earliest;
every root added; every root removed, at its earliest. All three only
grow, so merging needs no choice and loses nothing. The SDK merges as
soon as a rival arrives where the phrase root is (and with a security
key's next root action, or `reconcile`). One thing is left out of a
merge: a root added, by its adder's word, after that adder's own
removal. It is no member anyway, and a verifier that holds the removal
refuses the state that brings it. A state refused for that keeps its
revocations, in the SDK and on a server. A merged state is an ordinary
state: a server takes it in as the newest, its revocations into the
union, and keeps it as a root-changing state.

A server keeps every root-changing state it takes in, up to the newest
32; an older one is dropped only when the kept ones carry its root
changes (each root it adds added, each root it removes removed no
later), so a rival branch's removal is never forgotten. Of two states at
one revision, the one the server hands on as the newest is the one with
the lower BLAKE3 digest, whichever came first, so every server hands on
the same one.

**`identity_record { user }`** answers `{"user", "genesis", "state"}`,
the genesis and newest state kept for an identity this server knows
(tied or granted), for a device recovering it with one of its roots. Any
other is `not_found`. On the strangers' budget.

**`identity_record_by_handle { handle }`** (64 hex digits) answers the
same record, for the identity whose genesis has `handle`: the user
handle a security key gives back for its credential, which is all a
fresh install with only the key knows (the id is a hash of the whole
genesis). A key added to a phrase identity carries the identity's id as
its handle, so a handle that is the id of a phrase identity known here
answers that identity. Only an identity known here answers; any other
handle is `not_found`. Handles are 32 random bytes, so a miss says
nothing. On the strangers' budget.

**Pairing through a server.** A device with no one to scan its QR code
shows a 10-character code (Crockford base32, `ABCDE-FGHJK`) and holds its
request on the server for 5 minutes:

1. device: `offer_pairing {code, device_key, name}` → `done`. A code
   another device holds is `invalid_input`.
2. root terminal: `fetch_pairing {code}` → `pairing_offered {code,
   device_key, name}`. At most 5 fetches per request.
3. root terminal: `answer_pairing {code, answer: {credential, state,
   servers}}` → `done`: once, and only for the offered key
   (`invalid_input` otherwise).
4. device, every few seconds: `collect_pairing {code}` →
   `pairing_answered {answer}`: `null` until the answer comes, then the
   answer, once. Only the offering device may collect; this is not on
   the strangers' budget.

The server checks nothing else: the device checks that the credential is
for its own key, and shows whose identity it is before keeping it.

### SDK

`Session::tie(identity)` begins, signs with the root if this device holds
it (`Portable::tie_proof`), else with its device credential, and
finishes; the root is never sent. `Session::untie(identity)` severs that
tie; `Session::tie_info()` reads the ties from `WhoAmI`, a
`Vec<PortableTie>`. The errors are `SdkError::TieChallengeExpired` and
`PortableIdTaken`.

Pairing, on the device: `Portable::allow_guests()` /
`set_allow_guests(bool)`; `Client::start_pairing(name) -> Pairing`,
whose `link()` is the QR text. `connected()` gives `Connected { identity,
device_key, fingerprint }` once a phone's `hello` is taken and the key
sent, and `wait()` ends in `Paired { identity, device_key, fingerprint }`
or `SdkError::Pairing(PairingError)`; `cancel()`, or dropping it, stops
at once. The TV's flow:

1. Show `link()` as a QR code.
2. `connected()`: the phone has scanned it. Show "Check your phone shows
   <fingerprint>". It is `DevicePublicKey::fingerprint()`, the same
   string the phone shows as `PairingOffer::fingerprint()`.
3. `wait()`: the person on the phone authorized or declined.

`connected()` and `wait()` both take `&self`, may run side by side, and
give the same answer on each call. If the pairing ends before a phone
connects, `connected()` gives the error `wait()` gives (`Expired`,
`WrongCode`, `NotAllowed`, `Aborted` when cancelled). Once a phone has
connected, `connected()` gives it however the pairing ends.
`Portable::guests()` lists the identities held without their root;
`Portable::forget_guest(id)` signs one out.

On the phone: `Client::scan_pairing(link, identity) -> PairingOffer`,
with `device_name()`, `fingerprint()`, `authorize(label)` and
`decline()`. The device reaches no relay of its own, so its link has LAN
addresses only: the phone has to be on the same network.

### Test vectors

The test vectors in `spoolframe_protocol::portable` (`portable/mod.rs`; root seed `[7; 32]`, device seeds
`[9; 32]` and `[11; 32]`) were computed apart from the code, with Python
and `openssl pkeyutl -rawin`; the tie vector signs for username `alice`
on server `[0x5a; 32]` over nonce `[0xa5; 32]`.
