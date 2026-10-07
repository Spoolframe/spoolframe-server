# Development wire revision 6

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
hold at most an eighth of the entries and bytes. The anonymous sign-in
requests have their own pool of 512 entries and 8 MiB with the same
shares, and their outcomes are replayed for 30 s after they are known, or
until the operation expires if that is sooner.
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
| `network` | `"loopback"` (the server's own machine), `"local"` (private IPv4, link-local, CGNAT and VPNs' `100.64.0.0/10`, IPv6 unique-local) or `"internet"` (any other: a public address). An unknown value decodes as `Unknown`. |

The server judges `network` as the web approval page judges
`fromInternet`. An app's approval screen shows the address, "The TV
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

A signed-in device can ask where the server can be dialled directly
(PX-12). This needs no new revision: a server that does not know the
request answers `Unsupported`, and the SDK then does without.

| Request | Reply |
|---|---|
| `{"type": "server_addresses"}` | `ServerAddresses { addrs }` |

`addrs` is every address on the server endpoint's interfaces, with its
port: LAN, public and VPN addresses. Loopback, link-local and unspecified
addresses are left out. Each one is text, `192.168.1.10:4433` or
`[2001:db8::1]:4433`, in CBOR as in JSON. Only a session is told: without
one, or with one that is not live on this endpoint, the reply is
`Unauthenticated`. A locked profile is told too, as for `WhoAmI`. The
addresses are never in `Hello`, because a stranger must not learn the
server's internal and VPN addresses. It is a read, and the SDK may retry
it.

Iroh announces these addresses inside each connection too, but a client
sees only the paths that opened. A phone that never reached the server's
VPN address cannot learn it from iroh.

### SDK

The SDK asks after each sign-in or resume, and on each new connection
(first dial, app restart, reconnect) when it keeps a session for the
server. It asks on that one connection, and a failure costs only the
addresses. Nothing in the API changes. Each told address joins the saved
server's addresses, marked as told, with the time it was told. The same
30-day age-out and 32-address cap apply. A told address becomes reached
when a connection reaches it, and a reached one stays reached. Under the
cap, the newest reached addresses are kept first, and told ones fill the
room that is left. Every saved address is dialled, told or reached.

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
effect. A request with `bitmap: "text"` or `"burn_in"` still gets what it
asks for, whatever the preference.

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
