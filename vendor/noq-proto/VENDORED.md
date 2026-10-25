# Vendored: noq-proto 1.4.0

Source: <https://crates.io/crates/noq-proto/1.4.0>
(<https://github.com/n0-computer/noq>, commit `34f75c49`), MIT OR
Apache-2.0, by the Quinn and n0 contributors. `LICENSE-APACHE` and
`LICENSE-MIT` are the upstream licenses, unchanged. Every file is the
published crate's, as crates.io serves it (`.cargo_vcs_info.json`,
`Cargo.toml.orig` and its `Cargo.lock` included), except `src/connection/
assembler.rs` and this file.

noq-proto is iroh's QUIC state machine (n0's fork of quinn-proto): every
stream a client or a paired server reads from this server is assembled by
it. The workspace takes this copy in place of the crates.io release
(`[patch.crates-io]` in the root `Cargo.toml`). A `[patch]` reaches only
the workspace it is in: the Linux app and the tvOS bridge take
`spoolframe-client-sdk` from the registry and build against crates.io's
noq-proto until their own workspaces carry the patch, either as
`[patch.crates-io] noq-proto = { version = "=1.4.0+spoolframe.1",
registry = "coldsteel" }` once this crate is published to kellnr under
that version (its `Cargo.toml`'s `version`, nothing else, would change),
or as a copy of this directory with a `path` patch. Until they do, the
SDK's stream receive window stays 2 MiB, under the unpatched release's
cap (`STREAM_WINDOW` in the SDK's client.rs, PX-62).

## Changes from upstream

- `Assembler::defragment` coalesces contiguous chunks smaller than
  `buffered / MAX_CHUNKS` (and never smaller than 128 bytes), whether or
  not they were already marked defragmented. 1.4.0 keeps every
  well-utilized chunk as it is, so past a lost packet, or ahead of a
  slow reader, each full-size packet of a stream stays its own chunk and
  more than 2,048 of them close the connection with `INTERNAL_ERROR`
  ("too many gaps in stream buffer"), although nothing is wrong with the
  data. A port of quinn-rs/quinn#2814 (quinn-proto 0.11.18, commit
  `c3f8b098`), as n0-computer/noq#828's third commit carries it, onto
  1.4.0's `VecDeque` assembler (the shape quinn's `main` has, where the
  same hunks apply). The two tests of that commit come with it.

Drop this copy for the crates.io release once noq#828, or an equivalent,
ships in noq-proto (PX-62).
