# Development wire revision 3

Release 0.3.0 keeps the `spoolframe/client/1` ALPN and sets
`PROTOCOL_VERSION` to 3. Update the server and SDK together. Revision-1 and revision-2
binaries cannot use this wire format. Clients must explicitly select foreground profiles and report progress by
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
`RemovePasskey`, and `SetPinRequired` with `required: false`. An older
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
