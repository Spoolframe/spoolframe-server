//! The playback-ownership check's second part: an HLS conversion, and
//! progress from another session.
//!
//! After the direct playback is stopped, device A opens another
//! connection whose `Hello` asks for HLS and names no container the item
//! is in, as the iOS app does for music. The item is planned again, and
//! started only when the plan copies its audio into HLS and has no video
//! and no subtitle: no encode runs, only a remux of one track. A reads
//! both playlists and the first two segments.
//!
//! B and C, each signed in from a fresh key (the refused-request log
//! keeps 10 lines a minute per key and kind, and this keeps every one of
//! them), then send A's handle in the playlist and segment reads,
//! `UpdatePlaybackProgress` and `PlaybackProgressSequence`, a seek and a
//! stop. Each must be `Unauthenticated` with no bytes, and A's playlists
//! and segments read as before. A sends no progress either: its watch
//! state for the item, read with `Item` before and after, is unchanged,
//! and the handle's accepted progress sequence is still 0. A stops it.

use std::collections::BTreeSet;
use std::sync::Arc;

use spoolframe_protocol::resource::ResourceRequest;
use spoolframe_protocol::{
    AudioAction, BitmapSubtitles, ConvertedAudio, Error, ManifestKind, MediaRead, PlaybackHandle,
    PlaybackPlan, Reply, Request, Response, SessionId,
};
use spoolframe_types::{
    AudioCodec, ClientCapabilities, Container, Delivery, Item, MediaId, PlaybackState,
};

use super::{MAX_BYTES, READ, plain, read, refused, revision_of, short, stop, stop_playback};
use crate::account::{AccountState, DEVICE_NAME, Playing, Signed};
use crate::checks::Findings;
use crate::wire::{Conn, Probe, Problem, describe};

/// The position B and C claim: were it taken, A's resume point would move.
const CLAIMED_MS: u64 = 61_000;

/// What device A's second connection says it plays: the audio codecs a
/// phone has, in fragmented MP4 or MPEG-TS, by HLS only. No video codec:
/// a file with a picture cannot be copied for it, so its plan encodes and
/// is not started.
fn capabilities() -> ClientCapabilities {
    use AudioCodec as A;
    ClientCapabilities {
        video: BTreeSet::new(),
        audio: [A::Aac, A::Alac, A::Flac, A::Mp3, A::Ac3, A::Eac3, A::Opus].into(),
        containers: [Container::Mp4, Container::MpegTs].into(),
        subtitles: BTreeSet::new(),
        delivery: vec![Delivery::Hls],
        max_video: None,
        max_audio_channels: None,
        max_bitrate: None,
        hardware_video: None,
        video_codecs: None,
    }
}

/// The plan is an HLS remux of the audio alone: why not, if it is not.
fn copy_only(plan: &PlaybackPlan) -> Result<(), String> {
    match plan {
        PlaybackPlan::Converted {
            delivery: Delivery::Hls,
            video: None,
            audio:
                Some(ConvertedAudio {
                    action: AudioAction::Copy { .. },
                    ..
                }),
            subtitle: None,
            ..
        } => Ok(()),
        other => Err(format!(
            "not an HLS copy of the audio alone, so it is not started: {other:?}"
        )),
    }
}

/// The HLS part. `id` is the item the direct part played.
pub(super) async fn converted(
    probe: &Probe,
    config: &crate::Config,
    state: &AccountState,
    account: &crate::Account,
    a: &Signed,
    id: &MediaId,
    f: &mut Findings,
) -> Result<(), Problem> {
    let (conn, _) = probe
        .connect_hello_as(&a.endpoint, Some(DEVICE_NAME.into()), Some(capabilities()))
        .await?;
    let conn = Arc::new(conn);
    f.note(format!(
        "HLS: device A again (key {}), on a new connection whose Hello asks for HLS in MP4 \
         or MPEG-TS and names no video codec",
        a.endpoint.id().fmt_short()
    ));
    let asked = Request::PlaybackPlan {
        id: id.clone(),
        selection: plain(),
        bitmap: BitmapSubtitles::Auto,
        display: None,
    };
    match probe.request(&conn, Some(&a.session), asked).await? {
        Response::Ok(Reply::PlaybackPlan { plan, .. }) => {
            if let Err(why) = copy_only(&plan) {
                f.note(format!("HLS part not checked: A: PlaybackPlan {id}: {why}"));
                return Ok(());
            }
            f.note(format!("HLS: A: PlaybackPlan {id}: {plan:?}"));
        }
        other => {
            f.note(format!(
                "HLS part not checked: A: PlaybackPlan {id}: {}",
                describe(&other)
            ));
            return Ok(());
        }
    }
    let before = match watch_state(probe, &conn, &a.session, id).await? {
        Ok(before) => {
            f.note(format!("HLS: A: Item {id} before: {}", shown(&before)));
            before
        }
        Err(line) => {
            f.expect(false, format!("HLS: A: Item {id} before: {line}"));
            return Ok(());
        }
    };
    let start = Request::StartPlayback {
        id: id.clone(),
        selection: plain(),
        start_ms: None,
        bitmap: BitmapSubtitles::Auto,
        quality: Default::default(),
        display: None,
        capabilities: None,
        onward_share: None,
    };
    let handle = match probe.request(&conn, Some(&a.session), start).await? {
        Response::Ok(Reply::PlaybackStarted { handle, plan, .. }) => {
            state.playing(Playing {
                conn: conn.clone(),
                session: a.session.clone(),
                handle: handle.clone(),
            });
            // Planned again at the start: it must be what was planned.
            let copied = copy_only(&plan);
            f.expect(
                copied.is_ok(),
                format!("HLS: A: StartPlayback {id}: PlaybackStarted, plan {plan:?}"),
            );
            if copied.is_err() {
                let line = stop(probe, &conn, &a.session, &handle).await;
                state.stopped();
                f.note(format!("HLS: A: {line}"));
                return Ok(());
            }
            handle
        }
        other => {
            f.expect(
                false,
                format!("HLS: A: StartPlayback {id}: {}", describe(&other)),
            );
            return Ok(());
        }
    };
    owned(probe, state, account, config, a, &conn, &handle, f).await?;

    // A sent no progress, and none from B or C was taken.
    match watch_state(probe, &conn, &a.session, id).await? {
        Ok(after) => f.expect(
            after == before,
            format!(
                "HLS: A: Item {id} after: {} (was {})",
                shown(&after),
                shown(&before)
            ),
        ),
        Err(line) => f.expect(false, format!("HLS: A: Item {id} after: {line}")),
    }
    let sequence = Request::PlaybackProgressSequence {
        handle: handle.clone(),
    };
    let response = probe.request(&conn, Some(&a.session), sequence).await?;
    f.expect(
        response == Response::Ok(Reply::PlaybackProgressSequence { sequence: 0 }),
        format!(
            "HLS: A: PlaybackProgressSequence (none accepted): {}",
            describe(&response)
        ),
    );

    let response = stop_playback(probe, &conn, &a.session, &handle).await?;
    let stopped = response == Response::Ok(Reply::Done);
    if stopped {
        state.stopped();
    }
    f.expect(
        stopped,
        format!("HLS: A: StopPlayback: {}", describe(&response)),
    );
    let after = probe
        .request(
            &conn,
            Some(&a.session),
            Request::PlaybackRevision {
                handle: handle.clone(),
            },
        )
        .await?;
    f.expect(
        after == Response::Err(Error::PlaybackExpired),
        format!("HLS: A: PlaybackRevision after it: {}", describe(&after)),
    );
    Ok(())
}

/// A reads its playlists and segments; B and C, each from a fresh key,
/// are refused all of it, and A reads as before after each.
#[allow(clippy::too_many_arguments)]
async fn owned(
    probe: &Probe,
    state: &AccountState,
    account: &crate::Account,
    config: &crate::Config,
    a: &Signed,
    conn: &Conn,
    handle: &PlaybackHandle,
    f: &mut Findings,
) -> Result<(), Problem> {
    let revision = match revision_of(probe, conn, &a.session, handle).await? {
        Ok(revision) => revision,
        Err(response) => {
            f.expect(
                false,
                format!("HLS: A: PlaybackRevision: {}", describe(&response)),
            );
            return Ok(());
        }
    };
    if !reads(probe, f, a, conn, handle, revision, "HLS: A").await? {
        return Ok(());
    }

    // (account, device name, letter, whether it is A's user)
    let mut others = vec![(account, "spoolframe-probe-b", "B", true)];
    match &config.second {
        Some(second) => others.push((second, "spoolframe-probe-c", "C", false)),
        None => f.note(
            "HLS: not checked: another user (pass --second-username and \
             --second-password-file)"
                .into(),
        ),
    }
    for (account, device, letter, same_user) in others {
        let label = format!("device {letter} (HLS)");
        match state.sign_in_other(probe, account, device, &label).await? {
            Ok(other) => {
                let who = format!(
                    "{letter} ({:?} on another device), HLS",
                    other.user.username
                );
                f.expect(
                    (other.user.username == a.user.username) == same_user,
                    format!(
                        "{letter}, HLS: signed in as {:?} (A is {:?})",
                        other.user.username, a.user.username
                    ),
                );
                intrude(probe, f, &who, &other, handle, revision).await?;
                let line = state.sign_out_other(probe, &other).await;
                f.note(line);
            }
            Err(why) => f.expect(false, format!("{label} could not sign in: {why}")),
        }
        let after = format!("HLS: A, after {letter}");
        let now = revision_of(probe, conn, &a.session, handle).await?;
        f.expect(
            now == Ok(revision),
            format!(
                "{after}: PlaybackRevision: {}",
                match &now {
                    Ok(r) => format!("{r} (was {revision})"),
                    Err(response) => describe(response),
                }
            ),
        );
        reads(probe, f, a, conn, handle, revision, &after).await?;
    }
    Ok(())
}

/// A's own reads: both playlists, the init segment and the first media
/// segment. Whether all were answered.
async fn reads(
    probe: &Probe,
    f: &mut Findings,
    a: &Signed,
    conn: &Conn,
    handle: &PlaybackHandle,
    revision: u64,
    who: &str,
) -> Result<bool, Problem> {
    let mut all = true;
    for kind in [ManifestKind::Multivariant, ManifestKind::Media] {
        let asked = Request::GetManifest {
            handle: handle.clone(),
            kind,
            variant: 0,
        };
        let response = probe.request(conn, Some(&a.session), asked).await?;
        let line = match &response {
            Response::Ok(Reply::Manifest { text }) if text.starts_with("#EXTM3U") => {
                Some(format!("#EXTM3U, {} lines", text.lines().count()))
            }
            _ => None,
        };
        all &= line.is_some();
        f.expect(
            line.is_some(),
            format!(
                "{who}: GetManifest ({kind:?}): {}",
                line.unwrap_or_else(|| describe(&response))
            ),
        );
    }
    for n in [0, 1] {
        let (response, bytes) = probe
            .resource(conn, &segment(&a.session, handle, revision, n), MAX_BYTES)
            .await?;
        let ok = read(&response, &bytes).is_some_and(|got| got > 0);
        all &= ok;
        f.expect(
            ok,
            format!(
                "{who}: a {READ}-byte read of segment {n}: {}, {} bytes",
                short(&response),
                bytes.len()
            ),
        );
    }
    Ok(all)
}

/// `intruder` sends A's handle in each HLS read and control, and in
/// progress: each is `Unauthenticated`, with no bytes. Nine requests, under
/// the refused-request log's 10 a minute for one key. `StopPlayback` last.
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
    let h = || handle.clone();
    let requests = [
        (
            "GetManifest (Multivariant)",
            Request::GetManifest {
                handle: h(),
                kind: ManifestKind::Multivariant,
                variant: 0,
            },
        ),
        (
            "GetManifest (Media)",
            Request::GetManifest {
                handle: h(),
                kind: ManifestKind::Media,
                variant: 0,
            },
        ),
        (
            "UpdatePlaybackProgress",
            Request::UpdatePlaybackProgress {
                handle: h(),
                sequence: 1,
                position_ms: CLAIMED_MS,
            },
        ),
        (
            "PlaybackProgressSequence",
            Request::PlaybackProgressSequence { handle: h() },
        ),
        (
            "SeekPlayback",
            Request::SeekPlayback {
                handle: h(),
                position_ms: 0,
            },
        ),
    ];
    for (what, request) in requests {
        let response = probe
            .request_at(&intruder.conn, session, request, Some(revision))
            .await?;
        f.expect(
            response == Response::Err(Error::Unauthenticated),
            format!("{who}: {what}: {}", describe(&response)),
        );
    }
    let own = || SessionId(intruder.session.clone());
    let reads = [
        ("init segment read", segment(&own().0, handle, revision, 0)),
        ("segment 1 read", segment(&own().0, handle, revision, 1)),
        (
            "subtitle segment read",
            ResourceRequest::SubtitleSegment {
                session: own(),
                handle: h(),
                revision,
                segment: 1,
            },
        ),
    ];
    for (what, request) in reads {
        let (response, bytes) = probe.resource(&intruder.conn, &request, MAX_BYTES).await?;
        f.expect(
            refused(&response, &bytes),
            format!("{who}: {what}: {}, {} bytes", short(&response), bytes.len()),
        );
    }
    let stop = Request::StopPlayback { handle: h() };
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

/// The first [`READ`] bytes of HLS segment `n` (0 is the init segment).
fn segment(session: &str, handle: &PlaybackHandle, revision: u64, n: u32) -> ResourceRequest {
    ResourceRequest::Media {
        request: MediaRead {
            session: SessionId(session.to_owned()),
            handle: handle.clone(),
            offset: 0,
            length: READ,
            epoch: 0,
            segment: Some(n),
            variant: 0,
        },
        revision,
    }
}

/// The session user's progress on `id`, as `Item` shows it.
async fn watch_state(
    probe: &Probe,
    conn: &Conn,
    session: &str,
    id: &MediaId,
) -> Result<Result<PlaybackState, String>, Problem> {
    let asked = Request::Item { id: id.clone() };
    Ok(match probe.request(conn, Some(session), asked).await? {
        Response::Ok(Reply::Item { item, .. }) => match item {
            Item::Movie(m) => Ok(m.playback),
            Item::Episode(e) => Ok(e.playback),
            Item::Track(t) => Ok(t.playback),
            other => Err(format!("not a playable item: {}", other.id())),
        },
        other => Err(describe(&other)),
    })
}

fn shown(state: &PlaybackState) -> String {
    match state.position {
        Some(at) => format!("at {} ms, watched {}", at.as_millis(), state.watched),
        None => format!("never started, watched {}", state.watched),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_protocol::ConvertedVideo;
    use spoolframe_types::VideoCodec;

    fn hls(video: Option<ConvertedVideo>, action: AudioAction) -> PlaybackPlan {
        PlaybackPlan::Converted {
            delivery: Delivery::Hls,
            container: Container::Mp4,
            video,
            audio: Some(ConvertedAudio { index: 0, action }),
            subtitle: None,
            ladder: Vec::new(),
            reasons: Vec::new(),
            source_codec: None,
        }
    }

    #[test]
    fn an_hls_copy_of_the_audio_alone_is_started() {
        let plan = hls(
            None,
            AudioAction::Copy {
                codec: AudioCodec::Flac,
            },
        );
        assert_eq!(copy_only(&plan), Ok(()));
    }

    #[test]
    fn an_audio_encode_is_not_started() {
        let plan = hls(
            None,
            AudioAction::Encode {
                codec: AudioCodec::Aac,
                channels: 2,
                bitrate: 256_000,
            },
        );
        assert!(copy_only(&plan).is_err());
    }

    #[test]
    fn a_plan_with_video_is_not_started() {
        let plan = hls(
            Some(ConvertedVideo::Copy {
                codec: VideoCodec::H264,
                dolby_vision: false,
            }),
            AudioAction::Copy {
                codec: AudioCodec::Aac,
            },
        );
        assert!(copy_only(&plan).is_err());
    }

    #[test]
    fn a_progressive_remux_is_not_started() {
        let plan = PlaybackPlan::Converted {
            delivery: Delivery::Progressive,
            container: Container::Matroska,
            video: None,
            audio: Some(ConvertedAudio {
                index: 0,
                action: AudioAction::Copy {
                    codec: AudioCodec::Flac,
                },
            }),
            subtitle: None,
            ladder: Vec::new(),
            reasons: Vec::new(),
            source_codec: None,
        };
        assert!(copy_only(&plan).is_err());
    }
}
