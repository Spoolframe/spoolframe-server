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
  `FilterValues` (a counted value per type), and `SearchResults`. A plain
  search and a federated one both give `SearchResults` of
  `SearchResult`s, each an item with its `copies` (`None` for a plain
  search's); `only_from(origin)` narrows it to one server for a chip,
  counted again, and `see_all_count(kind)` is the number "See all" names.
- `SpoolClient` (the device), `SpoolSession` (a user on a server),
  `SpoolPlayback` (one item playing), `SpoolPortable` (portable
  identities), `SpoolPairing` and `SpoolPairingOffer` are objects;
  `CoreStorage` and `AppSecurityKey` are traits the app implements. Or, for `CoreStorage`, `FileStorage(root,
  cipher)`: a file per key under `root`, each written whole and renamed
  into place, the secrets sealed by the app's `SecretCipher` (`seal`,
  `unseal`; on Android a Keystore key).

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
| `watch_own_profile` | `SpoolSession` | `OwnProfileNotice` (`Change`, `Lagged`): this user's profile only |
| `watch_playlists` | `SpoolClient` | `PlaylistNotice` |
| `watch_libraries` | `SpoolClient` | `LibrariesNotice` |
| `watch_subtitle_timing` | `SpoolClient` | `SubtitleTimingNotice` |
| `watch_downloads` | `SpoolClient` | `DownloadNotice` (`Changed`, `Removed`, `Lagged`) |
| `watch_running_downloads` | `SpoolClient` | the count of downloads under way, every profile's (`running_downloads()`), after each change; `current()` is it now |
| `watch_servers` | `SpoolClient` | the server list (`servers()`), after each change to it |
| `watch_server(endpoint_id)` | `SpoolClient` | `ServerEvent`: what a connection banner shows |
| `watch_catalog` | `SpoolSession` | `CatalogKey`: a kept catalog read came back different |
| `watch_share()` | `SpoolPlayback` | `UplinkShare`: the playback's share of the server's uplink |

`Lagged { missed }` means the app fell behind and events were dropped:
read the state again.

`watch_server` is one server's connection state, sign-outs and a needed
update as one stream, for a banner: `Connection { state }` (a
`ConnectionState`), `SignedOut` (the server signed this device out: its
sessions there are ended), or `UpdateRequired { who }` (the app and the
server speak different protocol revisions; the link stopped dialling).
A sign-out comes before the state it left. `current()` is the state when
the watch began. `state(endpoint_id)` and `signed_out(endpoint_id)` are
the two halves alone.

A foreign task's cancellation does not reach the Rust future. Calls that
must stop early have their own `cancel()`: `PendingSignIn`,
`DownloadRun`, `SpoolPairing`.

## Playback

`SpoolSession::play` plans the item, opens it with the SDK's
`ManagedPlayback` and serves it on the loopback bridge (127.0.0.1, one
port for the app, a new one if the system takes a suspended app's socket
back), as a `SpoolPlayback`. Its `info().url` is
`http://127.0.0.1:<port>/<token>/master.m3u8` for HLS or
`/<token>/file` for direct play: AVPlayer, AirPlay and the system
subtitle menu depend on that shape.

A `SpoolPlayback` is one item playing, and its controls:
`report_position` on every player tick (sent in the background, a few
at a time) and `flush` on pause; `change_quality`, `change_subtitle`
and `limit`, each the new plan to switch to or `None`; `read_subtitle`
for a subtitle that comes as a file (waited for up to 30 s while the
server reads it as text); `finish(position)` or `stop` at the end, and
dropping it closes it. `info()` is how it plays now: its plan, the
tracks the server chose, its markers and its running time.

A seek is told around the player's own: `seek_started(target)` as it
begins (the app's scrubber or a seek it asked of the player), and
`seek_done()` from the player's seek-complete signal (Media3's position
discontinuity, AVPlayer's seek completion handler). Until then the
player's positions are still the old place's, so `report_position` saves
`target` instead. Without `seek_done` the seek is over once a position
lands within 2 s of the target, or after 5 s.

The screen's mode is a planning input, not a sign-in one: a TV switches
between a 1080p UI mode and a 4K mode for the film, and the sign-in is
sent once. `DeviceSpec.capabilities` is the decoders' limits only. Say
the screen as it is now with `SpoolClient.set_display(ScreenCapabilities)`
(Swift and Kotlin `setDisplay`) once after `open` and again on each
change; every plan, open and re-plan that names no display of its own
sends it, so a mode switch counts from the next plan. `None` forgets it.

The SDK may plan a playback again behind the player (an Auto playback
on a slow link, or its share of the uplink moved). The object follows
it: a read of the old plan's bytes answers `Replanned` once, the object
already on the new plan, and the player builds its media again from
`info()`. A URL player, whose reads the bridge answers, asks with
`follow_replan()` when it stalls.

The bridge is the `hls-bridge` feature, on by default. A player that
reads the bytes itself (Android's Media3, through a `DataSource`) needs
no bridge: it builds with `--no-default-features`, opens with
`open_playback` (no URL) and reads from the object, on its loader
threads: every read blocks until the bytes are here. A direct play's
file or a progressive conversion's output is `read(offset, length)`,
with `size`, `content_type`, and `set_flushing` to let a waiting read
go. A conversion's HLS is read by name instead: `open_resource(path)`
reads `master.m3u8`, or a name a playlist gives, whole (its type and
length), and `read_resource(path, offset, length)` reads ranges of the
one opened last, so Media3's ranges in one segment are one fetch. The
server's playlists name their resources relatively, so `HlsMediaSource`
resolves them under the app's own scheme. Only `play`, `url()` (serve
an opened playback) and `SpoolError.Bridge` need the feature.

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
spoolframe-client-ffi = { version = "=0.17.0-beta.1", registry = "coldsteel" }
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
  build --locked --release -p spoolframe-client-ffi --lib \
  --no-default-features --features android
cargo run -p spoolframe-client-ffi --features bindgen --bin uniffi-bindgen -- \
  generate --library target/aarch64-linux-android/release/libspoolframe_client_ffi.so \
  --language kotlin --out-dir android/src/main/kotlin
```

The Kotlin bindings load `libspoolframe_client_ffi.so` through JNA
(`net.java.dev.jna:jna`, the `@aar` artifact); the Gradle library module
with `jniLibs` and the generated sources builds the AAR.

The SDK's network crates call Android's Java APIs through ndk-context,
which needs the app's Context, and abort without it. The `android`
feature puts the one JNI call that hands it over in this crate's own
`.so`, so the app needs no Rust of its own, only this class beside the
generated bindings:

```kotlin
package uniffi.spoolframe_client_ffi

object AndroidContext {
    init { System.loadLibrary("spoolframe_client_ffi") }

    @JvmStatic external fun init(context: android.content.Context)
}
```

Call `AndroidContext.init(context.applicationContext)` before
`SpoolClient.open`; a later call does nothing.

## Tests

`cargo nextest run -p spoolframe-client-ffi`: every `SdkError` variant
crosses as the case of its name with its text and kind, the conversions,
the bridge's URLs, and `tests/bindings.rs`, which generates the Swift and
Kotlin bindings from the crate's cdylib and checks every crate's files
are there. In devenv's `ci-test` profile, which brings `kotlinc`, a JDK
and Swift, it also compiles every crate's Kotlin together and typechecks
the Swift as one module (on Linux); elsewhere those two tests say so and
pass.
