//! `spoolframe/client/1` on one connection (ARCHITECTURE §20, §33).
//!
//! Each bidirectional stream the client opens is one RPC. The first
//! request must be `Hello`; after it the server opens the events stream.
//! Only the pre-auth requests are served without a session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use iroh::endpoint::{Connection, RecvStream, SendStream, VarInt};
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{Endpoint, EndpointId};
use spoolframe_catalog::{CatalogError, Page};
use spoolframe_media::{MediaDescriptor, SubtitleSource, SubtitleStream, codecs};
use spoolframe_protocol::close;
use spoolframe_protocol::codec::{self, write_frame};
use spoolframe_protocol::device_sign_in::{
    normalize as device_sign_in_code, web_link as device_sign_in_web_link,
};
use spoolframe_protocol::features;
use spoolframe_protocol::invite_admin::{InviteCreated, InviteCreator, InviteSummary};
use spoolframe_protocol::resource::{ResourceMetadata, ResourceRequest, ResourceResponse};
use spoolframe_protocol::rpc::{
    PlaybackRevision, RpcReply, RpcRequest, RpcResponse, ServerInstance,
};
use spoolframe_protocol::streams::{bi, reset, uni};
use spoolframe_protocol::{
    ArtworkRequest, ArtworkSize, BitmapSubtitles, ChosenTracks, ConvertedSubtitle, MAX_READ,
    MediaRead, MediaReadReply, PlaybackHandle, PlaybackPlan, SUBTITLE_CHUNK, Selection,
    SubtitleAction, TopResult,
};
use spoolframe_protocol::{
    AuthMethod, DeviceInfo, DeviceSignInRequest, Error, Event, FinishPasskeySignIn, Hello,
    LoginPassword, PasskeyInfo, PinProof, PinState, PlexLink, PlexSyncSummary, PortableSignIn,
    RedeemInvite, RedeemPortableInvite, Reply, Request, Response, ServerInfo, SessionId,
    SessionInfo, TrackPreferencesInfo, TrustedProfile, UserSummary, WhoAmIReply, is_app_rp_id,
};
use spoolframe_types::{ArtworkId, MediaId, Namespace, SubtitleFormat};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::watch;

use crate::admission::{Peer, SIGN_OUT_DRAIN};
use crate::announce::Told;
use crate::artwork::{Artwork, ArtworkError};
use crate::auth::{self, Approval, LoginError, RECENT_SIGN_IN, SessionToken, UnlockError};
use crate::device_sign_in::{self, ClaimError, Outcome, Waiting};
use crate::encode::job::{EncodeJob, JobError, Progress};
use crate::encode::{Conversion, ConversionSource, Init, hls};
use crate::events::{Addresses, EventStreams};
use crate::network::{Connections, ServerName, Services};
use crate::passkeys::{CeremonyError, SignInError};
use crate::planner;
use crate::playback::{
    Changed, CopyUse, FileId, FirstSegment, KeyframesFrom, Opened, Playback, Playbacks, Refusal,
    Sidecar, Source, StartTimes, Using,
};
use crate::plex_accounts::Link;
use crate::previews::{self, PreviewState};
use crate::principal::Principal;
use crate::principal::Who;
use crate::refusals::{self, Channel, Kind};
use crate::store::passkeys::{PasskeyError, PasskeyRecord};
use crate::store::pins::PinError;
use crate::store::sessions::{SessionBinding, SessionUser, SignedInWith};
use crate::store::trust::TrustError;
use crate::subtitles::{TextSource, TextState};
use crate::tracks::{self, TrackOverride};
use crate::watch::WatchError;
use spoolframe_admin_api::PlexSyncRun;
use spoolframe_protocol::ConvertedVideo;
use spoolframe_types::Delivery;

/// How long shutdown waits to deliver `ShuttingDown` to one client.
const SHUTDOWN_NOTICE: Duration = Duration::from_millis(500);

/// The client protocol handler shared by every connection.
#[derive(Debug, Clone)]
pub(crate) struct ClientProtocol {
    /// This server's endpoint id, which app passkey challenges are made
    /// with (PX-10).
    id: EndpointId,
    connections: Connections,
    services: Services,
    info: Arc<ServerInfo>,
    /// What `info` calls the server, as it is now (P3-10).
    name: ServerName,
    /// The events stream of each connection that said Hello, and what
    /// pushes the server's addresses on them (PX-12).
    addresses: Addresses,
    instance: String,
    operations: crate::operations::Pools,
    /// Tie challenges begun and not yet answered (P7-04).
    ties: Arc<crate::ties::TieChallenges>,
    /// Portable sign-ins begun and not yet finished (P7-06).
    portable_sign_ins: Arc<crate::portable::SignInNonces>,
    /// Pairing requests held for root terminals with no camera (P7-08).
    pairing_relay: Arc<crate::pairing_relay::PairingRelay>,
    /// The HTTP playback port (PX-49), when it is on: what turns the
    /// `http-playback` experimental feature on.
    http_playback: Option<Arc<crate::http_playback::HttpPlayback>>,
    /// Who each connection that takes playlist events acts as (PX-40).
    playlist_listeners: crate::events::PlaylistListeners,
    /// The items each connection that takes subtitle timing events asked
    /// about (PX-03).
    subtitle_listeners: crate::events::SubtitleListeners,
}

/// What one connection has said so far.
#[derive(Debug)]
struct ConnectionState {
    /// Its number, never another's: what its playbacks name it by, so
    /// they are known as a killed app's once it closes or a newer one
    /// opens. Not the connection's `stable_id`, which a later connection
    /// may get again.
    number: u64,
    /// Its limits, and its streams being answered (PX-10).
    peer: Arc<Peer>,
    hello: OnceLock<Hello>,
    /// Profile locks (P2-08).
    locks: parking_lot::Mutex<Locks>,
    /// The paths its packets went by, as the log names them (`lan`,
    /// `direct` or `relay`, and where), the one now last: a relay, then a
    /// direct path, is a hole punched. Empty before iroh selects one.
    paths: parking_lot::Mutex<Vec<String>>,
    /// Changes of its path since it opened, every one: the link lines
    /// count them per interval, where the log of each is coalesced.
    switches: std::sync::atomic::AtomicU64,
    /// Which path changes are logged (a flapping path once in a while).
    path_log: parking_lot::Mutex<crate::link_stats::PathLog>,
    /// What tells the LAN from the WAN (PX-46).
    uplink: Arc<crate::uplink::Uplink>,
    /// Its UDP bytes sent and received counted into the metrics so far.
    counted: parking_lot::Mutex<(u64, u64)>,
    /// Whether it punched a hole, as its paths say (P3-14).
    punch: parking_lot::Mutex<crate::link_stats::HolePunch>,
    metrics: Arc<crate::metrics::Metrics>,
}

impl ConnectionState {
    /// The path now, for the log; `none` before one is selected.
    fn path(&self) -> String {
        self.paths
            .lock()
            .last()
            .cloned()
            .unwrap_or_else(|| "none".into())
    }

    /// Its packets go by `path` now (named by [`path_name`]): an attempt to
    /// punch a hole, or its success, counted (P3-14).
    fn path_is(&self, path: &str) {
        let punch = self
            .punch
            .lock()
            .path(crate::link_stats::PathKind::of(path));
        if let Some(punch) = punch {
            self.metrics.hole_punch(punch);
        }
    }

    /// Count what `connection` carried since the last count into the
    /// metrics, as the path it goes by now; and, with `rtt`, that path's
    /// round-trip time. Its bytes are the whole connection's, every path's,
    /// so none is lost as paths close.
    fn count_link(&self, connection: &iroh::endpoint::Connection, rtt: bool) {
        let stats = connection.stats();
        let (sent, received) = (stats.udp_tx.bytes, stats.udp_rx.bytes);
        let kind = crate::link_stats::PathKind::of(&self.path());
        {
            let mut counted = self.counted.lock();
            self.metrics.link_bytes(
                kind,
                sent.saturating_sub(counted.0),
                received.saturating_sub(counted.1),
            );
            *counted = (sent.max(counted.0), received.max(counted.1));
        }
        if rtt && let Some(path) = connection.paths().iter().find(|p| p.is_selected()) {
            let kind =
                crate::link_stats::PathKind::of(&path_name(path.remote_addr(), &self.uplink));
            self.metrics.link_rtt(kind, path.rtt());
        }
    }

    /// Every path it went by, oldest first, for the log.
    fn paths(&self) -> String {
        let paths = self.paths.lock();
        if paths.is_empty() {
            "none".into()
        } else {
            paths.join(" -> ")
        }
    }
}

/// The most paths a connection's log line names: the first few, which
/// tell how it came to be direct, and the rest left out.
const LOGGED_PATHS: usize = 8;

/// `addr`, the path a connection goes by, as the log names it: `relay`
/// and its URL; or `direct` and the address, `lan` for one on the LAN
/// (PX-46: in one of this host's subnets, a global IPv6 address in the
/// home's /64 too, private, or in `network.lan_prefixes`, and not in
/// `network.wan_prefixes`). Whether a
/// direct path is a hole punched in NAT or an open port, iroh does not
/// say.
fn path_name(addr: &iroh::TransportAddr, uplink: &crate::uplink::Uplink) -> String {
    match addr {
        iroh::TransportAddr::Relay(url) => format!("relay {url}"),
        iroh::TransportAddr::Ip(ip) if uplink.is_lan(ip.ip()) => format!("lan {ip}"),
        iroh::TransportAddr::Ip(ip) => format!("direct {ip}"),
        other => format!("{other:?}"),
    }
}

/// Whether a path named by [`path_name`] is on the LAN (PX-46); before
/// one is selected, `none`, it is not.
fn on_lan(path: &str) -> bool {
    path.starts_with("lan ")
}

/// Follow the path `connection`'s packets go by, into `state`, and log
/// each change of it: at INFO once it has signed in (a move from a relay
/// to a direct path is a hole punched), else at DEBUG, as most
/// connections are an app checking in. The disconnect line names them
/// all either way.
async fn log_paths(
    connection: iroh::endpoint::WeakConnectionHandle,
    mut events: iroh::endpoint::PathEventStream,
    state: std::sync::Weak<ConnectionState>,
) {
    use futures_util::StreamExt as _;
    use iroh::endpoint::PathEvent;
    let closed = connection.closed();
    tokio::pin!(closed);
    loop {
        let event = tokio::select! {
            event = events.next() => event,
            _ = &mut closed => return,
        };
        let remote_addr = match event {
            None => return,
            Some(PathEvent::Selected { remote_addr, .. }) => remote_addr,
            Some(_) => continue,
        };
        let Some(state) = state.upgrade() else {
            return;
        };
        let selected = path_name(&remote_addr, &state.uplink);
        let before = {
            let mut paths = state.paths.lock();
            let before = paths.last().cloned();
            if before.as_deref() == Some(selected.as_str()) {
                continue;
            }
            if paths.len() < LOGGED_PATHS {
                paths.push(selected.clone());
            } else if let Some(last) = paths.last_mut() {
                *last = selected.clone();
            }
            before.unwrap_or_else(|| "none".into())
        };
        state.path_is(&selected);
        state
            .switches
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // A phone on a cellular network can flip between two of its
        // addresses several times a second: the first change is logged,
        // then one line a [`crate::link_stats::PATH_LOG_EVERY`] counting
        // the rest.
        let Some(line) = state
            .path_log
            .lock()
            .change(&selected, std::time::Instant::now())
        else {
            continue;
        };
        let endpoint = state.peer.endpoint.fmt_short();
        let held = line.held;
        let between = line.between.join(" , ");
        if state.peer.was_signed_in() {
            tracing::info!(%endpoint, path = %selected, %before, held, %between, "connection path");
        } else {
            tracing::debug!(%endpoint, path = %selected, %before, held, %between, "connection path");
        }
    }
}

/// Read `connection`'s counters and its selected path now, for
/// [`watch_link`].
fn link_reading(
    connection: &iroh::endpoint::Connection,
    state: &ConnectionState,
) -> (crate::link_stats::Counters, crate::link_stats::PathNow) {
    let stats = connection.stats();
    let paths = connection.paths();
    let mut path = crate::link_stats::PathNow {
        open: paths.len(),
        ..Default::default()
    };
    let mut congestion_events = 0;
    for p in paths.iter() {
        let s = p.stats();
        congestion_events += s.congestion_events;
        if p.is_selected() {
            path.name = path_name(p.remote_addr(), &state.uplink);
            path.kind = crate::link_stats::PathKind::of(&path.name);
            path.rtt = s.rtt;
            path.cwnd = s.cwnd;
            path.mtu = s.current_mtu;
        }
    }
    let counters = crate::link_stats::Counters {
        sent_bytes: stats.udp_tx.bytes,
        sent_datagrams: stats.udp_tx.datagrams,
        received_bytes: stats.udp_rx.bytes,
        lost_packets: stats.lost_packets,
        lost_bytes: stats.lost_bytes,
        congestion_events,
        path_switches: state.switches.load(std::sync::atomic::Ordering::Relaxed),
    };
    (counters, path)
}

/// Every [`crate::link_stats::EVERY`], count what `connection` carried into
/// the metrics; and while its device has a playback open, log it and add
/// it to those playbacks' link totals: for telling a link too slow for the
/// film from a relay or from a path that will not hold still.
async fn watch_link(
    connection: iroh::endpoint::WeakConnectionHandle,
    endpoint_hex: String,
    state: std::sync::Weak<ConnectionState>,
    playbacks: Arc<crate::playback::Playbacks>,
) {
    // Weak, as for the path log: this task must not keep the connection
    // open.
    let closed = connection.closed();
    tokio::pin!(closed);
    let mut ticks = tokio::time::interval(crate::link_stats::EVERY);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The reading an interval starts from, and when; none while no
    // playback is open, so an idle stretch is not counted.
    let mut from: Option<(crate::link_stats::Counters, std::time::Instant)> = None;
    loop {
        tokio::select! {
            _ = ticks.tick() => {}
            _ = &mut closed => return,
        }
        let Some(state) = state.upgrade() else {
            return;
        };
        let Some(live) = connection.upgrade() else {
            return;
        };
        state.count_link(&live, true);
        if !playbacks.live_on(&endpoint_hex) {
            from = None;
            continue;
        }
        let now = std::time::Instant::now();
        let (counters, path) = link_reading(&live, &state);
        drop(live);
        let Some((before, at)) = from.replace((counters, now)) else {
            continue;
        };
        let interval = crate::link_stats::Interval::between(
            &before,
            &counters,
            now.saturating_duration_since(at).as_secs_f64(),
        );
        playbacks.record_link(&endpoint_hex, &interval, &path);
        tracing::info!(
            target: "spoolframe_server::link",
            endpoint = %state.peer.endpoint.fmt_short(),
            path = %path.name,
            rtt_ms = path.rtt.as_millis() as u64,
            cwnd_kb = path.cwnd / 1024,
            mtu = path.mtu,
            open_paths = path.open,
            send_mbps = %format_args!("{:.2}", interval.send_mbps()),
            recv_mbps = %format_args!("{:.2}", interval.receive_mbps()),
            lost_pct = %format_args!("{:.2}", interval.loss_percent()),
            lost_packets = interval.lost_packets,
            congestion = interval.congestion_events,
            switches = interval.path_switches,
            "link"
        );
    }
}

/// Which sessions on one connection need their user's PIN, and which are
/// unlocked (P2-08). Kept per connection: a new connection starts locked,
/// whatever an earlier one unlocked.
#[derive(Debug, Default)]
struct Locks {
    /// Each session looked up on this connection: whether its device asks
    /// for its user's PIN.
    needs_pin: HashMap<String, bool>,
    unlocked: std::collections::HashSet<String>,
    /// The explicitly selected foreground profile. Background requests
    /// never change it or invalidate its PIN unlock.
    active: Option<String>,
    generation: u64,
}

enum ResourceBody {
    Bytes(Vec<u8>),
    File(tokio::fs::File),
}

impl Locks {
    fn knows(&self, session: &SessionId) -> bool {
        self.needs_pin.contains_key(&session.0)
    }

    fn learn(&mut self, session: &SessionId, needs_pin: bool) {
        self.needs_pin.insert(session.0.clone(), needs_pin);
    }

    fn locked(&self, session: &SessionId) -> bool {
        self.needs_pin.get(&session.0) == Some(&true) && !self.unlocked.contains(&session.0)
    }

    /// Background and foreground requests obey the same PIN check.
    fn use_session(&mut self, session: &SessionId) -> Result<(), Error> {
        if self.locked(session) {
            return Err(Error::ProfileLocked);
        }
        Ok(())
    }

    fn activate(&mut self, session: &SessionId) {
        if self.active.as_deref() != Some(session.0.as_str()) {
            self.generation += 1;
            self.unlocked.retain(|unlocked| unlocked == &session.0);
            self.active = Some(session.0.clone());
        }
    }

    fn unlock(&mut self, session: &SessionId) {
        self.unlocked.insert(session.0.clone());
    }
}

impl ClientProtocol {
    pub(crate) fn new(
        endpoint: Endpoint,
        told: Told,
        connections: Connections,
        info: ServerInfo,
        name: ServerName,
        services: Services,
        http_playback: Option<Arc<crate::http_playback::HttpPlayback>>,
    ) -> Self {
        let addresses = Addresses {
            streams: EventStreams::default(),
            connections: connections.clone(),
            told,
        };
        tokio::spawn(addresses.clone().tell_changes());
        // PX-46: a WAN playback that ends gives its share back, and the
        // others are told theirs once starts and ends settle.
        let uplink = Arc::downgrade(&services.uplink);
        services.playbacks.on_end(Box::new(move |handle| {
            if let Some(uplink) = uplink.upgrade() {
                uplink.ended(handle);
            }
        }));
        // PX-52: every playback's play is kept, from its start to its end,
        // however it ends; the HTTP playback port's included.
        services
            .playbacks
            .observe(Arc::new(crate::watch::Plays::new(services.watch.clone())));
        tokio::spawn(crate::uplink::tell_shares(
            Arc::downgrade(&services.uplink),
            addresses.streams.clone(),
        ));
        let ties = Arc::new(crate::ties::TieChallenges::new(
            services.auth.store().clock().clone(),
        ));
        let portable_sign_ins = Arc::new(crate::portable::SignInNonces::new(
            services.auth.store().clock().clone(),
        ));
        let pairing_relay = Arc::new(crate::pairing_relay::PairingRelay::new(
            services.auth.store().clock().clone(),
        ));
        let playlist_listeners = crate::events::PlaylistListeners::default();
        tokio::spawn(crate::events::tell_playlist_news(
            services.playlists.news(),
            playlist_listeners.clone(),
            addresses.streams.clone(),
        ));
        let subtitle_listeners = crate::events::SubtitleListeners::default();
        tokio::spawn(crate::events::tell_subtitle_news(
            services.subtitle_timings.changes(),
            subtitle_listeners.clone(),
            addresses.streams.clone(),
        ));
        Self {
            id: endpoint.id(),
            connections,
            services,
            info: Arc::new(info),
            name,
            addresses,
            instance: uuid::Uuid::now_v7().to_string(),
            operations: crate::operations::Pools::default(),
            ties,
            portable_sign_ins,
            pairing_relay,
            http_playback,
            playlist_listeners,
            subtitle_listeners,
        }
    }

    /// The experimental features this server has on (PX-49).
    fn experimental_on(&self) -> Vec<&'static str> {
        crate::protocol_info::experimental_on(self.http_playback.is_some())
    }

    /// One stream: its kind byte, then (for RPC) one request and its
    /// response.
    async fn stream(
        &self,
        connection: Connection,
        state: Arc<ConnectionState>,
        mut send: SendStream,
        mut recv: RecvStream,
    ) {
        let _answering = state.peer.answering();
        // A stream holds one of the peer's few stream credits, and its
        // frame's memory, until its first frame is in (PX-10).
        let first = tokio::time::timeout(self.connections.admission().first_frame, async {
            let kind = recv.read_u8().await?;
            let frame = if kind == bi::RPC || kind == bi::RESOURCE {
                Some(codec::read_frame_bytes_within(&mut recv, state.peer.max_frame()).await)
            } else {
                None
            };
            Ok::<_, std::io::Error>((kind, frame))
        })
        .await;
        let (kind, frame) = match first {
            Ok(Ok(first)) => first,
            // The stream ended before its kind byte.
            Ok(Err(_)) => return,
            Err(_) => {
                tracing::debug!(
                    endpoint = %connection.remote_id().fmt_short(),
                    "a stream's first frame did not arrive in time"
                );
                let code = VarInt::from_u32(reset::FIRST_FRAME_TIMEOUT);
                let _ = send.reset(code);
                let _ = recv.stop(code);
                return;
            }
        };
        let Some(request) = frame else {
            // A kind from a newer client. The stream is refused; the
            // connection carries on.
            tracing::debug!(kind, "refusing a stream of unknown kind");
            let code = VarInt::from_u32(reset::UNKNOWN_STREAM_KIND);
            let _ = send.reset(code);
            let _ = recv.stop(code);
            return;
        };
        if kind == bi::RESOURCE {
            self.resource(&connection, &state, send, recv, request)
                .await;
            return;
        }
        // Named once, from its `type` alone, and the name is used
        // wherever the request is named (finding 50). The client chose
        // it: at most 64 characters, on one line (C-12).
        let name = match &request {
            Ok(bytes) => refusals::bounded(&codec::rpc_request_type(bytes).unwrap_or_default()),
            Err(_) => String::new(),
        };
        let response = match &request {
            Ok(bytes) => {
                // One span per request, named by its type, with how it went,
                // the path its connection takes now and what kind of app
                // asked: never its session, its user or its arguments
                // (P3-14).
                use tracing::Instrument as _;
                let span = tracing::info_span!(
                    "client request",
                    request = name.as_str(),
                    outcome = tracing::field::Empty,
                    connection.path = crate::link_stats::PathKind::of(&state.path()).as_str(),
                    client.platform = tracing::field::Empty,
                    client.name = tracing::field::Empty,
                    protocol.version = tracing::field::Empty,
                );
                let began = std::time::Instant::now();
                let (reply, known) = async {
                    match decode_request(bytes) {
                        Ok(wire) => {
                            let known = !matches!(wire.envelope.request, Request::Unknown);
                            (
                                self.answer(&connection, &state, bytes, wire, &name).await,
                                known,
                            )
                        }
                        Err(e) => (
                            Response::Err(Error::Malformed {
                                message: e.to_string(),
                            })
                            .into(),
                            false,
                        ),
                    }
                }
                .instrument(span.clone())
                .await;
                let outcome = match &reply.response {
                    Response::Err(error) => crate::telemetry::error_kind(error),
                    _ => "ok".into(),
                };
                span.record("outcome", outcome.as_str());
                // The type as the metrics name it: one this build knows,
                // as the client chooses what it sends (C-12).
                self.services.metrics.client_request(
                    if known { name.as_str() } else { "unknown" },
                    &outcome,
                    began.elapsed(),
                );
                // Once it has said Hello: a Hello's own span too.
                if let Some(hello) = state.hello.get() {
                    span.record("client.platform", hello.platform.as_str());
                    span.record("client.name", hello.client_name.as_str());
                    span.record("protocol.version", i64::from(hello.protocol_version));
                }
                reply
            }
            Err(e) => Response::Err(Error::Malformed {
                message: e.to_string(),
            })
            .into(),
        };
        if let Response::Err(error) = &response.response {
            self.count_error(error);
            if let Ok(bytes) = &request {
                self.refused(&connection, Frame::Rpc(bytes, &name), error)
                    .await;
            }
            // A frame that could not be read before any Hello was this
            // device's Hello, as far as it can tell.
            if name == "hello" || (request.is_err() && state.hello.get().is_none()) {
                self.hello_refused(&connection, request.as_deref().ok(), error)
                    .await;
            }
        }
        let response = RpcResponse {
            server: self.instance(),
            reply: response,
        };
        let sent = match codec::write_reply(&mut send, &response).await {
            // Nothing of it was written. The client gets the reason, not a
            // stream that ends with no response.
            Err(codec::CodecError::TooLarge { len }) => {
                tracing::warn!(
                    request = name.as_str(),
                    len,
                    "a reply is over the frame limit"
                );
                let refusal = RpcResponse {
                    server: self.instance(),
                    reply: Response::Err(Error::ReplyTooLarge { bytes: len }).into(),
                };
                codec::write_reply(&mut send, &refusal).await
            }
            sent => sent,
        };
        if let Err(e) = sent {
            tracing::debug!(error = %e, "cannot send a response");
            return;
        }
        let _ = send.finish();
        delivered(&send).await;
    }

    fn instance(&self) -> ServerInstance {
        ServerInstance {
            id: self.instance.clone(),
            time_ms: crate::operations::now_ms(),
        }
    }

    async fn answer(
        &self,
        connection: &Connection,
        state: &Arc<ConnectionState>,
        bytes: &[u8],
        wire: RpcRequest,
        name: &str,
    ) -> RpcReply {
        // The session a mutation was authenticated with for its operation,
        // handed to its execution so it is not looked up twice (Review
        // 2026-10-11 N2.3): set below, before the execution runs.
        let known: Arc<std::sync::OnceLock<(SessionId, Who)>> = Arc::default();
        // The request as decoded above, handed to its execution: its frame
        // is read once (review 2026-10-12). Owned, as a mutation's
        // execution is spawned.
        let execute = {
            let this = self.clone();
            let connection = connection.clone();
            let state = state.clone();
            let name = name.to_owned();
            let known = known.clone();
            move |wire: RpcRequest| async move {
                let expected = wire.expected_revision;
                // What the revision below is told from, kept before the
                // request goes to its answer.
                let request = match &wire.envelope.request {
                    request @ (Request::ChangeQuality { .. }
                    | Request::ChangeSubtitle { .. }
                    | Request::SeekPlayback { .. }
                    | Request::PlaybackRevision { .. }) => Some(request.clone()),
                    _ => None,
                };
                let response = this
                    .answer_once(&connection, &state, wire, &name, known.get().cloned())
                    .await;
                let playback = match (&response, request) {
                    (Response::Ok(Reply::PlaybackStarted { handle, .. }), _) => {
                        Some(PlaybackRevision {
                            handle: handle.clone(),
                            revision: 0,
                        })
                    }
                    // A quality that gives the same plan restarts nothing, and
                    // its revision stays: told as it is.
                    (
                        Response::Ok(Reply::PlaybackChanged { .. }),
                        Some(Request::ChangeQuality { handle, .. }),
                    ) => this
                        .services
                        .playbacks
                        .revision_of(&handle)
                        .map(|revision| PlaybackRevision { handle, revision }),
                    (
                        Response::Ok(Reply::PlaybackChanged { .. } | Reply::PlaybackSeeked { .. }),
                        Some(
                            Request::ChangeSubtitle { handle, .. }
                            | Request::SeekPlayback { handle, .. },
                        ),
                    ) => Some(PlaybackRevision {
                        handle,
                        revision: expected.expect("checked by control") + 1,
                    }),
                    (
                        Response::Ok(Reply::PlaybackRevision { revision }),
                        Some(Request::PlaybackRevision { handle }),
                    ) => Some(PlaybackRevision {
                        handle,
                        revision: *revision,
                    }),
                    _ => None,
                };
                RpcReply { response, playback }
            }
        };
        if spoolframe_protocol::policy::retry_safe(&wire.envelope.request)
            || matches!(wire.envelope.request, Request::Unknown)
        {
            return execute(wire).await;
        }
        // Preserve the handshake error even for an old or anonymous request.
        if state.hello.get().is_none() {
            return Response::Err(Error::HandshakeRequired).into();
        }
        let Some(operation) = wire.operation.clone() else {
            return Response::Err(Error::Malformed {
                message: "mutation requires an operation ID".into(),
            })
            .into();
        };
        let endpoint = connection.remote_id().to_string();
        let (operations, anonymous) = self.operations.of(&wire.envelope.request);
        let fingerprint = *blake3::hash(bytes).as_bytes();
        let now = crate::operations::now_ms();
        let cached =
            match operations.lookup(&endpoint, &self.instance, &operation, fingerprint, now) {
                Ok(cached) => cached,
                Err(error) => return Response::Err(error).into(),
            };
        let invocation = if let Some(cached) = cached {
            Ok(cached)
        } else {
            let mut user = None;
            if !anonymous {
                let (session, signed_in) = match self
                    .authenticate(connection, state, wire.envelope.session.clone())
                    .await
                {
                    Ok(user) => user,
                    Err(error) => return Response::Err(error).into(),
                };
                if !matches!(wire.envelope.request, Request::UnlockProfile { .. })
                    && let Err(error) = state.locks.lock().use_session(&session)
                {
                    return Response::Err(error).into();
                }
                user = Some(signed_in.principal().to_string());
                let _ = known.set((session, signed_in));
            }
            operations.begin(
                crate::operations::Owner {
                    endpoint: &endpoint,
                    user: user.as_deref(),
                },
                &self.instance,
                &operation,
                fingerprint,
                crate::operations::reservation(&wire.envelope.request),
                crate::operations::now_ms(),
            )
        };
        let invocation = match invocation {
            Ok(invocation) => invocation,
            Err(error) => return Response::Err(error).into(),
        };
        let replay = invocation.execute.is_none();
        // What a replay below reads of the request, which its execution
        // may take.
        let relocked = matches!(
            wire.envelope.request,
            Request::UnlockProfile { .. } | Request::SetPinRequired { .. }
        );
        let session = wire.envelope.session.clone();
        if let Some(send) = invocation.execute {
            let operations = operations.clone();
            // In the request's span: a spawned task starts with none, and
            // what the execution did (a Plex request, its log lines) went
            // to Tempo as traces of their own, the request's span empty.
            let execute = tracing::Instrument::in_current_span(execute(wire));
            tokio::spawn(async move {
                let reply = execute.await;
                operations.finish(
                    &endpoint,
                    &operation.id,
                    send,
                    reply,
                    crate::operations::now_ms(),
                );
            });
        }
        let mut outcome = invocation.outcome;
        let reply = match outcome.wait_for(Option::is_some).await {
            Ok(reply) => reply.clone().expect("waited for an outcome"),
            Err(_) => return Response::Err(Error::Internal).into(),
        };
        // A replay can recover an outcome, but cannot restore a PIN
        // unlock from another connection or from before a profile switch.
        if replay && matches!(reply.response, Response::Ok(_)) && relocked {
            let (session, _) = match self.authenticate(connection, state, session).await {
                Ok(user) => user,
                Err(error) => return Response::Err(error).into(),
            };
            if state.locks.lock().locked(&session) {
                return Response::Err(Error::ProfileLocked).into();
            }
        }
        reply
    }

    /// A Hello: the device's own words kept for the connection, its
    /// events stream opened, and this server's info as the answer.
    async fn hello(
        &self,
        connection: &Connection,
        state: &ConnectionState,
        hello: spoolframe_protocol::Hello,
    ) -> Response {
        // Kept, and shown to approvers: each string is bounded
        // (PX-10, finding 17).
        let hello = hello.within_bounds();
        if hello.protocol_version != spoolframe_protocol::PROTOCOL_VERSION {
            return Response::Err(Error::Unsupported {
                request: "protocol revision; update the SDK".into(),
            });
        }
        tracing::debug!(?hello, "hello");
        let platform = hello.platform;
        if state.hello.set(hello).is_ok() {
            // A device signed in before platforms were kept gets
            // one now; a failure costs only the label.
            let endpoint = connection.remote_id().to_string();
            if let Some(hello) = state.hello.get() {
                self.services
                    .connected
                    .hello(connection.stable_id(), endpoint.clone(), hello);
                // As for the events stream: closed meanwhile, the
                // accept loop's closed() may have come first.
                if connection.close_reason().is_some() {
                    self.services.connected.closed(connection.stable_id());
                }
            }
            if let Err(e) = self.store().note_device_platform(&endpoint, platform).await {
                tracing::warn!(error = %e, %endpoint, "cannot keep the device's platform");
            }
            self.open_events(connection).await;
            // A device that kept a session is signed in already.
            self.addresses.tell(connection.stable_id());
        }
        // The features this connection's Hello asked for that are on: from
        // the Hello kept, the first (PX-49).
        let experimental = state.hello.get().map_or_else(Vec::new, |hello| {
            spoolframe_protocol::experimental::turned_on(
                &hello.experimental,
                &self.experimental_on(),
            )
        });
        Ok::<_, Error>(Reply::ServerInfo(ServerInfo {
            name: self.name.get(),
            experimental,
            ..(*self.info).clone()
        }))
        .into()
    }

    /// `wire`: the request as [`Self::answer`] decoded it. `known`: the
    /// session the request names, already authenticated by
    /// [`Self::answer`] for its operation, used instead of a second look
    /// up when it is the one the request names.
    async fn answer_once(
        &self,
        connection: &Connection,
        state: &ConnectionState,
        wire: RpcRequest,
        name: &str,
        known: Option<(SessionId, Who)>,
    ) -> Response {
        let envelope = wire.envelope;
        let request = match envelope.request {
            Request::Hello(hello) => return self.hello(connection, state, hello).await,
            request => request,
        };
        let Some(hello) = state.hello.get() else {
            return Response::Err(Error::HandshakeRequired);
        };
        // The sign-ins a stranger makes, and what is guessed only at that
        // rate, draw on the strangers' sign-in budget first (PX-10).
        if crate::operations::on_strangers_budget(&request)
            && let Err(e) = self.stranger_attempt(state)
        {
            return Response::Err(e);
        }
        let reply = match request {
            Request::ReadSubtitle { .. } | Request::ReadSubtitleSegment { .. } => {
                Err(Error::Unsupported {
                    request: "subtitle bytes require a resource stream".into(),
                })
            }
            Request::Unknown => Err(Error::Unsupported {
                request: name.to_owned(),
            }),
            Request::AuthMethods => Ok(Reply::AuthMethods {
                methods: self.info.auth_methods.clone(),
            }),
            Request::Ping => Ok(Reply::Pong),
            Request::LoginPassword(login) => self.login_password(connection, login, hello).await,
            Request::BeginDeviceSignIn { remember } => {
                let started = self.services.auth.sign_ins().begin(Waiting {
                    endpoint: connection.remote_id().to_string(),
                    device_name: hello.device_name.clone(),
                    client_name: hello.client_name.clone(),
                    platform: hello.platform,
                    requested_at: self.services.auth.store().now(),
                    remember,
                    address: remote_ip(connection),
                });
                match started {
                    Some(started) => Ok(Reply::DeviceSignInStarted {
                        web_url: self
                            .services
                            .public_url
                            .as_deref()
                            .map(|origin| device_sign_in_web_link(origin, &started.code)),
                        request: started.request,
                        code: started.code,
                        expires_in_ms: started.expires_in.as_millis() as u64,
                    }),
                    None => Err(Error::RateLimited {
                        retry_after_ms: device_sign_in::LIFETIME.as_millis() as u64,
                    }),
                }
            }
            Request::AwaitDeviceSignIn { request } => {
                let endpoint = connection.remote_id().to_string();
                let outcome = self
                    .services
                    .auth
                    .sign_ins()
                    .wait(&request, &endpoint, device_sign_in::AWAIT)
                    .await;
                match outcome {
                    Outcome::Approved(handover) => {
                        tracing::info!(
                            %endpoint,
                            remembered = handover.remembered,
                            "device signed in by approval"
                        );
                        Ok(self
                            .logged_in(connection, handover.token, handover.remembered, false)
                            .await)
                    }
                    Outcome::Pending => Ok(Reply::DeviceSignInPending),
                    Outcome::Denied => Err(Error::DeviceSignInDenied),
                    Outcome::Expired => Err(Error::DeviceSignInExpired),
                }
            }
            Request::BeginPasskeySignIn { username } => {
                // Held per endpoint, and endpoints are free: strangers'
                // beginnings share their sign-in budget (PX-10), drawn above.
                let endpoint = connection.remote_id().to_string();
                self.services
                    .app_passkeys
                    .begin_sign_in(
                        self.store(),
                        self.id.as_bytes(),
                        &endpoint,
                        username.as_deref(),
                    )
                    .await
                    .map(Reply::PasskeySignInBegun)
                    .map_err(sign_in_error)
            }
            Request::FinishPasskeySignIn { signed, remember } => match user_handle_given(&signed) {
                Ok(()) => {
                    self.finish_passkey_sign_in(connection, &signed, hello, remember)
                        .await
                }
                Err(e) => Err(e),
            },
            // P4-01. Neither draws on the strangers' sign-in budget: the
            // device's key is the proof, and nothing is guessed here.
            Request::TrustedProfiles => {
                let endpoint = connection.remote_id().to_string();
                let profiles = self
                    .services
                    .auth
                    .trusted_profiles(&endpoint)
                    .await
                    .map_err(internal);
                profiles.map(|profiles| Reply::TrustedProfiles {
                    profiles: profiles
                        .into_iter()
                        .map(|p| TrustedProfile {
                            user_id: format!("local:{}", p.user),
                            username: p.username,
                            display_name: p.display_name,
                            pin_required: p.pin_required,
                            auto_login: p.auto_login,
                        })
                        .collect(),
                })
            }
            Request::ResumeProfile { user_id } => {
                self.resume_profile(connection, &user_id, hello).await
            }
            // P7-06: a sign-in a stranger makes, on the same budget.
            Request::BeginPortableSignIn => {
                let endpoint = connection.remote_id().to_string();
                match self.portable_sign_ins.begin(&endpoint) {
                    Some(nonce) => Ok(Reply::PortableSignInBegun {
                        nonce,
                        expires_in_secs: crate::portable::SIGN_IN_NONCE_LIFETIME.as_secs(),
                    }),
                    None => Err(Error::RateLimited {
                        retry_after_ms: crate::portable::SIGN_IN_NONCE_LIFETIME.as_millis() as u64,
                    }),
                }
            }
            Request::FinishPortableSignIn(sign_in) => {
                self.portable_sign_in(connection, &sign_in, hello).await
            }
            // P7-05: a sign-in a stranger makes, on the same budget.
            Request::RedeemPortableInvite(redeem) => {
                self.redeem_portable_invite(connection, &redeem, hello)
                    .await
            }
            // P7-08: public data, on the strangers' budget.
            Request::IdentityRecord { user } => self.identity_record(user).await,
            Request::IdentityRecordByHandle { handle } => {
                self.identity_record_by_handle(&handle).await
            }
            // P7-08: pairing through this server, the code guessed only at
            // the strangers' rate. Collecting is the offering device's
            // own, every few seconds, and off the budget.
            Request::OfferPairing(offer) => self
                .pairing_relay
                .offer(&connection.remote_id().to_string(), offer)
                .map(|()| Reply::Done)
                .map_err(relay_error),
            Request::FetchPairing { code } => self
                .pairing_relay
                .fetch(&code)
                .map(Reply::PairingOffered)
                .map_err(relay_error),
            Request::AnswerPairing { code, answer } => self
                .pairing_relay
                .answer(&code, answer)
                .map(|()| Reply::Done)
                .map_err(relay_error),
            Request::CollectPairing { code } => self
                .pairing_relay
                .collect(&connection.remote_id().to_string(), &code)
                .map(|answer| Reply::PairingAnswered { answer })
                .map_err(relay_error),
            // P6-01: a sign-in a stranger makes, on the same budget.
            Request::RedeemInvite(redeem) => self.redeem_invite(connection, &redeem, hello).await,
            // Everything else needs a session that is live on this
            // endpoint. Each request names its own, so one connection can
            // serve several users at once (§15).
            request => match match known {
                Some(known) if Some(&known.0) == envelope.session.as_ref() => Ok(known),
                _ => self.authenticate(connection, state, envelope.session).await,
            } {
                Ok((session, who)) => {
                    // Locked, only these two (P2-08).
                    let exempt = matches!(
                        request,
                        Request::ActivateProfile | Request::WhoAmI | Request::UnlockProfile { .. }
                    );
                    // Opened with a one-time password: only choosing a new
                    // one, or leaving.
                    let after_reset = matches!(
                        request,
                        Request::ChangePassword(_) | Request::Logout | Request::WhoAmI
                    );
                    let allowed = if who.password_change_only() && !after_reset {
                        Err(Error::PasswordChangeRequired)
                    } else if exempt || who.password_change_only() {
                        Ok(())
                    } else {
                        state.locks.lock().use_session(&session)
                    };
                    match allowed {
                        Ok(()) => {
                            self.serve(
                                connection,
                                state,
                                hello,
                                session,
                                who,
                                request,
                                wire.expected_revision,
                            )
                            .await
                        }
                        Err(e) => Err(e),
                    }
                }
                Err(e) => Err(e),
            },
        };
        reply.into()
    }

    /// [`Self::authenticate`], refusing a session a one-time password
    /// opened (`PasswordChangeRequired`): what reads media and artwork use,
    /// as requests other than choosing a new password do.
    async fn authenticate_unrestricted(
        &self,
        connection: &Connection,
        state: &ConnectionState,
        session: Option<SessionId>,
    ) -> Result<(SessionId, Who), Error> {
        let (session, who) = self.authenticate(connection, state, session).await?;
        if who.password_change_only() {
            return Err(Error::PasswordChangeRequired);
        }
        Ok((session, who))
    }

    /// The user of `session`, if it is live and bound to this connection's
    /// endpoint. Every failure is the same `Unauthenticated`. Whether its
    /// device asks for the user's PIN is noted for the connection's locks.
    async fn authenticate(
        &self,
        connection: &Connection,
        state: &ConnectionState,
        session: Option<SessionId>,
    ) -> Result<(SessionId, Who), Error> {
        let session = session.ok_or(Error::Unauthenticated)?;
        let endpoint = connection.remote_id().to_string();
        match self.services.auth.endpoint_who(&session.0, &endpoint).await {
            Ok(Some(who)) => {
                if matches!(who, Who::Local(_)) {
                    state.peer.note_local();
                }
                state.locks.lock().learn(&session, who.pin_required());
                state.peer.note_signed_in(Some(&who.log_name()));
                Ok((session, who))
            }
            Ok(None) => Err(Error::Unauthenticated),
            Err(e) => Err(internal(e)),
        }
    }

    async fn resource(
        &self,
        connection: &Connection,
        state: &ConnectionState,
        send: SendStream,
        recv: RecvStream,
        frame: Result<Vec<u8>, codec::CodecError>,
    ) {
        let request = frame
            .as_ref()
            .map_err(ToString::to_string)
            .and_then(|bytes| codec::decode::<ResourceRequest>(bytes).map_err(|e| e.to_string()));
        // A read of a playback's media is a span; no other resource is.
        let span = match &request {
            Ok(ResourceRequest::Media { request, .. }) => self.segment_read_span(request),
            _ => tracing::Span::none(),
        };
        use tracing::Instrument as _;
        self.send_resource(connection, state, send, recv, frame, request)
            .instrument(span)
            .await;
    }

    /// The `segment read` span of `read`, which its answer runs in: an HLS
    /// segment, or a range of the file or the progressive output. About
    /// one per 6 s segment, per playback.
    fn segment_read_span(&self, read: &MediaRead) -> tracing::Span {
        use tracing::field::Empty;
        let span = tracing::info_span!(
            "segment read",
            segment.index = read.segment.map(i64::from),
            segment.rung = read.segment.map(|_| i64::from(read.variant)),
            read.offset = read.offset as i64,
            read.length = read.length as i64,
            read.bytes = Empty,
            read.waited_for_encode = Empty,
            read.wait_ms = Empty,
            conversion.epoch = Empty,
            outcome = Empty,
        );
        if let Some(trace) = self.services.playbacks.traced(&read.handle) {
            trace.mark(&span);
        }
        span
    }

    /// The answer to a resource stream's `request`, in the span it runs in.
    async fn send_resource(
        &self,
        connection: &Connection,
        state: &ConnectionState,
        mut send: SendStream,
        recv: RecvStream,
        frame: Result<Vec<u8>, codec::CodecError>,
        request: Result<ResourceRequest, String>,
    ) {
        let result = match request {
            Ok(request) => self.open_resource(connection, state, request, recv).await,
            Err(message) => Err(Error::Malformed { message }),
        };
        if let (Err(error), Ok(bytes)) = (&result, &frame) {
            self.refused(connection, Frame::Resource(bytes), error)
                .await;
        }
        let span = tracing::Span::current();
        match &result {
            Ok((ResourceMetadata::Media { epoch, .. }, length, _)) => {
                span.record("read.bytes", *length as i64);
                span.record("conversion.epoch", i64::from(*epoch));
            }
            Ok(_) => {}
            Err(error) => {
                span.record("outcome", crate::telemetry::error_kind(error));
            }
        }
        let opened = result.is_ok();
        let stall = self.connections.admission().stalled_read;
        let sent = async {
            match result {
                Ok((metadata, length, body)) => {
                    let media = matches!(metadata, ResourceMetadata::Media { .. });
                    write_frame(&mut send, &ResourceResponse::Ready { length, metadata }).await?;
                    match body {
                        ResourceBody::Bytes(bytes) => {
                            send_unless_stalled(&mut send, &bytes, stall).await?;
                        }
                        ResourceBody::File(file) => {
                            let copied =
                                copy_unless_stalled(file.take(length), &mut send, stall).await?;
                            if media {
                                self.services.metrics.media_reads.inc();
                                self.services.metrics.media_read_bytes.add(copied);
                            }
                            if copied != length {
                                return Err(codec::CodecError::Io(std::io::Error::new(
                                    std::io::ErrorKind::UnexpectedEof,
                                    "resource ended before its advertised length",
                                )));
                            }
                        }
                    }
                }
                Err(error) => {
                    self.count_error(&error);
                    write_frame(&mut send, &ResourceResponse::Error { error }).await?
                }
            }
            Ok::<_, codec::CodecError>(())
        }
        .await;
        match sent {
            Ok(()) => {
                let _ = send.finish();
                delivered(&send).await;
                if opened {
                    span.record("outcome", "ok");
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "resource read failed");
                let _ = send.reset(0u32.into());
                span.record("outcome", "send_failed");
            }
        }
    }

    async fn open_resource(
        &self,
        connection: &Connection,
        state: &ConnectionState,
        request: ResourceRequest,
        recv: RecvStream,
    ) -> Result<(ResourceMetadata, u64, ResourceBody), Error> {
        let Some(hello) = state.hello.get() else {
            return Err(Error::HandshakeRequired);
        };
        match request {
            ResourceRequest::SetPlaylistArt {
                session,
                id,
                length,
            } => {
                let artwork = self
                    .set_playlist_art(connection, state, hello, session, &id, length, recv)
                    .await?;
                Ok((
                    ResourceMetadata::PlaylistArt { artwork },
                    0,
                    ResourceBody::Bytes(Vec::new()),
                ))
            }
            ResourceRequest::Artwork { request, max_bytes } => {
                let image = self.find_artwork(connection, state, request).await?;
                let length = image.bytes.len() as u64;
                if length > max_bytes {
                    return Err(Error::ReplyTooLarge { bytes: length });
                }
                Ok((
                    ResourceMetadata::Artwork {
                        content_type: image.content_type,
                        etag: image.etag,
                    },
                    length,
                    ResourceBody::Bytes(image.bytes),
                ))
            }
            ResourceRequest::Media { request, revision } => {
                let result = self.open_range(connection, state, &request, revision).await;
                if let (Some(n), Err(error)) = (request.segment, &result) {
                    tracing::warn!(id = %self.played(&request.handle), endpoint = %connection.remote_id().fmt_short(),
                        segment = n, %error, "an HLS segment read failed");
                }
                let (file, reply) = result?;
                // Seeking can replace the encode while a read waits. Check
                // again after preparation, outside that wait, before labelling
                // the pinned file with this revision.
                let _control = self
                    .hold_revision(connection, &request.handle, &request.session, revision)
                    .await?;
                // On its chart in playback debug (P3-15).
                self.services.playbacks.read(
                    &request.handle,
                    request.offset,
                    reply.sent,
                    request.segment,
                );
                Ok((
                    ResourceMetadata::Media {
                        size: reply.size,
                        complete: reply.complete,
                        epoch: reply.epoch,
                        revision,
                    },
                    reply.sent,
                    ResourceBody::File(file),
                ))
            }
            request => {
                let (session, handle, revision, read) = match request {
                    ResourceRequest::Subtitle {
                        session,
                        handle,
                        revision,
                        offset,
                        version,
                    } => (
                        session,
                        handle.clone(),
                        revision,
                        Request::ReadSubtitle {
                            handle,
                            offset,
                            version,
                        },
                    ),
                    ResourceRequest::SubtitleSegment {
                        session,
                        handle,
                        revision,
                        segment,
                    } => (
                        session,
                        handle.clone(),
                        revision,
                        Request::ReadSubtitleSegment { handle, n: segment },
                    ),
                    _ => unreachable!(),
                };
                let (session, who) = self
                    .authenticate_unrestricted(connection, state, Some(session))
                    .await?;
                state.locks.lock().use_session(&session)?;
                // A stale revision is refused before any work.
                self.check_revision(connection, &handle, &session, revision)
                    .await?;
                // The text is found, or waited for (up to READ_WAIT while
                // OCR runs), holding no lock: a seek, a subtitle change, a
                // stop or a progress report on this playback goes on
                // meanwhile (review 2026-10-11 M8). As for media, the
                // revision is checked again under the control lock after,
                // so a subtitle changed meanwhile is never labelled with
                // this revision.
                let reply = self
                    .serve(connection, state, hello, session.clone(), who, read, None)
                    .await?;
                let _control = self
                    .hold_revision(connection, &handle, &session, revision)
                    .await?;
                let (format, bytes, total, version) = match reply {
                    Reply::SubtitleChunk {
                        format,
                        bytes,
                        total,
                        version,
                    } => (format, bytes, total, version),
                    Reply::Subtitle { format, bytes } => {
                        let total = bytes.len() as u64;
                        (format, bytes, total, 0)
                    }
                    _ => unreachable!(),
                };
                let length = bytes.len() as u64;
                if length > MAX_READ {
                    return Err(Error::ReplyTooLarge { bytes: length });
                }
                Ok((
                    ResourceMetadata::Subtitle {
                        format,
                        total,
                        version,
                        revision,
                    },
                    length,
                    ResourceBody::Bytes(bytes),
                ))
            }
        }
    }

    /// `SetPlaylistArt` (PX-53): who may, and how large, are checked
    /// before the picture is read off the stream; then it is read, within
    /// the stall limit, and decoded and kept. The cover's id.
    #[allow(clippy::too_many_arguments)]
    async fn set_playlist_art(
        &self,
        connection: &Connection,
        state: &ConnectionState,
        hello: &spoolframe_protocol::Hello,
        session: SessionId,
        id: &str,
        length: u64,
        mut recv: RecvStream,
    ) -> Result<ArtworkId, Error> {
        if !(features::has(&hello.features, features::PLAYLISTS)
            && features::has(&hello.features, features::PLAYLIST_ART))
        {
            return Err(Error::Unsupported {
                request: format!("playlist art without {}", features::PLAYLIST_ART),
            });
        }
        let (session, who) = self
            .authenticate_unrestricted(connection, state, Some(session))
            .await?;
        state.locks.lock().use_session(&session)?;
        let actor = who.actor();
        let playlists = &self.services.playlists;
        // Liked and All are not there for a client that does not take them.
        if !features::has(&hello.features, features::LIKED)
            && playlists
                .builtin_of(id)
                .await
                .map_err(playlists_error)?
                .is_some()
        {
            return Err(playlists_error(crate::playlists::PlaylistsError::NotFound));
        }
        playlists
            .may_set_art(&actor, id)
            .await
            .map_err(playlists_error)?;
        if length > spoolframe_protocol::playlists::MAX_ART_BYTES {
            return Err(image_too_large());
        }
        let stall = self.connections.admission().stalled_read;
        // Held as it arrives: one that says 20 MB and sends little holds
        // little.
        let mut bytes = Vec::new();
        let mut chunk = vec![0; 64 << 10];
        while (bytes.len() as u64) < length {
            let want = ((length - bytes.len() as u64) as usize).min(chunk.len());
            let read = tokio::time::timeout(
                stall,
                tokio::io::AsyncReadExt::read(&mut recv, &mut chunk[..want]),
            )
            .await
            .map_err(|_| Error::Malformed {
                message: "the picture stopped arriving".into(),
            })?
            .map_err(|e| Error::Malformed {
                message: format!("the picture could not be read: {e}"),
            })?;
            if read == 0 {
                return Err(Error::Malformed {
                    message: format!(
                        "the stream ended {} bytes into a picture of {length}",
                        bytes.len()
                    ),
                });
            }
            bytes.extend_from_slice(&chunk[..read]);
        }
        let summary = playlists
            .set_art(&actor, id, &bytes)
            .await
            .map_err(playlists_error)?;
        summary.artwork.ok_or(Error::Internal)
    }

    async fn find_artwork(
        &self,
        connection: &Connection,
        state: &ConnectionState,
        request: ArtworkRequest,
    ) -> Result<Artwork, Error> {
        let (session, who) = self
            .authenticate(connection, state, Some(request.session))
            .await?;
        state.locks.lock().use_session(&session)?;
        // An uploaded cover (PX-53) is a user's photo: only for who sees a
        // playlist that has it.
        if matches!(
            crate::covers::local(&request.id),
            Some(crate::covers::Local::Cover(_))
        ) && !self
            .services
            .playlists
            .sees_art(&who.actor(), &request.id)
            .await
            .map_err(playlists_error)?
        {
            return Err(Error::NotFound {
                what: request.id.to_string(),
            });
        }
        if request.id.namespace() == Namespace::Preview {
            return self.preview_sheet(&request.id, request.size).await;
        }
        self.services
            .artwork
            .get(&request.id, request.size)
            .await
            .map_err(|e| match e {
                ArtworkError::Catalog(e) => catalog_error(e),
                ArtworkError::BadSize(size) => Error::Malformed {
                    message: format!("artwork size {} is not served", size.as_str()),
                },
                other => internal(other),
            })
    }

    /// A sheet of frame previews (P3-13), from the previews cache rather
    /// than the catalog's. A size below the sheet's scales it whole; the
    /// client scales the tile rectangles to match.
    async fn preview_sheet(&self, id: &ArtworkId, size: ArtworkSize) -> Result<Artwork, Error> {
        let not_found = || Error::NotFound {
            what: id.to_string(),
        };
        let (key, n) = previews::sheet_of(id).ok_or_else(not_found)?;
        let etag = format!("{key}-{n}-{}", size.as_str());
        let width = match size {
            ArtworkSize::Unknown => {
                return Err(Error::Malformed {
                    message: format!("artwork size {} is not served", size.as_str()),
                });
            }
            size => size.width(),
        };
        let bytes = match width {
            None => self.services.previews.sheet(key, n).await,
            Some(width) => self
                .services
                .previews
                .sheet_at(key, n, width)
                .await
                .map(|b| b.as_ref().clone()),
        }
        .ok_or_else(not_found)?;
        Ok(Artwork {
            bytes,
            content_type: "image/jpeg".into(),
            etag,
        })
    }

    /// The playback behind `handle`, if `session` on this connection's
    /// endpoint may use it. The session itself is checked again at most
    /// every `playback::RECHECK`, so reads stay off the database, and
    /// whether its item is still served with it: the backstop to ending
    /// playbacks when sessions end or the catalog changes (B-10).
    async fn use_playback(
        &self,
        connection: &Connection,
        handle: &PlaybackHandle,
        session: &SessionId,
    ) -> Result<Playback, Error> {
        self.use_playback_as(connection, handle, session, Using::Plays)
            .await
    }

    /// [`Self::use_playback`], used `how`: a progress report, which a
    /// paused client sends too, keeps it alive without playing it.
    async fn use_playback_as(
        &self,
        connection: &Connection,
        handle: &PlaybackHandle,
        session: &SessionId,
        how: Using,
    ) -> Result<Playback, Error> {
        self.use_playback_with(connection, handle, session, how, Playback::clone)
            .await
    }

    /// Refuse with `RevisionConflict` unless `handle` is still at
    /// `revision`.
    async fn check_revision(
        &self,
        connection: &Connection,
        handle: &PlaybackHandle,
        session: &SessionId,
        revision: u64,
    ) -> Result<(), Error> {
        let now = self
            .use_playback_with(connection, handle, session, Using::Plays, |p| p.revision)
            .await?;
        if now != revision {
            return Err(Error::RevisionConflict { revision: now });
        }
        Ok(())
    }

    /// Take `handle`'s control lock, then check `revision` again under it:
    /// a result prepared without the lock is never labelled with a
    /// revision replaced meanwhile. The lock is held while the guard is.
    async fn hold_revision(
        &self,
        connection: &Connection,
        handle: &PlaybackHandle,
        session: &SessionId,
        revision: u64,
    ) -> Result<crate::keyed_locks::Guard<'_>, Error> {
        let control = self
            .services
            .playbacks
            .control(handle, session, &connection.remote_id().to_string())
            .await
            .map_err(playback_refusal)?;
        self.check_revision(connection, handle, session, revision)
            .await?;
        Ok(control)
    }

    /// [`Self::use_playback_as`], taking only what `read` reads of the
    /// playback (review 2026-10-11 M10).
    async fn use_playback_with<T>(
        &self,
        connection: &Connection,
        handle: &PlaybackHandle,
        session: &SessionId,
        how: Using,
        read: impl FnOnce(&Playback) -> T,
    ) -> Result<T, Error> {
        let endpoint = connection.remote_id().to_string();
        use_playback_checked(&self.services, handle, session, &endpoint, how, read).await
    }

    /// Log that the request in `frame` was refused with `error`, if that
    /// is a refusal (finding 46) and the log is not full for this device
    /// ([`crate::refusals`]). The frame is read again, and whose session
    /// it named looked up, only then: a flood costs a counter each.
    async fn refused(&self, connection: &Connection, frame: Frame<'_>, error: &Error) {
        // A working invite code with a username taken or not acceptable is
        // a refused redemption too (P6-07, glm E-8); elsewhere those
        // answers are a form's, not refusals.
        let redeeming = matches!(frame, Frame::Rpc(_, "redeem_invite"));
        let kind = match error {
            Error::UsernameTaken | Error::InvalidInput { .. } if redeeming => Kind::SignInFailed,
            Error::Unauthenticated => Kind::Unauthenticated,
            Error::ProfileLocked => Kind::ProfileLocked,
            Error::RecentSignInRequired => Kind::RecentSignInRequired,
            Error::RateLimited { .. } => Kind::RateLimited,
            Error::InvalidCredentials
            | Error::InviteInvalid
            | Error::InviteExpired
            | Error::InviteExhausted => Kind::SignInFailed,
            _ => return,
        };
        let source = connection.remote_id().fmt_short().to_string();
        let auth = &self.services.auth;
        if !auth.refusals().admit(&source, kind, Channel::Quic) {
            return;
        }
        let asked = Asked::of(frame);
        let owner = match &asked.session {
            Some(session) => auth.session_owner(&session.0).await.ok().flatten(),
            None => None,
        };
        let endpoint = connection.remote_id().to_string();
        let (kind, reason) = match kind {
            Kind::Unauthenticated => match (&asked.session, &owner) {
                (None, _) if asked.resume => (kind, "device not trusted for that user"),
                (None, _) => (Kind::NoSession, "no session"),
                (Some(_), None) => (kind, "no such session"),
                (Some(_), Some(owner)) if !owner.live => (kind, "session ended"),
                (Some(_), Some(owner)) => match &owner.endpoint_id {
                    None => (kind, "a web session's token"),
                    Some(bound) if *bound != endpoint => (kind, "session bound to another device"),
                    // Live and this device's: what it was refused is a
                    // playback another session started (PB).
                    Some(_) => (kind, "playback is another session's"),
                },
            },
            Kind::SignInFailed if asked.invite => match error {
                Error::InviteExpired => (kind, "invite expired"),
                Error::InviteExhausted => (kind, "invite used up"),
                Error::UsernameTaken => (kind, "username taken"),
                // Its message may quote what was typed: not logged.
                Error::InvalidInput { .. } => (kind, "username or password not acceptable"),
                _ => (kind, "wrong or revoked invite code"),
            },
            Kind::SignInFailed => match (&asked.username, asked.credential) {
                (Some(username), _) => (kind, auth.why_password_refused(username).await),
                (None, Some(credential)) => (kind, credential),
                (None, None) => (kind, "invalid credentials"),
            },
            Kind::ProfileLocked => (kind, "profile locked"),
            Kind::RecentSignInRequired => (kind, "sign-in too old for this change"),
            Kind::RateLimited => (kind, "rate-limited"),
            // Not among the protocol's errors above.
            Kind::NoSession | Kind::Forbidden | Kind::HelloRefused => return,
        };
        refusals::log(&refusals::Refusal {
            kind,
            channel: Channel::Quic,
            request: asked.request,
            source,
            user: asked.username.or(owner.map(|owner| owner.username)),
            reason: reason.to_owned(),
        });
    }

    /// Log that a `Hello` (`frame`, if it could be read whole) was refused
    /// with `error`, unless this device's was logged within the hour
    /// ([`refusals::Refusals::hellos`]). The frame is read again, and the
    /// device looked up, only then.
    async fn hello_refused(&self, connection: &Connection, frame: Option<&[u8]>, error: &Error) {
        let source = connection.remote_id().fmt_short().to_string();
        if !self
            .services
            .auth
            .refused_hellos()
            .admit(&source, Kind::HelloRefused, Channel::Quic)
        {
            return;
        }
        let said = frame.map(SaidHello::of).unwrap_or_default();
        let reason = match error {
            Error::Unsupported { .. } => "unsupported protocol revision".to_owned(),
            Error::Malformed { message } => format!("malformed: {}", refusals::bounded(message)),
            other => refusals::bounded(&other.to_string()),
        };
        let device = match self
            .store()
            .device_name(&connection.remote_id().to_string())
            .await
        {
            Ok(device) => device.map(|name| refusals::bounded(&name)),
            Err(e) => {
                tracing::debug!(error = %e, "cannot look the device's name up");
                None
            }
        };
        refusals::log_hello(&refusals::HelloRefusal {
            source,
            device,
            client_protocol: said.protocol_version,
            client: said.client(),
            server_protocol: spoolframe_protocol::PROTOCOL_VERSION,
            server_version: self.info.server_version.clone(),
            reason,
        });
    }

    /// A playback's end as clients see it, counted (P3-14).
    fn count_error(&self, error: &Error) {
        let metrics = &self.services.metrics;
        match error {
            Error::PlaybackExpired => metrics.playback_expired.inc(),
            Error::PlaybackFailed { .. } => metrics.playback_failed.inc(),
            _ => {}
        }
    }

    /// What `handle` plays, for the log; the handle itself is never logged.
    fn played(&self, handle: &PlaybackHandle) -> String {
        self.services
            .playbacks
            .item(handle)
            .map_or_else(|| "(no open playback)".to_owned(), |id| id.to_string())
    }

    /// WebVTT segment `n` of `handle`'s subtitle rendition (P2-07).
    async fn subtitle_segment(
        &self,
        connection: &Connection,
        session: &SessionId,
        handle: &PlaybackHandle,
        n: u32,
    ) -> Result<Reply, Error> {
        let playback = self.use_playback(connection, handle, session).await?;
        Ok(Reply::Subtitle {
            format: SubtitleFormat::WebVtt,
            bytes: subtitle_segment_of(playback.conversion.as_deref(), playback.subtitle, n)
                .await?,
        })
    }

    /// The file positioned for `read`, and the reply that will precede it.
    async fn open_range(
        &self,
        connection: &Connection,
        state: &ConnectionState,
        read: &MediaRead,
        revision: u64,
    ) -> Result<(tokio::fs::File, MediaReadReply), Error> {
        let playback = self
            .use_playback_with(
                connection,
                &read.handle,
                &read.session,
                Using::Plays,
                RangeOf::of,
            )
            .await?;
        if playback.revision != revision {
            return Err(Error::RevisionConflict {
                revision: playback.revision,
            });
        }
        // The lock, in memory, on every read. A session this connection
        // has not looked up yet (a playback started before a reconnect) is
        // looked up first, so a new connection does not skip its lock.
        let known = state.locks.lock().knows(&read.session);
        if !known {
            self.authenticate_unrestricted(connection, state, Some(read.session.clone()))
                .await?;
        }
        state.locks.lock().use_session(&read.session)?;
        if let Some(conversion) = &playback.conversion {
            let keep_alive = || self.services.playbacks.touch(&read.handle);
            return converted_range(conversion, read, &keep_alive).await;
        }
        direct_range(&playback, read).await
    }

    /// Fill in `principal`'s watch state on `items`, in one lookup.
    async fn fill(
        &self,
        principal: Principal,
        items: impl Iterator<Item = &mut spoolframe_types::Item>,
    ) -> Result<(), Error> {
        let mut items: Vec<_> = items.collect();
        self.services
            .watch
            .fill(principal, &mut items)
            .await
            .map_err(watch_error)
    }

    /// A request in a checked session.
    #[expect(
        clippy::too_many_arguments,
        reason = "the call sites name each part; a struct would only repeat them"
    )]
    async fn serve(
        &self,
        connection: &Connection,
        state: &ConnectionState,
        hello: &spoolframe_protocol::Hello,
        session: SessionId,
        who: Who,
        request: Request,
        expected_revision: Option<u64>,
    ) -> Result<Reply, Error> {
        let catalog = &self.services.catalog;
        let principal = who.principal();
        let actor = who.actor();
        // PX-40: a connection that takes playlist events is told of the
        // changes to what each user it acts as sees.
        let playlists_listed = features::has(&hello.features, features::PLAYLISTS);
        // PX-43: Liked and All, and hearts, only for a client that knows
        // them; one that does not would offer to rename or delete them.
        let liked_listed = playlists_listed && features::has(&hello.features, features::LIKED);
        // When each item was last viewed, only for a client that takes it.
        let last_viewed_listed = features::has(&hello.features, features::LAST_VIEWED);
        // PX-52: play counts and Recently played, only for a client that
        // takes them.
        let play_history_listed = features::has(&hello.features, features::PLAY_HISTORY);
        if !play_history_listed && matches!(request, Request::RecentlyPlayed { .. }) {
            return Err(Error::Unsupported {
                request: format!("recently played without {}", features::PLAY_HISTORY),
            });
        }
        // Sharing by name: whom to share with, and the names of owners and
        // shares, only for a client that takes them.
        let sharing_listed =
            playlists_listed && features::has(&hello.features, features::PLAYLIST_SHARING);
        if !sharing_listed && matches!(request, Request::ListShareCandidates { .. }) {
            return Err(Error::Unsupported {
                request: format!("share candidates without {}", features::PLAYLIST_SHARING),
            });
        }
        // PX-53: playlists' pictures, only for a client that takes them.
        let art_listed = playlists_listed && features::has(&hello.features, features::PLAYLIST_ART);
        if !art_listed && matches!(request, Request::RemovePlaylistArt { .. }) {
            return Err(Error::Unsupported {
                request: format!("playlist art without {}", features::PLAYLIST_ART),
            });
        }
        // What comes up, and settings saved alone, only for a client that
        // takes them.
        let upcoming_listed =
            playlists_listed && features::has(&hello.features, features::PLAYLIST_UPCOMING);
        if !upcoming_listed
            && matches!(
                request,
                Request::UpcomingInPlaylist { .. } | Request::SetPlaylistSettings { .. }
            )
        {
            return Err(Error::Unsupported {
                request: format!("playlist upcoming without {}", features::PLAYLIST_UPCOMING),
            });
        }
        if playlists_listed {
            self.playlist_listeners
                .note(connection.stable_id(), principal, liked_listed);
        }
        // PX-03: subtitle timing, only for a client that takes it; it is
        // told of the items whose tracks it asks for or plays.
        let subtitle_sync_listed = features::has(&hello.features, features::SUBTITLE_SYNC);
        match &request {
            Request::SyncSubtitle { .. }
            | Request::UseOriginalSubtitleTiming { .. }
            | Request::SetSubtitleOffset { .. }
                if !subtitle_sync_listed =>
            {
                return Err(Error::Unsupported {
                    request: format!("subtitle timing without {}", features::SUBTITLE_SYNC),
                });
            }
            Request::Tracks { id }
            | Request::StartPlayback { id, .. }
            | Request::SyncSubtitle { id, .. }
            | Request::UseOriginalSubtitleTiming { id, .. }
            | Request::SetSubtitleOffset { id, .. }
                if subtitle_sync_listed =>
            {
                self.subtitle_listeners.note(connection.stable_id(), id);
            }
            _ => {}
        }
        // PX-02: subtitle downloads, only for a client that takes them.
        let subtitle_search_listed = features::has(&hello.features, features::SUBTITLE_SEARCH);
        if !subtitle_search_listed
            && matches!(
                request,
                Request::SearchSubtitles { .. }
                    | Request::DownloadSubtitle { .. }
                    | Request::RemoveSubtitleDownload { .. }
            )
        {
            return Err(Error::Unsupported {
                request: format!("subtitle downloads without {}", features::SUBTITLE_SEARCH),
            });
        }
        if !liked_listed {
            match &request {
                Request::Like { .. } | Request::Unlike { .. } => {
                    return Err(Error::Unsupported {
                        request: format!("a like without {}", features::LIKED),
                    });
                }
                // Liked and All are not there for it.
                Request::GetPlaylist { id, .. }
                | Request::UpdatePlaylist { id, .. }
                | Request::EditPlaylist { id, .. }
                | Request::DeletePlaylist { id }
                | Request::PinPlaylist { id, .. }
                | Request::UnpinPlaylist { id }
                | Request::PlayPlaylist { id, .. }
                | Request::NextInPlaylist { id, .. }
                | Request::UpcomingInPlaylist { id, .. }
                | Request::SetPlaylistSettings { id, .. }
                | Request::ListShareCandidates { id, .. }
                | Request::RemovePlaylistArt { id }
                    if self
                        .services
                        .playlists
                        .builtin_of(id)
                        .await
                        .map_err(playlists_error)?
                        .is_some() =>
                {
                    return Err(playlists_error(crate::playlists::PlaylistsError::NotFound));
                }
                _ => {}
            }
        }
        // A request about a playback says which in its span, and links to
        // the playback's start.
        if let Some(handle) = playback_of(&request)
            && let Some(trace) = self.services.playbacks.traced(handle)
        {
            trace.mark(&tracing::Span::current());
        }
        // Hold across preparation and publication, and re-read the handle
        // inside the handler. QUIC requests may arrive concurrently.
        let _control = match &request {
            Request::UpdatePlaybackProgress { handle, .. }
            | Request::ChangeSubtitle { handle, .. }
            | Request::ChangeQuality { handle, .. }
            | Request::SeekPlayback { handle, .. }
            | Request::StopPlayback { handle } => Some(
                self.services
                    .playbacks
                    .control(handle, &session, &connection.remote_id().to_string())
                    .await
                    .map_err(playback_refusal)?,
            ),
            _ => None,
        };
        if let Request::ChangeSubtitle { handle, .. }
        | Request::ChangeQuality { handle, .. }
        | Request::SeekPlayback { handle, .. }
        | Request::StopPlayback { handle } = &request
        {
            // Only the session and endpoint that started it may control or
            // end it; and only at the revision the client expects.
            let revision = self
                .use_playback_with(connection, handle, &session, Using::Plays, |p| p.revision)
                .await?;
            if expected_revision != Some(revision) {
                return Err(Error::RevisionConflict { revision });
            }
        }
        match request {
            Request::ActivateProfile => {
                state.locks.lock().activate(&session);
                Ok(Reply::Done)
            }
            Request::PlaybackProgressSequence { handle } => Ok(Reply::PlaybackProgressSequence {
                sequence: self
                    .use_playback_with(connection, &handle, &session, Using::Plays, |p| {
                        p.progress_sequence
                    })
                    .await?,
            }),
            Request::PlaybackRevision { handle } => Ok(Reply::PlaybackRevision {
                revision: self
                    .use_playback_with(connection, &handle, &session, Using::Plays, |p| p.revision)
                    .await?,
            }),
            Request::WhoAmI => {
                let user = match &who {
                    Who::Local(user) => user,
                    // P7-06: no PIN, no lock, no tie; the id is the
                    // name a device keeps their session by.
                    Who::Portable(portable) => {
                        // A portable user is never an admin: it may
                        // always leave.
                        return Ok(Reply::WhoAmI(WhoAmIReply {
                            user: UserSummary {
                                user_id: principal.to_string(),
                                username: who.username(),
                                display_name: portable.display_name.clone(),
                                role: spoolframe_types::Role::Consumer,
                                locked: false,
                                pin: None,
                                // A guest is no account: it has no ties,
                                // and its identity is its user id.
                                portable: Vec::new(),
                            },
                            can_leave: true,
                            features: features::names(features::ALL),
                        }));
                    }
                };
                // Asked while locked too: a PIN pad sizes itself by it.
                let pin = self.store().pin_info(user.user).await.map_err(internal)?;
                let ties = self.store().ties_of(user.user).await.map_err(internal)?;
                // PX-47: what `LeaveServer` would answer, by its own check,
                // so an app greys out leaving for the last admin.
                let can_leave = self.store().can_leave(user.user).await.map_err(internal)?;
                Ok(Reply::WhoAmI(WhoAmIReply {
                    user: UserSummary {
                        user_id: format!("local:{}", user.user),
                        username: user.username.clone(),
                        display_name: user.display_name.clone(),
                        role: user.role,
                        locked: state.locks.lock().locked(&session),
                        pin: pin.map(|p| PinState { digits: p.digits }),
                        portable: ties
                            .into_iter()
                            .map(|tie| spoolframe_protocol::PortableTie {
                                id: tie.portable,
                                tied_at: tie.tied_at,
                                last_used_at: tie.last_used_at,
                            })
                            .collect(),
                    },
                    can_leave,
                    features: features::names(features::ALL),
                }))
            }
            Request::BeginPasskeyRegistration => {
                let user = local(&who)?;
                // P3-03's rule, as for a passkey added in the web UI: a
                // session left signed in on a device does not add one.
                recently_signed_in(user, self.services.auth.store().now())?;
                let account = self.account(user).await?;
                self.services
                    .app_passkeys
                    .begin_registration(
                        self.store(),
                        self.id.as_bytes(),
                        &SessionToken(session.0.clone()).hash(),
                        &connection.remote_id().to_string(),
                        &account,
                        &self.name.get(),
                    )
                    .await
                    .map(Reply::PasskeyRegistrationBegun)
                    .map_err(ceremony_error)
            }
            Request::FinishPasskeyRegistration(made) => {
                let user = local(&who)?;
                let account = self.account(user).await?;
                self.services
                    .app_passkeys
                    .finish_registration(
                        self.store(),
                        &SessionToken(session.0.clone()).hash(),
                        &connection.remote_id().to_string(),
                        &account,
                        &made,
                    )
                    .await
                    .map_err(ceremony_error)?;
                Ok(Reply::Done)
            }
            Request::UnlockProfile { pin } => {
                let user = local(&who)?;
                let generation = state.locks.lock().generation;
                let endpoint = connection.remote_id().to_string();
                self.services
                    .auth
                    .check_pin(user.user, &endpoint, &pin)
                    .await
                    .map_err(pin_error)?;
                let mut locks = state.locks.lock();
                if locks.generation != generation
                    || locks
                        .active
                        .as_ref()
                        .is_some_and(|active| active != &session.0)
                {
                    return Err(Error::ProfileLocked);
                }
                locks.unlock(&session);
                Ok(Reply::Done)
            }
            // Changing or removing the PIN needs a proof (the current PIN
            // or the password), and the first PIN a recent sign-in, as the
            // web UI's pinSet (A-14): an unlocked session left on a TV must
            // not let whoever holds the remote take the PIN over, nor put
            // one on the account its owner then has to prove to change.
            // Where it is asked for needs only the session (a locked one
            // never gets here, and a PIN is never a way to sign in).
            Request::SetPin { pin, proof } => {
                let user = local(&who)?;
                let Some(digits) = auth::pin_digits(&pin) else {
                    return Err(Error::InvalidInput {
                        message: "a PIN is 4 to 8 digits".into(),
                    });
                };
                match proof {
                    Some(proof) => self.check_pin_proof(connection, user, proof).await?,
                    None => {
                        recently_signed_in(user, self.services.auth.store().now())?;
                        if self
                            .store()
                            .pin_info(user.user)
                            .await
                            .map_err(internal)?
                            .is_some()
                        {
                            return Err(Error::InvalidInput {
                                message: "enter the current PIN or the password to change it"
                                    .into(),
                            });
                        }
                    }
                }
                let phc = auth::hash_pin(&pin).await.map_err(internal)?;
                self.store()
                    .set_pin(user.user, &phc, digits)
                    .await
                    .map_err(internal)?;
                tracing::info!(user = %user.username, "PIN set");
                Ok(Reply::Done)
            }
            Request::ClearPin { proof } => {
                let user = local(&who)?;
                self.check_pin_proof(connection, user, proof).await?;
                self.store().clear_pin(user.user).await.map_err(internal)?;
                tracing::info!(user = %user.username, "PIN cleared");
                Ok(Reply::Done)
            }
            // The web UI's me.passkeys, me.passkeyRename and
            // me.passkeyRemove (P3-05), with the same store calls and so
            // the same rules: only the user's own, the name's length, and
            // the last way to sign in kept.
            Request::ListPasskeys => {
                let user = local(&who)?;
                let passkeys = self
                    .store()
                    .list_passkeys(user.user)
                    .await
                    .map_err(internal)?
                    .into_iter()
                    .map(|p| passkey_info(p, self.services.app_passkeys.rp_id()))
                    .collect();
                Ok(Reply::Passkeys { passkeys })
            }
            Request::RenamePasskey { id, name } => {
                let user = local(&who)?;
                self.store()
                    .rename_passkey(user.user, &id, &name)
                    .await
                    .map_err(|e| passkey_error(e, &id))?;
                Ok(Reply::Done)
            }
            Request::RemovePasskey { id } => {
                let user = local(&who)?;
                // As the web UI asks: a device left signed in does not
                // take a way to sign in away.
                recently_signed_in(user, self.services.auth.store().now())?;
                self.store()
                    .remove_passkey(user.user, &id)
                    .await
                    .map_err(|e| passkey_error(e, &id))?;
                tracing::info!(user = %user.user, "passkey removed by its user, in an app");
                Ok(Reply::Done)
            }
            // P3-07: the web UI's me.confirmPassword and me.confirmPasskey,
            // for an app, counted under the same keys. The PIN is no proof
            // here: it guards switching profiles on a shared device, and
            // is weaker than a sign-in. A locked session never gets here.
            Request::ConfirmPassword(confirm) => {
                let user = local(&who)?;
                self.confirm_password(connection, user, &confirm.password)
                    .await?;
                self.confirmed(&session, user, "password").await
            }
            Request::ChangePassword(change) => {
                let user = local(&who)?;
                self.change_password(connection, &session, user, &change)
                    .await?;
                Ok(Reply::Done)
            }
            Request::BeginConfirmPasskey => {
                let user = local(&who)?;
                let endpoint = connection.remote_id().to_string();
                // Refused before a challenge is made that could only fail.
                if let Some(retry_after) = self
                    .services
                    .auth
                    .attempt_limited(&crate::passkeys::limit_key(user.user), Some(&endpoint))
                {
                    return Err(sign_in_error(SignInError::RateLimited { retry_after }));
                }
                let begun = self
                    .services
                    .app_passkeys
                    .begin_confirm(
                        self.store(),
                        self.id.as_bytes(),
                        &SessionToken(session.0.clone()).hash(),
                        &endpoint,
                        user.user,
                    )
                    .await
                    .map_err(sign_in_error)?;
                begun.map(Reply::PasskeySignInBegun).ok_or(Error::NotFound {
                    what: "app passkey".into(),
                })
            }
            Request::FinishConfirmPasskey(signed) => {
                let user = local(&who)?;
                user_handle_given(&signed)?;
                let signer = self
                    .services
                    .app_passkeys
                    .finish_confirm(
                        &self.services.auth,
                        &SessionToken(session.0.clone()).hash(),
                        &connection.remote_id().to_string(),
                        &signed,
                    )
                    .await
                    .map_err(sign_in_error)?;
                // The challenge listed only the user's own; this is the
                // web UI's check too, in case that ever changes.
                if signer.id != user.user {
                    tracing::warn!(
                        user = %user.user,
                        passkey_of = %signer.id,
                        "a passkey of another user's was offered as a confirmation"
                    );
                    return Err(Error::InvalidCredentials);
                }
                self.confirmed(&session, user, "app passkey").await
            }
            Request::ListDevices => {
                let user = local(&who)?;
                let this = connection.remote_id().to_string();
                let devices = self
                    .store()
                    .user_devices(user.user)
                    .await
                    .map_err(internal)?
                    .into_iter()
                    .map(|d| DeviceInfo {
                        this_device: d.endpoint_id == this,
                        platform: d.platform(),
                        endpoint_id: d.endpoint_id,
                        name: d.name,
                        last_seen_at: d.last_seen_at,
                        pin_required: d.pin_required,
                        trusted: d.trusted,
                        auto_login: d.auto_login,
                    })
                    .collect();
                Ok(Reply::Devices { devices })
            }
            Request::GetPlexLink => {
                // A portable user links no Plex account here (P7-06).
                let Some(user) = who.local() else {
                    return Ok(Reply::PlexLink { link: None });
                };
                let Some(accounts) = &self.services.plex_accounts else {
                    return Ok(Reply::PlexLink { link: None });
                };
                let link = accounts.link(user.user).map_err(internal)?;
                let sync = match (&self.services.plex_sync, &link) {
                    (Some(sync), Some(_)) => sync.status(user.user),
                    _ => (None, false),
                };
                Ok(Reply::PlexLink {
                    link: plex_link(link, sync),
                })
            }
            Request::SetPinRequired { device, required } => {
                let user = local(&who)?;
                if !required {
                    recently_signed_in(user, self.services.auth.store().now())?;
                }
                let generation = state.locks.lock().generation;
                let this = connection.remote_id().to_string();
                let device = device.unwrap_or_else(|| this.clone());
                self.store()
                    .set_pin_required(user.user, &device, required)
                    .await
                    .map_err(|e| match e {
                        PinError::NoPin => Error::InvalidInput {
                            message: "set a PIN first".into(),
                        },
                        PinError::NoSuchDevice => Error::NotFound {
                            what: format!("device {device}"),
                        },
                        PinError::Store(e) => internal(e),
                    })?;
                if required && device == this {
                    // Who turns it on here is not locked out mid-use: this
                    // connection stays unlocked; the next one asks.
                    let mut locks = state.locks.lock();
                    locks.learn(&session, true);
                    if locks.generation == generation
                        && locks
                            .active
                            .as_ref()
                            .is_none_or(|active| active == &session.0)
                    {
                        locks.unlock(&session);
                    }
                }
                tracing::info!(user = %user.username, %device, required, "PIN requirement changed");
                Ok(Reply::Done)
            }
            // Forgets the device for the user too (P4-01): signing out is
            // leaving it.
            Request::Logout => {
                self.services
                    .auth
                    .logout(&session.0)
                    .await
                    .map_err(internal)?;
                Ok(Reply::LoggedOut)
            }
            Request::SetDeviceTrust {
                device,
                trusted,
                auto_login,
            } => {
                let user = local(&who)?;
                let this = connection.remote_id().to_string();
                let device = device.unwrap_or_else(|| this.clone());
                let own = device == this;
                // A client trusts only itself: another device is trusted
                // from the web UI (P4-01).
                if trusted && !own {
                    return Err(Error::InvalidInput {
                        message: "trust another device from the web UI".into(),
                    });
                }
                // Trust, and auto-login (which turns the PIN off), are as
                // strong as a sign-in: a session left signed in does not
                // grant them, nor end another device's sessions. Stopping
                // trust on this very device only takes away.
                if trusted || !own {
                    recently_signed_in(user, self.services.auth.store().now())?;
                }
                let revoked = self
                    .store()
                    .set_device_trust(user.user, &device, trusted, auto_login)
                    .await
                    .map_err(|e| match e {
                        TrustError::NoSuchDevice => Error::NotFound {
                            what: format!("device {device}"),
                        },
                        TrustError::AutoLoginUntrusted => Error::InvalidInput {
                            message: "auto-login needs the device to be trusted".into(),
                        },
                        TrustError::Store(e) => internal(e),
                    })?;
                tracing::info!(
                    user = %user.username,
                    %device,
                    trusted,
                    auto_login,
                    revoked,
                    "device trust changed in an app"
                );
                Ok(Reply::Done)
            }
            Request::ForgetDevice { device } => {
                let user = local(&who)?;
                // This device leaves by signing out; deleting it here would
                // end this session with no `LoggedOut` to say so.
                if device == connection.remote_id().to_string() {
                    return Err(Error::InvalidInput {
                        message: "sign out to leave this device".into(),
                    });
                }
                // It ends sessions elsewhere and takes away trust: a
                // session left signed in does not (P4-03).
                recently_signed_in(user, self.services.auth.store().now())?;
                let deleted = self
                    .store()
                    .delete_device(user.user, &device)
                    .await
                    .map_err(|e| match e {
                        PinError::NoSuchDevice => Error::NotFound {
                            what: format!("device {device}"),
                        },
                        e @ (PinError::NoPin | PinError::Store(_)) => internal(e),
                    })?;
                tracing::info!(
                    user = %user.username,
                    %device,
                    deleted,
                    "device deleted in an app"
                );
                Ok(Reply::Done)
            }
            Request::ListSessions => {
                let user = local(&who)?;
                let current = SessionToken(session.0).hash();
                let sessions = self
                    .services
                    .auth
                    .store()
                    .list_sessions(user.user)
                    .await
                    .map_err(internal)?
                    .into_iter()
                    .map(|s| SessionInfo {
                        current: s.token_hash == current,
                        platform: s.platform(),
                        device_name: s.device_name,
                        created_at: s.created_at,
                        last_used_at: s.last_used_at,
                    })
                    .collect();
                Ok(Reply::Sessions { sessions })
            }
            Request::Libraries => Ok(Reply::Libraries {
                libraries: catalog.libraries().await.map_err(catalog_error)?,
            }),
            Request::Items {
                library,
                offset,
                limit,
            } => {
                let page = Page {
                    offset,
                    limit: limit.min(MAX_PAGE),
                };
                let mut page = catalog.items(&library, page).await.map_err(catalog_error)?;
                self.fill(principal, page.items.iter_mut()).await?;
                Ok(Reply::Items {
                    items: page.items,
                    total: page.total,
                })
            }
            Request::Item { id } => {
                let mut found = catalog.item(&id).await.map_err(catalog_error)?;
                // The one place a client looks at a single item: bring over
                // any state it had under an earlier key.
                self.services
                    .watch
                    .relink(principal, &id, &found.external_ids)
                    .await
                    .map_err(watch_error)?;
                self.fill(
                    principal,
                    std::iter::once(&mut found.item).chain(found.children.iter_mut()),
                )
                .await?;
                if last_viewed_listed {
                    let mut items: Vec<&mut spoolframe_types::Item> =
                        std::iter::once(&mut found.item)
                            .chain(found.children.iter_mut())
                            .collect();
                    self.services
                        .watch
                        .fill_last_viewed(principal, &mut items)
                        .await
                        .map_err(watch_error)?;
                }
                if play_history_listed {
                    let mut items: Vec<&mut spoolframe_types::Item> =
                        std::iter::once(&mut found.item)
                            .chain(found.children.iter_mut())
                            .collect();
                    self.services
                        .watch
                        .fill_plays(principal, &mut items)
                        .await
                        .map_err(watch_error)?;
                }
                // PX-40: only for a client that takes them.
                let playlists = if playlists_listed {
                    Some(
                        self.services
                            .playlists
                            .holding(&actor, &found.item, &found.children, liked_listed)
                            .await
                            .map_err(playlists_error)?
                            .into_iter()
                            .map(|held| named_ref(held, sharing_listed, art_listed))
                            .collect(),
                    )
                } else {
                    None
                };
                // PX-43: the hearts, for a client that takes them.
                let (liked, liked_children) = if liked_listed {
                    self.services
                        .playlists
                        .liked(&actor, &found.item, &found.children)
                        .await
                        .map_err(playlists_error)?
                } else {
                    (None, None)
                };
                Ok(Reply::Item {
                    item: found.item,
                    children: found.children,
                    markers: found.markers,
                    playlists,
                    liked,
                    liked_children,
                })
            }
            Request::ListPlaylists => {
                let (playlists, pin_notes) = self
                    .services
                    .playlists
                    .list(&actor, liked_listed)
                    .await
                    .map_err(playlists_error)?;
                Ok(Reply::Playlists {
                    playlists: playlists
                        .into_iter()
                        .map(|p| named(p, sharing_listed, art_listed))
                        .collect(),
                    pin_notes,
                })
            }
            Request::GetPlaylist { id, offset, limit } => self
                .services
                .playlists
                .get(&actor, &id, offset, limit)
                .await
                .map(|mut page| {
                    if !liked_listed {
                        page.entries.iter_mut().for_each(|e| e.liked = false);
                    }
                    page.playlist = named(page.playlist, sharing_listed, art_listed);
                    Reply::Playlist(page)
                })
                .map_err(playlists_error),
            Request::CreatePlaylist {
                name,
                kinds,
                visibility,
            } => self
                .services
                .playlists
                .create(&actor, &name, kinds, visibility)
                .await
                .map(|p| Reply::PlaylistInfo(named(p, sharing_listed, art_listed)))
                .map_err(playlists_error),
            Request::UpdatePlaylist {
                id,
                name,
                kinds,
                visibility,
            } => self
                .services
                .playlists
                .update(
                    &actor,
                    &id,
                    crate::playlists::PlaylistUpdate {
                        name,
                        kinds,
                        visibility,
                    },
                )
                .await
                .map(|p| Reply::PlaylistInfo(named(p, sharing_listed, art_listed)))
                .map_err(playlists_error),
            Request::EditPlaylist {
                id,
                revision,
                edits,
            } => self
                .services
                .playlists
                .edit(&actor, &id, revision, edits)
                .await
                .map(|revision| Reply::PlaylistEdited { revision })
                .map_err(playlists_error),
            Request::RemovePlaylistArt { id } => self
                .services
                .playlists
                .remove_art(&actor, &id)
                .await
                .map(|p| Reply::PlaylistInfo(named(p, sharing_listed, art_listed)))
                .map_err(playlists_error),
            Request::DeletePlaylist { id } => self
                .services
                .playlists
                .delete(&actor, &id)
                .await
                .map(|()| Reply::Done)
                .map_err(playlists_error),
            Request::PinPlaylist { id, position } => self
                .services
                .playlists
                .pin(&actor, &id, position)
                .await
                .map(|()| Reply::Done)
                .map_err(playlists_error),
            Request::UnpinPlaylist { id } => self
                .services
                .playlists
                .unpin(&actor, &id)
                .await
                .map(|()| Reply::Done)
                .map_err(playlists_error),
            Request::PlayPlaylist {
                id,
                from,
                shuffle,
                repeat,
                skip_watched,
            } => self
                .services
                .playlists
                .play(
                    &actor,
                    &id,
                    from,
                    spoolframe_protocol::playlists::PlaySettings {
                        shuffle,
                        repeat,
                        skip_watched,
                    },
                )
                .await
                .map(|next| playing(next, liked_listed))
                .map_err(playlists_error),
            Request::NextInPlaylist { id, after } => self
                .services
                .playlists
                .next(&actor, &id, after)
                .await
                .map(|next| playing(next, liked_listed))
                .map_err(playlists_error),
            Request::UpcomingInPlaylist { id, limit } => self
                .services
                .playlists
                .upcoming(&actor, &id, limit)
                .await
                .map(|mut entries| {
                    if !liked_listed {
                        entries.iter_mut().for_each(|e| e.liked = false);
                    }
                    Reply::PlaylistUpcoming { entries }
                })
                .map_err(playlists_error),
            Request::SetPlaylistSettings {
                id,
                shuffle,
                repeat,
                skip_watched,
            } => self
                .services
                .playlists
                .set_settings(
                    &actor,
                    &id,
                    spoolframe_protocol::playlists::PlaySettings {
                        shuffle,
                        repeat,
                        skip_watched,
                    },
                )
                .await
                .map(|()| Reply::Done)
                .map_err(playlists_error),
            Request::ImportPlexPlaylists { dry_run } => {
                let user = local(&who)?;
                let Some(sync) = &self.services.plex_sync else {
                    return Err(Error::NotFound {
                        what: "a linked Plex account".into(),
                    });
                };
                crate::playlists::plex_import::import(
                    &self.services.playlists,
                    sync,
                    &actor,
                    user.user,
                    dry_run,
                )
                .await
                .map(Reply::PlexImport)
                .map_err(|e| match e {
                    crate::playlists::plex_import::ImportError::NotLinked => Error::NotFound {
                        what: "a linked Plex account".into(),
                    },
                    crate::playlists::plex_import::ImportError::Playlists(e) => playlists_error(e),
                    crate::playlists::plex_import::ImportError::Sync(e) => {
                        tracing::warn!(error = %e, "a Plex playlist import failed");
                        Error::Unavailable {
                            message: "Plex could not be read".into(),
                        }
                    }
                })
            }
            Request::DismissPinNotes => self
                .services
                .playlists
                .dismiss_pin_notes(&actor)
                .await
                .map(|()| Reply::Done)
                .map_err(playlists_error),
            Request::Like { items } => self
                .services
                .playlists
                .like(&actor, &items)
                .await
                .map(|p| Reply::PlaylistInfo(named(p, sharing_listed, art_listed)))
                .map_err(playlists_error),
            Request::Unlike { items } => self
                .services
                .playlists
                .unlike(&actor, &items)
                .await
                .map(|p| Reply::PlaylistInfo(named(p, sharing_listed, art_listed)))
                .map_err(playlists_error),
            Request::ListShareCandidates {
                id,
                query,
                offset,
                limit,
            } => self
                .services
                .playlists
                .share_candidates(&actor, &id, &query, offset, limit)
                .await
                .map(Reply::ShareCandidates)
                .map_err(playlists_error),
            Request::Search(mut search) => {
                search.limit = search.limit.min(MAX_PAGE);
                let mut results =
                    crate::search::search(catalog.as_ref(), &search, crate::search::this_year())
                        .await
                        .map_err(catalog_error)?;
                let top = match &mut results.top {
                    Some(TopResult::Item(item)) => Some(item),
                    _ => None,
                };
                let items = top
                    .into_iter()
                    .chain(results.groups.iter_mut().flat_map(|g| g.items.iter_mut()))
                    .chain(results.without_year.iter_mut());
                self.fill(principal, items).await?;
                Ok(Reply::Search(results))
            }
            Request::RecentlyPlayed {
                kinds,
                offset,
                limit,
            } => {
                let (items, total) = self
                    .services
                    .watch
                    .recently_played(principal, kinds, offset, limit.min(MAX_PAGE))
                    .await
                    .map_err(watch_error)?;
                Ok(Reply::Items { items, total })
            }
            Request::PersonItems {
                person,
                offset,
                limit,
            } => {
                // A person is in few items: the catalog lists them all,
                // and the page is cut here.
                let all = catalog.person_items(&person).await.map_err(catalog_error)?;
                let total = u32::try_from(all.len()).unwrap_or(u32::MAX);
                let mut items: Vec<_> = all
                    .into_iter()
                    .skip(usize::try_from(offset).unwrap_or(usize::MAX))
                    .take(usize::try_from(limit.min(MAX_PAGE)).unwrap_or(usize::MAX))
                    .collect();
                self.fill(principal, items.iter_mut()).await?;
                Ok(Reply::Items { items, total })
            }
            Request::UpdatePlaybackProgress {
                handle,
                sequence,
                position_ms,
            } => {
                // Alive, not played: a paused client reports on (once a
                // minute, PROTOCOL.md) and must not hold its slot against
                // its own device's next start.
                let item = self
                    .use_playback_with(connection, &handle, &session, Using::Reports, |p| {
                        p.item.clone()
                    })
                    .await?;
                let _progress = self
                    .services
                    .playbacks
                    .progress_control(&session, &item)
                    .await;
                self.services.playbacks.check_progress(&handle, sequence)?;
                let position = Duration::from_millis(position_ms);
                // Its observers first: they need no catalog, so a Plex that
                // is down does not stop them.
                self.services.playbacks.progressed(&handle, position);
                // A track has no watch state, only its plays (PX-52).
                if crate::watch::tracked(&item) {
                    self.services
                        .watch
                        .progress(principal, &item, position)
                        .await
                        .map_err(watch_error)?;
                }
                self.services
                    .playbacks
                    .record_progress(&handle, sequence, position_ms);
                Ok(Reply::Done)
            }
            Request::MarkWatched { id } => {
                self.services
                    .watch
                    .mark(principal, &id, true)
                    .await
                    .map_err(watch_error)?;
                Ok(Reply::Done)
            }
            Request::MarkUnwatched { id } => {
                self.services
                    .watch
                    .mark(principal, &id, false)
                    .await
                    .map_err(watch_error)?;
                Ok(Reply::Done)
            }
            Request::HideFromContinueWatching { id } => {
                self.services
                    .watch
                    .hide(principal, &id)
                    .await
                    .map_err(watch_error)?;
                Ok(Reply::Done)
            }
            Request::UnhideFromContinueWatching { id } => {
                self.services
                    .watch
                    .unhide(principal, &id)
                    .await
                    .map_err(watch_error)?;
                Ok(Reply::Done)
            }
            Request::Home => {
                let mut home = self
                    .services
                    .watch
                    .home(principal)
                    .await
                    .map_err(watch_error)?;
                if last_viewed_listed {
                    let mut rows: Vec<&mut spoolframe_types::Item> = home
                        .continue_watching
                        .iter_mut()
                        .chain(home.next_up.iter_mut())
                        .collect();
                    self.services
                        .watch
                        .fill_last_viewed(principal, &mut rows)
                        .await
                        .map_err(watch_error)?;
                }
                Ok(Reply::Home(home))
            }
            Request::Tracks { id } => {
                let (_, descriptor) = self.describe(&id).await?;
                let mut tracks = descriptor.tracks();
                if subtitle_sync_listed {
                    let timings = self.services.subtitle_timings.timings(&descriptor).await;
                    for (track, timing) in tracks.subtitles.iter_mut().zip(timings) {
                        track.timing = timing;
                    }
                }
                if subtitle_search_listed {
                    let downloads = self
                        .services
                        .subtitle_downloads
                        .downloads(&id, &descriptor, &who.actor())
                        .await
                        .map_err(internal)?;
                    for (track, download) in tracks.subtitles.iter_mut().zip(downloads) {
                        track.download = download;
                    }
                }
                Ok(Reply::Tracks(tracks))
            }
            Request::SearchSubtitles { id, language } => self
                .services
                .subtitle_downloads
                .search(&id, language.as_deref())
                .await
                .map(Reply::SubtitleResults)
                .map_err(download_error),
            Request::DownloadSubtitle { id, result } => {
                let asker = crate::subtitle_downloads::Asker {
                    actor: who.actor(),
                    name: who.display_name().to_owned(),
                };
                self.services
                    .subtitle_downloads
                    .download(&id, &result, &asker)
                    .await
                    .map(Reply::SubtitleDownloaded)
                    .map_err(download_error)
            }
            Request::RemoveSubtitleDownload { id, download } => self
                .services
                .subtitle_downloads
                .remove(&id, &download, &who.actor())
                .await
                .map(|()| Reply::Done)
                .map_err(download_error),
            Request::SyncSubtitle { id, track } => {
                let track = self.subtitle_track(&id, track).await?;
                let timings = &self.services.subtitle_timings;
                timings
                    .sync(track)
                    .await
                    .map(Reply::SubtitleTiming)
                    .map_err(timing_error)
            }
            Request::UseOriginalSubtitleTiming { id, track } => {
                let track = self.subtitle_track(&id, track).await?;
                self.services
                    .subtitle_timings
                    .use_original(track)
                    .await
                    .map(Reply::SubtitleTiming)
                    .map_err(timing_error)
            }
            Request::SetSubtitleOffset {
                id,
                track,
                offset_ms,
            } => {
                let track = self.subtitle_track(&id, track).await?;
                self.services
                    .subtitle_timings
                    .set_offset(track, offset_ms, &principal.to_string(), who.display_name())
                    .await
                    .map(Reply::SubtitleTiming)
                    .map_err(timing_error)
            }
            Request::GetPreviews { id } => {
                match self
                    .services
                    .previews
                    .state(&id)
                    .await
                    .map_err(catalog_error)?
                {
                    PreviewState::Ready { key, meta } => {
                        Ok(Reply::Previews(previews::reply(&key, &meta)))
                    }
                    PreviewState::Making { done_percent } => {
                        Err(Error::PreviewsNotReady { done_percent })
                    }
                    PreviewState::None => Err(Error::NoPreviews),
                }
            }
            Request::GetTrackPreferences => Ok(Reply::TrackPreferences(TrackPreferencesInfo {
                preferences: self
                    .store()
                    .track_preferences(principal)
                    .await
                    .map_err(internal)?,
                ocr_offered: !self.services.subtitles.ocr().languages().is_empty(),
            })),
            Request::SetTrackPreferences { preferences } => {
                self.store()
                    .set_track_preferences(principal, &preferences)
                    .await
                    .map_err(internal)?;
                Ok(Reply::Done)
            }
            Request::SetTrackOverride { id, selection } => {
                let (_, descriptor) = self.describe(&id).await?;
                let override_ = TrackOverride::from_selection(&descriptor.tracks(), selection);
                let scope = self.override_scope(&id).await?;
                self.store()
                    .set_track_override(principal, &scope, &override_)
                    .await
                    .map_err(internal)?;
                Ok(Reply::Done)
            }
            Request::ClearTrackOverride { id } => {
                let scope = self.override_scope(&id).await?;
                self.store()
                    .clear_track_override(principal, &scope)
                    .await
                    .map_err(internal)?;
                Ok(Reply::Done)
            }
            Request::PlaybackPlan {
                id,
                selection,
                bitmap,
                display,
            } => {
                // Capabilities belong to the connection: they came with
                // Hello, the screen's over them when the request says
                // (PGPU-06).
                let caps = capabilities(state, display.as_ref());
                if caps.is_none() {
                    // No file needs reading to know the answer.
                    return Ok(Reply::PlaybackPlan {
                        plan: planner::plan_without_capabilities(),
                        chosen: None,
                    });
                }
                let (_, descriptor) = self.describe(&id).await?;
                let (selection, chosen) = self
                    .selection(principal, &id, &descriptor, selection)
                    .await?;
                let bitmap = self.bitmap(principal, bitmap).await?;
                Ok(Reply::PlaybackPlan {
                    plan: planner::plan(
                        &descriptor,
                        caps.as_ref(),
                        selection,
                        bitmap,
                        &crate::encode::planning_encoders(
                            &self.services.jobs,
                            &self.services.transcoding.detected(),
                        ),
                        self.services.subtitles.ocr(),
                    ),
                    chosen,
                })
            }
            Request::StartPlayback {
                id,
                selection,
                start_ms,
                bitmap,
                quality,
                display,
            } => {
                // Its spans link to its `playback` span, a root that ends as
                // this start does, and this request's does too.
                let (trace, span) = crate::playback::PlaybackTrace::begin();
                trace.mark(&tracing::Span::current());
                let started = async {
                    // The start's breakdown is timed from here (PX-24).
                    let requested = std::time::Instant::now();
                    let caps = capabilities(state, display.as_ref());
                    // Off the LAN, this playback takes a share of the uplink
                    // (PX-46), and may be one too many.
                    let wan = (!on_lan(&state.path())).then(|| uplink_class(&who));
                    // The place is held from here: a start that fails on the
                    // way gives it back when `admission` is dropped.
                    let admission = match wan {
                        Some(class) => {
                            Some(self.services.uplink.reserve(class).map_err(|full| {
                                Error::WanStreamsFull {
                                    max: u32::try_from(full.max).unwrap_or(u32::MAX),
                                }
                            })?)
                        }
                        None => None,
                    };
                    let share = admission.as_ref().and_then(|a| a.share());
                    let (path, descriptor) = self.describe(&id).await?;
                    let (selection, chosen) = self
                        .selection(principal, &id, &descriptor, selection)
                        .await?;
                    let bitmap = self.bitmap(principal, bitmap).await?;
                    let planned = self
                        .planned(
                            state,
                            (&path, &descriptor),
                            caps.as_ref(),
                            quality,
                            share,
                            selection,
                            bitmap,
                        )
                        .await;
                    let plan = planned.plan.clone();
                    let planned_in = requested.elapsed();
                    // The item's markers go with the playback (P2-10), so a
                    // player needs no second request for them.
                    let found = catalog.item(&id).await.map_err(catalog_error)?;
                    let credits = spoolframe_policy::watch::credits_to_end(&found.markers);
                    let markers = found.markers;
                    // One length for the Playback page, the play and its
                    // watch state.
                    let duration = crate::watch::length(descriptor.duration, &found.item);
                    let shown = crate::playback::Shown::of(&found.item, descriptor.duration);
                    let viewer = crate::playback::Viewer {
                        user_id: principal.to_string(),
                        username: who.log_name(),
                        display_name: who.display_name().to_owned(),
                        device_name: hello.device_name.clone(),
                        client_name: hello.client_name.clone(),
                        platform: hello.platform,
                    };
                    let handle = Playbacks::new_handle();
                    // A device keeps "now + next": the one playing and this
                    // one, which may be the next track it prepares while that
                    // plays (iOS gapless audio). Its other encodes, not read
                    // lately, are abandoned: free their slots before this one
                    // takes one. One still read is never ended for it.
                    let endpoint = connection.remote_id().to_string();
                    match self
                        .services
                        .playbacks
                        .make_room_for_next(&endpoint, state.number)
                    {
                        Ok(_) => {}
                        Err(crate::playback::DeviceFull) => {
                            tracing::info!(
                                %endpoint,
                                "a device reading all its conversions is refused another"
                            );
                            return Err(Error::Busy);
                        }
                    }
                    let start = start_ms.map(Duration::from_millis);
                    let prepared = match self
                        .prepare(&handle, &id, &planned, &plan, quality, start, None, &trace)
                        .await
                    {
                        // No slot free, and the device holds one it does not
                        // read: what it starts now comes before that, as it did
                        // before "now + next" (one slot, a film started again).
                        // One it reads plays on, and this start is busy.
                        // Another device's slot is never taken.
                        Err(Error::Busy)
                            if self
                                .services
                                .playbacks
                                .end_conversions_on(&endpoint, state.number)
                                > 0 =>
                        {
                            self.prepare(
                                &handle, &id, &planned, &plan, quality, start, None, &trace,
                            )
                            .await
                        }
                        prepared => prepared,
                    };
                    let (subtitle, (size, file), conversion, keyframes) = prepared?;
                    let start_times = Arc::new(parking_lot::Mutex::new(StartTimes {
                        plan: planned_in,
                        keyframes,
                        first_segment: if conversion.is_some() {
                            FirstSegment::Waiting
                        } else {
                            FirstSegment::Direct
                        },
                    }));
                    // The first encode's progress, to time its first segment.
                    let first_run = conversion
                        .as_ref()
                        .map(|c| (c.job().progress(), c.delivery == Delivery::Hls));
                    // Its previews next, if it has none yet: the player asks
                    // for them as soon as it shows a seek bar.
                    self.services.previews.played(&id);
                    let _progress = self
                        .services
                        .playbacks
                        .progress_control(&session, &id)
                        .await;
                    // An episode's next one may be made ahead (PX-37).
                    let binge = match &found.item {
                        spoolframe_types::Item::Episode(episode) => {
                            Some(crate::optimized::ahead::Played::from(episode))
                        }
                        _ => None,
                    };
                    let handle = self.services.playbacks.start(
                        handle,
                        Opened {
                            trace: trace.clone(),
                            viewer,
                            shown,
                            planned: plan.clone(),
                            session,
                            endpoint,
                            item: id.clone(),
                            selection,
                            path,
                            size,
                            file,
                            subtitle,
                            conversion,
                            quality,
                            display,
                            video_size: planned
                                .descriptor
                                .video
                                .as_ref()
                                .map(|v| (v.width, v.height)),
                            source: planned.source,
                            start: start_times.clone(),
                            requested,
                            connection: state.number,
                            played: crate::playback::Played {
                                principal,
                                item: found.item,
                                external_ids: found.external_ids,
                                duration,
                                credits,
                                from: start.unwrap_or_default(),
                            },
                        },
                    );
                    // Counted off the LAN from now (PX-46): the others' shares
                    // follow once starts settle.
                    if let Some(admission) = admission {
                        admission.started(&handle.0, connection.stable_id());
                    }
                    if let (Some(pool), Some(episode), true) =
                        (&self.services.optimized, binge, self.services.binge_ahead)
                    {
                        crate::optimized::ahead::spawn(
                            pool,
                            &self.services.playbacks,
                            self.services.catalog.clone(),
                            handle.clone(),
                            principal.to_string(),
                            episode,
                        );
                    }
                    // What was decided, so a playback that then goes quiet
                    // can be read from the log; the endpoint as the
                    // connection lines print it.
                    let decided = planner::summary(&plan);
                    span.record("playback.plan", decided.kind);
                    span.record(
                        "playback.delivery",
                        tracing::field::display(decided.delivery),
                    );
                    span.record("playback.start_ms", start_ms.unwrap_or(0) as i64);
                    tracing::info!(
                        %id,
                        user = %who.log_name(),
                        endpoint = %connection.remote_id().fmt_short(),
                        plan = %decided.kind,
                        delivery = %decided.delivery,
                        video = %decided.video,
                        audio = %decided.audio,
                        subtitle = %decided.subtitle,
                        start_ms = start_ms.unwrap_or(0),
                        ?quality,
                        path = %state.path(),
                        wan = wan.is_some(),
                        share,
                        "playback started"
                    );
                    // Then how long it took: for direct play now, for a
                    // conversion once its first segment is ready.
                    match first_run {
                        Some((progress, hls)) => {
                            tokio::spawn(crate::playback::time_first_segment(
                                start_times,
                                requested,
                                progress,
                                hls,
                                id.clone(),
                            ));
                        }
                        None => start_times.lock().log(&id),
                    }
                    Ok::<_, Error>(Reply::PlaybackStarted {
                        handle,
                        plan,
                        size,
                        chosen,
                        epoch: 0,
                        markers,
                    })
                }
                .await;
                span.record("outcome", crate::telemetry::outcome(&started));
                started
            }
            Request::ChangeSubtitle {
                handle,
                subtitle,
                bitmap,
                position_ms,
                display,
            } => {
                let playback = self.use_playback(connection, &handle, &session).await?;
                let display = playback.display_for(display);
                let caps = capabilities(state, display.as_ref());
                let quality = playback.quality;
                let share = self.services.uplink.share_of(&handle.0);
                let (path, descriptor) = self.describe(&playback.item).await?;
                let selection = Selection {
                    audio: playback.selection.audio,
                    subtitle,
                };
                let bitmap = self.bitmap(principal, bitmap).await?;
                let planned = self
                    .planned(
                        state,
                        (&path, &descriptor),
                        caps.as_ref(),
                        quality,
                        share,
                        selection,
                        bitmap,
                    )
                    .await;
                let plan = planned.plan.clone();
                let (subtitle, (size, file), conversion, _) = self
                    .prepare(
                        &handle,
                        &playback.item,
                        &planned,
                        &plan,
                        quality,
                        Some(Duration::from_millis(position_ms)),
                        Some(&playback),
                        &playback.traced,
                    )
                    .await?;
                let epoch = conversion.as_ref().map_or(0, |c| c.job().epoch);
                let replaced = self.services.playbacks.change(
                    &handle,
                    Changed {
                        planned: plan.clone(),
                        selection,
                        size,
                        file,
                        subtitle,
                        conversion,
                        source: planned.source,
                        quality: playback.quality,
                        display,
                    },
                );
                drop(replaced);
                tracing::info!(id = %playback.item, epoch, "playback's subtitle changed");
                Ok(Reply::PlaybackChanged { plan, size, epoch })
            }
            Request::ChangeQuality {
                handle,
                quality,
                bitmap,
                position_ms,
                display,
            } => {
                let playback = self.use_playback(connection, &handle, &session).await?;
                let display = playback.display_for(display);
                let caps = capabilities(state, display.as_ref());
                let (path, descriptor) = self.describe(&playback.item).await?;
                let selection = playback.selection;
                let bitmap = self.bitmap(principal, bitmap).await?;
                // Its share now (PX-46): a re-plan after `PlaybackShare`
                // takes the one the event told.
                let share = self.services.uplink.share_of(&handle.0);
                let planned = self
                    .planned(
                        state,
                        (&path, &descriptor),
                        caps.as_ref(),
                        quality,
                        share,
                        selection,
                        bitmap,
                    )
                    .await;
                let plan = planned.plan.clone();
                let epoch_now = playback.conversion.as_ref().map_or(0, |c| c.job().epoch);
                // The same plan of the same file: a copy's plan may read as
                // the film's would (PX-37).
                let same_copy = planned.source.copy_id() == playback.source.copy_id();
                if plan == playback.planned && same_copy {
                    // The same plan: nothing to restart. The quality and
                    // the screen are kept for the next re-plan all the same.
                    self.services
                        .playbacks
                        .set_quality(&handle, quality, display);
                    return Ok(Reply::PlaybackChanged {
                        plan,
                        size: playback.size,
                        epoch: epoch_now,
                    });
                }
                // A plan nothing plays under is `CannotPlay` from here, and
                // the playback goes on as it was, as slow as the link is.
                let (subtitle, (size, file), conversion, _) = self
                    .prepare(
                        &handle,
                        &playback.item,
                        &planned,
                        &plan,
                        quality,
                        Some(Duration::from_millis(position_ms)),
                        Some(&playback),
                        &playback.traced,
                    )
                    .await?;
                let epoch = conversion.as_ref().map_or(0, |c| c.job().epoch);
                let replaced = self.services.playbacks.change(
                    &handle,
                    Changed {
                        planned: plan.clone(),
                        selection,
                        size,
                        file,
                        subtitle,
                        conversion,
                        source: planned.source,
                        quality,
                        display,
                    },
                );
                drop(replaced);
                let decided = planner::summary(&plan);
                tracing::info!(
                    id = %playback.item,
                    epoch,
                    ?quality,
                    plan = %decided.kind,
                    video = %decided.video,
                    "playback planned again for its quality"
                );
                Ok(Reply::PlaybackChanged { plan, size, epoch })
            }
            Request::StopPlayback { handle } => {
                // Its session and endpoint were checked above, with its
                // revision.
                self.services
                    .playbacks
                    .end(&handle, crate::playback::EndReason::Finished);
                Ok(Reply::Done)
            }
            // Converted playback only; served from P2-04 and P2-05.
            Request::SeekPlayback {
                handle,
                position_ms,
            } => {
                let playback = self.use_playback(connection, &handle, &session).await?;
                let Some(conversion) = playback.conversion else {
                    // A direct-play client seeks by reading elsewhere.
                    return Err(Error::NotFound {
                        what: "a conversion in this playback".into(),
                    });
                };
                let epoch = conversion
                    .seek(Duration::from_millis(position_ms))
                    .map_err(conversion_error)?;
                tracing::debug!(epoch, position_ms, "conversion restarted at a seek");
                self.services.playbacks.revised(&handle);
                Ok(Reply::PlaybackSeeked { epoch })
            }
            Request::GetManifest {
                handle,
                kind,
                variant,
            } => {
                let playback = self.use_playback(connection, &handle, &session).await?;
                playback
                    .conversion
                    .and_then(|c| c.manifest(kind, variant))
                    .map(|text| Reply::Manifest { text })
                    .ok_or_else(|| Error::NotFound {
                        what: "an HLS conversion in this playback".into(),
                    })
            }
            Request::ReadSubtitle {
                handle,
                offset,
                version,
            } => {
                let playback = self.use_playback(connection, &handle, &session).await?;
                let (path, format) = match playback.subtitle {
                    None => {
                        return Err(Error::NotFound {
                            what: "an external subtitle in this playback".into(),
                        });
                    }
                    // A playback that began before the track was synced
                    // or offset (PX-03) reads what it is served as now.
                    Some(Sidecar::File { path, format }) => {
                        (self.services.subtitle_timings.resolve(&path).await, format)
                    }
                    Some(Sidecar::Made { state, format }) => (read_text(state).await?, format),
                };
                let (bytes, total, version) = read_subtitle(&path, offset, version).await?;
                self.services.metrics.subtitle_reads.inc();
                Ok(Reply::SubtitleChunk {
                    format,
                    bytes,
                    total,
                    version,
                })
            }
            Request::ReadSubtitleSegment { handle, n } => {
                let read = self
                    .subtitle_segment(connection, &session, &handle, n)
                    .await;
                // As for a segment read (`media_read`). Not ready yet is
                // not a failure: the client asks again.
                if let Err(error) = &read
                    && !matches!(error, Error::SubtitleNotReady { .. })
                {
                    tracing::warn!(
                        id = %self.played(&handle),
                        endpoint = %connection.remote_id().fmt_short(),
                        subtitle_segment = n,
                        %error,
                        "an HLS subtitle segment read failed"
                    );
                }
                if read.is_ok() {
                    self.services.metrics.subtitle_reads.inc();
                }
                read
            }
            // Experimental (PX-49): only when this connection's Hello
            // named it and the port is on.
            Request::HttpPlaybackUrl { handle } => {
                use spoolframe_protocol::experimental::{HTTP_PLAYBACK, HttpPlaybackRefusal};
                let refused = |reason| Ok(Reply::HttpPlaybackRefused { reason });
                let asked = hello.experimental.iter().any(|n| n == HTTP_PLAYBACK);
                let Some(http) = self.http_playback.as_ref().filter(|_| asked) else {
                    return refused(HttpPlaybackRefusal::Off);
                };
                let playback = match self.use_playback(connection, &handle, &session).await {
                    Ok(playback) => playback,
                    Err(
                        Error::PlaybackExpired | Error::PlaybackStopped | Error::Unauthenticated,
                    ) => return refused(HttpPlaybackRefusal::NoSuchPlayback),
                    Err(e) => return Err(e),
                };
                let lan = lan_address(connection, &self.services.uplink);
                match http.url(&handle, &playback, lan) {
                    Ok(url) => {
                        tracing::info!(
                            id = %playback.item,
                            endpoint = %connection.remote_id().fmt_short(),
                            lan = lan.is_some(),
                            "a playback is served over HTTP"
                        );
                        Ok(Reply::HttpPlaybackUrl { url })
                    }
                    Err(reason) => refused(reason),
                }
            }
            Request::LookupDeviceSignIn { code } => {
                let user = local(&who)?;
                let (code, guess) = self.sign_in_code(connection, user, &code)?;
                let waiting = self.services.auth.sign_ins().lookup(&code);
                if waiting.is_some() {
                    self.services.auth.guessed_right(guess);
                }
                match waiting {
                    Some(waiting) => Ok(Reply::DeviceSignInRequest(DeviceSignInRequest {
                        device_name: waiting.device_name,
                        client_name: waiting.client_name,
                        platform: waiting.platform,
                        requested_at: waiting.requested_at,
                        remember: waiting.remember,
                        requested_from: waiting.address.map(crate::device_sign_in::device_address),
                    })),
                    None => Err(wrong_sign_in_code()),
                }
            }
            Request::ApproveDeviceSignIn { code, remember } => {
                let user = local(&who)?;
                recently_signed_in(user, self.services.auth.store().now())?;
                let (code, guess) = self.sign_in_code(connection, user, &code)?;
                let approver = connection.remote_id().to_string();
                let claim = self.services.auth.sign_ins().claim(&code, Some(&approver));
                if !matches!(claim, Err(ClaimError::Unknown)) {
                    self.services.auth.guessed_right(guess);
                }
                let claim = match claim {
                    Ok(claim) => claim,
                    Err(ClaimError::Unknown) => return Err(wrong_sign_in_code()),
                    Err(ClaimError::SameDevice) => {
                        return Err(Error::InvalidInput {
                            message: "approve this device's sign-in on another device".into(),
                        });
                    }
                };
                match self
                    .services
                    .auth
                    .approve_device(user.user, &claim, remember, user.signed_in_with)
                    .await
                {
                    Ok(Approval::SignedIn) => {
                        tracing::info!(
                            user = %user.username,
                            device = %claim.endpoint,
                            remembered = claim.remember && remember,
                            "device sign-in approved"
                        );
                        Ok(Reply::Done)
                    }
                    // The right code, run out while it was approved: not
                    // a guess, so not counted as one.
                    Ok(Approval::Expired) => Err(Error::NotFound {
                        what: "sign-in code".into(),
                    }),
                    Err(e) => Err(internal(e)),
                }
            }
            Request::DenyDeviceSignIn { code } => {
                let user = local(&who)?;
                let (code, guess) = self.sign_in_code(connection, user, &code)?;
                if self.services.auth.sign_ins().deny(&code) {
                    self.services.auth.guessed_right(guess);
                    tracing::info!(user = %user.username, "device sign-in denied");
                    Ok(Reply::Done)
                } else {
                    Err(wrong_sign_in_code())
                }
            }
            // P7-04: each needs a recent sign-in, as every change to how
            // the account is reached does.
            Request::TieBegin => {
                let user = local(&who)?;
                recently_signed_in(user, self.services.auth.store().now())?;
                // A tied account ties more (P7-09): no refusal here.
                let session = SessionToken(session.0.clone()).hash();
                let (nonce, lasts) =
                    self.ties
                        .begin(&session, user.user)
                        .map_err(|_| Error::RateLimited {
                            retry_after_ms: crate::ties::TIE_CHALLENGE_LIFETIME.as_millis() as u64,
                        })?;
                Ok(Reply::TieChallenge {
                    nonce,
                    server: *self.id.as_bytes(),
                    expires_in_secs: lasts.as_secs(),
                })
            }
            Request::TieFinish {
                user: portable,
                proof,
            } => {
                let user = local(&who)?;
                recently_signed_in(user, self.services.auth.store().now())?;
                self.tie(&session, user, portable, &proof).await
            }
            Request::Untie { user: portable } => {
                let user = local(&who)?;
                recently_signed_in(user, self.services.auth.store().now())?;
                // Only this tie: the account's others, and its sessions,
                // those opened with this identity's credential too, stay
                // (P7-09: "the account's rows and sessions as they were").
                if self
                    .store()
                    .untie(user.user, portable)
                    .await
                    .map_err(internal)?
                {
                    tracing::info!(
                        user = %user.username,
                        portable = %portable.fingerprint(),
                        "portable identity untied by its user, in an app"
                    );
                    Ok(Reply::Done)
                } else {
                    Err(Error::NotFound {
                        what: "portable identity tie".into(),
                    })
                }
            }
            // PX-47: a user leaves this server, after a recent sign-in.
            Request::LeaveServer => {
                let now = self.services.auth.store().now();
                match &who {
                    Who::Local(user) => {
                        recently_signed_in(user, now)?;
                        let ended =
                            self.store()
                                .leave_local(user.user)
                                .await
                                .map_err(|e| match e {
                                    crate::store::users::UserError::LastAdmin(_)
                                    | crate::store::users::UserError::LastPasswordAdmin(_) => {
                                        Error::LastAdmin
                                    }
                                    e => internal(e),
                                })?;
                        tracing::info!(
                            user = %user.username,
                            sessions_ended = ended,
                            "user left the server, in an app; an admin can restore them"
                        );
                    }
                    Who::Portable(portable) => {
                        // Its credential, presented again, is a portable
                        // user's recent sign-in: a session opened lately.
                        if now - portable.authenticated_at > RECENT_SIGN_IN.as_secs() as i64 {
                            return Err(Error::RecentSignInRequired);
                        }
                        self.store()
                            .leave_portable(portable.user)
                            .await
                            .map_err(internal)?;
                        tracing::info!(
                            portable = %portable.user.fingerprint(),
                            "portable user left the server, in an app: grant revoked, data deleted"
                        );
                    }
                }
                Ok(Reply::Done)
            }
            // PX-44: the web UI's invite pages, for an admin in an app.
            // The role first, so a consumer is told `AdminRequired`, not
            // asked to confirm; then the same recent confirmation the web
            // UI's `recent_admin` asks for creating and revoking.
            Request::CreateInvite(create) => {
                let user = admin(&who)?;
                recently_signed_in(user, self.services.auth.store().now())?;
                let origin = crate::store::invites::InviteOrigin::Device {
                    endpoint_id: connection.remote_id().to_string(),
                    name: hello.device_name.clone(),
                    platform: hello.platform,
                };
                let request = crate::invites::InviteRequest {
                    kind: invite_kind(create.kind)?,
                    note: create.note.as_deref(),
                    uses: create.uses,
                    expires_secs: create.expires_in_secs,
                    role: create.role,
                };
                let created =
                    crate::invites::create(self.store(), request, &user.username, &origin)
                        .await
                        .map_err(create_invite_error)?;
                let server = crate::invites::server_link(
                    self.id.to_string(),
                    self.name.get(),
                    Some(self.addresses.told.clone()),
                )
                .await;
                let invite = created.invite(server);
                Ok(Reply::InviteCreated(InviteCreated {
                    code: created.code.shown(),
                    link: invite.link(),
                    share_link: invite.share_link(),
                    invite: invite_summary(created.record),
                }))
            }
            Request::ListInvites => {
                admin(&who)?;
                let invites = self.store().live_invites().await.map_err(internal)?;
                Ok(Reply::Invites {
                    invites: invites.into_iter().map(invite_summary).collect(),
                })
            }
            Request::RevokeInvite { id } => {
                let user = admin(&who)?;
                recently_signed_in(user, self.services.auth.store().now())?;
                match self.store().revoke_invite(&id).await {
                    Ok(()) => {
                        tracing::info!(%id, by = %user.username, "invite revoked, in an app");
                        Ok(Reply::Done)
                    }
                    Err(crate::store::invites::InviteError::NoSuchInvite(_)) => {
                        Err(Error::NotFound {
                            what: "live invite".into(),
                        })
                    }
                    Err(e) => Err(internal(e)),
                }
            }
            Request::Hello(_)
            | Request::AuthMethods
            | Request::LoginPassword(_)
            | Request::Ping
            | Request::BeginDeviceSignIn { .. }
            | Request::AwaitDeviceSignIn { .. }
            | Request::BeginPasskeySignIn { .. }
            | Request::FinishPasskeySignIn { .. }
            | Request::TrustedProfiles
            | Request::ResumeProfile { .. }
            | Request::RedeemInvite(_)
            | Request::BeginPortableSignIn
            | Request::FinishPortableSignIn(_)
            | Request::RedeemPortableInvite(_)
            | Request::IdentityRecord { .. }
            | Request::IdentityRecordByHandle { .. }
            | Request::OfferPairing(_)
            | Request::FetchPairing { .. }
            | Request::AnswerPairing { .. }
            | Request::CollectPairing { .. }
            | Request::Unknown => {
                unreachable!("pre-auth and unknown requests are answered before the session check")
            }
        }
    }

    /// A sign-in attempt on `state`'s connection, if it may make one now:
    /// a device that is not signed in draws on the bucket all such devices
    /// share (PX-10); a signed-in one (adding a user) does not. One signed
    /// out is a stranger again while its connection drains (G-14).
    fn stranger_attempt(&self, state: &ConnectionState) -> Result<(), Error> {
        if state.peer.signed_in_locally() {
            return Ok(());
        }
        self.services.auth.stranger_attempt().map_err(rate_limited)
    }

    /// A password login from this connection's endpoint. The session is
    /// bound to that endpoint (P1-12 checks it on every request), and the
    /// endpoint is the source the failure limit counts.
    async fn login_password(
        &self,
        connection: &Connection,
        login: LoginPassword,
        hello: &Hello,
    ) -> Result<Reply, Error> {
        let endpoint_id = connection.remote_id().to_string();
        let binding = endpoint_binding(&endpoint_id, hello);
        match self
            .services
            .auth
            .login_password(
                &login.username,
                &login.password,
                binding,
                Some(&endpoint_id),
                login.remember,
            )
            .await
        {
            Ok(done) => {
                tracing::info!(
                    endpoint = %endpoint_id,
                    user = ?done.user,
                    remembered = done.remembered,
                    password_change_required = done.password_change_required,
                    "logged in"
                );
                Ok(self
                    .logged_in(
                        connection,
                        done.token,
                        done.remembered,
                        done.password_change_required,
                    )
                    .await)
            }
            Err(e) => Err(login_error(e)),
        }
    }

    /// Make an account with an invite (P6-01) from this connection's
    /// endpoint, and a session bound to it, as [`Self::login_password`]
    /// opens: the endpoint is also what its wrong codes are counted as.
    async fn redeem_invite(
        &self,
        connection: &Connection,
        redeem: &RedeemInvite,
        hello: &Hello,
    ) -> Result<Reply, Error> {
        let endpoint_id = connection.remote_id().to_string();
        let (user, role) = crate::invites::redeem(&self.services.auth, &endpoint_id, redeem)
            .await
            .map_err(redeem_error)?;
        let binding = endpoint_binding(&endpoint_id, hello);
        let token = self
            .services
            .auth
            // The redemption chose the account's password.
            .open_session(user, &binding, Some(SignedInWith::Password))
            .await
            .map_err(internal)?;
        tracing::info!(
            endpoint = %endpoint_id,
            %user,
            %role,
            "an account was made with an invite"
        );
        Ok(self.logged_in(connection, token, false, false).await)
    }

    /// A sign-in with an app passkey (P3-19) from this connection's
    /// endpoint: a session bound to it, as [`Self::login_password`] opens.
    async fn finish_passkey_sign_in(
        &self,
        connection: &Connection,
        signed: &FinishPasskeySignIn,
        hello: &Hello,
        remember: bool,
    ) -> Result<Reply, Error> {
        let endpoint_id = connection.remote_id().to_string();
        let user = self
            .services
            .app_passkeys
            .finish_sign_in(&self.services.auth, &endpoint_id, signed)
            .await
            .map_err(sign_in_error)?;
        let binding = endpoint_binding(&endpoint_id, hello);
        let token = self
            .services
            .auth
            .open_session(user.id, &binding, Some(SignedInWith::Passkey))
            .await
            .map_err(internal)?;
        let remembered = self
            .services
            .auth
            .remembered(&token, &binding, user.id, remember)
            .await
            .map_err(internal)?;
        tracing::info!(
            endpoint = %endpoint_id,
            user = %user.id,
            remembered,
            "logged in with an app passkey"
        );
        Ok(self.logged_in(connection, token, remembered, false).await)
    }

    /// A portable sign-in's guess, on its endpoint's own failure count: a
    /// sign-in and an invite redemption share it.
    fn portable_guess(&self, endpoint_id: &str) -> Result<crate::auth::Guess, Error> {
        self.services
            .auth
            .guess(
                format!("portable@{endpoint_id}"),
                Some(endpoint_id.to_owned()),
            )
            .map_err(rate_limited)
    }

    /// A sign-in with a portable user's device credential (P7-06, §58)
    /// from this connection's endpoint: the credential and its device's
    /// proof over the nonce this endpoint began, then the session
    /// [`Self::open_portable`] resolves.
    ///
    /// Counted as a wrong guess before anything is checked, and taken back
    /// only once a session is open (A-13): no grant here is as wrong as a
    /// forged credential. Every refusal is `InvalidCredentials`; why is
    /// logged here.
    async fn portable_sign_in(
        &self,
        connection: &Connection,
        sign_in: &PortableSignIn,
        hello: &Hello,
    ) -> Result<Reply, Error> {
        let endpoint_id = connection.remote_id().to_string();
        let auth = &self.services.auth;
        let guess = self.portable_guess(&endpoint_id)?;
        let credential = self.check_portable(&endpoint_id, sign_in).await?;
        // Kept only for someone this server knows: a stranger's states
        // would fill its disk.
        if self.known_here(credential.user).await? {
            self.keep_states(&credential, &sign_in.states).await?;
        }
        let reply = self.open_portable(connection, &credential, hello).await?;
        auth.guessed_right(guess);
        Ok(reply)
    }

    /// Redeem a `portable-account` invite (P7-05) as the portable user
    /// its sign-in proves, then sign them in as [`Self::portable_sign_in`]
    /// does. The credential is checked first, and counted as a wrong
    /// guess until it holds; the code is counted as `RedeemInvite` counts
    /// it. An identity tied to an account here needs no grant: it signs
    /// in as the account, and the invite keeps its use.
    async fn redeem_portable_invite(
        &self,
        connection: &Connection,
        redeem: &RedeemPortableInvite,
        hello: &Hello,
    ) -> Result<Reply, Error> {
        let endpoint_id = connection.remote_id().to_string();
        let auth = &self.services.auth;
        let guess = self.portable_guess(&endpoint_id)?;
        let credential = self.check_portable(&endpoint_id, &redeem.sign_in).await?;
        let user = credential.user;
        // The credential held: whatever the code makes of it, it was no
        // guess at one.
        auth.guessed_right(guess);
        let tied = self
            .store()
            .tied_account(user)
            .await
            .map_err(internal)?
            .is_some();
        if !tied {
            let redeemed = crate::invites::redeem_portable(
                auth,
                &endpoint_id,
                &redeem.code,
                user,
                redeem.sign_in.display_name.as_deref(),
            )
            .await
            .map_err(redeem_error)?;
            tracing::info!(
                endpoint = %endpoint_id,
                portable = %user.fingerprint(),
                granted = matches!(redeemed, crate::store::invites::PortableRedeemed::Granted),
                "a portable identity redeemed an invite"
            );
        }
        // Known here now: its states are worth keeping.
        self.keep_states(&credential, &redeem.sign_in.states)
            .await?;
        self.open_portable(connection, &credential, hello).await
    }

    /// The credential `sign_in` proves from `endpoint_id`, with the nonce
    /// that endpoint began, used up whatever follows. Its own signature
    /// and dates first; then the device's proof; then whether any state
    /// it carries (its identity's, verified, held in memory only) or any
    /// seen here revokes its key. Nothing is written: [`Self::keep_states`]
    /// keeps the states once the identity is known here.
    async fn check_portable(
        &self,
        endpoint_id: &str,
        sign_in: &PortableSignIn,
    ) -> Result<spoolframe_protocol::portable::UserDeviceCredential, Error> {
        let credential = &sign_in.credential;
        let refused = |why: &dyn std::fmt::Display| {
            tracing::debug!(
                endpoint = %endpoint_id,
                portable = %credential.user.fingerprint(),
                %why,
                "a portable sign-in did not hold"
            );
            Error::InvalidCredentials
        };
        let Some(nonce) = self.portable_sign_ins.take(endpoint_id) else {
            return Err(refused(&"no sign-in begun, or it is over"));
        };
        if sign_in.states.len() > spoolframe_protocol::messages::MAX_SIGN_IN_STATES {
            return Err(refused(&"too many identity states"));
        }
        let now = self.store().clock().unix_secs();
        credential.verify(now).map_err(|e| refused(&e))?;
        for state in &sign_in.states {
            if state.user != credential.user {
                return Err(refused(&"an identity state of another identity"));
            }
            state.verify().map_err(|e| refused(&e))?;
            let revokes = state.revoked.iter().any(|entry| {
                entry.key == credential.device_key
                    && !spoolframe_protocol::portable::revocation_lapsed(entry.revoked_at, now)
            });
            if revokes {
                return Err(refused(&"a state it carries revokes its key"));
            }
        }
        match crate::portable::check_presentation(
            self.store(),
            credential,
            &sign_in.states,
            self.id.as_bytes(),
            &nonce,
            &sign_in.proof,
            now,
        )
        .await
        {
            Ok(_) => Ok(credential.clone()),
            Err(crate::portable::PresentationError::Store(e)) => Err(internal(e)),
            Err(e) => Err(refused(&e)),
        }
    }

    /// What this server keeps of `user` (P7-08): its genesis and newest
    /// state, for a device recovering it. Only an identity known here has
    /// one kept; any other is `NotFound`, the same whether or not a stray
    /// row exists.
    async fn identity_record(
        &self,
        user: spoolframe_protocol::PortableUserId,
    ) -> Result<Reply, Error> {
        if !self.known_here(user).await? {
            return Err(Error::NotFound {
                what: "portable identity".into(),
            });
        }
        let store = self.store();
        Ok(Reply::IdentityRecord(
            spoolframe_protocol::portable::IdentityRecord {
                user,
                genesis: store.identity_genesis(user).await.map_err(internal)?,
                state: store.identity_state(user).await.map_err(internal)?,
            },
        ))
    }

    /// What this server keeps of the identity whose genesis handle is
    /// `handle` (P7-08), as [`Self::identity_record`]: the handle a
    /// security key gives back, for a fresh install with nothing else. A
    /// genesis kept here with that handle names the identity; else, for a
    /// key added to a phrase identity, the handle is the identity's id
    /// (a phrase identity keeps no genesis here). Either way only an
    /// identity known here answers, and a miss is `NotFound`.
    async fn identity_record_by_handle(
        &self,
        handle: &[u8; spoolframe_protocol::portable::HANDLE_LEN],
    ) -> Result<Reply, Error> {
        let store = self.store();
        let user = match store.identity_by_handle(handle).await.map_err(internal)? {
            Some(user) => user,
            None => {
                let user =
                    spoolframe_protocol::PortableUserId::from_bytes(*handle).map_err(|e| {
                        Error::InvalidInput {
                            message: e.to_string(),
                        }
                    })?;
                if store
                    .identity_genesis(user)
                    .await
                    .map_err(internal)?
                    .is_some()
                {
                    // A key identity whose id happens to be this handle:
                    // not the identity the handle names.
                    return Err(Error::NotFound {
                        what: "portable identity".into(),
                    });
                }
                user
            }
        };
        self.identity_record(user).await
    }

    /// Whether `user` is someone this server knows: tied to an account
    /// here, or with a live grant.
    async fn known_here(&self, user: spoolframe_protocol::PortableUserId) -> Result<bool, Error> {
        let store = self.store();
        Ok(store.tied_account(user).await.map_err(internal)?.is_some()
            || store.grant_of(user).await.map_err(internal)?.is_some())
    }

    /// Take in `states`, already checked by [`Self::check_portable`]: a
    /// revocation one brings ends the sessions of the key it names (§48).
    async fn keep_states(
        &self,
        credential: &spoolframe_protocol::portable::UserDeviceCredential,
        states: &[spoolframe_protocol::portable::PortableIdentityState],
    ) -> Result<(), Error> {
        let now = self.store().clock().unix_secs();
        // The genesis first (P7-08): a key identity's states are checked
        // against its roots.
        if let Some(genesis) = credential.genesis.as_deref() {
            match self.store().accept_genesis(genesis, now).await {
                Ok(_) => {}
                Err(crate::store::portable::IdentityStateError::Store(e)) => {
                    return Err(internal(e));
                }
                Err(e) => tracing::debug!(error = %e, "a genesis not kept"),
            }
        }
        for state in states {
            match self.store().accept_identity_state(state, now).await {
                Ok(_) => {}
                Err(crate::store::portable::IdentityStateError::Store(e)) => {
                    return Err(internal(e));
                }
                // Checked before: only a revision past the database's
                // range gets here, and the sign-in stands without it.
                Err(e) => tracing::debug!(error = %e, "an identity state not kept"),
            }
        }
        Ok(())
    }

    /// A session for the portable user `credential` names on this
    /// connection's endpoint, local-first (§58): the account tied to them
    /// here if there is one (and it is not disabled), else their own if
    /// they have a live grant here, else `InvalidCredentials`. Either ends
    /// when the credential does, or its key is revoked; the account's
    /// counts as signed in long ago.
    async fn open_portable(
        &self,
        connection: &Connection,
        credential: &spoolframe_protocol::portable::UserDeviceCredential,
        hello: &Hello,
    ) -> Result<Reply, Error> {
        let endpoint_id = connection.remote_id().to_string();
        let store = self.store();
        let user = credential.user;
        let binding_of = crate::store::grants::CredentialBinding::of(credential);
        let local = store.tied_account(user).await.map_err(internal)?;
        let token = if let Some(local) = local {
            let account = store.user_by_id(local).await.map_err(internal)?;
            if account.is_none_or(|a| a.disabled_at.is_some()) {
                tracing::debug!(
                    endpoint = %endpoint_id,
                    portable = %user.fingerprint(),
                    "a portable sign-in for a disabled account"
                );
                return Err(Error::InvalidCredentials);
            }
            let binding = endpoint_binding(&endpoint_id, hello);
            let token = self
                .services
                .auth
                .open_credential_session(local, &binding, &binding_of)
                .await
                .map_err(internal)?;
            tracing::info!(
                endpoint = %endpoint_id,
                user = %local,
                portable = %user.fingerprint(),
                "logged in with a tied portable credential"
            );
            // P7-09: the tie's last use, which the account's list of ties
            // shows. The session stands without it.
            if let Err(e) = store.tie_used(local, user).await {
                tracing::warn!(error = %e, "a tie's last use not kept");
            }
            token
        } else if store.grant_of(user).await.map_err(internal)?.is_some() {
            let token = self
                .services
                .auth
                .open_portable_session(
                    &binding_of,
                    &endpoint_id,
                    hello.device_name.as_deref(),
                    hello.platform,
                )
                .await
                .map_err(internal)?;
            tracing::info!(
                endpoint = %endpoint_id,
                portable = %user.fingerprint(),
                "a portable user signed in"
            );
            token
        } else {
            tracing::debug!(
                endpoint = %endpoint_id,
                portable = %user.fingerprint(),
                "a portable sign-in with neither a tie nor a grant here"
            );
            return Err(Error::InvalidCredentials);
        };
        if local.is_some() {
            self.signed_in(connection, &token.0).await;
        } else {
            self.guest_signed_in(connection);
        }
        Ok(Reply::LoggedIn {
            session: SessionId(token.0),
            remembered: false,
            password_change_required: false,
            features: features::names(features::ALL),
        })
    }

    /// A trusted device's resume of `user_id` (P4-01): a session bound to
    /// this connection's endpoint, as a sign-in opens, or
    /// `Unauthenticated` for every reason it is not trusted for them.
    async fn resume_profile(
        &self,
        connection: &Connection,
        user_id: &str,
        hello: &Hello,
    ) -> Result<Reply, Error> {
        let endpoint_id = connection.remote_id().to_string();
        let Some(user) = user_id
            .strip_prefix("local:")
            .and_then(|id| id.parse::<crate::store::users::LocalUserId>().ok())
        else {
            return Err(Error::Unauthenticated);
        };
        let binding = endpoint_binding(&endpoint_id, hello);
        match self.services.auth.resume(user, &binding).await {
            Ok(Some(token)) => {
                tracing::info!(endpoint = %endpoint_id, user = %user, "resumed on a trusted device");
                Ok(self.logged_in(connection, token, true, false).await)
            }
            Ok(None) => {
                tracing::info!(endpoint = %endpoint_id, user = %user, "a resume was refused");
                Err(Error::Unauthenticated)
            }
            Err(e) => Err(internal(e)),
        }
    }

    /// Tie `user`'s account to `portable` (P7-04), with `proof` over the
    /// challenge `session` began. The challenge is used up first, whatever
    /// follows: a proof gets one try.
    async fn tie(
        &self,
        session: &SessionId,
        user: &SessionUser,
        portable: spoolframe_protocol::PortableUserId,
        proof: &spoolframe_protocol::TieProof,
    ) -> Result<Reply, Error> {
        let Some(nonce) = self
            .ties
            .take(&SessionToken(session.0.clone()).hash(), user.user)
        else {
            return Err(Error::TieChallengeExpired);
        };
        let now = self.store().clock().unix_secs();
        if let Err(e) = crate::ties::check_tie(
            self.store(),
            proof,
            portable,
            self.id.as_bytes(),
            &nonce,
            &user.username,
            now,
        )
        .await
        {
            if let crate::ties::TieCheckError::Store(e) = e {
                return Err(internal(e));
            }
            // The refusal itself is logged as every one is; why, here.
            tracing::debug!(
                user = %user.username,
                portable = %portable.fingerprint(),
                error = %e,
                "a tie proof did not hold"
            );
            return Err(Error::InvalidCredentials);
        }
        let tied = self
            .store()
            .tie(user.user, portable, proof.kind())
            .await
            .map_err(|e| match e {
                crate::store::ties::TieError::PortableIdTaken => Error::PortableIdTaken,
                crate::store::ties::TieError::Store(e) => internal(e),
            })?;
        // Known here now: a key identity's genesis is kept (P7-08).
        if let Some(genesis) = proof.genesis()
            && let Err(e) = self.store().accept_genesis(genesis, now).await
        {
            tracing::warn!(error = %e, "a tied identity's genesis not kept");
        }
        match tied {
            crate::store::ties::Tied::New => tracing::info!(
                user = %user.username,
                portable = %portable.fingerprint(),
                proof = proof.kind(),
                "portable identity tied, in an app"
            ),
            // P7-09: tied here already, a no-op; not a second tie line.
            crate::store::ties::Tied::Already => tracing::debug!(
                user = %user.username,
                portable = %portable.fingerprint(),
                "portable identity tied again, in an app: nothing changed"
            ),
        }
        Ok(Reply::Done)
    }

    /// `session`'s user proved who they are again, `how`: it counts as
    /// signed in now ([`RECENT_SIGN_IN`]). Only this session.
    async fn confirmed(
        &self,
        session: &SessionId,
        user: &SessionUser,
        how: &str,
    ) -> Result<Reply, Error> {
        self.store()
            .confirm_session(&SessionToken(session.0.clone()).hash())
            .await
            .map_err(internal)?;
        tracing::info!(user = %user.username, how, "confirmed it is them, in an app");
        Ok(Reply::Confirmed)
    }

    /// The account of `user`, a session's.
    async fn account(&self, user: &SessionUser) -> Result<crate::store::users::UserRecord, Error> {
        self.store()
            .user_by_id(user.user)
            .await
            .map_err(internal)?
            .ok_or(Error::Unauthenticated)
    }

    /// `proof` that `user` may change or remove their PIN. The PIN is
    /// checked as at the lock screen and counted with it, but unlocks
    /// nothing (the lock stays as it is); the password as at sign-in.
    async fn check_pin_proof(
        &self,
        connection: &Connection,
        user: &SessionUser,
        proof: PinProof,
    ) -> Result<(), Error> {
        match proof {
            PinProof::Pin { pin } => {
                let endpoint = connection.remote_id().to_string();
                self.services
                    .auth
                    .check_pin(user.user, &endpoint, &pin)
                    .await
                    .map_err(pin_error)
            }
            PinProof::Password { password } => {
                self.confirm_password(connection, user, &password).await
            }
        }
    }

    /// `password` is `user`'s account password, checked and limited as a
    /// sign-in from this connection's endpoint is: a wrong one is a failed
    /// login for their username from here, and nothing else (no session
    /// is revoked, no PIN counted). A user with no password (passkeys or
    /// SSO only) gets `NoPassword` before anything is counted: their
    /// session already says who they are, so that tells no one anything.
    async fn confirm_password(
        &self,
        connection: &Connection,
        user: &SessionUser,
        password: &str,
    ) -> Result<(), Error> {
        let account = self.account(user).await?;
        if !account.has_password {
            return Err(Error::NoPassword);
        }
        let endpoint = connection.remote_id().to_string();
        let checked = self
            .services
            .auth
            .check_password(&account.username, password, Some(&endpoint))
            .await
            .map_err(login_error)?;
        // The username is the session user's own; a rename between the two
        // lookups is not a way to pass with another user's password.
        if checked.id != user.user {
            return Err(Error::InvalidCredentials);
        }
        Ok(())
    }

    /// Change `user`'s password from `change.current_password` (checked
    /// as a sign-in checks it, so a one-time one works here) to
    /// `change.new_password`. Every other session of theirs ends; this
    /// one may do anything again if a one-time password opened it.
    async fn change_password(
        &self,
        connection: &Connection,
        session: &SessionId,
        user: &SessionUser,
        change: &spoolframe_protocol::ChangePassword,
    ) -> Result<(), Error> {
        self.confirm_password(connection, user, &change.current_password)
            .await?;
        if change.new_password == change.current_password {
            return Err(Error::InvalidInput {
                message: "choose a password other than the current one".into(),
            });
        }
        let phc = auth::hash_password(&change.new_password)
            .await
            .map_err(|e| match e {
                auth::PasswordError::TooShort => Error::InvalidInput {
                    message: e.to_string(),
                },
                auth::PasswordError::Hash(_) => internal(e),
            })?;
        let revoked = self
            .store()
            .set_password(
                &user.username,
                &phc,
                Some(&SessionToken(session.0.clone()).hash()),
            )
            .await
            .map_err(internal)?;
        tracing::info!(
            username = %user.username,
            revoked,
            after_reset = user.password_change_only,
            "password changed by its user on a device"
        );
        Ok(())
    }

    /// `typed` as a device sign-in code and the guess it is, if `user` on
    /// this endpoint may try one now. Codes are short, so guessing them is
    /// limited as passwords are: the guess is counted as wrong before it
    /// is checked (A-13), and only [`crate::auth::Auth::guessed_right`]
    /// takes it back. A malformed code is a wrong one.
    fn sign_in_code(
        &self,
        connection: &Connection,
        user: &SessionUser,
        typed: &str,
    ) -> Result<(String, crate::auth::Guess), Error> {
        let (what, from) = sign_in_code_keys(connection, user);
        let guess = self
            .services
            .auth
            .guess(what, Some(from))
            .map_err(rate_limited)?;
        let code = device_sign_in_code(typed).ok_or_else(wrong_sign_in_code)?;
        Ok((code, guess))
    }

    /// A device's last connection closed: end its playbacks, and their
    /// encodes, unless it is back within the grace. A killed app's encode
    /// would otherwise hold a slot until it idles out.
    fn end_playbacks_unless_back(&self, endpoint: EndpointId) {
        let connections = self.connections.clone();
        let playbacks = self.services.playbacks.clone();
        let http_playback = self.http_playback.clone();
        tokio::spawn(async move {
            tokio::time::sleep(playbacks.disconnect_grace()).await;
            if connections.has_endpoint(&endpoint) {
                return;
            }
            // A playback a player fetches over HTTP plays on without its
            // device: a locked phone that AirPlays (PX-49).
            playbacks.end_device_but(&endpoint.to_string(), |handle| {
                http_playback
                    .as_ref()
                    .is_some_and(|http| http.holds(handle))
            });
        });
    }

    fn store(&self) -> &crate::store::Store {
        self.services.auth.store()
    }

    /// `stream` of the file at `path` as `to` text: reading it starts now
    /// (or is joined, or is cached) and the client fetches it when done.
    async fn read_as_text(
        &self,
        path: &Path,
        stream: &SubtitleStream,
        to: SubtitleFormat,
    ) -> Sidecar {
        // A synced or offset subtitle (PX-03) is made from what it is
        // served as.
        let served;
        let stream = match &stream.source {
            SubtitleSource::External { path } => {
                let resolved = self.services.subtitle_timings.resolve(path).await;
                served = SubtitleStream {
                    source: SubtitleSource::External { path: resolved },
                    ..stream.clone()
                };
                &served
            }
            SubtitleSource::Embedded { .. } => stream,
        };
        let state = self
            .services
            .subtitles
            .text(TextSource { path, stream, to })
            .await;
        Sidecar::Made { state, format: to }
    }

    /// The plan for a playback on this connection: off the LAN or not as
    /// its path is now, with `encoders` ([`crate::encode::planning_encoders`])
    /// and OCR as this server has them.
    #[expect(
        clippy::too_many_arguments,
        reason = "the call sites name each part; a struct would only repeat them"
    )]
    fn plan(
        &self,
        state: &ConnectionState,
        descriptor: &spoolframe_media::MediaDescriptor,
        caps: Option<&spoolframe_types::ClientCapabilities>,
        quality: spoolframe_protocol::Quality,
        share: Option<u64>,
        selection: Selection,
        bitmap: planner::Bitmap,
        encoders: &spoolframe_media::encoders::Encoders,
    ) -> PlaybackPlan {
        planner::plan_for(
            descriptor,
            caps,
            quality,
            share,
            !on_lan(&state.path()),
            selection,
            bitmap,
            encoders,
            self.services.subtitles.ocr(),
        )
    }

    /// [`Self::plan`] of `descriptor`, the film at `path`, or of an
    /// optimized copy of it in its place, or with a copy's audio (PX-37,
    /// [`crate::optimized::play`]): what plays, lent for the playback.
    #[expect(
        clippy::too_many_arguments,
        reason = "the call sites name each part; a struct would only repeat them"
    )]
    async fn planned(
        &self,
        state: &ConnectionState,
        (path, descriptor): (&Path, &Arc<MediaDescriptor>),
        caps: Option<&spoolframe_types::ClientCapabilities>,
        quality: spoolframe_protocol::Quality,
        share: Option<u64>,
        selection: Selection,
        bitmap: planner::Bitmap,
    ) -> Planned {
        // Once for the film and every copy planned in its place.
        let encoders = crate::encode::planning_encoders(
            &self.services.jobs,
            &self.services.transcoding.detected(),
        );
        let plan = self.plan(
            state, descriptor, caps, quality, share, selection, bitmap, &encoders,
        );
        let film = |plan| Planned {
            plan,
            film: path.to_path_buf(),
            descriptor: descriptor.clone(),
            source: Source::Film,
        };
        // Only a plan that encodes has anything to gain.
        let Some(pool) = self
            .services
            .optimized
            .as_ref()
            .filter(|_| planner::Kind::of(&plan) == planner::Kind::Transcode)
        else {
            return film(plan);
        };
        let copies = self.ready_copies(pool, path, descriptor, &plan).await;
        if copies.is_empty() {
            return film(plan);
        }
        let described: Vec<&MediaDescriptor> = copies.iter().map(|(_, d)| d).collect();
        let chosen = crate::optimized::play::choose(
            descriptor,
            &plan,
            selection,
            &described,
            |d, selection| self.plan(state, d, caps, quality, share, selection, bitmap, &encoders),
        );
        let (at, chosen_plan, chosen_descriptor, whole) = match chosen {
            crate::optimized::play::Choice::Source => return film(plan),
            crate::optimized::play::Choice::Copy {
                copy,
                plan,
                descriptor,
            } => (copy, plan, descriptor, true),
            crate::optimized::play::Choice::Audio {
                copy,
                plan,
                descriptor,
            } => (copy, plan, descriptor, false),
        };
        let row = &copies[at].0;
        // Gone since it was described (evicted, deleted): the film.
        let lease = match pool.lease(&row.id).await {
            Ok(Some(lease)) => lease,
            Ok(None) => return film(plan),
            Err(e) => {
                tracing::warn!(copy = row.id, error = %e, "cannot lend an optimized copy");
                return film(plan);
            }
        };
        // The film's stream the picked audio is, which a stored track of
        // the copy may stand in for.
        let audio_stream = match &chosen_plan {
            PlaybackPlan::Converted {
                audio: Some(audio), ..
            } => descriptor.audio.get(audio.index as usize).map(|t| t.index),
            _ => None,
        };
        let used = CopyUse::new(lease, &row.target, whole, audio_stream);
        let source = if whole {
            Source::WholeCopy(used)
        } else {
            Source::AudioFrom(used)
        };
        tracing::info!(
            copy = row.id,
            media = %path.display(),
            whole = matches!(source, Source::WholeCopy(_)),
            plan = planner::Kind::of(&chosen_plan).as_str(),
            "a playback plays from an optimized copy"
        );
        Planned {
            plan: chosen_plan,
            film: path.to_path_buf(),
            descriptor: Arc::new(chosen_descriptor),
            source,
        }
    }

    /// The ready copies of the film at `path` (described as `source`) made
    /// from it as it is now that may play `plan` or lend it their audio
    /// ([`crate::optimized::play::worth_describing`]), each described as
    /// the planner sees it: the film with its stored tracks in their
    /// streams' places ([`crate::optimized::play::built`]), each track
    /// read at once. One that cannot be read is passed over, and the film
    /// plays as planned.
    async fn ready_copies(
        &self,
        pool: &crate::optimized::Pool,
        path: &Path,
        source: &MediaDescriptor,
        plan: &PlaybackPlan,
    ) -> Vec<(crate::store::optimized::CopyRow, MediaDescriptor)> {
        let store = self.store();
        let rows = match store.ready_optimized_copies(&path.to_string_lossy()).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(media = %path.display(), error = %e, "cannot list the optimized copies");
                return Vec::new();
            }
        };
        if rows.is_empty() {
            return Vec::new();
        }
        // A copy of the file as it was is not the file now.
        let Ok(stamp) = crate::files::stamp_text(path).await else {
            return Vec::new();
        };
        let described = rows
            .into_iter()
            .filter(|r| {
                r.source_stamp == stamp
                    && crate::optimized::play::worth_describing(plan, source, &r.video)
            })
            .map(|row| async move {
                let files = pool.files_of(&row.tracks);
                let id = &row.id;
                let describe = |file: PathBuf| async move {
                    match self.services.prober.describe(&file).await {
                        Ok(described) => Some(described),
                        Err(e) => {
                            tracing::warn!(copy = id, error = %e, "cannot describe an optimized copy's track");
                            None
                        }
                    }
                };
                let video = match files.video.clone() {
                    Some(file) => Some(describe(file).await?),
                    None => None,
                };
                let mut audio = Vec::new();
                for (stream, file) in &files.audio {
                    audio.push((*stream, describe(file.clone()).await?));
                }
                let audio: Vec<(u32, &MediaDescriptor)> =
                    audio.iter().map(|(s, d)| (*s, &**d)).collect();
                let built = crate::optimized::play::built(source, video.as_deref(), &audio)?;
                Some((row, built))
            });
        futures_util::future::join_all(described)
            .await
            .into_iter()
            .flatten()
            .collect()
    }

    /// What `plan` of `planned` needs to play as `handle`: the subtitle the client
    /// fetches beside it, the size of what it reads (0 while a conversion
    /// writes it) with the file read for direct play, the conversion,
    /// started at `start`, and how long its keyframes took to read and
    /// where from, if it needed them (PX-24). `replaces` is the playback as it was, when this
    /// is a subtitle change: direct play goes on reading the file it began
    /// with, or none. `trace` is the playback's, which its conversion's
    /// spans link to.
    #[expect(
        clippy::too_many_arguments,
        reason = "the call sites name each part; a struct would only repeat them"
    )]
    async fn prepare(
        &self,
        handle: &PlaybackHandle,
        id: &MediaId,
        planned: &Planned,
        plan: &PlaybackPlan,
        quality: spoolframe_protocol::Quality,
        start: Option<Duration>,
        replaces: Option<&Playback>,
        trace: &crate::playback::PlaybackTrace,
    ) -> Result<
        (
            Option<Sidecar>,
            (u64, Option<FileId>),
            Option<Arc<Conversion>>,
            Option<(Duration, KeyframesFrom)>,
        ),
        Error,
    > {
        let (path, descriptor) = (planned.read_path(), &planned.descriptor);
        let video_from = planned.source.video_from();
        let audio_from = planned.source.audio_from();
        Ok(match plan {
            PlaybackPlan::Direct { subtitle, .. } => {
                let subtitle = match subtitle {
                    Some(planned) => {
                        let stream = &descriptor.subtitles[planned.index as usize];
                        // Planned as another format than its own: read
                        // as text (P2-07).
                        if planned.format != codecs::subtitle_format(&stream.codec) {
                            Some(self.read_as_text(path, stream, planned.format).await)
                        } else if let SubtitleSource::External { path } = &stream.source {
                            Some(Sidecar::File {
                                // Synced or offset (PX-03), what it is
                                // served as.
                                path: self.services.subtitle_timings.resolve(path).await,
                                format: planned.format,
                            })
                        } else {
                            None
                        }
                    }
                    None => None,
                };
                let metadata = tokio::fs::metadata(path).await.map_err(|e| {
                    tracing::warn!(%id, error = %e, "cannot stat a media file");
                    Error::Unavailable {
                        message: "the media file could not be read".into(),
                    }
                })?;
                let file = FileId::of(&metadata);
                // The file it began with, if it reads the same one: a
                // re-plan may move it to or from an optimized copy (PX-37).
                if replaces
                    .filter(|p| p.read_path() == path)
                    .and_then(|p| p.file)
                    .is_some_and(|began| began != file)
                {
                    tracing::warn!(%id, "a media file was replaced during its playback");
                    return Err(Error::Unavailable {
                        message: "the media file changed during playback".into(),
                    });
                }
                (subtitle, (metadata.len(), Some(file)), None, None)
            }
            PlaybackPlan::Converted {
                delivery,
                video,
                audio,
                subtitle,
                ..
            } => {
                // Frame previews made ahead wait while the playback is
                // prepared (PX-25): its subtitle read, its keyframes read
                // (a whole file, when its index will not do), until its
                // conversion, which counts itself, has started.
                let _preparing = self.services.jobs.encoding().begin();
                // Text the client fetches beside the stream: read from
                // pictures; or, for HLS, any text, as the WebVTT of
                // its subtitle rendition (P2-07).
                let text = match (delivery, subtitle) {
                    (
                        Delivery::Hls,
                        Some(ConvertedSubtitle {
                            index,
                            action:
                                SubtitleAction::Copy { .. }
                                | SubtitleAction::Convert { .. }
                                | SubtitleAction::Ocr { .. },
                        }),
                    ) => Some(
                        self.read_as_text(
                            path,
                            &descriptor.subtitles[*index as usize],
                            SubtitleFormat::WebVtt,
                        )
                        .await,
                    ),
                    (
                        _,
                        Some(ConvertedSubtitle {
                            index,
                            action: SubtitleAction::Ocr { to, .. },
                        }),
                    ) => Some(
                        self.read_as_text(path, &descriptor.subtitles[*index as usize], *to)
                            .await,
                    ),
                    _ => None,
                };
                // HLS of a copied video is cut at its keyframes: from the
                // file's index where it lists them all (PX-25).
                let mut keyframes_read = None;
                let keyframes = match (delivery, video) {
                    (Delivery::Hls, Some(ConvertedVideo::Copy { .. })) => {
                        let reading = std::time::Instant::now();
                        let (keyframes, from) = self
                            .services
                            .prober
                            .keyframes_from(video_from.unwrap_or(path))
                            .await
                            .map_err(|e| {
                                tracing::warn!(%id, error = %e, "cannot list keyframes");
                                Error::PlaybackFailed {
                                    reason: "the file's keyframes could not be read".into(),
                                }
                            })?;
                        tracing::debug!(%id, ?from, count = keyframes.len(), "keyframes");
                        keyframes_read = Some((reading.elapsed(), KeyframesFrom::of(&from)));
                        Some(keyframes)
                    }
                    _ => None,
                };
                // An AC-3 or E-AC-3 track in fMP4 needs a packet before
                // the first cut: where its first one is, read up to it. A
                // read that fails leaves the conversion as planned.
                let audio_first_ms = match (delivery, audio) {
                    (Delivery::Hls, Some(a))
                        if crate::encode::sample_entry_from_packets(a.action) =>
                    {
                        match descriptor.audio.get(a.index as usize) {
                            Some(stream) => self
                                .services
                                .prober
                                .first_packet_ms(audio_from.unwrap_or(path), stream.index)
                                .await
                                .inspect_err(|e| {
                                    tracing::warn!(%id, error = %e, "cannot read where the audio starts");
                                })
                                .ok()
                                .flatten(),
                            None => None,
                        }
                    }
                    _ => None,
                };
                // A new conversion's first epoch is past the old one's, so
                // reads of the old one are told to start over. The old one
                // runs until the new one has started, so the new one takes
                // no slot of its own: it shares the old one's, or, weighing
                // more or less (a subtitle burned in, or no longer), counts
                // the old one's share as free.
                let replaced = replaces
                    .and_then(|p| p.conversion.as_ref())
                    .map(|c| c.job());
                let first_epoch = match replaces {
                    Some(_) => replaced.as_ref().map_or(0, |job| job.epoch) + 1,
                    None => 0,
                };
                let transcoding = &self.services.transcoding;
                // A subtitle file carried into the conversion is carried as
                // it is served: synced or offset (PX-03), a download synced
                // (PX-02).
                let carried = match subtitle {
                    Some(ConvertedSubtitle { index, .. }) => self.served(descriptor, *index).await,
                    None => descriptor.clone(),
                };
                let conversion = Conversion::start(
                    self.services.jobs.clone(),
                    ConversionSource {
                        handle: &handle.0,
                        path,
                        video_from,
                        audio_from,
                        descriptor: carried,
                        plan,
                        gpus: transcoding.detected(),
                        keyframes,
                        audio_first_ms,
                        first_epoch,
                        slot: replaced.and_then(|job| job.slot()),
                        start_rung: planner::start_rung(plan, quality),
                        trace: trace.clone(),
                    },
                    start,
                )
                .map_err(|e| {
                    if !matches!(e, JobError::Busy) {
                        tracing::warn!(%id, error = %e, "cannot start a conversion");
                    }
                    conversion_error(e)
                })?;
                (text, (0, None), Some(conversion), keyframes_read)
            }
            PlaybackPlan::Unsupported { reasons } => {
                return Err(Error::CannotPlay {
                    reasons: reasons.clone(),
                });
            }
            other => {
                return Err(Error::CannotPlay {
                    reasons: vec![format!("{other:?} is not served yet")],
                });
            }
        })
    }

    /// `id`'s media file and Spoolframe's description of it.
    async fn describe(&self, id: &MediaId) -> Result<(PathBuf, Arc<MediaDescriptor>), Error> {
        let path = self
            .services
            .catalog
            .media_path(id)
            .await
            .map_err(catalog_error)?;
        let descriptor = self.services.prober.describe(&path).await.map_err(|e| {
            tracing::warn!(%id, error = %e, "cannot describe a media file");
            Error::Unavailable {
                message: "the media file could not be read".into(),
            }
        })?;
        Ok((path, descriptor))
    }

    /// `descriptor` with its subtitle `index`, when a file, as what it is
    /// served as (PX-03): its synced or offset copy, when it has one.
    async fn served(&self, descriptor: &Arc<MediaDescriptor>, index: u32) -> Arc<MediaDescriptor> {
        let Some(SubtitleStream {
            source: SubtitleSource::External { path },
            ..
        }) = descriptor.subtitles.get(index as usize)
        else {
            return descriptor.clone();
        };
        let resolved = self.services.subtitle_timings.resolve(path).await;
        if &resolved == path {
            return descriptor.clone();
        }
        let mut served = (**descriptor).clone();
        served.subtitles[index as usize].source = SubtitleSource::External { path: resolved };
        Arc::new(served)
    }

    /// Subtitle `track` of item `id`, to retime (PX-03).
    async fn subtitle_track(
        &self,
        id: &MediaId,
        track: u32,
    ) -> Result<crate::subtitle_sync::TrackRef, Error> {
        let (media, descriptor) = self.describe(id).await?;
        Ok(crate::subtitle_sync::TrackRef {
            id: id.clone(),
            media,
            descriptor,
            index: track,
        })
    }

    /// The tracks to plan with: the client's, or, when it sent none, the
    /// ones the principal's preferences and override choose (P1-20).
    async fn selection(
        &self,
        principal: Principal,
        id: &MediaId,
        descriptor: &MediaDescriptor,
        selection: Option<Selection>,
    ) -> Result<(Selection, Option<ChosenTracks>), Error> {
        if let Some(selection) = selection {
            return Ok((selection, None));
        }
        let store = self.store();
        let preferences = store.track_preferences(principal).await.map_err(internal)?;
        let scope = self.override_scope(id).await?;
        let override_ = store
            .track_override(principal, &scope)
            .await
            .map_err(internal)?;
        let chosen = tracks::choose(&descriptor.tracks(), &preferences, override_.as_ref());
        Ok((chosen.selection, Some(chosen)))
    }

    /// How a picked bitmap subtitle is shown: as the client asked, or, for
    /// `Auto`, as the principal's preferences say.
    async fn bitmap(
        &self,
        principal: Principal,
        requested: BitmapSubtitles,
    ) -> Result<planner::Bitmap, Error> {
        if let Some(asked) = planner::Bitmap::asked(requested) {
            return Ok(asked);
        }
        let preferences = self
            .store()
            .track_preferences(principal)
            .await
            .map_err(internal)?;
        Ok(planner::Bitmap::Preferred(preferences.subtitles.bitmap))
    }

    /// What a track override for `id` applies to: its show for an episode,
    /// so a pick carries to the rest of the series; else `id` itself.
    async fn override_scope(&self, id: &MediaId) -> Result<String, Error> {
        tracks::override_scope(self.services.catalog.as_ref(), id)
            .await
            .map_err(catalog_error)
    }

    /// A sign-in done: the connection told it is signed in, and the reply
    /// that hands the device its session.
    async fn logged_in(
        &self,
        connection: &Connection,
        token: SessionToken,
        remembered: bool,
        password_change_required: bool,
    ) -> Reply {
        self.signed_in(connection, &token.0).await;
        Reply::LoggedIn {
            session: SessionId(token.0),
            remembered,
            password_change_required,
            features: features::names(features::ALL),
        }
    }

    /// `connection`'s device signed in: each of its connections gets the
    /// normal limits and is told the server's addresses.
    async fn signed_in(&self, connection: &Connection, session: &str) {
        // Whose session it is, for the log when the connection closes.
        let user = match self.services.auth.session_owner(session).await {
            Ok(owner) => owner.map(|owner| owner.username),
            Err(e) => {
                tracing::debug!(error = %e, "cannot look up whose session it is");
                None
            }
        };
        for id in self
            .connections
            .signed_in(&connection.remote_id(), user.as_deref(), true)
        {
            self.addresses.tell(id);
        }
    }

    /// `connection`'s device signed in as a portable guest (P7-06): the
    /// normal limits and the server's addresses, as after a sign-in, but
    /// its sign-ins stay on the strangers' budget.
    fn guest_signed_in(&self, connection: &Connection) {
        for id in self
            .connections
            .signed_in(&connection.remote_id(), None, false)
        {
            self.addresses.tell(id);
        }
    }

    /// Open the server→client events stream for `connection`.
    async fn open_events(&self, connection: &Connection) {
        let opened = async {
            let mut stream = connection.open_uni().await.map_err(std::io::Error::other)?;
            stream.write_u8(uni::EVENTS).await?;
            Ok::<_, std::io::Error>(stream)
        };
        // A client whose transport allows no stream of the server's own
        // would hold its Hello's answer for the connection's life: it is
        // given what a first frame is, then goes without events.
        let within = self.connections.admission().first_frame;
        match tokio::time::timeout(within, opened).await {
            Ok(Ok(stream)) => {
                let id = connection.stable_id();
                self.addresses.streams.insert(id, stream);
                // Closed while the stream opened: the accept loop's
                // remove() may have run before this insert.
                if connection.close_reason().is_some() {
                    self.addresses.streams.remove(id);
                }
            }
            Ok(Err(e)) => tracing::debug!(error = %e, "cannot open the events stream"),
            Err(_) => tracing::debug!(
                ?within,
                "the client opened no stream for the server's events in time; it gets none"
            ),
        }
    }
}

impl ProtocolHandler for ClientProtocol {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let id = connection.stable_id();
        // Named by its key in the log: who is signed in on it is a matter
        // of each request's session, not of the connection (P1-12).
        let endpoint = connection.remote_id();
        // A device with a live session gets the normal limits; any other
        // is a stranger, held only while there is room for one (PX-10).
        // A portable guest's session (P7-06) gives the limits, not a
        // local user's standing with the strangers' budget.
        // One store query for both (CR-11 L17): every connection asks it
        // before the strangers' cap is checked. A store error makes it a
        // stranger, which fails safe.
        let id_text = connection.remote_id().to_string();
        let (known, local) = match self.store().endpoint_sessions(&id_text).await {
            Ok(found) => (found.any(), found.local),
            Err(e) => {
                tracing::warn!(error = %e, "cannot look the device's sessions up");
                (false, false)
            }
        };
        let Some(peer) = self.connections.admit(&connection, known, local) else {
            tracing::info!(
                endpoint = %endpoint.fmt_short(),
                "refusing a connection: too many devices that are not signed in"
            );
            connection.close(
                VarInt::from_u32(close::BUSY),
                b"too many devices that are not signed in; try again later",
            );
            // Stay until the close is delivered, or the peer never sees why.
            let _ = tokio::time::timeout(SHUTDOWN_NOTICE, connection.closed()).await;
            return Ok(());
        };
        // Every connection starts anonymous, and most that stay so are an
        // app checking in: INFO is for the sign-in, and the disconnect.
        tracing::debug!(endpoint = %endpoint.fmt_short(), known, "client connected");
        let state = Arc::new(ConnectionState {
            number: self.services.playbacks.connection_opened(),
            peer,
            hello: OnceLock::new(),
            locks: parking_lot::Mutex::default(),
            paths: parking_lot::Mutex::new(
                connection
                    .paths()
                    .iter()
                    .find(|p| p.is_selected())
                    .map(|p| path_name(p.remote_addr(), &self.services.uplink))
                    .into_iter()
                    .collect(),
            ),
            switches: std::sync::atomic::AtomicU64::new(0),
            path_log: parking_lot::Mutex::default(),
            uplink: self.services.uplink.clone(),
            counted: parking_lot::Mutex::default(),
            punch: parking_lot::Mutex::default(),
            metrics: self.services.metrics.clone(),
        });
        if let Some(path) = state.paths.lock().last() {
            state.path_is(path);
        }
        tokio::spawn(log_paths(
            connection.weak_handle(),
            connection.path_events(),
            Arc::downgrade(&state),
        ));
        tokio::spawn(watch_link(
            connection.weak_handle(),
            connection.remote_id().to_string(),
            Arc::downgrade(&state),
            self.services.playbacks.clone(),
        ));

        let reason = loop {
            match connection.accept_bi().await {
                Ok((send, recv)) => {
                    let this = self.clone();
                    let connection = connection.clone();
                    let state = state.clone();
                    tokio::spawn(async move { this.stream(connection, state, send, recv).await });
                }
                Err(reason) => break reason,
            }
        };

        // What it carried since the last count: the rest of its bytes.
        state.count_link(&connection, false);
        self.addresses.streams.remove(id);
        self.playlist_listeners.remove(id);
        self.subtitle_listeners.remove(id);
        self.connections.remove(id);
        self.services.connected.closed(id);
        self.services.playbacks.connection_closed(state.number);
        let signed_in = state.peer.was_signed_in();
        if signed_in || !hung_up(&reason) {
            tracing::info!(
                endpoint = %endpoint.fmt_short(),
                users = %state.peer.users(),
                %reason,
                signed_in,
                paths = %state.paths(),
                "client disconnected"
            );
        } else {
            tracing::debug!(
                endpoint = %endpoint.fmt_short(),
                %reason,
                signed_in,
                paths = %state.paths(),
                "client disconnected"
            );
        }
        self.end_playbacks_unless_back(connection.remote_id());
        Ok(())
    }

    /// Tell every client the server is going away, before the router
    /// closes the connections.
    async fn shutdown(&self) {
        self.services.previews.stop();
        self.services.index_ahead.stop();
        let notices = self
            .addresses
            .streams
            .drain()
            .into_iter()
            .map(|stream| async move {
                let mut stream = stream.lock().await;
                write_frame(&mut *stream, &Event::ShuttingDown).await?;
                stream.finish().map_err(std::io::Error::other)?;
                // Wait for the client to acknowledge it; closing the
                // connection first could drop the frame.
                let _ = stream.stopped().await;
                Ok::<_, codec::CodecError>(())
            });
        deliver(notices).await;
    }
}

/// Whether a connection ended as an app ends one that is done: closed by
/// the app, with no error code. Anything else (an error code, a timeout,
/// a reset, the server closing it for a sign-out or a shutdown) is worth
/// INFO, signed in or not.
fn hung_up(reason: &iroh::endpoint::ConnectionError) -> bool {
    matches!(
        reason,
        iroh::endpoint::ConnectionError::ApplicationClosed(close)
            if close.error_code == VarInt::from_u32(0)
    )
}

/// Deliver every client's `ShuttingDown` notice, all at once, within
/// [`SHUTDOWN_NOTICE`] together: one at a time, each client that did not
/// take its notice held the shutdown up for another 500 ms (C-13).
async fn deliver<F>(notices: impl IntoIterator<Item = F>)
where
    F: Future<Output = Result<(), codec::CodecError>> + Send + 'static,
{
    let mut pending: tokio::task::JoinSet<_> = notices.into_iter().collect();
    let all = async {
        while let Some(delivered) = pending.join_next().await {
            if let Ok(Err(e)) = delivered {
                tracing::debug!(error = %e, "cannot send ShuttingDown");
            }
        }
    };
    if tokio::time::timeout(SHUTDOWN_NOTICE, all).await.is_err() {
        // Dropping the set gives up on the rest.
        tracing::debug!(
            clients = pending.len(),
            "clients did not take ShuttingDown in time"
        );
    }
}

/// The server's own address on `connection`'s path now, when that path is
/// on the LAN (PX-49): what an HTTP playback URL names, so a receiver on
/// the client's network reaches the server as the client did. `None`
/// through a relay or over the Internet.
fn lan_address(
    connection: &Connection,
    uplink: &crate::uplink::Uplink,
) -> Option<std::net::IpAddr> {
    let paths = connection.paths();
    let path = paths.iter().find(|path| path.is_selected())?;
    let iroh::TransportAddr::Ip(remote) = path.remote_addr() else {
        return None;
    };
    if !uplink.is_lan(remote.ip()) {
        return None;
    }
    match path.local_addr() {
        iroh::endpoint::LocalTransportAddr::Ip(Some(ip)) if !ip.is_unspecified() => {
            Some(ip.to_canonical())
        }
        // Not known on this host: the address the kernel would send to the
        // client from.
        _ => route_from(*remote),
    }
}

/// This host's address that packets to `remote` leave from. Nothing is
/// sent: connecting a UDP socket only picks the route.
fn route_from(remote: std::net::SocketAddr) -> Option<std::net::IpAddr> {
    let any: std::net::SocketAddr = if remote.is_ipv4() {
        (std::net::Ipv4Addr::UNSPECIFIED, 0).into()
    } else {
        (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
    };
    let socket = std::net::UdpSocket::bind(any).ok()?;
    socket.connect(remote).ok()?;
    let ip = socket.local_addr().ok()?.ip().to_canonical();
    (!ip.is_unspecified()).then_some(ip)
}

/// The address `connection` reaches the server from now, if it is an IP
/// path; `None` through a relay (P6-03), which hides it.
fn remote_ip(connection: &Connection) -> Option<std::net::IpAddr> {
    connection
        .paths()
        .iter()
        .find(|path| path.is_selected())
        .and_then(|path| match path.remote_addr() {
            iroh::TransportAddr::Ip(addr) => Some(addr.ip()),
            _ => None,
        })
}

/// The size of each part of a resource sent: what one write waits on.
const RESOURCE_PART: usize = 64 << 10;

/// The size of each read of a file a resource is copied from.
const READ_PART: usize = 1 << 20;

/// `bytes` written to `send`, unless the reader takes none of a part for
/// `stall` (PX-09 finding 14): then an error, and the stream is reset.
async fn send_unless_stalled(
    send: &mut SendStream,
    bytes: &[u8],
    stall: Duration,
) -> std::io::Result<()> {
    for part in bytes.chunks(RESOURCE_PART) {
        tokio::time::timeout(stall, send.write_all(part))
            .await
            .map_err(|_| stalled(stall))?
            .map_err(std::io::Error::other)?;
    }
    Ok(())
}

/// `from` copied to `send`, as [`send_unless_stalled`]: a read whose
/// reader takes nothing held its file open, and what waited to be sent,
/// for as long as the connection lived. The bytes copied.
async fn copy_unless_stalled(
    mut from: impl tokio::io::AsyncRead + Unpin,
    send: &mut SendStream,
    stall: Duration,
) -> std::io::Result<u64> {
    // Read in parts of READ_PART: a tokio::fs::File read is a hop to the
    // blocking pool and a copy, so 64 KiB at a time cost 256 of them per
    // 16 MiB. The writes still go RESOURCE_PART at a time, each with its
    // own stall timeout.
    let mut part = vec![0u8; READ_PART];
    let mut copied = 0u64;
    loop {
        let n = from.read(&mut part).await?;
        if n == 0 {
            return Ok(copied);
        }
        send_unless_stalled(send, &part[..n], stall).await?;
        copied += n as u64;
    }
}

fn stalled(stall: Duration) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("the reader took nothing for {stall:?}"),
    )
}

/// Wait, for at most [`SIGN_OUT_DRAIN`], for the peer to acknowledge all
/// of a finished stream: until then the stream counts as being answered,
/// so a signed-out device's connection is not closed under its reply.
async fn delivered(send: &SendStream) {
    let _ = tokio::time::timeout(SIGN_OUT_DRAIN, send.stopped()).await;
}

/// The most items one page or search returns, whatever the client asks.
const MAX_PAGE: u32 = 500;

/// A user's Plex link and their last sync run, as a client sees them: what
/// My profile shows (`Link::shown`), and never the token.
fn plex_link(
    link: Option<Link>,
    (last_run, syncing): (Option<PlexSyncRun>, bool),
) -> Option<PlexLink> {
    let shown = link?.shown();
    Some(PlexLink {
        username: shown.username,
        email: shown.email,
        by_config: shown.by_config,
        linked_at: shown.linked_at,
        last_sync: last_run.map(|run| PlexSyncSummary {
            at: run.at,
            libraries: run.libraries,
            pulled: run.pulled,
            pushed: run.pushed,
            skipped: run.skipped,
            failed: run.failed,
        }),
        syncing,
    })
}

/// An RPC request's frame, read as the request it is: once for each
/// request answered (review 2026-10-12).
fn decode_request(bytes: &[u8]) -> Result<RpcRequest, codec::CodecError> {
    #[cfg(any(test, feature = "test-support"))]
    crate::test_support::RPC_REQUEST_DECODES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    codec::decode(bytes)
}

/// A refused request's frame, as it came in on its stream.
#[derive(Clone, Copy)]
enum Frame<'a> {
    /// An RPC request's frame, and the type it was named by, bounded.
    Rpc(&'a [u8], &'a str),
    Resource(&'a [u8]),
}

/// What a refused `Hello` said of the device, as far as it could be read:
/// a `Hello` from another protocol revision, or one missing a field, may
/// not decode as this build's `Hello`, and still say these.
#[derive(Debug, Default, serde::Deserialize)]
struct SaidHello {
    #[serde(default)]
    protocol_version: Option<u32>,
    #[serde(default)]
    client_name: Option<String>,
    #[serde(default)]
    client_version: Option<String>,
}

impl SaidHello {
    /// The `Hello` in an RPC frame, read leniently.
    fn of(bytes: &[u8]) -> Self {
        #[derive(serde::Deserialize)]
        struct Wire {
            envelope: Envelope,
        }
        #[derive(serde::Deserialize)]
        struct Envelope {
            request: SaidHello,
        }
        codec::decode::<Wire>(bytes)
            .map(|wire| wire.envelope.request)
            .unwrap_or_default()
    }

    /// `client_name client_version`, bounded, if it said both.
    fn client(&self) -> Option<String> {
        let (Some(name), Some(version)) = (&self.client_name, &self.client_version) else {
            return None;
        };
        Some(refusals::bounded(&format!("{name} {version}")))
    }
}

/// What a refused request asked, as far as the refused-request log needs
/// it (finding 46): never a password, PIN or token.
#[derive(Default)]
struct Asked {
    /// Its type, as the wire names it.
    request: String,
    session: Option<SessionId>,
    /// The username a password sign-in gave.
    username: Option<String>,
    /// What a wrong credential in it was.
    credential: Option<&'static str>,
    /// A trusted device's resume (P4-01), which has no session yet.
    resume: bool,
    /// An invite's redemption (P6-01): the username in it is one asked
    /// for, not one tried.
    invite: bool,
}

impl Asked {
    fn of(frame: Frame<'_>) -> Self {
        match frame {
            Frame::Rpc(bytes, name) => Self::rpc(bytes, name),
            Frame::Resource(bytes) => Self::resource(bytes),
        }
    }

    fn rpc(bytes: &[u8], name: &str) -> Self {
        let request = name.to_owned();
        let Ok(wire) = codec::decode::<RpcRequest>(bytes) else {
            return Self {
                request,
                ..Self::default()
            };
        };
        let mut asked = Self {
            request,
            session: wire.envelope.session,
            ..Self::default()
        };
        match &wire.envelope.request {
            Request::LoginPassword(login) => {
                asked.username = Some(refusals::bounded(&login.username));
            }
            Request::UnlockProfile { .. }
            | Request::SetPin {
                proof: Some(PinProof::Pin { .. }),
                ..
            }
            | Request::ClearPin {
                proof: PinProof::Pin { .. },
            } => asked.credential = Some("wrong PIN"),
            Request::ConfirmPassword(_)
            | Request::ChangePassword(_)
            | Request::SetPin {
                proof: Some(PinProof::Password { .. }),
                ..
            }
            | Request::ClearPin {
                proof: PinProof::Password { .. },
            } => asked.credential = Some("wrong password"),
            Request::FinishPasskeySignIn { .. }
            | Request::FinishConfirmPasskey(_)
            | Request::FinishPasskeyRegistration(_) => asked.credential = Some("passkey refused"),
            Request::TieFinish { .. } => asked.credential = Some("tie proof refused"),
            Request::FinishPortableSignIn(_) => {
                asked.credential = Some("portable credential refused");
            }
            Request::ResumeProfile { .. } => asked.resume = true,
            Request::RedeemInvite(_) | Request::RedeemPortableInvite(_) => asked.invite = true,
            _ => {}
        }
        asked
    }

    fn resource(bytes: &[u8]) -> Self {
        let (request, session) = match codec::decode::<ResourceRequest>(bytes) {
            Ok(ResourceRequest::Artwork { request, .. }) => ("artwork", Some(request.session)),
            Ok(ResourceRequest::Media { request, .. }) => ("media", Some(request.session)),
            Ok(ResourceRequest::Subtitle { session, .. }) => ("subtitle", Some(session)),
            Ok(ResourceRequest::SubtitleSegment { session, .. }) => {
                ("subtitle_segment", Some(session))
            }
            Ok(ResourceRequest::SetPlaylistArt { session, .. }) => {
                ("set_playlist_art", Some(session))
            }
            Err(_) => ("resource", None),
        };
        Self {
            request: request.to_owned(),
            session,
            ..Self::default()
        }
    }
}

/// A pairing relay's refusal on the wire (P7-08).
fn relay_error(e: crate::pairing_relay::RelayError) -> Error {
    use crate::pairing_relay::RelayError;
    match e {
        RelayError::NotFound => Error::NotFound {
            what: "pairing request".into(),
        },
        RelayError::Taken | RelayError::WrongAnswer => Error::InvalidInput {
            message: e.to_string(),
        },
        RelayError::Full => Error::RateLimited {
            retry_after_ms: crate::pairing_relay::RELAY_TTL.as_millis() as u64,
        },
    }
}

/// A redemption that made no account, for the client.
fn redeem_error(e: crate::invites::RedeemError) -> Error {
    use crate::invites::RedeemError;
    match e {
        RedeemError::Invalid => Error::InviteInvalid,
        RedeemError::Expired => Error::InviteExpired,
        RedeemError::Exhausted => Error::InviteExhausted,
        RedeemError::UsernameTaken => Error::UsernameTaken,
        RedeemError::WrongKind => Error::WrongInviteKind,
        RedeemError::InvalidInput(message) => Error::InvalidInput { message },
        RedeemError::RateLimited { retry_after } => rate_limited(retry_after),
        RedeemError::Store(e) => internal(e),
        RedeemError::Hash(e) => internal(e),
    }
}

/// A password that did not pass, for the client: the same answers at
/// sign-in and wherever else it is asked for.
fn login_error(e: LoginError) -> Error {
    match e {
        LoginError::InvalidCredentials => Error::InvalidCredentials,
        LoginError::RateLimited { retry_after } => rate_limited(retry_after),
        LoginError::Internal(e) => {
            tracing::error!(error = %e, "login failed");
            Error::Internal
        }
    }
}

/// Whose share of the uplink a playback off the LAN takes (PX-46): an
/// admin's is the owner's.
fn uplink_class(who: &Who) -> crate::uplink::Class {
    match who {
        Who::Local(user) if user.role == spoolframe_types::Role::Admin => {
            crate::uplink::Class::Owner
        }
        _ => crate::uplink::Class::Guest,
    }
}

/// The session's local account, for a request only an account here can
/// make; a portable user's session (P7-06) has none.
fn local(who: &Who) -> Result<&SessionUser, Error> {
    who.local().ok_or(Error::NotForPortableUsers)
}

/// The session's local user, if an admin (PX-44).
fn admin(who: &Who) -> Result<&SessionUser, Error> {
    let user = local(who)?;
    if user.role != spoolframe_types::Role::Admin {
        return Err(Error::AdminRequired);
    }
    Ok(user)
}

/// The kind an app asked for, as the store keeps it; one this build does
/// not know is refused.
fn invite_kind(
    kind: spoolframe_protocol::invite_admin::InviteKind,
) -> Result<crate::store::invites::InviteKind, Error> {
    use crate::store::invites::InviteKind as Stored;
    use spoolframe_protocol::invite_admin::InviteKind as Wire;
    match kind {
        Wire::CreateUser => Ok(Stored::CreateUser),
        Wire::PortableAccount => Ok(Stored::PortableAccount),
        Wire::Federation => Ok(Stored::Federation),
        Wire::Unknown => Err(Error::InvalidInput {
            message: "use create-user, portable-account or federation".into(),
        }),
    }
}

fn create_invite_error(e: crate::invites::CreateError) -> Error {
    use crate::invites::CreateError;
    use crate::store::invites::InviteError;
    match e {
        CreateError::Invalid(message) => Error::InvalidInput { message },
        CreateError::Invite(e @ InviteError::NotYet(_)) => Error::InvalidInput {
            message: e.to_string(),
        },
        CreateError::Invite(e) => internal(e),
    }
}

/// An invite as the apps list it (PX-44): who made it by their display
/// name, and where.
fn invite_summary(record: crate::store::invites::InviteRecord) -> InviteSummary {
    use crate::store::invites::{InviteKind as Stored, InviteOrigin as Origin};
    use spoolframe_protocol::invite_admin::{InviteKind as Wire, InviteOrigin};
    let created_from = match record.origin {
        Origin::Web => InviteOrigin::WebAdmin,
        Origin::Cli => InviteOrigin::Cli,
        Origin::Device {
            endpoint_id,
            name,
            platform,
        } => InviteOrigin::Device {
            endpoint_id,
            name,
            platform,
        },
    };
    // The CLI signs in as no one: its invites say so in `created_by`.
    let created_by = match created_from {
        InviteOrigin::Cli => InviteCreator {
            username: None,
            display_name: record.created_by,
        },
        _ => InviteCreator {
            display_name: record
                .created_by_display_name
                .unwrap_or_else(|| record.created_by.clone()),
            username: Some(record.created_by),
        },
    };
    InviteSummary {
        id: record.id,
        kind: match record.kind {
            Stored::CreateUser => Wire::CreateUser,
            Stored::PortableAccount => Wire::PortableAccount,
            Stored::Federation => Wire::Federation,
        },
        note: record.note,
        role: record.role,
        uses: record.uses,
        uses_left: record.uses_left,
        expires_at: record.expires_at,
        created_at: record.created_at,
        created_by,
        created_from,
    }
}

/// P3-03's rule for what adds or takes away a way to sign in: `user`'s
/// session proved who they are within [`RECENT_SIGN_IN`].
fn recently_signed_in(user: &SessionUser, now: i64) -> Result<(), Error> {
    let age = now - user.authenticated_at;
    if age > RECENT_SIGN_IN.as_secs() as i64 {
        return Err(Error::RecentSignInRequired);
    }
    Ok(())
}

/// A stored passkey as the apps see it. Only an app passkey made for this
/// server's relying party, `app_rp_id`, signs in in an app (PX-14): one
/// made for the relying party every server once shared is an app's that
/// does not. A web UI's is listed so its user sees every way they sign in.
fn passkey_info(p: PasskeyRecord, app_rp_id: &str) -> PasskeyInfo {
    PasskeyInfo {
        app: is_app_rp_id(&p.rp_id),
        usable_here: p.rp_id == app_rp_id,
        id: p.credential_id,
        name: p.name,
        created_at: p.created_at,
        last_used_at: p.last_used_at,
        rp_id: p.rp_id,
    }
}

/// A passkey not renamed or removed, for the client.
fn passkey_error(e: PasskeyError, id: &str) -> Error {
    match e {
        PasskeyError::NoSuchPasskey => Error::NotFound {
            what: format!("passkey {id}"),
        },
        PasskeyError::InvalidName | PasskeyError::Exists => Error::InvalidInput {
            message: e.to_string(),
        },
        PasskeyError::LastSignInMethod => Error::LastSignInMethod,
        PasskeyError::Store(e) => internal(e),
    }
}

/// An app passkey that signed no one in, for the client: as a wrong
/// password is answered. The log has the reason.
fn sign_in_error(e: SignInError) -> Error {
    match e {
        SignInError::Refused => Error::InvalidCredentials,
        SignInError::RateLimited { retry_after } => rate_limited(retry_after),
        SignInError::Unavailable(e) | SignInError::Internal(e) => internal(e),
    }
}

/// `InvalidInput` for a user handle of no bytes: no handle is `None`
/// (P3-19), and the SDK sends it so. Refused before the ceremony, so it
/// uses up no challenge and counts as no failed sign-in: it is a client's
/// bug, not a guess.
fn user_handle_given(signed: &FinishPasskeySignIn) -> Result<(), Error> {
    if signed.user_handle.as_ref().is_some_and(Vec::is_empty) {
        return Err(Error::InvalidInput {
            message: "a passkey with no user handle sends none, not empty bytes".into(),
        });
    }
    Ok(())
}

/// An app passkey that was not kept, for the client.
fn ceremony_error(e: CeremonyError) -> Error {
    match e {
        CeremonyError::NoCeremony | CeremonyError::Refused(_) => {
            tracing::warn!(reason = %e, "app passkey refused");
            Error::InvalidCredentials
        }
        CeremonyError::Passkey(e @ (PasskeyError::InvalidName | PasskeyError::Exists)) => {
            Error::InvalidInput {
                message: e.to_string(),
            }
        }
        CeremonyError::Passkey(e) => internal(e),
        CeremonyError::Unavailable(e) | CeremonyError::Internal(e) => internal(e),
    }
}

/// The session binding of a sign-in on this endpoint, as its Hello names
/// the device.
fn endpoint_binding(endpoint_id: &str, hello: &Hello) -> SessionBinding {
    SessionBinding::Endpoint {
        endpoint_id: endpoint_id.to_owned(),
        device_name: hello.device_name.clone(),
        platform: hello.platform,
    }
}

/// A rate-limit reply for a limiter's `wait`: saturating, and never 0 ms,
/// so a client always backs off.
fn rate_limited(wait: Duration) -> Error {
    Error::RateLimited {
        retry_after_ms: u64::try_from(wait.as_millis()).unwrap_or(u64::MAX).max(1),
    }
}

/// A PIN that did not pass, for the client: the same answers wherever it
/// was entered.
fn pin_error(e: UnlockError) -> Error {
    match e {
        UnlockError::Wrong => Error::InvalidCredentials,
        UnlockError::RateLimited { retry_after } => rate_limited(retry_after),
        UnlockError::Revoked => Error::Unauthenticated,
        UnlockError::Internal(e) => internal(e),
    }
}

/// `handle`'s playback, as `session` on `endpoint` uses it `how`, taking
/// only what `read` reads of it: what [`ClientProtocol::use_playback_with`]
/// does for a request, and the HTTP playback port (PX-49) for the
/// playback's own session and device. The session is checked again at
/// most every `playback::RECHECK`, and whether its item is still served
/// with it (B-10).
pub(crate) async fn use_playback_checked<T>(
    services: &Services,
    handle: &PlaybackHandle,
    session: &SessionId,
    endpoint: &str,
    how: Using,
    read: impl FnOnce(&Playback) -> T,
) -> Result<T, Error> {
    let playbacks = &services.playbacks;
    let using = playbacks
        .use_handle_with(handle, session, endpoint, how, read)
        .map_err(playback_refusal)?;
    if using.recheck {
        let live = services
            .auth
            .endpoint_who(&session.0, endpoint)
            .await
            .map_err(internal)?;
        if live.is_none() {
            playbacks.end_session(session);
            return Err(Error::Unauthenticated);
        }
        // Beside the read, not before it: a Plex that is slow or down
        // must not hold up a file this server reads on its own.
        let catalog = services.catalog.clone();
        let playbacks = playbacks.clone();
        let item = using.item;
        tokio::spawn(async move {
            if let Err(e) = catalog.item(&item).await
                && unserved(&e)
            {
                playbacks.end_item(&item, crate::playback::EndReason::AccessRemoved);
            }
        });
    }
    Ok(using.playback)
}

/// The playback `request` is about, if it is about one.
fn playback_of(request: &Request) -> Option<&PlaybackHandle> {
    match request {
        Request::UpdatePlaybackProgress { handle, .. }
        | Request::ChangeSubtitle { handle, .. }
        | Request::ChangeQuality { handle, .. }
        | Request::SeekPlayback { handle, .. }
        | Request::StopPlayback { handle }
        | Request::PlaybackProgressSequence { handle }
        | Request::PlaybackRevision { handle }
        | Request::GetManifest { handle, .. }
        | Request::ReadSubtitle { handle, .. }
        | Request::ReadSubtitleSegment { handle, .. }
        | Request::HttpPlaybackUrl { handle } => Some(handle),
        _ => None,
    }
}

fn internal(e: impl std::fmt::Display) -> Error {
    tracing::error!(error = %e, "request failed");
    Error::Internal
}

fn playback_refusal(refusal: Refusal) -> Error {
    match refusal {
        Refusal::Expired => Error::PlaybackExpired,
        Refusal::Stopped => Error::PlaybackStopped,
        Refusal::SignedOut | Refusal::NotYours => Error::Unauthenticated,
    }
}

/// How long a read past what an encode has written waits for more.
const READ_WAIT: Duration = spoolframe_protocol::policy::wait::READ;

/// A range of a converted playback's progressive output (P2-04): the bytes
/// written so far, waiting a while for more when the read is past them.
pub(crate) async fn converted_range(
    conversion: &Conversion,
    read: &MediaRead,
    keep_alive: &(dyn Fn() + Sync),
) -> Result<(tokio::fs::File, MediaReadReply), Error> {
    if let Some(n) = read.segment {
        return segment_range(conversion, n, read, keep_alive).await;
    }
    let job = conversion.job();
    if read.epoch != job.epoch {
        return Err(Error::PlaybackRestarted { epoch: job.epoch });
    }
    // Asking for bytes ahead is reading: it may let a paused encode go on.
    job.read_to(read.offset);
    let offset = read.offset;
    // Still nothing after the wait: say so, and the client asks again.
    let now = wait_for(&job, keep_alive, |p| p.bytes > offset || p.done.is_some()).await;
    if let Some(Err(reason)) = &now.done {
        return Err(Error::PlaybackFailed {
            reason: reason.for_client().to_owned(),
        });
    }
    let size = now.bytes;
    let sent = read
        .length
        .min(MAX_READ)
        .min(size.saturating_sub(read.offset));
    job.read_to(read.offset + sent);
    let file = job.dir().join("out.mkv");
    let unreadable = |e: std::io::Error| {
        tracing::warn!(path = %file.display(), error = %e, "cannot read an encode's output");
        Error::PlaybackFailed {
            reason: "the conversion's output could not be read".into(),
        }
    };
    let handle = open_at(&file, read.offset, sent)
        .await
        .map_err(unreadable)?;
    Ok((
        handle,
        MediaReadReply {
            sent,
            size,
            complete: matches!(now.done, Some(Ok(()))),
            epoch: job.epoch,
        },
    ))
}

/// Wait up to [`READ_WAIT`] for `job` to get where `ready` says, keeping
/// the playback alive meanwhile; then how far it has got.
async fn wait_for(
    job: &EncodeJob,
    keep_alive: &(dyn Fn() + Sync),
    ready: impl Fn(&Progress) -> bool,
) -> Progress {
    let mut progress = job.progress();
    // Whether the read waited for the encode, and how long, on its
    // `segment read` span.
    let waited = !ready(&progress.borrow());
    let began = tokio::time::Instant::now();
    let deadline = began + READ_WAIT;
    let now = loop {
        keep_alive();
        let slice = tokio::time::timeout(Duration::from_secs(1), progress.wait_for(&ready)).await;
        match slice {
            Ok(Ok(p)) => break p.clone(),
            Ok(Err(_)) => break job.progress().borrow().clone(),
            Err(_) if tokio::time::Instant::now() >= deadline => {
                break job.progress().borrow().clone();
            }
            Err(_) => {}
        }
    };
    let took = began.elapsed();
    let span = tracing::Span::current();
    span.record("read.waited_for_encode", waited);
    span.record("read.wait_ms", took.as_millis() as i64);
    job.read_waited(waited.then_some(took));
    now
}

/// `file` at `offset`, to send `sent` bytes from. With nothing to send the
/// file may not exist yet; an empty stand-in does.
async fn open_at(file: &Path, offset: u64, sent: u64) -> std::io::Result<tokio::fs::File> {
    if sent == 0 {
        return tokio::fs::File::open("/dev/null").await;
    }
    let mut handle = tokio::fs::File::open(file).await?;
    handle.seek(std::io::SeekFrom::Start(offset)).await?;
    Ok(handle)
}

/// A range of a rung's kept init segment ([`Init::Kept`]): whole, so
/// read at once.
async fn kept_init(
    file: &Path,
    read: &MediaRead,
    epoch: u32,
) -> Result<(tokio::fs::File, MediaReadReply), Error> {
    let unreadable = |e: std::io::Error| {
        tracing::warn!(path = %file.display(), error = %e, "cannot read a kept init segment");
        Error::PlaybackFailed {
            reason: "a segment of the conversion could not be read".into(),
        }
    };
    let mut handle = tokio::fs::File::open(file).await.map_err(unreadable)?;
    let size = handle.metadata().await.map_err(unreadable)?.len();
    let sent = read
        .length
        .min(MAX_READ)
        .min(size.saturating_sub(read.offset));
    handle
        .seek(std::io::SeekFrom::Start(read.offset.min(size)))
        .await
        .map_err(unreadable)?;
    Ok((
        handle,
        MediaReadReply {
            sent,
            size,
            complete: true,
            epoch,
        },
    ))
}

/// A range of HLS segment `n` (P2-05), 0 being the init segment. Segments
/// are addressed by number, not by epoch: a read far from what is written
/// restarts the encode there and waits for it.
async fn segment_range(
    conversion: &Conversion,
    n: u32,
    read: &MediaRead,
    keep_alive: &(dyn Fn() + Sync),
) -> Result<(tokio::fs::File, MediaReadReply), Error> {
    let segments = conversion.segments().ok_or_else(|| Error::NotFound {
        what: "an HLS conversion in this playback".into(),
    })?;
    if n > segments.count() {
        return Err(Error::NotFound {
            what: format!("segment {n}"),
        });
    }
    // Not a failure of the conversion: a rung of a plan replaced since (a
    // ladder planned again as one encode, by `ChangeQuality` or Auto),
    // asked for by a player still on that plan's playlist; or no rung at
    // all. Its media playlist is not found either.
    if !conversion.has_rung(read.variant) {
        return Err(Error::NotFound {
            what: format!("rung {} of this playback's plan", read.variant),
        });
    }
    let job = if n == 0 {
        match conversion
            .init_for(read.variant)
            .map_err(conversion_error)?
        {
            Init::Job(job) => job,
            Init::Kept(file) => return kept_init(&file, read, conversion.job().epoch).await,
        }
    } else {
        conversion
            .job_for_segment(n, read.variant)
            .map_err(conversion_error)?
    };
    // A segment is final once ffmpeg has finished it or the encode has
    // ended; the init segment once the first media segment is finished.
    let wanted = if n == 0 { job.first_segment } else { n };
    // The reader is at the end of the segment it waits for, as far as the
    // throttle is concerned, or the encode pauses just short of finishing
    // it. A read of the init segment does not move a reader back.
    let end = segments.start(wanted + 1).unwrap_or(segments.total_ms);
    if n == 0 {
        job.reader_at_least(wanted, end);
    } else {
        job.reader_at(wanted, end);
    }
    let now = wait_for(&job, keep_alive, |p| {
        p.last_segment.is_some_and(|l| l >= wanted) || p.done.is_some()
    })
    .await;
    if let Some(Err(reason)) = &now.done {
        return Err(Error::PlaybackFailed {
            reason: reason.for_client().to_owned(),
        });
    }
    // Not a segment removed meanwhile to make room: asked for again, the
    // encode restarts there.
    let finished = (now.last_segment.is_some_and(|l| l >= wanted) || now.done.is_some())
        && (n == 0 || n >= job.first_kept());
    let name = if n == 0 {
        "init.mp4".to_owned()
    } else {
        format!("seg{n}.m4s")
    };
    let file = job.dir().join(name);
    let unreadable = |e: std::io::Error| {
        tracing::warn!(path = %file.display(), error = %e, "cannot read a segment");
        Error::PlaybackFailed {
            reason: "a segment of the conversion could not be read".into(),
        }
    };
    // Opened first and measured through the handle: a segment can be
    // removed after the check above (for room, or with all the job's
    // files when the job fails), and an open file stays readable.
    let opened = if finished {
        match tokio::fs::File::open(&file).await {
            Ok(handle) => {
                let size = handle.metadata().await.map_err(unreadable)?.len();
                Some((handle, size))
            }
            // Gone since the check: not ready, as a removed segment is,
            // unless the job has failed by now.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(Err(reason)) = &job.progress().borrow().done {
                    return Err(Error::PlaybackFailed {
                        reason: reason.for_client().to_owned(),
                    });
                }
                None
            }
            Err(e) => return Err(unreadable(e)),
        }
    } else {
        None
    };
    let complete = opened.is_some();
    let size = opened.as_ref().map_or(0, |(_, size)| *size);
    let sent = read
        .length
        .min(MAX_READ)
        .min(size.saturating_sub(read.offset));
    let handle = match opened {
        Some((mut handle, _)) if sent > 0 => {
            handle
                .seek(std::io::SeekFrom::Start(read.offset))
                .await
                .map_err(unreadable)?;
            handle
        }
        // Nothing to send: an empty stand-in.
        _ => open_at(&file, read.offset, 0).await.map_err(unreadable)?,
    };
    Ok((
        handle,
        MediaReadReply {
            sent,
            size,
            complete,
            epoch: job.epoch,
        },
    ))
}

/// One chunk of a subtitle file (P2-11): at most [`SUBTITLE_CHUNK`] bytes
/// from `offset`, the file's length and its version. At or past the end
/// that is no bytes, so a client's loop ends. Only the chunk is read,
/// however large the file.
///
/// With the `version` of an earlier chunk, a file that is not that one
/// any more is `SubtitleChanged`, and none of it is read.
async fn read_subtitle(
    path: &Path,
    offset: u64,
    version: Option<u64>,
) -> Result<(Vec<u8>, u64, u64), Error> {
    let unreadable = |e: std::io::Error| {
        tracing::warn!(path = %path.display(), error = %e, "cannot read a subtitle");
        Error::Unavailable {
            message: "the subtitle file could not be read".into(),
        }
    };
    let mut file = crate::files::open_regular(path).await.map_err(unreadable)?;
    // Of the open file, not of the path: the version is of the file the
    // bytes below come from, whatever the path names by then. It is taken
    // before the bytes, so a write during the read gives the next chunk's
    // request a version that no longer holds.
    let metadata = file.metadata().await.map_err(unreadable)?;
    let now = subtitle_version(&metadata);
    if version.is_some_and(|read| read != now) {
        return Err(Error::SubtitleChanged);
    }
    let total = metadata.len();
    let mut bytes = Vec::new();
    if offset < total {
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(unreadable)?;
        let mut chunk = file.take(SUBTITLE_CHUNK);
        chunk.read_to_end(&mut bytes).await.map_err(unreadable)?;
    }
    Ok((bytes, total, now))
}

/// A value that names a subtitle file as it is now: another one once the
/// file is written again (its length or its modification time changes) or
/// replaced (another inode). A hash, so that a client learns nothing of
/// the server's disk from it.
fn subtitle_version(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let mut hash = blake3::Hasher::new();
    for part in [
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime() as u64,
        metadata.mtime_nsec() as u64,
    ] {
        hash.update(&part.to_le_bytes());
    }
    let mut first = [0; 8];
    first.copy_from_slice(&hash.finalize().as_bytes()[..8]);
    u64::from_le_bytes(first)
}

/// WebVTT segment `n` of an HLS playback's subtitle rendition (P2-07),
/// made from `subtitle`, its text: the cues over that segment's time.
pub(crate) async fn subtitle_segment_of(
    conversion: Option<&Conversion>,
    subtitle: Option<Sidecar>,
    n: u32,
) -> Result<Vec<u8>, Error> {
    let span = conversion
        .filter(|c| c.subtitle_rendition().is_some())
        .and_then(|c| c.segment_span(n));
    let (Some((start_ms, end_ms)), Some(Sidecar::Made { state, .. })) = (span, subtitle) else {
        return Err(Error::NotFound {
            what: format!("subtitle segment {n} in this playback"),
        });
    };
    let path = read_text(state).await?;
    let vtt = tokio::fs::read_to_string(&path).await.map_err(|e| {
        tracing::warn!(path = %path.display(), error = %e, "cannot read a subtitle");
        Error::Unavailable {
            message: "the subtitle file could not be read".into(),
        }
    })?;
    Ok(hls::subtitle_segment(&vtt, start_ms, end_ms).into_bytes())
}

/// What a playback plays (PX-37): `plan`, of what `source` reads (the
/// film at `film`, or a copy) described as `descriptor`.
struct Planned {
    plan: PlaybackPlan,
    film: PathBuf,
    descriptor: Arc<MediaDescriptor>,
    source: Source,
}

impl Planned {
    /// The file it reads.
    fn read_path(&self) -> &Path {
        self.source.read_path(&self.film)
    }
}

/// A range of a direct playback's file: at most [`MAX_READ`] bytes from
/// `read.offset`, from the file the playback started with or not at all
/// (PX-09 B-1).
pub(crate) async fn direct_range(
    playback: &RangeOf,
    read: &MediaRead,
) -> Result<(tokio::fs::File, MediaReadReply), Error> {
    let size = playback.size;
    let sent = read
        .length
        .min(MAX_READ)
        .min(size.saturating_sub(read.offset));
    let unreadable = |e: std::io::Error| {
        tracing::warn!(path = %playback.path.display(), error = %e, "cannot read a media file");
        Error::Unavailable {
            message: "the media file could not be read".into(),
        }
    };
    let mut file = crate::files::open_nonblocking(&playback.path)
        .await
        .map_err(unreadable)?;
    // The file the playback started with, or none (PX-09 B-1): the
    // path may name another by now.
    let now = FileId::of(&file.metadata().await.map_err(unreadable)?);
    if playback.file != Some(now) {
        tracing::warn!(path = %playback.path.display(), "a media file was replaced during its playback");
        return Err(Error::Unavailable {
            message: "the media file changed during playback".into(),
        });
    }
    if sent > 0 {
        file.seek(std::io::SeekFrom::Start(read.offset))
            .await
            .map_err(unreadable)?;
    }
    Ok((
        file,
        MediaReadReply {
            sent,
            size,
            complete: true,
            epoch: 0,
        },
    ))
}

/// What a range read uses of its playback (review 2026-10-11 M10).
pub(crate) struct RangeOf {
    pub(crate) revision: u64,
    pub(crate) conversion: Option<Arc<Conversion>>,
    pub(crate) size: u64,
    pub(crate) path: PathBuf,
    pub(crate) file: Option<FileId>,
}

impl RangeOf {
    pub(crate) fn of(playback: &Playback) -> Self {
        Self {
            revision: playback.revision,
            conversion: playback.conversion.clone(),
            size: playback.size,
            path: playback.read_path().to_path_buf(),
            file: playback.file,
        }
    }
}

/// A conversion that could not start or restart, as the client is told:
/// a failure to run ffmpeg names where it is, so only the log has it
/// (PX-09 finding 23).
fn conversion_error(e: JobError) -> Error {
    match e {
        JobError::Busy => Error::Busy,
        JobError::Prepare(reason) => Error::PlaybackFailed { reason },
        JobError::Spawn(reason) => {
            tracing::warn!(reason, "cannot run ffmpeg");
            Error::PlaybackFailed {
                reason: crate::encode::job::SPAWN_FAILED.into(),
            }
        }
    }
}

/// The text a reading made: waits for it a while, then tells the client to
/// ask again.
/// A subtitle search, download or removal refused, as the client sees it
/// (PX-02). The site's own words go to the log: they may name the
/// account.
fn download_error(e: crate::subtitle_downloads::DownloadError) -> Error {
    use crate::subtitle_downloads::DownloadError;
    use crate::subtitle_site::SiteError;
    match e {
        DownloadError::NoSource => Error::NoSubtitleSource,
        DownloadError::NotFound(what) => Error::NotFound { what: what.into() },
        DownloadError::NotAllowed(_) => Error::NotAllowed,
        DownloadError::QuotaExhausted {
            whose,
            resets_at,
            site,
        } => Error::QuotaExhausted {
            whose,
            resets_at,
            site: site.map(|s| s.id().into()),
        },
        DownloadError::InvalidInput(message) => Error::InvalidInput { message },
        DownloadError::Site {
            error: SiteError::RateLimited { retry_after_ms },
            ..
        } => Error::RateLimited { retry_after_ms },
        DownloadError::Site { site, error } => {
            // The site's own words in the log only (`SiteError::user_message`).
            if matches!(error, SiteError::Refused { .. }) {
                tracing::warn!(%site, %error, "a subtitle site refused the server; check its API key and account");
            } else {
                tracing::warn!(%site, %error, "a subtitle search or download failed");
            }
            Error::Unavailable {
                message: error.user_message(site),
            }
        }
        DownloadError::Catalog(e) => catalog_error(e),
        DownloadError::Unavailable(message) => Error::Unavailable { message },
        DownloadError::Io(message) => {
            tracing::warn!(error = %message, "a downloaded subtitle could not be kept or removed");
            Error::Internal
        }
        DownloadError::Store(e) => internal(e),
    }
}

/// A subtitle timing refusal as the client sees it (PX-03).
fn timing_error(e: crate::subtitle_sync::TimingError) -> Error {
    use crate::subtitle_sync::TimingError;
    match e {
        TimingError::NoSuchTrack(_) => Error::NotFound {
            what: "a subtitle track of this item".into(),
        },
        TimingError::NotExternalText | TimingError::OffsetTooLarge => Error::InvalidInput {
            message: e.to_string(),
        },
        // Its paths go to the log, not to the client.
        TimingError::Io(message) => {
            tracing::warn!(error = %message, "a subtitle's timing could not be changed");
            Error::Unavailable {
                message: "the subtitle could not be read or written".into(),
            }
        }
        TimingError::Store(e) => internal(e),
    }
}

async fn read_text(mut state: watch::Receiver<TextState>) -> Result<PathBuf, Error> {
    let _ = tokio::time::timeout(READ_WAIT, state.wait_for(TextState::finished)).await;
    let now = state.borrow().clone();
    match now {
        TextState::Ready(path) => Ok(path),
        // The reason is in the log.
        TextState::Failed(_) => Err(Error::Unavailable {
            message: "the subtitle could not be read as text".into(),
        }),
        TextState::Waiting => Err(Error::SubtitleNotReady { done_percent: 0 }),
        TextState::Reading { done_percent } => Err(Error::SubtitleNotReady { done_percent }),
    }
}

/// A catalog failure as the client sees it. Backend details (the PMS URL,
/// file paths) go to the log, not to the client.
fn catalog_error(e: CatalogError) -> Error {
    match e {
        // A library removed or never added is, to a client, not there.
        CatalogError::NotFound(what)
        | CatalogError::ForeignId(what)
        | CatalogError::NotServed(what) => Error::NotFound { what },
        CatalogError::NotPlayable(id) => Error::NotFound {
            what: format!("a media file for {id}"),
        },
        CatalogError::MediaFileUnavailable(_) => {
            tracing::warn!(error = %e, "media file unavailable");
            Error::Unavailable {
                message: "the media file is not readable on the server".into(),
            }
        }
        CatalogError::Unreachable { .. } | CatalogError::Backend { .. } => {
            tracing::warn!(error = %e, "catalog unavailable");
            Error::Unavailable {
                message: "the library is unavailable".into(),
            }
        }
        CatalogError::NotSetUp => Error::NotSetUp,
    }
}

/// Whether `e` says the item is not served now: in a library removed or
/// never added, on a Plex server no longer configured, or gone. Not a
/// failure to reach Plex, which says nothing of whether it is served.
pub(crate) fn unserved(e: &CatalogError) -> bool {
    matches!(
        e,
        CatalogError::NotFound(_)
            | CatalogError::ForeignId(_)
            | CatalogError::NotServed(_)
            | CatalogError::NotSetUp
    )
}

/// What plays next, as a client is told it: its heart only for one that
/// takes hearts (PX-43).
fn playing(next: Option<spoolframe_protocol::playlists::PlaylistNext>, liked: bool) -> Reply {
    match next {
        Some(mut next) => {
            next.liked &= liked;
            Reply::PlaylistItem(next)
        }
        None => Reply::PlaylistFinished,
    }
}

/// `summary` as a session sees it: without its owner's and shares'
/// names unless its client listed `spoolframe:playlist-sharing`.
fn named(
    mut summary: spoolframe_protocol::playlists::PlaylistSummary,
    sharing: bool,
    art: bool,
) -> spoolframe_protocol::playlists::PlaylistSummary {
    // A picture only for a client that takes it (PX-53).
    if !art {
        summary.artwork = None;
        summary.custom_art = false;
    }
    if !sharing {
        summary.owner_name = None;
        if let spoolframe_protocol::playlists::Visibility::Shared { with } = &mut summary.visibility
        {
            for share in with {
                share.display_name = None;
                share.kind = None;
            }
        }
    }
    summary
}

/// An item's playlist reference as a session sees it: whose it is only
/// for a client that listed `spoolframe:playlist-sharing`, its picture
/// only for one that listed `spoolframe:playlist-art`, as `named` does
/// for a summary.
fn named_ref(
    mut held: spoolframe_protocol::playlists::PlaylistRef,
    sharing: bool,
    art: bool,
) -> spoolframe_protocol::playlists::PlaylistRef {
    if !sharing {
        held.owner_name = None;
    }
    if !art {
        held.artwork = None;
    }
    held
}

/// A playlist failure as the client sees it (PX-40).
fn playlists_error(e: crate::playlists::PlaylistsError) -> Error {
    use crate::playlists::PlaylistsError;
    match e {
        PlaylistsError::NotFound => Error::NotFound {
            what: "playlist".into(),
        },
        PlaylistsError::NoItem(id) => Error::NotFound {
            what: id.to_string(),
        },
        PlaylistsError::ReadOnly => Error::PlaylistReadOnly,
        PlaylistsError::AdminRequired => Error::AdminRequired,
        PlaylistsError::KindNotAllowed(kind) | PlaylistsError::NotLikeable(kind) => {
            Error::KindNotAllowed { kind: kind.into() }
        }
        PlaylistsError::InvalidInput(message) => Error::InvalidInput { message },
        PlaylistsError::Conflict { revision } => Error::PlaylistConflict { revision },
        PlaylistsError::NotAllowed => Error::NotAllowed,
        PlaylistsError::ImageTooLarge => image_too_large(),
        PlaylistsError::ImageUnreadable(why) => {
            tracing::debug!(why, "an uploaded cover was refused");
            Error::ImageUnreadable
        }
        PlaylistsError::Unavailable(message) => Error::Unavailable { message },
        PlaylistsError::Catalog(e) => catalog_error(e),
        PlaylistsError::Store(e) => internal(e),
        PlaylistsError::Io(e) => internal(e),
    }
}

/// A cover past the limits (PX-53), saying them.
fn image_too_large() -> Error {
    Error::ImageTooLarge {
        max_bytes: spoolframe_protocol::playlists::MAX_ART_BYTES,
        max_pixels: spoolframe_protocol::playlists::MAX_ART_PIXELS,
    }
}

fn watch_error(e: WatchError) -> Error {
    match e {
        WatchError::NotTracked(id) => Error::NotFound {
            what: format!("watch state for {id}"),
        },
        WatchError::Catalog(e) => catalog_error(e),
        WatchError::Store(e) => internal(e),
    }
}

/// The answer for a code that is not pending.
fn wrong_sign_in_code() -> Error {
    Error::NotFound {
        what: "sign-in code".into(),
    }
}

/// The failure limit's keys for device sign-in codes tried by `user` on
/// this connection's endpoint: apart from passwords and PINs.
fn sign_in_code_keys(connection: &Connection, user: &SessionUser) -> (String, String) {
    (
        format!("devcode:{}", user.user),
        format!("devcode@{}", connection.remote_id()),
    )
}

/// The auth methods this server offers.
pub(crate) fn auth_methods() -> Vec<AuthMethod> {
    vec![AuthMethod::Password, AuthMethod::DeviceApproval]
}

/// The connection's capabilities from Hello, with `display` over their
/// HDR formats and bit depth (PGPU-06); `None` before a Hello that gave
/// any.
fn capabilities(
    state: &ConnectionState,
    display: Option<&spoolframe_types::DisplayCapabilities>,
) -> Option<spoolframe_types::ClientCapabilities> {
    state
        .hello
        .get()
        .and_then(|h| h.capabilities.as_ref())
        .map(|caps| caps.with_display(display))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_app_closing_with_no_error_code_is_a_quiet_end() {
        use iroh::endpoint::{ApplicationClose, ConnectionError};
        let closed = |code: u32| {
            ConnectionError::ApplicationClosed(ApplicationClose {
                error_code: VarInt::from_u32(code),
                reason: Default::default(),
            })
        };
        assert!(hung_up(&closed(0)));
        for reason in [
            closed(1),
            ConnectionError::LocallyClosed,
            ConnectionError::TimedOut,
            ConnectionError::Reset,
        ] {
            assert!(!hung_up(&reason), "{reason}");
        }
    }
    use crate::plex_accounts::AccountLink;

    /// C-13: shutdown notices went to clients one at a time, each given
    /// `SHUTDOWN_NOTICE`, so N clients that did not take theirs held the
    /// shutdown up N times as long.
    #[tokio::test(start_paused = true)]
    async fn clients_that_do_not_take_their_notice_hold_shutdown_up_once_together() {
        let started = tokio::time::Instant::now();
        deliver((0..8).map(|_| std::future::pending::<Result<(), codec::CodecError>>())).await;
        let took = started.elapsed();
        assert!(
            took <= SHUTDOWN_NOTICE,
            "8 clients held shutdown up {took:?}"
        );
    }

    #[test]
    fn the_shared_probe_allowance_matches_the_media_probers_default() {
        assert_eq!(
            spoolframe_protocol::policy::wait::PROBE,
            spoolframe_media::Prober::DEFAULT_TIMEOUT
        );
    }

    fn run() -> PlexSyncRun {
        PlexSyncRun {
            at: 1_790_000_100,
            libraries: 2,
            pulled: 3,
            pushed: 4,
            skipped: 5,
            failed: 1,
            notes: vec!["Cabin movies skipped".into()],
        }
    }

    #[test]
    fn a_linked_account_is_shown_with_its_last_sync_and_never_its_token() {
        let link = Link::Account(AccountLink {
            token: "plex-token".into(),
            username: Some("alice_plex".into()),
            email: Some("alice@example.com".into()),
            linked_at: 1_790_000_000,
        });
        let shown = plex_link(Some(link), (Some(run()), true)).unwrap();
        assert_eq!(
            shown,
            PlexLink {
                username: Some("alice_plex".into()),
                email: Some("alice@example.com".into()),
                by_config: false,
                linked_at: Some(1_790_000_000),
                last_sync: Some(PlexSyncSummary {
                    at: 1_790_000_100,
                    libraries: 2,
                    pulled: 3,
                    pushed: 4,
                    skipped: 5,
                    failed: 1,
                }),
                syncing: true,
            }
        );
        let wire =
            spoolframe_protocol::codec::encode(&Reply::PlexLink { link: Some(shown) }).unwrap();
        assert!(!wire.windows(10).any(|w| w == b"plex-token"));
    }

    #[test]
    fn the_config_s_owner_link_has_no_account_details() {
        let shown = plex_link(Some(Link::Owner), (None, false)).unwrap();
        assert!(shown.by_config);
        assert_eq!(
            (shown.username, shown.linked_at, shown.last_sync),
            (None, None, None)
        );
    }

    #[test]
    fn no_link_is_none_whatever_the_sync_says() {
        assert_eq!(plex_link(None, (Some(run()), false)), None);
    }
}
