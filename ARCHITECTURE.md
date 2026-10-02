# Spoolframe Architecture Decisions

_Status: early architecture / MVP direction_

Spoolframe is a self-hosted media system focused on **movies, TV shows, and music**. The initial goal is not to replace every Plex feature. The first priority is to replace the parts of Plex that are most painful for playback and streaming while keeping the rest of the system deliberately small.

The architecture is designed so that Plex can be removed later without changing the client-facing library model or the transport model.

---

## 1. Scope

### In scope

- Movies
- TV shows
- Music
- Linux client as a first-class client
- tvOS client as a first-class client
- Web UI primarily for administration/debugging
- Direct play
- Remux
- Transcoding
- Local users
- Multiple users per server
- Multiple accounts from the same client/device
- Multiple devices per account
- Remote sharing
- Portable user identities
- Server-to-server federation
- Operation without a centralized identity provider
- LAN operation even when the public Internet is unavailable

### Explicitly not required for the first version

- Photos
- Live TV / DVR
- Podcasts
- Plex-compatible client protocol
- Global Spoolframe account service
- Centralized cloud discovery as a hard dependency
- Reimplementation of media matching/indexing before playback works

---

# 2. MVP Philosophy

The first version does **not** replace Plex library management.

Plex acts as a temporary **metadata and library control plane**:

- scanning
- media matching
- movie/show/music metadata
- posters and artwork
- library structure
- seasons / episodes
- artists / albums / tracks
- existing watched state where useful

Spoolframe owns:

- client-facing API
- user authentication
- users and sessions
- playback state for Spoolframe users
- media probing
- capability negotiation
- direct streaming
- remux
- transcoding
- federation
- remote sharing

The key rule is:

> Plex tells Spoolframe **what the media is**. Spoolframe decides **how it plays**.

---

# 3. High-Level Architecture

```text
                         ┌──────────────────────┐
                         │       Plex PMS       │
                         │                      │
                         │ libraries / metadata │
                         │ artwork / matching   │
                         └──────────┬───────────┘
                                    │
                                    │ metadata only
                                    ▼
┌─────────────────────────────────────────────────────────────┐
│                      Spoolframe Server                      │
│                                                             │
│  PlexCatalog                                                │
│  User/Auth                                                  │
│  Sessions                                                   │
│  Watch state                                                │
│  Federation                                                 │
│  Playback planner                                           │
│  ffprobe                                                    │
│  ffmpeg                                                     │
│  Direct / Remux / Transcode                                 │
└──────────────┬───────────────────┬──────────────────────────┘
               │                   │
         Iroh client API      Federation over Iroh
               │                   │
       ┌───────┴────────┐          │
       │                │          │
       ▼                ▼          ▼
 Linux / Relm4       tvOS       Other servers
 GStreamer           AVPlayer
```

Plex is **not** in the media data path.

---

# 4. Plex as the Initial Library Manager

For the MVP, Spoolframe does not need to import the Plex database into a native catalog.

Instead it has a `PlexCatalog` implementation behind Spoolframe's own internal catalog interface.

```text
Client
  │
  ▼
Spoolframe API
  │
  ▼
Catalog trait
  │
  └── PlexCatalog
         │
         ▼
       Plex PMS
```

Clients never receive raw Plex-specific semantics as their public API.

A future native catalog can replace Plex:

```text
Today:

Catalog
  └── PlexCatalog

Later:

Catalog
  └── NativeCatalog
```

The Linux and tvOS clients should not need major changes when this happens.

---

# 5. Public Media Model Belongs to Spoolframe

Even while Plex is the backing metadata source, public models are Spoolframe models.

Example:

```rust
struct Movie {
    id: MediaId,
    title: String,
    year: Option<u16>,
    summary: Option<String>,
    artwork: Artwork,
    playback: PlaybackState,
}

struct Episode {
    id: MediaId,
    show_id: MediaId,
    season: u32,
    episode: u32,
    title: String,
    artwork: Artwork,
    playback: PlaybackState,
}
```

Plex identifiers are an implementation detail.

For the first version, IDs may internally map directly to Plex `ratingKey`s, but the client-facing ID format should be namespaced/opaque enough that Plex-specific assumptions do not leak into client code.

Example:

```text
plex:movie:12345
plex:episode:98321
```

Later these can become fully native Spoolframe IDs.

---

# 6. Playback Is Fully Spoolframe-Owned

Plex is not trusted to decide whether something should direct-play or transcode.

Spoolframe resolves the media file, then performs its own probe:

```text
Plex item
   │
   ▼
Resolve media path
   │
   ▼
ffprobe
   │
   ▼
MediaDescriptor
   │
   ▼
PlaybackPlanner(client capabilities)
   │
   ├── Direct
   ├── Remux
   └── Transcode
```

Example media description:

```rust
struct MediaDescriptor {
    container: Container,
    duration: Duration,
    video: Option<VideoStream>,
    audio: Vec<AudioStream>,
    subtitles: Vec<SubtitleStream>,
}
```

Example result:

```rust
enum PlaybackPlan {
    Direct { /* source details */ },
    Remux { /* output/manifest details */ },
    Transcode { /* output/manifest details */ },
}
```

---

# 7. Shared Storage / Plex Path Mapping

Spoolframe and Plex may see the same files at different paths.

Example:

```text
Plex:
    /data/movies/Alien/Alien.mkv

Spoolframe:
    /media/movies/Alien/Alien.mkv
```

Configuration:

```ucl
plex {
  path_mapping [
    { from = "/data/movies"; to = "/media/movies"; },
  ]
}
```

Spoolframe must always probe and serve the local filesystem path it can access.

---

# 8. Client Capability Negotiation

Playback decisions are based on client capabilities, not hard-coded client names.

Example conceptual capability set:

```json
{
  "video": ["h264", "hevc", "av1"],
  "audio": ["aac", "ac3", "eac3"],
  "containers": ["mp4", "mov"],
  "streaming": ["hls"],
  "subtitles": ["webvtt"]
}
```

Linux/GStreamer may advertise a very broad capability set.

tvOS/AVPlayer will advertise Apple-specific capabilities.

The server should avoid transcoding unless necessary.

---

# 9. Linux Is a First-Class Client

The Linux client is not a temporary test UI.

Chosen stack:

- Rust
- Relm4
- GTK4
- GStreamer

Architecture:

```text
┌──────────────────────────────────────┐
│          Spoolframe Linux            │
│                                      │
│  Relm4 / GTK4                        │
│  library / search / details          │
│  controls / OSD                      │
│                                      │
│  GStreamer player                    │
│  appsrc/custom source                │
│           │                          │
│           ▼                          │
│         Iroh                         │
└───────────┬──────────────────────────┘
            │
            ▼
      Spoolframe Server
```

GStreamer is preferred over launching mpv because Spoolframe needs visibility into:

- buffering
- decoder selection
- hardware acceleration
- stream changes
- seeking
- timestamps
- HLS behavior
- audio/subtitle handling

---

# 10. tvOS Is Also First-Class

tvOS is a native client.

Likely stack:

- Swift / SwiftUI
- AVPlayer
- thin Rust/UniFFI layer for Spoolframe networking if necessary

Iroh is intended to be used by the tvOS client as the lower transport layer too.

The main integration problem is not authentication; it is feeding media from Iroh into AVPlayer.

Two approaches are possible:

## Option A — AVAssetResourceLoader

```text
AVPlayer
   │
AVURLAsset
   │
AVAssetResourceLoader
   │
Swift/Rust bridge
   │
Iroh
   │
Spoolframe Server
```

## Option B — Local loopback HTTP bridge

Likely the simpler initial path:

```text
AVPlayer
   │
   │ http://127.0.0.1:<port>/...
   ▼
tiny local HTTP server
   │
   ▼
Spoolframe Rust networking
   │
   ▼
Iroh
   │
   ▼
Spoolframe Server
```

The localhost HTTP server only exists inside the app/device.

The public network path remains Iroh.

---

# 11. Iroh Is the Lower-Layer Networking Substrate

Spoolframe uses Iroh because it provides:

- cryptographic endpoint identity
- encrypted QUIC connections
- authenticated peer identity
- NAT traversal
- direct connections where possible
- optional relays
- address discovery
- local-network discovery
- no dependency on public certificate authorities
- no dependency on a centralized Spoolframe account service

The intended resilience property is:

> If the public Internet disappears but a LAN survives, existing Spoolframe servers and clients can still discover/connect/play media.

---

# 12. Core Iroh Terminology

## `SecretKey`

Persistent private key for one Iroh endpoint.

It must remain private.

## `EndpointId`

Public identity derived from the `SecretKey`.

Think of this as a cryptographic installation/device/server identity.

```text
SecretKey
   │
   ▼
EndpointId
```

## `Endpoint`

A running Iroh networking instance.

Normally one per application installation/process.

## `EndpointAddr`

An `EndpointId` plus current ways to reach it.

Example:

```text
EndpointId:
    abc123...

Addresses:
    192.168.1.50:43123
    public-ip:43123
    optional relay
```

Identity and address are intentionally separate.

## `Connection`

An authenticated encrypted QUIC connection between two endpoints.

## QUIC stream

An independent logical stream inside one connection.

A single connection can carry many streams simultaneously.

## ALPN

Identifies which Spoolframe protocol is being spoken over an Iroh connection.

Planned:

```text
spoolframe/client/1
spoolframe/federation/1
```

## Address lookup

Maps an `EndpointId` to possible locations.

Possible sources:

- mDNS / LAN discovery
- saved addresses
- decentralized lookup
- optional relays/services
- explicit invite/ticket information

Address lookup is discovery, not authentication.

The Iroh connection still proves the remote endpoint key.

---

# 13. Iroh Identity Is Not User Identity

This is fundamental.

```text
Iroh EndpointId
    = device / installation / server identity

Spoolframe UserId
    = human account

Spoolframe Session
    = one user authenticated from one endpoint
```

Never equate:

```text
EndpointId == UserId
```

---

# 14. One Endpoint Per Client Installation

A client should normally have one persistent Iroh identity.

Example:

```text
Spoolframe Server     Endpoint S
Apple TV              Endpoint T
Linux desktop         Endpoint L
Friend's Linux PC     Endpoint F
```

Network graph:

```text
                    Server S
                  /    |    \
                 /     |     \
                T      L      F
            AppleTV  Linux  Friend PC
```

These are machines/installations, not accounts.

---

# 15. Multiple Accounts on One Client

A shared Apple TV has one Iroh endpoint:

```text
Endpoint T
```

but may have multiple Spoolframe sessions:

```text
Endpoint T
   ├── Session A → Andrey
   └── Session B → Alice
```

The same underlying Iroh connection may carry requests for different sessions.

Example:

```text
QUIC connection T ↔ S

stream 1:
    session = Andrey
    GetHome

stream 2:
    session = Alice
    GetWatchHistory

stream 3:
    session = Andrey
    StartPlayback(...)
```

No network reconnect is required when switching profiles.

---

# 16. One Account on Multiple Devices

The inverse is also valid.

```text
Andrey
   ├── Apple TV T
   ├── Linux L
   ├── Laptop L2
   └── Phone P
```

Therefore the user/device relationship is many-to-many.

The useful conceptual relationship is:

```text
User ←── Session ──→ Endpoint
```

---

# 17. Fresh Client Installation

A fresh tvOS or Linux client:

1. Generates an Iroh `SecretKey`.
2. Persists it locally.
3. Derives its `EndpointId`.
4. Discovers or imports a server.
5. Connects to the expected server `EndpointId`.
6. Establishes an encrypted Iroh connection.
7. Authenticates a Spoolframe user inside that connection.

Example:

```text
Fresh Apple TV
      │
      ├── generate SecretKey T
      ├── EndpointId T
      │
      ▼
discover Server S
      │
      ▼
Iroh connect(expected = S)
      │
      ▼
encrypted/authenticated endpoint connection
      │
      ▼
user login
```

---

# 18. First-Contact Server Discovery

## LAN

Use mDNS/Bonjour-style discovery.

Conceptually:

```text
_spoolframe._udp.local

name       = "Home Media"
endpoint   = S
protocol   = 1
address    = 192.168.1.20:...
```

The client finds the server and learns its `EndpointId`.

mDNS discovery is untrusted.

Iroh proves whether the contacted machine actually owns `EndpointId S`.

## Remote

Use an invite/ticket containing at least:

```text
Server EndpointId
known addresses / connection hints
optional one-time invite capability
```

This can be encoded as:

- QR code
- deep link
- copy/paste string

---

# 19. First-Contact Trust

For LAN servers, Spoolframe may use TOFU:

```text
user selects "Home Media"
        │
        ▼
connect to Endpoint S
        │
        ▼
pin S permanently for that saved server
```

For stronger verification, support a short pairing/fingerprint confirmation.

Remote invitations should carry the expected server `EndpointId`, avoiding blind TOFU.

---

# 20. Iroh Connections May Be Anonymous at the Spoolframe Layer

A server may accept an Iroh connection from an unknown `EndpointId`.

The connection is already cryptographically authenticated as that endpoint, but it has no Spoolframe authority yet.

Conceptual state:

```rust
enum ConnectionAuth {
    Anonymous {
        endpoint: EndpointId,
    },

    User {
        endpoint: EndpointId,
        user: UserId,
        session: SessionId,
    },

    FederationPeer {
        endpoint: EndpointId,
        peer: ServerId,
    },
}
```

Anonymous client connections get only a tiny pre-auth surface:

```text
ServerInfo
AuthMethods
LoginPassword
LoginPasskey...
RedeemInvite
PairDevice
Ping
```

They do not get:

```text
GetLibraries
GetMovie
StartPlayback
UpdateWatchState
Admin APIs
```

---

# 21. Local Users Belong to Spoolframe

Plex is not the user database.

Spoolframe has its own local accounts.

Example conceptual schema:

```text
users
    id
    username
    display_name
    password_hash

devices
    id
    endpoint_id
    display_name
    last_seen

sessions
    id
    user_id
    device_id
    created_at
    expires_at
    revoked_at
```

Later:

```text
library_grants
watch_state
preferences
passkeys
trusted_endpoints
federation_grants
```

---

# 22. Password Authentication

Passwords are normal application credentials.

They are transmitted only inside the already encrypted Iroh connection.

Server stores a strong password hash, e.g. Argon2id.

Flow:

```text
Client Endpoint T
       │
       │ encrypted Iroh connection
       ▼
Server Endpoint S

Login {
    username,
    password
}
       │
       ▼
Argon2id verification
       │
       ▼
Session created
```

---

# 23. Passkeys

Standard WebAuthn/passkeys are supported where practical while normal Internet/domain infrastructure exists.

However, passkeys are not the fundamental decentralized identity mechanism because WebAuthn relies on a relying-party domain model.

Therefore Spoolframe retains:

- username/password
- trusted-device authentication
- explicit device pairing

as independent fallback mechanisms.

Hardware security keys may continue to work locally where the WebAuthn/RP setup remains valid, but Spoolframe must not depend on that for offline survival.

---

# 24. Trusted Iroh Devices

After a successful login, a user may choose:

```text
Remember / trust this device
```

Server stores:

```text
trusted_user_endpoints

user_id
endpoint_id
```

Example:

```text
Andrey → Apple TV T
Alice  → Apple TV T
```

On future app launches, Iroh proves that the connecting endpoint is still `T`.

The server can then offer:

```text
Who's watching?

[ Andrey ] [ Alice ]
```

without asking for the password again.

A trusted endpoint is not itself the user.

It is simply authorized to re-establish a session for that user.

---


# 24A. OIDC Authentication

OIDC is supported as an optional authentication method.

OIDC does **not** replace Spoolframe's local user model.

The identity model remains:

```text
Iroh EndpointId
    = device / installation identity

Spoolframe UserId
    = local human account

Authentication method
    = one way to prove control of that local account
```

OIDC therefore maps an external identity onto an existing or newly-created local Spoolframe user.

Conceptually:

```text
Iroh connection
     │
     ▼
Spoolframe auth
     │
     ├── password
     ├── passkey
     ├── trusted Iroh endpoint
     └── OIDC
            │
            ▼
       local UserId
            │
            ▼
     Spoolframe Session
```

The stable external identity key is:

```text
(issuer, subject)
```

not email address.

Example:

```text
issuer  = https://accounts.google.com
subject = 108349857...

        │
        ▼

Spoolframe UserId = 42
```

Possible schema:

```text
external_identities
    user_id
    issuer
    subject
    email
    display_name
```

`email` and `display_name` are informational and may change.

`issuer + subject` is the stable binding.

---

# 24B. OIDC Session Creation

OIDC only changes the authentication step.

After successful OIDC authentication:

```text
OIDC identity
     │
     ▼
resolve local UserId
     │
     ▼
create Spoolframe Session
     │
     ▼
bind Session to authenticated Iroh EndpointId
```

Example:

```text
Session {
    user     = UserId(42)
    endpoint = EndpointId(T)
}
```

All normal Spoolframe authorization then applies.

Playback, watched state, ACLs, library grants, federation, and client behavior do not care whether the session originated from:

- password
- passkey
- OIDC
- trusted endpoint
- device approval

---

# 24C. OIDC on Linux

Linux clients should use the normal OIDC Authorization Code flow with PKCE.

Conceptually:

```text
Spoolframe Linux client
     │
     │ open system browser
     ▼
OIDC provider
     │
     │ authenticate
     ▼
localhost callback / custom callback URI
     │
     ▼
Linux client
     │
     │ complete auth through Spoolframe
     ▼
Spoolframe server
     │
     ▼
local UserId
     │
     ▼
Session bound to Linux EndpointId
```

The Spoolframe server validates the configured OIDC issuer and maps the result to a local user.

Example configuration:

```ucl
auth {
  oidc [
    {
      name = "Google";
      issuer = "https://accounts.google.com";
      client_id = "...";
    },
    {
      name = "Authentik";
      issuer = "https://auth.example.com/application/o/spoolframe/";
      client_id = "...";
    },
  ]
}
```

Self-hosted OIDC providers such as Authentik, Keycloak, Kanidm, Zitadel, or similar systems should be usable without affecting the rest of the architecture.

---

# 24D. OIDC on tvOS

tvOS should avoid requiring users to type long OIDC credentials with the remote.

Preferred flow:

```text
Apple TV
    │
    ▼
"Sign in with <provider>"

QR code / short code
    │
    ▼
phone or browser
    │
    ▼
Spoolframe server
    │
    ▼
OIDC provider
    │
    ▼
authentication succeeds
    │
    ▼
pending Apple TV login approved
    │
    ▼
Session created for Apple TV EndpointId
```

The tvOS client does not need to expose the full provider login UI itself.

Spoolframe can act as the OIDC relying party while the Apple TV participates in a device-style login flow.

---

# 24E. OIDC Account Linking

OIDC identities should be explicitly linked to local Spoolframe accounts.

Example:

```text
UserId 1: Andrey
    ├── password
    ├── passkey #1
    ├── passkey #2
    ├── trusted endpoint T
    ├── OIDC Google    (issuer, subject)
    └── OIDC Authentik (issuer, subject)
```

Do not automatically merge accounts based only on matching email addresses.

A logged-in user should be able to link an OIDC identity from account settings.

Conceptual UI:

```text
Authentication

Password            configured
Passkeys             2
Trusted devices      3
Google               linked
Authentik            [Link]
```

For invited/new users, server policy may optionally allow:

```text
invite
   │
   ▼
authenticate with OIDC
   │
   ▼
create local Spoolframe user
```

This should be an explicit server policy rather than an unconditional behavior.

---

# 24F. OIDC and Resilience

OIDC is a convenience authentication method, not a dependency of the core system.

Expected behavior:

```text
Internet available:

OIDC                 yes
Passkey              yes
Password             yes
Trusted endpoint     yes


Internet unavailable:

OIDC                 possibly unavailable
Passkey              depends on platform/RP setup
Password             yes
Trusted endpoint     yes
```

Previously trusted devices continue to authenticate using their Iroh `EndpointId` even if the OIDC provider is unreachable.

Therefore:

> OIDC may improve login convenience while infrastructure exists, but Spoolframe must remain usable without it.

Passwords and trusted Iroh endpoints remain durable fallback mechanisms.


# 25. Sessions Are Bound to EndpointId

A Spoolframe session should not be a globally reusable bearer token.

Conceptually:

```text
Session A:
    user     = Andrey
    endpoint = T
```

On every request:

```text
session.endpoint == connection.remote_endpoint_id
```

must hold.

If an attacker steals only the session ID but connects as endpoint `X`, the session is rejected.

Effective credential:

```text
(SessionId, authenticated EndpointId)
```

---

# 26. Federation

Federation is server-to-server.

It is primarily for:

- sharing library metadata
- grants
- remote-library discovery
- metadata updates
- reconciliation
- remote playback authorization

It is not intended to proxy media through the recipient's server.

---

# 27. Federation Identity

Each Spoolframe server has a long-lived Iroh endpoint key.

```text
Server A
    EndpointId SA

Server B
    EndpointId SB
```

The `EndpointId` is the canonical cryptographic server identity.

DNS/domain names may exist as aliases/convenience but are not the root of identity.

The Internet is only one possible discovery/transport environment.

---

# 28. Federation Protocol

Planned ALPN:

```text
spoolframe/federation/1
```

Federation connections should be stricter than ordinary client connections.

```text
incoming Iroh connection
        │
        ▼
which ALPN?
        │
   ┌────┴───────────────┐
   │                    │
client/1          federation/1
   │                    │
unknown endpoint     known peer?
allowed pre-auth       │
                    or valid invite?
```

Unknown clients may reach login.

Unknown federation peers may only reach pairing/invite flows.

---

# 29. Federation Is Metadata Replication, Not Media Proxying

Example:

```text
Alice Server SA
      │
      │ metadata
      ▼
Bob Server SB

Bob caches:
    titles
    posters
    descriptions
    hierarchy
    remote item IDs
    origin endpoint
```

Bob's client can browse this metadata locally.

When Bob plays an item:

```text
Bob client
    │
    │ obtain playback authorization
    ▼
Origin Server SA
```

The actual media should travel directly from the origin server to the client where practical.

---

# 30. Federated Principal Identity

A globally meaningful Spoolframe user identity can be represented by:

```rust
struct PrincipalId {
    server: EndpointId,
    user: UserId,
}
```

Example:

```text
(SA, Alice)
(SB, Bob)
```

No DNS-based global account service is required.

---

# 31. Relays Are Optional Routing Infrastructure

Relays are not identity providers.

A relay may help two endpoints connect through NAT or forward encrypted packets when direct connectivity is unavailable.

Security relationship remains:

```text
Client C ↔ Server S
```

not:

```text
Client C ↔ Relay ↔ Server S
```

The relay must not be the authority for user or server identity.

A self-hosted relay may be supported.

If all relays disappear but direct LAN connectivity exists, Spoolframe should still function.

---

# 32. LAN-Survival Requirement

Spoolframe should continue operating when all of the following are unavailable:

- public DNS
- public CA infrastructure
- Let's Encrypt
- Spoolframe-operated services
- Number 0 services
- Plex cloud account services
- public Internet connectivity

As long as:

- a Spoolframe server exists
- clients exist
- they share an IP network
- their persisted endpoint keys still exist

the system should be able to discover/connect/authenticate/play media locally.

Example:

```text
Internet gone
DNS gone
public CAs gone
relays gone

        │
        ▼

mDNS / saved address
        │
        ▼
EndpointId S discovered
        │
        ▼
Iroh verifies S
        │
        ▼
Client endpoint already trusted
        │
        ▼
choose local Spoolframe user
        │
        ▼
play media
```

---

# 33. Media Transport Over Iroh

Native Spoolframe clients should use Iroh for client/server communication.

Potential protocol split:

```text
spoolframe/client/1
    auth
    catalog
    search
    playback negotiation
    user state
    events
    media requests

spoolframe/federation/1
    federation
```

The initial design does not require separate ALPNs for control and media.

One Iroh connection can carry many QUIC streams:

```text
Connection Client ↔ Server
    ├── RPC stream
    ├── event stream
    ├── artwork stream
    └── media stream
```

Separate connections may later be introduced if congestion-control behavior makes that useful.

---

# 34. Seeking / Media Read Semantics

Media streaming over Iroh should not be designed as only one endless byte stream.

The protocol should support range-like semantics.

Example:

```text
OpenMedia {
    media_id
}

Read {
    offset
    length
}
```

or equivalent stream-opening requests.

This allows:

- seeking
- random access
- resumable playback
- player buffering
- efficient large-file handling

For transcoded/HLS-like playback, the protocol may expose manifests/segments as logical resources.

---

# 35. Web UI

The web UI is useful, but it is not the primary desktop client.

Primary purposes:

- admin
- server configuration
- inspect Plex integration
- inspect matching/path resolution
- inspect users/devices/sessions
- inspect federation
- debug playback
- basic browsing

Native Linux and tvOS remain first-class playback clients.

Because browser-native Iroh support is less straightforward, the web UI may use a conventional HTTP interface locally or through a dedicated gateway.

---

# 36. Watched State

Spoolframe should own watched/progress state for Spoolframe users.

This is required because multiple local Spoolframe users may share one Plex-backed catalog.

Example:

```text
Plex metadata item: Alien

Spoolframe:
    Andrey → watched
    Alice  → 37 minutes
    Steve  → unwatched
```

Plex cannot represent these users cleanly if Plex is only being used as the metadata backend.

For the owner's existing Plex account, Spoolframe may optionally mirror watched/progress state back to Plex during the transition period.

---

# 37. Initial Development Order

## Phase 1 — Server + Plex catalog

- connect to Plex
- browse libraries
- normalize movies/shows/music into Spoolframe models
- resolve media file paths
- path mappings
- artwork proxying/caching as needed

## Phase 2 — Playback core

- ffprobe
- media descriptor
- client capability negotiation
- direct play
- remux
- transcode
- seeking
- playback sessions

## Phase 3 — Linux first-class client

- Relm4
- GTK4
- GStreamer
- Iroh client transport
- user login
- profile switching
- watched/progress state

## Phase 4 — Local users

- password auth
- passkeys where practical
- OIDC
- account linking
- trusted endpoints
- multiple accounts per device
- multiple devices per account
- device/session management

## Phase 5 — tvOS

- SwiftUI
- Iroh transport through Rust/FFI
- AVPlayer bridge
- local loopback HTTP or resource loader
- profile switching

## Phase 6 — Federation

- server identity
- server pairing
- grants
- metadata replication
- remote-library browsing
- playback authorization

## Phase 7 — Remove Plex dependency

Replace:

```text
PlexCatalog
```

with:

```text
NativeCatalog
```

while preserving the client-facing protocol and federation model.

---

# 38. Architectural Rules

These are the decisions that should remain stable unless there is a strong reason to revisit them.

1. **Plex is temporary metadata/library infrastructure, not the streaming engine.**
2. **Spoolframe always runs its own ffprobe and ffmpeg logic.**
3. **Clients see Spoolframe models, never Plex models.**
4. **Linux is a first-class client.**
5. **tvOS is a first-class client.**
6. **Iroh is the lower-layer native transport and cryptographic endpoint identity.**
7. **Iroh endpoint identity is not user identity.**
8. **One client installation normally has one Iroh endpoint.**
9. **A single endpoint may have sessions for multiple users.**
10. **A single user may use multiple endpoints.**
11. **Passwords, passkeys, and OIDC belong to Spoolframe auth, not Iroh. OIDC maps external identities onto local Spoolframe users.**
12. **Trusted Iroh endpoints provide durable passwordless device authentication.**
13. **Sessions are bound to authenticated Iroh EndpointIds.**
14. **Servers have stable Iroh EndpointIds.**
15. **Federation uses server endpoint identities, not global accounts or DNS as the root identity.**
16. **Federation shares metadata and authorization; it does not require proxying the media path.**
17. **Relays are optional transport infrastructure, never identity authorities.**
18. **The system must continue to function on a surviving LAN without public Internet infrastructure.**
19. **The eventual removal of Plex must not require redesigning clients or federation.**

---

# 39. Core Mental Model

```text
                         SPOOLFRAME

┌────────────────────────────────────────────────────────────┐
│                    APPLICATION LAYER                       │
│                                                            │
│  Users                                                     │
│  Passwords / passkeys / OIDC                               │
│  Sessions                                                  │
│  Trusted devices                                           │
│  Watch state                                               │
│  Library ACLs                                              │
│  Federation grants                                         │
│  PlexCatalog / future NativeCatalog                        │
├────────────────────────────────────────────────────────────┤
│                    SPOOLFRAME PROTOCOL                     │
│                                                            │
│  spoolframe/client/1                                       │
│  spoolframe/federation/1                                   │
├────────────────────────────────────────────────────────────┤
│                          IROH                              │
│                                                            │
│  EndpointId                                                │
│  peer authentication                                       │
│  encryption                                                │
│  QUIC                                                      │
│  streams                                                   │
│  discovery                                                 │
│  NAT traversal                                             │
│  optional relays                                           │
├────────────────────────────────────────────────────────────┤
│                       IP NETWORK                           │
│                                                            │
│  LAN / Internet / whatever remains reachable               │
└────────────────────────────────────────────────────────────┘
```

The shortest description of the architecture is:

> **Iroh authenticates endpoints. Spoolframe authenticates users. Sessions bind users to endpoints. Plex supplies metadata for now. Spoolframe owns playback and eventually the whole library.**

# 40. Device Login Policies

Once a user has authenticated on a device, that user may explicitly trust the device.

Trust is represented by the pair:

```text
(UserId, EndpointId)
```

A trusted device may then use one of several login policies:

```rust
enum DeviceLoginPolicy {
    RequireFullLogin,
    Remembered,
    PinProtected,
    AutoLogin,
}
```

Semantics:

```text
RequireFullLogin
    password / passkey / OIDC required again

Remembered
    show profile picker
    no full login required

PinProtected
    show profile picker
    require a user PIN

AutoLogin
    immediately create a new session for one trusted user
```

Auto-login is allowed only after the endpoint has already been trusted for that user.

```text
Iroh connects as Endpoint T
        │
        ▼
server recognizes T
        │
        ▼
T is trusted for User A
        │
        ▼
device policy says AutoLogin(User A)
        │
        ▼
create fresh Session for User A
```

PIN protection is an additional user/device gate, not the primary cryptographic identity. A PIN should be valid only in combination with the already-authenticated trusted endpoint.

```text
Endpoint T + correct PIN
    → allow

Endpoint X + same PIN
    → reject
```

PIN verification should be server-side and rate-limited. Auto-login should be a device-level choice with at most one default user per device. PIN/remembered policy is naturally per `(UserId, EndpointId)`.

---

# 41. NAT Traversal and Remote Reachability

Spoolframe should rely on Iroh for NAT traversal rather than implementing its own STUN/TURN/DERP logic.

Expected path:

```text
Server S                         Client F
behind NAT                       behind NAT
    │                                │
    └──────────▶ Iroh Relay ◀────────┘
                    │
             rendezvous/fallback
                    │
             exchange candidates
                    │
              UDP hole punching
                    │
                    ▼
Server S ═════════════════════ Client F
                  direct
```

Preferred order:

```text
direct LAN/IP
    ↑
UDP hole punching
    ↑
Iroh relay fallback
```

Relays are optional infrastructure and are not identity authorities. Spoolframe should use Iroh's relay protocol/implementation rather than assume compatibility with arbitrary Tailscale DERP servers.

Spoolframe should support automatic relays, custom/self-hosted relays, and relays disabled. For media workloads, direct connectivity is especially desirable because sustained relay traffic is bandwidth-expensive. If direct connectivity is impossible, relayed media remains a valid fallback.

If the Internet and all relays disappear but a LAN remains reachable, direct Iroh connections must continue to work.

---

# 42. Portable User Identities

Spoolframe supports a second user model in addition to server-local users:

```text
LocalUser
    account hosted by a Spoolframe server

PortableUser
    cryptographic identity owned directly by the user
```

A portable identity exists independently of any server. It is intended for users who want access to shared libraries without requiring somebody else to host their account.

Example:

```text
Steve = Portable User U
```

Steve can authenticate to multiple Spoolframe servers as the same portable identity.

---

# 43. Portable Identity Root

A portable user identity is rooted in a long-lived signing key.

```text
UserRootSecret
       │
       ▼
UserRootPublicKey
       │
       ▼
PortableUserId
```

The user owns the root secret. The root key should be signing-only and is not copied to every playback device. Instead it authorizes per-device credentials.

---

# 44. Phone as Portable Account Holder

For simplicity, the initial portable-user UX may designate the phone app as the account holder/root-authority device.

```text
Steve's phone
    │
    ├── stores UserRootSecret
    ├── owns PortableUserId U
    ├── authorizes new devices
    ├── revokes devices
    └── manages recovery
```

The phone is not an always-online account server. It is merely the device that normally holds the user's root authority. Other devices continue authenticating without the phone being online once they have their own signed credentials.

---

# 45. Portable User Device Credentials

Each portable-user device gets its own authentication keypair.

```text
                    Steve U
                 root authority
                /      |      \
               /       |       \
          Linux K1   TV K2   Phone K3
```

The root signs a credential binding a device key to the portable user.

```rust
struct UserDeviceCredential {
    user: PortableUserId,
    device_public_key: PublicKey,
    serial: u64,
    issued_at: Timestamp,
    expires_at: Option<Timestamp>,
    signature_by_user_root: Signature,
}
```

Authentication to a Spoolframe server proves:

1. possession of the device private key;
2. possession of a valid root-signed device credential;
3. the portable user identity named by that credential.

The server does not need to contact an account server to verify this chain.

---

# 46. Portable User Recovery

The root identity should support offline recovery. The first implementation may use a mnemonic/seed-phrase style recovery mechanism similar in UX to a cryptocurrency wallet.

```text
recovery phrase
      │
      ▼
recover root secret
      │
      ▼
same root public key
      │
      ▼
same PortableUserId
```

The exact mnemonic format is an implementation choice. The important property is deterministic recovery of the same portable identity.

This recovery method is intentionally not optimized for mainstream convenience. It exists to preserve user ownership and eliminate dependency on a hosted account authority.

---

# 47. Adding a Portable User to Another Device

Example: Steve wants to add an Apple TV while his phone holds the root identity.

```text
Apple TV
    │
    ├── has Iroh EndpointId T
    ├── generates portable-user device key K_tv
    └── shows QR containing K_tv.public + pairing nonce

Steve phone
    │
    ├── scans QR
    ├── shows "Authorize this Apple TV?"
    └── signs a UserDeviceCredential for K_tv

Apple TV
    │
    └── stores the signed credential
```

The Apple TV can then authenticate as Steve without Steve's phone being online.

This is distinct from the Apple TV's Iroh endpoint identity:

```text
Iroh EndpointId T
    = network/device installation identity

K_tv credential
    = authority to act as PortableUser Steve
```

---

# 48. Portable User Revocation

Revocation is eventually consistent when there is no always-online account authority.

A portable identity may maintain a signed, versioned identity state:

```text
PortableIdentityState
revision = 14

authorized:
    K_phone
    K_linux
    K_tv

revoked:
    K_old_phone
```

The state is signed by the user root. Servers cache the newest valid revision they have seen. Authorized devices may carry newer signed state to servers they contact.

This means revocation may not propagate instantly to every disconnected server, but the system remains decentralized and offline-capable. An optional future personal/home server may improve synchronization speed without becoming the root identity authority.

---

# 49. Portable User Across Multiple Clients

A portable user can use the same account identity across phone, Apple TV, Linux, and other clients without running a server.

```text
                    Portable User U
                    /      |      \
                   /       |       \
             Phone P    TV T    Linux L
```

All devices authenticate as the same `PortableUserId U` using their own credentials.

A server can therefore store user state once:

```text
watch_state

principal = PortableUser(U)
item      = Alien
position  = 01:17:42
```

and the user may resume from another device. No account server is required.

---

# 50. Share Targets

A library share may target either:

1. a portable user; or
2. another Spoolframe server.

Conceptually:

```rust
enum ShareTarget {
    PortableUser(PortableUserId),

    ServerUsers {
        server: ServerId,
    },

    RemoteUser {
        server: ServerId,
        user: RemoteUserId,
    },
}
```

The initial UI may expose only the first two cases.

---

# 51. Sharing to a Portable User

Example:

```text
Your Movies library
        │
        ▼
PortableUser Steve U
```

The origin server stores an ACL grant to the portable principal:

```text
principal  = PortableUser(U)
library    = Movies
permission = Stream
```

The origin server does not host Steve's password, passkey, OIDC account, or recovery mechanism. It stores only the public portable identity, ACL grants, and user-specific state relevant to this server.

---

# 52. Sharing to Another Server

A share may be granted to another Spoolframe server as an administrative trust boundary.

```text
Your Server SA
      │
      │ share Movies
      ▼
Bob's Server SB
      │
      ├── Bob
      └── Alice
```

A server-wide grant means:

> Server `SB` may authorize its own local users to consume this shared library.

The origin server does not host Bob or Alice's credentials. The remote server remains responsible for authenticating its own local users.

The origin may still distinguish those users for watch state and auditing.

Canonical remote-user reference:

```rust
struct RemoteUser {
    server: ServerId,
    user: RemoteUserId,
}
```

Terminology:

```text
LocalUser
    account hosted on this server

RemoteUser
    account hosted on another Spoolframe server

PortableUser
    account hosted by nobody; user-owned cryptographic identity
```

The term `federated local user` should not be used.

---

# 53. Authorization Through a Remote Server

When a remote server has a server-wide library grant:

```text
authenticated federation peer = SB
requesting local user          = Bob@SB
```

The origin server may evaluate:

```text
explicit grant to Bob@SB?
    no

server-wide grant to SB?
    yes

→ allow
```

This preserves individual remote-user state while delegating user authentication to the remote server.

---

# 54. Client-Only Users Do Not Need to Run a Server

A person without their own Spoolframe server can consume shared media using a portable user identity.

The client itself is an Iroh endpoint but is not a Spoolframe server.

```text
Steve phone / TV / Linux
         │
         │ Iroh
         ▼
Your Spoolframe Server
```

No full server needs to run on a phone or Apple TV.

---

# 55. Local Users, Remote Users, and Portable Users

Spoolframe keeps server-local accounts because they are convenient for households.

Example:

```text
Your server

Local users:
    Andrey
    Alice

Portable users with access:
    Steve U
    Carol V

Remote users through federation:
    Bob@SB
```

A common authorization abstraction may therefore use:

```rust
enum Principal {
    LocalUser(LocalUserId),

    PortableUser(PortableUserId),

    RemoteUser {
        server: ServerId,
        user: RemoteUserId,
    },
}
```

All playback, watched-state, and ACL logic should operate on the normalized `Principal` concept rather than on authentication-method-specific types.

---

# 56. Licensing Model

The Spoolframe server is intended to be open source under:

```text
AGPL-3.0-or-later
```

The wire protocol and shared client-facing types should remain permissively licensed so independent and proprietary clients can interoperate cleanly.

Recommended split:

```text
spoolframe-server          AGPL-3.0-or-later
spoolframe-federation      AGPL-3.0-or-later
spoolframe-plex            AGPL-3.0-or-later

spoolframe-protocol        MIT OR Apache-2.0
spoolframe-types           MIT OR Apache-2.0
spoolframe-client-sdk      MIT OR Apache-2.0

spoolframe-linux           free / open source
spoolframe-tvos            proprietary / paid
spoolframe-ios             proprietary / paid
spoolframe-android         proprietary / paid
```

The precise license for the Linux client may be chosen separately, but it is intended to remain free and open source.

The important architectural boundary is:

```text
        permissive protocol/types
             /             \
            /               \
    AGPL server        proprietary clients
```

Commercial clients communicate with the AGPL server over the documented protocol boundary rather than linking AGPL server implementation code into proprietary clients.

---

# 57. Commercial Client Strategy

Linux remains a first-class free/open client.

Official clients for platforms such as tvOS, iOS, Android, and Android TV may be commercial products. Third-party clients remain allowed because the protocol is open.

The business model therefore charges for the quality and maintenance of official native clients rather than for permission to run the server or participate in federation.

Spoolframe should not require a paid central service for federation, remote streaming, authentication, NAT traversal, identity, or basic discovery.

---

# 58. Final Identity Model

Spoolframe deliberately separates four concepts:

```text
1. Iroh EndpointId

   Which app/device/server endpoint is this?

2. Spoolframe local account

   Which user account hosted by this server is this?

3. PortableUserId

   Which user-owned cryptographic identity is this?

4. Session

   Which authenticated principal is using this endpoint right now?
```

For a local user:

```text
Iroh Endpoint T
      +
password / passkey / OIDC / trusted-device auth
      ↓
LocalUser A
      ↓
Session(A, T)
```

For a portable user:

```text
Iroh Endpoint T
      +
root-signed portable device credential
      ↓
PortableUser U
      ↓
Session(U, T)
```

For a remote server user:

```text
Remote Server SB
      +
authenticated federation connection
      +
SB's assertion of local user Bob
      ↓
RemoteUser(Bob@SB)
```

---

# 59. Final Sharing Model

```text
                          LIBRARY
                             │
             ┌───────────────┴────────────────┐
             │                                │
             ▼                                ▼
      Portable User U                   Server SB
             │                                │
       user-owned ID                    delegated auth
        /     |     \                    /          \
     phone    TV    Linux              Bob         Alice
```

A share is therefore granted to either a portable person or a server trust boundary, with optional future support for a specific remote user.

---

# 60. Final Architectural Summary

Spoolframe's core design is:

> **Plex supplies metadata for now. Spoolframe owns playback. Iroh provides endpoint identity, encrypted transport, NAT traversal, and optional relays. Spoolframe owns local users and sessions. Portable users own their own cryptographic identities. Federation lets servers share metadata and delegated access without a central authority.**

A shorter trust model:

```text
Iroh proves the endpoint.

Spoolframe auth proves a local user,
or a portable credential proves a portable user.

A session binds that principal to the endpoint.

ACLs decide what the principal may access.
```

The long-term resilience requirement remains:

> A working Spoolframe server, client, and local IP network must be sufficient to browse and play already-available media even if all centralized Internet infrastructure disappears.

