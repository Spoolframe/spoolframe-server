//! The wire format: every message round-trips, and what an older peer
//! does not know is skipped or named, never a decode failure.

use std::fmt::Debug;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use spoolframe_protocol::codec::{self, CodecError, MAX_FRAME};
use spoolframe_protocol::{
    AudioAction, BitmapSubtitles, ChosenTracks, ConvertedAudio, ConvertedSubtitle, ConvertedVideo,
    SubtitleAction, TrackReason,
};
use spoolframe_protocol::{
    AuthMethod, Error, Event, Hello, LoginPassword, PROTOCOL_VERSION, PinProof, Reply, Request,
    RequestEnvelope, Response, ServerInfo, SessionId, SessionInfo, UserSummary,
};
use spoolframe_protocol::{
    FinishPasskeyRegistration, FinishPasskeySignIn, PasskeyCreation, PasskeyInfo, PasskeyRequest,
};
use spoolframe_protocol::{
    PlannedAudio, PlannedSubtitle, PlaybackPlan, Selection, SubtitleDelivery,
};
use spoolframe_protocol::{SearchCounts, SearchGroup, SearchRequest, SearchResults, TopResult};
use spoolframe_types::Delivery;
use spoolframe_types::{
    AudioCodec, ClientCapabilities, Container, SubtitleFormat, TrackPreferences, VideoCodec,
};
use spoolframe_types::{
    Item, Library, LibraryId, LibraryKind, Marker, MarkerKind, MediaId, MediaKind, MediaTracks,
    Movie, Namespace, Person, PersonId, PersonRole, PlaybackState, SearchKind, SearchScope,
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
        content_rating: None,
        duration: Some(Duration::from_secs(7020)),
        artwork: Vec::new(),
        playback: PlaybackState::default(),
    })
}

fn markers() -> Vec<Marker> {
    vec![
        Marker {
            kind: MarkerKind::Intro,
            start_ms: 258_865,
            end_ms: 319_865,
            to_end: false,
        },
        Marker {
            kind: MarkerKind::Credits,
            start_ms: 2_662_962,
            end_ms: 2_709_958,
            to_end: true,
        },
    ]
}

fn search_results() -> SearchResults {
    SearchResults {
        top: Some(TopResult::Item(movie())),
        groups: vec![
            SearchGroup {
                kind: SearchKind::People,
                items: Vec::new(),
                people: vec![Person {
                    id: PersonId::new(Namespace::Plex, "actor-134469").unwrap(),
                    name: "Keanu Reeves".into(),
                    role: PersonRole::Actor,
                    count: Some(9),
                }],
                more: true,
                total: None,
                hidden: 0,
            },
            SearchGroup {
                kind: SearchKind::Episodes,
                items: Vec::new(),
                people: Vec::new(),
                more: false,
                total: Some(3),
                hidden: 3,
            },
        ],
        counts: SearchCounts {
            all: Some(12),
            movies: Some(1),
            shows: None,
            episodes: Some(3),
            music: Some(0),
        },
        year: Some(1979),
        without_year: vec![movie()],
    }
}

fn hello() -> Hello {
    Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "spoolframe-linux".into(),
        client_version: "0.1.0".into(),
        platform: spoolframe_types::Platform::Linux,
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
        Request::GetPreviews { id: id.clone() },
        Request::Hello(hello()),
        Request::AuthMethods,
        Request::LoginPassword(LoginPassword {
            username: "alice".into(),
            password: "hunter2".into(),
        }),
        Request::Ping,
        Request::BeginDeviceSignIn,
        Request::AwaitDeviceSignIn {
            request: "r".into(),
        },
        Request::LookupDeviceSignIn {
            code: "BCDFGHJK".into(),
        },
        Request::BeginPasskeySignIn { username: None },
        Request::BeginPasskeySignIn {
            username: Some("alice".into()),
        },
        Request::FinishPasskeySignIn(signed()),
        Request::BeginPasskeyRegistration,
        Request::FinishPasskeyRegistration(made()),
        Request::ListPasskeys,
        Request::RenamePasskey {
            id: "AQID".into(),
            name: "Alice's iPad".into(),
        },
        Request::RemovePasskey { id: "AQID".into() },
        Request::ApproveDeviceSignIn {
            code: "BCDFGHJK".into(),
        },
        Request::DenyDeviceSignIn {
            code: "BCDFGHJK".into(),
        },
        Request::Libraries,
        Request::Items {
            library: LibraryId::new(Namespace::Plex, "2").unwrap(),
            offset: 40,
            limit: 20,
        },
        Request::Item { id: id.clone() },
        Request::Search(SearchRequest {
            query: "alien 1979".into(),
            limit: 10,
            scope: SearchScope::Movies,
            kind: None,
            library: Some(LibraryId::new(Namespace::Plex, "2").unwrap()),
        }),
        Request::Search(SearchRequest {
            query: "keanu".into(),
            limit: 50,
            scope: SearchScope::All,
            kind: Some(SearchKind::People),
            library: None,
        }),
        Request::PersonItems {
            person: PersonId::new(Namespace::Plex, "director-131768").unwrap(),
            offset: 0,
            limit: 50,
        },
        Request::Tracks { id },
        Request::WhoAmI,
        Request::Logout,
        Request::ListSessions,
        Request::StartPlayback {
            id: MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
            selection: Some(Selection::default()),
            start_ms: Some(1_800_000),
            bitmap: BitmapSubtitles::Text,
        },
        Request::StartPlayback {
            id: MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
            selection: None,
            start_ms: None,
            bitmap: BitmapSubtitles::Auto,
        },
        Request::StopPlayback {
            handle: spoolframe_protocol::PlaybackHandle("h".into()),
        },
        Request::SetPin {
            pin: "4321".into(),
            proof: None,
        },
        Request::SetPin {
            pin: "4321".into(),
            proof: Some(PinProof::Pin { pin: "1234".into() }),
        },
        Request::ClearPin {
            proof: PinProof::Password {
                password: "correct horse".into(),
            },
        },
        Request::UnlockProfile { pin: "4321".into() },
        Request::ListDevices,
        Request::GetPlexLink,
        Request::SetPinRequired {
            device: None,
            required: false,
        },
        Request::ChangeSubtitle {
            handle: spoolframe_protocol::PlaybackHandle("h".into()),
            subtitle: Some(2),
            bitmap: BitmapSubtitles::BurnIn,
            position_ms: 61_000,
        },
        Request::ReadSubtitleSegment {
            handle: spoolframe_protocol::PlaybackHandle("h".into()),
            n: 7,
        },
        Request::GetManifest {
            handle: spoolframe_protocol::PlaybackHandle("h".into()),
            kind: spoolframe_protocol::ManifestKind::Subtitles,
        },
        Request::SeekPlayback {
            handle: spoolframe_protocol::PlaybackHandle("h".into()),
            position_ms: 600_000,
        },
        Request::GetManifest {
            handle: spoolframe_protocol::PlaybackHandle("h".into()),
            kind: spoolframe_protocol::ManifestKind::Media,
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
            handle: spoolframe_protocol::PlaybackHandle("handle".into()),
            sequence: 1,
            position_ms: 61_000,
        },
        Request::MarkWatched {
            id: MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
        },
        Request::MarkUnwatched {
            id: MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
        },
        Request::HideFromContinueWatching {
            id: MediaId::new(Namespace::Plex, MediaKind::Episode, "70").unwrap(),
        },
        Request::UnhideFromContinueWatching {
            id: MediaId::new(Namespace::Plex, MediaKind::Episode, "70").unwrap(),
        },
        Request::Home,
        Request::ReadSubtitle {
            handle: spoolframe_protocol::PlaybackHandle("h".into()),
            offset: 0,
            version: None,
        },
        Request::ReadSubtitle {
            handle: spoolframe_protocol::PlaybackHandle("h".into()),
            offset: spoolframe_protocol::SUBTITLE_CHUNK,
            version: Some(0x1234_5678_9abc_def0),
        },
        Request::PlaybackPlan {
            id: MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
            selection: Some(Selection {
                audio: Some(1),
                subtitle: None,
            }),
            bitmap: BitmapSubtitles::BurnIn,
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
            web_url: Some("https://media.example.org".into()),
        }),
        Reply::AuthMethods {
            methods: vec![AuthMethod::Password, AuthMethod::DeviceApproval],
        },
        Reply::PasskeyCreation(creation()),
        Reply::Passkeys {
            passkeys: vec![
                PasskeyInfo {
                    id: "AQID".into(),
                    name: "Alice's iPhone".into(),
                    created_at: 1,
                    last_used_at: Some(2),
                    rp_id: "spoolframe.app".into(),
                    app: true,
                    usable_here: true,
                },
                PasskeyInfo {
                    id: "BAUG".into(),
                    name: "YubiKey".into(),
                    created_at: 3,
                    last_used_at: None,
                    rp_id: "media.example.org".into(),
                    app: false,
                    usable_here: false,
                },
            ],
        },
        Reply::Passkeys {
            passkeys: Vec::new(),
        },
        Reply::PasskeyRequest(PasskeyRequest {
            challenge: vec![7; 32],
            allow_credential_ids: vec![vec![1; 16], vec![2; 16]],
        }),
        Reply::PasskeyRequest(PasskeyRequest {
            challenge: vec![7; 32],
            allow_credential_ids: Vec::new(),
        }),
        Reply::Previews(spoolframe_protocol::Previews {
            interval_ms: 10_000,
            tile_width: 320,
            tile_height: 134,
            columns: 10,
            rows: 10,
            count: 150,
            sheets: vec![
                "preview:0f3a/0".parse().unwrap(),
                "preview:0f3a/1".parse().unwrap(),
            ],
        }),
        Reply::DeviceSignInStarted {
            request: "r".into(),
            code: "BCDFGHJK".into(),
            expires_in_ms: 300_000,
            web_url: Some("https://media.example.org/auth/device?code=BCDFGHJK".into()),
        },
        Reply::DeviceSignInStarted {
            request: "r".into(),
            code: "BCDFGHJK".into(),
            expires_in_ms: 300_000,
            web_url: None,
        },
        Reply::DeviceSignInPending,
        Reply::DeviceSignInRequest(spoolframe_protocol::DeviceSignInRequest {
            device_name: Some("Living room TV".into()),
            client_name: "spoolframe-tvos".into(),
            platform: spoolframe_types::Platform::Tvos,
            requested_at: 1_790_000_000,
        }),
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
            markers: markers(),
        },
        Reply::Search(search_results()),
        Reply::Tracks(MediaTracks::default()),
        Reply::WhoAmI(UserSummary {
            user_id: "local:0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4c".into(),
            username: "alice".into(),
            display_name: "Alice".into(),
            role: spoolframe_types::Role::Consumer,
            locked: true,
            pin: Some(spoolframe_protocol::PinState { digits: Some(6) }),
        }),
        Reply::Devices {
            devices: vec![spoolframe_protocol::DeviceInfo {
                endpoint_id: "ab".repeat(32),
                name: Some("Living room TV".into()),
                platform: Some(spoolframe_types::Platform::Tvos),
                last_seen_at: 1_790_000_000,
                pin_required: true,
                this_device: false,
            }],
        },
        Reply::PlexLink { link: None },
        Reply::PlexLink {
            link: Some(spoolframe_protocol::PlexLink {
                username: Some("alice".into()),
                email: Some("alice@example.com".into()),
                by_config: false,
                linked_at: Some(1_790_000_000),
                last_sync: Some(spoolframe_protocol::PlexSyncSummary {
                    at: 1_790_000_100,
                    libraries: 2,
                    pulled: 3,
                    pushed: 4,
                    skipped: 5,
                    failed: 0,
                }),
                syncing: true,
            }),
        },
        Reply::LoggedOut,
        Reply::Done,
        Reply::PlaybackSeeked { epoch: 1 },
        Reply::PlaybackChanged {
            plan: PlaybackPlan::Unsupported {
                reasons: Vec::new(),
            },
            size: 0,
            epoch: 3,
        },
        Reply::Manifest {
            text: "#EXTM3U\n".into(),
        },
        Reply::Home(spoolframe_protocol::HomeScreen {
            continue_watching: vec![movie()],
            next_up: Vec::new(),
            recently_added: vec![movie()],
        }),
        Reply::PlaybackStarted {
            handle: spoolframe_protocol::PlaybackHandle("h".into()),
            plan: PlaybackPlan::Unsupported { reasons: vec![] },
            size: 12_345,
            epoch: 0,
            chosen: Some(ChosenTracks {
                selection: Selection {
                    audio: Some(2),
                    subtitle: Some(6),
                },
                audio: TrackReason::Preferences,
                subtitle: TrackReason::Override,
            }),
            markers: markers(),
        },
        Reply::TrackPreferences(TrackPreferences::default()),
        Reply::Subtitle {
            format: SubtitleFormat::Srt,
            bytes: b"1\n00:00:01,000 --> 00:00:02,000\nHi\n".to_vec(),
        },
        Reply::SubtitleChunk {
            format: SubtitleFormat::Srt,
            bytes: b"1\n00:00:01,000 --> 00:00:02,000\nHi\n".to_vec(),
            total: 1_600_000,
            version: u64::MAX,
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
                platform: Some(spoolframe_types::Platform::Linux),
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
        Error::DeviceSignInDenied,
        Error::DeviceSignInExpired,
        Error::CannotPlay {
            reasons: vec!["audio 0: dts not supported".into()],
        },
        Error::PlaybackExpired,
        Error::RateLimited {
            retry_after_ms: 60_000,
        },
        Error::ReplyTooLarge { bytes: 1_600_000 },
        Error::Unavailable {
            message: "Plex is down".into(),
        },
        Error::NotSetUp,
        Error::NoPassword,
        Error::RecentSignInRequired,
        Error::LastSignInMethod,
        Error::PreviewsNotReady { done_percent: 7 },
        Error::NoPreviews,
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
        PlaybackPlan::Converted {
            delivery: Delivery::Progressive,
            container: Container::Matroska,
            video: Some(ConvertedVideo::Copy {
                codec: VideoCodec::Hevc,
            }),
            audio: None,
            subtitle: Some(ConvertedSubtitle {
                index: 2,
                action: SubtitleAction::Ocr {
                    from: SubtitleFormat::Pgs,
                    to: SubtitleFormat::Srt,
                },
            }),
            reasons: vec!["subtitle 2: pgs read as text".into()],
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
        epoch: 2,
        segment: Some(14),
    });
    round_trip(spoolframe_protocol::MediaReadResponse::Ok(
        spoolframe_protocol::MediaReadReply {
            sent: 4096,
            size: 1 << 33,
            complete: false,
            epoch: 2,
        },
    ));
    for error in [
        Error::PlaybackExpired,
        Error::PlaybackStopped,
        Error::Busy,
        Error::PlaybackFailed {
            reason: "Invalid data found when processing input".into(),
        },
        Error::PlaybackRestarted { epoch: 3 },
        Error::SubtitleNotReady { done_percent: 42 },
        Error::SubtitleChanged,
        Error::ProfileLocked,
        Error::NotSetUp,
    ] {
        round_trip(spoolframe_protocol::MediaReadResponse::Err(error));
    }
}

/// A reply from a server before P2-04 has neither `complete` nor `epoch`:
/// it was direct play, so the whole file.
#[test]
fn an_older_server_s_read_reply_is_complete_at_epoch_0() {
    #[derive(Serialize)]
    struct OldReply {
        sent: u64,
        size: u64,
    }
    let bytes = codec::encode(&OldReply { sent: 10, size: 20 }).unwrap();
    let reply: spoolframe_protocol::MediaReadReply = codec::decode(&bytes).unwrap();
    assert!(reply.complete);
    assert_eq!(reply.epoch, 0);
}

#[test]
fn subtitle_bytes_are_a_cbor_byte_string() {
    let segment = Reply::Subtitle {
        format: SubtitleFormat::Srt,
        bytes: vec![0xAB; 100],
    };
    let chunk = Reply::SubtitleChunk {
        format: SubtitleFormat::Srt,
        bytes: vec![0xAB; 100],
        total: 100,
        version: 1,
    };
    // A byte string is about as long as its bytes; an array of integers
    // would take about two bytes each.
    for reply in [segment, chunk] {
        assert!(codec::encode(&reply).unwrap().len() < 170);
    }
}

fn creation() -> PasskeyCreation {
    PasskeyCreation {
        challenge: vec![7; 32],
        user_id: vec![9; 16],
        user_name: "alice on Home Media".into(),
        display_name: "Alice on Home Media".into(),
        exclude_credential_ids: vec![vec![1; 16]],
    }
}

fn made() -> FinishPasskeyRegistration {
    FinishPasskeyRegistration {
        credential_id: vec![1; 16],
        attestation_object: vec![0xA3; 64],
        client_data_json: b"{}".to_vec(),
        name: "Alice's iPhone".into(),
    }
}

fn signed() -> FinishPasskeySignIn {
    FinishPasskeySignIn {
        credential_id: vec![1; 16],
        authenticator_data: vec![0x49; 37],
        client_data_json: b"{}".to_vec(),
        signature: vec![0x30; 70],
        user_handle: vec![9; 16],
    }
}

/// Passkey bytes go as CBOR byte strings, lists of them too: an app's
/// platform API gives raw bytes, and arrays of integers would double them.
#[test]
fn passkey_bytes_are_cbor_byte_strings() {
    let ids = vec![vec![0xAB; 100]; 3];
    let request = Reply::PasskeyRequest(PasskeyRequest {
        challenge: vec![0xAB; 100],
        allow_credential_ids: ids.clone(),
    });
    assert!(codec::encode(&request).unwrap().len() < 470);
    let creation = Reply::PasskeyCreation(PasskeyCreation {
        exclude_credential_ids: ids,
        ..creation()
    });
    assert!(codec::encode(&creation).unwrap().len() < 600);
    let signed = Request::FinishPasskeySignIn(FinishPasskeySignIn {
        authenticator_data: vec![0xAB; 100],
        client_data_json: vec![0xAB; 100],
        signature: vec![0xAB; 100],
        ..signed()
    });
    assert!(codec::encode(&signed).unwrap().len() < 500);
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
        /// A platform this build has never heard of.
        pub platform: String,
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
        platform: "vision_os".into(),
        screen_inches: 65,
    })
    .unwrap();
    let hello: Hello = codec::decode(&bytes).unwrap();
    assert_eq!(hello.protocol_version, 2);
    assert_eq!(hello.platform, spoolframe_types::Platform::Unknown);
}

#[test]
fn a_search_is_flat_on_the_wire() {
    let request = Request::Search(SearchRequest {
        query: "dune 2021".into(),
        limit: 6,
        scope: SearchScope::Music,
        kind: Some(SearchKind::Artists),
        library: None,
    });
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        serde_json::json!({
            "type": "search",
            "query": "dune 2021",
            "limit": 6,
            "scope": "music",
            "kind": "artists",
            "library": null,
        })
    );
    let reply = serde_json::to_value(Reply::Search(search_results())).unwrap();
    assert_eq!(reply["type"], "search");
    assert_eq!(reply["year"], 1979);
    assert_eq!(reply["groups"][0]["kind"], "people");
    assert_eq!(reply["groups"][0]["people"][0]["id"], "plex:actor-134469");
    assert_eq!(reply["groups"][1]["hidden"], 3);
    assert_eq!(reply["counts"]["shows"], serde_json::Value::Null);
    assert_eq!(reply["top"]["item"]["kind"], "movie");
    assert_eq!(reply["groups"][1]["total"], 3);
    assert_eq!(reply["groups"][0]["total"], serde_json::Value::Null);
}

#[test]
fn a_person_at_the_top_is_tagged_person_on_the_wire() {
    let keanu = Person {
        id: PersonId::new(Namespace::Plex, "actor-134469").unwrap(),
        name: "Keanu Reeves".into(),
        role: PersonRole::Actor,
        count: Some(9),
    };
    let top = TopResult::Person(keanu);
    let json = serde_json::to_value(&top).unwrap();
    assert_eq!(json["person"]["name"], "Keanu Reeves");
    assert_eq!(json["person"]["id"], "plex:actor-134469");
    round_trip(top);
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
fn only_hello_auth_methods_logins_and_ping_are_pre_auth() {
    assert!(Request::Hello(hello()).is_pre_auth());
    assert!(Request::Ping.is_pre_auth());
    assert!(Request::BeginDeviceSignIn.is_pre_auth());
    // A passkey signs in (P3-19); only a session makes one.
    assert!(Request::BeginPasskeySignIn { username: None }.is_pre_auth());
    assert!(Request::FinishPasskeySignIn(signed()).is_pre_auth());
    assert!(!Request::BeginPasskeyRegistration.is_pre_auth());
    assert!(!Request::FinishPasskeyRegistration(made()).is_pre_auth());
    assert!(!Request::ListPasskeys.is_pre_auth());
    assert!(!Request::RemovePasskey { id: "AQID".into() }.is_pre_auth());
    assert!(
        Request::AwaitDeviceSignIn {
            request: "r".into()
        }
        .is_pre_auth()
    );
    // Approving signs someone in: only a session may.
    assert!(
        !Request::ApproveDeviceSignIn {
            code: "BCDFGHJK".into()
        }
        .is_pre_auth()
    );
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
    let clear = Request::ClearPin {
        proof: PinProof::Password {
            password: "hunter2".into(),
        },
    };
    assert!(!format!("{clear:?}").contains("hunter2"));
    let change = Request::SetPin {
        pin: "2468".into(),
        proof: Some(PinProof::Pin { pin: "1357".into() }),
    };
    assert!(!format!("{change:?}").contains("1357"));
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
            // Before P2-07: the mode a client never names is `Auto`.
            bitmap: BitmapSubtitles::Auto,
            ..
        }
    ));
}

#[test]
fn a_bitmap_mode_from_a_newer_client_is_unknown() {
    let mode: BitmapSubtitles = codec::decode(&codec::encode(&"draw_in_3d").unwrap()).unwrap();
    assert_eq!(mode, BitmapSubtitles::Unknown);
}

/// The item and started-playback replies as they were before P2-10, with
/// no `markers`: what an older server sends and an older client reads.
mod before_markers {
    use serde::{Deserialize, Serialize};
    use spoolframe_protocol::{PlaybackHandle, PlaybackPlan};
    use spoolframe_types::Item;

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Reply {
        Item {
            item: Item,
            children: Vec<Item>,
        },
        PlaybackStarted {
            handle: PlaybackHandle,
            plan: PlaybackPlan,
            size: u64,
            epoch: u32,
        },
    }
}

fn playback_started(markers: Vec<Marker>) -> Reply {
    Reply::PlaybackStarted {
        handle: spoolframe_protocol::PlaybackHandle("h".into()),
        plan: PlaybackPlan::Unsupported { reasons: vec![] },
        size: 12_345,
        chosen: None,
        epoch: 0,
        markers,
    }
}

#[test]
fn an_older_server_s_item_has_no_markers() {
    let bytes = codec::encode(&before_markers::Reply::Item {
        item: movie(),
        children: Vec::new(),
    })
    .unwrap();
    assert_eq!(
        codec::decode::<Reply>(&bytes).unwrap(),
        Reply::Item {
            item: movie(),
            children: Vec::new(),
            markers: Vec::new(),
        }
    );
}

#[test]
fn an_older_server_s_started_playback_has_no_markers() {
    let bytes = codec::encode(&before_markers::Reply::PlaybackStarted {
        handle: spoolframe_protocol::PlaybackHandle("h".into()),
        plan: PlaybackPlan::Unsupported { reasons: vec![] },
        size: 12_345,
        epoch: 0,
    })
    .unwrap();
    assert_eq!(
        codec::decode::<Reply>(&bytes).unwrap(),
        playback_started(Vec::new())
    );
}

#[test]
fn an_older_client_reads_an_item_that_has_markers() {
    let bytes = codec::encode(&Reply::Item {
        item: movie(),
        children: Vec::new(),
        markers: markers(),
    })
    .unwrap();
    let before_markers::Reply::Item { item, children } = codec::decode(&bytes).unwrap() else {
        panic!("not an item")
    };
    assert_eq!((item, children), (movie(), Vec::new()));
}

#[test]
fn an_older_client_reads_a_started_playback_that_has_markers() {
    let bytes = codec::encode(&playback_started(markers())).unwrap();
    let before_markers::Reply::PlaybackStarted { handle, size, .. } =
        codec::decode(&bytes).unwrap()
    else {
        panic!("not a started playback")
    };
    assert_eq!((handle.0.as_str(), size), ("h", 12_345));
}

/// A reply is one frame: a whole chunk has to fit one, with its framing.
#[test]
fn a_whole_subtitle_chunk_fits_a_frame() {
    let reply = Response::Ok(Reply::SubtitleChunk {
        format: SubtitleFormat::WebVtt,
        bytes: vec![0xAB; spoolframe_protocol::SUBTITLE_CHUNK as usize],
        total: u64::MAX,
        version: u64::MAX,
    });
    assert!(codec::encode(&reply).unwrap().len() < MAX_FRAME as usize);
}

/// A marker kind from a newer server does not cost the client the item or
/// its other markers.
#[test]
fn a_marker_kind_from_a_newer_server_is_unknown() {
    #[derive(Serialize)]
    struct NewerMarker {
        kind: &'static str,
        start_ms: u64,
        end_ms: u64,
        to_end: bool,
    }
    #[derive(Serialize)]
    struct NewerItem {
        #[serde(rename = "type")]
        kind: &'static str,
        item: Item,
        children: Vec<Item>,
        markers: Vec<NewerMarker>,
    }
    let bytes = codec::encode(&NewerItem {
        kind: "item",
        item: movie(),
        children: Vec::new(),
        markers: vec![
            NewerMarker {
                kind: "recap",
                start_ms: 0,
                end_ms: 45_000,
                to_end: false,
            },
            NewerMarker {
                kind: "intro",
                start_ms: 45_000,
                end_ms: 105_000,
                to_end: false,
            },
        ],
    })
    .unwrap();
    let Reply::Item { markers, .. } = codec::decode(&bytes).unwrap() else {
        panic!("not an item")
    };
    assert_eq!(
        markers.iter().map(|m| m.kind).collect::<Vec<_>>(),
        [MarkerKind::Unknown, MarkerKind::Intro]
    );
}
