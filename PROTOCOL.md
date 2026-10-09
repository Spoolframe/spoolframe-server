# Development wire revision 11

Revision 11 (PX-21, adaptive HLS) keeps the `spoolframe/client/1` ALPN
and sets `PROTOCOL_VERSION` to 11. Update the server and SDK together.
A converted plan has a new required field, `ladder`: the encodes an HLS
player switches between, highest first, each `{ "height", "max_bitrate"
}`; empty for one encode, which every plan but `auto` quality over HLS
is. The multivariant playlist of a ladder lists one variant per rung at
`v{n}/media.m3u8`, `n` the rung's place in `ladder`, the rung the link
chose first; under `v{n}/` are that rung's `init.mp4` and `seg{k}.m4s`,
on the same grid as every other rung. `MediaRead` and `GetManifest` have
a new required field, `variant`: that `n` (0 for one encode, and for
anything that is not a segment or a media playlist). One encode runs at
a time: a read of another rung's segment moves it there, and the rung
it left keeps what it wrote, readable.

Revision 10 (P6-06, the quality choice) kept the `spoolframe/client/1`
ALPN and sets `PROTOCOL_VERSION` to 10. Update the server and SDK
together. `StartPlayback`'s `link_bitrate` becomes `quality`, an object
tagged by `type`: `{ "type": "auto", "link_bitrate": n | null }` (for the
link, as revision 9 planned it), `{ "type": "bitrate", "max_bitrate":
n }` (at most that, whatever the link) or `{ "type": "original" }` (the
file as it is, whatever the link). The capabilities' `max_bitrate`
stays a ceiling over all three; an unknown type is `auto` with nothing
measured. `LimitPlayback` becomes `ChangeQuality { handle, quality,
bitmap, position_ms }`, the same control for any quality: the viewer's
new choice, or `auto` with a link found slow. The playback keeps its
quality for a later `ChangeSubtitle`.

Revision 9 (P6-06, media over the Internet) kept the
`spoolframe/client/1` ALPN and sets `PROTOCOL_VERSION` to 9. Update the
server and SDK together: a revision-8 client gets `Unsupported` from a
revision-9 server's `Hello`. `StartPlayback` has a new required field,
`link_bitrate` (bits per second, or `null`): what the device measured
its link to the server carrying lately. The playback is planned for 75%
of it as well as for the capabilities' `max_bitrate`, and keeps it for
later re-plans. There is a new request, `LimitPlayback { handle,
link_bitrate, bitmap, position_ms }`, answered `PlaybackChanged`: a
playback that stalls on its link is planned again for it, as
`ChangeSubtitle` plans one again for a subtitle. It is a playback
control, so it carries the expected revision, as `ChangeSubtitle` does.
A plan that changes restarts at `position_ms` as a new epoch; one that
does not is answered as it is, nothing restarted. A plan nothing plays
under is `CannotPlay`, and the playback goes on as it was.

Revision 8 (P6-01, invites) kept the `spoolframe/client/1` ALPN and sets
`PROTOCOL_VERSION` to 8. Update the server and SDK together: a
revision-7 client gets `Unsupported` from a revision-8 server's `Hello`.
There is a new pre-auth request, `redeem_invite { code, username,
display_name, password }`, answered `logged_in`, and four new errors:
`invite_invalid`, `invite_expired`, `invite_exhausted` and
`username_taken`. The invite link that carries the code is not a
message; its format is in "Invites" below, with what the server and the
SDK do with both.

Revision 7 (PX-12, pushed addresses) keeps the `spoolframe/client/1` ALPN
and sets `PROTOCOL_VERSION` to 7. Update the server and SDK together: a
revision-6 client gets `Unsupported` from a revision-7 server's `Hello`.
The events stream has a new event, `server_addresses { addrs, relays }`.
The server pushes it to a connection as soon as the connection is signed
in, and again when the list changes. Each event replaces the list told
before. The `server_addresses` request and its `ServerAddresses` reply
are removed: the event does their work, and a server answers the
request `Unsupported`. What the event holds, and what the SDK does with
it, is in "Server addresses" below.

Revision 6 (PX-09 finding 27) keeps the `spoolframe/client/1` ALPN and
sets `PROTOCOL_VERSION` to 6. Update the server and SDK together: a
revision-5 client gets `Unsupported` from a revision-6 server's `Hello`.
`DeviceSignInRequest`, the reply to `LookupDeviceSignIn`, has a new
required field, `requested_from`, so a revision-6 client cannot decode a
revision-5 server's. What it holds, and what an approving app shows, is
in "Where a device asked from" below.

Revision 5 (PX-14, the server id; PX-13, picture subtitles) set
`PROTOCOL_VERSION` to 5. A mismatched build is refused at `Hello`, not
later on one request:

- A revision-4 client gets `Unsupported` from a revision-5 server's
  `Hello`.
- `ServerInfo` has a new required field, `server_id` (PX-14), so a
  revision-5 client cannot decode a revision-4 server's `Hello` reply.
- `GetTrackPreferences` answers `Reply::TrackPreferences` with a
  `TrackPreferencesInfo { preferences, ocr_offered }`, not the bare
  `TrackPreferences` (PX-13). PX-13 made this change without a bump, so
  a mismatched build failed only that request; revision 5 covers it.
  PX-13's new `subtitles.bitmap` preference and the new meaning of
  `bitmap: "auto"` change no encoding.

What changed for the server id is in "The server id" below; what
changed for picture subtitles is in "Picture subtitles". PX-15 made every
field of every message required, in revision 5: see "Every field is
required".

Revision 4 (P4-01, trusted devices) set `PROTOCOL_VERSION` to 4.
Revision 3 and older binaries cannot use this wire format. What changed
is in "Trusted devices" below.

Release 0.3.0 set `PROTOCOL_VERSION` to 3. Revision-1 and revision-2
binaries cannot use that wire format either. Clients must explicitly select foreground profiles and report progress by
playback handle; see the migration examples below. Clients
that depend directly on the protocol/types crates must update those
packages together with the SDK.

## Every field is required

Since PX-15, a message must carry every field its type has. A field
with no value is sent as `null` (CBOR `null`), never left out. A message
that leaves out a field is refused: `Malformed` from the server, a
decode error in the SDK. Before, about 20 fields had defaults, so a peer
that left them out was read as if it sent `null`, `0`, `false` or `[]`.
Among them were `Hello.device_name` and `capabilities`, each field of
`ClientCapabilities` and `VideoLimits`, `RequestEnvelope.session`,
`Selection`, `MediaRead.epoch` and `segment`, `MediaReadReply.complete`
and `epoch`, the `markers` and `chosen` of replies, and
`TrackPreferences` and its sections.

`PROTOCOL_VERSION` stays 5. The server, the SDK and the Linux app send
every field already, so their bytes do not change. Only a peer that
builds messages by hand, and leaves a field out, must change. Unknown
fields are still ignored, and unknown enum values still decode as
`Unknown` (or the value the type names), so a newer peer can still add
both.

The admin API (JSON-RPC) is the same: every field is required, `null`
for none, except an error object's `data`, which JSON-RPC 2.0 lets an
error leave out.

## The server id

`ServerInfo` (the reply to `Hello`) has `server_id: String`. The key is
always there. It is the server's id: 80 random bits, written as
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

A `server.id` that is not a server id stops the server. It is never
made again while the file is there.

### SDK: the pin is the key and the server id

The SDK checks that `server_id` is a server id, or the connect is a
`Protocol` error. It pins the server id with the key, at the first
connect. A server saved by an older SDK takes the id at its next
connect. `ServerEntry` has a new field, `server_id: Option<String>`.

- Same key and same id: the connect succeeds. The name is saved, as
  before, whatever it is.
- Same id, another key: `SdkError::IdentityChanged { saved, now }`, from
  `connect` and from `pin`. `Client::trust_new_identity(saved, now)`
  pins the new key with the same id. The server's app passkeys keep
  working, because they belong to the id, and the SDK makes their
  challenges with the new key from then on.
- Same key, another id: `SdkError::ServerIdChanged { endpoint_id, saved,
  now }`, from every request to that server. Nothing is saved, and the
  pin does not change. A server never changes its id, so this is not
  accepted in place. `Client::forget` it and add it again if the user
  says it is theirs.
- Another key and another id: another server. No pin applies, whatever
  its name.

A pin with no id yet is still matched by name: another key heard on the
LAN under its name is an `IdentityChanged`. A pin with an id is not.

## RPC framing

A bidirectional RPC stream starts with byte `1`. The request is one
length-prefixed CBOR `RpcRequest`, capped at 1 MiB. It wraps the session
and request, an optional operation descriptor, and an optional expected
playback revision.

The reply is a CBOR `RpcResponse`. The stream carries its total byte length
as a big-endian `u64`, followed by chunks, each with a big-endian `u32`
length. Each chunk is at most 1 MiB; the complete encoded reply is at most
64 MiB. Empty chunks, oversized lengths, and truncated replies fail.
Serialization stops at the aggregate budget before anything is sent.
The SDK assembles the reply and returns its existing public result type.
This removes the old single-frame limit for collections. It does not add
catalog cursors or bound the backend's work to produce the collection.

Each reply identifies the server process and reports its clock. Both peers
require the current revision in the Hello exchange.

## Connection limits

A connection from an endpoint with no live session is a *stranger*. Its
limits come from the handshake: no unidirectional streams (the server
never accepts one; events go server → client), 4 concurrent
bidirectional streams, 512 KiB of unread data across them, and request
frames of at most 64 KiB. A connection from a device with a live session
gets 100 streams and 16 MiB, at accept or as soon as a sign-in on it
succeeds; frames may then be 1 MiB.

Every stream must deliver its kind byte and first frame within 10 s
(`network.first_frame_timeout_secs`), or it is reset with
`reset::FIRST_FRAME_TIMEOUT` (2); the connection stays.

The server holds at most 64 stranger connections
(`network.max_stranger_connections`). One more is closed with
`close::BUSY` (2) and may try again later; a signed-in device is always
let in. When a device's last live session ends, the server delivers the
replies it is sending (for at most 2 s) and closes its connections with
`close::SIGNED_OUT` (3). The SDK ends every session it keeps for the
server, answers a call in one with `SignInRequired`, and dials again
only when a call needs it, as a stranger. It reports `close::BUSY` as
`ServerFull`. Connections
idle for 15 s are dropped; iroh sends a keep-alive every 5 s.

## Mutation replay

Mutations require an operation ID, server process ID, and expiry expressed
in the server's clock, which advances monotonically within the process.
The SDK generates these once per method invocation
and reuses the exact request on its transport retry. Reads remain retryable
without an operation ID.

The server keys outcomes by authenticated transport endpoint and operation
ID. It fingerprints the complete request, including session and expected
revision. Concurrent duplicates wait for the same execution. Disconnecting
or cancelling the response stream does not cancel an admitted mutation.
Reusing an ID with another request returns `OperationConflict`.

An operation can live for at most 15 minutes. The cache has a 4096-entry
and 64 MiB serialized-outcome budget. A pending operation reserves what its
kind of reply can take: 1 MiB for `StartPlayback`, `ChangeSubtitle` and
`SeekPlayback`, 64 KiB for passkey ceremony options, 16 KiB for the rest;
a finished one counts its outcome's size. One endpoint, and one user, may
hold at most an eighth of the entries and bytes, and a user already
holding some may not take the last eighth of the pool, which is kept for
users holding none. The anonymous sign-in
requests have pools of their own, 512 entries and 8 MiB each with the same
shares: one for `LoginPassword`, `BeginPasskeySignIn` and
`FinishPasskeySignIn`, and one for `BeginDeviceSignIn`,
`AwaitDeviceSignIn` and `ResumeProfile`. Their outcomes are replayed for
30 s after they are known, or until the operation expires if that is
sooner; an outcome that changed nothing (an error, or
`DeviceSignInPending`) is not kept, and a retry runs again.
It refuses new operations with `RateLimited` when full; it does not evict
unexpired IDs to make room. Individual mutation outcomes are capped at 1 MiB. Expired IDs
return `OperationExpired`; IDs from an earlier server process return
`ServerRestarted`. Neither error re-executes the operation. This is a
bounded replay guarantee, not a durable transaction log. After a crash,
the client may need to inspect state to learn whether an operation happened.
Calling an SDK method again creates a new invocation, not a replay.

Profile unlocks remain connection-local. Replaying an earlier successful
PIN operation cannot unlock a new connection or undo a later profile
switch; a locked connection gets `ProfileLocked` and must unlock again.

## Playback revisions

A playback starts at revision 0. Successful subtitle changes and explicit
seeks increment that revision. Change, seek, and stop requests must name
the expected revision. Validation and mutation share the per-handle control
lock; a stale command returns `RevisionConflict` without applying.

The SDK keeps revisions internally and serializes controls for the same
server, session, and handle across cloned sessions. It can query the
revision for a handle restored by the app. It does not silently reapply a
conflicting control. Idle revision cache entries can be discarded and
fetched again.

The playback revision is separate from the encoder epoch. Automatic HLS
segment regeneration can change the encoder epoch without changing the
playback configuration. Existing SDK epoch fields and read methods retain
their meaning.

### SDK: a playback the server ended

The server ends a playback that idles out, at a restart, and when it lets a
device's playbacks go; a later use of its handle is `PlaybackExpired`. A new
`StartPlayback` of the same item, selection and `bitmap`, with `start_ms` at
the viewer's position, goes on: HLS segments are numbered on the item's own
time, so the media playlist of the same plan is the same, and segment `n` of
the new playback is segment `n` of the old. A progressive output, and the bytes
of a segment, are the new encode's, and do not continue the old one's.

`ManagedPlayback` in the SDK does this itself, at most 3 times in 5 minutes.
It does not start again after `Unauthenticated`, or when the start is
`NotFound` (the item is no longer served to the user), `CannotPlay` or
`PlaybackStopped`. A client that holds raw handles does the same.

## Resource streams

Byte `2` identifies a resource stream. One CBOR `ResourceRequest` is followed
by one CBOR `ResourceResponse` and exactly the announced raw body length.
Media, artwork, whole-file subtitle chunks, and subtitle segments use this
format. Error replies have no body. Each transfer uses its own QUIC stream.

Media and subtitle requests include the playback revision. The server
checks it before serving and rejects reads invalidated during preparation.
Media still has its encoder epoch and byte range; subtitle files retain
their file-version check between chunks. The SDK checks resource kind,
revision, body length, and allocation limits before returning bytes.

Media and subtitle segments are capped at 16 MiB; subtitle file chunks at
512 KiB. Artwork requests state a maximum length; the SDK uses 64 MiB.
The server bounds file copies by the announced length, even if a file grows
while it is sent.

## Confirming it is you

Some requests need a session that proved its user within the last 10
minutes: `ApproveDeviceSignIn`, `BeginPasskeyRegistration`,
`RemovePasskey`, `SetPin` with no `proof` (a user's first PIN),
`SetPinRequired` with `required: false`, `SetDeviceTrust` except to
stop trusting this device, and `ForgetDevice`. An older
session gets `RecentSignInRequired`. The app then asks its user to
confirm, and sends the request again. It does not sign out.

| Request | Reply |
|---|---|
| `{"type": "confirm_password", "password": "…"}` | `Confirmed` |
| `{"type": "begin_confirm_passkey"}` | `PasskeySignInBegun` |
| `{"type": "finish_confirm_passkey", …FinishPasskeySignIn fields}` | `Confirmed` |

Each request needs a session. Each is a mutation, so it carries an
operation ID, and the SDK never retries it on a new connection. A
`Confirmed` reply makes this session count as signed in now. The user's
other sessions do not change. This is the same store change as the web
UI's `me.confirmPassword` and `me.confirmPasskey`.

The password is checked as `LoginPassword` checks it. A wrong password is
a failed sign-in for the user's username from this endpoint. These are
the keys that a login and the web UI's confirmation count. After five
failures in 15 minutes the reply is `RateLimited`, also for the right
password and for a login. A user with no password gets `NoPassword`, and
nothing is counted.

`BeginConfirmPasskey` lists only the session user's app passkeys made
for this server's relying party (see "App passkeys: a relying party per
server"). It sends a nonce, and the app signs the
challenge made from it (see "App passkey challenges"). The challenge
belongs to this session on this endpoint, for 5 minutes, and is used once. A sign-in challenge or the
challenge of another session does not finish it. A passkey of another user
is `InvalidCredentials`. A failure counts as a failed passkey sign-in of
the passkey's user from this endpoint. A user with no app passkey gets
`NotFound`. When the user's passkey sign-ins are rate-limited, `Begin`
gets `RateLimited` before a challenge is made.

## App passkeys: a relying party per server

Each server has its own app relying party (PX-14):

| | Value |
|---|---|
| RP ID | `<server id>.spoolframe.app` (`ServerId::rp_id`) |
| Origin | `https://<server id>.spoolframe.app` (`ServerId::origin`) |

Before PX-14 every server shared the RP ID `spoolframe.app`, at the origin
`https://spoolframe.app`. A security key with passkeys for two servers,
or for two accounts, then answered a sign-in that listed no passkeys with
all of them. The platform took the first, and another server refused it.
With an RP per server, the key offers only that server's passkeys.

The server takes only its own RP ID hash and its own origin, for
registration, sign-in and confirmation. `https://spoolframe.app` is not
its origin: WebAuthn wants the origin's host at or under the RP ID, and
`spoolframe.app` is above `<server id>.spoolframe.app`.

`PasskeyCreation` and `PasskeyRequest`, which the SDK gives the app, have
two new fields, `rp_id` and `origin`. The SDK makes them from the server
id that it pinned with the key, not from a fixed value. The app gives
`rp_id` to the platform's passkey API, and `origin` where the platform
takes one (webauthn-authenticator-rs on Linux). `APP_RP_ID` and
`APP_ORIGIN` are gone. `PasskeyRegistrationBegun.user_name` is now
`<username> @ <server name>`, because the RP ID says nothing a person can
read. `display_name` is the user's display name, or the username.

### Passkeys made for `spoolframe.app`

The server keeps them. They sign no one in: a sign-in or confirmation
with one is `InvalidCredentials`, and the log says the passkey "was made
for another host". `ListPasskeys` lists each one with `app: true` and
`usable_here: false`. The web UI's `passkeys` lists it with `app: true`
and `usable: false`, and says it is an old app passkey to add again. The
user removes it as any other passkey. The server logs each one at
start, as it logs web passkeys of another host. They still count as a
way to sign in for the last-method rule, as web passkeys of another host
do. `LEGACY_APP_RP_ID` names their RP ID, and `is_app_rp_id` tells an app
passkey's RP ID from a web one.

The web UI's own relying party, the host of `admin.public_url`, does not
change.

## App passkey challenges

The RP ID comes from the server id that a server names in `ServerInfo`,
and a server can name another server's id. The platform writes only the
origin and the challenge into `clientDataJSON`. So the challenge names
the server by its key: otherwise a server could claim another server's
id, begin a ceremony there, give that challenge to an app as its own,
and finish there with what the app signed (PX-09, finding 2).

`BeginPasskeySignIn` and `BeginConfirmPasskey` reply `PasskeySignInBegun`
(`nonce`, `allow_credential_ids`). `BeginPasskeyRegistration` replies
`PasskeyRegistrationBegun` (`nonce`, `user_id`, `user_name`,
`display_name`, `exclude_credential_ids`). Each `nonce` is 32 random bytes.
There is no `challenge` field. The challenge is:

```text
SHA-256("spoolframe/app-passkey-challenge/1" || server endpoint id (32 bytes) || nonce)
```

`spoolframe_protocol::app_passkey_challenge` computes it. The server uses
its own endpoint id. The client uses the endpoint id that it pinned and
dialled, never an id that the server sends. The SDK gives the app a
`PasskeyRequest` or a `PasskeyCreation` with this challenge in it. A
signature for another server's challenge is `InvalidCredentials`.

`BeginPasskeySignIn` lists no credential ids, with or without `username`:
anyone can send it. App passkeys are discoverable, so the platform finds
them. With `username`, only that user's passkeys finish the sign-in.

### The user handle

`FinishPasskeySignIn` (and `FinishConfirmPasskey`, which carries the same
fields) has `user_handle: Option<bytes>`. The key is always there:

| Value | Meaning |
|---|---|
| CBOR byte string, 1 or more bytes | The handle the authenticator keeps with the passkey. It must be the passkey's user's, or the reply is `InvalidCredentials`. |
| CBOR null | The platform gave no handle (webauthn-authenticator-rs, for a phone over caBLE). The passkey's credential id says whose it is. |
| Empty byte string | `InvalidInput`. It uses no challenge and does not count as a failed sign-in. |
| Key not there | The message does not decode. |

The SDK sends an app's empty handle as null. This is still revision 4,
and `PROTOCOL_VERSION` stays 4. The bytes of a handle are encoded as
before. An older client's empty handle decodes as an empty byte string,
so it gets `InvalidInput`. An older server cannot decode null. Both
servers refused an empty handle before this change, so no sign-in that
worked stops working.

A PIN is not accepted as proof. A PIN guards switching profiles on a
shared device, and it is weaker than a sign-in. A locked session gets
`ProfileLocked` for all three requests, as for other sensitive requests:
unlock with the PIN first, then confirm. A confirmation does not unlock a
profile. The lock belongs to the connection, and the confirmation belongs
to the session.

## Where a device asked from

A device-sign-in approval shows the device's own name, which an attacker
chooses: one can begin a sign-in, send someone the approval link or
code, and get a session when they approve it (device-code phishing,
PX-09 finding 27). So the approval also shows where the device's
`BeginDeviceSignIn` came from, which the attacker cannot choose.

`DeviceSignInRequest.requested_from` is `null` if the server could not
tell, or a `DeviceAddress`:

| Field | Value |
|---|---|
| `address` | The IP address as text, `"192.168.86.20"`; an IPv4 address never in its IPv6-mapped form. |
| `network` | `"loopback"` (the server's own machine), `"local"` (private IPv4, link-local, CGNAT and VPNs' `100.64.0.0/10`, IPv6 unique-local, or any address in the network of one of the server's own interface addresses, by that interface's prefix length) or `"internet"` (any other: a public address). An unknown value decodes as `Unknown`. |

The server judges `network` as the web approval page judges
`fromInternet`. It reads its interfaces at each lookup. A home with
IPv6 gives its devices public addresses in the server's own /64, so
those are `local`; an interface with no netmask counts as a /64 for
IPv6 and as its one address for IPv4. Only the server's own networks
count, never a wider ISP block: another /64 of the same /56 is
`internet`. An app's approval screen shows the address, "The TV
asked from 192.168.86.20.", and for `internet` or `Unknown` warns
instead: the device is not on this network, so approve only a device
you can see; a code someone sent you is theirs. The SDK re-exports
`DeviceAddress` and `AddressNetwork`; `Session::lookup_device_sign_in`
returns the field with the rest.

## Trusted devices

A user can make a device trusted (P4-01). A trusted device starts a new
session for that user without a full sign-in. It is a row in the
server's `trusted_endpoints` table, for one user and one Iroh endpoint.
The device's key is the proof. A PIN unlock never makes trust or renews
it.

### Making trust

| Request | Change in revision 4 |
|---|---|
| `LoginPassword` | New `remember: bool` field. |
| `FinishPasskeySignIn` | Now `{signed: FinishPasskeySignIn, remember: bool}`. |
| `BeginDeviceSignIn` | Now `{remember: bool}`: the device asks to be trusted. |
| `LookupDeviceSignIn` | `DeviceSignInRequest` has a new `remember: bool`: the device asked. |
| `ApproveDeviceSignIn` | New `remember: bool`: the approver's answer. |
| `SetDeviceTrust` | New: `{device: Option<String>, trusted, auto_login}`. |

Each `remember` and `trusted` field is required. Clients send `false`
unless the user ticked "Remember this device". Nothing ticks it by
default.

After a device sign-in, the device is trusted only if it asked and the
approver's `remember` is `true`. An approver cannot trust a device that
did not ask.

`Reply::LoggedIn` has a new `remembered: bool`. It is `true` when the
device is trusted for the session's user after the sign-in. The device
learns from it whether the approver kept "Remember". The web UI's own
sign-in never makes trust, because a browser is not an Iroh endpoint.

`SetDeviceTrust` needs a session. `device` is an endpoint id from
`Devices`. Leave it out for this device. These rules apply:

- To turn trust or auto-login on, the device must be this device. For
  another device, the reply is `InvalidInput`. Only the web UI trusts
  another device.
- Turning trust or auto-login on needs a recent sign-in.
- Auto-login needs trust, or the reply is `InvalidInput`.
- To turn trust off, the device can be any of the user's devices. This
  ends the user's sessions on that device, including the caller's session
  if it is this device. For another device, it needs a recent sign-in.
- A device that is not the user's gets `NotFound`.

### Resuming

These two requests are pre-auth, beside `Hello` and `LoginPassword`:

| Request | Reply |
|---|---|
| `{"type": "trusted_profiles"}` | `TrustedProfiles { profiles }` |
| `{"type": "resume_profile", "user_id": "local:…"}` | `LoggedIn { session, remembered: true }` |

`TrustedProfiles` lists only the connecting endpoint's rows. Each one is
`{user_id, username, display_name, pin_required, auto_login}`. It is a
read, and the SDK may retry it.

`ResumeProfile` is a mutation, so it carries an operation ID. It opens a
normal session, bound to this endpoint as a sign-in's is. The reply is
`Unauthenticated`, the same for every reason: the device is not trusted
for the user, the trust lapsed or was removed, or the user is disabled.
If the device requires the user's PIN, the new session starts locked.
A resumed session is not a recent sign-in, because nobody proved who
they are. Until the user confirms (`ConfirmPassword`,
`BeginConfirmPasskey`), each request in "Confirming it is you" gets
`RecentSignInRequired`.

The server never opens a session by itself. For auto-login, the client
calls `TrustedProfiles` at start and resumes the profile that has
`auto_login`.

### Auto-login and the PIN

At most one user per device has `auto_login`. Turning it on for one user
turns it off for the others on that device. It also turns off the PIN
requirement for that user on that device. Turning the PIN requirement on
(`SetPinRequired`) turns auto-login off.

### What ends trust

Trust never lasts longer than what it would bypass:

- `Logout` on the device removes the device's trust for that user.
- When the user or an admin ends a session (`me.revokeSession`,
  `admin.userRevokeSession`), that device's trust goes too.
- `me.revokeOtherSessions` removes the trust of every other device.
- Ten wrong PINs in a row revoke the device's sessions and its trust.
- Each of these removes all of the user's trust: a password change, an
  admin's password reset, and disabling the user. Enabling the user
  again does not bring trust back.
- Turning trust off ends the user's sessions on that device.
- Trust lapses after `auth.trust_idle_days` without use (default 180).
  A resume uses the trust, and so does any use of the user's session on
  that device. These writes happen at most once a minute, as for
  sessions. Lapsed trust is refused and removed.

Expiry of a session does not end trust. That is the purpose of trust.

### Deleting a device

`ForgetDevice` (P4-03) is an added request. `PROTOCOL_VERSION` stays 5.

| Request | Reply |
|---|---|
| `{"type": "forget_device", "device": "<endpoint id>"}` | `Done` |

`device` is an endpoint id from `Devices`, and it is required. The
server deletes the user's sessions on that device, live or revoked, and
the device's trust and PIN requirement for the user. The device leaves
`Devices`. If it signs in again, it is a new device. These rules apply:

- It needs a session, and a locked profile gets `ProfileLocked`.
- It is a mutation, so it carries an operation ID and is not retried.
- It always needs a recent sign-in (`RecentSignInRequired`).
- This device gets `InvalidInput`. To leave this device, send `Logout`.
- A device that is not the user's gets `NotFound`.
- Other users' sessions, trust and PIN requirement on that device stay.
  The server closes the device's connection with `SIGNED_OUT` only when
  no user's session is left on it.

The web UI sends `me.forgetDevice(endpointId)`, and an admin sends
`admin.userForgetDevice(username, endpointId)`. Both always need a
recent sign-in (`RECENT_SIGN_IN_REQUIRED`). A device the user never
signed in on gets `NO_SUCH_DEVICE`.

`DeviceInfo` (`ListDevices`) has two new fields, `trusted` and
`auto_login`.

### SDK

- `Client::sign_in(server, username, password, remember)`.
- `Client::finish_passkey_sign_in(server, signed, remember)`.
- `Client::begin_device_sign_in(server, remember)`. The returned
  `DeviceSignIn` has `remember`.
- `Client::trusted_profiles(server)` returns `Vec<TrustedProfile>`.
- `Client::resume_profile(server, user_id)` returns a `Session`. If the
  device is no longer trusted, it returns `SdkError::SignInRequired`.
- `Session::remembered()` returns `Option<bool>`. It is `Some` for a
  session that a sign-in or a resume made in this process.
- `Session::set_device_trust(device, trusted, auto_login)`. When it turns
  this device's trust off, the SDK also forgets this session. The profile
  stays in `saved_profiles`.
- `Session::approve_device_sign_in(code, remember)`.
- `Session::forget_device(device)` deletes another of the user's devices
  (P4-03). For this device it returns `SdkError::InvalidInput`: use
  `Session::sign_out`.

## Server addresses

The server tells a signed-in device where it can be reached (PX-12), on
the events stream, without being asked (revision 7):

```json
{"type": "server_addresses",
 "addrs": ["192.168.1.10:4433", "[2001:db8::1]:4433"],
 "relays": []}
```

`addrs` is the addresses on the server endpoint's interfaces that
`network.announce` lets it tell, with their port: LAN, public and VPN
addresses. Loopback, link-local and unspecified addresses are left out.
After them come the server's public addresses (`network.wan`, P6-02),
the ones a device off the LAN dials: written in the config, made of a
port forwarded on the router and the network's public IP, or the host's
own stable IPv6 addresses. The event does not say which address came
from where; the admin status does.
Each one is text, `192.168.1.10:4433` or `[2001:db8::1]:4433`, in CBOR as
in JSON. `relays` is the URL of each iroh relay the server may use
(`network.relays`, P6-03), its home relay at the time first: the one it
is reached through, the rest where it fails over to. It is empty while
relays are off, as they are by default. Both fields are required, and an empty list is sent as `[]`.

Each event is the whole list. It replaces what the server told before:
an address it no longer tells is gone.

The server pushes it to a connection:

- right after a sign-in on it: `LoginPassword`, an approved device
  sign-in (`AwaitDeviceSignIn`), `FinishPasskeySignIn` or
  `ResumeProfile`. Every other connection of the same device that said
  `Hello` gets it too;
- right after `Hello`, when the device has a live session already (the
  app opened again with a kept session, or a reconnect);
- to every signed-in connection, when the addresses it would tell
  change. The server learns of a change when iroh sees the endpoint's
  addresses change, as when an interface comes up or goes down, and
  when it finds a public address after start: the router maps the
  port, the IP lookup answers, or the host's IPv6 addresses change.

A connection with no live session is never pushed it. A device signed
out is pushed nothing more, and its connections are closed with
`signed_out`, as before. A locked profile is pushed it too: the
addresses say nothing about the user.

The addresses are never in `Hello`, because a stranger must not learn the
server's internal and VPN addresses.

Revisions 6 and older had a request for the addresses,
`{"type": "server_addresses"}`, answered `ServerAddresses { addrs }`.
Revision 7 removes it. A device asked once on each connection, so it did
not learn of a change while it stayed connected.

Iroh announces these addresses inside each connection too, but a client
sees only the paths that opened. A phone that never reached the server's
VPN address cannot learn it from iroh.

### SDK

The SDK applies each `server_addresses` event it reads on a connection's
events stream to the saved server. It does not ask for the addresses any
more. Nothing in the API changes.

- The list replaces the told addresses saved before. A told address that
  is not in the new list is removed at once.
- A reached address stays, whether the list has it or not. It ages out
  as before, after 30 days without being reached.
- A new address is saved as told, with the time it was told. A told
  address becomes reached when a connection reaches it.
- The 32-address cap applies as before: the newest reached addresses
  are kept first, and told ones fill the room that is left.
- The relays are saved with the server (`relays` in the servers file).
  A file written before has none. The SDK does not dial through relays,
  because its endpoint has relays off, so it only keeps them.

An event for a server that is not saved is ignored. Every saved address
is dialled, told or reached.

`ServerEntry` shows the last event as it came, for an app to show the
server's advertised addresses apart from every dial hint in `addrs`:
`advertised_addrs` (in the server's order), `relays` and
`advertised_at` (when the event came, to the second). They are empty and
`None` until a server tells this device.

## Invites

An invite (P6-01) finds a server from anywhere and, with a code, makes an
account there. It is one string, the link, shown as a QR code and pasted
as it is (`spoolframe_protocol::invite`):

```
spoolframe://invite?server=3f9a…a0f1&name=Home%20Media&addr=203.0.113.24:41641&relay=https://relay.home-media.example&exp=1792003200&role=consumer&cap=QW7K-2MFP-XR4T
```

| parameter | |
|---|---|
| `server` | The server's `EndpointId`, 64 hex digits. Required. |
| `name` | What the server calls itself. |
| `addr` | An address it may be reached at, `ip:port` or `[ipv6]:port`. Repeated, one per address. |
| `relay` | An `http(s)` URL of a relay it may be reached through. Repeated. Empty until relays exist (P6-03). |
| `exp` | When the code stops working, in Unix seconds. A hint. |
| `role` | `consumer` or `admin`: the role the account gets. A hint. |
| `cap` | The code, 12 characters from `ABCDEFGHJKMNPQRSTVWXYZ23456789`, shown in groups of four. |

Values are percent-encoded where they would break the link (`&`, `=`,
`#`, `%`, `+`, spaces, anything outside ASCII); `:` and `/` are written as
they are. A reader ignores a parameter it does not know, and a hint it
cannot read, as it does in the device sign-in link. A link without
`server`, with a `server` that is not a key, or with a `cap` that is not a
code is not an invite. At most 32 `addr` and 32 `relay` hints are read.

Without `cap` the link only finds the server: a client pins the key
straight from it (it came from the server's admin, so there is nothing to
trust on first use), learns the server id at `Hello` and pins the pair,
and its hints join the addresses it knows as told ones. The user then
signs in as usual. `exp` and `role` only fill in the join form ("Joins as
a consumer · expires Oct 14"); the server decides both when the code is
redeemed.

With `cap`, the client sends, before any session:

```json
{"type": "redeem_invite", "code": "QW7K-2MFP-XR4T", "username": "mum",
 "display_name": "Mum", "password": "correct horse"}
```

Every field is required; `display_name` is `null` for none. The code may
have any case, dashes or spaces. The answer is `logged_in { session,
remembered: false }`: the account is made with the invite's role, and the
session is bound to the device that sent it, as a password sign-in's is.
Refusals:

- `invite_invalid`: no invite has that code, or its invite was revoked.
  It is counted as a wrong guess before it is checked, as a wrong
  password is, so guessing ends in `rate_limited`.
- `invite_expired`, `invite_exhausted`: the code is right, and its invite
  is past its expiry, or has made every account it could.
- `username_taken`; `invalid_input` for a username, display name or
  password that is not acceptable. Neither uses the invite up.

A device that is not signed in draws on the sign-in budget every such
device shares, as `login_password` does (`rate_limited` once it is
spent). A refused redemption is not kept as an operation: sent again
with the same operation id, it runs again. Of two redemptions of an
invite's last use, one makes its account and the other is
`invite_exhausted`. It is never retried on its own on a new connection.

### SDK

`spoolframe_client_sdk::invite` is the link codec. A `Client` takes the
link as the user pasted or opened it:

- `read_invite(link) -> InviteInfo`: what the link says, with nothing
  sent: the key and its fingerprint, the name, the hints, whether it has
  a code, and the expiry and role hints as `expires_at` and `role` for
  "Joins as a consumer · expires Oct 14". `saved` says the key is a
  saved server already. `NotAnInvite` for anything else.
- `connect_invite(link) -> ServerEntry`: `Hello` at the link's hints,
  for "Is this the server you were invited to?": the name, fingerprint
  and server id that answered. Nothing is saved.
- `save_invite_server(link) -> ServerEntry`: pins the key as the link
  names it, verified, with the server id it answers with ("Save server
  only", and all a link without a code does). A saved server keeps its
  pin and gains the hints.
- `redeem_invite(link, username, display_name, password) -> Session`:
  `redeem_invite`, then the server saved as above and the session kept as
  a password sign-in's. A refused redemption saves nothing. Its refusals
  are `InviteInvalid`, `InviteExpired`, `InviteExhausted`,
  `UsernameTaken`, `InvalidInput` and `RateLimited`.

The link's addresses join the server's known addresses as told ones,
replacing none; the server's next `server_addresses` list replaces them
as it replaces any told address. Its relays are kept beside the ones the
server told. A key the device has not saved is another server, whatever
its name; one that answers with a saved server's server id is that
server with a new key, `IdentityChanged`, before any account is made.

## Picture subtitles

Each user chooses how picture subtitles (PGS, VobSub, DVB) are shown,
in `TrackPreferences`: `subtitles.bitmap` is `"text"` (read as text by
the server, the default) or `"pictures"`. The choice is per user, so it
is the same on every device. An unknown value reads as `"text"`.

The server applies it when a `PlaybackPlan`, `StartPlayback` or
`ChangeSubtitle` request has `bitmap: "auto"`:

| Preference | The server reads the track | Else |
|---|---|---|
| `text` | sent as SubRip or WebVTT (`Ocr`) | drawn by the client if it can, else burned in |
| `pictures` | not used | drawn by the client if it can, else burned in |

The server cannot read a track when `subtitles.ocr` is off, when
tesseract has no data for the track's language, or when the client takes
neither SubRip nor WebVTT. With `subtitles.ocr` off the preference has no
effect. A request with `bitmap: "text"` still gets text, whatever the
preference, or the plan is `Unsupported` if the server cannot read the
track. A request with `bitmap: "burn_in"` is planned as `pictures`: it
asks for pictures, and they are burned in only if the client cannot draw
them. The value stays on the wire, but it no longer forces a burn-in.

The rules, for every plan:

- A text subtitle is never burned in. It is copied if the client shows
  it, else converted to a text format the client shows (WebVTT for HLS).
  If the client shows no text format the output can carry, the subtitle
  is not shown: the plan has no subtitle, and a reason says why, e.g.
  "subtitle 1: the client shows no text subtitles; not shown". The video
  is not encoded because of a text subtitle.
- A picture subtitle is burned in only when it is not read as text (OCR
  is off, the server cannot read the track, or pictures were asked for)
  and the client cannot draw it in this delivery.

The plan says which was used. A direct plan has the picked subtitle with
its format and delivery: `pgs` `embedded` is drawn by the client, and
`srt` or `web_vtt` `external` is read as text. A converted plan has a
`SubtitleAction` of `ocr`, `burn_in` or `copy`.

Before this, `auto` meant drawn by the client if it can, then read as
text, then burned in. The meaning of `auto` changed, not its encoding. An
older app that saves `TrackPreferences` without `subtitles.bitmap` sets it
back to `"text"`.

### Whether the server offers it

`GetTrackPreferences` answers `TrackPreferences { preferences,
ocr_offered }` (`TrackPreferencesInfo`), not the bare preferences.
`ocr_offered` is true when `subtitles.ocr` is on and tesseract has at
least one language. When it is false, apps show the picture subtitle
choice disabled, because pictures are shown whatever it says. The admin
API's `me.trackPreferences` answers the same, as `{ preferences,
ocrOffered }`. `SetTrackPreferences` and `me.setTrackPreferences` still
take the bare preferences.

This changes the reply's encoding. It shipped in revision 4 without a
bump, so only `GetTrackPreferences` failed between mismatched builds.
Revision 5 (PX-14) covers it: a mismatched build is now refused at
`Hello`. Update the server and SDK together. The SDK's `Session::track_preferences` returns
`TrackPreferencesInfo`; read `.preferences` where the bare preferences
were used.

## Home rows and "Remove"

`Home` shows a show in one row only. While an episode of a show is in
progress, that episode is in Continue watching and the show has nothing
in Next up. When the user finishes the episode, the next episode goes to
Next up.

`HideFromContinueWatching { id }` removes one card: the movie or the
episode named by `id`. It removes the card from Continue watching or from
Next up, wherever it is. It does not remove the episode's show. Send the
card's own `id`, never its show's. `UnhideFromContinueWatching { id }`
puts that card back.

- The show's other episodes stay on Home.
- A removed Next up episode stays out until the user watches it. Then
  the episode after it goes to Next up.
- A removed episode in progress does not keep its show out of Next up.
- Playing or marking the removed item brings it back. Playing another
  episode of the show does not.

Before, removing an episode hid its whole show from both rows. The
encoding did not change, so `PROTOCOL_VERSION` stays the same. The
server drops the hides it stored per show (migration 0015), so those
shows come back once.

## Paused playback and the idle limit

A playback handle lives for 30 minutes without use. Before, the limit
was 5 minutes. These count as use:

- a media read: a range of the file, an HLS playlist, or an HLS segment;
- a request that names the handle: `UpdatePlaybackProgress`,
  `SeekPlayback`, `ChangeSubtitle` or `StopPlayback`.

While the player is paused, send `UpdatePlaybackProgress { handle,
sequence, position_ms }` once a minute. Send the paused position and the
next sequence number. This keeps the playback alive for as long as the
pause lasts. It does not change the watch state: the same position gives
the same result, and a paused item does not become watched.

A progress report keeps a playback alive, but it does not count as
playing. If the device starts another playback and the server has no
conversion slot free, the server ends a converted playback of that
device that was not read in the last 30 s. A paused playback is one of
these.

After the limit, every use of the handle gets `PlaybackExpired`. Do not
send the old handle again. Call `StartPlayback` with `start_ms` set to
the paused position, and play the new handle. `ManagedPlayback` in the SDK does
this itself; see "SDK: a playback the server ended".

The server logs each playback that it ends, at INFO, as `a playback
ended`. The log line has the reason (`idle`, `replaced`, `device_gone`,
`signed_out`, `access_removed`, `stopped_by_admin` or `failed`) and how
long the playback was not used. A client's own `StopPlayback` is not
logged.

The encoding did not change, so `PROTOCOL_VERSION` stays the same.

## SDK migration in 0.3.0

`Client::activate_profile` explicitly selects a foreground profile and returns
an `ActiveProfile` exposing the session methods. Background library and account
requests remain available under their existing PIN checks, and no longer
select a profile. Selecting another profile relocks earlier PIN unlocks.
Activation is idempotent when selecting the same profile; a PIN verification
that crosses a profile switch cannot restore its old unlock.

```rust
// Before: requests on another session implicitly switch the active profile.
let profiles = client.profiles(server_id).await?;
let profile = &profiles[chosen];
profile.unlock(pin).await?;
let home = profile.home().await?;

// After: the UI explicitly selects a profile.
let active = client.activate_profile(&profiles[chosen]).await?;
active.unlock(pin).await?;
let home = active.home().await?;
```

Progress now takes a playback handle. The SDK assigns increasing sequence
numbers, preserves them across transport retries, and fetches the last accepted
sequence when a handle is restored. The server checks handle ownership, rejects
older sequences, and rejects progress from superseded playbacks of the same
item in that session. A newer playback remains authoritative even after it
stops. Separate sessions can still report independently. Invalid or stopped
handles cannot report progress. Callers must finish final progress before
stopping the handle or switching profiles; the Linux player queues those calls
in event order.

```rust
// Before
session.update_progress(&media_id, position).await?;

// After
session.update_progress(&playback.handle, position).await?;
```

An incremental collection API would also change callers that need to avoid
collecting the full result in memory. The current SDK still returns a
complete `Vec`; a future page/stream API would let the UI consume batches.

## License boundary checks

Run `devenv shell -- bash scripts/check-licenses.sh` from this project.
The `spoolframe:licenses` task also runs before `ci:static`.
`deny.toml` checks the server's complete normal and build dependency graph;
AGPL is allowed only for the named server crates. `deny-sdk.toml` checks
public crate graphs with a permissive-license allowlist and no copyleft
exceptions. These checks include optional features and all target platforms.
Dependencies with unaccepted or unrecognized licenses fail the check.

The Linux client has its own `deny.toml` and `spoolframe-linux:licenses`
CI task. It permits its GPL application crates and LGPL/MPL dependencies,
but not AGPL server crates. Cargo-deny checks Rust dependency graphs;
it does not audit system libraries, codecs, fonts, or other shipped assets.
