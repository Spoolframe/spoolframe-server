# Development wire revision 4

Revision 4 (P4-01, trusted devices) keeps the `spoolframe/client/1` ALPN
and sets `PROTOCOL_VERSION` to 4. Update the server and SDK together.
Revision 3 and older binaries cannot use this wire format. What changed
is in "Trusted devices" below.

Release 0.3.0 set `PROTOCOL_VERSION` to 3. Revision-1 and revision-2
binaries cannot use that wire format either. Clients must explicitly select foreground profiles and report progress by
playback handle; see the migration examples below. Clients
that depend directly on the protocol/types crates must update those
packages together with the SDK.

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
`RemovePasskey`, `SetPinRequired` with `required: false`, and
`SetDeviceTrust` except to stop trusting this device. An older
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

`BeginConfirmPasskey` lists only the session user's app passkeys
(relying party `spoolframe.app`). It sends a nonce, and the app signs the
challenge made from it (see "App passkey challenges"). The challenge
belongs to this session on this endpoint, for 5 minutes, and is used once. A sign-in challenge or the
challenge of another session does not finish it. A passkey of another user
is `InvalidCredentials`. A failure counts as a failed passkey sign-in of
the passkey's user from this endpoint. A user with no app passkey gets
`NotFound`. When the user's passkey sign-ins are rate-limited, `Begin`
gets `RateLimited` before a challenge is made.

## App passkey challenges

Every server shares the app relying party `spoolframe.app`, and the
platform writes only the origin `https://spoolframe.app` and the challenge
into `clientDataJSON`. So the challenge names the server: otherwise a
server could begin a ceremony on another server, give that challenge to
an app as its own, and finish there with what the app signed (PX-09,
finding 2).

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

A PIN is not accepted as proof. A PIN guards switching profiles on a
shared device, and it is weaker than a sign-in. A locked session gets
`ProfileLocked` for all three requests, as for other sensitive requests:
unlock with the PIN first, then confirm. A confirmation does not unlock a
profile. The lock belongs to the connection, and the confirmation belongs
to the session.

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
