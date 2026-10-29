//! Federation: experimental, on a session whose server turned
//! [`crate::Experimental::federation`] on.
//!
//! Pairing this session's server with another: an admin of this server
//! pastes, or scans, the other server's `federation` invite, and this
//! server redeems it, exactly as the web UI's Federation page would.
//! Before asking, an app looks the code up on the other server
//! ([`crate::Client::look_up_invite`]): the lookup names that server and
//! its fingerprint, and its outcome says the kind and who made it, for
//! the admin to check. A non-admin has nothing to pair with: offer to
//! send the invite to the server's admin instead. Then the admin shares
//! libraries of this server with the other ([`Session::share_libraries`]).
//!
//! Browsing a paired server: its libraries come with this server's own
//! from [`Session::libraries`], each with its server
//! ([`spoolframe_types::Library::server`]: name, server id, online), and
//! their items under `fed:` ids. [`crate::Client::libraries_events`] says
//! when to list them again. Home gains a row per paired server
//! ([`spoolframe_protocol::HomeScreen::servers`]), which the user turns
//! off and on ([`Session::set_server_row`]).
//!
//! Watching: this session's server, the user's home, keeps their
//! progress on the paired server's items. A paired server's item plays
//! from that server, and its progress goes here, by item
//! ([`Session::update_watch_progress`]).
//!
//! Playing: [`Session::start_remote_playback`] plays a `fed:` item from
//! the paired server itself when this device reaches it. It asks this
//! session's server where the paired server is, dials it once with a
//! short budget, asks this session's server for a token for this device,
//! redeems it on the paired server, and plays there. The
//! [`RemotePlayback`]'s session is on the paired server, for that one
//! item: read and control the playback through it as any other. Progress
//! still goes to this session's server.
//!
//! When the paired server is not reached, this session's server plays it
//! from the paired server and on to this device (proxy mode,
//! [`RemoteMode::Proxied`]). The playback is then this session's, read and
//! controlled through it. An HTTP player (a cast target) cannot present a
//! token to the paired server, so its play is always proxied
//! ([`Session::start_proxied_playback`]). A play that fails mid-way,
//! because the paired server or the route to it is gone, does not switch
//! to the other way: it ends. Start it again, and the SDK picks the way
//! anew.

use std::time::Duration;

use spoolframe_protocol::federation::{
    PairServer, PeerServer, PlayTokenRedeemed, RedeemPlayToken, RemoteGranted, RemoteHints,
    ShareLibraries,
};
use spoolframe_protocol::{
    Reply, Request, Response, SearchHit, SearchRequest, SearchResults, Selection,
};
use spoolframe_types::{LibraryId, MediaId, Namespace};

use crate::session::unexpected;
use crate::{Playback, PlaybackOptions, SdkError, Session};

/// How long a device tries to reach a paired server itself before it
/// plays the item another way: one dial, a few seconds.
pub const DIRECT_DIAL: Duration = Duration::from_secs(5);

/// How a paired server's item plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum RemoteMode {
    /// From the paired server to this device.
    Direct,
    /// Through the user's own server, which plays it from the paired one.
    Proxied,
}

/// A paired server's item, playing.
#[derive(Debug, Clone)]
pub struct RemotePlayback {
    /// What the playback is read and controlled through: a session on the
    /// paired server for this one item (direct), or the user's own
    /// (proxied).
    pub session: Session,
    pub playback: Playback,
    pub mode: RemoteMode,
    /// The item as `session` names it: the paired server's own id
    /// (direct), or the `fed:` id (proxied).
    pub item: MediaId,
    /// The paired server's relays, held with the play token for as long
    /// as the play lives, when it went through them. A start that fails
    /// lets them go at once.
    _relays: Option<std::sync::Arc<crate::client::PlayRelays>>,
}

/// A download of a paired server's item, read straight from that server:
/// what its download token opened there, for that one download.
#[derive(Debug, Clone)]
pub struct RemoteDownload {
    /// A session on the paired server that reads that download alone.
    pub session: Session,
    /// The download, as the paired server names it: what to read there.
    pub id: String,
    /// The paired server's relays, held with the token while it is read.
    _relays: Option<std::sync::Arc<crate::client::PlayRelays>>,
}

/// This server's own item as a hit: its own copy, from nowhere else.
fn own_hit(item: spoolframe_types::Item) -> SearchHit {
    SearchHit {
        play_from: item.id().clone(),
        origin: None,
        also_on: Vec::new(),
        item,
    }
}

/// Whether [`Session::open_origin`] reached the paired server.
enum Opening {
    Reached(Box<Opened>),
    /// It did not answer: why.
    NotReached(SdkError),
}

/// A session opened on a paired server by a token for this device.
struct Opened {
    session: Session,
    redeemed: PlayTokenRedeemed,
    granted: RemoteGranted,
    relays: Option<std::sync::Arc<crate::client::PlayRelays>>,
}

/// A server told that its libraries changed: a paired server came
/// online or went offline, or shared or stopped sharing a library. List
/// them again ([`Session::libraries`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct LibrariesChanged {
    /// The server's key: the one whose libraries changed.
    pub server: String,
}

impl Session {
    /// Pair this session's server with the one whose `federation` invite
    /// `link` is: the server paired, its key's fingerprint to show. An
    /// admin's only ([`SdkError::AdminRequired`]), after a recent
    /// confirmation ([`SdkError::RecentSignInRequired`]: confirm, then ask
    /// again). [`SdkError::Unsupported`] without
    /// [`crate::Experimental::federation`], and nothing is sent.
    ///
    /// The other server's refusal of the code is the invite's own error
    /// ([`SdkError::InviteInvalid`], [`SdkError::InviteExpired`],
    /// [`SdkError::InviteExhausted`], [`SdkError::WrongInviteKind`],
    /// [`SdkError::RateLimited`]); any other is
    /// [`SdkError::PairingRefused`]. [`SdkError::Unavailable`] when this
    /// server cannot reach the other; [`SdkError::InvalidInput`] for a
    /// link that is not one, has no code, or is this server's own.
    pub async fn pair_server(&self, link: &str) -> Result<PeerServer, SdkError> {
        self.federated()?;
        let request = Request::PairServer(PairServer {
            link: link.to_owned(),
        });
        match self.call(request).await? {
            Reply::ServerPaired { server } => Ok(server),
            other => Err(unexpected(other)),
        }
    }

    /// Share `libraries` of this session's server with the paired server
    /// `server_id`, as an app offers once [`Session::pair_server`]
    /// paired them: the libraries shared. Every user of the paired server
    /// may browse and watch them; a library shared already stays shared.
    /// An admin's only, recently confirmed, as pairing is.
    /// [`SdkError::NotFound`] for a server not paired;
    /// [`SdkError::InvalidInput`] for a library the server does not serve,
    /// and then none is shared. [`SdkError::Unsupported`] without
    /// [`crate::Experimental::federation`], and nothing is sent.
    pub async fn share_libraries(
        &self,
        server_id: &str,
        libraries: &[LibraryId],
    ) -> Result<Vec<LibraryId>, SdkError> {
        self.federated()?;
        let request = Request::ShareLibraries(ShareLibraries {
            server_id: server_id.to_owned(),
            libraries: libraries.to_vec(),
        });
        match self.call(request).await? {
            Reply::LibrariesShared { libraries } => Ok(libraries),
            other => Err(unexpected(other)),
        }
    }

    /// Show or hide paired server `server_id`'s own row on the user's Home:
    /// its items stay in Continue watching either way.
    /// [`SdkError::NotFound`] for a server not paired;
    /// [`SdkError::Unsupported`] without
    /// [`crate::Experimental::federation`].
    pub async fn set_server_row(&self, server_id: &str, shown: bool) -> Result<(), SdkError> {
        self.federated()?;
        let request = Request::SetServerRow {
            server_id: server_id.to_owned(),
            shown,
        };
        self.done(request).await
    }

    /// A search over this session's server and what its paired servers
    /// share, each item a [`SearchHit`]: a title on several servers
    /// once, with where it is from, its other copies and the copy that
    /// plays. Counts are approximate. One search screen serves every
    /// server: without [`crate::Experimental::federation`], or from a
    /// server that answers with plain results (one too old for hits),
    /// the hits are this server's own items, each its own copy. Any
    /// other error is the search's, never hidden by a second search.
    pub async fn search_hits(
        &self,
        request: SearchRequest,
    ) -> Result<SearchResults<SearchHit>, SdkError> {
        if self.federated().is_err() {
            return self.search(request).await.map(|r| r.map(own_hit));
        }
        match self.call(Request::Search(request)).await? {
            Reply::SearchHits(hits) => Ok(hits),
            Reply::Search(results) => Ok(results.map(own_hit)),
            other => Err(unexpected(other)),
        }
    }

    /// Play the title `title` (any copy's id) from paired server `from`,
    /// or from this session's server with `None`: remembered for
    /// the user, for every copy of the title, while that server has one.
    /// [`SdkError::InvalidInput`] for a title with no external ids, which
    /// has one copy; [`SdkError::NotFound`] for a server not paired or an
    /// item not found. [`SdkError::Unsupported`] without
    /// [`crate::Experimental::federation`], and nothing is sent.
    pub async fn set_play_from(&self, title: &MediaId, from: Option<&str>) -> Result<(), SdkError> {
        self.federated()?;
        let request = Request::SetPlayFrom {
            title: title.clone(),
            from: from.map(str::to_owned),
        };
        self.done(request).await
    }

    /// Tell this session's server, the user's home, where the user got to
    /// in `id`, a paired server's item that plays from that server rather
    /// than from a playback here. Report as a playback's progress
    /// is reported: now and then while playing, and on pause or stop. The
    /// paired server keeps nothing of it. [`SdkError::InvalidInput`] for an
    /// id that is not a paired server's; [`SdkError::NotFound`] once it is
    /// no longer available. [`SdkError::Unsupported`] without
    /// [`crate::Experimental::federation`]. Neither is sent.
    pub async fn update_watch_progress(
        &self,
        id: &MediaId,
        position: Duration,
    ) -> Result<(), SdkError> {
        self.federated()?;
        if id.namespace() != Namespace::Federated {
            return Err(SdkError::InvalidInput(format!(
                "{id} is not a paired server's item"
            )));
        }
        let request = Request::UpdateWatchProgress {
            id: id.clone(),
            position_ms: u64::try_from(position.as_millis()).unwrap_or(u64::MAX),
        };
        self.done(request).await
    }

    /// Where the paired server `id` (a `fed:` item) is from may be
    /// dialled, and whether this session's server is connected to it.
    /// [`SdkError::Unsupported`] without
    /// [`crate::Experimental::federation`].
    pub async fn remote_hints(&self, id: &MediaId) -> Result<RemoteHints, SdkError> {
        self.federated()?;
        match self.call(Request::RemoteHints { id: id.clone() }).await? {
            Reply::RemoteHints(hints) => Ok(hints),
            other => Err(unexpected(other)),
        }
    }

    /// A play token for `id`, a `fed:` item, bound to this device: this
    /// session's server vouches for its user to the paired server.
    /// [`SdkError::OriginOffline`] when that server is offline to this
    /// one. Each play asks its own; it works once.
    pub async fn remote_grant(&self, id: &MediaId) -> Result<RemoteGranted, SdkError> {
        self.federated()?;
        self.grant(id, None).await
    }

    /// A token for `id` bound to this device: a play token, or with
    /// `download` (this session's server's id of it) a download token of
    /// that one download. A server with no download tokens ignores
    /// `download` and grants a play token, which reads no download:
    /// [`SdkError::Unsupported`].
    async fn grant(&self, id: &MediaId, download: Option<&str>) -> Result<RemoteGranted, SdkError> {
        let request = Request::RemoteGrant {
            id: id.clone(),
            download: download.map(str::to_owned),
        };
        match self.call(request).await? {
            Reply::RemoteGranted(granted) if download.is_some() && granted.download.is_none() => {
                Err(SdkError::Unsupported {
                    feature: "reading a paired server's download from that server".into(),
                })
            }
            Reply::RemoteGranted(granted) => Ok(granted),
            other => Err(unexpected(other)),
        }
    }

    /// Reach the paired server `id` (a `fed:` item) is from, within
    /// [`DIRECT_DIAL`], and open a session there with a token for this
    /// device ([`Session::grant`] of `download`): [`Opening::NotReached`]
    /// when it did not answer, for the caller to go through this
    /// session's server instead. Through the paired server's own relay the
    /// token is what lets this device through, so it is asked
    /// first, and a dial that fails leaves it to expire unused; without
    /// relays, the dial comes first, and a device that cannot reach the
    /// server asks for none. [`SdkError::OriginOffline`] when the paired
    /// server is offline to this session's server; another server answering
    /// at its key is [`SdkError::ServerIdChanged`], never gone around.
    async fn open_origin(&self, id: &MediaId, download: Option<&str>) -> Result<Opening, SdkError> {
        self.federated()?;
        if id.namespace() != Namespace::Federated {
            return Err(SdkError::InvalidInput(format!(
                "{id} is not a paired server's item"
            )));
        }
        let hints = self.remote_hints(id).await?;
        if !hints.online {
            return Err(SdkError::OriginOffline);
        }
        let origin = crate::client::parse_id(&hints.endpoint_id)?;
        self.client
            .remember_paired_server(self.server, origin, &hints.addrs, &hints.relays);
        let relayed = !self.client.relays_to(origin).is_empty();
        let (early, relays) = if relayed {
            let granted = self.grant(id, download).await?;
            let relays = self.client.hold_play_token(origin, &granted.token).await;
            (Some(granted), relays)
        } else {
            (None, None)
        };
        match self.reach_origin(origin, &hints).await {
            Ok(()) => {}
            Err(e @ SdkError::ServerIdChanged { .. }) => return Err(e),
            Err(e) => return Ok(Opening::NotReached(e)),
        }
        let granted = match early {
            Some(granted) => granted,
            None => self.grant(id, download).await?,
        };
        let request = Request::RedeemPlayToken(RedeemPlayToken {
            token: granted.token.clone(),
        });
        let redeemed = match self.client.call(origin, None, request).await? {
            Response::Ok(Reply::PlayTokenRedeemed(redeemed)) => redeemed,
            Response::Ok(other) => return Err(unexpected(other)),
            Response::Err(e) => return Err(SdkError::from_server(e)),
        };
        Ok(Opening::Reached(Box::new(Opened {
            session: self.on_paired_server(origin, redeemed.session.clone()),
            redeemed,
            granted,
            relays: relays.map(std::sync::Arc::new),
        })))
    }

    /// Play `id`, a paired server's (`fed:`) item: from that server
    /// when this device reaches it within [`DIRECT_DIAL`], with `selection`
    /// and `options` as [`Session::start_playback_with`] takes them.
    /// [`SdkError::OriginOffline`] when the paired server is offline to
    /// this session's server. Report progress with
    /// [`Session::update_watch_progress`] on this session, by `id`.
    pub async fn start_remote_playback(
        &self,
        id: &MediaId,
        selection: Option<Selection>,
        options: PlaybackOptions,
    ) -> Result<RemotePlayback, SdkError> {
        let opened = match self.open_origin(id, None).await? {
            Opening::Reached(opened) => opened,
            // Not reached: this session's server plays it from there, and
            // on to this device (proxy mode).
            Opening::NotReached(e) => {
                tracing::info!(%id, error = %e, "a paired server is not reached; playing through this server");
                return self.start_proxied_playback(id, selection, options).await;
            }
        };
        let PlayTokenRedeemed { item, .. } = opened.redeemed;
        let playback = opened
            .session
            .start_playback_with(&item, selection, options)
            .await?;
        Ok(RemotePlayback {
            session: opened.session,
            playback,
            mode: RemoteMode::Direct,
            item,
            _relays: opened.relays,
        })
    }

    /// Read `download`, this session's server's download of `id`, a paired
    /// server's item, straight from that server when this device reaches
    /// it within [`DIRECT_DIAL`]: its session there, for that one
    /// download, and its id there. Not reached, read it from this session's
    /// server, which reads it from there in its name
    /// ([`SdkError::Unreachable`] here). [`SdkError::OriginOffline`] when
    /// the paired server is offline to this session's server.
    pub async fn remote_download(
        &self,
        id: &MediaId,
        download: &str,
    ) -> Result<RemoteDownload, SdkError> {
        let opened = match self.open_origin(id, Some(download)).await? {
            Opening::Reached(opened) => opened,
            Opening::NotReached(e) => return Err(e),
        };
        Ok(RemoteDownload {
            session: opened.session,
            id: opened
                .granted
                .download
                .expect("a download's grant names it: checked as granted"),
            _relays: opened.relays,
        })
    }

    /// Play `id`, a paired server's item, through this session's server
    /// (proxy mode): it plays it from the paired server in its own
    /// name, and on to this device. Its reads and controls are this
    /// session's, as any playback's.
    pub async fn start_proxied_playback(
        &self,
        id: &MediaId,
        selection: Option<Selection>,
        options: PlaybackOptions,
    ) -> Result<RemotePlayback, SdkError> {
        self.federated()?;
        let playback = self.start_playback_with(id, selection, options).await?;
        Ok(RemotePlayback {
            session: self.clone(),
            playback,
            mode: RemoteMode::Proxied,
            item: id.clone(),
            _relays: None,
        })
    }

    /// Dial paired server `origin`, as `hints` name it, once, within
    /// [`DIRECT_DIAL`]: `Ok` once it answered as the server id its pair
    /// knows.
    async fn reach_origin(
        &self,
        origin: iroh::EndpointId,
        hints: &RemoteHints,
    ) -> Result<(), SdkError> {
        let link = self.client.link(origin);
        let conn = tokio::time::timeout(DIRECT_DIAL, link.connection(&self.client))
            .await
            .map_err(|_| {
                SdkError::Unreachable("the paired server did not answer in time".into())
            })??;
        if conn.info.server_id != hints.server_id {
            return Err(SdkError::ServerIdChanged {
                endpoint_id: hints.endpoint_id.clone(),
                saved: hints.server_id.clone(),
                now: conn.info.server_id.clone(),
            });
        }
        Ok(())
    }

    /// A session on paired server `origin` for what `token` opened there:
    /// this user's, as their own server knows them, for the app to show.
    fn on_paired_server(
        &self,
        origin: iroh::EndpointId,
        token: spoolframe_protocol::SessionId,
    ) -> Session {
        Session {
            server: origin,
            token,
            remembered: None,
            password_change_required: std::sync::Arc::default(),
            ..self.clone()
        }
    }

    /// `Ok` when the server turned federation on for this session.
    fn federated(&self) -> Result<(), SdkError> {
        if self.experimental().federation {
            Ok(())
        } else {
            Err(SdkError::Unsupported {
                feature: spoolframe_protocol::experimental::FEDERATION.to_owned(),
            })
        }
    }
}
