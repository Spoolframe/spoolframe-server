# Spoolframe client SDK

The SDK for Spoolframe client apps. It finds and dials Spoolframe servers
over [Iroh](https://iroh.computer), keeps the saved servers and their pins,
signs users in, and browses and plays media.

License: MIT OR Apache-2.0, including the media readers. Proprietary apps can
link it: the SDK does not depend on any GPL or AGPL crate.

## Install

```toml
[dependencies]
spoolframe-client-sdk = "0.16"
# With the GStreamer-friendly media readers:
# spoolframe-client-sdk = { version = "0.16", features = ["media-readers"] }
```

The wire protocol is stable: an app and a server of different releases
still talk to each other. This crate's API follows semver but is not
stable: each minor release (0.17 → 0.18) can break it, while patch
releases on one minor line do not. Read the CHANGELOG's BREAKING entries
before moving to a new minor.

The SDK runs on Tokio. Then add the noq-proto patch below to your
workspace.

## Taking the crates from the Coldsteel registry

Each version also goes to the Coldsteel registry (kellnr,
`https://cargo.coldsteel.casa`), right after crates.io, so an app need not
wait for the kellnr crates.io proxy's cache to see it. During development
an app can take every spoolframe crate from there. Name the
registry in the app's `.cargo/config.toml`:

```toml
[registries.coldsteel]
index = "sparse+https://cargo.coldsteel.casa/api/v1/crates/"
```

Keep the dependency a crates.io one, and patch each spoolframe crate to its
Coldsteel copy, at the same exact version:

```toml
[dependencies]
spoolframe-client-sdk = "=0.17.0-beta.2"

[patch.crates-io]
spoolframe-types = { version = "=0.17.0-beta.2", registry = "coldsteel" }
spoolframe-protocol = { version = "=0.17.0-beta.2", registry = "coldsteel" }
spoolframe-policy = { version = "=0.17.0-beta.2", registry = "coldsteel" }
spoolframe-client-sdk = { version = "=0.17.0-beta.2", registry = "coldsteel" }
spoolframe-hls-bridge = { version = "=0.17.0-beta.2", registry = "coldsteel" }
```

The Coldsteel copies depend on each other as crates.io crates, and a
`[patch.crates-io]` applies to the whole graph, so with every spoolframe
crate patched none comes from crates.io. A patch the graph does not use
is a warning only. Remove the section to go back to crates.io; cargo drops
`[patch]` when the app publishes, as for noq-proto below.

## Getting started

The app gives the SDK a `Storage` (where to keep secrets and data) and a
`Device` (what the device and the app call themselves). `MemoryStorage` is
for tests; a real app keeps secrets in the platform's secret store.

```rust
use std::sync::Arc;
use spoolframe_client_sdk::types::{ClientCapabilities, Platform};
use spoolframe_client_sdk::{Client, Device, MemoryStorage, SdkError};

async fn run() -> Result<(), SdkError> {
    let device = Device {
        name: "Living room TV".into(),
        client_name: "my-spoolframe-app".into(),
        client_version: "1.0.0".into(),
        platform: Platform::Cli,
        capabilities: ClientCapabilities::default(),
    };
    let client = Client::open(Arc::new(MemoryStorage::default()), device).await?;

    // Servers heard on the LAN, and the saved ones.
    let server = client.servers().await?.into_iter().next().expect("a server");

    // Connect, show the fingerprint to the user, then save (pin) the server.
    let entry = client.connect(&server.endpoint_id).await?;
    println!("Is this your server? {}", entry.fingerprint);
    client.pin(&server.endpoint_id, true).await?;

    let session = client.sign_in(&server.endpoint_id, "alice", "password", false).await?;
    let home = session.home().await?;
    println!("{} items to continue", home.continue_watching.len());

    client.close().await;
    Ok(())
}
```

`examples/browse.rs` in the repository does the same on a real network.

Every request has a time limit (`Timeouts`); a request not answered in time
is `SdkError::TimedOut`. Call `Client::resume` when the app comes back to
the foreground or the device wakes, so connections that died while the app
was away are dropped before a request waits on one.

## Finding servers

`Client::open` listens for servers on the LAN over mDNS, and
`Client::servers` lists what it hears beside the saved servers.
`Client::open_with_options` with `ClientOptions { lan_discovery: false, .. }`
turns the listening off: only saved servers are listed, and a server is
dialled only at its saved addresses or at the ones given with
`add_address`. Use it in tests, so that a real server on the developer's
network cannot change their results.

`ClientOptions::relays`, on by default, reaches a server through the relays
it told the device of (in an invite, or its `ServerAddresses`) when it cannot
be dialled directly. The device has no relay of its own: none is contacted
until a server is dialled through one. Off, the endpoint has no relay
transport, and a server is reached only at its addresses. It is fixed for
the life of a `Client`; an app whose user changes it opens a new one.

## Playback

`Session::open_playback` returns a `ManagedPlayback`. Keep clones in the
player and its readers so they share replacement handles. `report_position`
coalesces updates; `flush` waits for pending progress; `finish(position)`
sends the final position before stopping. `close` stops without replacing
the pending position. Subscribe to `errors()` to show background progress
failures.

Playback admission and dispatched controls finish if their caller is
cancelled. An abandoned admission is stopped. Dropping the last owner also
attempts cleanup; call `finish` or `close` before shutting down Tokio to
await it reliably. Construct an owner in a Tokio runtime (`adopt` also
requires an entered runtime).

### When the server ends a playback

The server ends a playback after a long pause, when it restarts, or when the
device lets go of it. The `ManagedPlayback` resumes it by itself. On
`PlaybackExpired` it starts the item again with the same tracks and options,
at the last known position (the reported progress, a seek, or the HLS
segment asked for), swaps the new handle in for every clone, and tries the
operation once more. Concurrent operations share one replacement.

These go on with no error: direct reads, HLS playlists and segments read
from offset 0, progress, seeks, subtitle changes and `finish`. HLS segments
are numbered on the item's own time, so the playlist stays the same. Drive
HLS through `ManagedPlayback::manifest`, `read_segment` and
`read_subtitle_segment`, not `Session`, to get this.

A progressive read (`read_chunk`), or the rest of an HLS segment read
part-way, gets `PlaybackResumed { at }` instead: a new encode's bytes do not
continue the old one's. Retire the reader, build one from `snapshot()` from
offset 0, and show the player at `at`; read the segment again from offset 0.
Converted readers stay pinned to their starting handle and epoch the same
way after a `seek` (`PlaybackRestarted`).

The `ManagedPlayback` does not resume a playback in these cases:

- the app closed it;
- its start is refused for good (`SignInRequired`, `NotFound`, `CannotPlay`,
  `PlaybackStopped`): the same error comes every time after;
- it would play differently (`PlaybackChanged`);
- it was resumed more than `MAX_RESUMES` (3) times in `RESUME_WINDOW`
  (5 minutes): then the operation fails with `PlaybackExpired`.

### Media readers

The optional `media-readers` feature provides `reader::MediaReader` and
`progressive::ProgressiveReader`, with bounded read-ahead, buffering reports
and flushing. Their synchronous reads must run on a media or worker thread,
never the UI thread or a Tokio worker that must drive their asynchronous
fetches.

## Profiles

Use `ProfileScope::token` when you dispatch UI work, and
`ProfileToken::accept` when it completes. Invalidate the scope when the
profile changes or a session ends, so stale successes and errors cannot
change the next session's UI.

## Downloads

Downloads need `ClientOptions::downloads_dir`.
`Client::downloads().start(session, ...)` prepares one on the server. The
client's download manager then moves it on while the client is open, one at
a time, and `Client::close` stops it where it got to. `list` reads the disk
alone. On iOS, call `run_until_done` from a background task, give the free
space the system counts with `set_free_space`, and exclude the directory
from backup.

## noq-proto: add the patch to your app

iroh's QUIC state machine, noq-proto 1.4.0, closes a connection with "too
many gaps in stream buffer" when a stream holds more than 2,048 packets after
a lost packet or ahead of a slow reader. No data is missing when this occurs.
The fix is n0-computer/noq#866. It is merged but not released: 1.4.0 is the
newest noq-proto on crates.io, and iroh 1.4.1 uses it.

The SDK works without the fix: it keeps the receive window of a stream at
2 MiB (`STREAM_WINDOW`), below the limit of 1.4.0. cargo ignores the
`[patch]` section of a dependency, so to get the fix, add the patch to your
own workspace's `Cargo.toml`:

```toml
[patch.crates-io]
noq-proto = { git = "https://github.com/n0-computer/noq", rev = "3ff2822f9e6ef327501f75cd6bc0f7d0f3e9b76a" }
```

That commit is noq-proto 1.5.0 from noq's `main`. It also contains #867
(unordered streams keep unread data when compacted), the deprecation of
`send_fairness` (#835) and `SendStream::set_affinity` (#836). The patch does
not stop you from publishing your own crate: cargo drops `[patch]` when it
publishes. Remove the patch when a noq-proto release contains #866.
