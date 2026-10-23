//! `spoolframe-probe federation` (P8): what a user's device sees of a
//! paired server, through the SDK, as an app does it. It signs in to the
//! user's own server (SB), lists the libraries with the experimental
//! `federation` feature named, and, when asked, plays one paired server's
//! (SA's) film the way an app does ([`Session::start_remote_playback`]):
//! from SA when this device reaches it, else through SB. It reads the
//! file to its end, says which way the bytes came, and reports progress to
//! SB.
//!
//! Every step is one JSON object on a line of its own ([`Event`]), for a
//! script to assert on: the NixOS federation test (`nix/federation-e2e.nix`)
//! and, later, P8-06's federation checks.
//!
//! It changes nothing but the user's progress on the film played, and
//! opens no session it does not end.

use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use spoolframe_client_sdk::types::SearchScope;
use spoolframe_client_sdk::types::{
    ClientCapabilities, Container, Item, Library, LibraryKind, MediaId, Platform, VideoCodec,
};
use spoolframe_client_sdk::{
    Client, ClientOptions, ConnectionPath, Device, DownloadSource, DownloadTracks, Experimental,
    MemoryStorage, PlaybackOptions, RemoteMode, RemotePlayback, SdkError, SearchRequest, Selection,
    Session, TitleCopy, TopResult,
};
use spoolframe_protocol::downloads::DownloadState;
use spoolframe_protocol::{MAX_READ, PlaybackPlan};

/// What a federation run does.
#[derive(Debug, Clone)]
pub struct FederationRun {
    /// The user's own server (SB): its key, hex, and where to dial it.
    pub server: String,
    pub addrs: Vec<SocketAddr>,
    pub username: String,
    pub password: String,
    /// List the items of each paired server's libraries too.
    pub list_items: bool,
    pub play: Option<Play>,
    /// Search the films for this (P8-05): each hit, with its copies.
    pub search: Option<String>,
    /// Play the title of this id from this paired server (`SetPlayFrom`,
    /// P8-05), then say which copy the item plays from.
    pub play_from: Option<(MediaId, String)>,
    /// Download a paired server's film (P8-07), as an app does.
    pub download: Option<Download>,
    /// Ask SB for these downloads, by SB's ids (`DownloadStatus`): what
    /// it says of each, as it says it.
    pub download_status: Vec<String>,
    /// Have SB prepare a download of this item (`PrepareDownload`), wait
    /// until its file is made, and read none of it: a download a device
    /// has not fetched yet. Asked about with `download_status`.
    pub prepare: Option<MediaId>,
}

/// A download of a paired server's film: prepared on SB, and read from
/// that server when this device reaches it, else through SB.
#[derive(Debug, Clone)]
pub struct Download {
    /// The film of this title in a paired server's library.
    pub title: String,
    /// Where the download manager keeps it.
    pub dir: PathBuf,
    /// The most it may take.
    pub within: Duration,
}

/// A play of a paired server's item.
#[derive(Debug, Clone)]
pub struct Play {
    pub target: Target,
    /// Play through SB, without trying SA first
    /// ([`Session::start_proxied_playback`]).
    pub proxied: bool,
    /// The most bytes read; the whole file without it.
    pub max_bytes: Option<u64>,
    /// The most each read asks for; at most, and by default,
    /// [`MAX_READ`].
    pub read_size: Option<u64>,
    /// Where the bytes read are written, for the caller to compare.
    pub out: Option<PathBuf>,
    /// Report this position to SB once the bytes are read, and read the
    /// position SB keeps back.
    pub progress: Option<Duration>,
}

/// Which item to play.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The film of this title in a paired server's library.
    Title(String),
    /// This `fed:` id, listed or not: a play of an item no longer shared.
    Id(MediaId),
}

/// One line of the report.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Signed in to SB; `federation` is whether SB turned the feature on
    /// for this connection.
    SignedIn {
        server: String,
        user: String,
        federation: bool,
    },
    /// A library SB lists; `server` is set for a paired server's.
    Library { library: Value },
    /// An item of a paired server's library.
    Item {
        library: String,
        id: String,
        kind: String,
        title: String,
    },
    /// A play started. `mode` is how the SDK played it (`direct` from the
    /// paired server, `proxied` through SB); `route` is which way the
    /// bytes come (`direct`, `relayed` or `proxied`), and `path` the
    /// connection they come on (`lan`, `direct`, `relay`, `unknown`).
    Playing {
        item: String,
        played_as: String,
        mode: String,
        route: String,
        path: String,
        session_server: String,
        plan: String,
        size: u64,
    },
    /// The bytes read: all of the file when `complete`.
    Read {
        bytes: u64,
        complete: bool,
        out: Option<String>,
    },
    /// The position reported to SB, and the one SB keeps for the user.
    Progress {
        reported_ms: u64,
        stored_ms: Option<u64>,
    },
    /// The playback stopped.
    Stopped,
    /// A film a search found (P8-05): the hit's item, the paired server
    /// it is from (`None` for SB's own), its title's other copies, and the
    /// copy that plays.
    Hit {
        id: String,
        title: String,
        year: Option<u64>,
        origin: Option<String>,
        also_on: Vec<CopyOf>,
        play_from: String,
    },
    /// An item's copies, as its details say them (P8-05).
    Copies {
        id: String,
        origin: Option<String>,
        also_on: Vec<CopyOf>,
        play_from: String,
    },
    /// A download, as the download manager left it (P8-07): `phase` as the
    /// SDK names it, and its file once it is done.
    Download {
        key: String,
        phase: Value,
        size: Option<u64>,
        received: u64,
        file: Option<String>,
    },
    /// What SB says of the downloads asked about (`DownloadStatus`), as its
    /// wire has them; `gone`, those it no longer knows.
    DownloadStatus {
        downloads: Vec<Value>,
        gone: Vec<String>,
    },
    /// A step failed: `kind` is the SDK's error, `error` what it says.
    Error {
        stage: String,
        kind: String,
        error: String,
    },
    /// The end of the run.
    Done { ok: bool },
}

/// A title's copy on a server: its id here, and its paired server's name
/// (`None` for SB's own).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CopyOf {
    pub id: String,
    pub server: Option<String>,
}

impl CopyOf {
    fn of(copy: &TitleCopy) -> Self {
        Self {
            id: copy.id.to_string(),
            server: copy.server.as_ref().map(|s| s.name.clone()),
        }
    }
}

/// A step that failed, for [`Event::Error`].
#[derive(Debug)]
struct Failed {
    stage: &'static str,
    kind: String,
    error: String,
}

impl Failed {
    fn sdk(stage: &'static str, e: SdkError) -> Self {
        Self {
            stage,
            kind: kind_of(&e),
            error: e.to_string(),
        }
    }

    fn other(stage: &'static str, error: impl Into<String>) -> Self {
        Self {
            stage,
            kind: "probe".into(),
            error: error.into(),
        }
    }
}

/// The SDK error's variant name: `OriginOffline`, `NotFound`.
fn kind_of(e: &SdkError) -> String {
    let debug = format!("{e:?}");
    debug
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// What a device that plays H.264 and AAC in Matroska or MP4 can play: the
/// test's media plays as it is.
pub fn capabilities() -> ClientCapabilities {
    use spoolframe_client_sdk::types::AudioCodec;
    ClientCapabilities {
        video: [VideoCodec::H264].into(),
        audio: [AudioCodec::Aac].into(),
        containers: [Container::Matroska, Container::Mp4].into(),
        ..ClientCapabilities::default()
    }
}

/// Which way the bytes of `remote` come.
fn route(remote: &RemotePlayback) -> (&'static str, &'static str) {
    let path = match remote.session.connection_path() {
        ConnectionPath::Lan { .. } => "lan",
        ConnectionPath::Direct { .. } => "direct",
        ConnectionPath::Relay => "relay",
        ConnectionPath::Unknown => "unknown",
    };
    let route = match (remote.mode, path) {
        (RemoteMode::Proxied, _) => "proxied",
        (RemoteMode::Direct, "relay") => "relayed",
        (RemoteMode::Direct, _) => "direct",
    };
    (route, path)
}

fn plan_name(plan: &PlaybackPlan) -> &'static str {
    match plan {
        PlaybackPlan::Direct { .. } => "direct",
        PlaybackPlan::Converted { .. } => "converted",
        PlaybackPlan::Unsupported { .. } => "unsupported",
        PlaybackPlan::Unknown => "unknown",
    }
}

/// `item`'s text field `name` in its wire form (`kind`, `title`), or "".
fn wire_field(item: &Item, name: &str) -> String {
    serde_json::to_value(item)
        .ok()
        .and_then(|value| value.get(name).and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_default()
}

/// `item`'s title (an artist's is its name).
fn title_of(item: &Item) -> String {
    match wire_field(item, "title") {
        title if title.is_empty() => wire_field(item, "name"),
        title => title,
    }
}

/// Where `item`'s playback stopped, in ms, as its server keeps it.
fn position_ms(item: &Item) -> Option<u64> {
    serde_json::to_value(item)
        .ok()?
        .get("playback")?
        .get("position")?
        .as_u64()
}

/// Run `run`, telling each step to `emit`. `Ok(true)` when every step
/// worked.
pub async fn run_federation(run: &FederationRun, mut emit: impl FnMut(Event)) -> bool {
    let options = ClientOptions {
        // Nothing heard from the LAN and no relay: the device reaches a
        // server only where it is told to, so the route is the test's.
        lan_discovery: false,
        relays: false,
        experimental: Experimental {
            federation: true,
            ..Experimental::default()
        },
        features: spoolframe_client_sdk::Features {
            downloads: run.download.is_some(),
            ..spoolframe_client_sdk::Features::default()
        },
        downloads_dir: run.download.as_ref().map(|d| d.dir.clone()),
        ..ClientOptions::default()
    };
    let device = Device {
        name: "spoolframe-probe".into(),
        client_name: "spoolframe-probe".into(),
        client_version: env!("CARGO_PKG_VERSION").into(),
        platform: Platform::Cli,
        capabilities: capabilities(),
    };
    let client = match Client::open_with_options(
        Arc::new(MemoryStorage::default()),
        device,
        options,
    )
    .await
    {
        Ok(client) => client,
        Err(e) => {
            emit(error(Failed::sdk("open", e)));
            emit(Event::Done { ok: false });
            return false;
        }
    };
    let outcome = steps(&client, run, &mut emit).await;
    let ok = match outcome {
        Ok(()) => true,
        Err(failed) => {
            emit(error(failed));
            false
        }
    };
    client.close().await;
    emit(Event::Done { ok });
    ok
}

fn error(failed: Failed) -> Event {
    Event::Error {
        stage: failed.stage.into(),
        kind: failed.kind,
        error: failed.error,
    }
}

async fn steps(
    client: &Client,
    run: &FederationRun,
    emit: &mut impl FnMut(Event),
) -> Result<(), Failed> {
    client
        .add_address(&run.server, run.addrs.clone())
        .map_err(|e| Failed::sdk("connect", e))?;
    client
        .connect(&run.server)
        .await
        .map_err(|e| Failed::sdk("connect", e))?;
    client
        .pin(&run.server, true)
        .await
        .map_err(|e| Failed::sdk("connect", e))?;
    let session = client
        .sign_in(&run.server, &run.username, &run.password, false)
        .await
        .map_err(|e| Failed::sdk("sign_in", e))?;
    let mut outcome = signed_in(client, &session, run, emit).await;
    if outcome.is_ok() && (!run.download_status.is_empty() || run.prepare.is_some()) {
        outcome = download_status(run, emit).await;
    }
    // Whatever happened: the session the run made ends with it. A failure
    // here does not fail the run, and is said all the same.
    if let Err(e) = session.sign_out().await {
        emit(error(Failed::sdk("sign_out", e)));
    }
    outcome
}

async fn signed_in(
    client: &Client,
    session: &Session,
    run: &FederationRun,
    emit: &mut impl FnMut(Event),
) -> Result<(), Failed> {
    emit(Event::SignedIn {
        server: session.server(),
        user: session.user().username.clone(),
        federation: session.experimental().federation,
    });
    let libraries = session
        .libraries()
        .await
        .map_err(|e| Failed::sdk("libraries", e))?;
    for library in &libraries {
        emit(Event::Library {
            library: serde_json::to_value(library).unwrap_or_default(),
        });
    }
    let remote: Vec<&Library> = libraries.iter().filter(|l| l.server.is_some()).collect();
    let mut films = Vec::new();
    for library in &remote {
        let items = all_items(session, library).await?;
        for item in items {
            if run.list_items {
                emit(Event::Item {
                    library: library.id.to_string(),
                    id: item.id().to_string(),
                    kind: wire_field(&item, "kind"),
                    title: title_of(&item),
                });
            }
            if library.kind == LibraryKind::Movies {
                films.push(item);
            }
        }
    }
    let film = |title: &str| {
        films
            .iter()
            .find(|film| title_of(film) == title)
            .map(|film| film.id().clone())
            .ok_or_else(|| {
                Failed::other(
                    "find",
                    format!("no paired server's film is called {title:?}"),
                )
            })
    };
    if let Some(query) = &run.search {
        searched(session, query, emit).await?;
    }
    if let Some((title, from)) = &run.play_from {
        session
            .set_play_from(title, Some(from))
            .await
            .map_err(|e| Failed::sdk("play_from", e))?;
        let detail = session
            .item(title)
            .await
            .map_err(|e| Failed::sdk("play_from", e))?;
        let copies = detail
            .copies
            .ok_or_else(|| Failed::other("play_from", "the item's details have no copies"))?;
        emit(Event::Copies {
            id: title.to_string(),
            origin: copies.origin.map(|s| s.name),
            also_on: copies.also_on.iter().map(CopyOf::of).collect(),
            play_from: copies.play_from.to_string(),
        });
    }
    if let Some(download) = &run.download {
        downloaded(client, session, &film(&download.title)?, download, emit).await?;
    }
    let Some(play) = &run.play else {
        return Ok(());
    };
    let id = match &play.target {
        Target::Id(id) => id.clone(),
        Target::Title(title) => film(title)?,
    };
    played(session, &id, play, emit).await
}

/// The films a search for `query` finds, each a [`Event::Hit`].
async fn searched(
    session: &Session,
    query: &str,
    emit: &mut impl FnMut(Event),
) -> Result<(), Failed> {
    let request = SearchRequest {
        query: query.to_owned(),
        limit: 20,
        scope: SearchScope::Movies,
        kind: None,
        library: None,
    };
    let results = session
        .search_hits(request)
        .await
        .map_err(|e| Failed::sdk("search", e))?;
    let top = match results.top {
        Some(TopResult::Item(hit)) => Some(hit),
        _ => None,
    };
    let hits = top
        .into_iter()
        .chain(results.groups.into_iter().flat_map(|g| g.items));
    for hit in hits {
        let wire = serde_json::to_value(&hit.item).unwrap_or_default();
        emit(Event::Hit {
            id: hit.item.id().to_string(),
            title: title_of(&hit.item),
            year: wire.get("year").and_then(Value::as_u64),
            origin: hit.origin.map(|s| s.name),
            also_on: hit.also_on.iter().map(CopyOf::of).collect(),
            play_from: hit.play_from.to_string(),
        });
    }
    Ok(())
}

/// Download film `id` as `download` says, with every track as it is, and
/// wait for the manager to finish it.
async fn downloaded(
    client: &Client,
    session: &Session,
    id: &MediaId,
    download: &Download,
    emit: &mut impl FnMut(Event),
) -> Result<(), Failed> {
    let downloads = client.downloads();
    let started = downloads
        .start(session, id, DownloadSource::Original, DownloadTracks::All)
        .await
        .map_err(|e| Failed::sdk("download", e))?;
    tokio::time::timeout(download.within, downloads.run_until_done())
        .await
        .map_err(|_| Failed::other("download", format!("not done within {:?}", download.within)))?
        .map_err(|e| Failed::sdk("download", e))?;
    let done = downloads
        .get(&started.key)
        .map_err(|e| Failed::sdk("download", e))?;
    emit(Event::Download {
        key: done.key,
        phase: serde_json::to_value(&done.phase).unwrap_or_default(),
        size: done.size,
        received: done.received,
        file: done.file_path.map(|p| p.display().to_string()),
    });
    Ok(())
}

/// What SB says of `run.download_status` (`DownloadStatus`), asked on a
/// connection of its own, as the SDK has no call for it: the download
/// manager asks it for its own downloads only.
async fn download_status(run: &FederationRun, emit: &mut impl FnMut(Event)) -> Result<(), Failed> {
    use spoolframe_protocol::{LoginPassword, Reply, Request, Response};
    let id: iroh::EndpointId = run
        .server
        .parse()
        .map_err(|e| Failed::other("download_status", format!("{e}")))?;
    let target = run
        .addrs
        .iter()
        .fold(iroh::EndpointAddr::new(id), |addr, ip| {
            addr.with_ip_addr(*ip)
        });
    let failed = |e: crate::wire::Problem| Failed::other("download_status", format!("{e:?}"));
    let probe = crate::wire::Probe::new(target, 10, Duration::from_secs(10))
        .await
        .map_err(|e| Failed::other("download_status", e.to_string()))?;
    let outcome = async {
        let conn = probe
            .connect_hello_naming(
                vec![spoolframe_protocol::experimental::FEDERATION.to_owned()],
                vec![spoolframe_protocol::features::DOWNLOADS.to_owned()],
            )
            .await
            .map_err(failed)?;
        let login = Request::LoginPassword(LoginPassword {
            username: run.username.clone(),
            password: run.password.clone(),
            remember: false,
        });
        let session = match probe.request(&conn, None, login).await.map_err(failed)? {
            Response::Ok(Reply::LoggedIn { session, .. }) => session,
            other => {
                return Err(Failed::other(
                    "download_status",
                    crate::wire::describe(&other),
                ));
            }
        };
        let status = async |ids: Vec<String>| {
            let asked = Request::DownloadStatus { ids };
            match probe
                .request(&conn, Some(&session.0), asked)
                .await
                .map_err(failed)?
            {
                Response::Ok(Reply::Downloads { downloads, gone }) => Ok((downloads, gone)),
                other => Err(Failed::other(
                    "download_status",
                    crate::wire::describe(&other),
                )),
            }
        };
        let mut ids = run.download_status.clone();
        if let Some(item) = &run.prepare {
            let prepare = Request::PrepareDownload {
                id: item.clone(),
                source: DownloadSource::Original,
                tracks: DownloadTracks::All,
                device: None,
            };
            let id = match probe
                .request(&conn, Some(&session.0), prepare)
                .await
                .map_err(failed)?
            {
                Response::Ok(Reply::Download(info)) => info.id,
                other => {
                    return Err(Failed::other(
                        "download_status",
                        crate::wire::describe(&other),
                    ));
                }
            };
            // Until the file is made: then it waits for this device, which
            // reads none of it.
            for _ in 0..60 {
                let (downloads, _) = status(vec![id.clone()]).await?;
                if downloads.iter().any(|d| {
                    !matches!(
                        d.state,
                        DownloadState::Waiting
                            | DownloadState::Converting { .. }
                            | DownloadState::Writing { .. }
                    )
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            ids.push(id);
        }
        let (downloads, gone) = status(ids).await?;
        emit(Event::DownloadStatus {
            downloads: downloads
                .iter()
                .map(|d| serde_json::to_value(d).unwrap_or_default())
                .collect(),
            gone,
        });
        let _ = probe
            .request(&conn, Some(&session.0), Request::Logout)
            .await;
        Ok(())
    }
    .await;
    probe.close().await;
    outcome
}

/// Every top-level item of `library`, page by page.
async fn all_items(session: &Session, library: &Library) -> Result<Vec<Item>, Failed> {
    const PAGE: u32 = 100;
    let mut items = Vec::new();
    loop {
        let offset = u32::try_from(items.len()).unwrap_or(u32::MAX);
        let page = session
            .items(&library.id, offset, PAGE)
            .await
            .map_err(|e| Failed::sdk("items", e))?;
        let got = page.items.len();
        items.extend(page.items);
        if got == 0 || items.len() >= page.total as usize {
            return Ok(items);
        }
    }
}

async fn played(
    session: &Session,
    id: &MediaId,
    play: &Play,
    emit: &mut impl FnMut(Event),
) -> Result<(), Failed> {
    let selection = Some(Selection::default());
    let remote = if play.proxied {
        session
            .start_proxied_playback(id, selection, PlaybackOptions::default())
            .await
    } else {
        session
            .start_remote_playback(id, selection, PlaybackOptions::default())
            .await
    }
    .map_err(|e| Failed::sdk("play", e))?;
    let (route, path) = route(&remote);
    emit(Event::Playing {
        item: id.to_string(),
        played_as: remote.item.to_string(),
        mode: match remote.mode {
            RemoteMode::Direct => "direct",
            RemoteMode::Proxied => "proxied",
        }
        .into(),
        route: route.into(),
        path: path.into(),
        session_server: remote.session.server(),
        plan: plan_name(&remote.playback.plan).into(),
        size: remote.playback.size,
    });
    let read = read_all(&remote, play).await;
    // Stopped whether the read worked or not.
    let stopped = remote
        .session
        .stop_playback(&remote.playback.handle)
        .await
        .map_err(|e| Failed::sdk("stop", e));
    let (bytes, complete) = read?;
    emit(Event::Read {
        bytes,
        complete,
        out: play.out.as_ref().map(|p| p.display().to_string()),
    });
    stopped?;
    emit(Event::Stopped);
    if let Some(position) = play.progress {
        session
            .update_watch_progress(id, position)
            .await
            .map_err(|e| Failed::sdk("progress", e))?;
        let detail = session
            .item(id)
            .await
            .map_err(|e| Failed::sdk("progress", e))?;
        emit(Event::Progress {
            reported_ms: u64::try_from(position.as_millis()).unwrap_or(u64::MAX),
            stored_ms: position_ms(&detail.item),
        });
    }
    Ok(())
}

/// Read the playback's file from the start, to its end or
/// [`Play::max_bytes`], into [`Play::out`]: how many bytes, and whether
/// that is the whole file.
async fn read_all(remote: &RemotePlayback, play: &Play) -> Result<(u64, bool), Failed> {
    if !matches!(remote.playback.plan, PlaybackPlan::Direct { .. }) {
        return Err(Failed::other(
            "read",
            format!(
                "the film is planned as {}, not played as it is",
                plan_name(&remote.playback.plan)
            ),
        ));
    }
    let size = remote.playback.size;
    let limit = play.max_bytes.map_or(size, |max| max.min(size));
    let read_size = play.read_size.map_or(MAX_READ, |n| n.clamp(1, MAX_READ));
    let mut out =
        match &play.out {
            Some(path) => Some(std::fs::File::create(path).map_err(|e| {
                Failed::other("read", format!("cannot write {}: {e}", path.display()))
            })?),
            None => None,
        };
    let mut offset = 0;
    while offset < limit {
        let length = (limit - offset).min(read_size);
        let bytes = remote
            .session
            .read(&remote.playback.handle, offset, length)
            .await
            .map_err(|e| Failed::sdk("read", e))?;
        if bytes.is_empty() {
            break;
        }
        if let Some(file) = &mut out {
            file.write_all(&bytes)
                .map_err(|e| Failed::other("read", format!("cannot write the bytes read: {e}")))?;
        }
        offset += bytes.len() as u64;
    }
    Ok((offset, offset == size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_is_named_by_its_variant() {
        assert_eq!(kind_of(&SdkError::OriginOffline), "OriginOffline");
        assert_eq!(
            kind_of(&SdkError::Unreachable("gone".into())),
            "Unreachable"
        );
    }

    #[test]
    fn each_event_is_one_object_named_by_its_event() {
        let line = serde_json::to_value(Event::Playing {
            item: "fed:x".into(),
            played_as: "plex:movie:1".into(),
            mode: "direct".into(),
            route: "direct".into(),
            path: "lan".into(),
            session_server: "ab".into(),
            plan: "direct".into(),
            size: 7,
        })
        .unwrap();
        assert_eq!(line["event"], "playing");
        assert_eq!(line["route"], "direct");
        let done = serde_json::to_value(Event::Done { ok: false }).unwrap();
        assert_eq!(done, serde_json::json!({ "event": "done", "ok": false }));
    }
}
