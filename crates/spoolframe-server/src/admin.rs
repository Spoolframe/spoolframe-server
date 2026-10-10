//! The admin API server: JSON-RPC over WebSocket on TCP (ARCHITECTURE
//! §35), as the other foundry services serve theirs. The listener's HTTP
//! side (the browser's sign-in, the pages) is `crate::admin_http`.
//!
//! On every interface by default (P3-10): passwords and session tokens
//! cross it in clear, so the expectation is a TLS reverse proxy in front
//! (`admin.public_url`). Plain HTTP on the LAN reaches the setup code and
//! the wizard; staying signed in there needs `admin.insecure_cookies`.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};

use jsonrpsee::Extensions;
use jsonrpsee::core::{RpcResult, async_trait};
use jsonrpsee::server::ServerHandle;
use jsonrpsee::types::ErrorObjectOwned;
use serde_json::json;
use spoolframe_admin_api::ConfigReload;
use spoolframe_admin_api::{
    Activity, LogPage, LogQuery, MetricsEndpointStatus, OtlpStatus, PlexServerActivity,
    RefusedScrape, ResourceAttribute, TelemetryStatus, UserSessionCount,
};
use spoolframe_admin_api::{
    AdminApiServer, Credentials, DeviceEntry, DeviceSignIn, DiscoveredPlex, LibrariesOverview,
    LibraryPathCheck, LoggedIn, MeApiServer, NewUser, PasskeyChallenge, PasskeyEntry, PasskeyHost,
    PasskeySetupBegin, PasskeySetupFinish, PasswordChange, PathCheckRun, PathFailures,
    PathMappingEntry, PathReport, PlexAccountServer, PlexLinkState, PlexPin, PlexPinState,
    PlexServerEntry, PlexStatus, PlexSyncOverview, PlexSyncReport, PlexUser, Role, ServerStatus,
    SessionEntry, SettingEntry, Settings, SetupRequest, SetupState, SyncDirection,
    TranscodingStatus, UserInfo, error_code,
};
use spoolframe_admin_api::{
    ArtworkInfo, CatalogItem, CatalogLibrary, CatalogPage, CatalogQuery, CatalogRow, PathsChecked,
};
use spoolframe_admin_api::{
    AudioPick, SsoLink, SubtitlePick, TrackOverrideEntry, TrackPreferences, TrackPreferencesInfo,
};
use spoolframe_admin_api::{InviteCreated, InviteEntry, InviteKind, NewInvite};
use spoolframe_admin_api::{LookupStatus, MappingStatus};
use spoolframe_plex::PlexCatalog;
use spoolframe_plex::check::Scope;

pub use crate::admin_http::{AddressRanges, CookiePolicy, Origins, TrustedProxies};
use crate::auth::{
    Approval, Auth, Guess, LoginError, PasswordError, RECENT_SIGN_IN, SessionToken, hash_password,
    hash_pin, pin_digits,
};
use crate::catalog::LiveCatalog;
use crate::config::layers::SettingError;
use crate::config::settings::PLEX_TOKEN;
use crate::config::{FIRST_PLEX_SERVER, PlexConfig};
use crate::device_sign_in::ClaimError;
use crate::libraries::{self, LibraryError};
use crate::network::ServerName;
use crate::oidc::{Begun, Identity, Oidc, OidcError, Purpose, ReturnTo};
use crate::passkeys::{CeremonyError, Passkeys, SignInError};
use crate::plex_accounts::{Link, LinkError, Linking, PlexAccounts};
use crate::plex_sync::{SyncError, WatchSync};
use crate::principal::Principal;
use crate::refusals::{Channel, Kind, Refusal};
use crate::runtime::{ChangeError, RuntimeSettings};
use crate::store::identities::{IdentityError, IdentityRecord};
use crate::store::invites::InviteError;
use crate::store::passkeys::{FirstPasskey, PasskeyError, PasskeyRecord, passkey_name};
use crate::store::pins::{DeviceRecord, PinError};
use crate::store::sessions::{ApiSessionUser, SessionBinding, SessionRecord};
use crate::store::trust::TrustError;
use crate::store::users::{UserError, UserRecord, normalize_username};
use crate::tracks::SubtitleChoice;
use crate::transcode::Transcoding;
use spoolframe_plex::account::{PinState, PlexTv};

#[path = "admin_catalog.rs"]
mod catalog;

pub use catalog::PathChecks;

/// The address the caller is at: its connection's, or behind a trusted
/// proxy the one the proxy forwards for, `None` if the proxy named none
/// ([`TrustedProxies::caller`]).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Peer(pub(crate) Option<IpAddr>);

impl Peer {
    /// What the failure limit counts for this caller besides the username:
    /// its address. A loopback address is shared by everyone who comes
    /// through this machine (the CLI, an SSH tunnel, a proxy), so it names
    /// no one: counting it would let any of them lock all the others out.
    /// So does a trusted proxy that did not say whom it carries.
    fn source(self) -> Option<String> {
        let ip = self.0?.to_canonical();
        (!ip.is_loopback()).then(|| ip.to_string())
    }
}

/// The failure limit's source for the caller.
fn source(ext: &Extensions) -> RpcResult<Option<String>> {
    match ext.get::<Peer>() {
        Some(peer) => Ok(peer.source()),
        None => Err(internal("a request without its peer address")),
    }
}

/// The caller's session token, from the upgrade's Bearer header or the
/// browser's cookie (`crate::admin_http`), if there was one.
#[derive(Debug, Clone)]
pub(crate) struct Bearer(pub(crate) String);

/// The username a sign-in gave, for the refused-request log (finding 46):
/// the method sets it, and the listener reads it once the call is
/// answered. The password beside it is never kept.
#[derive(Debug, Clone, Default)]
pub(crate) struct Attempt(Arc<parking_lot::Mutex<Option<String>>>);

impl Attempt {
    /// Note `username` as the one the caller's sign-in gave.
    fn note(ext: &Extensions, username: &str) {
        if let Some(Attempt(given)) = ext.get::<Attempt>() {
            *given.lock() = Some(crate::refusals::bounded(username));
        }
    }

    fn given(&self) -> Option<String> {
        self.0.lock().clone()
    }
}

/// The web UI's origin as the caller's browser has it open (the socket
/// upgrade's `Origin`, which [`crate::admin_http`] has already checked):
/// where a page that leaves for another site can be sent back to. The CLI
/// sends none.
#[derive(Debug, Clone)]
pub(crate) struct PageOrigin(pub(crate) String);

/// The web UI's page that Plex's sign-in goes on to once it is done: it
/// closes itself, since the page that opened it is the one waiting.
const PLEX_SIGNED_IN: &str = "/auth/plex";

/// Where Plex's sign-in sends the caller's browser back to, if it has one.
fn plex_forward(ext: &Extensions) -> Option<String> {
    ext.get::<PageOrigin>()
        .map(|PageOrigin(origin)| format!("{}{PLEX_SIGNED_IN}", origin.trim_end_matches('/')))
}

/// The one-time code that `setup` needs: printed in the server's log at
/// start while there are no users, so only someone who can read that log
/// (the person installing the server) can claim the first admin.
#[derive(Clone, PartialEq, Eq)]
pub struct SetupCode(String);

impl std::fmt::Debug for SetupCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SetupCode(<redacted>)")
    }
}

/// Says the server has no users yet, where to set it up, and the setup
/// code, in this host's log: never in the OTLP export (PX-09 finding 20).
pub fn log_setup_code(urls: &[String], code: &SetupCode) {
    tracing::warn!(
        target: crate::telemetry::LOCAL_ONLY,
        "no users yet: open the web UI to set up this server, at {} (or register the first \
         admin with `spoolframe-server setup`); the setup code is {code}",
        urls.join(", ")
    );
}

impl std::fmt::Display for SetupCode {
    /// Grouped by four for reading aloud and typing: `K7QD-M2XP-9RTB`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let groups: Vec<&str> = (0..self.0.len())
            .step_by(4)
            .map(|i| &self.0[i..(i + 4).min(self.0.len())])
            .collect();
        f.write_str(&groups.join("-"))
    }
}

impl SetupCode {
    /// No 0/O, 1/I/L or U: nothing to misread.
    const ALPHABET: &'static [u8] = b"ABCDEFGHJKMNPQRSTVWXYZ23456789";
    const LEN: usize = 12;

    /// A fresh random code (about 59 bits).
    pub fn generate() -> Self {
        use password_hash::rand_core::{OsRng, RngCore};
        let code = (0..Self::LEN)
            .map(|_| {
                let i = OsRng.next_u32() as usize % Self::ALPHABET.len();
                Self::ALPHABET[i] as char
            })
            .collect();
        Self(code)
    }

    /// A code as given; dashes, spaces and case do not matter.
    pub fn new(code: &str) -> Self {
        Self(
            code.chars()
                .filter(|c| !matches!(c, '-' | ' '))
                .map(|c| c.to_ascii_uppercase())
                .collect(),
        )
    }

    /// Compared through hashes, so the time taken says nothing about how
    /// much of a guess was right.
    fn matches(&self, given: &str) -> bool {
        blake3::hash(self.0.as_bytes()) == blake3::hash(Self::new(given).0.as_bytes())
    }
}

/// What `serverStatus` reports. The endpoint starts after the admin API,
/// so its key is filled in when it is known.
#[derive(Debug)]
pub struct StatusSource {
    /// A live setting (P3-10): read at each call.
    pub name: ServerName,
    /// Known from the start: it is read before anything listens (PX-14).
    pub server_id: spoolframe_protocol::ServerId,
    pub transcoding: Arc<Transcoding>,
    /// The conversions, for the encode budget's use.
    jobs: Option<Arc<crate::encode::job::Jobs>>,
    /// Whether the endpoint announces itself on the LAN.
    pub mdns: bool,
    endpoint_id: OnceLock<String>,
    addresses: OnceLock<Vec<String>>,
    /// What a signed-in device is told (PX-12), once the endpoint is up.
    told: OnceLock<crate::announce::Told>,
    /// The languages subtitles are read as text in.
    pub ocr_languages: Vec<String>,
    web_urls: OnceLock<Vec<String>>,
}

/// The default of `previews.workers` on this machine, as the settings page
/// shows it.
fn preview_workers_default() -> spoolframe_admin_api::DefaultWorkers {
    use crate::previews::workers::{Counted, default_workers};
    let default = default_workers();
    spoolframe_admin_api::DefaultWorkers {
        count: u32::try_from(default.count).unwrap_or(u32::MAX),
        counted: match default.counted {
            Counted::PhysicalCores => spoolframe_admin_api::CoreCount::PhysicalCores,
            Counted::AvailableCpus => spoolframe_admin_api::CoreCount::AvailableCpus,
            Counted::LogicalCpus => spoolframe_admin_api::CoreCount::LogicalCpus,
        },
    }
}

impl StatusSource {
    pub fn new(
        name: ServerName,
        server_id: spoolframe_protocol::ServerId,
        transcoding: Arc<Transcoding>,
    ) -> Self {
        Self {
            name,
            server_id,
            transcoding,
            jobs: None,
            mdns: false,
            endpoint_id: OnceLock::new(),
            addresses: OnceLock::new(),
            told: OnceLock::new(),
            ocr_languages: Vec::new(),
            web_urls: OnceLock::new(),
        }
    }

    pub fn with_jobs(mut self, jobs: Arc<crate::encode::job::Jobs>) -> Self {
        self.jobs = Some(jobs);
        self
    }

    pub fn with_ocr_languages(mut self, languages: Vec<String>) -> Self {
        self.ocr_languages = languages;
        self
    }

    /// Where the web UI answers, once the listener is up.
    pub fn set_web_urls(&self, urls: Vec<String>) {
        let _ = self.web_urls.set(urls);
    }

    pub fn with_mdns(mut self, mdns: bool) -> Self {
        self.mdns = mdns;
        self
    }

    pub fn set_endpoint_id(&self, id: String) {
        let _ = self.endpoint_id.set(id);
    }

    /// Where the endpoint listens, once it does.
    pub fn set_addresses(&self, addresses: Vec<String>) {
        let _ = self.addresses.set(addresses);
    }

    /// What a signed-in device is told, once the endpoint is up.
    pub fn set_told(&self, told: crate::announce::Told) {
        let _ = self.told.set(told);
    }

    /// What an invite link says of this server (P6-01): its key, its name,
    /// the addresses a signed-in device is told (`network.announce` and
    /// `network.wan`) and its relays; `None` until the endpoint is up.
    /// It reads the interfaces, off the async threads.
    pub(crate) async fn invite(&self) -> Option<spoolframe_protocol::invite::Invite> {
        let server = self.endpoint_id.get()?.clone();
        let told = self.told.get().cloned();
        let (addrs, relays, lookup) = match told {
            Some(told) => {
                tokio::task::spawn_blocking(move || (told.addrs(), told.relays(), told.lookup()))
                    .await
                    .unwrap_or_default()
            }
            None => Default::default(),
        };
        Some(spoolframe_protocol::invite::Invite {
            server,
            name: Some(self.name.get()),
            addrs: addrs
                .into_iter()
                .take(spoolframe_protocol::invite::MAX_HINTS)
                .collect(),
            relays,
            lookup,
            ..Default::default()
        })
    }

    /// The endpoint key's fingerprint (P1-21), once the endpoint is up.
    fn fingerprint(&self) -> Option<String> {
        self.endpoint_id
            .get()
            .and_then(|id| spoolframe_protocol::fingerprint::of_hex(id))
    }

    fn status(&self) -> ServerStatus {
        let encoders = self.transcoding.encoders();
        ServerStatus {
            name: self.name.get(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            revision: revision(BUILD_REVISION),
            endpoint_id: self.endpoint_id.get().cloned(),
            fingerprint: self.fingerprint(),
            server_id: self.server_id.to_string(),
            transcoding: TranscodingStatus {
                video_encoders: encoders.video.iter().map(|e| e.name.clone()).collect(),
                audio_encoders: encoders.audio.iter().map(|e| e.name.clone()).collect(),
                hardware_decoders: encoders
                    .hardware_decoders
                    .iter()
                    .map(|d| d.label())
                    .collect(),
                gpu_filters: self.jobs.as_ref().is_some_and(|j| j.limits().gpu_filters),
                gpu_tonemap: encoders.gpu_tonemap,
                gpu_overlay: encoders.gpu_overlay,
                gpu_tone_maps: crate::playback_debug::gpu_tone_maps(&encoders),
                budget: self.jobs.as_ref().map(|jobs| {
                    let now = jobs.budget();
                    let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
                    spoolframe_admin_api::EncodeBudget {
                        used_quarters: now.used.get(),
                        max_quarters: now.max.get(),
                        remuxes: count(now.remuxes),
                        max_remux: count(now.max_remux),
                        gpu_jobs: count(now.gpu_jobs),
                        max_gpu_jobs: count(now.max_gpu_jobs),
                    }
                }),
            },
            mdns: self.mdns,
            addresses: self.addresses.get().cloned().unwrap_or_default(),
            told_addresses: self.told.get().map_or_else(Vec::new, |told| {
                told.addrs().iter().map(ToString::to_string).collect()
            }),
            wan: self
                .told
                .get()
                .is_some_and(|told| told.public().configured()),
            public_addresses: self.told.get().map_or_else(Vec::new, |told| {
                told.public()
                    .addrs()
                    .into_iter()
                    .map(|found| spoolframe_admin_api::PublicAddress {
                        address: found.addr.to_string(),
                        source: match found.source {
                            crate::wan::Source::Static => {
                                spoolframe_admin_api::AddressSource::Static
                            }
                            crate::wan::Source::MappedPort => {
                                spoolframe_admin_api::AddressSource::MappedPort
                            }
                            crate::wan::Source::Lookup => {
                                spoolframe_admin_api::AddressSource::Lookup
                            }
                        },
                    })
                    .collect()
            }),
            public_notes: self
                .told
                .get()
                .map_or_else(Vec::new, |told| told.public().notes()),
            wan_mapping: mapping_status(
                self.told
                    .get()
                    .and_then(|told| told.public().mapping())
                    .as_ref(),
            ),
            wan_lookup: self.told.get().map_or(LookupStatus::NotUsed, |told| {
                lookup_status(told.public().lookup())
            }),
            ocr_languages: self.ocr_languages.clone(),
            web_urls: self.web_urls.get().cloned().unwrap_or_default(),
            preview_workers_default: preview_workers_default(),
        }
    }
}

/// The router's answer about the mapped port as the admin API says it.
fn mapping_status(mapping: Option<&(u16, crate::wan::Mapping)>) -> MappingStatus {
    use crate::wan::Mapping;
    match mapping {
        None => MappingStatus::NotUsed,
        Some((_, Mapping::Asking)) => MappingStatus::Asking,
        Some((port, Mapping::Granted(external))) => MappingStatus::Granted {
            local_port: *port,
            external: external.to_string(),
        },
        Some((_, Mapping::Refused(reason))) => MappingStatus::Refused {
            reason: reason.clone(),
        },
    }
}

/// The public IP lookup as the admin API says it.
fn lookup_status(lookup: crate::wan::LookupState) -> LookupStatus {
    use crate::wan::LookupState;
    match lookup {
        LookupState::NotUsed => LookupStatus::NotUsed,
        LookupState::NotNeeded => LookupStatus::NotNeeded,
        LookupState::Asking => LookupStatus::Asking,
        LookupState::Found { ip, via, at } => LookupStatus::Found {
            ip: ip.to_string(),
            via: via.name().to_owned(),
            at: at
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        },
        LookupState::Failed { error, retrying } => LookupStatus::Failed { error, retrying },
    }
}

/// What the admin API acts on.
pub struct Admin {
    pub auth: Auth,
    /// The code `setup` accepts; `None` when the server had users at start.
    pub setup_code: Option<SetupCode>,
    pub status: Arc<StatusSource>,
    /// Each linked user's watch sync (PX-04, P3-12); `None` in tests
    /// without.
    pub plex_sync: Option<Arc<WatchSync>>,
    /// Each user's Plex link (P3-12); `None` in tests without.
    pub accounts: Option<Arc<PlexAccounts>>,
    /// For `plexStatus` and `plexCheckPaths`: the catalog the server
    /// serves from, without Plex until it is set up.
    pub plex: Arc<LiveCatalog>,
    /// The relying party passkeys are bound to (P3-05).
    pub passkeys: Arc<Passkeys>,
    /// How the browser's session cookie is sent (`admin.insecure_cookies`).
    pub cookies: CookiePolicy,
    /// The settings changed at run time (P3-10); `None` in tests without.
    pub settings: Option<Arc<RuntimeSettings>>,
    /// What the setup wizard reaches beyond the server.
    pub wizard: Wizard,
    /// Single sign-on (P3-04): the providers, and the flows in progress.
    pub oidc: Arc<Oidc>,
    /// Each user's watch state, for an item as a client of the caller's
    /// gets it (P3-16); `None` in tests without.
    pub watch: Option<Arc<crate::watch::Watch>>,
    /// Artwork, resized, for the catalog pages' thumbnails (P3-16);
    /// `None` in tests without, which get the catalog's own images.
    pub artwork: Option<Arc<crate::artwork::ArtworkCache>>,
    /// What `GET /metrics` answers (P3-14); `None` in tests without, where
    /// it is not found.
    pub metrics: Option<Arc<crate::metrics::Metrics>>,
    /// The last log lines (P3-14); `None` in tests without.
    pub logs: Option<Arc<crate::logs::LogRing>>,
    /// The OTLP export (P3-14), for its status; `None` in tests without.
    pub telemetry: Option<Arc<crate::telemetry::Telemetry>>,
    /// What plays, for playback debug (P3-15); `None` in tests without,
    /// where its methods are `NOT_CONFIGURED`.
    pub playback: Option<Arc<crate::playback_debug::PlaybackDebug>>,
}

/// What the setup wizard reaches beyond the server (P3-10): plex.tv for
/// Sign in with Plex, the LAN for Plex's discovery, and the mount points
/// suggested path mappings are looked for in. Stand-ins in tests.
#[derive(Debug, Clone)]
pub struct Wizard {
    pub plex_tv: url::Url,
    pub plex_app: url::Url,
    pub gdm: Vec<SocketAddr>,
    /// `None`: [`spoolframe_plex::paths::usual_bases`].
    pub path_bases: Option<Vec<std::path::PathBuf>>,
    /// The router, the IP lookup and the host's interfaces, for trying
    /// `network.wan` before it is chosen (P6-02).
    pub wan: crate::wan::Finders,
}

impl Default for Wizard {
    fn default() -> Self {
        Self {
            plex_tv: spoolframe_plex::account::PLEX_TV
                .parse()
                .expect("a fixed URL"),
            plex_app: spoolframe_plex::account::PLEX_APP
                .parse()
                .expect("a fixed URL"),
            gdm: spoolframe_plex::gdm::TARGETS.to_vec(),
            path_bases: None,
            wan: crate::wan::Finders::system(),
        }
    }
}

impl Wizard {
    /// Where plex.tv is, for each user's Plex link (P3-12) as for the
    /// wizard's.
    pub fn plex_site(&self) -> crate::plex_accounts::PlexSite {
        crate::plex_accounts::PlexSite {
            api: self.plex_tv.clone(),
            app: self.plex_app.clone(),
        }
    }
}

/// Where a browser can open the web UI on a listener bound to `listen`:
/// that address, or for a wildcard every address of this host's
/// (link-local IPv6 left out: it needs a zone a URL cannot carry), the
/// LAN's first and loopback last. What the log prints at start.
pub fn web_urls(listen: SocketAddr) -> Vec<String> {
    urls(listen, &host_addresses())
}

fn urls(listen: SocketAddr, host: &[IpAddr]) -> Vec<String> {
    let addresses: Vec<IpAddr> = match listen.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => {
            host.iter().copied().filter(IpAddr::is_ipv4).collect()
        }
        IpAddr::V6(ip) if ip.is_unspecified() => host.to_vec(),
        ip => vec![ip],
    };
    let mut addresses: Vec<IpAddr> = addresses
        .into_iter()
        .filter(|ip| match ip {
            IpAddr::V6(v6) => !v6.is_unicast_link_local(),
            IpAddr::V4(_) => true,
        })
        .collect();
    addresses.sort_by_key(|ip| (ip.is_loopback(), ip.is_ipv6()));
    addresses.dedup();
    addresses
        .into_iter()
        .map(|ip| format!("http://{}/", SocketAddr::new(ip, listen.port())))
        .collect()
}

/// This host's interface addresses, of the interfaces that are up.
fn host_addresses() -> Vec<IpAddr> {
    crate::announce::interfaces()
        .into_iter()
        .flat_map(|i| i.addrs)
        .collect()
}

/// Start the admin API on `listen`. Browsers are let in from `origins`;
/// `proxies` are believed about whom they carry.
pub async fn serve(
    listen: SocketAddr,
    admin: Admin,
    origins: Origins,
    proxies: TrustedProxies,
) -> std::io::Result<(SocketAddr, ServerHandle)> {
    // Live sessions are counted from the store the API signs in with.
    if let Some(metrics) = &admin.metrics {
        metrics.observe_store(admin.auth.store().clone());
    }
    let checks = Arc::new(
        admin
            .settings
            .as_ref()
            .map_or_else(PathChecks::default, |s| PathChecks::kept_in(s.state_dir())),
    );
    let api = AdminApi {
        auth: admin.auth,
        setup_code: admin.setup_code,
        status: admin.status,
        plex_sync: admin.plex_sync,
        accounts: admin.accounts,
        plex: admin.plex,
        passkeys: admin.passkeys,
        cookies: admin.cookies,
        settings: admin.settings,
        wizard: admin.wizard,
        oidc: admin.oidc,
        watch: admin.watch,
        artwork: admin.artwork,
        metrics: admin.metrics,
        logs: admin.logs,
        telemetry: admin.telemetry,
        playback: admin.playback,
        checks,
        pins: Arc::default(),
        signed_in: Arc::default(),
        public_url: origins.public_url().map(str::to_owned),
    };
    let mut methods = AdminApiServer::into_rpc(api.clone());
    methods
        .merge(MeApiServer::into_rpc(api.clone()))
        .expect("the me and admin namespaces never share a name");
    crate::admin_http::serve(listen, api, methods.into(), origins, proxies).await
}

#[derive(Clone)]
pub(crate) struct AdminApi {
    auth: Auth,
    setup_code: Option<SetupCode>,
    status: Arc<StatusSource>,
    plex_sync: Option<Arc<WatchSync>>,
    accounts: Option<Arc<PlexAccounts>>,
    plex: Arc<LiveCatalog>,
    passkeys: Arc<Passkeys>,
    cookies: CookiePolicy,
    settings: Option<Arc<RuntimeSettings>>,
    wizard: Wizard,
    oidc: Arc<Oidc>,
    watch: Option<Arc<crate::watch::Watch>>,
    artwork: Option<Arc<crate::artwork::ArtworkCache>>,
    metrics: Option<Arc<crate::metrics::Metrics>>,
    logs: Option<Arc<crate::logs::LogRing>>,
    telemetry: Option<Arc<crate::telemetry::Telemetry>>,
    playback: Option<Arc<crate::playback_debug::PlaybackDebug>>,
    /// Each library's last path check (P3-16).
    checks: Arc<PathChecks>,
    /// The plex.tv PINs this server began, so a check is only of those.
    pins: Arc<parking_lot::Mutex<std::collections::HashSet<u64>>>,
    /// The account token the wizard's last Sign in with Plex got: whose
    /// servers the wizard offers before anyone has linked an account. In
    /// memory only; `plex.token` keeps it too, until the token of the
    /// server chosen replaces it.
    signed_in: Arc<parking_lot::Mutex<Option<String>>>,
    /// Where browsers reach the web UI (`admin.public_url`), if anywhere.
    public_url: Option<String>,
}

fn error(code: i32, message: impl Into<String>) -> ErrorObjectOwned {
    ErrorObjectOwned::owned::<()>(code, message.into(), None)
}

fn internal(e: impl std::fmt::Display) -> ErrorObjectOwned {
    tracing::error!(error = %e, "admin API");
    error(
        jsonrpsee::types::error::INTERNAL_ERROR_CODE,
        "internal error",
    )
}

impl From<LoginError> for ErrorObjectOwned {
    fn from(e: LoginError) -> Self {
        match e {
            LoginError::InvalidCredentials => {
                error(error_code::INVALID_CREDENTIALS, "invalid credentials")
            }
            LoginError::RateLimited { retry_after } => ErrorObjectOwned::owned(
                error_code::RATE_LIMITED,
                "too many failed logins",
                Some(json!({ "retryAfterMs": retry_after.as_millis() as u64 })),
            ),
            LoginError::Internal(e) => internal(e),
        }
    }
}

impl From<InviteError> for ErrorObjectOwned {
    fn from(e: InviteError) -> Self {
        match e {
            InviteError::NotYet(_) => error(error_code::INVALID_INPUT, e.to_string()),
            InviteError::NoSuchInvite(_) => error(error_code::NO_SUCH_INVITE, e.to_string()),
            InviteError::Store(e) => internal(e),
        }
    }
}

/// The admin API's invite kind as the store has it.
fn store_invite_kind(kind: InviteKind) -> crate::store::invites::InviteKind {
    use crate::store::invites::InviteKind as Stored;
    match kind {
        InviteKind::CreateUser => Stored::CreateUser,
        InviteKind::PortableAccount => Stored::PortableAccount,
        InviteKind::Federation => Stored::Federation,
    }
}

/// A kept invite as the admin API shows it.
fn invite_entry(record: crate::store::invites::InviteRecord) -> InviteEntry {
    use crate::store::invites::InviteKind as Stored;
    InviteEntry {
        id: record.id,
        kind: match record.kind {
            Stored::CreateUser => InviteKind::CreateUser,
            Stored::PortableAccount => InviteKind::PortableAccount,
            Stored::Federation => InviteKind::Federation,
        },
        note: record.note,
        role: record.role,
        uses: record.uses,
        uses_left: record.uses_left,
        expires_at: record.expires_at,
        created_by: record.created_by,
        created_at: record.created_at,
    }
}

impl From<UserError> for ErrorObjectOwned {
    fn from(e: UserError) -> Self {
        match &e {
            UserError::Exists(_) => error(error_code::USER_EXISTS, e.to_string()),
            UserError::NoSuchUser(_) => error(error_code::NO_SUCH_USER, e.to_string()),
            UserError::InvalidUsername(_) | UserError::InvalidDisplayName => {
                error(error_code::INVALID_INPUT, e.to_string())
            }
            UserError::AlreadySetUp => error(error_code::ALREADY_SET_UP, e.to_string()),
            UserError::LastAdmin(_) | UserError::LastPasswordAdmin(_) => {
                error(error_code::LAST_ADMIN, e.to_string())
            }
            UserError::LastSignInMethod(_) => error(error_code::LAST_SIGN_IN_METHOD, e.to_string()),
            UserError::NoPassword(_) => error(error_code::INVALID_INPUT, e.to_string()),
            UserError::Store(_) => internal(e),
        }
    }
}

impl From<PasswordError> for ErrorObjectOwned {
    fn from(e: PasswordError) -> Self {
        match e {
            PasswordError::TooShort => error(error_code::INVALID_INPUT, e.to_string()),
            PasswordError::Hash(_) => internal(e),
        }
    }
}

impl From<PasskeyError> for ErrorObjectOwned {
    fn from(e: PasskeyError) -> Self {
        match &e {
            PasskeyError::NoSuchPasskey => error(error_code::NO_SUCH_PASSKEY, e.to_string()),
            PasskeyError::Exists | PasskeyError::InvalidName => {
                error(error_code::INVALID_INPUT, e.to_string())
            }
            PasskeyError::LastSignInMethod => error(error_code::LAST_SIGN_IN_METHOD, e.to_string()),
            PasskeyError::Store(_) => internal(e),
        }
    }
}

impl From<SignInError> for ErrorObjectOwned {
    fn from(e: SignInError) -> Self {
        match e {
            SignInError::Refused => error(error_code::PASSKEY_REFUSED, e.to_string()),
            SignInError::RateLimited { retry_after } => {
                LoginError::RateLimited { retry_after }.into()
            }
            SignInError::Unavailable(why) => error(error_code::NOT_CONFIGURED, why),
            SignInError::Internal(e) => internal(e),
        }
    }
}

impl From<CeremonyError> for ErrorObjectOwned {
    fn from(e: CeremonyError) -> Self {
        match e {
            CeremonyError::Unavailable(why) => error(error_code::NOT_CONFIGURED, why),
            CeremonyError::NoCeremony | CeremonyError::Refused(_) => {
                error(error_code::INVALID_INPUT, e.to_string())
            }
            CeremonyError::Passkey(e) => e.into(),
            CeremonyError::Internal(e) => internal(e),
        }
    }
}

impl AdminApi {
    /// What a signed-out page shows and says: the server's name, and the
    /// public URL to send a browser to when its own address cannot keep
    /// the session cookie (plain HTTP on a real host).
    pub(crate) fn site(&self) -> serde_json::Value {
        json!({
            "name": self.status.name.get(),
            "publicUrl": self.public_url,
            "insecureCookies": self.cookies.insecure(),
            "sso": self.oidc.names(),
        })
    }

    /// Note that the session `token` is the web UI's: what set it as a
    /// browser's cookie (P3-18). Only a label for the session list, so a
    /// failure is logged and the sign-in goes on.
    pub(crate) async fn mark_web_session(&self, token: &str) {
        let hash = crate::auth::SessionToken(token.to_owned()).hash();
        if let Err(e) = self.auth.store().mark_web_session(&hash).await {
            tracing::warn!(error = %e, "cannot mark a session as the web UI's");
        }
    }

    /// The server's metrics, if it keeps any.
    pub(crate) fn metrics(&self) -> Option<&crate::metrics::Metrics> {
        self.metrics.as_deref()
    }

    pub(crate) fn metrics_arc(&self) -> Option<Arc<crate::metrics::Metrics>> {
        self.metrics.clone()
    }

    /// Playback debug's parts (P3-15), if this server has them.
    fn playback_debug(&self) -> RpcResult<&crate::playback_debug::PlaybackDebug> {
        self.playback.as_deref().ok_or_else(|| {
            error(
                error_code::NOT_CONFIGURED,
                "this admin API has no playback to look at",
            )
        })
    }

    /// How the session cookie is sent now.
    pub(crate) fn cookies(&self) -> &CookiePolicy {
        &self.cookies
    }

    /// Why the server cannot offer passkeys, if it cannot.
    pub(crate) fn passkeys_unavailable(&self) -> Option<&str> {
        self.passkeys.unavailable()
    }

    /// Whether `setup` would take a code now: no users, and a code was
    /// made at start. What a browser's first page decides on.
    pub(crate) async fn setup_open(&self) -> RpcResult<bool> {
        if self.setup_code.is_none() {
            return Ok(false);
        }
        Ok(self.auth.store().user_count().await? == 0)
    }

    /// Whether `given` is the setup code, while setup can be done at all;
    /// guesses are limited as failed logins are, per caller (PX-09 finding
    /// 24): under one name for all, five wrong codes from anyone kept the
    /// owner from setting the server up. The code is 59 random bits, so
    /// guesses from many places at five each get nowhere.
    fn setup_code(&self, ext: &Extensions, given: &str) -> RpcResult<()> {
        let Some(code) = &self.setup_code else {
            return Err(UserError::AlreadySetUp.into());
        };
        let source = source(ext)?;
        let what = format!("setup@{}", source.as_deref().unwrap_or_default());
        // Counted before it is checked (A-13), and taken back if right.
        let guess = self
            .auth
            .guess(what, source)
            .map_err(|retry_after| LoginError::RateLimited { retry_after })?;
        if !code.matches(given) {
            return Err(error(
                error_code::INVALID_SETUP_CODE,
                "that is not the setup code in the server's log",
            ));
        }
        self.auth.guessed_right(guess);
        Ok(())
    }

    /// Log that `request` was refused with `code` (finding 46), if that
    /// code is a refusal and the log is not full for the caller's address
    /// ([`crate::refusals`]). Who it was is looked up only then.
    pub(crate) async fn refused(
        &self,
        channel: Channel,
        request: &str,
        ext: &Extensions,
        code: i32,
    ) {
        let bearer = ext.get::<Bearer>();
        let kind = match code {
            error_code::UNAUTHENTICATED if bearer.is_none() => Kind::NoSession,
            error_code::UNAUTHENTICATED => Kind::Unauthenticated,
            error_code::FORBIDDEN => Kind::Forbidden,
            error_code::RECENT_SIGN_IN_REQUIRED => Kind::RecentSignInRequired,
            error_code::RATE_LIMITED => Kind::RateLimited,
            error_code::INVALID_CREDENTIALS
            | error_code::INVALID_SETUP_CODE
            | error_code::PASSKEY_REFUSED
            | error_code::NO_SUCH_SIGN_IN_CODE => Kind::SignInFailed,
            _ => return,
        };
        // The address itself, loopback included: unlike the failure
        // limit's source, the log is not shared out by it.
        let source = ext
            .get::<Peer>()
            .and_then(|Peer(ip)| *ip)
            .map_or_else(|| "-".to_owned(), |ip| ip.to_canonical().to_string());
        let refusals = self.auth.refusals();
        if !refusals.admit(&source, kind) {
            return;
        }
        let owner = match bearer {
            Some(Bearer(token)) => self.auth.session_owner(token).await.ok().flatten(),
            None => None,
        };
        let given = ext.get::<Attempt>().and_then(Attempt::given);
        let reason = match kind {
            Kind::NoSession => "no session",
            Kind::Unauthenticated => match &owner {
                None => "no such session",
                Some(owner) if !owner.live => "session ended",
                Some(owner) if owner.endpoint_id.is_some() => {
                    "an app's session, not the admin API's"
                }
                Some(_) => "session is not valid",
            },
            Kind::Forbidden => "admins only",
            Kind::RecentSignInRequired => "sign-in too old for this change",
            Kind::RateLimited => "rate-limited",
            Kind::SignInFailed => match code {
                error_code::INVALID_CREDENTIALS => match &given {
                    Some(username) => self.auth.why_password_refused(username).await,
                    None => "wrong password",
                },
                error_code::INVALID_SETUP_CODE => "wrong setup code",
                error_code::PASSKEY_REFUSED => "passkey refused",
                _ => "wrong device sign-in code",
            },
            Kind::ProfileLocked => "profile locked",
            // Only the client protocol has a Hello.
            Kind::HelloRefused => return,
        };
        crate::refusals::log(&Refusal {
            kind,
            channel,
            request: request.to_owned(),
            source,
            user: given.or(owner.map(|owner| owner.username)),
            reason: reason.to_owned(),
        });
    }

    /// The caller's live session, checked on every call so a revoked
    /// session or a disabled user stops at once.
    async fn caller(&self, ext: &Extensions) -> RpcResult<(String, ApiSessionUser)> {
        let Some(Bearer(token)) = ext.get::<Bearer>() else {
            return Err(error(error_code::UNAUTHENTICATED, "log in first"));
        };
        match self.auth.api_session(token).await.map_err(internal)? {
            Some(user) => Ok((token.clone(), user)),
            None => Err(error(error_code::UNAUTHENTICATED, "session is not valid")),
        }
    }

    /// The caller, if an admin. The role is the user's now, not when the
    /// session began: a demoted admin's next `admin.*` call is refused.
    async fn admin(&self, ext: &Extensions) -> RpcResult<ApiSessionUser> {
        let (_, user) = self.caller(ext).await?;
        if user.role.is_admin() {
            Ok(user)
        } else {
            Err(error(error_code::FORBIDDEN, "admins only"))
        }
    }

    /// The caller, if they proved who they are in this session within
    /// [`RECENT_SIGN_IN`] (P3-03): what a sensitive change needs, so a
    /// session left open, or its stolen cookie, cannot make one.
    async fn recently_signed_in(&self, ext: &Extensions) -> RpcResult<(String, ApiSessionUser)> {
        let (token, user) = self.caller(ext).await?;
        recent(&user)?;
        Ok((token, user))
    }

    /// The caller, if an admin who signed in within [`RECENT_SIGN_IN`]
    /// (PX-09 finding 3): what an admin change needs that could take over
    /// an account or the server, so an admin session left open, or its
    /// stolen cookie, cannot make one. The role is checked first, so a
    /// consumer is told `FORBIDDEN`, not asked to confirm.
    async fn recent_admin(&self, ext: &Extensions) -> RpcResult<ApiSessionUser> {
        let user = self.admin(ext).await?;
        recent(&user)?;
        Ok(user)
    }

    /// The caller, `typed` as a device sign-in code (P3-07) and the guess
    /// it is, if they may try one now. Any live web session may: it was
    /// made by a full sign-in, and the PIN lock (P2-08) is a device's,
    /// which a browser is not. Codes are short, so guessing them is
    /// limited as passwords are, against `user` and where they are: the
    /// guess is counted as wrong before it is checked (A-13), and only
    /// [`Auth::guessed_right`] takes it back. A malformed code is a wrong
    /// one.
    async fn sign_in_code(
        &self,
        ext: &Extensions,
        typed: &str,
    ) -> RpcResult<(ApiSessionUser, String, Guess)> {
        let (_, user) = self.caller(ext).await?;
        let (what, from) = sign_in_code_keys(ext, user)?;
        let guess = self
            .auth
            .guess(what, from)
            .map_err(|retry_after| LoginError::RateLimited { retry_after })?;
        match spoolframe_protocol::device_sign_in::normalize(typed) {
            Some(code) => Ok((user, code, guess)),
            None => Err(wrong_sign_in_code()),
        }
    }

    /// The account `user` is.
    async fn account(&self, user: ApiSessionUser) -> RpcResult<UserRecord> {
        self.auth
            .store()
            .user_by_id(user.user)
            .await?
            .ok_or_else(|| error(error_code::UNAUTHENTICATED, "session is not valid"))
    }

    /// A stored passkey as the API shows it.
    fn passkey_entry(&self, p: PasskeyRecord) -> PasskeyEntry {
        // An app's made for the relying party every server once shared is
        // an app's that signs in nowhere now (PX-14).
        PasskeyEntry {
            usable: p.rp_id == self.passkeys.app_rp_id()
                || self.passkeys.rp_id() == Some(p.rp_id.as_str()),
            app: spoolframe_protocol::is_app_rp_id(&p.rp_id),
            id: p.credential_id,
            name: p.name,
            created_at: p.created_at,
            last_used_at: p.last_used_at,
            rp_id: p.rp_id,
        }
    }

    async fn passkey_entries(
        &self,
        user: crate::store::users::LocalUserId,
    ) -> RpcResult<Vec<PasskeyEntry>> {
        let passkeys = self
            .auth
            .store()
            .list_passkeys(user)
            .await
            .map_err(internal)?;
        Ok(passkeys
            .into_iter()
            .map(|p| self.passkey_entry(p))
            .collect())
    }

    /// `u` as the API shows it, with whether their logins are limited now.
    fn info(&self, u: UserRecord) -> UserInfo {
        let limited = self.auth.login_limited(&u.username);
        info(u, limited)
    }

    async fn user_info(&self, username: &str) -> RpcResult<UserInfo> {
        let name = normalize_username(username)?;
        let users = self.auth.store().list_users().await?;
        users
            .into_iter()
            .find(|u| u.username == name)
            .map(|u| self.info(u))
            .ok_or_else(|| UserError::NoSuchUser(name).into())
    }
}

impl From<ChangeError> for ErrorObjectOwned {
    fn from(e: ChangeError) -> Self {
        match &e {
            ChangeError::Setting(SettingError::Locked { .. }) => {
                error(error_code::SETTING_LOCKED, e.to_string())
            }
            ChangeError::Setting(_) | ChangeError::Secret { .. } => {
                error(error_code::INVALID_INPUT, e.to_string())
            }
            ChangeError::Write(_) => internal(e),
        }
    }
}

impl From<LibraryError> for ErrorObjectOwned {
    fn from(e: LibraryError) -> Self {
        match e {
            LibraryError::NoSuchLibrary(_)
            | LibraryError::NoSuchServer(_)
            | LibraryError::Invalid(_) => error(error_code::INVALID_INPUT, e.to_string()),
            LibraryError::Unavailable(_) => error(error_code::UNAVAILABLE, e.to_string()),
            LibraryError::NotSetUp => error(error_code::NOT_CONFIGURED, e.to_string()),
            LibraryError::Change(e) => e.into(),
            LibraryError::Config(e) => internal(e),
        }
    }
}

impl From<LinkError> for ErrorObjectOwned {
    fn from(e: LinkError) -> Self {
        match &e {
            LinkError::NotYourPin => error(error_code::INVALID_INPUT, e.to_string()),
            LinkError::TooMany { retry_after } => LoginError::RateLimited {
                retry_after: *retry_after,
            }
            .into(),
            LinkError::ByConfig => error(error_code::NOT_CONFIGURED, e.to_string()),
            LinkError::PlexTv(_) => error(error_code::UNAVAILABLE, e.to_string()),
            LinkError::Io(_) | LinkError::Users(_) => internal(e),
        }
    }
}

impl From<IdentityError> for ErrorObjectOwned {
    fn from(e: IdentityError) -> Self {
        match &e {
            IdentityError::NoSuchLink => error(error_code::NO_SUCH_LINK, e.to_string()),
            IdentityError::LinkedToAnother | IdentityError::ProviderLinked => {
                error(error_code::LINK_REFUSED, e.to_string())
            }
            IdentityError::LastSignInMethod => {
                error(error_code::LAST_SIGN_IN_METHOD, e.to_string())
            }
            IdentityError::Store(_) => internal(e),
        }
    }
}

impl From<OidcError> for ErrorObjectOwned {
    fn from(e: OidcError) -> Self {
        match &e {
            OidcError::NoSuchProvider(_) | OidcError::NotConfigured(_) => {
                error(error_code::NOT_CONFIGURED, e.to_string())
            }
            OidcError::Unavailable { provider, .. } => error(
                error_code::UNAVAILABLE,
                format!(
                    "{provider} can't be reached right now. Sign in another way, or try again later."
                ),
            ),
            OidcError::Busy => error(error_code::UNAVAILABLE, e.to_string()),
            OidcError::NoFlow | OidcError::Refused { .. } => {
                error(error_code::INVALID_INPUT, e.to_string())
            }
        }
    }
}

/// What a sign-in provider's callback comes to (P3-04), for the browser.
#[derive(Debug)]
pub(crate) enum SsoOutcome {
    /// Signed in: the session cookie, then the page the sign-in began on,
    /// or the start page.
    SignedIn { done: LoggedIn, to: String },
    /// Back to a page, which says what happened: `/login?sso=unlinked`.
    Back(String),
}

/// Where a sign-in that did not sign in goes: the page it began on, else
/// the login page.
fn sign_in_page(return_to: Option<&ReturnTo>) -> &str {
    return_to.map_or("/login", ReturnTo::as_str)
}

/// What happened, as the pages read it from `?sso=`, after whatever query
/// `page` has.
fn back(page: &str, what: &str, provider: Option<&str>) -> SsoOutcome {
    let mut url = url::Url::parse("http://page.invalid")
        .and_then(|base| base.join(page))
        .expect("a page on this server");
    url.set_fragment(None);
    url.query_pairs_mut().append_pair("sso", what);
    if let Some(name) = provider {
        url.query_pairs_mut().append_pair("provider", name);
    }
    SsoOutcome::Back(format!(
        "{}?{}",
        url.path(),
        url.query().unwrap_or_default()
    ))
}

/// A failed flow, as the page it began on says it.
fn failed(page: &str, e: &OidcError) -> SsoOutcome {
    match e {
        OidcError::Unavailable { provider, .. } => back(page, "unavailable", Some(provider)),
        OidcError::NoFlow => back(page, "expired", None),
        OidcError::Refused { provider, .. } => back(page, "failed", Some(provider)),
        OidcError::NoSuchProvider(_) | OidcError::NotConfigured(_) | OidcError::Busy => {
            back(page, "failed", None)
        }
    }
}

impl From<TrustError> for ErrorObjectOwned {
    fn from(e: TrustError) -> Self {
        match e {
            TrustError::NoSuchDevice => error(error_code::NO_SUCH_DEVICE, e.to_string()),
            TrustError::AutoLoginUntrusted => error(error_code::INVALID_INPUT, e.to_string()),
            TrustError::Store(_) => internal(e),
        }
    }
}

impl From<PinError> for ErrorObjectOwned {
    fn from(e: PinError) -> Self {
        match &e {
            PinError::NoPin => error(error_code::INVALID_INPUT, e.to_string()),
            PinError::NoSuchDevice => error(error_code::NO_SUCH_DEVICE, e.to_string()),
            PinError::Store(_) => internal(e),
        }
    }
}

/// The answer for a code that is not pending.
fn wrong_sign_in_code() -> ErrorObjectOwned {
    error(
        error_code::NO_SUCH_SIGN_IN_CODE,
        "that is not a sign-in code a device is showing now",
    )
}

/// The failure limit's keys for device sign-in codes tried by `user` from
/// a browser: the user's is the one their phone counts under too
/// (`crate::client`), so both share one allowance.
fn sign_in_code_keys(
    ext: &Extensions,
    user: ApiSessionUser,
) -> RpcResult<(String, Option<String>)> {
    Ok((
        format!("devcode:{}", user.user),
        source(ext)?.map(|address| format!("devcode@{address}")),
    ))
}

fn device(d: DeviceRecord) -> DeviceEntry {
    DeviceEntry {
        platform: d.platform(),
        endpoint_id: d.endpoint_id,
        name: d.name,
        last_seen_at: d.last_seen_at,
        pin_required: d.pin_required,
        trusted: d.trusted,
        auto_login: d.auto_login,
    }
}

/// `RECENT_SIGN_IN_REQUIRED` unless `user` proved who they are in this
/// session within [`RECENT_SIGN_IN`].
fn recent(user: &ApiSessionUser) -> RpcResult<()> {
    let age = crate::store::now() - user.authenticated_at;
    if age > RECENT_SIGN_IN.as_secs() as i64 {
        return Err(error(
            error_code::RECENT_SIGN_IN_REQUIRED,
            "confirm it is you: enter your password, or sign in again",
        ));
    }
    Ok(())
}

/// Whether a setting is one whose change could take the server over: the
/// admin listener, its cookies, proxies and public URL (`admin.*`),
/// sign-in providers (`auth.*`), the programs the server runs and
/// whether they are confined (`media.*`, PX-11 finding 34), the first
/// Plex server's token (`plex.token`, finding 44), as
/// `plexServerSetToken` is for every server's, and where the kept tokens
/// are sent (finding 47): the first server's address (`plex.url`) and the
/// others' (`plex.servers`, one key with their names and path mappings,
/// so all of it). The file read and sent as the first server's token
/// (`plex.token_file`, finding 57) is that token as much as `plex.token`
/// is. A name alone (`plex.name`) sends nothing anywhere. And
/// the OTLP export (`telemetry.*`, finding 56): its endpoint receives
/// every span, metric and log record, refusal lines with usernames
/// among them, and its headers are a secret sent with each export.
/// And the network (`network.*`, finding 61): the endpoint's port and
/// relays, the addresses it announces and its public ones (`network.wan`,
/// P6-02) are where signed-in devices are told to connect. Pinning keeps
/// another host from passing for this server, but a device still
/// connects out to an address it is given and so shows where it is.
fn sensitive_setting(key: &str) -> bool {
    key.starts_with("admin.")
        || key.starts_with("auth.")
        || key.starts_with("media.")
        || key.starts_with("network.")
        || key.starts_with("telemetry.")
        || [PLEX_TOKEN, "plex.token_file", "plex.url", "plex.servers"].contains(&key)
}

impl AdminApi {
    /// The runtime settings, and the admin changing them, by name for the
    /// log.
    async fn settings_as_admin(&self, ext: &Extensions) -> RpcResult<(&RuntimeSettings, String)> {
        let user = self.admin(ext).await?;
        let Some(settings) = &self.settings else {
            return Err(error(
                error_code::NOT_CONFIGURED,
                "this server does not change settings at run time",
            ));
        };
        Ok((settings, self.account(user).await?.username))
    }

    /// As [`AdminApi::settings_as_admin`], for a change that needs a recent
    /// sign-in when `sensitive`.
    async fn settings_as_recent_admin(
        &self,
        ext: &Extensions,
        sensitive: bool,
    ) -> RpcResult<(&RuntimeSettings, String)> {
        let (settings, by) = self.settings_as_admin(ext).await?;
        if sensitive {
            self.recent_admin(ext).await?;
        }
        Ok((settings, by))
    }

    fn plex(&self) -> RpcResult<Arc<PlexCatalog>> {
        self.plex.plex().ok_or_else(|| {
            error(
                error_code::NOT_CONFIGURED,
                "this server is not set up: no Plex server is configured",
            )
        })
    }

    /// Every user's Plex link (P3-12).
    fn accounts(&self) -> RpcResult<&PlexAccounts> {
        self.accounts.as_deref().ok_or_else(|| {
            error(
                error_code::NOT_CONFIGURED,
                "this server does not link Plex accounts",
            )
        })
    }

    /// `user`, their Plex link and their last run, as the API shows them.
    fn plex_user(&self, user: UserRecord, link: Option<Link>) -> PlexUser {
        let (last_run, running) = match (&self.plex_sync, &link) {
            (Some(sync), Some(_)) => sync.status(user.id),
            _ => (None, false),
        };
        PlexUser {
            username: user.username,
            display_name: user.display_name,
            link: link.map(|l| l.shown()),
            last_run,
            running,
        }
    }

    /// The token the caller's Plex account is reached with: their own
    /// link's, or, for the config's owner, the first server's.
    fn account_token(&self, user: crate::store::users::LocalUserId) -> RpcResult<Option<String>> {
        let accounts = self.accounts()?;
        Ok(match accounts.link(user).map_err(internal)? {
            Some(Link::Account(a)) => Some(a.token),
            Some(Link::Owner) => self.settings.as_ref().and_then(|s| s.plex_token()),
            None => None,
        })
    }

    /// The caller's own account.
    async fn me(&self, ext: &Extensions) -> RpcResult<UserRecord> {
        let (_, user) = self.caller(ext).await?;
        self.auth
            .store()
            .user_by_id(user.user)
            .await?
            .ok_or_else(|| error(error_code::UNAUTHENTICATED, "session is not valid"))
    }

    /// A linked identity as the API shows it.
    fn sso_link(&self, r: IdentityRecord) -> SsoLink {
        SsoLink {
            provider: self.oidc.provider_of(&r.issuer),
            issuer: r.issuer,
            display: r.display,
            linked_at: r.linked_at,
            last_used_at: r.last_used_at,
        }
    }

    async fn sso_entries(&self, user: crate::store::users::LocalUserId) -> RpcResult<Vec<SsoLink>> {
        let links = self
            .auth
            .store()
            .user_identities(user)
            .await
            .map_err(internal)?;
        Ok(links.into_iter().map(|r| self.sso_link(r)).collect())
    }

    /// Begin a flow with sign-in provider `provider` (P3-04): `signIn`
    /// with no session; `link` (needs a recent sign-in, as adding a
    /// passkey does) and `confirm` in the caller's session.
    pub(crate) async fn sso_begin(
        &self,
        ext: &Extensions,
        provider: &str,
        purpose: &str,
        return_to: Option<&str>,
    ) -> RpcResult<Begun> {
        // Only a sign-in goes back to where it began; a link or a
        // confirmation goes back to My profile.
        if return_to.is_some() && purpose != "signIn" {
            return Err(error(
                error_code::INVALID_INPUT,
                "only a sign-in returns to the page it began on",
            ));
        }
        let return_to = match return_to.map(|r| (r, ReturnTo::parse(r))) {
            None => None,
            Some((_, Some(page))) => Some(page),
            Some((text, None)) => {
                return Err(error(
                    error_code::INVALID_INPUT,
                    format!("{text:?} is not a page on this server: a path starting with one /"),
                ));
            }
        };
        let purpose = match purpose {
            "signIn" => Purpose::SignIn { return_to },
            "link" => {
                let (token, user) = self.recently_signed_in(ext).await?;
                Purpose::Link {
                    user: user.user,
                    session: SessionToken(token).hash(),
                }
            }
            "confirm" => {
                let (token, user) = self.caller(ext).await?;
                Purpose::Confirm {
                    user: user.user,
                    session: SessionToken(token).hash(),
                }
            }
            other => {
                return Err(error(
                    error_code::INVALID_INPUT,
                    format!("{other:?} is not signIn, link or confirm"),
                ));
            }
        };
        let source = source(ext)?;
        Ok(self
            .oidc
            .begin(provider, purpose, source.as_deref())
            .await?)
    }

    /// The provider's callback: `code` (or `declined`, its `error`) for
    /// the flow under `state`, from the browser whose flow cookie is
    /// `browser`. Never an error: each outcome is a page to go to.
    pub(crate) async fn sso_callback(
        &self,
        state: &str,
        code: Option<&str>,
        browser: Option<&str>,
    ) -> SsoOutcome {
        let Some(code) = code else {
            return match self.oidc.decline(state, browser) {
                Ok(Purpose::SignIn { return_to }) => {
                    back(sign_in_page(return_to.as_ref()), "cancelled", None)
                }
                Ok(_) => back("/profile", "cancelled", None),
                Err(e) => failed("/login", &e),
            };
        };
        match self.oidc.finish(state, code, browser).await {
            Ok((purpose, identity)) => match self.sso_done(purpose, identity).await {
                Ok(outcome) => outcome,
                Err(e) => {
                    tracing::error!(error = %e.message(), "single sign-on");
                    back("/login", "failed", None)
                }
            },
            Err((Some(Purpose::SignIn { return_to }), e)) => {
                failed(sign_in_page(return_to.as_ref()), &e)
            }
            Err((None, e)) => failed("/login", &e),
            Err((Some(_), e)) => failed("/profile", &e),
        }
    }

    async fn sso_done(&self, purpose: Purpose, identity: Identity) -> RpcResult<SsoOutcome> {
        let store = self.auth.store();
        let provider = identity.provider.as_str();
        match purpose {
            Purpose::SignIn { return_to } => {
                let page = sign_in_page(return_to.as_ref());
                let Some(user) = store
                    .identity_signs_in(&identity.issuer, &identity.subject)
                    .await
                    .map_err(internal)?
                else {
                    // Never an account made, never one found by email.
                    tracing::info!(
                        provider,
                        "an account not linked to any user tried to sign in"
                    );
                    return Ok(back(page, "unlinked", Some(provider)));
                };
                let Some(record) = store.user_by_id(user).await? else {
                    return Ok(back(page, "failed", Some(provider)));
                };
                if record.disabled_at.is_some() {
                    tracing::info!(username = %record.username, provider, "a disabled user tried to sign in");
                    return Ok(back(page, "failed", Some(provider)));
                }
                let token = self.auth.open_api_session(user).await.map_err(internal)?;
                tracing::info!(username = %record.username, provider, "signed in with a sign-in provider (API session)");
                Ok(SsoOutcome::SignedIn {
                    done: LoggedIn {
                        token: token.0,
                        username: record.username,
                        role: record.role,
                    },
                    to: return_to.map_or_else(|| "/".to_owned(), |r| r.as_str().to_owned()),
                })
            }
            Purpose::Link { user, session } => {
                if !self.session_is(&session, user).await? {
                    return Ok(back("/login", "expired", None));
                }
                match store
                    .link_identity(
                        user,
                        &identity.issuer,
                        &identity.subject,
                        identity.display.as_deref(),
                    )
                    .await
                {
                    Ok(_) => {
                        tracing::info!(user = %user, provider, "sign-in provider account linked");
                        Ok(back("/profile", "linked", Some(provider)))
                    }
                    Err(IdentityError::LinkedToAnother) => {
                        tracing::warn!(
                            user = %user,
                            provider,
                            "refused to link an account another user has linked"
                        );
                        Ok(back("/profile", "taken", Some(provider)))
                    }
                    Err(IdentityError::ProviderLinked) => {
                        Ok(back("/profile", "already", Some(provider)))
                    }
                    Err(e) => Err(e.into()),
                }
            }
            Purpose::Confirm { user, session } => {
                if !self.session_is(&session, user).await? {
                    return Ok(back("/login", "expired", None));
                }
                let owner = store
                    .identity_owner(&identity.issuer, &identity.subject)
                    .await
                    .map_err(internal)?;
                if owner != Some(user) {
                    tracing::warn!(
                        user = %user,
                        provider,
                        "a sign-in provider account not linked to this user was offered as a confirmation"
                    );
                    return Ok(back("/profile", "notyours", Some(provider)));
                }
                store.confirm_session(&session).await.map_err(internal)?;
                Ok(back("/profile", "confirmed", Some(provider)))
            }
        }
    }

    /// Whether the session with token hash `session` is live, and
    /// `user`'s: a link or a confirmation lands only in the session that
    /// asked for it.
    async fn session_is(
        &self,
        session: &str,
        user: crate::store::users::LocalUserId,
    ) -> RpcResult<bool> {
        Ok(self
            .auth
            .store()
            .api_session(session)
            .await
            .map_err(internal)?
            .is_some_and(|s| s.user == user))
    }

    /// The title of the series or movie `scope` names, if the catalog
    /// answers by `until`.
    async fn title_of(&self, scope: &str, until: tokio::time::Instant) -> Option<String> {
        use spoolframe_catalog::Catalog as _;
        let id: spoolframe_types::MediaId = scope.parse().ok()?;
        let found = tokio::time::timeout_at(until, self.plex.item(&id))
            .await
            .ok()?
            .ok()?;
        match found.item {
            spoolframe_types::Item::Movie(m) => Some(m.title),
            spoolframe_types::Item::Show(s) => Some(s.title),
            spoolframe_types::Item::Episode(e) => Some(e.title),
            spoolframe_types::Item::Season(s) => Some(s.title),
            _ => None,
        }
    }

    /// The account called `username`.
    async fn record(&self, username: &str) -> RpcResult<UserRecord> {
        let name = normalize_username(username)?;
        self.auth
            .store()
            .list_users()
            .await?
            .into_iter()
            .find(|u| u.username == name)
            .ok_or_else(|| UserError::NoSuchUser(name).into())
    }
}

/// How long titles are waited for in a list of track overrides.
const TITLE_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

fn info(u: UserRecord, limited: Option<std::time::Duration>) -> UserInfo {
    UserInfo {
        // Rounded up: a wait of 0.4 s is still a wait.
        rate_limited_secs: limited.map(|d| d.as_secs() + u64::from(d.subsec_nanos() > 0)),
        username: u.username,
        display_name: u.display_name,
        role: u.role,
        created_at: u.created_at,
        disabled: u.disabled_at.is_some(),
        has_password: u.has_password,
        password_changed_at: u.password_changed_at,
        last_seen_at: u.last_seen_at,
        has_passkey: u.has_passkey,
        has_sso: u.has_sso,
        has_pin: u.has_pin,
        device_count: u.device_count,
        session_count: u.session_count,
        portable: u.tie.map(|tie| spoolframe_admin_api::PortableTie {
            id: tie.portable.to_string(),
            fingerprint: tie.portable.fingerprint(),
            tied_at: tie.tied_at,
        }),
    }
}

/// Sever `user`'s tie (P7-04), `by` whom for the log: the identity it
/// was to, or `NO_SUCH_LINK` for none.
async fn untie(
    store: &crate::store::Store,
    user: crate::store::users::LocalUserId,
    username: &str,
    by: &str,
) -> RpcResult<()> {
    match store.untie(user).await.map_err(internal)? {
        Some(portable) => {
            tracing::info!(
                user = %username,
                portable = %portable.fingerprint(),
                "portable identity untied by {by}"
            );
            Ok(())
        }
        None => Err(error(
            error_code::NO_SUCH_LINK,
            "the account is tied to no portable identity",
        )),
    }
}

/// A session as the API shows it; `current` is the caller's own.
fn session_entry(s: SessionRecord, current: &str) -> SessionEntry {
    SessionEntry {
        platform: s.platform(),
        id: session_id(&s.token_hash),
        current: s.token_hash == current,
        device_name: s.device_name,
        created_at: s.created_at,
        last_used_at: s.last_used_at,
        endpoint_id: s.endpoint_id,
    }
}

/// A session's id for `me.revokeSession`: derived from its row's key, so it
/// is stable, and of no use without the session itself.
fn session_id(token_hash: &str) -> String {
    blake3::hash(format!("spoolframe session id {token_hash}").as_bytes()).to_hex()[..16].to_owned()
}

#[async_trait]
impl MeApiServer for AdminApi {
    async fn login(&self, ext: &Extensions, credentials: Credentials) -> RpcResult<LoggedIn> {
        Attempt::note(ext, &credentials.username);
        let source = source(ext)?;
        let login = self
            .auth
            .login_password(
                &credentials.username,
                &credentials.password,
                SessionBinding::Api,
                source.as_deref(),
                // A browser is no Iroh endpoint: the web's own sign-in
                // never makes trust (P4-01).
                false,
            )
            .await?;
        let username = normalize_username(&credentials.username)?;
        tracing::info!(
            username = %username,
            peer = ?ext.get::<Peer>().and_then(|p| p.0),
            "signed in with a password (API session)"
        );
        Ok(LoggedIn {
            token: login.token.0,
            username,
            role: login.role,
        })
    }

    async fn passkey_login_begin(&self, ext: &Extensions) -> RpcResult<PasskeyChallenge> {
        let source = source(ext)?;
        let (ceremony, options) = self.passkeys.begin_sign_in(source.as_deref())?;
        Ok(PasskeyChallenge {
            ceremony,
            options: serde_json::to_value(options).map_err(internal)?,
        })
    }

    async fn passkey_login_finish(
        &self,
        ext: &Extensions,
        ceremony: String,
        credential: serde_json::Value,
    ) -> RpcResult<LoggedIn> {
        let source = source(ext)?;
        let signed = serde_json::from_value(credential).map_err(|e| {
            error(
                error_code::INVALID_INPUT,
                format!("not what a browser's passkey signs: {e}"),
            )
        })?;
        let user = self
            .passkeys
            .finish_sign_in(&self.auth, &ceremony, &signed, source.as_deref())
            .await?;
        let token = self
            .auth
            .open_api_session(user.id)
            .await
            .map_err(internal)?;
        tracing::info!(
            username = %user.username,
            peer = ?ext.get::<Peer>().and_then(|p| p.0),
            "signed in with a passkey (API session)"
        );
        Ok(LoggedIn {
            token: token.0,
            username: user.username,
            role: user.role,
        })
    }

    async fn logout(&self, ext: &Extensions) -> RpcResult<()> {
        let (token, _) = self.caller(ext).await?;
        self.auth.logout(&token).await.map_err(internal)
    }

    async fn who_am_i(&self, ext: &Extensions) -> RpcResult<UserInfo> {
        let (_, user) = self.caller(ext).await?;
        let record = self.auth.store().user_by_id(user.user).await?;
        record
            .map(|u| self.info(u))
            .ok_or_else(|| error(error_code::UNAUTHENTICATED, "session is not valid"))
    }

    async fn set_password(&self, ext: &Extensions, change: PasswordChange) -> RpcResult<()> {
        let (token, user) = self.caller(ext).await?;
        let me = self.account(user).await?;
        if me.has_password {
            let source = source(ext)?;
            self.auth
                .check_password(&me.username, &change.current_password, source.as_deref())
                .await?;
        } else {
            // Set up with a passkey, with no password to prove it is them:
            // a recent sign-in does, as for adding a passkey.
            self.recently_signed_in(ext).await?;
        }
        let phc = hash_password(&change.new_password).await?;
        // Every other session of theirs ends, on the web and on devices.
        let keep = SessionToken(token).hash();
        let revoked = self
            .auth
            .store()
            .set_password(&me.username, &phc, Some(&keep))
            .await?;
        tracing::info!(username = %me.username, revoked, "password changed by its user");
        Ok(())
    }

    async fn sessions(&self, ext: &Extensions) -> RpcResult<Vec<SessionEntry>> {
        let (token, user) = self.caller(ext).await?;
        let current = crate::auth::SessionToken(token).hash();
        let sessions = self
            .auth
            .store()
            .list_sessions(user.user)
            .await
            .map_err(internal)?;
        Ok(sessions
            .into_iter()
            .map(|s| session_entry(s, &current))
            .collect())
    }

    async fn pin_set(&self, ext: &Extensions, pin: String) -> RpcResult<()> {
        let (_, user) = self.recently_signed_in(ext).await?;
        let me = self.account(user).await?;
        let Some(digits) = pin_digits(&pin) else {
            return Err(error(error_code::INVALID_INPUT, "a PIN is 4 to 8 digits"));
        };
        let phc = hash_pin(&pin).await?;
        self.auth
            .store()
            .set_pin(me.id, &phc, digits)
            .await
            .map_err(internal)?;
        tracing::info!(username = %me.username, "PIN set");
        Ok(())
    }

    async fn pin_clear(&self, ext: &Extensions) -> RpcResult<()> {
        let (_, user) = self.recently_signed_in(ext).await?;
        self.auth
            .store()
            .clear_pin(user.user)
            .await
            .map_err(internal)?;
        tracing::info!(user = %user.user, "PIN removed by its user");
        Ok(())
    }

    async fn devices(&self, ext: &Extensions) -> RpcResult<Vec<DeviceEntry>> {
        let me = self.me(ext).await?;
        let devices = self
            .auth
            .store()
            .user_devices(me.id)
            .await
            .map_err(internal)?;
        Ok(devices.into_iter().map(device).collect())
    }

    async fn set_pin_required(
        &self,
        ext: &Extensions,
        endpoint_id: String,
        required: bool,
    ) -> RpcResult<()> {
        // Asking for the PIN guards the device more; no longer asking is
        // a sensitive change, as removing the PIN is.
        let (_, user) = if required {
            self.caller(ext).await?
        } else {
            self.recently_signed_in(ext).await?
        };
        Ok(self
            .auth
            .store()
            .set_pin_required(user.user, &endpoint_id, required)
            .await?)
    }

    async fn set_device_trust(
        &self,
        ext: &Extensions,
        endpoint_id: String,
        trusted: bool,
        auto_login: bool,
    ) -> RpcResult<()> {
        // Either way (P4-01, P4-03): trust is a way in, and taking it away
        // ends the device's sessions.
        let (_, user) = self.recently_signed_in(ext).await?;
        let revoked = self
            .auth
            .store()
            .set_device_trust(user.user, &endpoint_id, trusted, auto_login)
            .await?;
        tracing::info!(
            user = %user.user,
            endpoint_id,
            trusted,
            auto_login,
            revoked,
            "device trust changed by its user"
        );
        Ok(())
    }

    async fn forget_device(&self, ext: &Extensions, endpoint_id: String) -> RpcResult<()> {
        // An API session is on no device, so any of the caller's may go;
        // it ends sessions there and takes away trust (P4-03).
        let (_, user) = self.recently_signed_in(ext).await?;
        let deleted = self
            .auth
            .store()
            .delete_device(user.user, &endpoint_id)
            .await?;
        tracing::info!(user = %user.user, endpoint_id, deleted, "device deleted by its user");
        Ok(())
    }

    async fn confirm_password(&self, ext: &Extensions, password: String) -> RpcResult<()> {
        let (token, user) = self.caller(ext).await?;
        let me = self.account(user).await?;
        let source = source(ext)?;
        self.auth
            .check_password(&me.username, &password, source.as_deref())
            .await?;
        self.auth
            .store()
            .confirm_session(&SessionToken(token).hash())
            .await
            .map_err(internal)
    }

    async fn device_sign_in_lookup(
        &self,
        ext: &Extensions,
        code: String,
    ) -> RpcResult<DeviceSignIn> {
        let (_, code, guess) = self.sign_in_code(ext, &code).await?;
        let Some(waiting) = self.auth.sign_ins().lookup(&code) else {
            return Err(wrong_sign_in_code());
        };
        self.auth.guessed_right(guess);
        // A code is pending only once a device reached the endpoint, so
        // the endpoint's key is known; never an empty fingerprint to match.
        let server_fingerprint = self.status.fingerprint().ok_or_else(|| {
            error(
                error_code::UNAVAILABLE,
                "the server's endpoint is not up yet",
            )
        })?;
        Ok(DeviceSignIn {
            device_name: waiting.device_name,
            client_name: waiting.client_name,
            platform: waiting.platform,
            requested_at: waiting.requested_at,
            remember: waiting.remember,
            server_fingerprint,
            address: waiting.address.map(|ip| ip.to_canonical().to_string()),
            from_internet: waiting
                .address
                .is_some_and(crate::device_sign_in::from_internet),
            public_ipv6: waiting
                .address
                .is_some_and(crate::device_sign_in::public_ipv6),
        })
    }

    async fn device_sign_in_approve(
        &self,
        ext: &Extensions,
        code: String,
        remember: bool,
    ) -> RpcResult<()> {
        self.recently_signed_in(ext).await?;
        let (user, code, guess) = self.sign_in_code(ext, &code).await?;
        // No endpoint approves here: a web session is never the device
        // that waits.
        let claim = match self.auth.sign_ins().claim(&code, None) {
            Ok(claim) => claim,
            Err(ClaimError::Unknown | ClaimError::SameDevice) => {
                return Err(wrong_sign_in_code());
            }
        };
        self.auth.guessed_right(guess);
        match self.auth.approve_device(user.user, &claim, remember).await {
            Ok(Approval::SignedIn) => {
                tracing::info!(
                    user = %user.user,
                    device = %claim.endpoint,
                    remembered = claim.remember && remember,
                    "device sign-in approved from the web"
                );
                Ok(())
            }
            // The right code, run out while it was approved: not a guess,
            // so not counted as one.
            Ok(Approval::Expired) => Err(error(
                error_code::NO_SUCH_SIGN_IN_CODE,
                "that sign-in code ran out while it was approved",
            )),
            Err(e) => Err(internal(e)),
        }
    }

    async fn device_sign_in_deny(&self, ext: &Extensions, code: String) -> RpcResult<()> {
        let (user, code, guess) = self.sign_in_code(ext, &code).await?;
        if self.auth.sign_ins().deny(&code) {
            self.auth.guessed_right(guess);
            tracing::info!(user = %user.user, "device sign-in denied from the web");
            Ok(())
        } else {
            Err(wrong_sign_in_code())
        }
    }

    async fn confirm_passkey(
        &self,
        ext: &Extensions,
        ceremony: String,
        credential: serde_json::Value,
    ) -> RpcResult<()> {
        let (token, user) = self.caller(ext).await?;
        let source = source(ext)?;
        let signed = serde_json::from_value(credential).map_err(|e| {
            error(
                error_code::INVALID_INPUT,
                format!("not what a browser's passkey signs: {e}"),
            )
        })?;
        let signer = self
            .passkeys
            .finish_sign_in(&self.auth, &ceremony, &signed, source.as_deref())
            .await?;
        if signer.id != user.user {
            tracing::warn!(
                user = %user.user,
                passkey_of = %signer.username,
                "a passkey of another user's was offered as a confirmation"
            );
            return Err(SignInError::Refused.into());
        }
        self.auth
            .store()
            .confirm_session(&SessionToken(token).hash())
            .await
            .map_err(internal)
    }

    async fn passkey_host(&self, ext: &Extensions) -> RpcResult<PasskeyHost> {
        self.caller(ext).await?;
        let (rp_id, origin) = self.passkeys.host()?;
        Ok(PasskeyHost {
            rp_id: rp_id.to_owned(),
            origin: origin.to_owned(),
        })
    }

    async fn passkeys(&self, ext: &Extensions) -> RpcResult<Vec<PasskeyEntry>> {
        let (_, user) = self.caller(ext).await?;
        self.passkey_entries(user.user).await
    }

    async fn passkey_add_begin(&self, ext: &Extensions) -> RpcResult<serde_json::Value> {
        let (token, user) = self.recently_signed_in(ext).await?;
        let me = self.account(user).await?;
        let options = self
            .passkeys
            .begin_registration(self.auth.store(), &SessionToken(token).hash(), &me)
            .await?;
        serde_json::to_value(options).map_err(internal)
    }

    async fn passkey_add_finish(
        &self,
        ext: &Extensions,
        name: String,
        credential: serde_json::Value,
    ) -> RpcResult<PasskeyEntry> {
        let (token, user) = self.caller(ext).await?;
        let me = self.account(user).await?;
        let made = serde_json::from_value(credential).map_err(|e| {
            error(
                error_code::INVALID_INPUT,
                format!("not what a browser makes: {e}"),
            )
        })?;
        let record = self
            .passkeys
            .finish_registration(
                self.auth.store(),
                &SessionToken(token).hash(),
                &me,
                &name,
                &made,
            )
            .await?;
        Ok(self.passkey_entry(record))
    }

    async fn passkey_rename(&self, ext: &Extensions, id: String, name: String) -> RpcResult<()> {
        let (_, user) = self.caller(ext).await?;
        Ok(self
            .auth
            .store()
            .rename_passkey(user.user, &id, &name)
            .await?)
    }

    async fn password_remove(&self, ext: &Extensions) -> RpcResult<()> {
        let (token, user) = self.recently_signed_in(ext).await?;
        let me = self.account(user).await?;
        // Every other session of theirs ends, as with a new password.
        let keep = SessionToken(token).hash();
        let revoked = self
            .auth
            .store()
            .remove_password(&me.username, Some(&keep))
            .await?;
        tracing::info!(username = %me.username, revoked, "password removed by its user");
        Ok(())
    }

    async fn passkey_remove(&self, ext: &Extensions, id: String) -> RpcResult<()> {
        let (_, user) = self.recently_signed_in(ext).await?;
        self.auth.store().remove_passkey(user.user, &id).await?;
        tracing::info!(user = %user.user, "passkey removed by its user");
        Ok(())
    }

    async fn sso_links(&self, ext: &Extensions) -> RpcResult<Vec<SsoLink>> {
        let (_, user) = self.caller(ext).await?;
        self.sso_entries(user.user).await
    }

    async fn sso_unlink(&self, ext: &Extensions, issuer: String) -> RpcResult<()> {
        let (_, user) = self.recently_signed_in(ext).await?;
        self.auth
            .store()
            .unlink_identity(user.user, &issuer)
            .await?;
        tracing::info!(user = %user.user, provider = ?self.oidc.provider_of(&issuer), "sign-in provider account unlinked by its user");
        Ok(())
    }

    async fn portable_untie(&self, ext: &Extensions) -> RpcResult<()> {
        let (_, user) = self.recently_signed_in(ext).await?;
        let me = self.account(user).await?;
        untie(self.auth.store(), me.id, &me.username, "its user").await
    }

    async fn plex_link(&self, ext: &Extensions) -> RpcResult<PlexUser> {
        let me = self.me(ext).await?;
        let link = self.accounts()?.link(me.id).map_err(internal)?;
        Ok(self.plex_user(me, link))
    }

    async fn plex_link_begin(&self, ext: &Extensions) -> RpcResult<PlexPin> {
        let (_, user) = self.caller(ext).await?;
        let pin = self
            .accounts()?
            .begin(user.user, plex_forward(ext).as_deref())
            .await?;
        Ok(PlexPin {
            id: pin.id,
            auth_url: pin.auth_url,
            expires_in: pin.expires_in,
        })
    }

    async fn plex_link_check(&self, ext: &Extensions, id: u64) -> RpcResult<PlexLinkState> {
        let (_, user) = self.caller(ext).await?;
        Ok(match self.accounts()?.check(user.user, id).await? {
            Linking::Waiting => PlexLinkState::Waiting,
            Linking::Expired => PlexLinkState::Expired,
            Linking::Linked(link) => PlexLinkState::Linked { link: link.shown() },
        })
    }

    async fn plex_unlink(&self, ext: &Extensions) -> RpcResult<()> {
        let me = self.me(ext).await?;
        self.accounts()?.unlink(me.id, &me.username)?;
        Ok(())
    }

    async fn revoke_other_sessions(&self, ext: &Extensions) -> RpcResult<u64> {
        let (token, user) = self.caller(ext).await?;
        let revoked = self
            .auth
            .store()
            .revoke_other_sessions(user.user, &SessionToken(token).hash())
            .await
            .map_err(internal)?;
        tracing::info!(
            user = %user.user,
            revoked,
            "signed out everywhere else by its user"
        );
        Ok(revoked)
    }

    async fn set_display_name(
        &self,
        ext: &Extensions,
        display_name: Option<String>,
    ) -> RpcResult<UserInfo> {
        let (_, user) = self.caller(ext).await?;
        self.auth
            .store()
            .set_display_name(user.user, display_name.as_deref())
            .await?;
        Ok(self.info(self.account(user).await?))
    }

    async fn track_preferences(&self, ext: &Extensions) -> RpcResult<TrackPreferencesInfo> {
        let (_, user) = self.caller(ext).await?;
        let preferences = self
            .auth
            .store()
            .track_preferences(Principal::LocalUser(user.user))
            .await
            .map_err(internal)?;
        Ok(TrackPreferencesInfo {
            preferences,
            ocr_offered: !self.status.ocr_languages.is_empty(),
        })
    }

    async fn set_track_preferences(
        &self,
        ext: &Extensions,
        preferences: TrackPreferences,
    ) -> RpcResult<()> {
        let (_, user) = self.caller(ext).await?;
        self.auth
            .store()
            .set_track_preferences(Principal::LocalUser(user.user), &preferences)
            .await
            .map_err(internal)
    }

    async fn track_overrides(&self, ext: &Extensions) -> RpcResult<Vec<TrackOverrideEntry>> {
        let (_, user) = self.caller(ext).await?;
        let overrides = self
            .auth
            .store()
            .track_overrides(Principal::LocalUser(user.user))
            .await
            .map_err(internal)?;
        // One wait for all the titles: a list of overrides is not held up
        // by Plex being down.
        let until = tokio::time::Instant::now() + TITLE_WAIT;
        let mut entries = Vec::with_capacity(overrides.len());
        for (scope, picked) in overrides {
            entries.push(TrackOverrideEntry {
                title: self.title_of(&scope, until).await,
                id: scope,
                audio: picked.audio.map(|a| AudioPick {
                    language: a.language,
                    role: a.role,
                }),
                subtitle: picked.subtitle.map(|s| match s {
                    SubtitleChoice::Track { language, role } => {
                        SubtitlePick::Track { language, role }
                    }
                    SubtitleChoice::Off => SubtitlePick::Off,
                }),
            });
        }
        Ok(entries)
    }

    async fn clear_track_override(&self, ext: &Extensions, id: String) -> RpcResult<()> {
        let (_, user) = self.caller(ext).await?;
        self.auth
            .store()
            .clear_track_override(Principal::LocalUser(user.user), &id)
            .await
            .map_err(internal)
    }

    async fn revoke_session(&self, ext: &Extensions, id: String) -> RpcResult<()> {
        let (_, user) = self.caller(ext).await?;
        // Only among the caller's own: anyone else's is not found.
        let sessions = self
            .auth
            .store()
            .list_sessions(user.user)
            .await
            .map_err(internal)?;
        let Some(session) = sessions
            .into_iter()
            .find(|s| session_id(&s.token_hash) == id)
        else {
            return Err(error(error_code::NO_SUCH_SESSION, "no such session"));
        };
        self.auth
            .store()
            .revoke_session(&session.token_hash)
            .await
            .map_err(internal)
    }
}

#[async_trait]
impl AdminApiServer for AdminApi {
    async fn setup(&self, ext: &Extensions, request: SetupRequest) -> RpcResult<LoggedIn> {
        self.setup_code(ext, &request.code)?;
        let phc = hash_password(&request.password).await?;
        let user = self
            .auth
            .store()
            .add_first_admin(&request.username, &phc)
            .await?;
        let token = self.auth.open_api_session(user).await.map_err(internal)?;
        tracing::info!(username = %request.username, "first admin created");
        Ok(LoggedIn {
            token: token.0,
            username: normalize_username(&request.username)?,
            role: Role::Admin,
        })
    }

    async fn setup_passkey_begin(
        &self,
        ext: &Extensions,
        request: PasskeySetupBegin,
    ) -> RpcResult<PasskeyChallenge> {
        self.setup_code(ext, &request.code)?;
        // Refused now rather than after the browser's prompt: a passkey
        // made for nothing stays on the device.
        if !self.setup_open().await? {
            return Err(UserError::AlreadySetUp.into());
        }
        let username = normalize_username(&request.username)?;
        let (ceremony, options) = self.passkeys.begin_setup(&username)?;
        Ok(PasskeyChallenge {
            ceremony,
            options: serde_json::to_value(options).map_err(internal)?,
        })
    }

    async fn setup_passkey_finish(
        &self,
        ext: &Extensions,
        request: PasskeySetupFinish,
    ) -> RpcResult<LoggedIn> {
        // Again here: a request begun with the code does not outlive it.
        self.setup_code(ext, &request.code)?;
        let made = serde_json::from_value(request.credential).map_err(|e| {
            error(
                error_code::INVALID_INPUT,
                format!("not what a browser makes: {e}"),
            )
        })?;
        // Before the request is used up: a name can be typed again.
        let name = passkey_name(&request.name)?;
        let passkey = self.passkeys.finish_setup(&request.ceremony, &made)?;
        let user = self
            .auth
            .store()
            .add_first_admin_with_passkey(
                &passkey.username,
                FirstPasskey {
                    handle: &passkey.handle,
                    credential_id: &passkey.credential_id,
                    name,
                    credential: &passkey.credential,
                    rp_id: &passkey.rp_id,
                },
            )
            .await?;
        let token = self.auth.open_api_session(user).await.map_err(internal)?;
        tracing::info!(
            username = %passkey.username,
            passkey = name,
            rp_id = %passkey.rp_id,
            "first admin created, with a passkey and no password"
        );
        Ok(LoggedIn {
            token: token.0,
            username: passkey.username,
            role: Role::Admin,
        })
    }

    async fn user_list(&self, ext: &Extensions) -> RpcResult<Vec<UserInfo>> {
        self.admin(ext).await?;
        Ok(self
            .auth
            .store()
            .list_users()
            .await?
            .into_iter()
            .map(|u| self.info(u))
            .collect())
    }

    async fn user_add(&self, ext: &Extensions, user: NewUser) -> RpcResult<UserInfo> {
        match user.role {
            Role::Admin => self.recent_admin(ext).await?,
            _ => self.admin(ext).await?,
        };
        if user.role == Role::Unknown {
            return Err(error(error_code::INVALID_INPUT, "use admin or consumer"));
        }
        let phc = hash_password(&user.password).await?;
        self.auth
            .store()
            .add_user(
                &user.username,
                user.display_name.as_deref(),
                &phc,
                user.role,
            )
            .await?;
        tracing::info!(username = %user.username, role = %user.role, "user added");
        self.user_info(&user.username).await
    }

    async fn user_set_password(&self, ext: &Extensions, credentials: Credentials) -> RpcResult<()> {
        self.recent_admin(ext).await?;
        let (token, _) = self.caller(ext).await?;
        let phc = hash_password(&credentials.password).await?;
        // Every session of theirs ends; the admin's own is kept, which is
        // one of theirs only when they reset their own password.
        let keep = SessionToken(token).hash();
        let revoked = self
            .auth
            .store()
            .set_password(&credentials.username, &phc, Some(&keep))
            .await?;
        tracing::info!(username = %credentials.username, revoked, "password set by an admin");
        Ok(())
    }

    async fn user_set_role(
        &self,
        ext: &Extensions,
        username: String,
        role: Role,
    ) -> RpcResult<UserInfo> {
        match role {
            Role::Admin => self.recent_admin(ext).await?,
            _ => self.admin(ext).await?,
        };
        if role == Role::Unknown {
            return Err(error(error_code::INVALID_INPUT, "use admin or consumer"));
        }
        self.auth.store().set_role(&username, role).await?;
        tracing::info!(%username, %role, "role changed");
        self.user_info(&username).await
    }

    async fn user_disable(&self, ext: &Extensions, username: String) -> RpcResult<u64> {
        self.admin(ext).await?;
        let revoked = self.auth.store().disable_user(&username).await?;
        tracing::info!(%username, revoked, "user disabled");
        Ok(revoked)
    }

    async fn user_enable(&self, ext: &Extensions, username: String) -> RpcResult<()> {
        self.admin(ext).await?;
        Ok(self.auth.store().enable_user(&username).await?)
    }

    async fn user_clear_pin(&self, ext: &Extensions, username: String) -> RpcResult<()> {
        self.recent_admin(ext).await?;
        let user = self.record(&username).await?;
        self.auth
            .store()
            .clear_pin(user.id)
            .await
            .map_err(internal)?;
        tracing::info!(%username, "PIN cleared by an admin");
        Ok(())
    }

    async fn user_devices(
        &self,
        ext: &Extensions,
        username: String,
    ) -> RpcResult<Vec<DeviceEntry>> {
        self.admin(ext).await?;
        let user = self.record(&username).await?;
        let devices = self
            .auth
            .store()
            .user_devices(user.id)
            .await
            .map_err(internal)?;
        Ok(devices.into_iter().map(device).collect())
    }

    async fn user_set_pin_required(
        &self,
        ext: &Extensions,
        username: String,
        endpoint_id: String,
        required: bool,
    ) -> RpcResult<()> {
        // Asking for the PIN guards the device more; no longer asking
        // does not, as for `me.setPinRequired`.
        if required {
            self.admin(ext).await?;
        } else {
            self.recent_admin(ext).await?;
        }
        let user = self.record(&username).await?;
        self.auth
            .store()
            .set_pin_required(user.id, &endpoint_id, required)
            .await?;
        tracing::info!(%username, endpoint_id, required, "PIN requirement changed by an admin");
        Ok(())
    }

    async fn user_set_device_trust(
        &self,
        ext: &Extensions,
        username: String,
        endpoint_id: String,
        trusted: bool,
        auto_login: bool,
    ) -> RpcResult<()> {
        self.recent_admin(ext).await?;
        let user = self.record(&username).await?;
        let revoked = self
            .auth
            .store()
            .set_device_trust(user.id, &endpoint_id, trusted, auto_login)
            .await?;
        tracing::info!(
            %username,
            endpoint_id,
            trusted,
            auto_login,
            revoked,
            "device trust changed by an admin"
        );
        Ok(())
    }

    async fn user_forget_device(
        &self,
        ext: &Extensions,
        username: String,
        endpoint_id: String,
    ) -> RpcResult<()> {
        self.recent_admin(ext).await?;
        let user = self.record(&username).await?;
        let deleted = self
            .auth
            .store()
            .delete_device(user.id, &endpoint_id)
            .await?;
        tracing::info!(%username, endpoint_id, deleted, "device deleted by an admin");
        Ok(())
    }

    async fn user_sessions(
        &self,
        ext: &Extensions,
        username: String,
    ) -> RpcResult<Vec<SessionEntry>> {
        let (token, _) = self.caller(ext).await?;
        self.admin(ext).await?;
        let user = self.record(&username).await?;
        let current = crate::auth::SessionToken(token).hash();
        let sessions = self
            .auth
            .store()
            .list_sessions(user.id)
            .await
            .map_err(internal)?;
        Ok(sessions
            .into_iter()
            .map(|s| session_entry(s, &current))
            .collect())
    }

    async fn user_revoke_session(
        &self,
        ext: &Extensions,
        username: String,
        id: String,
    ) -> RpcResult<()> {
        self.admin(ext).await?;
        let user = self.record(&username).await?;
        let sessions = self
            .auth
            .store()
            .list_sessions(user.id)
            .await
            .map_err(internal)?;
        let Some(session) = sessions
            .into_iter()
            .find(|s| session_id(&s.token_hash) == id)
        else {
            return Err(error(error_code::NO_SUCH_SESSION, "no such session"));
        };
        self.auth
            .store()
            .revoke_session(&session.token_hash)
            .await
            .map_err(internal)?;
        tracing::info!(%username, "session revoked by an admin");
        Ok(())
    }

    async fn user_passkeys(
        &self,
        ext: &Extensions,
        username: String,
    ) -> RpcResult<Vec<PasskeyEntry>> {
        self.admin(ext).await?;
        let user = self.record(&username).await?;
        self.passkey_entries(user.id).await
    }

    async fn user_remove_password(&self, ext: &Extensions, username: String) -> RpcResult<()> {
        self.recent_admin(ext).await?;
        let (token, _) = self.caller(ext).await?;
        // Their own sessions but this one, when an admin takes their own.
        let keep = SessionToken(token).hash();
        let revoked = self
            .auth
            .store()
            .remove_password(&username, Some(&keep))
            .await?;
        tracing::info!(%username, revoked, "password removed by an admin");
        Ok(())
    }

    async fn user_remove_passkey(
        &self,
        ext: &Extensions,
        username: String,
        id: String,
    ) -> RpcResult<()> {
        self.recent_admin(ext).await?;
        let user = self.record(&username).await?;
        self.auth.store().remove_passkey(user.id, &id).await?;
        tracing::info!(%username, "passkey removed by an admin");
        Ok(())
    }

    async fn user_sso_links(&self, ext: &Extensions, username: String) -> RpcResult<Vec<SsoLink>> {
        self.admin(ext).await?;
        let user = self.record(&username).await?;
        self.sso_entries(user.id).await
    }

    async fn user_sso_unlink(
        &self,
        ext: &Extensions,
        username: String,
        issuer: String,
    ) -> RpcResult<()> {
        self.recent_admin(ext).await?;
        let user = self.record(&username).await?;
        self.auth.store().unlink_identity(user.id, &issuer).await?;
        tracing::info!(%username, provider = ?self.oidc.provider_of(&issuer), "sign-in provider account unlinked by an admin");
        Ok(())
    }

    async fn user_untie(&self, ext: &Extensions, username: String) -> RpcResult<()> {
        self.recent_admin(ext).await?;
        let user = self.record(&username).await?;
        untie(self.auth.store(), user.id, &user.username, "an admin").await
    }

    async fn sso_set_client_secret(
        &self,
        ext: &Extensions,
        provider: String,
        secret: Option<String>,
    ) -> RpcResult<()> {
        let (settings, by) = self.settings_as_recent_admin(ext, true).await?;
        let config = settings.layers().config().map_err(internal)?;
        let Some(p) = config.auth.oidc.iter().find(|p| p.name.trim() == provider) else {
            return Err(OidcError::NoSuchProvider(provider).into());
        };
        if let Some(file) = &p.client_secret_file {
            return Err(error(
                error_code::SETTING_LOCKED,
                format!(
                    "{provider}'s client secret is read from {}, as the config file says",
                    file.display()
                ),
            ));
        }
        if p.client_secret.is_some() {
            return Err(error(
                error_code::SETTING_LOCKED,
                format!("{provider}'s client secret is its client_secret setting"),
            ));
        }
        let name = p.secret_name();
        match secret.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(value) => settings.secrets().set(&name, value).map_err(internal)?,
            None => {
                settings.secrets().remove(&name).map_err(internal)?;
            }
        }
        // Never the secret: only that it changed, and who changed it.
        tracing::info!(
            by,
            provider,
            set = secret.is_some(),
            "sign-in provider client secret changed"
        );
        Ok(())
    }

    async fn plex_status(&self, ext: &Extensions) -> RpcResult<PlexStatus> {
        self.admin(ext).await?;
        let mut status = libraries::plex_status(&*self.plex()?).await;
        if let Some(settings) = &self.settings {
            settings.mark_served(FIRST_PLEX_SERVER, &mut status);
        }
        Ok(status)
    }

    async fn plex_check_paths(
        &self,
        ext: &Extensions,
        library: Option<String>,
    ) -> RpcResult<PathCheckRun> {
        self.admin(ext).await?;
        let composite = self.plex.composite();
        let plex = self.plex()?;
        let reports = plex
            .check_paths(library.as_deref(), Scope::All)
            .await
            .map_err(|e| match e {
                spoolframe_catalog::CatalogError::NotFound(what) => {
                    error(error_code::INVALID_INPUT, format!("no {what}"))
                }
                other => error(error_code::UNAVAILABLE, other.to_string()),
            })?;
        let failures = |f: spoolframe_plex::check::Failures| PathFailures {
            count: f.count,
            examples: f.examples,
        };
        // Each library's catalog pages list this check (P3-16).
        if let Some(composite) = &composite {
            for r in &reports {
                let Ok(local) =
                    spoolframe_types::LibraryId::new(spoolframe_types::Namespace::Plex, &r.section)
                else {
                    continue;
                };
                let library = crate::catalog::ids::library(FIRST_PLEX_SERVER, local);
                let check = spoolframe_catalog::LibraryCheck {
                    ok: r.ok,
                    missing: r.missing.clone(),
                };
                self.checks.record(composite, &library.to_string(), check);
            }
        }
        let run = PathCheckRun {
            at: crate::store::now(),
            reports: reports
                .into_iter()
                .map(|r| PathReport {
                    library: r.library,
                    ok: r.ok,
                    no_mapping: failures(r.no_mapping),
                    unreadable: failures(r.unreadable),
                })
                .collect(),
        };
        // Only a check of every library stands for the page's.
        if library.is_none() {
            match plex.paths_basis().await {
                Ok(basis) => self.checks.keep_run(&basis, run.clone()),
                Err(e) => tracing::warn!(error = %e, "cannot keep the path check"),
            }
        }
        Ok(run)
    }

    /// Server › Media paths' last check, while the mappings and the
    /// served libraries' folders are what it read. Plex is asked for the
    /// folders; without an answer, nothing says the check still stands.
    async fn plex_last_path_check(&self, ext: &Extensions) -> RpcResult<Option<PathCheckRun>> {
        self.admin(ext).await?;
        let Some(plex) = self.plex.plex() else {
            return Ok(None);
        };
        Ok(match plex.paths_basis().await {
            Ok(basis) => self.checks.last_run(&basis),
            Err(e) => {
                tracing::debug!(error = %e, "cannot tell whether the last path check stands");
                None
            }
        })
    }

    async fn server_status(&self, ext: &Extensions) -> RpcResult<ServerStatus> {
        self.admin(ext).await?;
        Ok(self.status.status())
    }

    async fn plex_watch_sync(
        &self,
        ext: &Extensions,
        username: Option<String>,
        dry_run: bool,
    ) -> RpcResult<Vec<PlexSyncReport>> {
        self.admin(ext).await?;
        let Some(sync) = &self.plex_sync else {
            return Err(error(
                error_code::NOT_CONFIGURED,
                "this server does not sync watch state with Plex",
            ));
        };
        let report = |user: UserRecord, r: crate::plex_sync::Report| PlexSyncReport {
            username: user.username,
            pulled: r.pulled,
            pushed: r.pushed,
            failed: r.failed,
            skipped: r.skipped,
            notes: r.notes,
        };
        if let Some(username) = username {
            let user = self.record(&username).await?;
            let done = sync.run(user.id, dry_run).await.map_err(|e| match e {
                SyncError::NotLinked => error(
                    error_code::NOT_CONFIGURED,
                    format!("{} has not linked a Plex account", user.username),
                ),
                SyncError::Plex(e) => error(error_code::UNAVAILABLE, e.to_string()),
                SyncError::Store(_) | SyncError::Link(_) => internal(e),
            })?;
            return Ok(vec![report(user, done)]);
        }
        // Everyone linked, in turn; one who fails does not stop the rest.
        let mut reports = Vec::new();
        for (user, link) in self.accounts()?.users().await? {
            if link.is_none() {
                continue;
            }
            match sync.run(user.id, dry_run).await {
                Ok(done) => reports.push(report(user, done)),
                Err(e) => reports.push(PlexSyncReport {
                    username: user.username,
                    notes: vec![format!("Not synced: {e}")],
                    ..PlexSyncReport::default()
                }),
            }
        }
        Ok(reports)
    }

    async fn plex_sync_overview(&self, ext: &Extensions) -> RpcResult<PlexSyncOverview> {
        self.admin(ext).await?;
        let users = self.accounts()?.users().await?;
        let interval = self
            .plex_sync
            .as_ref()
            .map_or(crate::config::WatchSyncConfig::DEFAULT_INTERVAL, |s| {
                s.interval()
            });
        Ok(PlexSyncOverview {
            interval_secs: interval.as_secs(),
            users: users
                .into_iter()
                .map(|(user, link)| self.plex_user(user, link))
                .collect(),
        })
    }

    async fn user_plex_unlink(&self, ext: &Extensions, username: String) -> RpcResult<()> {
        let by = self.admin(ext).await?;
        let by = self.account(by).await?.username;
        let user = self.record(&username).await?;
        self.accounts()?.unlink(user.id, &by)?;
        Ok(())
    }

    async fn plex_account_servers(&self, ext: &Extensions) -> RpcResult<Vec<PlexAccountServer>> {
        let user = self.admin(ext).await?;
        let Some(resources) = self.offered_servers(user.user).await? else {
            return Err(error(
                error_code::NOT_CONFIGURED,
                "link your Plex account in My profile, or sign in with Plex, to list its servers",
            ));
        };
        let configured: Vec<(String, String)> = self
            .plex
            .composite()
            .map(|c| {
                c.servers()
                    .iter()
                    .map(|s| (s.id.clone(), s.catalog.base_url().to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        Ok(crate::plex_accounts::account_servers(
            &resources,
            &configured,
        ))
    }

    async fn plex_account_token_use(&self, ext: &Extensions, url: String) -> RpcResult<()> {
        // It replaces `plex.token` (finding 44).
        let (settings, by) = self.settings_as_recent_admin(ext, true).await?;
        let user = self.admin(ext).await?;
        let Some(resources) = self.offered_servers(user.user).await? else {
            return Err(error(
                error_code::NOT_CONFIGURED,
                "sign in with Plex first: no Plex account lists this server",
            ));
        };
        let Some(token) = crate::plex_accounts::token_for_url(&resources, url.trim()) else {
            return Err(error(
                error_code::INVALID_INPUT,
                format!("the Plex account does not reach {}", url.trim()),
            ));
        };
        settings.set(PLEX_TOKEN, &serde_json::Value::String(token), &by)?;
        tracing::info!(
            by,
            url = url.trim(),
            "the Plex account's token for the server chosen is kept as plex.token"
        );
        Ok(())
    }

    async fn settings(&self, ext: &Extensions) -> RpcResult<Settings> {
        let (settings, _) = self.settings_as_admin(ext).await?;
        Ok(settings.settings())
    }

    async fn setting_set(
        &self,
        ext: &Extensions,
        key: String,
        value: serde_json::Value,
    ) -> RpcResult<SettingEntry> {
        let (settings, by) = self
            .settings_as_recent_admin(ext, sensitive_setting(&key))
            .await?;
        Ok(settings.set(&key, &value, &by)?)
    }

    async fn setting_unset(&self, ext: &Extensions, key: String) -> RpcResult<SettingEntry> {
        let (settings, by) = self
            .settings_as_recent_admin(ext, sensitive_setting(&key))
            .await?;
        Ok(settings.unset(&key, &by)?)
    }

    async fn restart(&self, ext: &Extensions) -> RpcResult<()> {
        // Every device connected drops, and a server started by hand runs
        // whatever binary is at its path now.
        let (settings, by) = self.settings_as_recent_admin(ext, true).await?;
        settings.restart(&by);
        Ok(())
    }

    async fn config_reload(&self, ext: &Extensions) -> RpcResult<ConfigReload> {
        let (settings, by) = self.settings_as_recent_admin(ext, true).await?;
        let reloaded = settings
            .reload(&format!("configReload by {by}"))
            .await
            .map_err(|e| error(error_code::INVALID_INPUT, e.to_string()))?;
        Ok(ConfigReload {
            applied: reloaded.applied,
            after_restart: reloaded.after_restart,
        })
    }

    async fn setup_state(&self, ext: &Extensions) -> RpcResult<SetupState> {
        let (settings, _) = self.settings_as_admin(ext).await?;
        Ok(setup_state(settings.setup()))
    }

    async fn setup_step_done(&self, ext: &Extensions, step: String) -> RpcResult<SetupState> {
        let (settings, by) = self.settings_as_admin(ext).await?;
        Ok(setup_state(settings.setup_progress(
            Some(&step),
            false,
            &by,
        )?))
    }

    async fn setup_finish(&self, ext: &Extensions) -> RpcResult<SetupState> {
        let (settings, by) = self.settings_as_admin(ext).await?;
        // A first admin made with a passkey adds a password before setup
        // ends: passkeys can all be lost, and then only a shell gets in.
        if !self.auth.store().an_admin_has_a_password().await? {
            return Err(error(
                error_code::INVALID_INPUT,
                "give the admin account a password before finishing: at least one admin needs one",
            ));
        }
        Ok(setup_state(settings.setup_progress(None, true, &by)?))
    }

    async fn plex_discover(&self, ext: &Extensions) -> RpcResult<Vec<DiscoveredPlex>> {
        self.admin(ext).await?;
        // The setting now (it is live), else the default.
        let port = self
            .settings
            .as_ref()
            .map_or(spoolframe_plex::gdm::PORT, |s| s.plex_discovery_port());
        let found =
            spoolframe_plex::gdm::discover(&self.wizard.gdm, port, spoolframe_plex::gdm::WAIT)
                .await
                .map_err(|e| {
                    error(
                        error_code::UNAVAILABLE,
                        format!("cannot search the LAN: {e}"),
                    )
                })?;
        Ok(found
            .into_iter()
            .map(|f| DiscoveredPlex {
                name: f.name,
                url: f.url,
                version: f.version,
                machine_id: f.machine_id,
            })
            .collect())
    }

    async fn wan_try_mapping(&self, ext: &Extensions) -> RpcResult<MappingStatus> {
        self.admin(ext).await?;
        let port = self
            .status
            .told
            .get()
            .and_then(crate::announce::Told::own_port)
            .and_then(std::num::NonZeroU16::new)
            .ok_or_else(|| error(error_code::NOT_CONFIGURED, "the endpoint is starting"))?;
        let mapping = crate::wan::try_mapping(&*self.wizard.wan.mapper, port).await;
        Ok(mapping_status(Some(&(port.get(), mapping))))
    }

    async fn wan_lookup(&self, ext: &Extensions) -> RpcResult<String> {
        self.admin(ext).await?;
        self.wizard
            .wan
            .lookup
            .once(crate::wan::LookupVia::Wtfismyip)
            .await
            .map(|ip| ip.to_string())
            .map_err(|e| error(error_code::UNAVAILABLE, e))
    }

    async fn invite_create(&self, ext: &Extensions, invite: NewInvite) -> RpcResult<InviteCreated> {
        use spoolframe_admin_api::invite::{
            DEFAULT_EXPIRES_SECS, DEFAULT_USES, MADE_BY_CLI, MAX_EXPIRES_SECS, MAX_NOTE_CHARS,
            MAX_USES, MIN_EXPIRES_SECS,
        };
        let caller = self.recent_admin(ext).await?;
        let kind = store_invite_kind(invite.kind);
        if kind != crate::store::invites::InviteKind::CreateUser {
            return Err(InviteError::NotYet(kind).into());
        }
        let uses = invite.uses.unwrap_or(DEFAULT_USES);
        if !(1..=MAX_USES).contains(&uses) {
            return Err(error(
                error_code::INVALID_INPUT,
                format!("an invite makes 1 to {MAX_USES} accounts"),
            ));
        }
        let expires = invite.expires.unwrap_or(DEFAULT_EXPIRES_SECS);
        if !(MIN_EXPIRES_SECS..=MAX_EXPIRES_SECS).contains(&expires) {
            return Err(error(
                error_code::INVALID_INPUT,
                "an invite lasts an hour to 30 days",
            ));
        }
        let role = invite.role.unwrap_or(Role::Consumer);
        if role == Role::Unknown {
            return Err(error(error_code::INVALID_INPUT, "use admin or consumer"));
        }
        let note = invite
            .note
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty());
        if note
            .is_some_and(|n| n.chars().count() > MAX_NOTE_CHARS || n.chars().any(char::is_control))
        {
            return Err(error(
                error_code::INVALID_INPUT,
                format!("a note is at most {MAX_NOTE_CHARS} characters, on one line"),
            ));
        }
        let Some(mut link) = self.status.invite().await else {
            return Err(error(
                error_code::NOT_CONFIGURED,
                "the server's endpoint is starting; try again in a moment",
            ));
        };
        // The browser's pages send their origin; the CLI does not.
        let created_by = if ext.get::<PageOrigin>().is_some() {
            self.account(caller).await?.username
        } else {
            MADE_BY_CLI.to_owned()
        };
        let code = crate::invites::InviteCode::generate();
        let hash = code.hash();
        let expires_at =
            self.auth.store().clock().unix_secs() + i64::try_from(expires).unwrap_or(i64::MAX);
        let record = self
            .auth
            .store()
            .create_invite(crate::store::invites::NewInvite {
                kind,
                capability_hash: &hash,
                role,
                note,
                uses,
                expires_at,
                created_by: &created_by,
            })
            .await?;
        tracing::info!(
            id = %record.id,
            %role,
            uses,
            by = %created_by,
            "invite made"
        );
        link.code = Some(code.as_str().to_owned());
        link.expires_at = u64::try_from(expires_at).ok();
        link.role = Some(role);
        Ok(InviteCreated {
            invite: invite_entry(record),
            code: code.shown(),
            link: link.link(),
        })
    }

    async fn invite_list(&self, ext: &Extensions) -> RpcResult<Vec<InviteEntry>> {
        self.admin(ext).await?;
        let invites = self.auth.store().live_invites().await.map_err(internal)?;
        Ok(invites.into_iter().map(invite_entry).collect())
    }

    async fn invite_revoke(&self, ext: &Extensions, id: String) -> RpcResult<()> {
        self.recent_admin(ext).await?;
        self.auth.store().revoke_invite(&id).await?;
        tracing::info!(%id, "invite revoked");
        Ok(())
    }

    async fn invite_link(&self, ext: &Extensions) -> RpcResult<String> {
        self.admin(ext).await?;
        match self.status.invite().await {
            Some(link) => Ok(link.link()),
            None => Err(error(
                error_code::NOT_CONFIGURED,
                "the server's endpoint is starting; try again in a moment",
            )),
        }
    }

    async fn plex_probe(
        &self,
        ext: &Extensions,
        url: String,
        token: Option<String>,
    ) -> RpcResult<PlexStatus> {
        let (settings, _) = self.settings_as_admin(ext).await?;
        let plex = PlexConfig {
            name: None,
            servers: Vec::new(),
            url,
            token_file: None,
            path_mapping: Vec::new(),
            watch_sync: None,
        };
        let url = plex
            .url()
            .map_err(|e| error(error_code::INVALID_INPUT, e.to_string()))?;
        // As `plexServerAdd` will add it: a pasted token, else the one the
        // caller's linked Plex account has for it (P3-12); a server already
        // added, with its own. Never another server's (PX-09 B-2): the URL
        // may be anyone's that answered discovery.
        let token = match token.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()) {
            Some(token) => Some(token),
            None => match self.linked_token_for(ext, url.as_str()).await? {
                Some(token) => Some(token),
                None => settings.server_token_at(url.as_str()),
            },
        };
        let catalog = PlexCatalog::new(url, token).map_err(internal)?;
        let mut status = libraries::plex_status(&catalog).await;
        // A server already added: its sections say which are libraries.
        if let Some(source) = self.plex.composite().and_then(|c| {
            c.servers()
                .iter()
                .find(|s| s.catalog.base_url() == catalog.base_url())
                .cloned()
        }) {
            settings.mark_served(&source.id, &mut status);
        }
        Ok(status)
    }

    async fn plex_pin_begin(&self, ext: &Extensions) -> RpcResult<PlexPin> {
        let (settings, _) = self.settings_as_admin(ext).await?;
        let pin = self
            .plex_tv(settings)?
            .begin(plex_forward(ext).as_deref())
            .await
            .map_err(|e| error(error_code::UNAVAILABLE, e.to_string()))?;
        self.pins.lock().insert(pin.id);
        Ok(PlexPin {
            id: pin.id,
            auth_url: pin.auth_url,
            expires_in: pin.expires_in,
        })
    }

    async fn plex_pin_check(
        &self,
        ext: &Extensions,
        id: u64,
        keep_token: bool,
    ) -> RpcResult<PlexPinState> {
        // Keeping the token replaces `plex.token` (finding 44); checked
        // before the PIN is, so an approved one waits for "Confirm it's
        // you" and is not used up.
        let (settings, by) = self.settings_as_recent_admin(ext, keep_token).await?;
        if !self.pins.lock().contains(&id) {
            return Err(error(
                error_code::INVALID_INPUT,
                "not a PIN this server began",
            ));
        }
        let plex_tv = self.plex_tv(settings)?;
        let state = plex_tv
            .check(id)
            .await
            .map_err(|e| error(error_code::UNAVAILABLE, e.to_string()))?;
        let token = match state {
            PinState::Waiting => return Ok(PlexPinState::Waiting),
            PinState::Expired => {
                self.pins.lock().remove(&id);
                return Ok(PlexPinState::Expired);
            }
            PinState::Linked(token) => token,
        };
        self.pins.lock().remove(&id);
        // Signing in to add another server leaves the first one's token
        // alone: each server added takes its own from the account.
        if keep_token {
            settings.set(PLEX_TOKEN, &serde_json::Value::String(token.clone()), &by)?;
        }
        *self.signed_in.lock() = Some(token.clone());
        // Only for the page to say who; the token is kept either way.
        let account = plex_tv.account(&token).await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "cannot ask plex.tv whose token it is");
            None
        });
        if keep_token {
            tracing::info!(
                by,
                account,
                "signed in with Plex: the token is kept as plex.token"
            );
        } else {
            tracing::info!(
                by,
                account,
                "signed in with Plex: its servers can be added with their own tokens"
            );
        }
        Ok(PlexPinState::Linked { account })
    }

    async fn plex_path_try(
        &self,
        ext: &Extensions,
        mappings: Vec<PathMappingEntry>,
        server: Option<String>,
        sections: Option<Vec<String>>,
    ) -> RpcResult<Vec<LibraryPathCheck>> {
        self.admin(ext).await?;
        let server = server.unwrap_or_else(|| FIRST_PLEX_SERVER.to_owned());
        let composite = self.plex.composite().ok_or_else(|| {
            error(
                error_code::NOT_CONFIGURED,
                "this server is not set up: no Plex server is configured",
            )
        })?;
        let source = composite.server(&server).ok_or_else(|| {
            error(
                error_code::INVALID_INPUT,
                format!("there is no Plex server {server}"),
            )
        })?;
        // Checked as the setting will be.
        let paths = crate::config::path_map(&libraries::mappings(mappings), "plex.path_mapping")
            .map_err(|e| error(error_code::INVALID_INPUT, e.to_string()))?;
        let plex = match sections {
            Some(sections) => source
                .catalog
                .unrestricted()
                .serving(Some(sections.into_iter().collect()))
                .trying_paths(paths),
            None => source.catalog.trying_paths(paths),
        };
        let reports = plex
            .check_paths(None, Scope::Sample(PATH_SAMPLE))
            .await
            .map_err(|e| error(error_code::UNAVAILABLE, e.to_string()))?;
        let bases = self.path_bases();
        Ok(reports
            .into_iter()
            .map(|r| libraries::path_check(r, &bases))
            .collect())
    }

    async fn libraries(&self, ext: &Extensions) -> RpcResult<LibrariesOverview> {
        let (settings, _) = self.settings_as_admin(ext).await?;
        Ok(settings.libraries_overview(&self.path_bases()).await?)
    }

    async fn library_add(
        &self,
        ext: &Extensions,
        server: String,
        section: String,
        name: Option<String>,
    ) -> RpcResult<String> {
        let (settings, by) = self.settings_as_admin(ext).await?;
        Ok(settings
            .library_add(&server, &section, name.as_deref(), &by)
            .await?)
    }

    async fn library_rename(&self, ext: &Extensions, id: String, name: String) -> RpcResult<()> {
        let (settings, by) = self.settings_as_admin(ext).await?;
        Ok(settings.library_rename(&id, &name, &by).await?)
    }

    async fn library_move(&self, ext: &Extensions, id: String, position: u32) -> RpcResult<()> {
        let (settings, by) = self.settings_as_admin(ext).await?;
        Ok(settings
            .library_move(&id, usize::try_from(position).unwrap_or(usize::MAX), &by)
            .await?)
    }

    async fn library_set_sync(
        &self,
        ext: &Extensions,
        id: String,
        sync: SyncDirection,
    ) -> RpcResult<()> {
        let (settings, by) = self.settings_as_admin(ext).await?;
        Ok(settings.library_set_sync(&id, sync, &by).await?)
    }

    async fn library_remove(&self, ext: &Extensions, id: String) -> RpcResult<()> {
        let (settings, by) = self.settings_as_admin(ext).await?;
        Ok(settings.library_remove(&id, &by).await?)
    }

    async fn plex_server_add(
        &self,
        ext: &Extensions,
        url: String,
        token: Option<String>,
    ) -> RpcResult<PlexServerEntry> {
        // The first server takes the token kept before it, so its address
        // is where that token goes (finding 47), as `plex.url` is. A later
        // one has its own token, given or the linked account's for it.
        let (settings, _) = self.settings_as_admin(ext).await?;
        let first = !settings.plex_configured()?;
        let (settings, by) = self.settings_as_recent_admin(ext, first).await?;
        let token = match token.filter(|t| !t.trim().is_empty()) {
            Some(token) => Some(token),
            None => self.linked_token_for(ext, &url).await?,
        };
        let id = settings
            .plex_server_add(&url, token.as_deref(), &by)
            .await?;
        let overview = settings.libraries_overview(&self.path_bases()).await?;
        overview
            .servers
            .into_iter()
            .find(|s| s.id == id)
            .ok_or_else(|| internal("the server added is not configured"))
    }

    async fn plex_server_set_token(
        &self,
        ext: &Extensions,
        id: String,
        token: String,
    ) -> RpcResult<()> {
        let (settings, by) = self.settings_as_recent_admin(ext, true).await?;
        Ok(settings.plex_server_set_token(&id, &token, &by)?)
    }

    async fn plex_server_set_paths(
        &self,
        ext: &Extensions,
        id: String,
        mappings: Vec<PathMappingEntry>,
    ) -> RpcResult<()> {
        let (settings, by) = self.settings_as_admin(ext).await?;
        Ok(settings.plex_server_set_paths(&id, libraries::mappings(mappings), &by)?)
    }

    async fn plex_server_remove(&self, ext: &Extensions, id: String) -> RpcResult<()> {
        let (settings, by) = self.settings_as_admin(ext).await?;
        Ok(settings.plex_server_remove(&id, &by)?)
    }

    async fn catalog_library(&self, ext: &Extensions, id: String) -> RpcResult<CatalogLibrary> {
        self.admin(ext).await?;
        self.catalog_library_of(id).await
    }

    async fn catalog_rows(&self, ext: &Extensions, query: CatalogQuery) -> RpcResult<CatalogPage> {
        self.admin(ext).await?;
        self.catalog_rows_of(query).await
    }

    async fn catalog_children(
        &self,
        ext: &Extensions,
        library: String,
        id: String,
    ) -> RpcResult<Vec<CatalogRow>> {
        self.admin(ext).await?;
        self.catalog_children_of(library, id).await
    }

    async fn catalog_item(&self, ext: &Extensions, id: String) -> RpcResult<CatalogItem> {
        let user = self.admin(ext).await?;
        self.catalog_item_of(user.user, id).await
    }

    async fn catalog_artwork(&self, ext: &Extensions, id: String) -> RpcResult<Vec<ArtworkInfo>> {
        self.admin(ext).await?;
        self.catalog_artwork_of(id).await
    }

    async fn catalog_check_paths(&self, ext: &Extensions, id: String) -> RpcResult<PathsChecked> {
        self.admin(ext).await?;
        self.catalog_check_paths_of(id).await
    }

    async fn activity(&self, ext: &Extensions) -> RpcResult<Activity> {
        self.admin(ext).await?;
        let Some(metrics) = &self.metrics else {
            return Err(error(
                error_code::NOT_CONFIGURED,
                "this server keeps no metrics",
            ));
        };
        let (history, now) = metrics.activity();
        let names: std::collections::HashMap<_, _> = self
            .auth
            .store()
            .list_users()
            .await?
            .into_iter()
            .map(|u| (u.id, u.username))
            .collect();
        Ok(Activity {
            sample_secs: crate::metrics::history::SAMPLE_EVERY.as_secs() as u32,
            now: metrics.now_point(&now),
            history,
            totals: now.totals,
            plex: metrics
                .plex_servers()
                .into_iter()
                .map(|(id, name, counts)| PlexServerActivity {
                    id,
                    name,
                    requests: counts.requests,
                    errors: counts.errors,
                    mean_latency_ms: (counts.requests > 0)
                        .then(|| counts.sum_seconds * 1000.0 / counts.requests as f64),
                })
                .collect(),
            sessions: metrics
                .sessions()
                .into_iter()
                .map(|s| UserSessionCount {
                    user_id: format!("local:{}", s.user),
                    username: names.get(&s.user).cloned().unwrap_or_default(),
                    app: s.app,
                    web: s.web,
                })
                .collect(),
        })
    }

    async fn telemetry_status(&self, ext: &Extensions) -> RpcResult<TelemetryStatus> {
        self.admin(ext).await?;
        let now = crate::metrics::history::unix_ms();
        let metrics = MetricsEndpointStatus {
            enabled: self.metrics.as_ref().is_some_and(|m| m.access().enabled()),
            path: "/metrics".into(),
            refused: self
                .metrics
                .as_ref()
                .map(|m| m.access().refused_last_hour(now))
                .unwrap_or_default()
                .into_iter()
                .map(|r| RefusedScrape {
                    address: r.address.to_string(),
                    count: r.count,
                    last_at: r.last_at,
                })
                .collect(),
        };
        let Some(telemetry) = &self.telemetry else {
            return Ok(TelemetryStatus {
                otlp: None,
                resource: Vec::new(),
                metrics,
            });
        };
        let otlp = telemetry.status().map(|(settings, status)| OtlpStatus {
            endpoint: settings.endpoint,
            protocol: match settings.protocol {
                crate::telemetry::Protocol::Grpc => "grpc".into(),
                crate::telemetry::Protocol::Http => "http".into(),
            },
            traces: settings.traces,
            metrics: settings.metrics,
            logs: settings.logs,
            header_names: settings.headers.into_iter().map(|(k, _)| k).collect(),
            last_export_at: status.last_export_at,
            dropped: status.dropped,
            dropping_since: status.dropping_since,
            last_error: status.last_error,
            last_error_at: status.last_error_at,
            metrics_every_secs: telemetry.metrics_every().as_secs() as u32,
        });
        Ok(TelemetryStatus {
            otlp,
            resource: telemetry
                .resource_attributes()
                .into_iter()
                .map(|(key, value)| ResourceAttribute { key, value })
                .collect(),
            metrics,
        })
    }

    async fn logs(&self, ext: &Extensions, query: LogQuery) -> RpcResult<LogPage> {
        self.admin(ext).await?;
        match &self.logs {
            Some(ring) => Ok(ring.page(&query)),
            None => Err(error(
                error_code::NOT_CONFIGURED,
                "this server keeps no log lines",
            )),
        }
    }

    async fn playbacks(&self, ext: &Extensions) -> RpcResult<spoolframe_admin_api::PlaybackList> {
        self.admin(ext).await?;
        Ok(self.playback_debug()?.list())
    }

    async fn playback(
        &self,
        ext: &Extensions,
        id: String,
    ) -> RpcResult<spoolframe_admin_api::PlaybackDetail> {
        self.admin(ext).await?;
        self.playback_debug()?
            .detail(&id)
            .await
            .ok_or_else(|| no_such_playback(&id))
    }

    async fn playback_stop(&self, ext: &Extensions, id: String) -> RpcResult<()> {
        self.admin(ext).await?;
        if self.playback_debug()?.stop(&id) {
            Ok(())
        } else {
            Err(no_such_playback(&id))
        }
    }

    async fn playback_end_conversion(&self, ext: &Extensions, id: String) -> RpcResult<()> {
        self.admin(ext).await?;
        self.playback_debug()?
            .end_conversion(&id)
            .map_err(|e| match e {
                crate::playback::EndConversionError::NotFound => no_such_playback(&id),
                crate::playback::EndConversionError::NoConversion => error(
                    error_code::NO_CONVERSION,
                    "this playback plays the file as it is: there is no conversion to end",
                ),
            })
    }

    async fn catalog_debug(
        &self,
        ext: &Extensions,
        id: String,
    ) -> RpcResult<spoolframe_admin_api::ItemDebug> {
        self.admin(ext).await?;
        let id = catalog::media_id(&id)?;
        self.playback_debug()?.item(&id).await.map_err(debug_error)
    }

    async fn catalog_plan_for(
        &self,
        ext: &Extensions,
        id: String,
        request: spoolframe_admin_api::PlanFor,
    ) -> RpcResult<spoolframe_admin_api::PlanView> {
        self.admin(ext).await?;
        let id = catalog::media_id(&id)?;
        self.playback_debug()?
            .plan_for(&id, &request)
            .await
            .map_err(debug_error)
    }
}

fn no_such_playback(id: &str) -> ErrorObjectOwned {
    error(
        error_code::NO_SUCH_PLAYBACK,
        format!("no playback {id} is open or ended in the last hour"),
    )
}

fn debug_error(e: crate::playback_debug::DebugError) -> ErrorObjectOwned {
    use crate::playback_debug::DebugError;
    match e {
        DebugError::Catalog(e) => catalog::catalog_error(e),
        DebugError::Unreadable(why) => error(error_code::UNAVAILABLE, why),
        DebugError::NoSuchDevice(why) => error(error_code::NO_SUCH_DEVICE, why),
    }
}

/// The source revision this binary was built from, when the build named
/// one (P3-18): the nix package sets `SPOOLFRAME_GIT_REV` to the flake's
/// short revision; a plain `cargo build` leaves it out.
const BUILD_REVISION: Option<&str> = option_env!("SPOOLFRAME_GIT_REV");

fn revision(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_owned)
}

/// How many files of each library the wizard's path check, and the
/// libraries overview, read: as many as the check at startup.
pub const PATH_SAMPLE: u32 = 5;

fn setup_state((done, finished): (Vec<String>, bool)) -> SetupState {
    SetupState { finished, done }
}

impl AdminApi {
    /// The token the caller's own linked Plex account has for the server
    /// at `url`, if it reaches it (P3-12): plex.tv hands out one per
    /// server. `None` without a link, or when plex.tv cannot be asked.
    async fn linked_token_for(&self, ext: &Extensions, url: &str) -> RpcResult<Option<String>> {
        let (_, user) = self.caller(ext).await?;
        let linked = match &self.accounts {
            Some(accounts) => accounts.account(user.user).map_err(internal)?,
            None => None,
        };
        let token = match linked {
            Some(link) => link.token,
            None => match self.wizard_sign_in() {
                Some(token) => token,
                None => return Ok(None),
            },
        };
        match self.servers_of(&token).await {
            Ok(resources) => Ok(crate::plex_accounts::token_for_url(&resources, url.trim())),
            Err(e) => {
                tracing::warn!(error = %e.message(), "cannot list the Plex account's servers");
                Ok(None)
            }
        }
    }

    /// The account token the wizard's last Sign in with Plex got.
    fn wizard_sign_in(&self) -> Option<String> {
        self.signed_in.lock().clone()
    }

    /// The Plex servers `token`'s account reaches, asked of plex.tv.
    async fn servers_of(
        &self,
        token: &str,
    ) -> RpcResult<Vec<spoolframe_plex::account::PlexResource>> {
        if let Some(accounts) = &self.accounts {
            return Ok(accounts.servers(token).await?);
        }
        let Some(settings) = &self.settings else {
            return Err(error(
                error_code::NOT_CONFIGURED,
                "this server does not reach plex.tv",
            ));
        };
        self.plex_tv(settings)?
            .servers(token)
            .await
            .map_err(|e| error(error_code::UNAVAILABLE, e.to_string()))
    }

    /// The servers of the Plex account `user` is offered: their own linked
    /// one, else the one the wizard last signed in with (P3-10), before
    /// anyone has linked one. `None` without either.
    async fn offered_servers(
        &self,
        user: crate::store::users::LocalUserId,
    ) -> RpcResult<Option<Vec<spoolframe_plex::account::PlexResource>>> {
        let token = match self.account_token(user)? {
            Some(token) => token,
            None => match self.wizard_sign_in() {
                Some(token) => token,
                None => return Ok(None),
            },
        };
        Ok(Some(self.servers_of(&token).await?))
    }

    /// Where suggested path mappings are looked for.
    fn path_bases(&self) -> Vec<std::path::PathBuf> {
        self.wizard
            .path_bases
            .clone()
            .unwrap_or_else(spoolframe_plex::paths::usual_bases)
    }

    fn plex_tv(&self, settings: &RuntimeSettings) -> RpcResult<PlexTv> {
        let client_id = settings.plex_client_id().map_err(internal)?;
        PlexTv::new(
            self.wizard.plex_tv.clone(),
            self.wizard.plex_app.clone(),
            client_id,
        )
        .map_err(|e| error(error_code::UNAVAILABLE, e.to_string()))
    }

    /// Before the first admin exists: the setup code from the log is
    /// right, and with `insecure_cookies` the session cookie is sent
    /// without `Secure` from now on, so the admin made next stays signed
    /// in over plain HTTP (P3-10). The code is what authorizes it, as it
    /// authorizes making the admin.
    pub(crate) async fn setup_check(
        &self,
        ext: &Extensions,
        code: &str,
        insecure_cookies: bool,
    ) -> RpcResult<()> {
        self.setup_code(ext, code)?;
        if !self.setup_open().await? {
            return Err(UserError::AlreadySetUp.into());
        }
        if insecure_cookies {
            let Some(settings) = &self.settings else {
                return Err(error(
                    error_code::NOT_CONFIGURED,
                    "this server does not change settings at run time",
                ));
            };
            settings.set(
                "admin.insecure_cookies",
                &serde_json::Value::Bool(true),
                "the setup code",
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_s_revision_is_what_the_build_named_or_none() {
        assert_eq!(revision(Some("9f3c2a1")).as_deref(), Some("9f3c2a1"));
        assert_eq!(
            revision(Some(" 9f3c2a1-dirty\n")).as_deref(),
            Some("9f3c2a1-dirty")
        );
        // nix sets the variable empty when the tree has no revision.
        assert_eq!(revision(Some("")), None);
        assert_eq!(revision(None), None);
    }

    #[test]
    fn a_code_reads_in_groups_and_matches_however_it_is_typed() {
        let code = SetupCode::new("K7QDM2XP9RTB");
        assert_eq!(code.to_string(), "K7QD-M2XP-9RTB");
        assert!(code.matches("k7qd-m2xp-9rtb"));
        assert!(code.matches("K7QD M2XP 9RTB"));
        assert!(!code.matches("K7QD-M2XP-9RTC"));
        assert!(!code.matches(""));
    }

    #[test]
    fn a_loopback_peer_is_no_source_and_any_other_is_its_address() {
        let source = |ip: &str| Peer(Some(ip.parse().unwrap())).source();
        assert_eq!(source("127.0.0.1"), None);
        assert_eq!(source("::1"), None);
        assert_eq!(source("::ffff:127.0.0.1"), None);
        assert_eq!(source("192.0.2.7").as_deref(), Some("192.0.2.7"));
        assert_eq!(source("::ffff:192.0.2.7").as_deref(), Some("192.0.2.7"));
        assert_eq!(source("2001:db8::7").as_deref(), Some("2001:db8::7"));
        assert_eq!(Peer(None).source(), None);
    }

    /// The API over a store of its own, and alice's session in it: what
    /// her calls carry, and the session's key in the store.
    async fn api_with_alice(test: &str) -> (AdminApi, Extensions, String, impl Sized) {
        let dir = crate::store::tests::state_dir(test).await;
        let store = crate::store::Store::open(dir.as_ref()).await.unwrap();
        let phc = hash_password("correct horse").await.unwrap();
        store
            .add_user("alice", None, &phc, Role::Consumer)
            .await
            .unwrap();
        let auth = Auth::new(store);
        let login = auth
            .login_password("alice", "correct horse", SessionBinding::Api, None, false)
            .await
            .unwrap();
        let mut ext = Extensions::new();
        ext.insert(Bearer(login.token.0.clone()));
        ext.insert(Peer(Some("127.0.0.1".parse().unwrap())));
        (api(auth), ext, login.token.hash(), dir)
    }

    /// The API over `auth`, at `https://media.example.org`.
    fn api(auth: Auth) -> AdminApi {
        let transcoding = Transcoding::new(
            Box::new(spoolframe_media::encoders::FfmpegCommand::new(
                spoolframe_media::sandbox::Tool::unconfined("/nonexistent/ffmpeg"),
            )),
            Default::default(),
            std::env::temp_dir(),
        );
        let origins = Origins::Exactly("https://media.example.org".into());
        AdminApi {
            auth,
            setup_code: None,
            status: Arc::new(StatusSource::new(
                ServerName::new("Den"),
                spoolframe_protocol::ServerId::from_bytes([0; 10]),
                Arc::new(transcoding),
            )),
            plex_sync: None,
            accounts: None,
            plex: Arc::default(),
            passkeys: Arc::new(Passkeys::new(
                &origins,
                &spoolframe_protocol::ServerId::from_bytes([0; 10]),
            )),
            cookies: CookiePolicy::default(),
            settings: None,
            wizard: Wizard::default(),
            oidc: Arc::default(),
            watch: None,
            artwork: None,
            metrics: None,
            logs: None,
            telemetry: None,
            playback: None,
            checks: Arc::default(),
            pins: Arc::default(),
            signed_in: Arc::default(),
            public_url: origins.public_url().map(str::to_owned),
        }
    }

    #[tokio::test]
    async fn track_preferences_say_whether_picture_subtitles_are_read_as_text() {
        let (mut api, ext, _, _dir) = api_with_alice("track-preferences-ocr").await;
        assert!(!api.track_preferences(&ext).await.unwrap().ocr_offered);
        let transcoding = api.status.transcoding.clone();
        api.status = Arc::new(
            StatusSource::new(
                ServerName::new("Den"),
                spoolframe_protocol::ServerId::from_bytes([0; 10]),
                transcoding,
            )
            .with_ocr_languages(vec!["eng".into()]),
        );
        let read = api.track_preferences(&ext).await.unwrap();
        assert!(read.ocr_offered);
        assert_eq!(read.preferences, TrackPreferences::default());
    }

    #[tokio::test]
    async fn old_web_sessions_cannot_approve_devices() {
        let (api, ext, hash, _dir) = api_with_alice("device-approval-recent").await;
        api.auth
            .store()
            .age_session(&hash, RECENT_SIGN_IN.as_secs() as i64 + 1)
            .await;
        let started = api
            .auth
            .sign_ins()
            .begin(crate::device_sign_in::Waiting {
                address: None,
                endpoint: "new-device".into(),
                device_name: None,
                client_name: "test".into(),
                platform: spoolframe_types::Platform::Linux,
                requested_at: crate::store::now(),
                remember: false,
            })
            .unwrap();
        let error = api
            .device_sign_in_approve(&ext, started.code.clone(), false)
            .await
            .unwrap_err();
        assert_eq!(error.code(), error_code::RECENT_SIGN_IN_REQUIRED);
        assert!(api.auth.sign_ins().lookup(&started.code).is_some());
        api.auth.store().confirm_session(&hash).await.unwrap();
        api.device_sign_in_approve(&ext, started.code, false)
            .await
            .unwrap();
    }

    /// A user set up with a passkey has no password to prove it is them
    /// with, so a first password needs a recent sign-in, as a passkey does.
    #[tokio::test]
    async fn a_first_password_is_set_only_soon_after_signing_in() {
        let dir = crate::store::tests::state_dir("first-password").await;
        let store = crate::store::Store::open(dir.as_ref()).await.unwrap();
        let root = store
            .add_first_admin_with_passkey(
                "root",
                FirstPasskey {
                    handle: "handle-1",
                    credential_id: "cred-1",
                    name: "iPhone",
                    credential: "{}",
                    rp_id: "media.example.org",
                },
            )
            .await
            .unwrap();
        let auth = Auth::new(store.clone());
        let token = auth.open_api_session(root).await.unwrap();
        let api = api(auth);
        let mut ext = Extensions::new();
        ext.insert(Bearer(token.0.clone()));
        ext.insert(Peer(Some("127.0.0.1".parse().unwrap())));
        let first = || PasswordChange {
            current_password: String::new(),
            new_password: "correct horse".into(),
        };
        store
            .age_session(&token.hash(), RECENT_SIGN_IN.as_secs() as i64 + 1)
            .await;
        let stale = api.set_password(&ext, first()).await.unwrap_err();
        assert_eq!(stale.code(), error_code::RECENT_SIGN_IN_REQUIRED);
        assert!(!api.who_am_i(&ext).await.unwrap().has_password);

        store
            .age_session(&token.hash(), -(RECENT_SIGN_IN.as_secs() as i64))
            .await;
        api.set_password(&ext, first()).await.unwrap();
        api.auth
            .login_password("root", "correct horse", SessionBinding::Api, None, false)
            .await
            .unwrap();
    }

    /// Removing a password, the caller's or a user's, needs a recent
    /// sign-in; and the last admin with a password keeps it.
    #[tokio::test]
    async fn a_password_is_removed_only_soon_after_signing_in() {
        let dir = crate::store::tests::state_dir("remove-password-api").await;
        let store = crate::store::Store::open(dir.as_ref()).await.unwrap();
        let root = store.add_first_admin("root", "phc").await.unwrap();
        store
            .add_passkey(root, "cred-1", "iPhone", "{}", "media.example.org")
            .await
            .unwrap();
        let alice = store
            .add_user("alice", None, "phc", Role::Admin)
            .await
            .unwrap();
        store
            .add_passkey(alice, "cred-2", "Pixel", "{}", "media.example.org")
            .await
            .unwrap();
        let auth = Auth::new(store.clone());
        let token = auth.open_api_session(root).await.unwrap();
        let api = api(auth);
        let mut ext = Extensions::new();
        ext.insert(Bearer(token.0.clone()));
        ext.insert(Peer(Some("127.0.0.1".parse().unwrap())));
        store
            .age_session(&token.hash(), RECENT_SIGN_IN.as_secs() as i64 + 1)
            .await;
        let stale = api.password_remove(&ext).await.unwrap_err();
        assert_eq!(stale.code(), error_code::RECENT_SIGN_IN_REQUIRED);
        let stale = api
            .user_remove_password(&ext, "alice".into())
            .await
            .unwrap_err();
        assert_eq!(stale.code(), error_code::RECENT_SIGN_IN_REQUIRED);

        store
            .age_session(&token.hash(), -(RECENT_SIGN_IN.as_secs() as i64))
            .await;
        api.user_remove_password(&ext, "alice".into())
            .await
            .unwrap();
        // Root is now the last admin with a password.
        let last = api.password_remove(&ext).await.unwrap_err();
        assert_eq!(last.code(), error_code::LAST_ADMIN);
        assert!(api.who_am_i(&ext).await.unwrap().has_password);
    }

    /// P3-03: a sensitive change needs a sign-in, or a confirmation, within
    /// the last 10 minutes.
    #[tokio::test]
    async fn a_passkey_is_added_or_removed_only_soon_after_signing_in() {
        let (api, ext, session, _dir) = api_with_alice("recent").await;
        let store = api.auth.store().clone();
        let half = RECENT_SIGN_IN.as_secs() as i64 / 2;
        store.age_session(&session, half).await;
        api.passkey_add_begin(&ext).await.unwrap();

        let (_, alice) = api.caller(&ext).await.unwrap();
        store
            .add_passkey(alice.user, "cred-1", "MacBook", "{}", "media.example.org")
            .await
            .unwrap();
        store.age_session(&session, half + 1).await;
        let stale = api.passkey_add_begin(&ext).await.unwrap_err();
        assert_eq!(stale.code(), error_code::RECENT_SIGN_IN_REQUIRED);
        let stale = api.passkey_remove(&ext, "cred-1".into()).await.unwrap_err();
        assert_eq!(stale.code(), error_code::RECENT_SIGN_IN_REQUIRED);
        // What is not sensitive still works.
        api.passkey_rename(&ext, "cred-1".into(), "Laptop".into())
            .await
            .unwrap();
        assert_eq!(api.passkeys(&ext).await.unwrap()[0].name, "Laptop");

        // A wrong password confirms nothing.
        let wrong = api
            .confirm_password(&ext, "battery staple".into())
            .await
            .unwrap_err();
        assert_eq!(wrong.code(), error_code::INVALID_CREDENTIALS);
        let stale = api.passkey_remove(&ext, "cred-1".into()).await.unwrap_err();
        assert_eq!(stale.code(), error_code::RECENT_SIGN_IN_REQUIRED);

        api.confirm_password(&ext, "correct horse".into())
            .await
            .unwrap();
        api.passkey_remove(&ext, "cred-1".into()).await.unwrap();
        assert_eq!(api.passkeys(&ext).await.unwrap(), []);
        api.passkey_add_begin(&ext).await.unwrap();
    }

    /// P3-03: the PIN guards the user's devices, so setting, removing or
    /// no longer asking for it is a sensitive change; asking for it on a
    /// device is not.
    #[tokio::test]
    async fn the_pin_is_changed_only_soon_after_signing_in() {
        let (api, ext, session, _dir) = api_with_alice("recent-pin").await;
        let store = api.auth.store().clone();
        let (_, alice) = api.caller(&ext).await.unwrap();
        let tv = "ab".repeat(32);
        store
            .create_session(
                "tv",
                alice.user,
                &SessionBinding::Endpoint {
                    endpoint_id: tv.clone(),
                    device_name: Some("TV".into()),
                    platform: spoolframe_types::Platform::Tvos,
                },
                i64::MAX / 2,
            )
            .await
            .unwrap();
        api.pin_set(&ext, "43210".into()).await.unwrap();
        // Set here, it is as long for the client protocol's WhoAmI.
        assert_eq!(
            store.pin_info(alice.user).await.unwrap(),
            Some(crate::store::pins::PinInfo { digits: Some(5) })
        );
        store
            .age_session(&session, RECENT_SIGN_IN.as_secs() as i64 + 1)
            .await;
        let recent = |r: RpcResult<()>| {
            assert_eq!(r.unwrap_err().code(), error_code::RECENT_SIGN_IN_REQUIRED);
        };
        recent(api.pin_set(&ext, "1234".into()).await);
        recent(api.pin_clear(&ext).await);
        api.set_pin_required(&ext, tv.clone(), true).await.unwrap();
        recent(api.set_pin_required(&ext, tv.clone(), false).await);
        assert!(api.devices(&ext).await.unwrap()[0].pin_required);
        assert!(api.who_am_i(&ext).await.unwrap().has_pin);

        api.confirm_password(&ext, "correct horse".into())
            .await
            .unwrap();
        api.set_pin_required(&ext, tv, false).await.unwrap();
        api.pin_clear(&ext).await.unwrap();
        assert!(!api.who_am_i(&ext).await.unwrap().has_pin);
    }

    /// P3-03: a password change from a session signed in long ago asks
    /// for the current password, and goes through with it.
    #[tokio::test]
    async fn an_old_session_changes_the_password_only_with_the_current_one() {
        let (api, ext, session, _dir) = api_with_alice("old-password").await;
        api.auth
            .store()
            .age_session(&session, RECENT_SIGN_IN.as_secs() as i64 * 6)
            .await;
        let change = |current: &str| PasswordChange {
            current_password: current.into(),
            new_password: "battery staple".into(),
        };
        let wrong = api.set_password(&ext, change("guess")).await.unwrap_err();
        assert_eq!(wrong.code(), error_code::INVALID_CREDENTIALS);
        api.set_password(&ext, change("correct horse"))
            .await
            .unwrap();
        let old = api
            .auth
            .login_password("alice", "correct horse", SessionBinding::Api, None, false)
            .await;
        assert!(matches!(old, Err(LoginError::InvalidCredentials)));
    }

    #[test]
    fn a_wildcard_listener_answers_on_every_address_of_its_family() {
        let host: Vec<IpAddr> = [
            "127.0.0.1",
            "192.168.1.20",
            "::1",
            "fe80::1",
            "fd00::20",
            "10.0.0.5",
        ]
        .iter()
        .map(|a| a.parse().unwrap())
        .collect();
        assert_eq!(
            urls("0.0.0.0:7421".parse().unwrap(), &host),
            [
                "http://192.168.1.20:7421/",
                "http://10.0.0.5:7421/",
                "http://127.0.0.1:7421/"
            ]
        );
        assert_eq!(
            urls("[::]:7421".parse().unwrap(), &host),
            [
                "http://192.168.1.20:7421/",
                "http://10.0.0.5:7421/",
                "http://[fd00::20]:7421/",
                "http://127.0.0.1:7421/",
                "http://[::1]:7421/"
            ]
        );
        assert_eq!(
            urls("127.0.0.1:7421".parse().unwrap(), &host),
            ["http://127.0.0.1:7421/"]
        );
    }

    #[test]
    fn this_host_has_a_loopback_address() {
        assert!(
            host_addresses().iter().any(IpAddr::is_loopback),
            "{:?}",
            host_addresses()
        );
    }

    #[test]
    fn generated_codes_use_the_unambiguous_alphabet() {
        let code = SetupCode::generate();
        assert_eq!(code.0.len(), SetupCode::LEN);
        assert!(code.0.bytes().all(|b| SetupCode::ALPHABET.contains(&b)));
        assert_ne!(SetupCode::generate(), code);
    }
}
