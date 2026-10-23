//! Writes the seeds in `seeds/` for the targets that have no fixture
//! files of their own: protocol frames and payloads, and PGS, DVB and
//! VobSub tracks. The JSON and BIF targets start from the test fixtures
//! of their crates instead (scripts/fuzz.sh names them).
//!
//! Run from `fuzz/`, in the dev shell (it needs ffmpeg and ffprobe):
//! `cargo run --example seeds`. The seeds are small on purpose: a cue of a
//! few pixels encodes to a few hundred bytes, and libFuzzer grows inputs
//! from there.

use std::path::{Path, PathBuf};
use std::process::Command;

use spoolframe_media::sandbox::Tool;
use spoolframe_media::{ocr, pgs::fixture};
use spoolframe_protocol::codec;
use spoolframe_protocol::invite::Invite;
use spoolframe_protocol::resource::{ResourceMetadata, ResourceRequest, ResourceResponse};
use spoolframe_protocol::rpc::{Operation, RpcReply, RpcRequest, RpcResponse, ServerInstance};
use spoolframe_protocol::{
    ConvertedVideo, Error, Hello, LoginPassword, ManifestKind, MediaRead, PlaybackHandle,
    PlaybackPlan, Quality, Reply, Request, RequestEnvelope, Response, Rung, SessionId,
};
use spoolframe_types::{Container, Delivery, MediaId, Platform, SubtitleFormat, VideoCodec};

fn write(target: &str, name: &str, bytes: &[u8]) {
    let dir = Path::new("seeds").join(target);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), bytes).unwrap();
}

fn requests() -> Vec<(&'static str, RpcRequest)> {
    let session = Some(SessionId("0123456789abcdef".into()));
    let handle = PlaybackHandle("00112233445566778899aabbccddeeff".into());
    let operation = Some(Operation {
        id: "op-1".into(),
        instance: "server-1".into(),
        expires_at_ms: 900_000,
    });
    let rpc = |session: Option<SessionId>, request, expected_revision| RpcRequest {
        envelope: RequestEnvelope { session, request },
        operation: operation.clone(),
        expected_revision,
    };
    vec![
        (
            "hello",
            rpc(
                None,
                Request::Hello(Hello {
                    protocol_version: spoolframe_protocol::PROTOCOL_VERSION,
                    client_name: "spoolframe-linux".into(),
                    client_version: "0.5.0".into(),
                    platform: Platform::Linux,
                    device_name: Some("laptop".into()),
                    capabilities: Some(Default::default()),
                    experimental: Vec::new(),
                }),
                None,
            ),
        ),
        (
            "login",
            rpc(
                None,
                Request::LoginPassword(LoginPassword {
                    username: "alice".into(),
                    password: "correct horse".into(),
                    remember: false,
                }),
                None,
            ),
        ),
        (
            "stop",
            rpc(
                session.clone(),
                Request::StopPlayback {
                    handle: handle.clone(),
                },
                Some(0),
            ),
        ),
        (
            "seek",
            rpc(
                session.clone(),
                Request::SeekPlayback {
                    handle: handle.clone(),
                    position_ms: 4_000,
                },
                Some(1),
            ),
        ),
        // P6-06, PX-21: a start for the link, the quality choices, a rung's
        // playlist.
        (
            "start",
            rpc(
                session.clone(),
                Request::StartPlayback {
                    id: MediaId::new(
                        spoolframe_types::Namespace::Plex,
                        spoolframe_types::MediaKind::Movie,
                        "1",
                    )
                    .unwrap(),
                    selection: None,
                    start_ms: Some(60_000),
                    bitmap: Default::default(),
                    quality: Quality::Auto {
                        link_bitrate: Some(20_000_000),
                    },
                    capabilities: None,
                    onward_share: None,
},
                None,
            ),
        ),
        (
            "quality-bitrate",
            rpc(
                session.clone(),
                Request::ChangeQuality {
                    handle: handle.clone(),
                    quality: Quality::Bitrate {
                        max_bitrate: 4_000_000,
                    },
                    bitmap: Default::default(),
                    position_ms: 4_000,
                    onward_share: None,
},
                Some(2),
            ),
        ),
        (
            "quality-original",
            rpc(
                session.clone(),
                Request::ChangeQuality {
                    handle: handle.clone(),
                    quality: Quality::Original,
                    bitmap: Default::default(),
                    position_ms: 4_000,
                    onward_share: None,
},
                Some(3),
            ),
        ),
        (
            "manifest-rung",
            rpc(
                session.clone(),
                Request::GetManifest {
                    handle,
                    kind: ManifestKind::Media,
                    variant: 2,
                },
                None,
            ),
        ),
        ("ping", rpc(session, Request::Ping, None)),
    ]
}

fn resources() -> Vec<(&'static str, ResourceRequest)> {
    let session = SessionId("0123456789abcdef".into());
    let handle = PlaybackHandle("00112233445566778899aabbccddeeff".into());
    vec![
        (
            "media",
            ResourceRequest::Media {
                request: MediaRead {
                    session: session.clone(),
                    handle: handle.clone(),
                    offset: 0,
                    length: 65_536,
                    epoch: 0,
                    segment: Some(1),
                    variant: 0,
                },
                revision: 0,
            },
        ),
        (
            "media-rung",
            ResourceRequest::Media {
                request: MediaRead {
                    session: session.clone(),
                    handle: handle.clone(),
                    offset: 0,
                    length: 65_536,
                    epoch: 1,
                    segment: Some(7),
                    variant: 2,
                },
                revision: 1,
            },
        ),
        (
            "subtitle",
            ResourceRequest::Subtitle {
                session,
                handle,
                revision: 2,
                offset: 0,
                version: Some(1),
            },
        ),
    ]
}

fn responses() -> Vec<(&'static str, RpcResponse)> {
    let server = ServerInstance {
        id: "server-1".into(),
        time_ms: 1_790_000_000_000,
    };
    vec![
        (
            "done",
            RpcResponse {
                server: server.clone(),
                reply: RpcReply::from(Response::Ok(Reply::Done)),
            },
        ),
        (
            "error",
            RpcResponse {
                server: server.clone(),
                reply: RpcReply::from(Response::Err(Error::Unauthenticated)),
            },
        ),
        // PX-21: a plan that is a ladder of encodes.
        (
            "ladder",
            RpcResponse {
                server,
                reply: RpcReply::from(Response::Ok(Reply::PlaybackChanged {
                    plan: PlaybackPlan::Converted {
                        delivery: Delivery::Hls,
                        container: Container::Mp4,
                        video: Some(ConvertedVideo::Encode {
                            codec: VideoCodec::H264,
                            height: 1080,
                            max_bitrate: Some(8_000_000),
                            tone_map: false,
                        }),
                        audio: None,
                        subtitle: None,
                        ladder: vec![
                            Rung {
                                height: 1080,
                                max_bitrate: 8_000_000,
                            },
                            Rung {
                                height: 540,
                                max_bitrate: 1_000_000,
                            },
                        ],
                        reasons: vec!["bitrate: 24.7 Mbit/s over the link's".into()],
                    },
                    size: 0,
                    epoch: 1,
                })),
            },
        ),
    ]
}

/// Invite links (P6-01, P6-05): one whole, one that only finds the
/// server.
fn invites() {
    let server = "3f9a1c7e5b2d8a04e6c1f7b39d52a8e0c4b6f1a2d3e5c7b9a1f0e2d4c6b8a0f1";
    let whole = Invite {
        server: server.into(),
        name: Some("Home Media".into()),
        addrs: vec![
            "203.0.113.24:41641".parse().unwrap(),
            "[2001:db8::1]:7420".parse().unwrap(),
        ],
        relays: vec!["https://relay.home-media.example".into()],
        lookup: Some("https://dns.iroh.link/pkarr".into()),
        code: Some("QW7K2MFPXR4T".into()),
        expires_at: Some(1_792_003_200),
        role: Some(spoolframe_types::Role::Consumer),
    };
    write("invite_link", "whole", whole.link().as_bytes());
    let bare = Invite {
        server: server.into(),
        ..Invite::default()
    };
    write("invite_link", "server-only", bare.link().as_bytes());
}

/// The protocol seeds: payloads for `protocol_cbor`, and the same as
/// frames (and multipart replies) for `protocol_frame`.
async fn protocol() {
    for (name, request) in requests() {
        write("protocol_cbor", name, &codec::encode(&request).unwrap());
        let mut frame = Vec::new();
        codec::write_frame(&mut frame, &request).await.unwrap();
        write("protocol_frame", name, &frame);
    }
    for (name, request) in resources() {
        write(
            "protocol_cbor",
            &format!("resource-{name}"),
            &codec::encode(&request).unwrap(),
        );
        let mut frame = Vec::new();
        codec::write_frame(&mut frame, &request).await.unwrap();
        write("protocol_frame", &format!("resource-{name}"), &frame);
    }
    for (name, response) in responses() {
        write(
            "protocol_cbor",
            &format!("reply-{name}"),
            &codec::encode(&response).unwrap(),
        );
        let mut reply = Vec::new();
        codec::write_reply(&mut reply, &response).await.unwrap();
        write("protocol_frame", &format!("reply-{name}"), &reply);
    }
    let ready = ResourceResponse::Ready {
        length: 12,
        metadata: ResourceMetadata::Subtitle {
            format: SubtitleFormat::Srt,
            total: 12,
            version: 1,
            revision: 2,
        },
    };
    write(
        "protocol_cbor",
        "resource-ready",
        &codec::encode(&ready).unwrap(),
    );
}

/// Two small cues: an outlined bar, then a box.
fn cues() -> Vec<fixture::Cue> {
    let (width, height) = (24u16, 6u16);
    let bar: Vec<u8> = (0..usize::from(width) * usize::from(height))
        .map(|i| {
            let (x, y) = (i % 24, i / 24);
            if (2..4).contains(&y) && (2..22).contains(&x) {
                1
            } else if (1..5).contains(&y) && (1..23).contains(&x) {
                2
            } else {
                0
            }
        })
        .collect();
    let mut boxed = vec![2u8; bar.len()];
    for y in 1..5 {
        for x in 1..23 {
            boxed[y * 24 + x] = if y == 1 || y == 4 { 2 } else { 1 };
        }
    }
    vec![
        fixture::Cue {
            start_ms: 1_000,
            end_ms: 2_500,
            x: 160,
            y: 400,
            width,
            height,
            mask: bar,
        },
        fixture::Cue {
            start_ms: 3_000,
            end_ms: 4_000,
            x: 100,
            y: 300,
            width,
            height,
            mask: boxed,
        },
    ]
}

fn run(cmd: &mut Command) {
    let out = cmd.output().expect("ffmpeg runs");
    assert!(
        out.status.success(),
        "{cmd:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `sup` encoded by ffmpeg's `codec` into a Matroska file, and read back
/// as the server reads a track.
async fn track(dir: &Path, sup: &Path, codec: &str) -> spoolframe_media::bitmap::Track {
    let movie: PathBuf = dir.join(format!("{codec}.mkv"));
    run(Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=64x36:d=5",
        ])
        .args(["-copyts", "-fix_sub_duration", "-i"])
        .arg(sup)
        .args(["-map", "0", "-map", "1", "-c:v", "libx264", "-c:s", codec])
        .arg(&movie));
    ocr::read_track(&Tool::unconfined("ffprobe"), &movie, 1, true)
        .await
        .unwrap()
}

async fn bitmaps() {
    let cues = cues();
    let whole = fixture::sup(&cues, 65_000);
    assert_eq!(spoolframe_media::pgs::cues(&whole).unwrap().len(), 2);
    write("pgs", "two-cues", &whole);
    // The same objects split into fragments, as large ones are on discs.
    write("pgs", "fragmented", &fixture::sup(&cues, 32));

    let dir = std::env::temp_dir().join(format!("spoolframe-fuzz-seeds-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sup = dir.join("subs.sup");
    std::fs::write(&sup, &whole).unwrap();

    let dvb = track(&dir, &sup, "dvbsub").await;
    write(
        "dvb",
        "two-cues",
        &spoolframe_fuzz::write_packets(&dvb.packets),
    );

    let vobsub = track(&dir, &sup, "dvdsub").await;
    let mut input = u16::try_from(vobsub.extradata.len())
        .unwrap()
        .to_le_bytes()
        .to_vec();
    input.extend_from_slice(&vobsub.extradata);
    input.extend(spoolframe_fuzz::write_packets(&vobsub.packets));
    let (header, packets) = spoolframe_fuzz::vobsub(&input).unwrap();
    let read = spoolframe_media::vobsub::cues(header, &packets).unwrap();
    assert_eq!(read.len(), 2, "VobSub seed");
    write("vobsub", "two-cues", &input);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Two seconds of a test pattern with sound, as Matroska with its Cues
/// and as MP4 with its sync samples, for `keyframe_index`.
fn films() {
    let dir = std::env::temp_dir().join(format!("spoolframe-fuzz-films-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, extension) in [("cues", "mkv"), ("stss", "mp4")] {
        let film = dir.join(format!("film.{extension}"));
        run(Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .args(["testsrc2=s=32x18:r=10:d=2", "-f", "lavfi", "-i"])
            .args(["sine=d=2:sample_rate=8000", "-c:v", "libx264", "-g", "5"])
            .args(["-c:a", "aac", "-b:a", "8k"])
            .arg(&film));
        let bytes = std::fs::read(&film).unwrap();
        let mut reader = std::io::Cursor::new(&bytes);
        let indexed = spoolframe_media::keyframe_index::read(&mut reader, bytes.len() as u64)
            .unwrap_or_else(|e| panic!("the {extension} seed: {e}"));
        assert!(indexed.times_ms.len() >= 2, "the {extension} seed");
        write("keyframe_index", name, &bytes);
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    protocol().await;
    invites();
    bitmaps().await;
    films();
}
