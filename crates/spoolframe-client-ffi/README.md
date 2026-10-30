# spoolframe-client-ffi

Spoolframe's client side for Swift (iOS, macOS, tvOS) and Kotlin
(Android), through [UniFFI](https://mozilla.github.io/uniffi-rs/) 0.32.2:
`spoolframe-client-sdk` and the loopback `spoolframe-hls-bridge` as one
library an app links. The SDK does the work; this crate exports it.

Licensed MIT OR Apache-2.0, as the SDK and the bridge are, so a
proprietary app may link it. It never depends on an AGPL crate
(`scripts/check-licenses.sh` checks it with `deny-sdk.toml`). UniFFI itself
is MPL-2.0, whose copyleft is per file: an app links it under its own
license.

## What crosses

Everything is typed. Nothing is passed as JSON.

- The SDK's, protocol's and types' own records and enums: `Item`,
  `ItemDetail` (with `copies`), `HomeScreen`, `PlaybackPlan`,
  `ServerEntry`, `InviteInfo` (its `kind` says account, guest or
  federation), `LocalDownload`, `Reachability` and the rest. Their crates
  derive UniFFI's traits behind a `uniffi` feature, off by default: the
  server never builds UniFFI (`cargo tree -p spoolframe-server -i uniffi`
  finds nothing).
- IDs (`MediaId`, `LibraryId`, `ArtworkId`, `PersonId`) and dates cross as
  the strings they are on the wire; socket addresses as `host:port`; paths
  as strings; durations as durations (`TimeInterval`,
  `java.time.Duration`); times as timestamps (`Date`, `Instant`).
- A few records of this crate's own, where UniFFI can't take the SDK's
  type as it is (`values.rs`): `DeviceCapabilities` (sets as lists),
  `FilterValues` (a counted value per type), and `ItemSearchResults` and
  `HitSearchResults` (the generic `SearchResults`, once per item type).
- `SpoolClient` (the device), `SpoolSession` (a user on a server),
  `SpoolPortable` (portable identities), `SpoolPairing` and
  `SpoolPairingOffer` are objects; `CoreStorage` and `AppSecurityKey` are
  traits the app implements.

The bindings are one file per crate: `spoolframe_client_ffi`,
`spoolframe_client_sdk`, `spoolframe_protocol` and `spoolframe_types`.
Swift compiles all four into one module (their type names do not clash);
Kotlin gets a package each, `uniffi.<crate>`.

## Errors

Every call that can fail throws `SpoolError`. Its cases are `SdkError`'s,
one for one, with the same names and their fields as values:
`IdentityChanged { saved, now, server_id }`, `RateLimited { retry_after }`,
`KindNotAllowed { kind }`, `PairingRefused { reason }`,
`NotOnServer { request }` and so on. Nested SDK errors keep their own enum
(`Phrase { error: PhraseFailure }`, `SecurityKey { error: SecurityKeyError }`,
`Pairing { error: PairingFailure }`). Two cases are this layer's own:
`Cancelled` (the app stopped the call) and `Bridge` (the loopback player
bridge couldn't start).

No error is flattened to a string. `message()` (and Swift's
`description`) is the SDK's own text, to show.

`kind()` says what to do about it, whichever case it is. It is
`SdkError::kind()`, computed in the SDK:

| `ErrorKind` | Do |
| --- | --- |
| `Transport` | Show the state again; the next call dials. |
| `SessionEnded` | Show Who's watching or the sign-in. |
| `NeedsPin` | Ask for the profile's PIN, `unlock`. |
| `NeedsNewPassword` | Show "Choose a new password". |
| `NeedsRecentSignIn` | Confirm (password or passkey), then ask again. |
| `RateLimited { retry_after }` | Try again after `retry_after`. |
| `ServerTooOld` | Hide what needs a newer server. |
| `Refused` | Say why, from the case or `message()`. |

## Events

Each stream is an object with an async `next()`. Loop over it in a task;
it returns `nil`/`null` once the client is closed:

```swift
let profiles = client.watchProfiles()
Task {
    while let notice = await profiles.next() {
        switch notice {
        case .event(let event): handle(event)
        case .lagged: reloadProfiles()
        }
    }
}
```

| Stream | From | Gives |
| --- | --- | --- |
| `watch_profiles` | `SpoolClient` | `ProfileNotice` (`ProfileEvent`, or `Lagged`) |
| `watch_playlists` | `SpoolClient` | `PlaylistNotice` |
| `watch_libraries` | `SpoolClient` | `LibrariesNotice` |
| `watch_subtitle_timing` | `SpoolClient` | `SubtitleTimingNotice` |
| `watch_downloads` | `SpoolClient` | `DownloadNotice` (`Changed`, `Removed`, `Lagged`) |
| `watch_servers` | `SpoolClient` | `changed()`: the list changed |
| `state(endpoint_id)` | `SpoolClient` | `changed()`: a `ConnectionState` |
| `signed_out(endpoint_id)` | `SpoolClient` | `next()`: the server signed this device out |
| `watch_playback_share(token)` | `SpoolSession` | `UplinkShare`: the playback's share of the server's uplink |

`Lagged { missed }` means the app fell behind and events were dropped:
read the state again.

A foreign task's cancellation does not reach the Rust future. Calls that
must stop early have their own `cancel()`: `PendingSignIn`,
`DownloadRun`, `SpoolPairing`.

## Playback

`SpoolSession::play` plans the item, opens it with the SDK's
`ManagedPlayback` and serves it on the loopback bridge (127.0.0.1, one
port for the app, a new one if the system takes a suspended app's socket
back). The URL is `http://127.0.0.1:<port>/<token>/master.m3u8` for HLS
or `/<token>/file` for direct play: AVPlayer, AirPlay and the system
subtitle menu depend on that shape. The token is what the other playback
calls take (`update_progress`, `change_quality`, `follow_replan`,
`subtitle_menu`, `end_playback`).

The bridge is the `hls-bridge` feature, on by default. A player that
reads the bytes itself (Android's Media3, through a `DataSource`) needs
no bridge: it builds with `--no-default-features`, opens with
`open_playback` (a token, no URL) and reads through
`playback_reader(token, length)`, the SDK's media readers: `read`
blocks its thread until the bytes are here, `size`, `content_type`,
and `set_flushing` to let a waiting read go. A conversion's HLS is read
by name instead: `open_playback_resource(token, path)` reads
`master.m3u8`, or a name a playlist gives, whole (its type and length),
and `read_playback_resource(token, path, offset, length)` reads ranges
of the one opened last, so Media3's ranges in one segment are one fetch.
The server's playlists name their resources relatively, so
`HlsMediaSource` resolves them under the app's own scheme and token.
Every other playback call takes the token either way; only `play`,
`playback_url` (serve an opened playback) and `SpoolError.Bridge` need
the feature.

A paired server's `fed:` item plays from that server when this device
reaches it, else through the user's own (`PlaybackStart.proxied`). That
routing (`federation.rs`) moves into the SDK as
`Session::open_any_playback`.

## From the Coldsteel registry

During development, take this crate and the SDK crates under it from the
Coldsteel registry, as the SDK's README says ("Taking the crates from the
Coldsteel registry"): patch each spoolframe crate, this one too, to its
Coldsteel copy:

```toml
[patch.crates-io]
spoolframe-client-ffi = { version = "=0.16.4-beta.1", registry = "coldsteel" }
# and spoolframe-types, -protocol, -policy, -client-sdk, -hls-bridge
```

## Building

The bindings generator is this crate's `uniffi-bindgen` bin, at the
library's UniFFI version, behind the `bindgen` feature:

```sh
cargo run -p spoolframe-client-ffi --features bindgen --bin uniffi-bindgen -- \
  generate --library <the built library> --language swift --out-dir <dir>
```

`--library` takes the static library (`.a`) or the shared one (`.so`); it
reads the UniFFI metadata of every crate in it.

### XCFramework (iOS, macOS, tvOS)

```sh
targets=(aarch64-apple-ios aarch64-apple-ios-sim aarch64-apple-darwin
         aarch64-apple-tvos aarch64-apple-tvos-sim)
for t in "${targets[@]}"; do
  cargo build --locked --release -p spoolframe-client-ffi --lib --target "$t"
done
cargo run -p spoolframe-client-ffi --features bindgen --bin uniffi-bindgen -- \
  generate --library target/aarch64-apple-darwin/release/libspoolframe_client_ffi.a \
  --language swift --out-dir out/bindings
```

Then one headers directory with the four `*FFI.h` headers and a
`module.modulemap` naming a module for each, the names the generated
Swift imports:

```
module spoolframe_client_ffiFFI { header "spoolframe_client_ffiFFI.h" export * }
module spoolframe_client_sdkFFI { header "spoolframe_client_sdkFFI.h" export * }
module spoolframe_protocolFFI { header "spoolframe_protocolFFI.h" export * }
module spoolframe_typesFFI { header "spoolframe_typesFFI.h" export * }
```

and `xcodebuild -create-xcframework` with `-library <target>/release/libspoolframe_client_ffi.a -headers <headers>`
per target (outside Nix's SDK and toolchain: unset `SDKROOT`,
`DEVELOPER_DIR`, `NIX_CFLAGS_COMPILE`, `NIX_LDFLAGS`, `CC`, `CXX`, `LD`).
The four `.swift` files go into the Swift package beside it.

tvOS, and iOS's IPv6 flags, need iroh's `netdev` 0.46.3 and `netwatch`
0.19.3 patched (with `libc` at most 0.2.189). This workspace does not
carry those patches: build the Apple slices from a workspace that does
(the iOS repository's `rust/`, with this crate as a git dependency), and
its `[patch.crates-io]` applies.

### AAR (Android)

With [cargo-ndk](https://github.com/bbqsrc/cargo-ndk) and the NDK:

```sh
cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 -o android/src/main/jniLibs \
  build --locked --release -p spoolframe-client-ffi --lib
cargo run -p spoolframe-client-ffi --features bindgen --bin uniffi-bindgen -- \
  generate --library target/aarch64-linux-android/release/libspoolframe_client_ffi.so \
  --language kotlin --out-dir android/src/main/kotlin
```

The Kotlin bindings load `libspoolframe_client_ffi.so` through JNA
(`net.java.dev.jna:jna`, the `@aar` artifact); the Gradle library module
with `jniLibs` and the generated sources builds the AAR.

## Tests

`cargo nextest run -p spoolframe-client-ffi`: every `SdkError` variant
crosses as the case of its name with its text and kind, the conversions,
the bridge's URLs, and `tests/bindings.rs`, which generates the Swift and
Kotlin bindings from the crate's cdylib and checks every crate's files
are there. Nothing is compiled in Swift or Kotlin here.
