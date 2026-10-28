# Spoolframe client SDK

License: MIT OR Apache-2.0, including the media readers extracted from the Linux
client with the copyright holder's permission. This crate does not depend on
`spoolframe-policy` or any GPL/AGPL implementation crate.

`Session::open_playback` returns a `ManagedPlayback`. Keep clones in the player
and its readers so they share replacement handles. `report_position` coalesces
updates; `flush` waits for pending progress; `finish(position)` sends the final
position before stopping. `close` stops without replacing the pending position.
Subscribe to `errors()` to surface background progress failures.

Playback admission and dispatched controls finish if their caller is cancelled.
An abandoned admission is stopped. Dropping the last owner also attempts cleanup;
call `finish` or `close` before shutting down Tokio to await it reliably.
Construct an owner in a Tokio runtime (`adopt` also requires an entered runtime).

A playback the server ended (a long pause, a server restart, the device let
go) is resumed by the `ManagedPlayback` itself. On `PlaybackExpired` it starts
the item again with the same tracks and options, at the last known position
(the reported progress, a seek, or the HLS segment asked for), swaps the new
handle in for every clone, and tries the operation once more. Concurrent
operations share one replacement. Direct reads, HLS playlists and segments read
from offset 0, progress, seeks, subtitle changes and `finish` go on with no
error: HLS segments are numbered on the item's own time, so the playlist stays
the same. Drive HLS through `ManagedPlayback::manifest`, `read_segment` and
`read_subtitle_segment`, not `Session`, to get this.

A progressive read (`read_chunk`), or the rest of an HLS segment read part-way,
gets `PlaybackResumed { at }` instead: a new encode's bytes do not continue the
old one's. Retire the reader, build one from `snapshot()` from offset 0, and
show the player at `at`; read the segment again from offset 0. Converted readers
stay pinned to their starting handle and epoch the same way after a `seek`
(`PlaybackRestarted`).

It does not resume one the app closed, one whose start is refused for good
(`SignInRequired`, `NotFound`, `CannotPlay`, `PlaybackStopped`; the same error
every time after), one that would play differently (`PlaybackChanged`), or more
than `MAX_RESUMES` (3) times in `RESUME_WINDOW` (5 minutes): then the operation
fails with `PlaybackExpired`.

The optional `media-readers` feature provides `reader::MediaReader` and
`progressive::ProgressiveReader`, with bounded read-ahead, buffering reports and
flushing. Their synchronous reads must run on a media/worker thread, never the
UI thread or a Tokio worker that must drive their asynchronous fetches.

Use `ProfileScope::token` when dispatching UI work and `ProfileToken::accept` on
completion. Invalidate the scope when changing profile or ending a session so
stale successes and errors cannot change the replacement session's UI.

`Client::open` listens for servers on the LAN over mDNS, and `Client::servers`
lists what it hears beside the saved servers. `Client::open_with_options` with
`ClientOptions { lan_discovery: false, .. }` turns the listening off: only saved
servers are listed, and a server is dialled only at its saved addresses or at
the ones given with `add_address`. Tests use it so that a real server on the
developer's network cannot change their results.

`ClientOptions::relays`, on by default, reaches a server through the relays it
told the device of (in an invite, or its `ServerAddresses`) when it cannot be
dialled directly. The device has no relay of its own: none is contacted until a
server is dialled through one. Off, the endpoint has no relay transport, and a
server is reached only at its addresses. It is fixed for the life of a
`Client`; an app whose user changes it opens a new one.

Downloads (PX-38, feature `spoolframe:downloads`) need
`ClientOptions::downloads_dir`. `Client::downloads().start(session, ...)`
prepares one on the server; the client's download manager then moves it on
while the client is open, one at a time, and `Client::close` stops it where it
got to. `list` reads the disk alone. On iOS, call `run_until_done` from a
background task, give the free space the system counts with `set_free_space`,
and exclude the directory from backup.

## noq-proto: add the patch to your app

iroh's QUIC state machine, noq-proto 1.4.0, closes a connection with "too
many gaps in stream buffer" when a stream holds more than 2,048 packets after
a lost packet or ahead of a slow reader. No data is missing when this occurs
(PX-62). The fix is n0-computer/noq#866. It is merged but not released: 1.4.0
is the newest noq-proto on crates.io, and iroh 1.4.1 uses it.

This repository's workspace takes noq-proto from git at the merge of #866.
crates.io ignores the `[patch]` of a published crate, so an app that takes
this SDK from crates.io builds against the unfixed 1.4.0.

The SDK works without the fix: it keeps the receive window of a stream at
2 MiB (`STREAM_WINDOW`), below the limit of 1.4.0. To get the fix, add the
same patch to your workspace's `Cargo.toml`:

```toml
[patch.crates-io]
noq-proto = { git = "https://github.com/n0-computer/noq", rev = "3ff2822f9e6ef327501f75cd6bc0f7d0f3e9b76a" }
```

That commit is noq-proto 1.5.0 from noq's `main`. It also contains #867
(unordered streams keep unread data when compacted), the deprecation of
`send_fairness` (#835) and `SendStream::set_affinity` (#836). The patch does
not stop you from publishing your own crate: cargo drops `[patch]` when it
publishes. Remove the patch when a noq-proto release contains #866.
