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

Direct range reads recover an expired handle once per call, with concurrent
recoveries sharing one replacement. Recovery requires another direct plan with
the same size. Converted readers stay pinned to their starting handle and epoch: retire the
old reader before `seek`, then build its replacement from the returned snapshot.

The optional `media-readers` feature provides `reader::MediaReader` and
`progressive::ProgressiveReader`, with bounded read-ahead, buffering reports and
flushing. Their synchronous reads must run on a media/worker thread, never the
UI thread or a Tokio worker that must drive their asynchronous fetches.

Use `ProfileScope::token` when dispatching UI work and `ProfileToken::accept` on
completion. Invalidate the scope when changing profile or ending a session so
stale successes and errors cannot change the replacement session's UI.
