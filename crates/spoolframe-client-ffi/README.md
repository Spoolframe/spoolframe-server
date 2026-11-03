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
  `SpoolPlayback` (one item playing), `SpoolMarkers` (its skip buttons),
  `SpoolPortable` (portable identities), `SpoolPairing` and `SpoolPairingOffer` are objects;
  `CoreStorage` and `AppSecurityKey` are traits the app implements. Or, for `CoreStorage`, `FileStorage(root,
  cipher)`: a file per key under `root`, each written whole and renamed
  into place, the secrets sealed by the app's `SecretCipher` (`seal`,
  `unseal`; on Android a Keystore key). Open it with
  `SpoolClient.openFiles`: on Android, `open` aborts the app.

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
`report_position` on every player tick (the first sent at once, then
the newest every 10 s, a move back or a seek at once) and `flush` on
pause; `change_quality`, `change_subtitle`
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

A progressive conversion can't seek in its bytes: its output has no
length, and a byte offset means nothing to the next encode. So every seek
goes through `seek(target)`, one call whatever the plan, which counts as
`seek_started`. `None` is a direct play or an HLS conversion: nothing was
sent, and the app seeks its player. On a progressive conversion the
server encodes again from `target`, and the answer is the new plan:

```kotlin
suspend fun seek(to: Duration) {
    val start = try {
        playback.seek(to)
    } catch (e: SpoolException.Cancelled) {
        return // a newer seek took its place
    }
    if (start == null) player.seekTo(to.toMillis()) else rebuildMedia(start)
}
```

```swift
func seek(to: TimeInterval) async throws {
    do {
        if let start = try await playback.seek(target: to) {
            rebuildMedia(start)
        } else {
            player.seek(to: to)
        }
    } catch SpoolError.Cancelled {
        // A newer seek took its place.
    }
}
```

- The next `read` is at offset 0, of the new encode. Its times are the
  item's own, from `target`, not from 0.
- A read in flight, and a read of the old media after, gets `Replanned`,
  as for any new plan; `Cancelled` while `set_flushing(true)` holds, so
  clear that before reading again.
- Seeks within 250 ms gather into the last, one restart. The ones it
  replaced get `Cancelled` and need nothing.
- A failed seek leaves the player on the encode it had, reading on.
- Call `seek_done` once the player plays there.

A `Duration` can't be negative, here or anywhere in the API, and the
check is in the generated bindings, before this crate runs: Swift's
stops the app with `Invalid duration, must be non-negative`, and Kotlin's
throws. So clamp what the app works out itself, `max(0, position - 10)`,
before a `seek`. UniFFI has no setting for it: `Duration` is
a built-in type, not a custom one, and a wrapper type for every time in
the API would cost Rust callers more than the clamp costs an app.

A read before what the reader keeps (1 MiB behind the last read) gets
`ReadBehind { offset, kept_from }`: the player started over, as Media3
does with no seek map, and those bytes are gone. Call `seek` with where
the viewer is.

`SpoolMarkers` follows an item's markers for a player the app drives
itself: Skip Intro, Skip Credits, Next, Up next, and intros that skip by
themselves, which the viewer can undo. `SpoolPlayback.follow_markers`
makes it for a playback, sharing the playback's seeks; `SpoolMarkers.new`
makes it for a file on this device, played with no `SpoolPlayback`, and
then `seek_started` and `seek_done` go to it. Feed it the player's tick.
Its calls never block. It seeks nothing: where it gives a target the app
seeks there with `SpoolPlayback.seek`, as above (the player, for a file
on this device), and the target counts as a seek from then.

```kotlin
val markers = playback.followMarkers(hasNext = false, autoSkip = true, upNextCard = true)

// On each tick; the length is null until the player knows it.
for (action in markers.onPosition(at, length)) when (action) {
    is MarkerAction.AutoSkip -> { seek(action.to); showUndo(undoWindow()) }
    is MarkerAction.Offer -> showButton(action.v1) // SkipIntro, SkipCredits, Next, or null
    is MarkerAction.UpNext -> showUpNext(action.v1)
}

markers.skip()?.let { seek(it) } // the button, pressed
markers.undoSkip()?.let { seek(it) } // Undo, pressed
```

```swift
let markers = playback.followMarkers(hasNext: false, autoSkip: true, upNextCard: true)

for action in markers.onPosition(at: time, length: duration) {
    switch action {
    case .autoSkip(let to): seek(to: to); showUndo(for: undoWindow())
    case .offer(let offer): button = offer // .skipIntro, .skipCredits, .next, or nil
    case .upNext(let shows): upNextShown = shows
    }
}

if let to = markers.skip() { seek(to: to) } // the button, pressed
if let back = markers.undoSkip() { seek(to: back) } // Undo, pressed
```

`skip()` gives the target of the skip button showing, and `None` for Next,
which is the app's to play. Only an intro skipped by itself can be undone,
for `undo_window()` (5 s); `undoable()` tells a view that comes later
whether to show Undo. An intro is skipped by itself once, only with the
length known and where a seek has landed, so call `seek_started` for a
resume. `set_has_next` when the next episode is found later, and
`set_auto_skip` when the setting changes. `position(at)` is for a scrubber.
Once its playback is over it gives no skip or undo, and it never keeps a
playback open.

The screen's mode is a planning input, not a sign-in one: a TV switches
between a 1080p UI mode and a 4K mode for the film, and the sign-in is
sent once. `DeviceSpec.capabilities` is the decoders' limits only. Say
the screen as it is now with `SpoolClient.set_display(ScreenCapabilities)`
(Swift and Kotlin `setDisplay`) once after `open` and again on each
change; every plan, open and re-plan that names no display of its own
sends it, so a mode switch counts from the next plan. `None` forgets it.

A setting between HLS and progressive goes with each play: `delivery` on
`open_playback` and `play` (`ConversionDelivery.HLS` or `.PROGRESSIVE` in
Kotlin, `.hls` or `.progressive` in Swift). It holds for that playback,
through its re-plans and restarts, and the next play reads the setting
again; left out, or `None`, the first way `DeviceSpec.capabilities` lists
applies. It is about conversions: a file the device plays is still played
as it is, and the rest of `DeviceSpec.capabilities` stays. `plan` has no
`delivery`, so the plan it shows is by the device's own order.

```kotlin
val delivery = if (settings.progressive) ConversionDelivery.PROGRESSIVE
    else ConversionDelivery.HLS
val playback = session.openPlayback(id, start, tracks, quality, null, delivery = delivery)
```

```swift
let delivery: ConversionDelivery = settings.progressive ? .progressive : .hls
let playback = try await session.openPlayback(
    id: id, start: start, tracks: tracks, quality: quality, display: nil, delivery: delivery)
```

A setting that changes the sound the device takes, such as Passthrough,
goes with each play too: `audio` on `open_playback` and `play`, an
`AudioOutput`. Its `codecs` replace `DeviceSpec.capabilities`' audio list
for that playback, and `max_channels` replaces its `max_audio_channels`,
which the server holds every codec to; `None` keeps the device's. So a
codec a receiver takes but the device can't decode needs its channels
said too. A file whose audio isn't listed is converted, where the device's
list would have played it; a codec added is copied. It holds through
re-plans and restarts and goes with `delivery`. Left out, or `None`, the
device's list applies. An empty `codecs` means no codec, not the device's:
a file with sound is `CannotPlay`, and one without plays. Nothing is added
to the list, `Pcm` included, so start from the device's. `plan` has no
`audio`, so the plan it shows is by the device's own list.

```kotlin
val audio = when (settings.passthrough) {
    Passthrough.ALWAYS -> AudioOutput(
        capabilities.audio + listOf(AudioCodec.EAC3, AudioCodec.TRUE_HD),
        maxChannels = 8u,
    )
    Passthrough.NEVER -> AudioOutput(decoded) // what the sink itself decodes
    Passthrough.AUTO -> null
}
val playback = session.openPlayback(id, start, tracks, quality, null, audio = audio)
```

```swift
let audio: AudioOutput?
switch settings.passthrough {
case .always:
    audio = AudioOutput(codecs: capabilities.audio + [.eac3, .trueHd], maxChannels: 8)
case .never:
    audio = AudioOutput(codecs: decoded) // what the sink itself decodes
case .auto:
    audio = nil
}
let playback = try await session.openPlayback(
    id: id, start: start, tracks: tracks, quality: quality, display: nil, audio: audio)
```

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
go; a progressive conversion is read in order, and `seek` is how it
moves. A conversion's HLS is read by name instead: `open_resource(path)`
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

For a diagnostics overlay, show `SpoolPlayback.wan()` first: `true`, the
server counts this device off its LAN; `false`, it does not; `nil` or
`null`, an older server, which does not say. It is the server's verdict,
kept current across re-plans, and it can differ from
`SpoolSession.connection_path()`, which is only the address as this
device sees it: `Lan` is any private address, a WireGuard peer the
server lists in `network.wan_prefixes` too. Show `connection_path()` for
how the packets go (the address, a relay). On a proxied play `wan()` is
the user's own server's, of this device's link to it.

```kotlin
val link = when (playback.wan()) {
    true -> "WAN"
    false -> "LAN"
    null -> "not told"
}
```

```swift
let link = playback.wan().map { $0 ? "WAN" : "LAN" } ?? "not told"
```

## From the Coldsteel registry

During development, take this crate and the SDK crates under it from the
Coldsteel registry, as the SDK's README says ("Taking the crates from the
Coldsteel registry"): patch each spoolframe crate, this one too, to its
Coldsteel copy:

```toml
[patch.crates-io]
spoolframe-client-ffi = { version = "=0.17.2", registry = "coldsteel" }
# and spoolframe-types, -protocol, -policy, -client-sdk, -hls-bridge
```

To try a PR before it merges, take the build of its commit. It needs no
`[patch]` (the SDK's README, "Taking a PR build"):

```toml
[dependencies]
spoolframe-client-ffi = { version = "=0.17.2-gab12d3b", registry = "coldsteel" }
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
the Swift as one module (on Linux), each with an app's use of
`SpoolMarkers` and `seek`; elsewhere those two tests say so and pass.
