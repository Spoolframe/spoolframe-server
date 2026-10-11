//! The WAN uplink budget (PX-46): which devices are on the LAN, how much
//! of the home's upload each playback off it may take, and telling the
//! running ones when that changes.
//!
//! A device is on the LAN when its address is in one of this host's own
//! interface subnets (the interface's real prefix: a global IPv6 address
//! in the home's /64 is the LAN, one on 5G is not), private, link-local
//! or unique local (as the log always called them), or in
//! `network.lan_prefixes`. A relay path is never the LAN.
//!
//! The budget is split between the WAN playbacks open now: equally, but
//! while an owner (an admin of this server) plays off the LAN, the
//! owners' playbacks share `owner_share_percent` of it and everyone
//! else's the rest. A share only caps `Quality::Auto`; one the viewer
//! chose is theirs.

use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::admin_http::Network;

/// `network.uplink` and `network.lan_prefixes`, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UplinkSettings {
    /// What the WAN playbacks may take together, in bit/s; `None` is
    /// no limit (`budget_mbps = 0`, the default).
    pub budget: Option<u64>,
    /// The most WAN playbacks at once; `None` is no limit.
    pub max_streams: Option<usize>,
    /// The owners' part of the budget while one of them plays off the
    /// LAN beside anyone else, 0 to 100.
    pub owner_share_percent: u8,
    /// Ranges whose devices are on the LAN besides the host's own
    /// subnets.
    pub lan_prefixes: Vec<Network>,
}

/// `network.uplink.owner_share_percent` left out.
pub const DEFAULT_OWNER_SHARE_PERCENT: u8 = 50;

impl Default for UplinkSettings {
    fn default() -> Self {
        Self {
            budget: None,
            max_streams: None,
            owner_share_percent: DEFAULT_OWNER_SHARE_PERCENT,
            lan_prefixes: Vec::new(),
        }
    }
}

/// Whose a WAN playback is, for the share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// An admin of this server: the owner's household.
    Owner,
    /// Anyone else: a consumer, a portable guest, later a federated user.
    Guest,
}

/// Whether `ip` is on the LAN: loopback or link-local, or in one of
/// `subnets` (this host's, tunnels left out) or `extra`
/// (`network.lan_prefixes`). A private address is not the LAN for being
/// private (review 2026-10-11 M28): a phone on cellular reaching the
/// server over Tailscale (100.64.0.0/10) or WireGuard (10.x) has a private
/// address, and its bytes leave through the home's uplink. A tunnel's
/// network counts as home only when an admin lists it.
pub fn is_lan(ip: IpAddr, subnets: &[Network], extra: &[Network]) -> bool {
    let ip = ip.to_canonical();
    ip.is_loopback()
        || link_local(ip)
        || subnets.iter().any(|n| n.contains(ip))
        || extra.iter().any(|n| n.contains(ip))
}

fn link_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => v6.segments()[0] & 0xffc0 == 0xfe80,
    }
}

/// Whether an interface named `name` is a tunnel (Tailscale, WireGuard,
/// tun): the devices at its far end are wherever they are, so its
/// subnet is not the LAN's.
fn tunnel(name: &str) -> bool {
    ["tailscale", "wg", "tun", "utun"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// How long the host's subnets are kept before they are read again: an
/// address the router gives out changes rarely, and reading them is a
/// system call per path change otherwise.
const SUBNETS_FOR: Duration = Duration::from_secs(30);

/// Each `budget` part a WAN playback of each of `classes` gets, in their
/// order: equal parts, but while there are owners and guests both, the
/// owners share `owner_percent` of the budget and the guests the rest.
pub fn shares(budget: u64, owner_percent: u8, classes: &[Class]) -> Vec<u64> {
    let owners = classes.iter().filter(|c| **c == Class::Owner).count() as u64;
    let guests = classes.len() as u64 - owners;
    if owners == 0 || guests == 0 {
        let each = budget / (owners + guests).max(1);
        return vec![each; classes.len()];
    }
    let owned = budget * u64::from(owner_percent.min(100)) / 100;
    let owner_each = owned / owners;
    let guest_each = (budget - owned) / guests;
    classes
        .iter()
        .map(|c| match c {
            Class::Owner => owner_each,
            Class::Guest => guest_each,
        })
        .collect()
}

/// A share that moved by less than this part of the one told is not told
/// again: a re-plan is a rebuffer.
pub const HYSTERESIS: f64 = 0.15;

/// Whether a playback told `before` should be told `now`.
pub fn worth_telling(before: Option<u64>, now: Option<u64>) -> bool {
    match (before, now) {
        (None, None) => false,
        (Some(_), None) | (None, Some(_)) => true,
        (Some(before), Some(now)) => {
            let before = before.max(1) as f64;
            (now as f64 - before).abs() / before >= HYSTERESIS
        }
    }
}

/// A WAN playback the budget is split with.
#[derive(Debug, Clone)]
struct Stream {
    class: Class,
    /// The connection whose events stream it is told on.
    connection: usize,
    /// The share it was planned with, or last told.
    told: Option<u64>,
}

/// A new WAN playback would be one over `network.uplink.max_streams`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Full {
    pub max: usize,
}

/// One playback's new share to tell, on its connection's events stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Told {
    pub handle: String,
    pub connection: usize,
    pub max_bitrate: Option<u64>,
}

/// The budget, the LAN, and the WAN playbacks open now.
#[derive(Debug)]
pub struct Uplink {
    settings: RwLock<UplinkSettings>,
    subnets: Mutex<Option<(Instant, Vec<Network>)>>,
    streams: Mutex<HashMap<String, Stream>>,
    /// Woken at each start and end, for the shares to be worked out again
    /// once things settle.
    changed: tokio::sync::Notify,
}

impl Default for Uplink {
    fn default() -> Self {
        Self::new(UplinkSettings::default())
    }
}

impl Uplink {
    pub fn new(settings: UplinkSettings) -> Self {
        Self {
            settings: RwLock::new(settings),
            subnets: Mutex::new(None),
            streams: Mutex::new(HashMap::new()),
            changed: tokio::sync::Notify::new(),
        }
    }

    /// The settings changed (a live setting): the shares are worked out
    /// again.
    pub fn set_settings(&self, settings: UplinkSettings) {
        let changed = *self.settings.read() != settings;
        *self.settings.write() = settings;
        if changed {
            self.changed.notify_one();
        }
    }

    pub fn settings(&self) -> UplinkSettings {
        self.settings.read().clone()
    }

    /// Whether `ip` is on the LAN, by this host's subnets as they are now
    /// and `network.lan_prefixes`.
    pub fn is_lan(&self, ip: IpAddr) -> bool {
        let subnets = self.subnets();
        is_lan(ip, &subnets, &self.settings.read().lan_prefixes)
    }

    fn subnets(&self) -> Vec<Network> {
        let mut cached = self.subnets.lock();
        match &*cached {
            Some((at, subnets)) if at.elapsed() < SUBNETS_FOR => subnets.clone(),
            _ => {
                let subnets = host_subnets();
                *cached = Some((Instant::now(), subnets.clone()));
                subnets
            }
        }
    }

    /// The share a new WAN playback of `class` would have, counted with
    /// the ones open now; `None` with no budget. `Full` when one more
    /// would be over `max_streams`.
    pub fn admit(&self, class: Class) -> Result<Option<u64>, Full> {
        let settings = self.settings.read().clone();
        let streams = self.streams.lock();
        if let Some(max) = settings.max_streams
            && streams.len() >= max
        {
            return Err(Full { max });
        }
        let Some(budget) = settings.budget else {
            return Ok(None);
        };
        let mut classes: Vec<Class> = streams.values().map(|s| s.class).collect();
        classes.push(class);
        Ok(shares(budget, settings.owner_share_percent, &classes)
            .last()
            .copied())
    }

    /// A WAN playback opened, planned with `share`: the others' shares
    /// are worked out again shortly.
    pub fn started(&self, handle: &str, class: Class, connection: usize, share: Option<u64>) {
        self.streams.lock().insert(
            handle.to_owned(),
            Stream {
                class,
                connection,
                told: share,
            },
        );
        self.changed.notify_one();
    }

    /// A playback ended; nothing when it was not a WAN one.
    pub fn ended(&self, handle: &str) {
        if self.streams.lock().remove(handle).is_some() {
            self.changed.notify_one();
        }
    }

    /// The share WAN playback `handle` has now, for its re-plans; `None`
    /// when it is not a WAN playback or there is no budget.
    pub fn share_of(&self, handle: &str) -> Option<u64> {
        let settings = self.settings.read().clone();
        let budget = settings.budget?;
        let streams = self.streams.lock();
        let mut handles: Vec<&String> = streams.keys().collect();
        handles.sort();
        let at = handles.iter().position(|h| h.as_str() == handle)?;
        let classes: Vec<Class> = handles.iter().map(|h| streams[*h].class).collect();
        shares(budget, settings.owner_share_percent, &classes)
            .get(at)
            .copied()
    }

    /// The WAN playbacks open now.
    pub fn wan_streams(&self) -> usize {
        self.streams.lock().len()
    }

    /// Work the shares out again, and note what each playback whose
    /// share moved by [`HYSTERESIS`] or more is to be told.
    pub fn rebalance(&self) -> Vec<Told> {
        let settings = self.settings.read().clone();
        let mut streams = self.streams.lock();
        let mut handles: Vec<String> = streams.keys().cloned().collect();
        handles.sort();
        let now: Vec<Option<u64>> = match settings.budget {
            None => vec![None; handles.len()],
            Some(budget) => {
                let classes: Vec<Class> = handles.iter().map(|h| streams[h].class).collect();
                shares(budget, settings.owner_share_percent, &classes)
                    .into_iter()
                    .map(Some)
                    .collect()
            }
        };
        let mut told = Vec::new();
        for (handle, share) in handles.into_iter().zip(now) {
            let stream = streams.get_mut(&handle).expect("listed above");
            if worth_telling(stream.told, share) {
                stream.told = share;
                told.push(Told {
                    handle,
                    connection: stream.connection,
                    max_bitrate: share,
                });
            }
        }
        told
    }

    /// Wait for a start, an end or a settings change, then for
    /// `settle` without another, and work the shares out again.
    pub async fn settled(&self, settle: Duration) -> Vec<Told> {
        self.changed.notified().await;
        loop {
            tokio::select! {
                () = self.changed.notified() => {}
                () = tokio::time::sleep(settle) => break,
            }
        }
        self.rebalance()
    }
}

/// How long starts and ends are let settle before the shares are told:
/// a device's next track, or a film started again, should not re-plan
/// everyone twice.
pub const SETTLE: Duration = Duration::from_secs(3);

/// This host's interface subnets now: each address with its netmask's
/// prefix. Loopback ones are left out (`is_lan` takes them anyway), and a
/// tunnel's ([`tunnel`]) is its own address only.
pub fn host_subnets() -> Vec<Network> {
    let mut found = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list it allocates, read only
    // until the freeifaddrs below; each entry's address and netmask are
    // read as the struct their family names.
    unsafe {
        if libc::getifaddrs(&mut head) != 0 {
            return found;
        }
        let mut entry = head;
        while let Some(ifa) = entry.as_ref() {
            entry = ifa.ifa_next;
            if ifa.ifa_addr.is_null()
                || ifa.ifa_netmask.is_null()
                || ifa.ifa_flags & libc::IFF_UP as u32 == 0
            {
                continue;
            }
            let tunnel = !ifa.ifa_name.is_null()
                && tunnel(&std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy());
            let (ip, prefix) = match i32::from((*ifa.ifa_addr).sa_family) {
                libc::AF_INET => {
                    let sa = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    let mask = &*(ifa.ifa_netmask as *const libc::sockaddr_in);
                    (
                        IpAddr::from(sa.sin_addr.s_addr.to_ne_bytes()),
                        u32::from_be(mask.sin_addr.s_addr).count_ones(),
                    )
                }
                libc::AF_INET6 => {
                    let sa = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                    let mask = &*(ifa.ifa_netmask as *const libc::sockaddr_in6);
                    (
                        IpAddr::from(sa.sin6_addr.s6_addr),
                        u128::from_be_bytes(mask.sin6_addr.s6_addr).count_ones(),
                    )
                }
                _ => continue,
            };
            if ip.is_loopback() || prefix == 0 {
                continue;
            }
            // A tunnel's own address is this machine's, the LAN: its
            // network's other devices are not.
            let prefix = match (tunnel, ip) {
                (true, IpAddr::V4(_)) => 32,
                (true, IpAddr::V6(_)) => 128,
                (false, _) => prefix,
            };
            found.push(Network::new(ip, prefix));
        }
        libc::freeifaddrs(head);
    }
    found
}

/// Tell each of `told` its new share on its connection's events stream,
/// until `uplink` is dropped.
pub(crate) async fn tell_shares(
    uplink: std::sync::Weak<Uplink>,
    streams: crate::events::EventStreams,
) {
    loop {
        let Some(this) = uplink.upgrade() else {
            return;
        };
        let told = this.settled(SETTLE).await;
        drop(this);
        for t in told {
            tracing::info!(
                connection = t.connection,
                max_bitrate = t.max_bitrate,
                "a WAN playback's share of the uplink changed; telling its device"
            );
            streams.send(
                t.connection,
                spoolframe_protocol::Event::PlaybackShare {
                    handle: spoolframe_protocol::PlaybackHandle(t.handle),
                    max_bitrate: t.max_bitrate,
                },
            );
        }
    }
}

/// The shared [`Uplink`] the services hold.
pub type SharedUplink = Arc<Uplink>;

/// Where the upload is measured (PX-46): Cloudflare's public speed
/// test, which takes a POST of any body and answers once it has it.
pub const SPEED_TEST_UP: &str = "https://speed.cloudflare.com/__up";

/// Each upload's size: long enough that the round trip is a small part
/// of its time on a fast upload, short enough on a slow one.
pub const MEASURE_BYTES: usize = 10 << 20;

/// How many uploads are made; their median is the answer.
pub const MEASURE_UPLOADS: usize = 3;

/// The longest one upload may take before the measurement gives up.
pub const MEASURE_TIMEOUT: Duration = Duration::from_secs(25);

/// The part of the measured upload the budget is offered at: the rest
/// is for everything else the home sends.
pub const SUGGESTED_PERCENT: u64 = 80;

/// Upload `bytes` to `url` `uploads` times, one after another, and each
/// one's rate in bit/s: from the request's start to its answer. The body
/// is not zeros, which a link could compress.
pub async fn measure_upload(
    client: &reqwest::Client,
    url: &str,
    bytes: usize,
    uploads: usize,
) -> Result<Vec<f64>, String> {
    let mut body = vec![0u8; bytes];
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    for chunk in body.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let b = x.to_le_bytes();
        chunk.copy_from_slice(&b[..chunk.len()]);
    }
    let mut rates = Vec::with_capacity(uploads);
    for _ in 0..uploads {
        let started = Instant::now();
        let answer = client
            .post(url)
            .timeout(MEASURE_TIMEOUT)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(body.clone())
            .send()
            .await
            .map_err(|e| format!("cannot upload to {url}: {e}"))?;
        if !answer.status().is_success() {
            return Err(format!("{url} answered {}", answer.status()));
        }
        let secs = started.elapsed().as_secs_f64().max(0.001);
        rates.push(bytes as f64 * 8.0 / secs);
    }
    Ok(rates)
}

/// The measured upload from each upload's rate (bit/s): their median, in
/// Mbit/s, and the budget [`SUGGESTED_PERCENT`] of it offers, at least 1.
pub fn summarize(rates: &[f64]) -> (f64, u64) {
    let mut sorted: Vec<f64> = rates.to_vec();
    sorted.sort_by(f64::total_cmp);
    let median = match sorted.len() {
        0 => 0.0,
        n if n % 2 == 1 => sorted[n / 2],
        n => (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0,
    };
    let mbps = median / 1_000_000.0;
    let suggested = ((mbps * SUGGESTED_PERCENT as f64 / 100.0) as u64).max(1);
    (mbps, suggested)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MBIT: u64 = 1_000_000;

    fn net(text: &str) -> Network {
        text.parse().unwrap()
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn the_home_64_is_the_lan_and_5g_is_not() {
        let subnets = [net("2603:8000:c33e:6ac6::/64"), net("192.168.86.10/24")];
        assert!(is_lan(ip("2603:8000:c33e:6ac6:5c3e::1"), &subnets, &[]));
        assert!(!is_lan(ip("2607:fb90:bd81:1d9::1"), &subnets, &[]));
        assert!(is_lan(ip("192.168.86.134"), &subnets, &[]));
        // Link-local and loopback always are.
        assert!(is_lan(ip("fe80::1"), &[], &[]));
        assert!(is_lan(ip("169.254.1.2"), &[], &[]));
        assert!(is_lan(ip("127.0.0.1"), &[], &[]));
        // A public IPv4 address off the host's subnets is the WAN.
        assert!(!is_lan(ip("76.50.95.40"), &subnets, &[]));
    }

    /// A private address off the host's subnets is not the LAN for being
    /// private: a phone on cellular over Tailscale or WireGuard is one,
    /// and its bytes leave through the uplink. Listed in
    /// `lan_prefixes`, a tunnel's network is home. Review 2026-10-11 M28.
    #[test]
    fn a_tunnel_s_private_address_is_not_the_lan_unless_listed() {
        let subnets = [net("192.168.86.10/24")];
        assert!(!is_lan(ip("100.101.102.103"), &subnets, &[]));
        assert!(!is_lan(ip("10.10.0.7"), &subnets, &[]));
        assert!(!is_lan(ip("fd7a:115c:a1e0::1"), &subnets, &[]));
        assert!(is_lan(ip("10.10.0.7"), &subnets, &[net("10.10.0.0/24")]));
        assert!(tunnel("tailscale0") && tunnel("wg0") && tunnel("tun0"));
        assert!(!tunnel("enp5s0") && !tunnel("wlan0") && !tunnel("br0"));
    }

    #[test]
    fn an_admin_s_extra_prefix_is_the_lan() {
        let extra = [net("203.0.113.0/24")];
        assert!(is_lan(ip("203.0.113.9"), &[], &extra));
        assert!(!is_lan(ip("203.0.114.9"), &[], &extra));
        // An IPv4-mapped IPv6 address is its IPv4 one.
        assert!(is_lan(ip("::ffff:203.0.113.9"), &[], &extra));
    }

    /// The user's example: budget 20; Samir alone gets 20; Diana joins,
    /// 10/10; the owner joins, owner 10, Samir and Diana 5 each.
    #[test]
    fn shares_follow_the_owner_s_half() {
        use Class::*;
        assert_eq!(shares(20 * MBIT, 50, &[Guest]), [20 * MBIT]);
        assert_eq!(
            shares(20 * MBIT, 50, &[Guest, Guest]),
            [10 * MBIT, 10 * MBIT]
        );
        assert_eq!(
            shares(20 * MBIT, 50, &[Guest, Guest, Owner]),
            [5 * MBIT, 5 * MBIT, 10 * MBIT]
        );
        // Owners alone split all of it.
        assert_eq!(
            shares(20 * MBIT, 50, &[Owner, Owner]),
            [10 * MBIT, 10 * MBIT]
        );
        // Another owner part.
        assert_eq!(
            shares(20 * MBIT, 75, &[Owner, Guest, Guest]),
            [15 * MBIT, 2_500_000, 2_500_000]
        );
    }

    fn uplink(budget_mbps: u64, max_streams: Option<usize>) -> Uplink {
        Uplink::new(UplinkSettings {
            budget: Some(budget_mbps * MBIT),
            max_streams,
            ..Default::default()
        })
    }

    #[test]
    fn a_new_stream_s_share_counts_itself() {
        let up = uplink(20, None);
        assert_eq!(up.admit(Class::Guest), Ok(Some(20 * MBIT)));
        up.started("samir", Class::Guest, 1, Some(20 * MBIT));
        assert_eq!(up.admit(Class::Guest), Ok(Some(10 * MBIT)));
        up.started("diana", Class::Guest, 2, Some(10 * MBIT));
        assert_eq!(up.admit(Class::Owner), Ok(Some(10 * MBIT)));
    }

    #[test]
    fn one_over_max_streams_is_full() {
        let up = uplink(20, Some(1));
        up.started("a", Class::Guest, 1, Some(20 * MBIT));
        assert_eq!(up.admit(Class::Guest), Err(Full { max: 1 }));
        up.ended("a");
        assert!(up.admit(Class::Guest).is_ok());
        // No budget still counts streams.
        let unlimited = Uplink::new(UplinkSettings {
            max_streams: Some(1),
            ..Default::default()
        });
        assert_eq!(unlimited.admit(Class::Guest), Ok(None));
        unlimited.started("a", Class::Guest, 1, None);
        assert_eq!(unlimited.admit(Class::Owner), Err(Full { max: 1 }));
    }

    #[test]
    fn a_start_tells_the_others_their_smaller_share() {
        let up = uplink(20, None);
        up.started("samir", Class::Guest, 1, Some(20 * MBIT));
        assert!(up.rebalance().is_empty());
        up.started("diana", Class::Guest, 2, Some(10 * MBIT));
        assert_eq!(
            up.rebalance(),
            [Told {
                handle: "samir".into(),
                connection: 1,
                max_bitrate: Some(10 * MBIT)
            }]
        );
        up.started("owner", Class::Owner, 3, Some(10 * MBIT));
        let mut told = up.rebalance();
        told.sort_by(|a, b| a.handle.cmp(&b.handle));
        assert_eq!(
            told.iter()
                .map(|t| (t.handle.as_str(), t.max_bitrate))
                .collect::<Vec<_>>(),
            [("diana", Some(5 * MBIT)), ("samir", Some(5 * MBIT))]
        );
        assert_eq!(up.share_of("owner"), Some(10 * MBIT));
        // The owner leaves: 10/10 again.
        up.ended("owner");
        let told = up.rebalance();
        assert_eq!(told.len(), 2);
        assert!(told.iter().all(|t| t.max_bitrate == Some(10 * MBIT)));
    }

    #[test]
    fn the_measured_upload_is_the_median_and_its_budget_80_percent() {
        let (mbps, suggested) = summarize(&[20e6, 5e6, 22e6]);
        assert!((mbps - 20.0).abs() < 1e-9);
        assert_eq!(suggested, 16);
        let (mbps, _) = summarize(&[10e6, 30e6]);
        assert!((mbps - 20.0).abs() < 1e-9);
        // A very slow upload still offers 1.
        assert_eq!(summarize(&[400e3]).1, 1);
    }

    /// The uploads reach the URL with their whole body, one after
    /// another, and each is timed.
    #[tokio::test]
    async fn the_upload_is_posted_and_timed() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let seen = received.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let seen = seen.clone();
                tokio::spawn(async move {
                    // Read one request's head and its body, by its length.
                    let mut data = Vec::new();
                    let mut buf = [0u8; 65536];
                    loop {
                        let n = socket.read(&mut buf).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        data.extend_from_slice(&buf[..n]);
                        let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") else {
                            continue;
                        };
                        let head = String::from_utf8_lossy(&data[..end]).to_lowercase();
                        let length: usize = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length: "))
                            .unwrap()
                            .trim()
                            .parse()
                            .unwrap();
                        if data.len() - end - 4 < length {
                            continue;
                        }
                        seen.lock().push(length);
                        data.drain(..end + 4 + length);
                        socket
                            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                            .await
                            .unwrap();
                    }
                });
            }
        });
        let client = reqwest::Client::new();
        let rates = measure_upload(&client, &format!("http://{addr}/__up"), 1 << 20, 2)
            .await
            .unwrap();
        assert_eq!(rates.len(), 2);
        assert!(rates.iter().all(|r| *r > 0.0));
        assert_eq!(*received.lock(), [1 << 20, 1 << 20]);
    }

    #[test]
    fn a_small_move_is_not_told() {
        assert!(!worth_telling(Some(10 * MBIT), Some(9 * MBIT)));
        assert!(worth_telling(Some(10 * MBIT), Some(8 * MBIT)));
        assert!(worth_telling(Some(10 * MBIT), None));
        assert!(worth_telling(None, Some(10 * MBIT)));
        assert!(!worth_telling(None, None));
    }
}
