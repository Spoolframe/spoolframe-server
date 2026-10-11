//! playback-ownership (PB): a playback handle works only for the session
//! and endpoint that started it (`use_playback`, `Playbacks::control`).
//!
//! Device A, the account's own sign-in, starts one playback of an item it
//! can play directly, and reads a few bytes of it. Device B, the same
//! account signed in from another fresh key, then sends A's handle in
//! every control and read; so does device C, a second account, when the
//! run has one. Each is refused `Unauthenticated`, as the authorization
//! matrix says, and A's playback is as it was after. A stops it.
//!
//! Then the same item as an HLS conversion, and progress from B and C
//! ([`hls`]): one playback at a time, never two.
//!
//! Safe on a real server by construction:
//! - A's first `Hello` says it reads ranges only, so no plan converts: no
//!   encode starts. The item is planned first (`PlaybackPlan`), and only
//!   one that plays directly is started. The HLS part starts only a plan
//!   that copies the audio alone: a remux, never an encode.
//! - Were a refusal not one, the worst it does is seek, re-plan or stop
//!   the probe's own playback, or move the account's resume point on the
//!   item (B's and C's `UpdatePlaybackProgress`, which is checked for).
//!   A itself never sends progress.
//! - A stops each playback; if the check cannot, the run's end does
//!   ([`AccountState::finish`]), as it signs out B and C.

mod hls;

use std::collections::BTreeSet;
use std::sync::Arc;

use spoolframe_protocol::resource::{ResourceMetadata, ResourceRequest, ResourceResponse};
use spoolframe_protocol::{
    BitmapSubtitles, Error, ManifestKind, MediaRead, PlaybackHandle, PlaybackPlan, Reply, Request,
    Response, SessionId,
};
use spoolframe_types::{
    AudioCodec, ClientCapabilities, Container, Delivery, Item, LibraryKind, MediaId, VideoCodec,
};

use crate::Config;
use crate::account::{self, AccountState, DEVICE_NAME, NotSigned, Playing, Signed, ready};
use crate::checks::{Findings, Outcome};
use crate::wire::{Conn, Probe, Problem, describe};

/// How many bytes each media read asks for.
const READ: u64 = 16;

/// The most items planned before giving up: one per library, music first.
const MAX_PLANS: usize = 3;

/// The most bytes a refused read may bring: none are expected.
const MAX_BYTES: usize = 64 << 10;

/// What device A says it plays: every codec and container this build has
/// a name for, by range reads only. Any file whose formats it knows plays
/// directly, and nothing can be converted for it.
fn capabilities() -> ClientCapabilities {
    use AudioCodec as A;
    use Container as C;
    use VideoCodec as V;
    ClientCapabilities {
        video: [
            V::H264,
            V::Hevc,
            V::Av1,
            V::Vp9,
            V::Vp8,
            V::Mpeg2,
            V::Mpeg4,
            V::Vc1,
        ]
        .into(),
        audio: [
            A::Aac,
            A::Ac3,
            A::Eac3,
            A::Dts,
            A::TrueHd,
            A::Flac,
            A::Alac,
            A::Opus,
            A::Vorbis,
            A::Mp3,
            A::Pcm,
        ]
        .into(),
        containers: [
            C::Matroska,
            C::Mp4,
            C::MpegTs,
            C::Avi,
            C::Flac,
            C::Mp3,
            C::Ogg,
        ]
        .into(),
        // No subtitle is picked.
        subtitles: BTreeSet::new(),
        delivery: vec![Delivery::Range],
        max_video: None,
        max_audio_channels: None,
        max_bitrate: None,
        hardware_video: None,
    }
}

/// No subtitle, the file's default audio: never the account's
/// preferences, which could pick a subtitle to burn in.
fn plain() -> Option<spoolframe_protocol::Selection> {
    Some(spoolframe_protocol::Selection::default())
}

pub(crate) async fn playback_ownership(
    probe: &Probe,
    config: &Config,
    state: &AccountState,
) -> Result<Outcome, Problem> {
    if !config.check_playback {
        return Ok(Outcome::skip(
            "not asked for: pass --check-playback (starts and stops one direct playback, then one \
             HLS remux of its audio)",
        ));
    }
    let a = ready!(state.unlocked(probe, config).await);
    let Some(account) = config.account.as_ref() else {
        return Ok(Outcome::skip("no account"));
    };
    let mut f = Findings::default();
    let (conn, _) = probe
        .connect_hello_as(&a.endpoint, Some(DEVICE_NAME.into()), Some(capabilities()))
        .await?;
    let conn = Arc::new(conn);
    f.note(format!(
        "device A: the account's sign-in, key {}, on a new connection whose Hello says it \
         plays every format this build names, by range reads only (nothing is converted)",
        a.endpoint.id().fmt_short()
    ));
    let Some(id) = pick(probe, &conn, &a.session, &mut f).await? else {
        f.note("no item the account sees plays directly: nothing started".into());
        let mut outcome = f.done();
        outcome.status = crate::Status::Skip;
        return Ok(outcome);
    };
    let start = Request::StartPlayback {
        id: id.clone(),
        selection: plain(),
        start_ms: None,
        bitmap: BitmapSubtitles::Auto,
        quality: Default::default(),
        display: None,
    };
    let (handle, size) = match probe.request(&conn, Some(&a.session), start).await? {
        Response::Ok(Reply::PlaybackStarted {
            handle, plan, size, ..
        }) => {
            state.playing(Playing {
                conn: conn.clone(),
                session: a.session.clone(),
                handle: handle.clone(),
            });
            let direct = matches!(plan, PlaybackPlan::Direct { .. });
            f.expect(
                direct,
                format!("A: StartPlayback {id}: PlaybackStarted, size {size}, plan {plan:?}"),
            );
            if !direct {
                let line = stop(probe, &conn, &a.session, &handle).await;
                state.stopped();
                f.note(format!("A: {line}"));
                return Ok(f.done());
            }
            (handle, size)
        }
        other => {
            f.expect(
                false,
                format!("A: StartPlayback {id}: {}", describe(&other)),
            );
            return Ok(f.done());
        }
    };
    owned(
        probe, config, state, account, &a, &conn, &handle, size, &mut f,
    )
    .await?;
    // A stops it, and it is gone after.
    let response = stop_playback(probe, &conn, &a.session, &handle).await?;
    let stopped = response == Response::Ok(Reply::Done);
    if stopped {
        state.stopped();
    }
    f.expect(stopped, format!("A: StopPlayback: {}", describe(&response)));
    let after = probe
        .request(
            &conn,
            Some(&a.session),
            Request::PlaybackRevision {
                handle: handle.clone(),
            },
        )
        .await?;
    // Gone: `PlaybackStopped` is kept for an admin's stop alone.
    f.expect(
        after == Response::Err(Error::PlaybackExpired),
        format!("A: PlaybackRevision after it: {}", describe(&after)),
    );
    if stopped {
        hls::converted(probe, config, state, account, &a, &id, &mut f).await?;
    } else {
        f.note("HLS part not checked: the direct playback did not stop".into());
    }
    Ok(f.done())
}

/// The playback is A's alone: B, and C if there is a second account, are
/// refused everything, and A's playback is as it was after each.
#[allow(clippy::too_many_arguments)]
async fn owned(
    probe: &Probe,
    config: &Config,
    state: &AccountState,
    account: &crate::Account,
    a: &Signed,
    conn: &Conn,
    handle: &PlaybackHandle,
    size: u64,
    f: &mut Findings,
) -> Result<(), Problem> {
    let revision = match revision_of(probe, conn, &a.session, handle).await? {
        Ok(revision) => revision,
        Err(response) => {
            f.expect(
                false,
                format!("A: PlaybackRevision: {}", describe(&response)),
            );
            return Ok(());
        }
    };
    let want = READ.min(size);
    let (response, bytes) = probe
        .resource(conn, &media(&a.session, handle, revision), MAX_BYTES)
        .await?;
    f.expect(
        read(&response, &bytes) == Some(want),
        format!(
            "A: a {READ}-byte read of its media at revision {revision}: {}, {} bytes",
            short(&response),
            bytes.len()
        ),
    );

    match state
        .sign_in_other(probe, account, "spoolframe-probe-b", "device B")
        .await?
    {
        Ok(b) => {
            let who = format!("B ({:?} on another device)", b.user.username);
            f.expect(
                b.user.username == a.user.username,
                format!("B signed in as {:?}, A's user", b.user.username),
            );
            intrude(probe, f, &who, &b, handle, revision).await?;
            // A's own session, presented from B's key (G3 before PB).
            let (response, bytes) = probe
                .resource(&b.conn, &media(&a.session, handle, revision), MAX_BYTES)
                .await?;
            f.expect(
                refused(&response, &bytes),
                format!(
                    "B, with A's session: media read: {}, {} bytes",
                    short(&response),
                    bytes.len()
                ),
            );
            let line = state.sign_out_other(probe, &b).await;
            f.note(line);
        }
        Err(why) => f.expect(false, format!("device B could not sign in: {why}")),
    }
    still_owned(probe, f, a, conn, handle, revision, want, "after B").await?;

    let Some(second) = &config.second else {
        f.note(
            "not checked: another user (pass --second-username and --second-password-file)".into(),
        );
        return Ok(());
    };
    match state
        .sign_in_other(probe, second, "spoolframe-probe-c", "device C")
        .await?
    {
        Ok(c) => {
            let who = format!("C ({:?} on another device)", c.user.username);
            f.expect(
                c.user.username != a.user.username,
                format!("C signed in as {:?}, not A's user", c.user.username),
            );
            intrude(probe, f, &who, &c, handle, revision).await?;
            listed(probe, f, &c).await?;
            let line = state.sign_out_other(probe, &c).await;
            f.note(line);
        }
        Err(why) => f.expect(false, format!("device C could not sign in: {why}")),
    }
    still_owned(probe, f, a, conn, handle, revision, want, "after C").await
}

/// `intruder` sends `handle` in every PB read and control but progress,
/// and every resource read: each is `Unauthenticated`, with no bytes.
/// `StopPlayback` last: were it allowed, the rest would meet a stopped
/// playback.
async fn intrude(
    probe: &Probe,
    f: &mut Findings,
    who: &str,
    intruder: &Signed,
    handle: &PlaybackHandle,
    revision: u64,
) -> Result<(), Problem> {
    if intruder.user.locked {
        f.note(format!(
            "{who}: not checked: its profile is PIN-locked on a new device, so every request \
             would be ProfileLocked before PB is reached"
        ));
        return Ok(());
    }
    let session = Some(intruder.session.as_str());
    for (what, request) in controls(handle) {
        let response = probe
            .request_at(&intruder.conn, session, request, Some(revision))
            .await?;
        f.expect(
            response == Response::Err(Error::Unauthenticated),
            format!("{who}: {what}: {}", describe(&response)),
        );
    }
    for (what, request) in resource_reads(&intruder.session, handle, revision) {
        let (response, bytes) = probe.resource(&intruder.conn, &request, MAX_BYTES).await?;
        f.expect(
            refused(&response, &bytes),
            format!("{who}: {what}: {}, {} bytes", short(&response), bytes.len()),
        );
    }
    let stop = Request::StopPlayback {
        handle: handle.clone(),
    };
    let response = probe
        .request_at(&intruder.conn, session, stop, Some(revision))
        .await?;
    f.expect(
        response == Response::Err(Error::Unauthenticated),
        format!(
            "{who}: StopPlayback at revision {revision}: {}",
            describe(&response)
        ),
    );
    Ok(())
}

/// The RPC requests that take a handle, but `StopPlayback` and
/// `UpdatePlaybackProgress`.
fn controls(handle: &PlaybackHandle) -> Vec<(&'static str, Request)> {
    let handle = || handle.clone();
    vec![
        (
            "PlaybackRevision",
            Request::PlaybackRevision { handle: handle() },
        ),
        (
            "PlaybackProgressSequence",
            Request::PlaybackProgressSequence { handle: handle() },
        ),
        (
            "GetManifest (media)",
            Request::GetManifest {
                handle: handle(),
                kind: ManifestKind::Media,
                variant: 0,
            },
        ),
        (
            "SeekPlayback",
            Request::SeekPlayback {
                handle: handle(),
                position_ms: 0,
            },
        ),
        (
            "ChangeSubtitle",
            Request::ChangeSubtitle {
                handle: handle(),
                subtitle: None,
                bitmap: BitmapSubtitles::Auto,
                position_ms: 0,
                display: None,
            },
        ),
    ]
}

/// The resource reads that take a handle, as `session` would send them.
fn resource_reads(
    session: &str,
    handle: &PlaybackHandle,
    revision: u64,
) -> Vec<(&'static str, ResourceRequest)> {
    let session = || SessionId(session.to_owned());
    vec![
        ("media read", media(&session().0, handle, revision)),
        (
            "subtitle read",
            ResourceRequest::Subtitle {
                session: session(),
                handle: handle.clone(),
                revision,
                offset: 0,
                version: None,
            },
        ),
        (
            "subtitle segment read",
            ResourceRequest::SubtitleSegment {
                session: session(),
                handle: handle.clone(),
                revision,
                segment: 1,
            },
        ),
    ]
}

fn media(session: &str, handle: &PlaybackHandle, revision: u64) -> ResourceRequest {
    ResourceRequest::Media {
        request: MediaRead {
            session: SessionId(session.to_owned()),
            handle: handle.clone(),
            offset: 0,
            length: READ,
            epoch: 0,
            segment: None,
            variant: 0,
        },
        revision,
    }
}

/// How many bytes a media read gave, if it was answered as one and they
/// all came.
fn read(response: &ResourceResponse, bytes: &[u8]) -> Option<u64> {
    match response {
        ResourceResponse::Ready {
            length,
            metadata: ResourceMetadata::Media { .. },
        } if *length == bytes.len() as u64 => Some(*length),
        _ => None,
    }
}

fn refused(response: &ResourceResponse, bytes: &[u8]) -> bool {
    *response
        == ResourceResponse::Error {
            error: Error::Unauthenticated,
        }
        && bytes.is_empty()
}

/// A resource response, short enough for a line of evidence.
fn short(response: &ResourceResponse) -> String {
    let text = format!("{response:?}");
    const MAX: usize = 160;
    match text.char_indices().nth(MAX) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text,
    }
}

/// A's playback is as it was: the same revision, and its media reads.
#[allow(clippy::too_many_arguments)]
async fn still_owned(
    probe: &Probe,
    f: &mut Findings,
    a: &Signed,
    conn: &Conn,
    handle: &PlaybackHandle,
    revision: u64,
    want: u64,
    after: &str,
) -> Result<(), Problem> {
    let now = revision_of(probe, conn, &a.session, handle).await?;
    f.expect(
        now == Ok(revision),
        format!(
            "A, {after}: PlaybackRevision: {}",
            match &now {
                Ok(r) => format!("{r} (was {revision})"),
                Err(response) => describe(response),
            }
        ),
    );
    let (response, bytes) = probe
        .resource(conn, &media(&a.session, handle, revision), MAX_BYTES)
        .await?;
    f.expect(
        read(&response, &bytes) == Some(want),
        format!(
            "A, {after}: media read: {}, {} bytes",
            short(&response),
            bytes.len()
        ),
    );
    Ok(())
}

/// What the second account lists, from its own session, before it signs
/// out: the run's web session is the first account's only.
async fn listed(probe: &Probe, f: &mut Findings, signed: &Signed) -> Result<(), Problem> {
    let session = Some(signed.session.as_str());
    match probe
        .request(&signed.conn, session, Request::ListSessions)
        .await?
    {
        Response::Ok(Reply::Sessions { sessions }) => f.note(format!(
            "{:?}: ListSessions before signing out: {} live, {} of them this one",
            signed.user.username,
            sessions.len(),
            sessions.iter().filter(|s| s.current).count()
        )),
        other => f.note(format!("ListSessions: {}", describe(&other))),
    }
    match probe
        .request(&signed.conn, session, Request::ListDevices)
        .await?
    {
        Response::Ok(Reply::Devices { devices }) => {
            let listed: Vec<String> = devices
                .iter()
                .map(|d| {
                    let id = &d.endpoint_id;
                    let mark = if d.this_device { " (this run's)" } else { "" };
                    format!(
                        "{}… {:?}{mark}",
                        &id[..id.len().min(10)],
                        d.name.as_deref().unwrap_or("(no name)")
                    )
                })
                .collect();
            f.note(format!(
                "{:?}: ListDevices: {} listed: {}",
                signed.user.username,
                devices.len(),
                listed.join(", ")
            ));
        }
        other => f.note(format!("ListDevices: {}", describe(&other))),
    }
    Ok(())
}

/// The handle's revision, or what was answered instead.
async fn revision_of(
    probe: &Probe,
    conn: &Conn,
    session: &str,
    handle: &PlaybackHandle,
) -> Result<Result<u64, Response>, Problem> {
    let asked = Request::PlaybackRevision {
        handle: handle.clone(),
    };
    Ok(match probe.request(conn, Some(session), asked).await? {
        Response::Ok(Reply::PlaybackRevision { revision }) => Ok(revision),
        other => Err(other),
    })
}

/// `StopPlayback` at the handle's current revision.
async fn stop_playback(
    probe: &Probe,
    conn: &Conn,
    session: &str,
    handle: &PlaybackHandle,
) -> Result<Response, Problem> {
    let revision = match revision_of(probe, conn, session, handle).await? {
        Ok(revision) => revision,
        Err(response) => return Ok(response),
    };
    let stop = Request::StopPlayback {
        handle: handle.clone(),
    };
    probe
        .request_at(conn, Some(session), stop, Some(revision))
        .await
}

/// Stop `handle`: the line for the report.
async fn stop(probe: &Probe, conn: &Conn, session: &str, handle: &PlaybackHandle) -> String {
    match stop_playback(probe, conn, session, handle).await {
        Ok(response) => format!("StopPlayback: {}", describe(&response)),
        Err(p) => format!("NOT stopped: {}", account::problem(p)),
    }
}

/// Stop a playback the run left playing: the line for the report.
pub(crate) async fn stop_left(probe: &Probe, playing: &Playing) -> String {
    stop(probe, &playing.conn, &playing.session, &playing.handle).await
}

/// An item A can play directly: the first one down each library, music
/// first (no previews are made for audio, and a track is short), then
/// shows, then movies. Each is planned before it is started.
async fn pick(
    probe: &Probe,
    conn: &Conn,
    session: &str,
    f: &mut Findings,
) -> Result<Option<MediaId>, Problem> {
    let session = Some(session);
    let mut libraries = match probe.request(conn, session, Request::Libraries).await? {
        Response::Ok(Reply::Libraries { libraries }) => libraries,
        other => {
            f.expect(false, format!("A: Libraries: {}", describe(&other)));
            return Ok(None);
        }
    };
    libraries.sort_by_key(|l| match l.kind {
        LibraryKind::Music => 0,
        LibraryKind::Shows => 1,
        LibraryKind::Movies => 2,
    });
    let mut planned = 0;
    for library in libraries {
        if planned == MAX_PLANS {
            break;
        }
        let items = Request::Items {
            library: library.id.clone(),
            offset: 0,
            limit: 1,
        };
        let mut item = match probe.request(conn, session, items).await? {
            Response::Ok(Reply::Items { mut items, .. }) if !items.is_empty() => items.remove(0),
            Response::Ok(Reply::Items { .. }) => continue,
            other => {
                f.note(format!("A: Items of {}: {}", library.id, describe(&other)));
                continue;
            }
        };
        // Artist → album → track; show → season → episode.
        for _ in 0..2 {
            if matches!(item, Item::Movie(_) | Item::Episode(_) | Item::Track(_)) {
                break;
            }
            let asked = Request::Item {
                id: item.id().clone(),
            };
            match probe.request(conn, session, asked).await? {
                Response::Ok(Reply::Item { mut children, .. }) if !children.is_empty() => {
                    item = children.remove(0);
                }
                other => {
                    f.note(format!("A: Item {}: {}", item.id(), describe(&other)));
                    break;
                }
            }
        }
        if !matches!(item, Item::Movie(_) | Item::Episode(_) | Item::Track(_)) {
            continue;
        }
        let id = item.id().clone();
        planned += 1;
        let plan = Request::PlaybackPlan {
            id: id.clone(),
            selection: plain(),
            bitmap: BitmapSubtitles::Auto,
            display: None,
        };
        match probe.request(conn, session, plan).await? {
            Response::Ok(Reply::PlaybackPlan {
                plan: plan @ PlaybackPlan::Direct { .. },
                ..
            }) => {
                f.note(format!(
                    "A: picked {id} from library {} ({:?}): PlaybackPlan {plan:?}",
                    library.id, library.kind
                ));
                return Ok(Some(id));
            }
            other => f.note(format!(
                "A: {id} does not play directly: {}",
                describe(&other)
            )),
        }
    }
    Ok(None)
}
