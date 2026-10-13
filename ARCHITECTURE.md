# Spoolframe Architecture Decisions

Spoolframe is a self-hosted media system for **movies, TV shows and music**.
It does not try to replace every Plex feature. It replaces the parts of
Plex that hurt most, playback and streaming, and keeps the rest of the
system small.

The design lets Plex be removed later without changing the client-facing
library model or the transport.

This document records the decisions and the reasons for them. The wire
format that implements them is in [PROTOCOL.md](PROTOCOL.md).

## How to read this document

Sections are numbered, and code, tests and other documents cite them as
`§N`. The numbers are stable: a section is never renumbered. 24A to 24F
are later additions to §24.

| Topic | Sections |
|---|---|
| Scope and the role of Plex | §1–§7, §36 |
| Clients and playback | §8–§10, §33–§35 |
| Iroh, endpoints and identity | §11–§20, §25, §31, §41 |
| Local users and sign-in | §21–§24F, §40 |
| Federation between servers | §26–§30 |
| Working without the Internet | §32 |
| Portable users | §42–§49, §54 |
| Sharing | §50–§53, §55, §59 |
| Licensing | §56, §57 |
| Summaries and rules | §38, §39, §58, §60 |
| Build order | §37 |

## 1. Scope

### In scope

- Movies, TV shows, music
- Linux and tvOS clients, both first-class
- A web UI, mainly for administration and debugging
- Direct play, remux and transcoding
- Local users; many users per server
- Many accounts on one device; many devices per account
- Remote sharing
- Portable user identities
- Server-to-server federation
- No centralized identity provider
- LAN operation when the public Internet is down

### Not required for the first version

- Photos
- Live TV / DVR
- Podcasts
- A Plex-compatible client protocol
- A global Spoolframe account service
- Centralized cloud discovery as a hard dependency
- Own media matching and indexing before playback works

## 2. MVP Philosophy

The first version does **not** replace Plex library management.

Plex is the **metadata and library control plane**:

- scanning and media matching
- movie, show and music metadata
- posters and artwork
- library structure: seasons and episodes, artists, albums and tracks
- existing watched state, where useful

Spoolframe owns:

- the client-facing API
- users, authentication and sessions
- playback state for Spoolframe users
- media probing and capability negotiation
- direct streaming, remux and transcoding
- federation and remote sharing

The key rule:

> Plex tells Spoolframe **what the media is**. Spoolframe decides **how it plays**.

## 3. High-Level Architecture

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

## 4. Plex as the Initial Library Manager

Spoolframe does not import the Plex database into a native catalog. A
`PlexCatalog` sits behind Spoolframe's own catalog interface, and a
native catalog can replace it later:

```text
Client → Spoolframe API → Catalog trait ─┬─ PlexCatalog → Plex PMS   (today)
                                         └─ NativeCatalog            (later)
```

Clients never see Plex-specific semantics in the public API, so the
Linux and tvOS clients need no major change when the catalog changes.

## 5. Public Media Model Belongs to Spoolframe

Public models are Spoolframe models, even while Plex supplies the
metadata:

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

Plex identifiers are an implementation detail. An id may map directly to
a Plex `ratingKey` inside the server, but the client-facing id is
namespaced and opaque, so Plex assumptions do not leak into client code:

```text
plex:movie:12345
plex:episode:98321
```

These can become native Spoolframe ids later.

## 6. Playback Is Fully Spoolframe-Owned

Plex does not decide whether something direct-plays or transcodes.
Spoolframe resolves the file and probes it itself:

```text
Plex item → resolve media path → ffprobe → MediaDescriptor
          → PlaybackPlanner(client capabilities) → Direct | Remux | Transcode
```

```rust
struct MediaDescriptor {
    container: Container,
    duration: Duration,
    video: Option<VideoStream>,
    audio: Vec<AudioStream>,
    subtitles: Vec<SubtitleStream>,
}

enum PlaybackPlan {
    Direct { /* source details */ },
    Remux { /* output/manifest details */ },
    Transcode { /* output/manifest details */ },
}
```

## 7. Shared Storage / Plex Path Mapping

Spoolframe and Plex may see the same files at different paths:

```text
Plex:        /data/movies/Alien/Alien.mkv
Spoolframe:  /media/movies/Alien/Alien.mkv
```

```ucl
plex {
  path_mapping [
    { from = "/data/movies"; to = "/media/movies"; },
  ]
}
```

Spoolframe always probes and serves the local path it can access.

Plex names the files Spoolframe reads, so its paths are confined: a path
with a `..` component is never served, and once any rule exists, neither
is a path that no rule maps. A library whose files are at the same path
on both hosts then needs a rule from that path to itself. With no rules
at all, Plex's paths are used as they are.

## 8. Client Capability Negotiation

Playback decisions come from client capabilities, not from client names.
A conceptual capability set:

```json
{
  "video": ["h264", "hevc", "av1"],
  "audio": ["aac", "ac3", "eac3"],
  "containers": ["mp4", "mov"],
  "streaming": ["hls"],
  "subtitles": ["webvtt"]
}
```

Linux/GStreamer may advertise a very broad set; tvOS/AVPlayer advertises
Apple's. The server avoids transcoding unless it has to.

## 9. Linux Is a First-Class Client

The Linux client is not a temporary test UI. Its stack is Rust, Relm4,
GTK4 and GStreamer:

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

GStreamer is preferred over launching mpv because Spoolframe needs to
see buffering, decoder selection, hardware acceleration, stream changes,
seeking, timestamps, HLS behaviour, and audio and subtitle handling.

## 10. tvOS Is Also First-Class

tvOS is a native client: Swift/SwiftUI, AVPlayer, and a thin Rust/UniFFI
layer for Spoolframe networking where needed. It uses Iroh as its
transport too.

The hard part is not authentication. It is feeding media from Iroh into
AVPlayer. There are two ways.

### Option A — AVAssetResourceLoader

```text
AVPlayer → AVURLAsset → AVAssetResourceLoader → Swift/Rust bridge → Iroh → Spoolframe Server
```

### Option B — Local loopback HTTP bridge

Probably the simpler first path:

```text
AVPlayer
   │ http://127.0.0.1:<port>/...
   ▼
tiny local HTTP server → Spoolframe Rust networking → Iroh → Spoolframe Server
```

The loopback server exists only inside the app. The network path is
still Iroh.

## 11. Iroh Is the Lower-Layer Networking Substrate

Spoolframe uses Iroh because it gives:

- cryptographic endpoint identity
- encrypted, authenticated QUIC connections
- NAT traversal, direct connections where possible, optional relays
- address and local-network discovery
- no dependency on public certificate authorities
- no dependency on a centralized Spoolframe account service

The resilience property this buys:

> If the public Internet disappears but a LAN survives, existing Spoolframe servers and clients can still discover, connect and play media.

## 12. Core Iroh Terminology

### `SecretKey`

The persistent private key of one Iroh endpoint. It stays private.

### `EndpointId`

The public identity derived from the `SecretKey`: a cryptographic
installation, device or server identity.

### `Endpoint`

A running Iroh networking instance; normally one per application
installation or process.

### `EndpointAddr`

An `EndpointId` plus the current ways to reach it (LAN address, public
address, optional relay). Identity and address are separate on purpose.

### `Connection`

An authenticated, encrypted QUIC connection between two endpoints.

### QUIC stream

An independent logical stream inside one connection. One connection
carries many streams at once.

### ALPN

Names the Spoolframe protocol spoken on an Iroh connection:

```text
spoolframe/client/1         apps to a server
spoolframe/pair/1           a phone to a device, for pairing a portable user
spoolframe/relay-access/1   a server's own relay to that server
spoolframe/federation/1     server to server (reserved; refused until federation exists)
```

The client protocol carries a revision number inside `/1`. See
[PROTOCOL.md](PROTOCOL.md) for the framing, resource streams, replay
rules and the current revision.

### Address lookup

Maps an `EndpointId` to possible locations: mDNS on the LAN, saved
addresses, decentralized lookup, optional relays and services, or hints
in an invite.

Address lookup is discovery, not authentication. The Iroh connection
still proves the remote endpoint's key.

## 13. Iroh Identity Is Not User Identity

This is fundamental.

```text
Iroh EndpointId     = device / installation / server identity
Spoolframe UserId   = human account
Spoolframe Session  = one user authenticated from one endpoint
```

Never equate `EndpointId == UserId`.

## 14. One Endpoint Per Client Installation

A client normally has one persistent Iroh identity:

```text
Spoolframe Server     Endpoint S
Apple TV              Endpoint T
Linux desktop         Endpoint L
Friend's Linux PC     Endpoint F
```

These are machines and installations, not accounts.

## 15. Multiple Accounts on One Client

A shared Apple TV has one endpoint, `T`, but may hold several Spoolframe
sessions. One connection carries requests for different sessions:

```text
QUIC connection T ↔ S

stream 1:  session = Andrey   GetHome
stream 2:  session = Alice    GetWatchHistory
stream 3:  session = Andrey   StartPlayback(...)
```

Switching profiles needs no reconnect.

## 16. One Account on Multiple Devices

The inverse holds too: one user signs in from an Apple TV, a Linux
desktop, a laptop and a phone. The user/device relationship is
many-to-many:

```text
User ←── Session ──→ Endpoint
```

## 17. Fresh Client Installation

A fresh tvOS or Linux client:

1. Generates an Iroh `SecretKey` and persists it.
2. Derives its `EndpointId`.
3. Discovers or imports a server.
4. Connects to the expected server `EndpointId`.
5. Establishes an encrypted Iroh connection.
6. Authenticates a Spoolframe user inside that connection.

## 18. First-Contact Server Discovery

### LAN

mDNS/Bonjour-style discovery. Conceptually:

```text
_spoolframe._udp.local

name       = "Home Media"
endpoint   = S
protocol   = 1
address    = 192.168.1.20:...
```

The client learns the server's `EndpointId`. mDNS is untrusted: Iroh
proves whether the machine contacted owns `EndpointId S`.

### Remote

An invite carries at least the server `EndpointId`, known addresses or
connection hints, and optionally a one-time invite capability. It can be
a QR code, a deep link or a string to copy and paste.

## 19. First-Contact Trust

For LAN servers, Spoolframe may trust on first use: the user selects
"Home Media", the client connects to `S`, and pins `S` for that saved
server. A short pairing or fingerprint check gives stronger
verification. Remote invites carry the expected `EndpointId`, so there
is no blind trust on first use.

The pin is a pair: the server's `EndpointId` and its server id, which
`Hello` names (`ServerInfo.server_id`). The server id is made once, kept
in the state directory beside the key, and outlives a new key. The SDK
pins both at the first connect; a pin saved without an id takes the id
at its next connect. Then:

| What answers | What the SDK does |
|---|---|
| Same key, same id, any name | Connects. A new name is saved; it never asks. |
| Same id, another key | `IdentityChanged`: the key was replaced. The user checks the new fingerprint, and `trust_new_identity` pins the new key with the same id, so the server's app passkeys keep working. |
| Same key, another id | `ServerIdChanged`: refused every time, and nothing is kept. A server never changes its id, so this cannot be accepted in place. |
| Another key and another id | Another server, whatever its name. No pin applies; the user may add it. |

A name says nothing about a pin that has an id. Only a pin with no id
yet is still matched by name: another key heard under its name is an
`IdentityChanged`.

## 20. Iroh Connections May Be Anonymous at the Spoolframe Layer

A server may accept a connection from an unknown `EndpointId`. The
connection is already authenticated as that endpoint, but has no
Spoolframe authority yet:

```rust
enum ConnectionAuth {
    Anonymous { endpoint: EndpointId },
    User { endpoint: EndpointId, user: UserId, session: SessionId },
    FederationPeer { endpoint: EndpointId, peer: ServerId },
}
```

An anonymous client connection gets only a small pre-auth surface:
`Hello`, `AuthMethods`, `Ping`, the sign-in requests (password, passkey,
device approval, resuming a trusted profile, a portable credential),
invite redemption, and the public identity-record and pairing-relay
requests. It gets no catalog, playback, watch state or admin requests.
PROTOCOL.md lists the exact requests.

## 21. Local Users Belong to Spoolframe

Plex is not the user database. Spoolframe has its own local accounts.
A conceptual schema:

```text
users      id, username, display_name, password_hash
devices    id, endpoint_id, display_name, last_seen
sessions   id, user_id, device_id, created_at, expires_at, revoked_at
```

Later: `library_grants`, `watch_state`, `preferences`, `passkeys`,
`trusted_endpoints`, `federation_grants`.

## 22. Password Authentication

Passwords are ordinary application credentials. They travel only inside
the encrypted Iroh connection. The server stores a strong hash
(Argon2id).

```text
Client T ──encrypted Iroh connection──▶ Server S
Login { username, password } → Argon2id verification → Session created
```

## 23. Passkeys

Standard WebAuthn passkeys are supported where practical, while normal
Internet and domain infrastructure exists. They are not the fundamental
decentralized identity mechanism, because WebAuthn relies on a
relying-party domain model.

So Spoolframe keeps username/password, trusted-device authentication and
explicit device pairing as independent fallbacks. They are available,
not required of every user: a user may sign in with a passkey alone. The
UI recommends a password or SSO next to a passkey, because a user, or a
sole admin, who loses their only passkey has no other way in short of
shell access to the server.

The apps make their passkeys for a relying party per server under
`spoolframe.app`: `<server id>.spoolframe.app`, at the origin
`https://<server id>.spoolframe.app`. A platform's passkey API works only
for a domain the app is associated with at build time, and a self-hosted
server's domain is not known then. One association file at
`spoolframe.app` covers every subdomain (on Apple, one
`webcredentials:*.spoolframe.app` entitlement). The ceremonies run from
app to server over `spoolframe/client/1`, and the server never contacts
`spoolframe.app`, so a LAN-only server has app passkeys too.

The server id, not the key, names the relying party: a passkey syncs with
its relying party and must outlive a replaced key. With one relying
party per server, a security key holding passkeys for two servers offers
each server only its own. Passkeys made by early builds for
`spoolframe.app` itself are kept, listed as old so the user can add them
again, and sign no one in.

Each server keeps its own app passkeys (beside the web UI's, told apart
by relying-party ID), its own random user handle per user, and its own
challenges. A challenge is bound to the device that asked and to the
server: the server sends a nonce, and the challenge is a hash of the
server's endpoint id and the nonce, which the app's SDK computes with the
id it pinned. The relying party alone does not bind the server, because
a server may name another's server id; the challenge does. A passkey or
an assertion for one server is no use at another, even one relayed by a
trusted server that claims the other's id. An app lists, renames and
removes its user's passkeys, the web UI's included, through the same
store calls and rules as the web UI's profile page.

Hardware security keys may keep working locally where the WebAuthn
relying-party setup stays valid, but Spoolframe must not depend on that
for offline survival.

## 24. Trusted Iroh Devices

After a successful login, a user may choose "Remember / trust this
device". The server stores the pair `(user_id, endpoint_id)` in
`trusted_user_endpoints`, for example `Andrey → Apple TV T` and
`Alice → Apple TV T`.

On later launches Iroh proves the endpoint is still `T`, and the server
can offer "Who's watching? [Andrey] [Alice]" without asking for the
password again.

A trusted endpoint is not the user. It is only allowed to re-establish a
session for that user.

## 24A. OIDC Authentication

OIDC is an optional authentication method. It does **not** replace
Spoolframe's local user model:

```text
Iroh EndpointId        = device / installation identity
Spoolframe UserId      = local human account
Authentication method  = one way to prove control of that account
```

OIDC maps an external identity onto an existing or newly created local
user, beside the other methods:

```text
Iroh connection → Spoolframe auth ─┬─ password
                                   ├─ passkey
                                   ├─ trusted Iroh endpoint
                                   └─ OIDC
                                        → local UserId → Spoolframe Session
```

The stable external key is `(issuer, subject)`, never the email address:

```text
issuer  = https://accounts.google.com
subject = 108349857...
        → Spoolframe UserId 42
```

A possible schema:

```text
external_identities   user_id, issuer, subject, email, display_name
```

`email` and `display_name` are informational and may change.
`issuer + subject` is the binding.

## 24B. OIDC Session Creation

OIDC changes only the authentication step. After it succeeds the server
resolves the local `UserId`, creates a session, and binds it to the
authenticated `EndpointId`, as for any sign-in:

```text
Session { user = UserId(42), endpoint = EndpointId(T) }
```

Playback, watched state, ACLs, library grants, federation and client
behaviour do not care whether a session came from a password, a passkey,
OIDC, a trusted endpoint or a device approval.

## 24C. OIDC on Linux

Linux clients use the standard OIDC Authorization Code flow with PKCE:

```text
Linux client → system browser → OIDC provider (authenticate)
  → localhost or custom callback URI → Linux client
  → complete auth through Spoolframe server → local UserId
  → Session bound to the Linux EndpointId
```

The server validates the configured issuer and maps the result to a
local user:

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

Self-hosted providers (Authentik, Keycloak, Kanidm, Zitadel and similar)
work without affecting the rest of the architecture.

## 24D. OIDC on tvOS

tvOS should not make users type long credentials with the remote. The
TV shows a QR code or a short code, and the sign-in completes elsewhere:

```text
Apple TV: "Sign in with <provider>" + QR code / short code
  → phone or browser → Spoolframe server → OIDC provider
  → authentication succeeds → pending Apple TV login approved
  → Session created for the Apple TV EndpointId
```

The TV never shows the provider's login UI. Spoolframe is the OIDC
relying party, and the TV takes part in a device-style login.

The first way to complete the pending request is not OIDC but approval
from the Spoolframe app on a phone that is already signed in: the phone
scans the QR code or takes the short code, shows the device's name, and
approves it as its own user. OIDC in the phone's browser completes the
same pending request, so the TV side is one flow either way.

## 24E. OIDC Account Linking

OIDC identities are linked to local accounts explicitly:

```text
UserId 1: Andrey
    ├── password
    ├── passkey #1, passkey #2
    ├── trusted endpoint T
    ├── OIDC Google    (issuer, subject)
    └── OIDC Authentik (issuer, subject)
```

Accounts are never merged automatically because their email addresses
match. A signed-in user links an OIDC identity from account settings.

For invited or new users, server policy may allow "invite → authenticate
with OIDC → create local user". This is an explicit policy, never the
default.

## 24F. OIDC and Resilience

OIDC is a convenience, not a dependency of the core system:

| | Internet available | Internet unavailable |
|---|---|---|
| OIDC | yes | possibly unavailable |
| Passkey | yes | depends on platform and relying-party setup |
| Password | yes | yes |
| Trusted endpoint | yes | yes |

Previously trusted devices keep authenticating by their `EndpointId`
when the OIDC provider is unreachable. Passwords and trusted endpoints
remain the durable fallbacks.

> OIDC may improve login convenience while infrastructure exists, but Spoolframe must remain usable without it.

## 25. Sessions Are Bound to EndpointId

A session is not a globally reusable bearer token. On every request

```text
session.endpoint == connection.remote_endpoint_id
```

must hold. An attacker who steals only a session id and connects as
endpoint `X` is rejected. The effective credential is
`(SessionId, authenticated EndpointId)`.

## 26. Federation

Federation is server-to-server. It is for sharing library metadata,
grants, remote-library discovery, metadata updates, reconciliation and
remote playback authorization. It does not proxy media through the
recipient's server.

## 27. Federation Identity

Each server has a long-lived Iroh endpoint key, and its `EndpointId` is
its canonical identity. DNS names may exist as aliases, but are not the
root of identity. The Internet is only one possible environment for
discovery and transport.

## 28. Federation Protocol

Federation uses its own ALPN, `spoolframe/federation/1`, and is stricter
than client connections:

```text
incoming Iroh connection → which ALPN?
   client/1:      unknown endpoint allowed pre-auth
   federation/1:  known peer, or a valid invite?
```

Unknown clients may reach login. Unknown federation peers may reach only
pairing and invite flows.

## 29. Federation Is Metadata Replication, Not Media Proxying

Alice's server `SA` sends metadata to Bob's server `SB`, which caches
titles, posters, descriptions, hierarchy, remote item ids and the origin
endpoint. Bob's client browses that cache locally.

When Bob plays an item, his client gets playback authorization from the
origin server `SA`, and the media travels directly from `SA` to the
client where practical.

## 30. Federated Principal Identity

A globally meaningful user identity is a pair:

```rust
struct PrincipalId {
    server: EndpointId,
    user: UserId,
}
```

for example `(SA, Alice)` and `(SB, Bob)`. No DNS-based global account
service is needed.

## 31. Relays Are Optional Routing Infrastructure

Relays are not identity providers. A relay may help two endpoints
connect through NAT, or forward encrypted packets when there is no
direct path. The security relationship stays `Client C ↔ Server S`, not
`Client C ↔ Relay ↔ Server S`. A relay is never the authority for user
or server identity.

Self-hosted relays may be supported. If every relay disappears but
direct LAN connectivity exists, Spoolframe still works.

## 32. LAN-Survival Requirement

Spoolframe keeps working when all of these are gone:

- public DNS
- public CA infrastructure and Let's Encrypt
- Spoolframe-operated services
- Number 0 services
- Plex cloud account services
- public Internet connectivity

As long as a server and clients exist, share an IP network, and still
have their persisted endpoint keys, the system can discover, connect,
authenticate and play locally:

```text
Internet, DNS, public CAs, relays gone
  → mDNS / saved address → EndpointId S discovered → Iroh verifies S
  → client endpoint already trusted → choose local user → play media
```

## 33. Media Transport Over Iroh

Native clients use Iroh for all client/server communication. The split:

```text
spoolframe/client/1       auth, catalog, search, playback negotiation,
                          user state, events, media requests
spoolframe/federation/1   federation
```

Control and media share one ALPN. One connection carries many QUIC
streams: RPC, events, artwork, media. Separate connections may come later
if congestion control makes them useful.

## 34. Seeking / Media Read Semantics

Media over Iroh is not one endless byte stream. The protocol has
range-like reads (`OpenMedia { media_id }`, then
`Read { offset, length }`, or an equivalent), which give seeking, random
access, resumable playback, player buffering and efficient handling of
large files.

For transcoded, HLS-like playback, manifests and segments are logical
resources.

## 35. Web UI

The web UI is useful, but it is not the primary desktop client. It is
for administration, server configuration, inspecting the Plex
integration, matching and path resolution, users, devices, sessions and
federation, debugging playback, and basic browsing. Native Linux and
tvOS remain the first-class playback clients.

Browsers cannot easily speak Iroh, so the web UI uses a conventional
HTTP interface, locally or through a gateway.

## 36. Watched State

Spoolframe owns watched and progress state for its users, because
several local users may share one Plex-backed catalog:

```text
Plex metadata item: Alien
Spoolframe:  Andrey → watched   Alice → 37 minutes   Steve → unwatched
```

Plex cannot represent these users when it is only the metadata backend.
For the owner's own Plex account, Spoolframe may mirror watched and
progress state back to Plex during the transition.

## 37. Initial Development Order

### Phase 1 — Server + Plex catalog

Connect to Plex; browse libraries; normalize movies, shows and music into
Spoolframe models; resolve media paths; path mappings; artwork proxying
and caching.

### Phase 2 — Playback core

ffprobe; media descriptor; capability negotiation; direct play, remux,
transcode; seeking; playback sessions.

### Phase 3 — Linux first-class client

Relm4, GTK4, GStreamer; Iroh client transport; login; profile switching;
watched and progress state.

### Phase 4 — Local users

Passwords; passkeys where practical; OIDC; account linking; trusted
endpoints; many accounts per device and devices per account; device and
session management.

### Phase 5 — tvOS

SwiftUI; Iroh through Rust/FFI; AVPlayer bridge (loopback HTTP or
resource loader); profile switching.

### Phase 6 — Federation

Server identity and pairing; grants; metadata replication; remote-library
browsing; playback authorization.

### Phase 7 — Remove Plex dependency

Replace `PlexCatalog` with `NativeCatalog`, keeping the client-facing
protocol and the federation model.

## 38. Architectural Rules

These decisions stay unless there is a strong reason to revisit them.

1. **Plex is one optional library source, never the streaming engine.**
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
19. **Adding or removing Plex libraries, or a server without Plex at all, must not require redesigning clients or federation.**

## 39. Core Mental Model

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

In one sentence:

> **Iroh authenticates endpoints. Spoolframe authenticates users. Sessions bind users to endpoints. Plex supplies metadata for now. Spoolframe owns playback and eventually the whole library.**

## 40. Device Login Policies

Once a user has signed in on a device, they may trust it. Trust is the
pair `(UserId, EndpointId)`. A trusted device then has one of these
policies:

```rust
enum DeviceLoginPolicy {
    RequireFullLogin, // password / passkey / OIDC required again
    Remembered,       // profile picker, no full login
    PinProtected,     // profile picker, then the user's PIN
    AutoLogin,        // a new session for one trusted user at once
}
```

Auto-login is allowed only for an endpoint already trusted for that
user: Iroh connects as `T`, the server recognizes `T`, `T` is trusted
for user A, the device's policy is `AutoLogin(A)`, and the server
creates a fresh session for A.

A PIN is an extra gate for a user on a device, not the cryptographic
identity. It is valid only with the trusted endpoint: endpoint `T` with
the right PIN is allowed; endpoint `X` with the same PIN is rejected.

The server checks PINs and rate-limits them. Auto-login is a
device-level choice, with at most one default user per device. PIN and
remembered policies are per `(UserId, EndpointId)`.

## 41. NAT Traversal and Remote Reachability

Spoolframe relies on Iroh for NAT traversal and does not implement its
own STUN, TURN or DERP logic.

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

Preferred order: direct LAN/IP, then UDP hole punching, then Iroh relay
fallback.

Relays are optional and are not identity authorities. Spoolframe uses
Iroh's relay protocol and implementation, and does not assume that
arbitrary Tailscale DERP servers are compatible.

Spoolframe supports automatic relays, custom or self-hosted relays, and
no relays. Direct paths matter most for media, because sustained relay
traffic is expensive; relayed media is still a valid fallback. If the
Internet and all relays disappear but a LAN remains, direct Iroh
connections keep working.

## 42. Portable User Identities

Spoolframe has a second user model beside server-local users:

```text
LocalUser      an account hosted by a Spoolframe server
PortableUser   a cryptographic identity owned by the user
```

A portable identity exists independently of any server. It is for people
who want access to shared libraries without someone else hosting their
account. Steve, a portable user `U`, signs in to several servers as the
same identity.

## 43. Portable Identity Root

A portable identity is rooted in a long-lived signing key:

```text
UserRootSecret → UserRootPublicKey → PortableUserId
```

The user owns the root secret. The root key only signs, and is not
copied to every playback device. It authorizes per-device credentials
instead.

## 44. Phone as Portable Account Holder

The phone app holds the root: it stores `UserRootSecret`, owns
`PortableUserId U`, authorizes and revokes devices, and manages
recovery.

The phone is not an always-online account server. It is only the device
that normally holds the root authority. Once other devices have their
own signed credentials, they authenticate without the phone.

## 45. Portable User Device Credentials

Each device of a portable user gets its own authentication key pair, and
the root signs a credential binding that key to the user:

```rust
struct UserDeviceCredential {
    user: PortableUserId,
    device_public_key: PublicKey,
    serial: u64,
    issued_at: Timestamp,
    expires_at: Timestamp,
    signature_by_user_root: Signature,
}
```

Signing in to a server proves:

1. possession of the device private key;
2. possession of a valid, root-signed device credential;
3. the portable identity that credential names.

The server verifies this chain without contacting any account server.

`expires_at` is required, at most 90 days after `issued_at`. The
signature is over a fixed binary layout under a domain string, not CBOR
(PROTOCOL.md, "Portable identities"). Proof of possession is the device
key's signature over the server's endpoint id and a nonce the server
chose.

## 46. Portable User Recovery

The root can be recovered offline, with a recovery phrase in the style
of a cryptocurrency wallet:

```text
recovery phrase → root secret → same root public key → same PortableUserId
```

The format: BIP-39, English wordlist, 24 words (256 bits). The standard
BIP-39 seed (empty passphrase) goes through
`blake3::derive_key("spoolframe 2026-10-09 portable root v1", seed)` to
give the Ed25519 root key, whose public key is the `PortableUserId`.

This method is not tuned for mainstream convenience. It exists so users
own their identity and no hosted account authority is needed.

## 47. Adding a Portable User to Another Device

Steve adds an Apple TV while his phone holds the root:

```text
Apple TV      has Iroh EndpointId T
              generates a portable-user device key K_tv
              shows a QR code with K_tv.public and a pairing nonce

Steve phone   scans the QR code
              asks "Authorize this Apple TV?"
              signs a UserDeviceCredential for K_tv

Apple TV      stores the signed credential
```

The Apple TV can then sign in as Steve without his phone online. This
is separate from the TV's Iroh identity: `EndpointId T` is the network
identity of the installation; the `K_tv` credential is the authority to
act as Steve.

## 48. Portable User Revocation

With no always-online account authority, revocation is eventually
consistent. The root signs a versioned identity state, servers keep the
newest valid revision they have seen, and the user's devices carry newer
states to the servers they contact. A revocation may not reach a
disconnected server at once, but the system stays decentralized and
works offline. A personal or home server may later speed up propagation
without becoming the root authority.

The state lists revoked keys only, each with when it was revoked; there
is no list of authorized keys, so adding a device needs no new state.
Revocation only grows: a server refuses an unexpired credential whose
key any valid state it has seen revokes, older revisions included, and
keeps the highest revision to hand on. A revocation lapses 90 days and 5
minutes after it was made, when no credential it could stop can still
hold, and may then be dropped. A state holds at most 1024 live
revocations.

## 49. Portable User Across Multiple Clients

A portable user uses one identity across phone, Apple TV, Linux and
other clients, without running a server. Every device signs in as the
same `PortableUserId U` with its own credential, so a server stores the
user's state once:

```text
watch_state   principal = PortableUser(U), item = Alien, position = 01:17:42
```

The user resumes on another device. No account server is involved.

## 50. Share Targets

A library share targets a portable user or another Spoolframe server:

```rust
enum ShareTarget {
    PortableUser(PortableUserId),
    ServerUsers { server: ServerId },
    RemoteUser { server: ServerId, user: RemoteUserId },
}
```

The first UI may offer only the first two.

## 51. Sharing to a Portable User

The origin server stores an ACL grant to the portable principal:

```text
principal  = PortableUser(U)
library    = Movies
permission = Stream
```

It does not host Steve's password, passkey, OIDC account or recovery. It
stores only the public identity, the grants, and Steve's state on this
server.

## 52. Sharing to Another Server

A share may go to another server, as an administrative trust boundary:
your server `SA` shares Movies with Bob's server `SB`, which has users
Bob and Alice. A server-wide grant means:

> Server `SB` may authorize its own local users to consume this shared library.

The origin does not host Bob's or Alice's credentials; `SB`
authenticates its own users. The origin may still tell them apart for
watch state and auditing, by the remote-user reference:

```rust
struct RemoteUser {
    server: ServerId,
    user: RemoteUserId,
}
```

Terms:

```text
LocalUser      an account hosted on this server
RemoteUser     an account hosted on another Spoolframe server
PortableUser   an account hosted by nobody; a user-owned cryptographic identity
```

Do not say "federated local user".

## 53. Authorization Through a Remote Server

With a server-wide grant, the authenticated federation peer is `SB` and
the requesting user is `Bob@SB`. The origin asks: is there an explicit
grant to `Bob@SB`? No. A server-wide grant to `SB`? Yes: allow.

This keeps per-user state for remote users while `SB` authenticates them.

## 54. Client-Only Users Do Not Need to Run a Server

Someone without a server consumes shared media with a portable identity.
Their phone, TV or Linux machine is an Iroh endpoint that talks to your
server; it is not a Spoolframe server, and no full server runs on a
phone or Apple TV.

## 55. Local Users, Remote Users, and Portable Users

Server-local accounts stay, because they suit households. One server
may have local users (Andrey, Alice), portable users with access
(Steve `U`, Carol `V`), and remote users through federation (`Bob@SB`).
Authorization works on one abstraction:

```rust
enum Principal {
    LocalUser(LocalUserId),
    PortableUser(PortableUserId),
    RemoteUser { server: ServerId, user: RemoteUserId },
}
```

Playback, watched state and ACLs operate on `Principal`, never on types
specific to an authentication method.

## 56. Licensing Model

The server is open source under `AGPL-3.0-or-later`. The wire protocol
and shared client-facing types are permissive, so independent and
proprietary clients can interoperate.

```text
spoolframe-server          AGPL-3.0-or-later
spoolframe-federation      AGPL-3.0-or-later   (planned)
spoolframe-plex            AGPL-3.0-or-later

spoolframe-protocol        MIT OR Apache-2.0
spoolframe-types           MIT OR Apache-2.0
spoolframe-client-sdk      MIT OR Apache-2.0

spoolframe-linux           free / open source
spoolframe-tvos            proprietary / paid
spoolframe-ios             proprietary / paid
spoolframe-android         proprietary / paid
```

The Linux client's exact license may be chosen separately; it stays free
and open source.

The boundary:

```text
        permissive protocol/types
             /             \
    AGPL server        proprietary clients
```

Commercial clients talk to the AGPL server over the documented protocol.
They do not link AGPL server code.

### Shared playback code

`spoolframe-policy` (GPL-3.0-or-later) holds the pure watch thresholds,
credits interpretation and track selection. The AGPL server and the GPL
Linux test backend use these rules. The public SDK does not depend on
this crate.

The MIT OR Apache-2.0 SDK owns managed playback, ordered and coalesced
progress, cancellation cleanup, profile completion scopes and optional
media readers. Reader code taken from the Linux client is relicensed by
its copyright holder under the SDK's license. GTK and GStreamer
integration stays in the Linux client; persistence and transcoding stay
in the server.

### Checking the boundary

Run `devenv shell -- bash scripts/check-licenses.sh` from this project.
The `spoolframe:licenses` task also runs before `ci:static`. `deny.toml`
checks the server's complete normal and build dependency graph; AGPL is
allowed only for the named server crates. `deny-sdk.toml` checks the
public crates' graphs with a permissive allowlist and no copyleft
exceptions. Both checks cover optional features and all target
platforms. A dependency with an unaccepted or unrecognized license fails
the check.

The Linux client has its own `deny.toml` and `spoolframe-linux:licenses`
task. It permits its GPL application crates and LGPL/MPL dependencies,
but not the AGPL server crates. cargo-deny checks Rust dependency graphs
only; it does not audit system libraries, codecs, fonts or other shipped
assets.

## 57. Commercial Client Strategy

Linux stays a first-class free and open client. Official clients for
tvOS, iOS, Android and Android TV may be commercial products. Third-party
clients are allowed, because the protocol is open.

The business model charges for the quality and upkeep of the official
native clients, not for permission to run the server or to federate.
Spoolframe does not require a paid central service for federation,
remote streaming, authentication, NAT traversal, identity or basic
discovery.

## 58. Final Identity Model

Spoolframe keeps four concepts apart:

```text
1. Iroh EndpointId           Which app, device or server endpoint is this?
2. Spoolframe local account  Which account hosted by this server is this?
3. PortableUserId            Which user-owned cryptographic identity is this?
4. Session                   Which authenticated principal is using this endpoint now?
```

```text
Local user:     Endpoint T + password / passkey / OIDC / trusted device
                  → LocalUser A → Session(A, T)

Portable user:  Endpoint T + root-signed device credential
                  → PortableUser U → Session(U, T)

Remote user:    Server SB + authenticated federation connection
                  + SB's assertion of its user Bob → RemoteUser(Bob@SB)
```

## 59. Final Sharing Model

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

A share goes to a portable person or to a server trust boundary, with
possible later support for one specific remote user.

## 60. Final Architectural Summary

> **Plex supplies metadata for now. Spoolframe owns playback. Iroh provides endpoint identity, encrypted transport, NAT traversal, and optional relays. Spoolframe owns local users and sessions. Portable users own their own cryptographic identities. Federation lets servers share metadata and delegated access without a central authority.**

The trust model:

```text
Iroh proves the endpoint.
Spoolframe auth proves a local user, or a portable credential proves a portable user.
A session binds that principal to the endpoint.
ACLs decide what the principal may access.
```

The long-term resilience requirement:

> A working Spoolframe server, client, and local IP network must be sufficient to browse and play already-available media even if all centralized Internet infrastructure disappears.
