//! The download manager (PX-38): one task per client, moving one download
//! at a time, oldest first, and sending offline watch entries.

use crate::session::unexpected;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use spoolframe_protocol::downloads::{
    DownloadFailure, DownloadInfo, DownloadSource, DownloadState, DownloadTracks, MAX_ASKED,
    OfflineWatchOutcome,
};
use spoolframe_protocol::resource::{ResourceMetadata, ResourceRequest, ResourceResponse};
use spoolframe_protocol::{ArtworkSize, Reply, Request};
use spoolframe_types::{ArtworkId, Item, MediaId};

use super::store::{EntryKind, Hold, Manifest, Store, StoredArtwork, sha256_file};
use super::writer::{BlockWriter, Unfinished};
use super::{DownloadEvent, DownloadPhase, LocalWatch, Shared, SyncedWatch, disk, music, now};
use crate::client::{Client, Inner, parse_id};
use crate::{SdkError, Session};

/// How much one read asks for.
const READ: u64 = 8 << 20;
/// How much is written between two syncs of the file and its offset: at
/// most this much is read again after a crash.
const SYNC_EVERY: u64 = 64 << 20;
/// Room left free beside a download, on top of its size.
const HEADROOM: u64 = 1 << 30;
/// How often a download being prepared is asked about, events or not.
const POLL: Duration = Duration::from_secs(5);
/// The first wait before trying a download again, doubling to
/// [`LONGEST_WAIT`].
const FIRST_WAIT: Duration = Duration::from_secs(1);
const LONGEST_WAIT: Duration = Duration::from_secs(5 * 60);

/// Until the client is closed or gone: send what is wanted of offline
/// watch, then move the next download due, else wait for something to
/// do. A download the server is still making is asked about again after
/// [`POLL`] or when the server says it moved, and the ones behind it go
/// on meanwhile: an episode converting for half an hour holds up no
/// album.
pub(super) async fn run(client: Weak<Inner>, shared: Arc<Shared>) {
    // Before anything is prepared: an event sent meanwhile is not missed.
    let mut server_events = shared.server_events.subscribe();
    if let Some(store) = &shared.store {
        let mut users = store.carried_users();
        users.extend(music::users_to_sync(store));
        for (server, user) in users {
            if let Ok(server) = parse_id(&server) {
                shared.sync_wanted.lock().insert((server, user));
            }
        }
        // Tracks being kept as they played when the client stopped: none
        // is whole, and a part is not gone on with.
        if let Err(e) = std::fs::remove_dir_all(store.listening_parts())
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(error = %e, "tracks part kept from listening not removed");
        }
    }
    loop {
        if *shared.closed.borrow() {
            return;
        }
        let wanted = shared.sync_wanted.lock().iter().next().cloned();
        if let Some((server, user)) = wanted {
            shared.sync_wanted.lock().remove(&(server, user.clone()));
            let Some(inner) = client.upgrade() else {
                return;
            };
            let client = Client { inner };
            match session_for(&client, &shared, &server.to_string(), &user).await {
                Ok(session) => {
                    if let Err(e) = sync(&shared, &session).await {
                        tracing::info!(%server, error = %e, "offline watch not sent; sent with the next session");
                    }
                    if let Err(e) = music::sync(&shared, &session).await {
                        tracing::info!(%server, error = %e, "music not synced; synced with the next session");
                    }
                }
                Err(e) => {
                    tracing::debug!(%server, error = %e, "no session to send offline watch in")
                }
            }
            continue;
        }
        match next(&shared) {
            Next::Drive(key) => {
                let Some(inner) = client.upgrade() else {
                    return;
                };
                drive(&Client { inner }, &shared, &key).await;
            }
            Next::Wait(until) => {
                let mut closed = shared.closed.subscribe();
                tokio::select! {
                    () = shared.wake.notified() => {}
                    () = sleep_until(until) => {}
                    change = server_events.recv() => {
                        // A preparation moved (or events were missed): every
                        // one being made is asked about now.
                        if !matches!(change, Err(tokio::sync::broadcast::error::RecvError::Closed)) {
                            shared.check_at.lock().clear();
                        }
                    }
                    _ = closed.wait_for(|closed| *closed) => return,
                }
            }
        }
    }
}

pub(super) async fn sleep_until(until: Option<tokio::time::Instant>) {
    match until {
        Some(until) => tokio::time::sleep_until(until).await,
        None => std::future::pending().await,
    }
}

enum Next {
    Drive(String),
    /// Nothing due: until the first that waits to try again, if any.
    Wait(Option<tokio::time::Instant>),
}

/// The oldest download under way that is not stopping or waiting.
fn next(shared: &Shared) -> Next {
    let Some(store) = &shared.store else {
        return Next::Wait(None);
    };
    // On a mobile network downloads may not use: until it changes.
    if shared.network_held() {
        return Next::Wait(None);
    }
    let now = tokio::time::Instant::now();
    let mut soonest = None;
    for key in store.active() {
        if shared.stopping.lock().contains(&key) {
            continue;
        }
        let retry = shared.retry_at.lock().get(&key).map(|(at, _)| *at);
        let check = shared.check_at.lock().get(&key).copied();
        match retry.into_iter().chain(check).filter(|at| *at > now).max() {
            Some(at) => {
                soonest = Some(soonest.map_or(at, |s: tokio::time::Instant| s.min(at)));
            }
            None => return Next::Drive(key),
        }
    }
    Next::Wait(soonest)
}

/// What one step of a download came to.
enum Step {
    /// Go on with the next step at once.
    Continue,
    /// Let go of it: it is done, failed, paused or gone, or the client is
    /// closing.
    Stop,
}

/// Move download `key` on until it is done, fails, is stopped, or must
/// wait to try again.
async fn drive(client: &Client, shared: &Arc<Shared>, key: &str) {
    let claim = shared.claim(key);
    let _claimed = claim.lock().await;
    loop {
        if shared.should_stop(key) {
            return;
        }
        let Some(store) = &shared.store else {
            return;
        };
        let Some(m) = store.find(key).filter(|m| m.phase.is_active()) else {
            store.recheck(key);
            return;
        };
        let step = match session_for(client, shared, &m.server, &m.user_id).await {
            Ok(session) => match m.phase {
                DownloadPhase::Preparing { .. } => preparing(shared, &session, m).await,
                _ => transferring(shared, &session, m).await,
            },
            Err(e) => Err(e),
        };
        shared.received.lock().remove(key);
        match step {
            Ok(Step::Continue) => {
                shared.retry_at.lock().remove(key);
                shared.check_at.lock().remove(key);
            }
            Ok(Step::Stop) => return,
            Err(e) => {
                failed(shared, key, e).await;
                return;
            }
        }
    }
}

/// The session of `user_id` on `server` that has downloads: the one last
/// made or found, else one of the server's profiles here.
async fn session_for(
    client: &Client,
    shared: &Shared,
    server: &str,
    user_id: &str,
) -> Result<Session, SdkError> {
    let id = parse_id(server)?;
    let known = shared
        .sessions
        .lock()
        .get(&(id, user_id.to_owned()))
        .cloned();
    if let Some(parts) = known {
        return Ok(parts.attach(client.clone()));
    }
    client
        .profiles(server)
        .await?
        .into_iter()
        .find(|s| s.user().user_id == user_id)
        .ok_or(SdkError::SignInRequired)
}

/// What to do after `error` moving download `key`: wait and try again
/// when it may pass by itself, else fail it, saying why.
async fn failed(shared: &Shared, key: &str, error: SdkError) {
    let note = match &error {
        SdkError::Unreachable(_)
        | SdkError::TimedOut(_)
        | SdkError::Busy
        | SdkError::Unavailable(_)
        | SdkError::RateLimited { .. }
        | SdkError::ServerRestarted
        | SdkError::ProfileLocked
        | SdkError::DownloadNotReady => Some(format!("Waiting for the server: {error}")),
        SdkError::SignInRequired => {
            if let Some(m) = shared.store.as_ref().and_then(|s| s.find(key))
                && let Ok(server) = parse_id(&m.server)
            {
                shared.sessions.lock().remove(&(server, m.user_id.clone()));
            }
            Some("Sign in to the server again to go on".to_owned())
        }
        _ => None,
    };
    match note {
        Some(note) => {
            tracing::debug!(key, error = %error, "a download waits to try again");
            {
                let mut retry = shared.retry_at.lock();
                let wait = retry
                    .get(key)
                    .map_or(FIRST_WAIT, |(_, wait)| (*wait * 2).min(LONGEST_WAIT));
                retry.insert(key.to_owned(), (tokio::time::Instant::now() + wait, wait));
            }
            let _ = shared
                .update(key, |m| {
                    m.note = Some(note);
                    Ok(())
                })
                .await;
        }
        None => {
            tracing::info!(key, error = %error, "a download failed");
            let _ = shared
                .update(key, |m| {
                    if m.phase.is_active() {
                        m.phase = DownloadPhase::Failed {
                            reason: error.to_string(),
                            cause: cause_of(&error),
                        };
                    }
                    m.note = None;
                    Ok(())
                })
                .await;
        }
    }
}

/// The cause of a download failed for `error`: talking to the server
/// (an answer cut off or one that made no sense) is the network's.
fn cause_of(error: &SdkError) -> DownloadFailure {
    if error.is_transport() || matches!(error, SdkError::Protocol(_)) {
        DownloadFailure::Network
    } else {
        DownloadFailure::Other
    }
}

/// The phase of a download that needs `needed` bytes free where
/// `available` are: failed for room, with how much more, when they are
/// not.
fn no_room(needed: u64, available: u64) -> Option<DownloadPhase> {
    let short = needed.checked_sub(available).filter(|&short| short > 0)?;
    Some(DownloadPhase::Failed {
        reason: format!(
            "Not enough space on this device: {} MB more needed",
            short.div_ceil(1 << 20)
        ),
        cause: DownloadFailure::NoSpace {
            needed_bytes: short,
        },
    })
}

/// Ask the server to prepare `id` (the same id each time for the same
/// request).
pub(super) async fn prepare(
    session: &Session,
    id: &MediaId,
    source: &DownloadSource,
    tracks: &DownloadTracks,
) -> Result<DownloadInfo, SdkError> {
    let request = Request::PrepareDownload {
        id: id.clone(),
        source: source.clone(),
        tracks: tracks.clone(),
        device: None,
    };
    match session.call(request).await? {
        Reply::Download(info) => Ok(info),
        other => Err(unexpected(other)),
    }
}

async fn status(session: &Session, id: &str) -> Result<DownloadInfo, SdkError> {
    let request = Request::DownloadStatus {
        ids: vec![id.to_owned()],
    };
    status_of(session.call(request).await?, id)
}

/// Download `id` as `reply` to asking about it alone says. One the server
/// has not (cancelled, or its rows dropped) is `NotFound`: prepared again.
fn status_of(reply: Reply, id: &str) -> Result<DownloadInfo, SdkError> {
    match reply {
        Reply::Downloads { mut downloads, .. } if downloads.len() == 1 => Ok(downloads.remove(0)),
        Reply::Downloads { gone, .. } if gone.iter().any(|g| g == id) => {
            Err(SdkError::NotFound(format!("download {id}")))
        }
        other => Err(unexpected(other)),
    }
}

/// Take in what the server says of `m`'s download. A ready file other
/// than the one being transferred (another size or SHA-256) starts the
/// transfer over.
pub(super) fn take(m: &mut Manifest, info: DownloadInfo) {
    m.id = Some(info.id);
    if info.file.is_some() {
        m.file = info.file;
    }
    // Said when it is prepared and when it is asked about; an older server
    // says it only when it is prepared, and the one known stays.
    if info.expected_size.is_some() {
        m.expected_size = info.expected_size;
    }
    m.phase = match info.state {
        DownloadState::Ready { size, sha256 } => {
            if m.size != Some(size) || m.sha256.as_deref() != Some(sha256.as_str()) {
                m.offset = 0;
            }
            m.size = Some(size);
            m.sha256 = Some(sha256);
            DownloadPhase::Transferring
        }
        DownloadState::Failed { reason, cause } => DownloadPhase::Failed { reason, cause },
        server => DownloadPhase::Preparing { server },
    };
}

/// An episode's show and season, as the server has them now; `None` for
/// what cannot be read.
pub(super) async fn parents(session: &Session, item: &Item) -> (Option<Item>, Option<Item>) {
    let Item::Episode(episode) = item else {
        return (None, None);
    };
    let of = |id: MediaId| async move { session.item(&id).await.ok().map(|d| d.item) };
    (
        of(episode.show_id.clone()).await,
        of(episode.season_id.clone()).await,
    )
}

/// Pictures kept already, by artwork id: where one is on disk, for the
/// next entry with it to link to instead of fetching it again.
pub(super) type KeptArt = HashMap<ArtworkId, PathBuf>;

/// The artwork of `m`'s item, show and season (a track's album and
/// artist), kept in its `art` directory: linked to a copy in `kept_art`,
/// else fetched. A picture that cannot be fetched is left out.
pub(super) async fn fetch_artwork(
    session: &Session,
    store: &Store,
    m: &Manifest,
    kept_art: &mut KeptArt,
) -> Vec<StoredArtwork> {
    let dir = store.art_dir(m);
    let mut kept = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for item in [
        Some(&m.item),
        m.show.as_ref(),
        m.season.as_ref(),
        m.album.as_ref(),
        m.artist.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        let artwork = match item {
            Item::Movie(i) => &i.artwork,
            Item::Episode(i) => &i.artwork,
            Item::Show(i) => &i.artwork,
            Item::Season(i) => &i.artwork,
            Item::Track(i) => &i.artwork,
            Item::Album(i) => &i.artwork,
            Item::Artist(i) => &i.artwork,
        };
        for art in artwork {
            // A track's poster is its album's cover: kept once.
            if !seen.insert(art.id.clone()) {
                continue;
            }
            let file = format!(
                "{}.jpg",
                &blake3::hash(art.id.to_string().as_bytes()).to_hex()[..16]
            );
            let path = dir.join(&file);
            if !kept_art.get(&art.id).is_some_and(|from| reuse(from, &path)) {
                let bytes = match session.artwork(&art.id, ArtworkSize::Large).await {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        tracing::debug!(id = %art.id, error = %e, "artwork not kept with a download");
                        continue;
                    }
                };
                let written =
                    std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, &bytes));
                if let Err(e) = written {
                    tracing::warn!(error = %e, "artwork not kept with a download");
                    continue;
                }
            }
            kept_art.insert(art.id.clone(), path);
            kept.push(StoredArtwork {
                item: item.id().clone(),
                kind: art.kind,
                file,
            });
        }
    }
    kept
}

/// `to` is the picture at `from` too: a hard link to it, else a copy.
/// False when `from` is gone (its entry removed): it is fetched again.
fn reuse(from: &Path, to: &Path) -> bool {
    if from == to {
        return from.exists();
    }
    let Some(dir) = to.parent() else {
        return false;
    };
    let _ = std::fs::remove_file(to);
    std::fs::create_dir_all(dir).is_ok()
        && (std::fs::hard_link(from, to).is_ok() || std::fs::copy(from, to).is_ok())
}

/// Prepare `m`'s download on the server, or ask how it goes, and wait
/// for it to move.
async fn preparing(shared: &Shared, session: &Session, m: Manifest) -> Result<Step, SdkError> {
    // Before asking: an event sent while the answer comes is not missed.
    let mut events = shared.server_events.subscribe();
    let info = match &m.id {
        Some(id) => match status(session, id).await {
            // The server no longer has it (cancelled, or its rows
            // dropped): prepared again, the same file comes back.
            Err(SdkError::NotFound(_)) => {
                prepare(session, m.item.id(), &m.source, &m.chosen).await?
            }
            other => other?,
        },
        None => prepare(session, m.item.id(), &m.source, &m.chosen).await?,
    };
    let id = info.id.clone();
    let m = shared
        .update(&m.key, |m| {
            // Paused or removed while asking, or handed to another user
            // of the device (store::Store::release): as it is.
            if matches!(m.phase, DownloadPhase::Preparing { .. })
                && m.user_id == session.user().user_id
            {
                take(m, info);
                m.username = session.user().username.clone();
                m.note = None;
            }
            Ok(())
        })
        .await?;
    match m.phase {
        DownloadPhase::Transferring => return Ok(Step::Continue),
        DownloadPhase::Preparing { .. } => {}
        _ => return Ok(Step::Stop),
    }
    // Told it moved while asking: asked again at once.
    let server = session.server;
    while let Ok(change) = events.try_recv() {
        if change.server == server && change.id == id {
            return Ok(Step::Continue);
        }
    }
    // Still being made: the others go on until it is asked about again.
    shared
        .check_at
        .lock()
        .insert(m.key.clone(), tokio::time::Instant::now() + POLL);
    Ok(Step::Stop)
}

/// Read `m`'s file from where it stopped, check it whole and keep it.
async fn transferring(
    shared: &Arc<Shared>,
    session: &Session,
    m: Manifest,
) -> Result<Step, SdkError> {
    let store = shared.store()?;
    let (Some(id), Some(size), Some(sha256)) = (m.id.clone(), m.size, m.sha256.clone()) else {
        return to_prepare(shared, &m.key, false).await;
    };
    // Kept before its manifest said so (a crash between the two).
    if let Some(media) = store.media(&m)
        && media.exists()
        && sha256_file(&media).await.map_err(disk)? == sha256
    {
        return finish(shared, session, &m, &id, size).await;
    }
    // A track kept from listening that is this very file: moved here,
    // with nothing fetched.
    if m.kind == EntryKind::Track
        && let Some(kept) = music::kept(store, &m.server, m.item.id())
        && kept.size == Some(size)
        && let (Some(from), Some(to)) = (store.media(&kept), store.media(&m))
        && music::kept_is(&kept, &from, &sha256).await
    {
        let moved = {
            let _writing = shared.manifests.lock().await;
            std::fs::create_dir_all(store.dir(&m))
                .and_then(|()| std::fs::rename(&from, &to))
                .and_then(|()| store.remove(&kept))
        };
        match moved {
            Ok(()) => {
                tracing::debug!(
                    key = m.key,
                    "a kept track became a download, fetched from nothing"
                );
                let _ = shared.events.send(DownloadEvent::Removed {
                    key: kept.key.clone(),
                });
                shared
                    .update(&m.key, |m| {
                        m.last_played_at = kept.last_played_at.max(m.last_played_at);
                        // Still the device's too: every profile plays it,
                        // and it is cache again once the download goes.
                        m.add_hold(&Hold::listening());
                        Ok(())
                    })
                    .await?;
                return finish(shared, session, &m, &id, size).await;
            }
            Err(e) => tracing::info!(error = %e, "a kept track not moved to its download; fetched"),
        }
    }
    let part = store.part(&m);
    let mut writer = BlockWriter::open(&part, size, m.offset)
        .await
        .map_err(disk)?;
    let needed = (size - writer.contiguous()).saturating_add(HEADROOM);
    if let Some(failed) = no_room(needed, shared.available(store)?) {
        return fail(shared, &m.key, failed).await;
    }
    let save = |offset: u64| {
        let sha256 = sha256.clone();
        shared.update(&m.key, move |m| {
            if m.sha256.as_deref() == Some(sha256.as_str()) {
                m.offset = offset;
            }
            Ok(())
        })
    };
    // A paired server's item (P8-07) is read straight from that server
    // when this device reaches it, else through the user's own server.
    let mut direct = remote_source(session, &m, &id).await;
    while !writer.is_complete() {
        let offset = writer.contiguous();
        if shared.should_stop(&m.key) {
            writer.sync().await.map_err(disk)?;
            save(offset).await?;
            return Ok(Step::Stop);
        }
        let read = match &direct {
            Some(remote) => read(&remote.session, &remote.id, offset).await,
            None => read(session, &id, offset).await,
        };
        let read = match read {
            Ok(read) => read,
            // The paired server stopped answering this device: the rest
            // comes through the user's own server.
            Err(e) if direct.is_some() => {
                tracing::info!(key = m.key, error = %e, "a paired server's download goes on through this session's server");
                direct = None;
                continue;
            }
            Err(e) => {
                writer.sync().await.map_err(disk)?;
                save(offset).await?;
                return match e {
                    // Gone from the server (cancelled, or removed a day
                    // after its last read): prepared again, and the same
                    // file goes on from here.
                    SdkError::NotFound(_) => to_prepare(shared, &m.key, true).await,
                    SdkError::DownloadNotReady => to_prepare(shared, &m.key, false).await,
                    e => Err(e),
                };
            }
        };
        let (told_size, bytes) = read;
        if told_size != size {
            // Another file under the same id: as the server says now.
            writer.sync().await.map_err(disk)?;
            return to_prepare(shared, &m.key, false).await;
        }
        if bytes.is_empty() {
            return Err(SdkError::Protocol(format!(
                "an empty read at {offset} of {size} bytes"
            )));
        }
        writer.write_at(offset, &bytes).await.map_err(disk)?;
        shared
            .received
            .lock()
            .insert(m.key.clone(), writer.contiguous());
        if writer.unsynced() >= SYNC_EVERY {
            writer.sync().await.map_err(disk)?;
            save(writer.contiguous()).await?;
        } else {
            shared.emit(&m);
        }
    }
    // Whole: the paired server's session for this download is ended, and
    // its place under that server's token caps let go (P8-07).
    if let Some(remote) = direct.take()
        && let Err(e) = remote.session.call(Request::Logout).await
    {
        tracing::debug!(key = m.key, error = %e, "a paired server's download session was not ended");
    }
    let media = store
        .media(&m)
        .ok_or_else(|| SdkError::Protocol("a ready download with no container".into()))?;
    match writer.finish(&media, Some(&sha256)).await {
        Ok(()) => finish(shared, session, &m, &id, size).await,
        Err(Unfinished::Damaged) => {
            shared
                .update(&m.key, |m| {
                    m.offset = 0;
                    Ok(())
                })
                .await?;
            fail(
                shared,
                &m.key,
                DownloadPhase::Failed {
                    reason: "The file arrived damaged; try again".to_owned(),
                    cause: DownloadFailure::Network,
                },
            )
            .await
        }
        Err(Unfinished::Io(e)) => Err(disk(e)),
        Err(Unfinished::Incomplete) => unreachable!("the loop ends once it is whole"),
    }
}

/// Where `m`'s download `id` is read from when it is a paired server's
/// item (P8-07) and that server is reached: its session there for that
/// download. `None` reads it through `session`'s server.
async fn remote_source(session: &Session, m: &Manifest, id: &str) -> Option<crate::RemoteDownload> {
    if m.item.id().namespace() != spoolframe_types::Namespace::Federated
        || !session.experimental().federation
    {
        return None;
    }
    match session.remote_download(m.item.id(), id).await {
        Ok(remote) => Some(remote),
        Err(e) => {
            tracing::info!(key = m.key, error = %e, "a paired server's download is read through this session's server");
            None
        }
    }
}

/// One read of download `id` from `offset`: the file's size as the server
/// told it, and the bytes.
async fn read(session: &Session, id: &str, offset: u64) -> Result<(u64, Vec<u8>), SdkError> {
    let request = ResourceRequest::Download {
        session: session.token.clone(),
        id: id.to_owned(),
        offset,
        length: READ,
    };
    let timeouts = session.client.inner.timeouts;
    let request = &request;
    let (response, bytes) = session
        .client
        .on_connection(
            session.server,
            Some(&session.token),
            true,
            |conn| async move {
                conn.resource(request, READ, timeouts.of_read(), timeouts.request)
                    .await
            },
        )
        .await?;
    match response {
        ResourceResponse::Ready {
            metadata: ResourceMetadata::Download { size },
            ..
        } => Ok((size, bytes)),
        ResourceResponse::Error { error } => Err(session.failed(error).await),
        other => Err(SdkError::Protocol(format!(
            "unexpected answer to a download read: {other:?}"
        ))),
    }
}

/// Download `key` is to be prepared again; `forget` its server id, which
/// the server no longer knows.
async fn to_prepare(shared: &Shared, key: &str, forget: bool) -> Result<Step, SdkError> {
    shared
        .update(key, |m| {
            if forget {
                m.id = None;
            }
            if m.phase == DownloadPhase::Transferring {
                m.phase = DownloadPhase::Preparing {
                    server: DownloadState::Waiting,
                };
            }
            Ok(())
        })
        .await?;
    Ok(Step::Continue)
}

/// Download `key` is `failed`, a [`DownloadPhase::Failed`].
async fn fail(shared: &Shared, key: &str, failed: DownloadPhase) -> Result<Step, SdkError> {
    shared
        .update(key, |m| {
            m.phase = failed;
            Ok(())
        })
        .await?;
    Ok(Step::Stop)
}

/// `m`'s file is here, whole and checked: the server is told it may let
/// it go (once; one not told keeps it until a day after the last read),
/// and it is done.
async fn finish(
    shared: &Shared,
    session: &Session,
    m: &Manifest,
    id: &str,
    size: u64,
) -> Result<Step, SdkError> {
    if let Err(e) = cancel(session, id).await {
        tracing::info!(id, error = %e, "the server was not told a download finished");
    }
    shared
        .update(&m.key, |m| {
            m.phase = DownloadPhase::Done;
            m.offset = size;
            m.note = None;
            Ok(())
        })
        .await?;
    // The same track kept from listening in another quality: the download
    // stands for it now.
    if m.kind == EntryKind::Track
        && let Some(store) = &shared.store
        && let Some(kept) = music::kept(store, &m.server, m.item.id())
        && kept.cached_only()
    {
        let removed = {
            let _writing = shared.manifests.lock().await;
            store.remove(&kept)
        };
        if removed.is_ok() {
            let _ = shared.events.send(DownloadEvent::Removed { key: kept.key });
        }
    }
    Ok(Step::Stop)
}

/// `CancelDownload`: one the server no longer has is gone already.
async fn cancel(session: &Session, id: &str) -> Result<(), SdkError> {
    match session
        .call(Request::CancelDownload { id: id.to_owned() })
        .await
    {
        Ok(Reply::Done) | Err(SdkError::NotFound(_)) => Ok(()),
        Ok(other) => Err(unexpected(other)),
        Err(e) => Err(e),
    }
}

/// Cancel `m`'s download `id` on the server, with a session there now; a
/// server that cannot be reached is not waited for.
pub(super) async fn cancel_if_reachable(client: &Client, shared: &Shared, m: &Manifest, id: &str) {
    let Ok(server) = parse_id(&m.server) else {
        return;
    };
    let known = shared
        .sessions
        .lock()
        .get(&(server, m.user_id.clone()))
        .cloned();
    let Some(parts) = known else {
        return;
    };
    let session = parts.attach(client.clone());
    let limit = client.inner.timeouts.request;
    match tokio::time::timeout(limit, cancel(&session, id)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::info!(id, error = %e, "a removed download was not cancelled"),
        Err(_) => tracing::info!(id, "a removed download was not cancelled in time"),
    }
}

/// Send `session`'s user's offline watch on its server: every pending
/// entry of their downloads there, and of ones deleted since.
pub(super) async fn sync(shared: &Shared, session: &Session) -> Result<Vec<SyncedWatch>, SdkError> {
    let store = shared.store()?;
    let server = session.server();
    let user_id = session.user().user_id.clone();
    let mut sending = Vec::new();
    for m in store.scan() {
        if m.server == server
            && let Some(watch) = m.watches.get(&user_id)
            && watch.pending
            && let Some(entry) = watch.entry(&m.item)
        {
            sending.push((Some(m.key.clone()), entry));
        }
    }
    sending.extend(
        store
            .carried(&server, &user_id)
            .into_iter()
            .map(|entry| (None, entry)),
    );
    let mut synced = Vec::new();
    for batch in sending.chunks(MAX_ASKED) {
        let request = Request::SyncOfflineWatch {
            device_now: now(),
            entries: batch.iter().map(|(_, e)| e.clone()).collect(),
        };
        let outcomes = match session.call(request).await? {
            Reply::OfflineWatchSynced { outcomes } if outcomes.len() == batch.len() => outcomes,
            other => return Err(unexpected(other)),
        };
        for ((key, entry), outcome) in batch.iter().zip(outcomes) {
            synced.push(SyncedWatch {
                item: entry.id.clone(),
                outcome,
            });
            if outcome == OfflineWatchOutcome::Unknown {
                continue;
            }
            match key {
                Some(key) => {
                    let server_state = match outcome {
                        OfflineWatchOutcome::Older => {
                            session.item(&entry.id).await.ok().map(|d| d.item)
                        }
                        _ => None,
                    };
                    let at = entry.at;
                    let _ = shared
                        .update(key, |m| {
                            // Watched again while this was on the way: that
                            // is sent next time.
                            let watch = m.watch_mut(&user_id);
                            if watch.at == at {
                                watch.pending = false;
                                if let Some(item) = &server_state {
                                    *watch = LocalWatch::of(item);
                                }
                            }
                            Ok(())
                        })
                        .await;
                }
                None => {
                    let _writing = shared.manifests.lock().await;
                    let mut carried = store.carried(&server, &user_id);
                    carried.retain(|c| !(c.id == entry.id && c.at == entry.at));
                    store
                        .set_carried(&server, &user_id, &carried)
                        .map_err(disk)?;
                }
            }
        }
    }
    Ok(synced)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// sf-ios, 2026-10-21: album tracks sat waiting behind an episode the
    /// server took half an hour to convert. One still being made lets the
    /// next go until it is asked about again.
    #[tokio::test]
    async fn a_download_the_server_is_still_making_lets_the_next_one_go() {
        let dir = std::env::temp_dir().join(format!("sf-sdk-next-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let shared = Shared::new(Some(dir));
        let store = shared.store().unwrap();
        let film = Item::Movie(spoolframe_types::Movie {
            id: MediaId::new(
                spoolframe_types::Namespace::Plex,
                spoolframe_types::MediaKind::Movie,
                "1",
            )
            .unwrap(),
            title: "Alien".into(),
            year: None,
            release_date: None,
            summary: None,
            content_rating: None,
            duration: None,
            cast: Vec::new(),
            artwork: Vec::new(),
            playback: spoolframe_types::PlaybackState::default(),
        });
        for (key, created_at) in [("aa", 1), ("bb", 2)] {
            let m = Store::manifest(
                EntryKind::Film,
                key.into(),
                "s".into(),
                "u".into(),
                "u".into(),
                film.clone(),
                spoolframe_types::MediaTracks::default(),
                spoolframe_protocol::downloads::DownloadSource::Original,
                spoolframe_protocol::downloads::DownloadTracks::All,
                LocalWatch::default(),
                created_at,
            );
            store.write(&m).unwrap();
        }
        let driven = |shared: &Shared| match next(shared) {
            Next::Drive(key) => Some(key),
            Next::Wait(_) => None,
        };
        assert_eq!(driven(&shared).as_deref(), Some("aa"));
        let later = tokio::time::Instant::now() + POLL;
        shared.check_at.lock().insert("aa".into(), later);
        assert_eq!(driven(&shared).as_deref(), Some("bb"));
        shared.check_at.lock().insert("bb".into(), later);
        assert!(matches!(next(&shared), Next::Wait(Some(at)) if at == later));
        // A server event asks about them all again.
        shared.check_at.lock().clear();
        assert_eq!(driven(&shared).as_deref(), Some("aa"));
    }

    /// A picture kept already is linked, not fetched; one whose entry is
    /// gone is fetched again.
    #[test]
    fn a_kept_picture_is_linked_while_it_is_there() {
        let dir = std::env::temp_dir().join(format!("sf-sdk-art-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let from = dir.join("a.jpg");
        std::fs::write(&from, b"cover").unwrap();
        let to = dir.join("b").join("art").join("a.jpg");
        let linked = reuse(&from, &to);
        let again = reuse(&from, &from);
        std::fs::remove_file(&from).unwrap();
        assert_eq!(
            (
                linked,
                again,
                std::fs::read(&to).unwrap(),
                reuse(&from, &to)
            ),
            (true, true, b"cover".to_vec(), false)
        );
    }

    /// Short of room, a download fails saying how much more is needed, to
    /// the byte; with room, it goes on.
    #[test]
    fn short_of_room_a_download_fails_with_the_bytes_to_free() {
        let gib = 1u64 << 30;
        assert_eq!(
            (no_room(3 * gib + 1, gib), no_room(gib, gib)),
            (
                Some(DownloadPhase::Failed {
                    reason: "Not enough space on this device: 2049 MB more needed".into(),
                    cause: DownloadFailure::NoSpace {
                        needed_bytes: 2 * gib + 1
                    },
                }),
                None
            )
        );
    }

    /// An answer that made no sense, or a connection that broke, is the
    /// network's; anything else, as the server or the disk says, other.
    #[test]
    fn a_failure_talking_to_the_server_is_the_network_s() {
        assert_eq!(
            [
                SdkError::Protocol("an empty read".into()),
                SdkError::Unreachable("s".into()),
                SdkError::Server("boom".into()),
                SdkError::Downloads("disk".into()),
            ]
            .map(|e| cause_of(&e)),
            [
                DownloadFailure::Network,
                DownloadFailure::Network,
                DownloadFailure::Other,
                DownloadFailure::Other,
            ]
        );
    }

    /// The size the server expects when it prepares a download is kept,
    /// and asking about it later, which does not say, keeps it.
    #[test]
    fn the_expected_size_said_when_prepared_is_kept() {
        let mut m = film_manifest();
        let info = |expected_size| DownloadInfo {
            id: "ab".repeat(16),
            state: DownloadState::Waiting,
            file: None,
            expected_size,
            no_longer_available: false,
        };
        take(&mut m, info(Some(2_800_000_000)));
        take(&mut m, info(None));
        assert_eq!(m.expected_size, Some(2_800_000_000));
    }

    /// A device that never saw the prepared answer (its manifest has no
    /// size) learns the size from the status reply.
    #[test]
    fn the_expected_size_is_taken_from_a_status_reply() {
        let mut m = film_manifest();
        assert_eq!(m.expected_size, None);
        let id = "ab".repeat(16);
        let reply = |expected_size| Reply::Downloads {
            downloads: vec![DownloadInfo {
                id: id.clone(),
                state: DownloadState::Converting {
                    done_percent: Some(10),
                    eta_secs: None,
                },
                file: None,
                expected_size,
                no_longer_available: false,
            }],
            gone: Vec::new(),
        };
        take(&mut m, status_of(reply(Some(2_800_000_000)), &id).unwrap());
        assert_eq!(m.expected_size, Some(2_800_000_000));
        // An older server says none: the one known stays.
        take(&mut m, status_of(reply(None), &id).unwrap());
        assert_eq!(m.expected_size, Some(2_800_000_000));
    }

    fn film_manifest() -> Manifest {
        Store::manifest(
            EntryKind::Film,
            "aa".into(),
            "s".into(),
            "u".into(),
            "u".into(),
            Item::Movie(spoolframe_types::Movie {
                id: MediaId::new(
                    spoolframe_types::Namespace::Plex,
                    spoolframe_types::MediaKind::Movie,
                    "1",
                )
                .unwrap(),
                title: "Alien".into(),
                year: None,
                release_date: None,
                summary: None,
                content_rating: None,
                duration: None,
                cast: Vec::new(),
                artwork: Vec::new(),
                playback: spoolframe_types::PlaybackState::default(),
            }),
            spoolframe_types::MediaTracks::default(),
            DownloadSource::Original,
            DownloadTracks::All,
            LocalWatch::default(),
            1,
        )
    }

    /// The server's cause is the download's.
    #[test]
    fn a_failure_the_server_tells_keeps_its_cause() {
        let mut m = film_manifest();
        take(
            &mut m,
            DownloadInfo {
                id: "ab".repeat(16),
                state: DownloadState::Failed {
                    reason: "the media file changed".into(),
                    cause: DownloadFailure::SourceMissing,
                },
                file: None,
                expected_size: None,
                no_longer_available: false,
            },
        );
        assert_eq!(
            m.phase,
            DownloadPhase::Failed {
                reason: "the media file changed".into(),
                cause: DownloadFailure::SourceMissing,
            }
        );
    }

    /// Review 2026-10-20, finding 8: a download the server has not comes
    /// back gone, and is prepared again, not taken for a broken reply.
    #[test]
    fn a_gone_download_is_not_found() {
        let reply = Reply::Downloads {
            downloads: Vec::new(),
            gone: vec!["ab".repeat(16)],
        };
        assert!(matches!(
            status_of(reply, &"ab".repeat(16)),
            Err(SdkError::NotFound(_))
        ));
    }
}
