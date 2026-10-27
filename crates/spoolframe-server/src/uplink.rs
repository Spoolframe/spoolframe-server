//! The WAN uplink budget (PX-46): which devices are on the LAN, how much
//! of the home's upload each playback off it may take, and telling the
//! running ones when that changes.
//!
//! A device is on the LAN when its address is in one of this host's own
//! interface subnets (the interface's real prefix: a global IPv6 address
//! in the home's /64 is the LAN, one on 5G is not), private, link-local,
//! carrier-grade NAT or unique local (a WireGuard or Tailscale peer
//! too), or in `network.lan_prefixes`; but not when it is in
//! `network.wan_prefixes`, where an admin lists a peer known to be
//! outside (the longer of two matching prefixes decides). A relay path
//! is never the LAN.
//!
//! The budget is split between the WAN playbacks open now: equally, but
//! while an owner (an admin of this server) plays off the LAN, the
//! owners' playbacks share `owner_share_percent` of it and everyone
//! else's the rest. A share only caps `Quality::Auto`; one the viewer
//! chose is theirs.
//!
//! A paired server's user's download read off the LAN (P8-07) is not a
//! playback: reads have a cap of their own ([`MAX_DOWNLOAD_READS`] at
//! once), and take neither a playback's place nor part of its share, so
//! downloads never make a play `WanStreamsFull`.

use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::admin_http::Network;

/// `network.uplink`, `network.lan_prefixes` and `network.wan_prefixes`,
/// checked.
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
    /// Ranges whose devices are off the LAN even at a private address or
    /// in `lan_prefixes` (a longer prefix there wins): a WireGuard peer
    /// that is always away.
    pub wan_prefixes: Vec<Network>,
}

/// A WAN start's place, held from its admission until it starts
/// ([`Admission::started`]) or fails (dropped).
#[must_use]
pub struct Admission {
    uplink: Arc<Uplink>,
    ticket: u64,
    class: Class,
    share: Option<u64>,
}

impl Admission {
    /// The share the playback is planned with.
    pub fn share(&self) -> Option<u64> {
        self.share
    }

    /// The playback `handle` opened on `connection`: its place is its own.
    pub fn started(self, handle: &str, connection: usize) {
        self.uplink
            .started(handle, self.class, connection, self.share);
        // Dropped here: the place is let go after the stream holds one.
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.uplink.pending.lock().remove(&self.ticket);
    }
}

/// How many download reads off the LAN are sent at once (P8-07), apart
/// from the playbacks: each is at most `MAX_READ`, and a device reads one
/// after another.
pub const MAX_DOWNLOAD_READS: usize = 4;

/// A download read's place off the LAN while it is sent (P8-07): dropped,
/// it is let go.
#[must_use]
#[derive(Debug)]
pub struct DownloadSlot {
    uplink: Arc<Uplink>,
}

impl Drop for DownloadSlot {
    fn drop(&mut self) {
        self.uplink
            .download_reads
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
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
            wan_prefixes: Vec::new(),
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

/// Whether `ip` is on the LAN. In `wan` (`network.wan_prefixes`) it is
/// not, unless a longer prefix of `lan` (`network.lan_prefixes`) also
/// holds it: the more specific range decides, the WAN on a tie, so a
/// `/32` of one WireGuard peer known to be away is taken out of a `/24`
/// counted as home. Otherwise it is when it is in `lan`, in one of
/// `subnets` (this host's), or not public: private, carrier-grade NAT,
/// link-local, unique local or loopback, so a WireGuard or Tailscale peer
/// is home unless listed in `wan`.
pub fn is_lan(ip: IpAddr, subnets: &[Network], lan: &[Network], wan: &[Network]) -> bool {
    let ip = ip.to_canonical();
    let longest = |ranges: &[Network]| {
        ranges
            .iter()
            .filter(|n| n.contains(ip))
            .map(Network::prefix)
            .max()
    };
    match (longest(wan), longest(lan)) {
        (Some(wan), Some(lan)) => lan > wan,
        (Some(_), None) => false,
        (None, Some(_)) => true,
        (None, None) => {
            crate::wan::not_public(ip).is_some() || subnets.iter().any(|n| n.contains(ip))
        }
    }
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
    /// WAN starts admitted and not yet started ([`Uplink::reserve`]): each
    /// one's class, by its ticket. They count against `max_streams` and in
    /// the shares, so two starts at once cannot both take the last place.
    pending: Mutex<HashMap<u64, Class>>,
    next_ticket: std::sync::atomic::AtomicU64,
    /// Download reads off the LAN being sent ([`DownloadSlot`]).
    download_reads: std::sync::atomic::AtomicUsize,
    /// Woken at each start and end, for the shares to be worked out again
    /// once things settle. Shared, so [`tell_shares`] waits on it without
    /// holding the uplink.
    changed: Arc<tokio::sync::Notify>,
}

impl Drop for Uplink {
    /// Wake `tell_shares`, which then finds the uplink gone and ends.
    fn drop(&mut self) {
        self.changed.notify_waiters();
    }
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
            pending: Mutex::new(HashMap::new()),
            next_ticket: std::sync::atomic::AtomicU64::new(0),
            download_reads: std::sync::atomic::AtomicUsize::new(0),
            changed: Arc::new(tokio::sync::Notify::new()),
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
    /// and `network.lan_prefixes` / `network.wan_prefixes`.
    pub fn is_lan(&self, ip: IpAddr) -> bool {
        let subnets = self.subnets();
        let settings = self.settings.read();
        is_lan(ip, &subnets, &settings.lan_prefixes, &settings.wan_prefixes)
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
        let streams = self.streams.lock();
        let pending = self.pending.lock();
        self.admit_locked(class, &streams, &pending)
    }

    fn admit_locked(
        &self,
        class: Class,
        streams: &HashMap<String, Stream>,
        pending: &HashMap<u64, Class>,
    ) -> Result<Option<u64>, Full> {
        let settings = self.settings.read().clone();
        if let Some(max) = settings.max_streams
            && streams.len() + pending.len() >= max
        {
            return Err(Full { max });
        }
        let Some(budget) = settings.budget else {
            return Ok(None);
        };
        let mut classes: Vec<Class> = streams.values().map(|s| s.class).collect();
        classes.extend(pending.values().copied());
        classes.push(class);
        Ok(shares(budget, settings.owner_share_percent, &classes)
            .last()
            .copied())
    }

    /// [`Uplink::admit`], holding the place until the playback is
    /// [`Admission::started`] or the admission is dropped (a start that
    /// failed on the way): two WAN starts that arrive together at
    /// `max_streams - 1` do not both pass.
    pub fn reserve(self: &Arc<Self>, class: Class) -> Result<Admission, Full> {
        let streams = self.streams.lock();
        let mut pending = self.pending.lock();
        let share = self.admit_locked(class, &streams, &pending)?;
        let ticket = self
            .next_ticket
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        pending.insert(ticket, class);
        Ok(Admission {
            uplink: Arc::clone(self),
            ticket,
            class,
            share,
        })
    }

    /// A place for a download read off the LAN (P8-07), apart from the
    /// playbacks'; `Full` with [`MAX_DOWNLOAD_READS`] being sent.
    pub fn reserve_download(self: &Arc<Self>) -> Result<DownloadSlot, Full> {
        use std::sync::atomic::Ordering;
        self.download_reads
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < MAX_DOWNLOAD_READS).then_some(n + 1)
            })
            .map_err(|_| Full {
                max: MAX_DOWNLOAD_READS,
            })?;
        Ok(DownloadSlot {
            uplink: Arc::clone(self),
        })
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
}

/// How long starts and ends are let settle before the shares are told:
/// a device's next track, or a film started again, should not re-plan
/// everyone twice.
pub const SETTLE: Duration = Duration::from_secs(3);

/// This host's interface subnets now: each address with its netmask's
/// prefix. Loopback ones are left out (`is_lan` takes them anyway).
pub fn host_subnets() -> Vec<Network> {
    crate::ifaddrs::up()
        .into_iter()
        .filter_map(|addr| {
            let prefix = addr.prefix?;
            (!addr.ip.is_loopback() && prefix != 0).then(|| Network::new(addr.ip, prefix.into()))
        })
        .collect()
}

/// Tell each of `told` its new share on its connection's events stream,
/// until `uplink` is dropped.
pub(crate) async fn tell_shares(
    uplink: std::sync::Weak<Uplink>,
    streams: crate::events::EventStreams,
) {
    loop {
        // The wait holds the notify, not the uplink: holding it across an
        // indefinite wait kept it alive, and this task with it, after its
        // server was gone (review 2026-10-11 L45).
        let Some(changed) = uplink.upgrade().map(|u| u.changed.clone()) else {
            return;
        };
        changed.notified().await;
        loop {
            tokio::select! {
                () = changed.notified() => {}
                () = tokio::time::sleep(SETTLE) => break,
            }
        }
        let Some(this) = uplink.upgrade() else {
            return;
        };
        let told = this.rebalance();
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

/// The most each upload sends: long enough that the round trip is a
/// small part of its time on a fast upload.
pub const MEASURE_BYTES: usize = 10 << 20;

/// The first upload, which sizes the rest: small enough to finish within
/// [`MEASURE_TIMEOUT`] on an upload of a few hundred kbit/s.
pub const PROBE_BYTES: usize = 1 << 20;

/// How long each measured upload is sized to take at the probe's rate:
/// well inside [`MEASURE_TIMEOUT`] should the upload slow down.
pub const MEASURE_SECS: f64 = 8.0;

/// How many uploads are made; their median is the answer.
pub const MEASURE_UPLOADS: usize = 3;

/// The longest one upload may take before the measurement gives up.
pub const MEASURE_TIMEOUT: Duration = Duration::from_secs(25);

/// The part of the measured upload the budget is offered at: the rest
/// is for everything else the home sends.
pub const SUGGESTED_PERCENT: u64 = 80;

/// What each measured upload sends after a probe at `probe_rate` bit/s:
/// [`MEASURE_SECS`] of it, between [`PROBE_BYTES`] and `max`. A fixed
/// 10 MiB took longer than [`MEASURE_TIMEOUT`] under about 3.4 Mbit/s,
/// the uploads a budget matters most for, and the measurement failed.
pub fn upload_size(probe_rate: f64, max: usize) -> usize {
    let sized = (probe_rate / 8.0 * MEASURE_SECS) as usize;
    sized.clamp(PROBE_BYTES.min(max), max)
}

/// Upload to `url` once to size the rest ([`upload_size`], at most
/// `bytes`), then `uploads` times one after another, and each of those
/// uploads' rate in bit/s: from the request's start to its answer. The
/// body is not zeros, which a link could compress, and is made once.
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
    let body = bytes::Bytes::from(body);
    let upload = |len: usize| {
        let body = body.slice(..len);
        async move {
            let started = Instant::now();
            let answer = client
                .post(url)
                .timeout(MEASURE_TIMEOUT)
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .body(body)
                .send()
                .await
                .map_err(|e| format!("cannot upload to {url}: {e}"))?;
            if !answer.status().is_success() {
                return Err(format!("{url} answered {}", answer.status()));
            }
            let secs = started.elapsed().as_secs_f64().max(0.001);
            Ok(len as f64 * 8.0 / secs)
        }
    };
    let probe = upload(PROBE_BYTES.min(bytes)).await?;
    let size = upload_size(probe, bytes);
    let mut rates = Vec::with_capacity(uploads);
    for _ in 0..uploads {
        rates.push(upload(size).await?);
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

    /// The task that tells shares ends once its uplink is dropped, and
    /// does not keep it alive (review 2026-10-11 L45).
    #[tokio::test]
    async fn telling_shares_ends_with_the_uplink() {
        let uplink = Arc::new(Uplink::default());
        let task = tokio::spawn(tell_shares(
            Arc::downgrade(&uplink),
            crate::events::EventStreams::default(),
        ));
        tokio::task::yield_now().await;
        let weak = Arc::downgrade(&uplink);
        drop(uplink);
        assert!(weak.upgrade().is_none(), "the task held the uplink");
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the task ended")
            .unwrap();
    }
    use super::*;

    /// Download reads (P8-07) have a cap of their own, and neither take a
    /// playback's place nor part of the shares.
    #[test]
    fn download_reads_are_apart_from_the_playbacks() {
        let uplink = Arc::new(Uplink::new(UplinkSettings {
            budget: Some(8_000_000),
            max_streams: Some(1),
            ..UplinkSettings::default()
        }));
        let slots: Vec<DownloadSlot> = (0..MAX_DOWNLOAD_READS)
            .map(|_| uplink.reserve_download().unwrap())
            .collect();
        assert_eq!(
            uplink.reserve_download().unwrap_err(),
            Full {
                max: MAX_DOWNLOAD_READS
            }
        );
        assert_eq!(
            uplink.admit(Class::Guest),
            Ok(Some(8_000_000)),
            "the whole budget"
        );
        uplink.started("play", Class::Guest, 1, Some(8_000_000));
        assert_eq!(uplink.wan_streams(), 1);
        drop(slots);
        assert!(uplink.reserve_download().is_ok());
    }

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
        assert!(is_lan(
            ip("2603:8000:c33e:6ac6:5c3e::1"),
            &subnets,
            &[],
            &[]
        ));
        assert!(!is_lan(ip("2607:fb90:bd81:1d9::1"), &subnets, &[], &[]));
        assert!(is_lan(ip("192.168.86.134"), &subnets, &[], &[]));
        // Link-local and loopback always are.
        assert!(is_lan(ip("fe80::1"), &[], &[], &[]));
        assert!(is_lan(ip("169.254.1.2"), &[], &[], &[]));
        assert!(is_lan(ip("127.0.0.1"), &[], &[], &[]));
        // A public IPv4 address off the host's subnets is the WAN.
        assert!(!is_lan(ip("76.50.95.40"), &subnets, &[], &[]));
    }

    /// A private address is the LAN, a WireGuard or Tailscale peer's
    /// too, unless `wan_prefixes` takes it out; between a `wan` and a
    /// `lan` range that both hold an address the longer prefix decides,
    /// the WAN on a tie.
    #[test]
    fn a_tunnel_peer_is_the_lan_unless_listed_as_outside() {
        let subnets = [net("192.168.86.10/24")];
        assert!(is_lan(ip("10.10.0.7"), &subnets, &[], &[]));
        assert!(is_lan(ip("100.101.102.103"), &subnets, &[], &[]));
        assert!(is_lan(ip("fd7a:115c:a1e0::1"), &subnets, &[], &[]));
        let away = [net("10.10.0.7/32")];
        assert!(!is_lan(ip("10.10.0.7"), &subnets, &[], &away));
        assert!(is_lan(ip("10.10.0.8"), &subnets, &[], &away));
        // A /32 outside within a /24 counted as home is outside.
        let home = [net("10.10.0.0/24")];
        assert!(!is_lan(ip("10.10.0.7"), &subnets, &home, &away));
        assert!(is_lan(ip("10.10.0.8"), &subnets, &home, &away));
        // A /32 at home within a /16 outside is home.
        let wide = [net("10.10.0.0/16")];
        let one = [net("10.10.3.4/32")];
        assert!(is_lan(ip("10.10.3.4"), &subnets, &one, &wide));
        assert!(!is_lan(ip("10.10.3.5"), &subnets, &one, &wide));
        // The same range on both lists is outside.
        assert!(!is_lan(ip("10.10.0.9"), &subnets, &home, &home));
        // A host subnet listed as outside is outside.
        assert!(!is_lan(
            ip("192.168.86.50"),
            &subnets,
            &[],
            &[net("192.168.86.50/32")]
        ));
        // A public 5G address is the WAN.
        assert!(!is_lan(ip("2607:fb90:bd81:1d9::1"), &subnets, &[], &[]));
        assert!(!is_lan(ip("172.58.1.2"), &subnets, &[], &[]));
    }

    #[test]
    fn an_admin_s_extra_prefix_is_the_lan() {
        let extra = [net("203.0.113.0/24")];
        assert!(is_lan(ip("203.0.113.9"), &[], &extra, &[]));
        assert!(!is_lan(ip("203.0.114.9"), &[], &extra, &[]));
        // An IPv4-mapped IPv6 address is its IPv4 one.
        assert!(is_lan(ip("::ffff:203.0.113.9"), &[], &extra, &[]));
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

    /// Two WAN starts admitted together at max - 1 do not both pass: the
    /// first holds its place until it starts or fails.
    #[test]
    fn a_reserved_start_holds_its_place_until_it_starts_or_fails() {
        let up = Arc::new(uplink(20, Some(2)));
        up.started("a", Class::Guest, 1, Some(20 * MBIT));
        let first = up.reserve(Class::Guest).unwrap();
        assert_eq!(first.share(), Some(10 * MBIT));
        assert!(matches!(up.reserve(Class::Guest), Err(Full { max: 2 })));
        drop(first);
        let again = up.reserve(Class::Guest).unwrap();
        again.started("b", 2);
        assert_eq!(up.wan_streams(), 2);
        assert!(matches!(up.reserve(Class::Guest), Err(Full { max: 2 })));
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
        // The probe, then the two measured.
        assert_eq!(*received.lock(), [1 << 20, 1 << 20, 1 << 20]);
    }

    /// A slow upload is measured with uploads it can finish in time
    /// (review 2026-10-11 M30): 10 MiB at 2 Mbit/s took 42 s, past the
    /// 25 s each may take.
    #[test]
    fn a_slow_upload_is_sized_to_finish_in_time() {
        for rate in [0.5e6, 2e6, 3e6] {
            let size = upload_size(rate, MEASURE_BYTES);
            let secs = size as f64 * 8.0 / rate;
            assert!(secs < MEASURE_TIMEOUT.as_secs_f64(), "{rate}: {secs} s");
        }
        assert_eq!(upload_size(500e6, MEASURE_BYTES), MEASURE_BYTES);
        assert_eq!(upload_size(10.0, MEASURE_BYTES), PROBE_BYTES);
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
