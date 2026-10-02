//! The wire format: every message round-trips, and what an older peer
//! does not know is skipped or named, never a decode failure.

use std::fmt::Debug;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use spoolframe_protocol::codec::{self, CodecError, MAX_FRAME};
use spoolframe_protocol::{
    AudioAction, ChosenTracks, ConvertedAudio, ConvertedSubtitle, ConvertedVideo, SubtitleAction,
    TrackReason,
};
use spoolframe_protocol::{
    AuthMethod, Error, Event, Hello, LoginPassword, PROTOCOL_VERSION, Reply, Request,
    RequestEnvelope, Response, ServerInfo, SessionId, SessionInfo, UserSummary,
};
use spoolframe_protocol::{
    PlannedAudio, PlannedSubtitle, PlaybackPlan, Selection, SubtitleDelivery,
};
use spoolframe_types::Delivery;
use spoolframe_types::{
    AudioCodec, ClientCapabilities, Container, SubtitleFormat, TrackPreferences, VideoCodec,
};
use spoolframe_types::{
    Item, Library, LibraryId, LibraryKind, MediaId, MediaKind, MediaTracks, Movie, Namespace,
    PlaybackState,
};

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: T) {
    let bytes = codec::encode(&value).unwrap();
    assert_eq!(codec::decode::<T>(&bytes).unwrap(), value);
}

fn movie() -> Item {
    Item::Movie(Movie {
        id: MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
        title: "Alien".into(),
        year: Some(1979),
        summary: None,
        duration: Some(Duration::from_secs(7020)),
        artwork: Vec::new(),
        playback: PlaybackState::default(),
    })
}

fn hello() -> Hello {
    Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "spoolframe-linux".into(),
        client_version: "0.1.0".into(),
        device_name: Some("Living room".into()),
        capabilities: Some(ClientCapabilities {
            video: [VideoCodec::H264].into(),
            audio: [AudioCodec::Aac].into(),
            containers: [Container::Matroska].into(),
            subtitles: [SubtitleFormat::Srt].into(),
            ..ClientCapabilities::default()
        }),
    }
}

#[test]
fn every_request_round_trips() {
    let id = MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap();
    for request in [
        Request::Hello(hello()),
        Request::AuthMethods,
        Request::LoginPassword(LoginPassword {
            username: "alice".into(),
            password: "hunter2".into(),
        }),
        Request::Ping,
        Request::Libraries,
        Request::Items {
            library: LibraryId::new(Namespace::Plex, "2").unwrap(),
            offset: 40,
            limit: 20,
        },
        Request::Item { id: id.clone() },
        Request::Search {
            query: "alien".into(),
            limit: 10,
        },
        Request::Tracks { id },
        Request::WhoAmI,
        Request::Logout,
        Request::ListSessions,
        Request::StartPlayback {
            id: MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
            selection: Some(Selection::default()),
        },
        Request::StartPlayback {
            id: MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
            selection: None,
        },
        Request::GetTrackPreferences,
        Request::SetTrackPreferences {
            preferences: TrackPreferences::default(),
        },
        Request::SetTrackOverride {
            id: MediaId::new(Namespace::Plex, MediaKind::Episode, "12").unwrap(),
            selection: Selection {
                audio: Some(2),
                subtitle: None,
            },
        },
        Request::ClearTrackOverride {
            id: MediaId::new(Namespace::Plex, MediaKind::Episode, "12").unwrap(),
        },
        Request::UpdatePlaybackProgress {
            id: MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
            position_ms: 61_000,
        },
        Request::MarkWatched {
            id: MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
        },
        Request::MarkUnwatched {
            id: MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
        },
        Request::Home,
        Request::ReadSubtitle {
            handle: spoolframe_protocol::PlaybackHandle("h".into()),
        },
        Request::PlaybackPlan {
            id: MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
            selection: Some(Selection {
                audio: Some(1),
                subtitle: None,
            }),
        },
    ] {
        round_trip(RequestEnvelope {
            session: Some(SessionId("s".into())),
            request,
        });
    }
}

#[test]
fn every_reply_round_trips() {
    for reply in [
        Reply::ServerInfo(ServerInfo {
            name: "Home Media".into(),
            endpoint_id: "e9c1".into(),
            protocol_version: PROTOCOL_VERSION,
            server_version: "0.1.0".into(),
            auth_methods: vec![AuthMethod::Password],
        }),
        Reply::AuthMethods {
            methods: vec![AuthMethod::Password],
        },
        Reply::LoggedIn {
            session: SessionId("s".into()),
        },
        Reply::Pong,
        Reply::Libraries {
            libraries: vec![Library {
                id: LibraryId::new(Namespace::Plex, "2").unwrap(),
                title: "Movies".into(),
                kind: LibraryKind::Movies,
            }],
        },
        Reply::Items {
            items: vec![movie()],
            total: 770,
        },
        Reply::Item {
            item: movie(),
            children: Vec::new(),
        },
        Reply::Search {
            items: vec![movie()],
        },
        Reply::Tracks(MediaTracks::default()),
        Reply::WhoAmI(UserSummary {
            user_id: "local:1".into(),
            username: "alice".into(),
            display_name: "Alice".into(),
            is_admin: false,
        }),
        Reply::LoggedOut,
        Reply::Done,
        Reply::Home(spoolframe_protocol::HomeScreen {
            continue_watching: vec![movie()],
            next_up: Vec::new(),
            recently_added: vec![movie()],
        }),
        Reply::PlaybackStarted {
            handle: spoolframe_protocol::PlaybackHandle("h".into()),
            plan: PlaybackPlan::Unsupported { reasons: vec![] },
            size: 12_345,
            chosen: Some(ChosenTracks {
                selection: Selection {
                    audio: Some(2),
                    subtitle: Some(6),
                },
                audio: TrackReason::Preferences,
                subtitle: TrackReason::Override,
            }),
        },
        Reply::TrackPreferences(TrackPreferences::default()),
        Reply::Subtitle {
            format: SubtitleFormat::Srt,
            bytes: b"1\n00:00:01,000 --> 00:00:02,000\nHi\n".to_vec(),
        },
        Reply::PlaybackPlan {
            plan: PlaybackPlan::Direct {
                container: Container::Matroska,
                video: Some(VideoCodec::Hevc),
                audio: Some(PlannedAudio {
                    index: 1,
                    codec: AudioCodec::Aac,
                }),
                subtitle: Some(PlannedSubtitle {
                    index: 0,
                    format: SubtitleFormat::Srt,
                    delivery: SubtitleDelivery::External,
                }),
            },
            chosen: None,
        },
        Reply::Sessions {
            sessions: vec![SessionInfo {
                device_name: Some("Living room".into()),
                created_at: 1,
                last_used_at: 2,
                current: true,
            }],
        },
    ] {
        round_trip(Response::Ok(reply));
    }
}

#[test]
fn every_error_round_trips() {
    for error in [
        Error::HandshakeRequired,
        Error::Unauthenticated,
        Error::Unsupported {
            request: "redeem_invite".into(),
        },
        Error::Malformed {
            message: "x".into(),
        },
        Error::NotFound {
            what: "plex:movie:1".into(),
        },
        Error::InvalidCredentials,
        Error::CannotPlay {
            reasons: vec!["audio 0: dts not supported".into()],
        },
        Error::PlaybackExpired,
        Error::RateLimited {
            retry_after_ms: 60_000,
        },
        Error::Unavailable {
            message: "Plex is down".into(),
        },
        Error::Internal,
    ] {
        round_trip(Response::Err(error));
    }
}

#[test]
fn every_plan_kind_round_trips_phase_2_ones_included() {
    for plan in [
        // A remux: every stream copied.
        PlaybackPlan::Converted {
            delivery: Delivery::Progressive,
            container: Container::Mp4,
            video: Some(ConvertedVideo::Copy {
                codec: VideoCodec::Hevc,
            }),
            audio: Some(ConvertedAudio {
                index: 0,
                action: AudioAction::Copy {
                    codec: AudioCodec::Eac3,
                },
            }),
            subtitle: None,
            reasons: vec!["container: matroska not supported".into()],
        },
        PlaybackPlan::Converted {
            delivery: Delivery::Hls,
            container: Container::Mp4,
            video: Some(ConvertedVideo::Encode {
                codec: VideoCodec::H264,
                height: 1080,
                max_bitrate: Some(8_000_000),
                tone_map: true,
            }),
            audio: Some(ConvertedAudio {
                index: 1,
                action: AudioAction::Encode {
                    codec: AudioCodec::Aac,
                    channels: 2,
                    bitrate: 192_000,
                },
            }),
            subtitle: Some(ConvertedSubtitle {
                index: 0,
                action: SubtitleAction::BurnIn {
                    format: SubtitleFormat::Pgs,
                },
            }),
            reasons: vec!["subtitle 0: pgs cannot be shown; burned into the video".into()],
        },
        PlaybackPlan::Unsupported {
            reasons: vec!["audio 0: dts not supported".into()],
        },
    ] {
        round_trip(Response::Ok(Reply::PlaybackPlan { plan, chosen: None }));
    }
}

#[test]
fn plan_parts_from_a_newer_peer_decode_as_unknown() {
    // Over the wire's own encoding: a newer peer's CBOR.
    fn wire<T: DeserializeOwned>(value: serde_json::Value) -> T {
        codec::decode(&codec::encode(&value).unwrap()).unwrap()
    }
    let plan: PlaybackPlan = wire(serde_json::json!({"type": "dash_ladder", "rungs": 4}));
    assert_eq!(plan, PlaybackPlan::Unknown);
    let video: ConvertedVideo = wire(serde_json::json!({"type": "super_resolve"}));
    assert_eq!(video, ConvertedVideo::Unknown);
    let delivery: Delivery = wire(serde_json::json!("dash"));
    assert_eq!(delivery, Delivery::Unknown);
}

#[test]
fn artwork_messages_round_trip() {
    round_trip(spoolframe_protocol::ArtworkRequest {
        session: SessionId("s".into()),
        id: spoolframe_types::ArtworkId::new(Namespace::Plex, "/library/metadata/1/thumb/2")
            .unwrap(),
        size: spoolframe_protocol::ArtworkSize::Medium,
    });
    round_trip(spoolframe_protocol::ArtworkResponse::Ok(
        spoolframe_protocol::ArtworkReply {
            content_type: "image/jpeg".into(),
            length: 1234,
            etag: "abc".into(),
        },
    ));
    round_trip(spoolframe_protocol::ArtworkResponse::Err(
        Error::Unauthenticated,
    ));
}

#[test]
fn an_artwork_size_from_a_newer_client_is_unknown() {
    #[derive(serde::Serialize)]
    #[serde(rename_all = "snake_case")]
    enum Newer {
        Huge,
    }
    let bytes = codec::encode(&Newer::Huge).unwrap();
    assert_eq!(
        codec::decode::<spoolframe_protocol::ArtworkSize>(&bytes).unwrap(),
        spoolframe_protocol::ArtworkSize::Unknown
    );
}

#[test]
fn media_reads_round_trip() {
    round_trip(spoolframe_protocol::MediaRead {
        session: SessionId("s".into()),
        handle: spoolframe_protocol::PlaybackHandle("h".into()),
        offset: 1 << 30,
        length: spoolframe_protocol::MAX_READ,
    });
    round_trip(spoolframe_protocol::MediaReadResponse::Ok(
        spoolframe_protocol::MediaReadReply {
            sent: 4096,
            size: 1 << 33,
        },
    ));
    round_trip(spoolframe_protocol::MediaReadResponse::Err(
        Error::PlaybackExpired,
    ));
}

#[test]
fn subtitle_bytes_are_a_cbor_byte_string() {
    let reply = Reply::Subtitle {
        format: SubtitleFormat::Srt,
        bytes: vec![0xAB; 100],
    };
    // A byte string is about as long as its bytes; an array of integers
    // would take about two bytes each.
    assert!(codec::encode(&reply).unwrap().len() < 150);
}

#[test]
fn events_round_trip() {
    round_trip(Event::ShuttingDown);
}

/// What a newer peer might send.
mod newer {
    use serde::Serialize;

    #[derive(Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Request {
        RedeemInvite { code: String },
    }

    #[derive(Serialize)]
    pub struct Envelope {
        pub session: Option<String>,
        pub request: Request,
    }

    #[derive(Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Error {
        InviteExpired { at: u64 },
    }

    #[derive(Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Event {
        WatchStateChanged { id: String },
    }

    #[derive(Serialize)]
    pub struct Hello {
        pub protocol_version: u32,
        pub client_name: String,
        pub client_version: String,
        /// A field this build has never heard of.
        pub screen_inches: u32,
    }
}

#[test]
fn an_unknown_request_decodes_as_unknown_and_can_be_named() {
    let bytes = codec::encode(&newer::Envelope {
        session: None,
        request: newer::Request::RedeemInvite { code: "abc".into() },
    })
    .unwrap();
    let envelope: RequestEnvelope = codec::decode(&bytes).unwrap();
    assert_eq!(envelope.request, Request::Unknown);
    assert_eq!(
        codec::request_type(&bytes).as_deref(),
        Some("redeem_invite")
    );
}

#[test]
fn an_unknown_error_decodes_as_unknown() {
    let bytes = codec::encode(&newer::Error::InviteExpired { at: 1 }).unwrap();
    assert_eq!(codec::decode::<Error>(&bytes).unwrap(), Error::Unknown);
}

#[test]
fn an_unknown_event_decodes_as_unknown() {
    let bytes = codec::encode(&newer::Event::WatchStateChanged { id: "x".into() }).unwrap();
    assert_eq!(codec::decode::<Event>(&bytes).unwrap(), Event::Unknown);
}

#[test]
fn a_field_added_by_a_newer_client_is_ignored() {
    let bytes = codec::encode(&newer::Hello {
        protocol_version: 2,
        client_name: "next".into(),
        client_version: "9".into(),
        screen_inches: 65,
    })
    .unwrap();
    let hello: Hello = codec::decode(&bytes).unwrap();
    assert_eq!(hello.protocol_version, 2);
}

#[test]
fn request_types_are_snake_case() {
    let bytes = codec::encode(&RequestEnvelope {
        session: None,
        request: Request::AuthMethods,
    })
    .unwrap();
    assert_eq!(codec::request_type(&bytes).as_deref(), Some("auth_methods"));
}

#[test]
fn only_hello_auth_methods_login_and_ping_are_pre_auth() {
    assert!(Request::Hello(hello()).is_pre_auth());
    assert!(Request::Ping.is_pre_auth());
    assert!(!Request::Libraries.is_pre_auth());
    assert!(!Request::WhoAmI.is_pre_auth());
    assert!(!Request::Unknown.is_pre_auth());
}

#[test]
fn credentials_stay_out_of_debug_output() {
    let login = LoginPassword {
        username: "alice".into(),
        password: "hunter2".into(),
    };
    assert!(!format!("{login:?}").contains("hunter2"));
    assert!(!format!("{:?}", SessionId("tok3n".into())).contains("tok3n"));
}

#[tokio::test]
async fn frames_round_trip_over_a_stream() {
    let (mut a, mut b) = tokio::io::duplex(64);
    let writer = tokio::spawn(async move {
        codec::write_frame(&mut a, &Request::Ping).await.unwrap();
        codec::write_frame(&mut a, &Event::ShuttingDown)
            .await
            .unwrap();
    });
    assert_eq!(
        codec::read_frame::<_, Request>(&mut b).await.unwrap(),
        Request::Ping
    );
    assert_eq!(
        codec::read_frame::<_, Event>(&mut b).await.unwrap(),
        Event::ShuttingDown
    );
    writer.await.unwrap();
}

#[tokio::test]
async fn an_oversized_frame_is_refused_before_it_is_read() {
    let mut bytes = (MAX_FRAME + 1).to_be_bytes().to_vec();
    bytes.extend_from_slice(b"no payload follows");
    let err = codec::read_frame_bytes(&mut bytes.as_slice())
        .await
        .unwrap_err();
    assert!(matches!(err, CodecError::TooLarge { .. }), "{err}");
}

/// A client from before P1-20 sends a selection as a plain struct; it
/// still decodes, as an explicit one.
#[test]
fn an_older_client_s_selection_is_still_explicit() {
    #[derive(Serialize)]
    struct OldStart {
        #[serde(rename = "type")]
        kind: &'static str,
        id: &'static str,
        selection: Selection,
    }
    #[derive(Serialize)]
    struct Envelope {
        request: OldStart,
    }
    let bytes = codec::encode(&Envelope {
        request: OldStart {
            kind: "start_playback",
            id: "plex:movie:1",
            selection: Selection::default(),
        },
    })
    .unwrap();
    let decoded: RequestEnvelope = codec::decode(&bytes).unwrap();
    assert!(matches!(
        decoded.request,
        Request::StartPlayback {
            selection: Some(_),
            ..
        }
    ));
}
