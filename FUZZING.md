# Fuzzing

The fuzz targets (PX-10) feed random and mutated input to the code that reads
what Spoolframe does not control: frames and CBOR from a peer, subtitle tracks
in a user's files, ffprobe's description of a file, and what a Plex Media Server
sends.

They use [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) and libFuzzer,
with AddressSanitizer and debug assertions on. cargo-fuzz needs a nightly
compiler. The rest of Spoolframe builds on stable, so the targets are a
workspace of their own in `fuzz/`, and `fuzz/toolchain.nix` supplies the
nightly compiler, cargo-fuzz and a C++ compiler for libFuzzer. You do not need
the dev shell to fuzz.

## Targets

| target | reads | code |
|---|---|---|
| `protocol_frame` | a stream: a frame (length, then CBOR), a multipart RPC reply | `spoolframe-protocol` `codec.rs` |
| `invite_link` | an invite link, as pasted or read from a QR code; one that parses must be written back as itself | `spoolframe-protocol` `invite.rs` |
| `webauthn` | a security key's WebAuthn registration (`attestationObject`, `clientDataJSON`) and assertion, as an Apple device sends them (P7-08) | `spoolframe-protocol` `portable/roots.rs`, `portable/client_data.rs` |
| `protocol_cbor` | a CBOR payload as `RpcRequest`, `RpcResponse`, `ResourceRequest`, `ResourceResponse`; a decoded request must encode and decode back to itself | `codec.rs`, `rpc.rs`, `resource.rs` |
| `pgs` | a PGS (`.sup`) stream | `spoolframe-media` `pgs.rs` |
| `dvb` | a DVB subtitle track's packets | `spoolframe-media` `dvb.rs` |
| `vobsub` | a VobSub track: its `.idx` header and packets | `spoolframe-media` `vobsub.rs` |
| `bif` | a PMS preview index (BIF) | `spoolframe-plex` `bif.rs` |
| `plex_json` | a PMS JSON response, decoded and converted to items | `spoolframe-plex` `wire.rs`, `convert.rs` |
| `ffprobe_json` | ffprobe's JSON for a file, made into a `MediaDescriptor` | the vendored `easy-ffprobe` model, `spoolframe-media` `descriptor.rs` |
| `keyframe_index` | a media file's own keyframe index: Matroska `Cues`, MP4 `stss` (PX-25) | `spoolframe-media` `keyframe_index.rs` |
| `picture_header` | an uploaded playlist cover's header: its format (JPEG, PNG, WebP, HEIC) and size, read before anything is decoded (PX-53) | `spoolframe-media` `picture.rs` |

The `dvb` and `vobsub` inputs are packets framed as `u16 LE length, u32 LE pts
in ms, data`. A VobSub input starts with a `u16 LE` length and that much `.idx`
header. `fuzz/src/lib.rs` has the framing. `spoolframe-plex` builds its private
parsers into the targets only with its `fuzzing` feature.

`pgs`, `dvb` and `vobsub` call each format's `cues`, which decodes within
`bitmap::Budget::DEFAULT` (finding 45): an input that decodes past it is an
`OverBudget` error, not a crash. The budget allows 1 Gi pixels of pictures, so
such an input can still use about 1 GiB before it is refused; libFuzzer's
default `-rss_limit_mb=2048` leaves room for that. The server runs this code in
a child (`decode.rs`), but the targets call it directly.

`keyframe_index` runs in the server process itself, not a child: it reads no
more than 48 MiB of a file in all and 32 MiB at once, walks MP4's sample
tables where they lie in what it read (no allocation from a count the file
states), and keeps at most about 4 million times (32 MiB).
An input it does not follow is a `NoIndex` error (the server then reads the
keyframes with ffprobe, confined), never a panic.

`picture_header` runs in the server process too, on any editor's upload:
it walks JPEG markers, PNG's `IHDR`, WebP's first chunk and HEIF's boxes
without allocating from a length the file states. The picture itself is
decoded by ffmpeg, confined, which this target does not reach.

## Running

```sh
scripts/fuzz.sh                 # every target, 60 s each
scripts/fuzz.sh 3600 pgs dvb    # an hour each, on two targets
FUZZ_FLAGS="-jobs=8 -workers=8" scripts/fuzz.sh 3600 protocol_cbor
```

Each target starts from its seeds in `fuzz/seeds/TARGET/` and, for `bif`,
`plex_json`, `ffprobe_json` and `picture_header`, from its crate's test
fixtures. It grows its
corpus in `fuzz/corpus/TARGET/`. A crash goes to `fuzz/artifacts/TARGET/`, and
the script exits 1 after it runs the other targets. The corpus and the
artifacts are not tracked.

The script finds crashes by the files in `fuzz/artifacts/TARGET/`, not only by
the exit status of `cargo fuzz`. It lists the directory before each target and
after it. A file that was not there before is a crash, an OOM or a timeout. The
script names the new files and counts the target as crashed. Old files do not
count, so you can keep them while you fix them. This is necessary because, with
`-ignore_crashes=1` (normal with `-fork`), libFuzzer exits 0 after crashes. An
overnight run printed "no crashes" over 64 crashes in `ffprobe_json` before the
script counted files.

For long campaigns use independent workers, not fork mode:

```sh
FUZZ_FLAGS="-jobs=16 -workers=16" scripts/fuzz.sh 6300
```

Each worker runs the target for the whole time and writes `fuzz/fuzz-N.log`
(ignored). With libfuzzer-sys 0.4.13 and the 2026-10-01 nightly, `-fork=16`
aborts within seconds of starting, in libFuzzer's own parent process
(AddressSanitizer `negative-size-param`, no frame of ours, no input saved),
on every target. Workers are not affected.

To reproduce a crash, and to make an input smaller:

```sh
nix shell --impure -f fuzz/toolchain.nix -c bash -c \
  'cd fuzz && cargo fuzz run TARGET artifacts/TARGET/crash-…'
nix shell --impure -f fuzz/toolchain.nix -c bash -c \
  'cd fuzz && cargo fuzz tmin TARGET artifacts/TARGET/crash-…'
```

Fix a crash with a regression test in the crate that owns the code, red first,
with the smallest input that shows it. Do not add crash inputs to `fuzz/seeds/`.

## Seeds

`fuzz/seeds/` holds small inputs only. `cargo run --example seeds`, run in
`fuzz/` in the dev shell (it needs ffmpeg), writes them again: protocol
messages (rewrite them whenever the wire revision changes: a seed of an
older revision no longer decodes, and the target starts from nothing),
invite links, a two-cue PGS stream (whole, and split into fragments), and the same
cues encoded by ffmpeg's `dvbsub` and `dvdsub` encoders and read back as the
server reads a track, and two seconds of film as Matroska and as MP4.

## Pins

`fuzz/toolchain.nix` pins nixpkgs and rust-overlay to the revisions that
`devenv.lock` had when it was written, so a campaign gets the same nightly
each time. Move the pins with `devenv.lock` when the nightly must change.
`fuzz/Cargo.lock` is tracked, and `scripts/check-licenses.sh` checks the fuzz
workspace against `deny.toml`. libFuzzer is under NCSA, which is allowed for
`libfuzzer-sys` only.
